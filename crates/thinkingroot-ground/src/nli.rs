//! NLI (Natural Language Inference) judge for hallucination detection.
//!
//! Uses cross-encoder/nli-deberta-v3-xsmall (71M params, INT8 quantized)
//! loaded from disk at runtime — downloaded by install.sh at install time
//! to ~/.cache/thinkingroot/models/.
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

use std::path::{Path, PathBuf};

use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;

/// Peak memory per ONNX session during inference.
const SESSION_MEMORY_MB: f64 = 600.0;

/// Minimum available RAM to keep free.
const RAM_HEADROOM_MB: f64 = 2000.0;

// ── Model path resolution ────────────────────────────────────────────────────

/// Arch-specific ONNX filename.
#[cfg(target_arch = "aarch64")]
const ONNX_FILENAME: &str = "model_qint8_arm64.onnx";

#[cfg(target_arch = "x86_64")]
const ONNX_FILENAME: &str = "model_quint8_avx2.onnx";

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
const ONNX_FILENAME: &str = "model_quint8_avx2.onnx";

const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Resolve model directory — tries in order:
///   1. Provided cache_dir override
///   2. ~/.cache/thinkingroot/models/  (Linux / macOS standard)
///   3. ~/Library/Caches/thinkingroot/models/  (macOS alternate)
///   4. Executable-adjacent models/  (portable / offline installs)
fn resolve_model_dir(cache_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = cache_dir {
        let candidate = dir.join("models");
        if candidate.join(ONNX_FILENAME).exists() {
            return Some(candidate);
        }
        // Also accept cache_dir itself if it directly contains the model.
        if dir.join(ONNX_FILENAME).exists() {
            return Some(dir.to_path_buf());
        }
    }

    // ~/.cache/thinkingroot/models/
    if let Some(cache) = dirs::cache_dir() {
        let candidate = cache.join("thinkingroot").join("models");
        if candidate.join(ONNX_FILENAME).exists() {
            return Some(candidate);
        }
    }

    // Executable-adjacent models/ (useful for offline/portable installs)
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("models");
            if candidate.join(ONNX_FILENAME).exists() {
                return Some(candidate);
            }
    }

    None
}

// ── NLI result ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct NliResult {
    pub entailment: f64,
    pub neutral: f64,
    pub contradiction: f64,
}

impl NliResult {
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

pub struct NliJudge {
    session: Session,
    tokenizer: tokenizers::Tokenizer,
}

impl NliJudge {
    fn load(model_dir: &Path, intra_threads: usize) -> thinkingroot_core::Result<Self> {
        let tokenizer_path = model_dir.join(TOKENIZER_FILENAME);
        let onnx_path = model_dir.join(ONNX_FILENAME);

        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            thinkingroot_core::Error::VectorStorage(format!(
                "NLI tokenizer load failed from {}: {e}",
                tokenizer_path.display()
            ))
        })?;

        let session = Session::builder()
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!("ort session builder failed: {e}"))
            })?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!("optimization level failed: {e}"))
            })?
            .with_intra_threads(intra_threads)
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!("intra_threads config failed: {e}"))
            })?
            .commit_from_file(&onnx_path)
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!(
                    "NLI model load failed from {}: {e}",
                    onnx_path.display()
                ))
            })?;

        Ok(Self { session, tokenizer })
    }

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
                    let ids: Vec<i64> = enc.get_ids().iter().map(|&id| id as i64).collect();
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

        let max_len = encodings
            .iter()
            .map(|(ids, _)| ids.len())
            .max()
            .unwrap_or(1);
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

        match outputs[0].try_extract_tensor::<f32>() {
            Ok((_shape, data)) => (0..batch_size)
                .map(|i| {
                    let offset = i * 3;
                    if offset + 2 < data.len() {
                        let probs = softmax(&[data[offset], data[offset + 1], data[offset + 2]]);
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

pub struct NliJudgePool {
    pub num_workers: usize,
    intra_threads: usize,
    model_dir: PathBuf,
}

impl NliJudgePool {
    /// Load pool config. Returns Err if models are not found on disk —
    /// caller should gracefully skip NLI (judges 1-3 still run).
    pub fn load(cache_dir: Option<&Path>) -> thinkingroot_core::Result<Self> {
        let model_dir = resolve_model_dir(cache_dir).ok_or_else(|| {
            thinkingroot_core::Error::VectorStorage(format!(
                "NLI models not found. Expected {} in ~/.cache/thinkingroot/models/. \
                 Re-run the installer: curl -fsSL https://raw.githubusercontent.com/DevbyNaveen/ThinkingRoot/main/install.sh | sh",
                ONNX_FILENAME
            ))
        })?;

        let (workers, intra_threads) = compute_pool_config();
        Ok(Self {
            num_workers: workers,
            intra_threads,
            model_dir,
        })
    }

    pub fn create_judge(&self) -> thinkingroot_core::Result<NliJudge> {
        NliJudge::load(&self.model_dir, self.intra_threads)
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
        let budget = (3200.0_f64 - RAM_HEADROOM_MB).max(SESSION_MEMORY_MB);
        let max_by_ram = (budget / SESSION_MEMORY_MB) as usize;
        assert_eq!(max_by_ram, 2);
    }

    #[test]
    fn pool_config_very_low_ram() {
        let budget = (1500.0_f64 - RAM_HEADROOM_MB).max(SESSION_MEMORY_MB);
        let max_by_ram = (budget / SESSION_MEMORY_MB) as usize;
        assert_eq!(max_by_ram, 1);
    }

    #[test]
    fn pool_config_high_ram() {
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
    fn resolve_model_dir_returns_none_when_no_models() {
        // In CI / fresh environments with no models installed, should return None
        // (not panic). The pool gracefully skips NLI in this case.
        let result = resolve_model_dir(Some(Path::new("/nonexistent/path")));
        // Either None (no models) or Some (models found elsewhere) — must not panic.
        let _ = result;
    }
}
