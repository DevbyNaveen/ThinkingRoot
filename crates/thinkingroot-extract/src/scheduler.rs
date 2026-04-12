use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

/// Rate limit info parsed from provider response headers.
///
/// Populated by HTTP-based providers (OpenAI, Anthropic, Groq).
/// Empty (default) for Bedrock (uses AWS SDK) and Ollama (no limits).
#[derive(Debug, Clone, Default)]
pub struct HeaderRateLimits {
    /// Requests per minute limit from provider.
    pub rpm: Option<u32>,
    /// Tokens per minute limit from provider.
    pub tpm: Option<u32>,
    /// Remaining requests in the current window.
    pub remaining_requests: Option<u32>,
    /// Remaining tokens in the current window.
    pub remaining_tokens: Option<u32>,
}

impl HeaderRateLimits {
    /// Parse from reqwest response headers.
    /// Handles both Anthropic (`anthropic-ratelimit-*`) and OpenAI/Groq (`x-ratelimit-*`) formats.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        let get_u32 = |name: &str| -> Option<u32> {
            headers.get(name)?.to_str().ok()?.parse().ok()
        };

        Self {
            rpm: get_u32("anthropic-ratelimit-requests-limit")
                .or_else(|| get_u32("x-ratelimit-limit-requests")),
            tpm: get_u32("anthropic-ratelimit-tokens-limit")
                .or_else(|| get_u32("x-ratelimit-limit-tokens")),
            remaining_requests: get_u32("anthropic-ratelimit-requests-remaining")
                .or_else(|| get_u32("x-ratelimit-remaining-requests")),
            remaining_tokens: get_u32("anthropic-ratelimit-tokens-remaining")
                .or_else(|| get_u32("x-ratelimit-remaining-tokens")),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rpm.is_none() && self.tpm.is_none()
    }
}

/// Pre-emptive throughput scheduler — the core of the rate-limit prevention system.
///
/// Instead of reacting to 429 errors (hit wall → back off → retry), this
/// scheduler PREDICTS the safe send rate and gates every request before it
/// is sent. The provider never receives more requests than it can handle.
///
/// # How it works
///
/// 1. **Start conservative**: 1 call/second regardless of concurrency.
/// 2. **Learn from headers**: first response from an HTTP provider returns
///    `x-ratelimit-limit-requests` / `anthropic-ratelimit-requests-limit`.
///    The scheduler calculates the precise safe interval from these values.
/// 3. **Gate every send**: `wait_for_slot()` serialises sends through a
///    mutex-protected interval timer. No request is issued until the minimum
///    safe interval since the last send has elapsed.
/// 4. **Self-tune for header-less providers** (Bedrock, Ollama): ramp up
///    10% every 20 consecutive successes.
/// 5. **Safety net**: if a 429 somehow occurs (provider inconsistency, clock
///    drift), `record_throttle()` doubles the interval immediately.
pub struct ThroughputScheduler {
    /// Milliseconds between successive sends. Starts at 1000, updated dynamically.
    interval_ms: AtomicU64,

    /// Epoch-ms timestamp of the last completed send.
    last_send_ms: AtomicU64,

    /// Serialises the check-and-update in `wait_for_slot`.
    send_gate: Mutex<()>,

    /// Rolling window of recent token usage (last 20 calls).
    token_window: Mutex<VecDeque<u64>>,

    /// Running sum of the token window (avoids re-summing).
    token_window_sum: AtomicU64,

    /// Consecutive successful calls since last throttle.
    consecutive_successes: AtomicU64,

    /// Set once real limits have been observed from response headers.
    limits_known: AtomicBool,

    /// Last known requests-per-minute from headers (0 = unknown).
    known_rpm: AtomicU64,

    /// Last known tokens-per-minute from headers (0 = unknown).
    known_tpm: AtomicU64,

    /// Live remaining requests in the current provider window (0 = unknown).
    /// Updated every response — used to detect shared-quota pressure.
    remaining_requests: AtomicU64,

    /// Live remaining tokens in the current provider window (0 = unknown).
    remaining_tokens: AtomicU64,
}

impl ThroughputScheduler {
    /// Create a new scheduler. Starts at 1 call/second (conservative).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            interval_ms: AtomicU64::new(1_000),
            last_send_ms: AtomicU64::new(0),
            send_gate: Mutex::new(()),
            token_window: Mutex::new(VecDeque::with_capacity(20)),
            token_window_sum: AtomicU64::new(0),
            consecutive_successes: AtomicU64::new(0),
            limits_known: AtomicBool::new(false),
            known_rpm: AtomicU64::new(0),
            known_tpm: AtomicU64::new(0),
            remaining_requests: AtomicU64::new(0),
            remaining_tokens: AtomicU64::new(0),
        })
    }

    /// Block until it is safe to send the next request.
    ///
    /// Serialises all sends through an interval gate. If the time since the
    /// last send is less than `interval_ms`, sleeps for the difference.
    /// Callers queue behind the mutex — no thundering herd.
    pub async fn wait_for_slot(&self) {
        let _gate = self.send_gate.lock().await;

        let interval = self.interval_ms.load(Ordering::Relaxed);
        let last = self.last_send_ms.load(Ordering::Relaxed);
        let now = unix_ms();

        if interval > 0 && now < last + interval {
            let wait_ms = (last + interval) - now;
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }

        self.last_send_ms.store(unix_ms(), Ordering::Relaxed);
        // _gate drops — next waiter can enter
    }

    /// Record a successful LLM response.
    ///
    /// Updates the rolling token average and recalculates the safe send
    /// interval. `tokens` is an estimate of total tokens used (input + output).
    pub async fn record_success(&self, tokens: u64, limits: &HeaderRateLimits) {
        let avg = self.push_token(tokens).await;

        // Update known limits from headers.
        if !limits.is_empty() {
            if let Some(rpm) = limits.rpm {
                self.known_rpm.store(rpm as u64, Ordering::Relaxed);
            }
            if let Some(tpm) = limits.tpm {
                self.known_tpm.store(tpm as u64, Ordering::Relaxed);
            }
            // Store live remaining counts — used to detect shared-quota pressure
            // (e.g. another process or agent draining the same org quota).
            if let Some(r) = limits.remaining_requests {
                self.remaining_requests.store(r as u64, Ordering::Relaxed);
            }
            if let Some(r) = limits.remaining_tokens {
                self.remaining_tokens.store(r as u64, Ordering::Relaxed);
            }
            self.limits_known.store(true, Ordering::Relaxed);
        }

        let successes = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;
        self.recalculate_interval(avg, successes);
    }

    /// Record a rate-limit hit (429). Doubles the send interval immediately.
    ///
    /// In practice this should rarely fire — the scheduler should prevent
    /// 429s entirely. But providers can be inconsistent, so this is the
    /// safety net.
    pub fn record_throttle(&self) {
        self.consecutive_successes.store(0, Ordering::Relaxed);
        let current = self.interval_ms.load(Ordering::Relaxed);
        let new_interval = (current * 2).min(60_000);
        self.interval_ms.store(new_interval, Ordering::Relaxed);
        tracing::warn!(
            interval_ms = new_interval,
            "scheduler: 429 hit — doubling send interval (safety net fired)"
        );
    }

    /// Current estimated calls per minute at the active send rate.
    pub fn calls_per_min(&self) -> f64 {
        let ms = self.interval_ms.load(Ordering::Relaxed);
        if ms == 0 {
            return f64::INFINITY;
        }
        60_000.0 / ms as f64
    }

    // ── private ─────────────────────────────────────────────────────────

    async fn push_token(&self, tokens: u64) -> f64 {
        let mut w = self.token_window.lock().await;
        if w.len() >= 20 {
            if let Some(evicted) = w.pop_front() {
                self.token_window_sum.fetch_sub(evicted, Ordering::Relaxed);
            }
        }
        w.push_back(tokens);
        let new_sum = self.token_window_sum.fetch_add(tokens, Ordering::Relaxed) + tokens;
        new_sum as f64 / w.len() as f64
    }

    fn recalculate_interval(&self, avg_tokens: f64, successes: u64) {
        let rpm = self.known_rpm.load(Ordering::Relaxed);
        let tpm = self.known_tpm.load(Ordering::Relaxed);

        if rpm > 0 || tpm > 0 {
            // Real limits known — calculate safe interval precisely.
            // Use 90% of limit as margin (never ride the ceiling).
            let safe_cpm_by_rpm = if rpm > 0 {
                rpm as f64 * 0.90
            } else {
                f64::MAX
            };
            let safe_cpm_by_tpm = if tpm > 0 && avg_tokens > 0.0 {
                (tpm as f64 * 0.90) / avg_tokens
            } else {
                f64::MAX
            };

            let safe_cpm = safe_cpm_by_rpm.min(safe_cpm_by_tpm);
            if safe_cpm > 0.0 && safe_cpm.is_finite() {
                let base_interval = (60_000.0 / safe_cpm) as u64;

                // Remaining-window correction: if live remaining quota is low
                // (shared org quota, another process draining the window), scale
                // the interval up so we don't blow through what's left.
                // Below 20% remaining → up to 2× slowdown at 0%.
                let rem_req = self.remaining_requests.load(Ordering::Relaxed);
                let rem_tok = self.remaining_tokens.load(Ordering::Relaxed);

                let req_fraction = if rpm > 0 && rem_req > 0 {
                    (rem_req as f64 / rpm as f64).min(1.0)
                } else {
                    1.0
                };
                let tok_fraction = if tpm > 0 && rem_tok > 0 {
                    (rem_tok as f64 / tpm as f64).min(1.0)
                } else {
                    1.0
                };
                let fraction_remaining = req_fraction.min(tok_fraction);
                let window_correction = if fraction_remaining < 0.20 {
                    1.0 + (0.20 - fraction_remaining) * 5.0 // 1.0 at 20%, 2.0 at 0%
                } else {
                    1.0
                };

                let new_interval = ((base_interval as f64 * window_correction) as u64).min(60_000);
                let old = self.interval_ms.swap(new_interval, Ordering::Relaxed);
                // Only log when the interval changes meaningfully (>100ms delta).
                if (old as i64 - new_interval as i64).unsigned_abs() > 100 {
                    tracing::info!(
                        rpm,
                        tpm,
                        avg_tokens,
                        calls_per_min = safe_cpm,
                        interval_ms = new_interval,
                        window_correction,
                        "scheduler: send rate calibrated from provider limits"
                    );
                }
            }
        } else {
            // No limits known — self-tune: ramp up 10% every 20 successes.
            if successes > 0 && successes % 20 == 0 {
                let current = self.interval_ms.load(Ordering::Relaxed);
                let new_interval = ((current as f64 * 0.90) as u64).max(100);
                self.interval_ms.store(new_interval, Ordering::Relaxed);
                tracing::debug!(
                    successes,
                    interval_ms = new_interval,
                    "scheduler: ramping up send rate (provider limits unknown)"
                );
            }
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anthropic_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-requests-limit",
            "1000".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-tokens-limit",
            "80000".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-requests-remaining",
            "950".parse().unwrap(),
        );

        let limits = HeaderRateLimits::from_headers(&headers);
        assert_eq!(limits.rpm, Some(1000));
        assert_eq!(limits.tpm, Some(80000));
        assert_eq!(limits.remaining_requests, Some(950));
    }

    #[test]
    fn parses_openai_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-limit-requests", "500".parse().unwrap());
        headers.insert("x-ratelimit-limit-tokens", "30000".parse().unwrap());

        let limits = HeaderRateLimits::from_headers(&headers);
        assert_eq!(limits.rpm, Some(500));
        assert_eq!(limits.tpm, Some(30000));
    }

    #[test]
    fn empty_for_missing_headers() {
        let headers = reqwest::header::HeaderMap::new();
        assert!(HeaderRateLimits::from_headers(&headers).is_empty());
    }

    #[tokio::test]
    async fn calibrates_interval_from_rpm() {
        let scheduler = ThroughputScheduler::new();

        let limits = HeaderRateLimits {
            rpm: Some(60),
            tpm: Some(900_000), // TPM not binding
            ..Default::default()
        };

        scheduler.record_success(1_000, &limits).await;

        // safe_cpm = 60 * 0.90 = 54 → interval = 60000/54 ≈ 1111 ms
        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval > 1000 && interval < 1200,
            "expected ~1111ms, got {interval}"
        );
    }

    #[tokio::test]
    async fn calibrates_interval_from_tpm_when_binding() {
        let scheduler = ThroughputScheduler::new();

        let limits = HeaderRateLimits {
            rpm: Some(1000),     // RPM not binding
            tpm: Some(10_000),   // TPM is binding
            ..Default::default()
        };

        // avg_tokens = 500 per call
        scheduler.record_success(500, &limits).await;

        // safe_cpm_by_tpm = (10000 * 0.90) / 500 = 18 → interval = 60000/18 = 3333ms
        // safe_cpm_by_rpm = 1000 * 0.90 = 900 → not binding
        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval > 3000 && interval < 3500,
            "expected ~3333ms, got {interval}"
        );
    }

    #[tokio::test]
    async fn throttle_doubles_interval() {
        let scheduler = ThroughputScheduler::new();
        scheduler.interval_ms.store(1_000, Ordering::Relaxed);
        scheduler.record_throttle();
        assert_eq!(scheduler.interval_ms.load(Ordering::Relaxed), 2_000);
    }

    #[tokio::test]
    async fn throttle_caps_at_60_seconds() {
        let scheduler = ThroughputScheduler::new();
        scheduler.interval_ms.store(40_000, Ordering::Relaxed);
        scheduler.record_throttle();
        assert_eq!(scheduler.interval_ms.load(Ordering::Relaxed), 60_000);
    }

    #[tokio::test]
    async fn self_tunes_for_unknown_provider() {
        let scheduler = ThroughputScheduler::new();
        scheduler.interval_ms.store(2_000, Ordering::Relaxed);

        // 20 successes with no header limits
        for _ in 0..20 {
            scheduler
                .record_success(500, &HeaderRateLimits::default())
                .await;
        }

        // Interval should have ramped up (decreased) by ~10%
        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval < 2_000,
            "expected ramp-up, got interval={interval}"
        );
    }

    #[tokio::test]
    async fn window_correction_slows_down_when_quota_low() {
        let scheduler = ThroughputScheduler::new();

        // RPM=60 → base interval ≈ 1111ms (60 * 0.90 = 54 cpm)
        // With only 5% remaining requests, correction = 1 + (0.20 - 0.05) * 5 = 1.75
        // Expected interval ≈ 1111 * 1.75 ≈ 1944ms
        let limits = HeaderRateLimits {
            rpm: Some(60),
            tpm: Some(900_000),
            remaining_requests: Some(3), // 3/60 = 5% remaining
            remaining_tokens: None,
        };
        scheduler.record_success(1_000, &limits).await;

        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval > 1500,
            "expected window correction to raise interval above 1500ms, got {interval}"
        );
    }

    #[tokio::test]
    async fn window_correction_absent_when_quota_healthy() {
        let scheduler = ThroughputScheduler::new();

        // 80% remaining → no correction applied
        let limits = HeaderRateLimits {
            rpm: Some(60),
            tpm: Some(900_000),
            remaining_requests: Some(48), // 48/60 = 80%
            remaining_tokens: None,
        };
        scheduler.record_success(1_000, &limits).await;

        // Interval should be close to base ~1111ms (no window correction)
        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval < 1300,
            "expected no window correction at 80% remaining, got {interval}"
        );
    }

    #[tokio::test]
    async fn wait_for_slot_serialises_rapid_sends() {
        let scheduler = ThroughputScheduler::new();
        scheduler.interval_ms.store(50, Ordering::Relaxed); // 50ms interval for fast test

        let start = std::time::Instant::now();
        // 3 rapid sends should take at least 2 * 50ms = 100ms
        scheduler.wait_for_slot().await;
        scheduler.wait_for_slot().await;
        scheduler.wait_for_slot().await;
        let elapsed = start.elapsed().as_millis();

        assert!(
            elapsed >= 90,
            "3 sends at 50ms interval should take ≥100ms, got {elapsed}ms"
        );
    }
}
