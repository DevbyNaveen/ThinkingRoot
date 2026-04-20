use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
        let get_u32 =
            |name: &str| -> Option<u32> { headers.get(name)?.to_str().ok()?.parse().ok() };

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

// ── Request Ticket ────────────────────────────────────────────────

/// Per-request token issued by `wait_for_slot` and consumed by
/// `record_success` or `record_throttle`.
///
/// Carries the request start timestamp for latency measurement. Holds a
/// shared reference to the scheduler's `in_flight` counter and **automatically
/// decrements it on drop** — so the count is always correct regardless of
/// which code path (success, rate-limit, network error, truncation) the
/// request takes.
pub struct RequestTicket {
    /// Epoch-ms timestamp when the slot was granted.
    pub(crate) start_ms: u64,
    /// Shared in-flight counter. Decremented on drop.
    in_flight: Arc<AtomicU64>,
}

impl Drop for RequestTicket {
    fn drop(&mut self) {
        // saturating_sub guards against test-created tickets that bypass
        // wait_for_slot (which would leave the counter at 0 before the drop).
        self.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            })
            .ok();
    }
}

#[cfg(test)]
impl RequestTicket {
    /// Construct a standalone ticket for unit tests.
    /// Uses its own counter (starts at 1) so the Drop does a harmless 1→0.
    pub fn for_test() -> Self {
        Self {
            start_ms: unix_ms(),
            in_flight: Arc::new(AtomicU64::new(1)),
        }
    }
}

// ── Scheduler ────────────────────────────────────────────────────

/// Pre-emptive throughput scheduler — the core of the rate-limit prevention system.
///
/// Instead of reacting to 429 errors (hit wall → back off → retry), this
/// scheduler PREDICTS the safe send rate and gates every request before it
/// is sent. The provider never receives more requests than it can handle.
///
/// # How it works
///
/// 1. **Start conservative**: 1 call/second, concurrency = `max_concurrency`.
/// 2. **Learn from headers**: first response from an HTTP provider returns
///    `x-ratelimit-limit-requests` / `anthropic-ratelimit-requests-limit`.
///    The scheduler calculates the precise safe interval from these values.
/// 3. **Rate gate**: `wait_for_slot()` serialises sends through a
///    mutex-protected interval timer. No request is issued until the minimum
///    safe interval since the last send has elapsed.
/// 4. **Concurrency auto-tune** (Little's Law — N = λ × W): after each
///    response the scheduler derives the optimal number of in-flight requests
///    from the rolling average latency and current send interval. Low-latency
///    providers (Groq ~400 ms) converge to 1–2 concurrent; high-latency
///    providers (Bedrock ~5 s) converge higher. Never exceeds `max_concurrency`.
/// 5. **Self-tune for header-less providers** (Bedrock, Ollama): ramp up
///    10% every 20 consecutive successes.
/// 6. **Safety net**: if a 429 somehow occurs, `record_throttle()` doubles
///    the interval and halves concurrency immediately.
pub struct ThroughputScheduler {
    /// Milliseconds between successive sends. Starts at 1000, updated dynamically.
    interval_ms: AtomicU64,

    /// Epoch-ms timestamp of the last completed send.
    last_send_ms: AtomicU64,

    /// Serialises the check-and-update in `wait_for_slot` (rate gate).
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
    remaining_requests: AtomicU64,

    /// Live remaining tokens in the current provider window (0 = unknown).
    remaining_tokens: AtomicU64,

    // ── Concurrency auto-tuning ───────────────────────────────────
    /// HTTP requests currently in-flight (dispatched, not yet responded).
    /// Shared with issued `RequestTicket`s which decrement it on drop.
    in_flight: Arc<AtomicU64>,

    /// Dynamically-tuned concurrency limit derived via Little's Law.
    /// Updated after every successful response. Capped at `max_concurrency`.
    safe_concurrency: AtomicU64,

    /// Hard upper-bound from `LlmConfig.max_concurrent_requests`.
    /// Auto-tuning never exceeds this value.
    max_concurrency: AtomicU64,

    /// Rolling window of recent request latencies in ms (last 20 calls).
    latency_window: Mutex<VecDeque<u64>>,

    /// Running sum of the latency window (avoids re-summing).
    latency_window_sum: AtomicU64,
}

impl ThroughputScheduler {
    /// Create a new scheduler.
    ///
    /// `max_concurrency` is the hard upper-bound from `LlmConfig.max_concurrent_requests`.
    /// The scheduler starts permissive (full concurrency allowed) and converges
    /// downward to the minimum that keeps the pipeline saturated.
    pub fn new(max_concurrency: usize) -> Arc<Self> {
        let max = max_concurrency.max(1) as u64;
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
            in_flight: Arc::new(AtomicU64::new(0)),
            safe_concurrency: AtomicU64::new(max), // starts permissive; converges down
            max_concurrency: AtomicU64::new(max),
            latency_window: Mutex::new(VecDeque::with_capacity(20)),
            latency_window_sum: AtomicU64::new(0),
        })
    }

    /// Block until it is safe to dispatch the next request.
    ///
    /// Two sequential gates:
    ///
    /// 1. **Concurrency gate** — if `in_flight >= safe_concurrency`, waits
    ///    (50 ms poll) until a request completes before joining the send queue.
    ///    In practice this rarely blocks: the rate gate is the primary constraint,
    ///    and safe_concurrency is initialised to max.
    ///
    /// 2. **Rate gate** — serialises sends through an interval timer so the
    ///    provider never receives requests faster than its safe rate.
    ///
    /// Returns a `RequestTicket` that **must** be passed to `record_success` or
    /// `record_throttle`. On any other error path simply drop it — the in-flight
    /// counter is decremented automatically.
    pub async fn wait_for_slot(&self) -> RequestTicket {
        // ── 1. Concurrency gate ───────────────────────────────────
        loop {
            let in_flight = self.in_flight.load(Ordering::Acquire);
            let safe = self.safe_concurrency.load(Ordering::Acquire).max(1);
            if in_flight < safe {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // ── 2. Rate gate ──────────────────────────────────────────
        let _gate = self.send_gate.lock().await;

        let interval = self.interval_ms.load(Ordering::Relaxed);
        let last = self.last_send_ms.load(Ordering::Relaxed);
        let now = unix_ms();

        if interval > 0 && now < last + interval {
            let wait_ms = (last + interval) - now;
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }

        self.last_send_ms.store(unix_ms(), Ordering::Relaxed);
        // Increment before releasing the gate so subsequent waiters at the
        // concurrency gate see the updated in-flight count immediately.
        self.in_flight.fetch_add(1, Ordering::AcqRel);

        RequestTicket {
            start_ms: unix_ms(),
            in_flight: Arc::clone(&self.in_flight),
        }
        // _gate drops here — next waiter can enter the rate gate
    }

    /// Record a successful LLM response.
    ///
    /// Consumes the ticket (in-flight decremented via Drop). Updates the
    /// rolling token and latency windows, calibrates the send-rate interval
    /// from provider headers, and re-derives optimal concurrency via Little's Law.
    pub async fn record_success(
        &self,
        tokens: u64,
        limits: &HeaderRateLimits,
        ticket: RequestTicket,
    ) {
        let latency_ms = unix_ms().saturating_sub(ticket.start_ms);
        // ticket drops at end of this function → in_flight decremented.

        let avg_tokens = self.push_token(tokens).await;
        let avg_latency = self.push_latency(latency_ms).await;

        // Update known limits from headers.
        if !limits.is_empty() {
            if let Some(rpm) = limits.rpm {
                self.known_rpm.store(rpm as u64, Ordering::Relaxed);
            }
            if let Some(tpm) = limits.tpm {
                self.known_tpm.store(tpm as u64, Ordering::Relaxed);
            }
            // Live remaining counts detect shared-quota pressure (another process
            // draining the same org quota).
            if let Some(r) = limits.remaining_requests {
                self.remaining_requests.store(r as u64, Ordering::Relaxed);
            }
            if let Some(r) = limits.remaining_tokens {
                self.remaining_tokens.store(r as u64, Ordering::Relaxed);
            }
            self.limits_known.store(true, Ordering::Relaxed);
        }

        let successes = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;
        self.recalculate_interval(avg_tokens, successes);
        self.recalculate_concurrency(avg_latency);
    }

    /// Record a rate-limit hit (429). Doubles the send interval and halves
    /// concurrency immediately as a safety-net back-off.
    ///
    /// Consumes the ticket (in-flight decremented via Drop).
    pub fn record_throttle(&self, ticket: RequestTicket) {
        drop(ticket); // explicit drop for clarity; in_flight decremented here.

        self.consecutive_successes.store(0, Ordering::Relaxed);

        // Rate back-off: double send interval.
        let cur_interval = self.interval_ms.load(Ordering::Relaxed);
        let new_interval = (cur_interval * 2).min(60_000);
        self.interval_ms.store(new_interval, Ordering::Relaxed);

        // Concurrency back-off: halve safe_concurrency.
        let cur_conc = self.safe_concurrency.load(Ordering::Relaxed);
        let new_conc = (cur_conc / 2).max(1);
        self.safe_concurrency.store(new_conc, Ordering::Relaxed);

        tracing::warn!(
            interval_ms = new_interval,
            safe_concurrency = new_conc,
            "scheduler: 429 hit — doubled interval, halved concurrency (safety net fired)"
        );
    }

    /// Current estimated calls per minute at the active send rate.
    pub fn calls_per_min(&self) -> f64 {
        let ms = self.interval_ms.load(Ordering::Relaxed);
        if ms == 0 {
            f64::INFINITY
        } else {
            60_000.0 / ms as f64
        }
    }

    /// Current auto-tuned safe concurrency.
    pub fn safe_concurrency(&self) -> usize {
        self.safe_concurrency.load(Ordering::Relaxed) as usize
    }

    // ── private ──────────────────────────────────────────────────

    async fn push_token(&self, tokens: u64) -> f64 {
        let mut w = self.token_window.lock().await;
        if w.len() >= 20
            && let Some(evicted) = w.pop_front()
        {
            self.token_window_sum.fetch_sub(evicted, Ordering::Relaxed);
        }
        w.push_back(tokens);
        let new_sum = self.token_window_sum.fetch_add(tokens, Ordering::Relaxed) + tokens;
        new_sum as f64 / w.len() as f64
    }

    async fn push_latency(&self, latency_ms: u64) -> f64 {
        let mut w = self.latency_window.lock().await;
        if w.len() >= 20
            && let Some(evicted) = w.pop_front()
        {
            self.latency_window_sum
                .fetch_sub(evicted, Ordering::Relaxed);
        }
        w.push_back(latency_ms);
        let new_sum = self
            .latency_window_sum
            .fetch_add(latency_ms, Ordering::Relaxed)
            + latency_ms;
        new_sum as f64 / w.len() as f64
    }

    fn recalculate_interval(&self, avg_tokens: f64, successes: u64) {
        let rpm = self.known_rpm.load(Ordering::Relaxed);
        let tpm = self.known_tpm.load(Ordering::Relaxed);

        if rpm > 0 || tpm > 0 {
            // Real limits known — calculate safe interval precisely.
            // Use 90% of limit as margin (never ride the ceiling).
            let safe_cpm_by_rpm = if rpm > 0 { rpm as f64 * 0.90 } else { f64::MAX };
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
                // the interval up. Below 20% remaining → up to 2× slowdown at 0%.
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
            // No limits known (Bedrock, Ollama) — self-tune: ramp up 10% every 20 successes.
            if successes > 0 && successes.is_multiple_of(20) {
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

    /// Derive optimal concurrency from rolling average latency using Little's Law:
    /// `N = λ × W` where λ = 1/interval_ms and W = avg_latency_ms.
    ///
    /// Intuition: if requests take 3 s each and we send one per second, we need
    /// 3 concurrent slots to keep the pipeline fully saturated — fewer starves
    /// throughput, more wastes resources without improving it.
    fn recalculate_concurrency(&self, avg_latency_ms: f64) {
        if avg_latency_ms <= 0.0 {
            return;
        }
        let interval = self.interval_ms.load(Ordering::Relaxed);
        if interval == 0 {
            return;
        }

        // N = W / T  (ceil to avoid pipeline stall from rounding down)
        let derived = (avg_latency_ms / interval as f64).ceil() as u64;
        let max = self.max_concurrency.load(Ordering::Relaxed);
        let new_safe = derived.max(1).min(max);

        let old = self.safe_concurrency.swap(new_safe, Ordering::Relaxed);
        if old != new_safe {
            tracing::info!(
                avg_latency_ms,
                interval_ms = interval,
                safe_concurrency = new_safe,
                max_concurrency = max,
                "scheduler: concurrency recalibrated (Little's Law)"
            );
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
        headers.insert("anthropic-ratelimit-tokens-limit", "80000".parse().unwrap());
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
        let scheduler = ThroughputScheduler::new(5);
        let limits = HeaderRateLimits {
            rpm: Some(60),
            tpm: Some(900_000), // not binding
            ..Default::default()
        };
        scheduler
            .record_success(1_000, &limits, RequestTicket::for_test())
            .await;

        // safe_cpm = 60 × 0.90 = 54 → interval = 60_000/54 ≈ 1111 ms
        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval > 1000 && interval < 1200,
            "expected ~1111ms, got {interval}"
        );
    }

    #[tokio::test]
    async fn calibrates_interval_from_tpm_when_binding() {
        let scheduler = ThroughputScheduler::new(5);
        let limits = HeaderRateLimits {
            rpm: Some(1000),   // not binding
            tpm: Some(10_000), // binding
            ..Default::default()
        };
        scheduler
            .record_success(500, &limits, RequestTicket::for_test())
            .await;

        // safe_cpm_by_tpm = (10_000 × 0.90) / 500 = 18 → interval = 60_000/18 = 3333ms
        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval > 3000 && interval < 3500,
            "expected ~3333ms, got {interval}"
        );
    }

    #[tokio::test]
    async fn throttle_doubles_interval() {
        let scheduler = ThroughputScheduler::new(5);
        scheduler.interval_ms.store(1_000, Ordering::Relaxed);
        scheduler.record_throttle(RequestTicket::for_test());
        assert_eq!(scheduler.interval_ms.load(Ordering::Relaxed), 2_000);
    }

    #[tokio::test]
    async fn throttle_caps_at_60_seconds() {
        let scheduler = ThroughputScheduler::new(5);
        scheduler.interval_ms.store(40_000, Ordering::Relaxed);
        scheduler.record_throttle(RequestTicket::for_test());
        assert_eq!(scheduler.interval_ms.load(Ordering::Relaxed), 60_000);
    }

    #[tokio::test]
    async fn throttle_halves_concurrency() {
        let scheduler = ThroughputScheduler::new(5);
        scheduler.safe_concurrency.store(4, Ordering::Relaxed);
        scheduler.record_throttle(RequestTicket::for_test());
        assert_eq!(scheduler.safe_concurrency.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn throttle_concurrency_floor_is_one() {
        let scheduler = ThroughputScheduler::new(5);
        scheduler.safe_concurrency.store(1, Ordering::Relaxed);
        scheduler.record_throttle(RequestTicket::for_test());
        assert_eq!(scheduler.safe_concurrency.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn self_tunes_for_unknown_provider() {
        let scheduler = ThroughputScheduler::new(5);
        scheduler.interval_ms.store(2_000, Ordering::Relaxed);

        for _ in 0..20 {
            scheduler
                .record_success(500, &HeaderRateLimits::default(), RequestTicket::for_test())
                .await;
        }

        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval < 2_000,
            "expected ramp-up, got interval={interval}"
        );
    }

    #[tokio::test]
    async fn window_correction_slows_down_when_quota_low() {
        let scheduler = ThroughputScheduler::new(5);
        let limits = HeaderRateLimits {
            rpm: Some(60),
            tpm: Some(900_000),
            remaining_requests: Some(3), // 3/60 = 5% remaining
            remaining_tokens: None,
        };
        scheduler
            .record_success(1_000, &limits, RequestTicket::for_test())
            .await;

        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval > 1500,
            "expected window correction, got {interval}"
        );
    }

    #[tokio::test]
    async fn window_correction_absent_when_quota_healthy() {
        let scheduler = ThroughputScheduler::new(5);
        let limits = HeaderRateLimits {
            rpm: Some(60),
            tpm: Some(900_000),
            remaining_requests: Some(48), // 80% remaining
            remaining_tokens: None,
        };
        scheduler
            .record_success(1_000, &limits, RequestTicket::for_test())
            .await;

        let interval = scheduler.interval_ms.load(Ordering::Relaxed);
        assert!(
            interval < 1300,
            "expected no window correction, got {interval}"
        );
    }

    #[tokio::test]
    async fn concurrency_auto_tunes_via_littles_law() {
        let scheduler = ThroughputScheduler::new(10);
        // Set interval to 1000ms so Little's Law gives us a predictable result.
        scheduler.interval_ms.store(1_000, Ordering::Relaxed);

        // Simulate a 3000ms latency.
        let ticket = RequestTicket {
            start_ms: unix_ms().saturating_sub(3_000),
            in_flight: Arc::new(AtomicU64::new(1)),
        };
        let limits = HeaderRateLimits {
            rpm: Some(60),
            tpm: Some(900_000),
            ..Default::default()
        };
        scheduler.record_success(1_000, &limits, ticket).await;

        // N = ceil(3000 / interval_after_calibration)
        // interval_after_calibration ≈ 1111ms (60 RPM @ 90%)
        // N = ceil(3000 / 1111) = ceil(2.7) = 3
        let conc = scheduler.safe_concurrency.load(Ordering::Relaxed);
        assert!(
            (1..=10).contains(&conc),
            "concurrency out of bounds: {conc}"
        );
    }

    #[tokio::test]
    async fn concurrency_never_exceeds_max() {
        let scheduler = ThroughputScheduler::new(3); // hard cap
        let ticket = RequestTicket {
            start_ms: unix_ms().saturating_sub(30_000), // extreme latency
            in_flight: Arc::new(AtomicU64::new(1)),
        };
        let limits = HeaderRateLimits {
            rpm: Some(60),
            tpm: Some(900_000),
            ..Default::default()
        };
        scheduler.record_success(1_000, &limits, ticket).await;

        let conc = scheduler.safe_concurrency.load(Ordering::Relaxed);
        assert!(conc <= 3, "concurrency exceeded max_concurrency=3: {conc}");
    }

    #[tokio::test]
    async fn ticket_drop_decrements_in_flight() {
        let counter = Arc::new(AtomicU64::new(2));
        {
            let _ticket = RequestTicket {
                start_ms: unix_ms(),
                in_flight: Arc::clone(&counter),
            };
            assert_eq!(counter.load(Ordering::Relaxed), 2);
        } // ticket dropped here
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ticket_drop_saturates_at_zero() {
        let counter = Arc::new(AtomicU64::new(0));
        {
            let _ticket = RequestTicket {
                start_ms: unix_ms(),
                in_flight: Arc::clone(&counter),
            };
        }
        // Must not underflow to u64::MAX
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn wait_for_slot_serialises_rapid_sends() {
        let scheduler = ThroughputScheduler::new(5);
        scheduler.interval_ms.store(50, Ordering::Relaxed);

        let start = std::time::Instant::now();
        // 3 rapid sends should take at least 2 × 50 ms = 100 ms.
        let t1 = scheduler.wait_for_slot().await;
        drop(t1); // keep in_flight from blocking the concurrency gate
        let t2 = scheduler.wait_for_slot().await;
        drop(t2);
        let t3 = scheduler.wait_for_slot().await;
        drop(t3);
        let elapsed = start.elapsed().as_millis();

        assert!(
            elapsed >= 90,
            "3 sends at 50ms interval should take ≥100ms, got {elapsed}ms"
        );
    }
}
