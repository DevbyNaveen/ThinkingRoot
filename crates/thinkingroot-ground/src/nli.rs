//! NLI (Natural Language Inference) judge for hallucination detection.
//!
//! Uses cross-encoder/nli-deberta-v3-xsmall (71M params, INT8 quantized)
//! embedded directly in the binary — zero downloads, zero network, zero setup.
//!
//! Model: cross-encoder/nli-deberta-v3-xsmall
//! License: Apache 2.0
//! MNLI-mm accuracy: 87.77%
//! INT8 size: ~83 MB (ARM64 and x86_64 variants)
//!
//! Label order (config.json id2label):
//!   index 0 → contradiction
//!   index 1 → entailment
//!   index 2 → neutral

use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;

// ── Embedded model bytes ─────────────────────────────────────────────────────
// Compiled into the binary at build time. No downloads, no cache, no network.

#[cfg(target_arch = "aarch64")]
static ONNX_MODEL: &[u8] = include_bytes!("../models/model_qint8_arm64.onnx");

#[cfg(target_arch = "x86_64")]
static ONNX_MODEL: &[u8] = include_bytes!("../models/model_quint8_avx2.onnx");

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
compile_error!("NLI model: unsupported architecture. Only aarch64 and x86_64 are supported.");

static TOKENIZER_JSON: &[u8] = include_bytes!("../models/tokenizer.json");

/// Peak memory per ONNX session during inference.
///
/// DeBERTa-v3-xsmall INT8 at batch_size=16 × seq_len=512:
/// - Model weights:         ~83 MB
/// - ONNX Runtime overhead: ~50 MB
/// - Activation buffers:    ~200-400 MB
/// - Peak total:            ~400-600 MB
const SESSION_MEMORY_MB: f64 = 600.0;

/// Minimum available RAM to keep free (OS, pipeline data, fastembed, safety).
const RAM_HEADROOM_MB: f64 = 2000.0;

// ── NLI result ────────────────────────────────────────────────────────────────

/// NLI classification result.
///
/// Label order per config.json: [contradiction=0, entailment=1, neutral=2]
#[derive(Debug, Clone, Copy)]
pub struct NliResult {
    pub entailment: f64,
    pub neutral: f64,
    pub contradiction: f64,
}

impl NliResult {
    /// Grounding score: entailment probability penalised by contradiction.
    /// Returns a value in [0.0, 1.0]; higher = more grounded in source.
    pub fn score(&self) -> f64 {
        (self.entailment - 0.5 * self.contradiction).max(0.0)
    }

    fn neutral_fallback() -> Self {
        Self {
            entailment: 0.33,
            neutral: 0.34,
            contradiction: 0.33,
        }
    }
}

// ── RAM + CPU detection ──────────────────────────────────────────────────────

/// Returns conservatively available RAM in megabytes.
///
/// macOS: "Pages free" only (not inactive — macOS OOM-kills before reclaiming).
/// Linux: `/proc/meminfo` MemAvailable.
/// Fallback: 2048 MB (conservative).
fn available_ram_mb() -> f64 {
    #[cfg(target_os = "macos")]
    {
        let output = match std::process::Command::new("vm_stat").output() {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(_) => return 2048.0,
        };

        let page_size: f64 = output
            .lines()
            .next()
            .and_then(|line| {
                line.split("page size of ")
                    .nth(1)?
                    .split(' ')
                    .next()?
                    .trim_end_matches(')')
                    .parse::<f64>()
                    .ok()
            })
            .unwrap_or(16384.0);

        let free: f64 = output
            .lines()
            .find(|l| l.starts_with("Pages free"))
            .and_then(|l| {
                l.split(':')
                    .nth(1)?
                    .trim()
                    .trim_end_matches('.')
                    .parse::<f64>()
                    .ok()
            })
            .unwrap_or(0.0);

        let mb = free * page_size / (1024.0 * 1024.0);
        tracing::info!("available RAM (free pages): {mb:.0} MB");
        mb
    }

    #[cfg(target_os = "linux")]
    {
        let meminfo = match std::fs::read_to_string("/proc/meminfo") {
            Ok(s) => s,
            Err(_) => return 2048.0,
        };

        let mb = meminfo
            .lines()
            .find(|l| l.starts_with("MemAvailable:"))
            .and_then(|l| {
                l.split_whitespace()
                    .nth(1)?
                    .parse::<f64>()
                    .ok()
                    .map(|kb| kb / 1024.0)
            })
            .unwrap_or(2048.0);

        tracing::info!("available RAM: {mb:.0} MB");
        mb
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        2048.0
    }
}

/// Returns P-core count (Apple Silicon) or physical core count (other).
fn p_core_count() -> usize {
    #[cfg(target_os = "macos")]
    {
        let p_cores = std::process::Command::new("sysctl")
            .args(["-n", "hw.perflevel0.physicalcpu"])
            .output()
            .ok()
            .and_then(|o| {
                std::str::from_utf8(&o.stdout)
                    .ok()?
                    .trim()
                    .parse::<usize>()
                    .ok()
            });

        if let Some(p) = p_cores {
            return p.max(1);
        }

        std::process::Command::new("sysctl")
            .args(["-n", "hw.physicalcpu"])
            .output()
            .ok()
            .and_then(|o| {
                std::str::from_utf8(&o.stdout)
                    .ok()?
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(2)
            .max(1)
    }

    #[cfg(target_os = "linux")]
    {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        cpuinfo
            .lines()
            .filter(|l| l.starts_with("cpu cores"))
            .filter_map(|l| l.split(':').nth(1)?.trim().parse::<usize>().ok())
            .sum::<usize>()
            .max(1)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .max(1)
    }
}

/// RAM-aware pool sizing. Returns (num_workers, intra_threads_per_worker).
fn compute_pool_config() -> (usize, usize) {
    let ram_mb = available_ram_mb();
    let cores = p_core_count();

    let budget_mb = (ram_mb - RAM_HEADROOM_MB).max(SESSION_MEMORY_MB);
    let max_by_ram = (budget_mb / SESSION_MEMORY_MB) as usize;
    let workers = max_by_ram.min(cores).clamp(1, 8);
    let intra_threads = (cores / workers).max(1);

    tracing::info!(
        "NLI pool: {workers} worker(s) × {intra_threads} intra-op thread(s) \
         (RAM: {ram_mb:.0} MB, cores: {cores}, budget: {budget_mb:.0} MB)"
    );

    (workers, intra_threads)
}

// ── NLI judge (single session) ───────────────────────────────────────────────

/// One NLI inference session (ONNX Runtime + tokenizer).
///
/// Created from embedded model bytes — no file I/O, no network.
pub struct NliJudge {
    session: Session,
    tokenizer: tokenizers::Tokenizer,
}

impl NliJudge {
    /// Create an NLI judge from the embedded model with configurable parallelism.
    fn load(intra_threads: usize) -> thinkingroot_core::Result<Self> {
        let tokenizer =
            tokenizers::Tokenizer::from_bytes(TOKENIZER_JSON).map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!(
                    "NLI tokenizer load failed: {e}"
                ))
            })?;

        let session = Session::builder()
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!(
                    "ort session builder failed: {e}"
                ))
            })?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!(
                    "optimization level failed: {e}"
                ))
            })?
            .with_intra_threads(intra_threads)
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!(
                    "intra_threads config failed: {e}"
                ))
            })?
            .commit_from_memory(ONNX_MODEL)
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!(
                    "NLI model load failed: {e}"
                ))
            })?;

        Ok(Self { session, tokenizer })
    }

    /// Score a batch of (source_text, claim) pairs.
    ///
    /// NLI convention: premise = source_text, hypothesis = claim.
    /// Returns one grounding score per pair in [0.0, 1.0].
    ///
    /// Label order: [contradiction=0, entailment=1, neutral=2]
    pub fn score_batch(&mut self, pairs: &[(&str, &str)]) -> Vec<f64> {
        if pairs.is_empty() {
            return vec![];
        }

        let batch_size = pairs.len();
        let mut encodings: Vec<(Vec<i64>, Vec<i64>)> = Vec::with_capacity(batch_size);

        for &(source_text, claim) in pairs {
            let source = if source_text.len() > 1500 {
                &source_text[..1500]
            } else {
                source_text
            };
            match self.tokenizer.encode((source, claim), true) {
                Ok(enc) => {
                    let ids: Vec<i64> =
                        enc.get_ids().iter().map(|&id| id as i64).collect();
                    let mask: Vec<i64> =
                        enc.get_attention_mask().iter().map(|&m| m as i64).collect();
                    encodings.push((ids, mask));
                }
                Err(e) => {
                    tracing::warn!("NLI tokenisation failed: {e}");
                    encodings.push((vec![0i64; 1], vec![0i64; 1]));
                }
            }
        }

        let max_len = encodings.iter().map(|(ids, _)| ids.len()).max().unwrap_or(1);
        let mut flat_ids = vec![0i64; batch_size * max_len];
        let mut flat_mask = vec![0i64; batch_size * max_len];

        for (i, (ids, mask)) in encodings.iter().enumerate() {
            let len = ids.len().min(max_len);
            let row = i * max_len;
            flat_ids[row..row + len].copy_from_slice(&ids[..len]);
            flat_mask[row..row + len].copy_from_slice(&mask[..len]);
        }

        let ids_tensor =
            match Tensor::from_array(([batch_size, max_len], flat_ids.into_boxed_slice())) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("NLI tensor creation failed: {e}");
                    return vec![NliResult::neutral_fallback().score(); batch_size];
                }
            };
        let mask_tensor =
            match Tensor::from_array(([batch_size, max_len], flat_mask.into_boxed_slice())) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("NLI tensor creation failed: {e}");
                    return vec![NliResult::neutral_fallback().score(); batch_size];
                }
            };

        let outputs = match self.session.run(ort::inputs![
            "input_ids"      => ids_tensor,
            "attention_mask" => mask_tensor,
        ]) {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!("NLI inference failed: {e}");
                return vec![NliResult::neutral_fallback().score(); batch_size];
            }
        };

        // Logits shape: (batch_size, 3)
        // [contradiction=0, entailment=1, neutral=2]
        match outputs[0].try_extract_tensor::<f32>() {
            Ok((_shape, data)) => (0..batch_size)
                .map(|i| {
                    let offset = i * 3;
                    if offset + 2 < data.len() {
                        let probs =
                            softmax(&[data[offset], data[offset + 1], data[offset + 2]]);
                        NliResult {
                            contradiction: probs[0] as f64,
                            entailment: probs[1] as f64,
                            neutral: probs[2] as f64,
                        }
                        .score()
                    } else {
                        NliResult::neutral_fallback().score()
                    }
                })
                .collect(),
            Err(e) => {
                tracing::warn!("NLI logit extraction failed: {e}");
                vec![NliResult::neutral_fallback().score(); batch_size]
            }
        }
    }
}

// ── NLI judge pool ───────────────────────────────────────────────────────────

/// RAM-aware NLI inference pool.
///
/// Model is embedded in the binary — `load()` just computes pool sizing.
/// Each worker creates its own ONNX session from the embedded bytes.
pub struct NliJudgePool {
    pub num_workers: usize,
    intra_threads: usize,
}

impl NliJudgePool {
    /// Compute RAM-aware pool configuration.
    ///
    /// No file I/O, no network — the model is embedded in the binary.
    pub fn load(_cache_dir: Option<&std::path::Path>) -> thinkingroot_core::Result<Self> {
        let (workers, intra_threads) = compute_pool_config();
        Ok(Self {
            num_workers: workers,
            intra_threads,
        })
    }

    /// Create a single NLI judge with the pool's intra-thread config.
    pub fn create_judge(&self) -> thinkingroot_core::Result<NliJudge> {
        NliJudge::load(self.intra_threads)
    }
}

// ── Softmax ──────────────────────────────────────────────────────────────────

fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|&x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn softmax_sums_to_one() {
        let probs = softmax(&[2.0, 1.0, 0.1]);
        let sum: f32 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 0.001);
    }

    #[test]
    fn softmax_preserves_order() {
        let probs = softmax(&[5.0, 1.0, 0.1]);
        assert!(probs[0] > probs[1]);
        assert!(probs[1] > probs[2]);
    }

    #[test]
    fn entailment_scores_high() {
        let r = NliResult {
            contradiction: 0.05,
            entailment: 0.90,
            neutral: 0.05,
        };
        assert!(r.score() > 0.8, "got {}", r.score());
    }

    #[test]
    fn contradiction_penalizes() {
        let r = NliResult {
            contradiction: 0.50,
            entailment: 0.40,
            neutral: 0.10,
        };
        assert!(r.score() < 0.20, "got {}", r.score());
    }

    #[test]
    fn score_floors_at_zero() {
        let r = NliResult {
            contradiction: 0.80,
            entailment: 0.10,
            neutral: 0.10,
        };
        assert_eq!(r.score(), 0.0);
    }

    #[test]
    fn pool_config_low_ram() {
        // 3200 MB free → budget = max(1200, 600) = 1200 → workers = 2
        let budget = (3200.0_f64 - RAM_HEADROOM_MB).max(SESSION_MEMORY_MB);
        let max_by_ram = (budget / SESSION_MEMORY_MB) as usize;
        assert_eq!(max_by_ram, 2);
    }

    #[test]
    fn pool_config_very_low_ram() {
        // 1500 MB free → budget = max(-500, 600) = 600 → workers = 1
        let budget = (1500.0_f64 - RAM_HEADROOM_MB).max(SESSION_MEMORY_MB);
        let max_by_ram = (budget / SESSION_MEMORY_MB) as usize;
        assert_eq!(max_by_ram, 1);
    }

    #[test]
    fn pool_config_high_ram() {
        // 32000 MB free, 8 cores → budget = 30000 → max_by_ram = 50 → capped at 8
        let budget = (32000.0_f64 - RAM_HEADROOM_MB).max(SESSION_MEMORY_MB);
        let max_by_ram = (budget / SESSION_MEMORY_MB) as usize;
        let workers = max_by_ram.min(8).clamp(1, 8);
        assert_eq!(workers, 8);
    }

    #[test]
    fn p_core_count_is_positive() {
        assert!(p_core_count() >= 1);
    }

    #[test]
    fn available_ram_is_positive() {
        assert!(available_ram_mb() > 0.0);
    }

    #[test]
    fn compute_pool_config_is_valid() {
        let (workers, intra) = compute_pool_config();
        assert!(workers >= 1);
        assert!(intra >= 1);
    }

    #[test]
    fn embedded_model_bytes_are_present() {
        assert!(ONNX_MODEL.len() > 1_000_000, "model should be >1MB");
        assert!(TOKENIZER_JSON.len() > 100_000, "tokenizer should be >100KB");
    }
}
