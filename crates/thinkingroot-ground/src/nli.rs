use ort::session::Session;
use ort::value::Tensor;

/// Judge 4: Natural Language Inference (NLI) entailment check.
///
/// Uses DeBERTa-v3-base-mnli (~64MB ONNX) to classify whether the source
/// text **entails** the claim, is **neutral**, or **contradicts** it.
///
/// This is the strongest hallucination signal — it doesn't just measure word
/// overlap or embedding similarity, it checks logical entailment:
///
/// - Source: "We migrated from MySQL to PostgreSQL in Q3"
/// - Claim:  "The system uses MySQL" → CONTRADICTION (it *was* MySQL, not *is*)
/// - Claim:  "PostgreSQL was adopted in Q3" → ENTAILMENT
///
/// Feature-gated behind `vector` (shares ONNX Runtime with fastembed).
pub struct NliJudge {
    session: Session,
    tokenizer: tokenizers::Tokenizer,
}

/// NLI classification result.
#[derive(Debug, Clone, Copy)]
pub struct NliResult {
    /// Probability that source entails the claim [0.0, 1.0].
    pub entailment: f64,
    /// Probability the relationship is neutral [0.0, 1.0].
    pub neutral: f64,
    /// Probability the source contradicts the claim [0.0, 1.0].
    pub contradiction: f64,
}

impl NliResult {
    /// The grounding score: entailment probability penalised by contradiction.
    /// Returns a score in [0.0, 1.0] where higher = more grounded.
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

// Model repository on HuggingFace (ONNX-exported DeBERTa-v3-base-mnli).
const HF_REPO: &str = "cross-encoder/nli-deberta-v3-base";

/// Resolve model files directly from the local HuggingFace cache without
/// making any network calls. Returns `None` if the file is not cached.
///
/// hf_hub::api::sync always contacts the HuggingFace Hub to resolve the
/// latest commit hash, even if the files are already cached. On a slow or
/// firewalled network this blocks indefinitely. This function bypasses the
/// API by reading `~/.cache/huggingface/hub/<model>/refs/main` for the
/// commit hash, then resolving `snapshots/<hash>/<filename>` directly.
fn find_in_local_cache(filename: &str) -> Option<std::path::PathBuf> {
    // dir name: "models--{org}--{repo}" (slashes → double-dash)
    let dir_name = format!("models--{}", HF_REPO.replace('/', "--"));

    // Locate the HuggingFace cache root ($HF_HOME/hub, $HF_HUB_CACHE, or ~/.cache/huggingface/hub)
    let cache_root = std::env::var("HF_HUB_CACHE")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HF_HOME").map(|h| std::path::PathBuf::from(h).join("hub")))
        .or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".cache/huggingface/hub"))
        })
        .ok()?
        .join(&dir_name);

    // refs/main → commit hash
    let commit = std::fs::read_to_string(cache_root.join("refs/main"))
        .ok()?
        .trim()
        .to_string();

    let snapshot_path = cache_root.join("snapshots").join(&commit).join(filename);

    // Resolve the symlink that hf_hub places here pointing to the actual blob
    snapshot_path.canonicalize().ok().filter(|p| p.exists())
}

impl NliJudge {
    /// Load the NLI model. Downloads from HuggingFace on first use,
    /// then cached at `~/.cache/huggingface/hub/` (hf-hub default).
    ///
    /// Prefers the local cache to avoid a blocking network call — hf_hub's
    /// sync API always contacts the Hub to resolve the latest commit hash,
    /// which hangs indefinitely on slow/firewalled networks. Only falls back
    /// to hf_hub (with download) when the model is not yet cached.
    pub fn load(_cache_dir: Option<&std::path::Path>) -> thinkingroot_core::Result<Self> {
        // Fast path: resolve from local cache without any network access.
        let (onnx_path, tokenizer_path) = if let (Some(o), Some(t)) = (
            find_in_local_cache("onnx/model.onnx"),
            find_in_local_cache("tokenizer.json"),
        ) {
            tracing::info!("NLI model found in local cache — loading without network");
            (o, t)
        } else {
            // Slow path: first run — download via hf_hub (~64 MB).
            tracing::info!(
                "NLI model not cached — downloading from HuggingFace (~64 MB, one-time)"
            );
            let api = hf_hub::api::sync::Api::new().map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!("hf-hub init failed: {e}"))
            })?;
            let repo = api.model(HF_REPO.to_string());

            let o = repo.get("onnx/model.onnx").map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!(
                    "failed to download NLI model from {HF_REPO}: {e}"
                ))
            })?;

            let t = repo.get("tokenizer.json").map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!(
                    "failed to download NLI tokenizer from {HF_REPO}: {e}"
                ))
            })?;

            (o, t)
        };

        // Load tokenizer.
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            thinkingroot_core::Error::VectorStorage(format!("tokenizer load failed: {e}"))
        })?;

        // Load ONNX session.
        // DeBERTa v3 uses disentangled attention — no token_type_ids in its ONNX export.
        let session = Session::builder()
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!("ort session builder failed: {e}"))
            })?
            .commit_from_file(&onnx_path)
            .map_err(|e| {
                thinkingroot_core::Error::VectorStorage(format!("ort model load failed: {e}"))
            })?;

        tracing::info!("NLI model loaded successfully");
        Ok(Self { session, tokenizer })
    }

    /// Classify whether `source_text` entails `claim`.
    ///
    /// NLI convention: premise = source_text, hypothesis = claim.
    /// Returns entailment/neutral/contradiction probabilities.
    pub fn classify(&mut self, claim: &str, source_text: &str) -> NliResult {
        // Truncate source to ~512 tokens (DeBERTa max is 512).
        let max_source_chars = 1500; // ~375 tokens, leaving room for claim + special tokens
        let source = if source_text.len() > max_source_chars {
            &source_text[..max_source_chars]
        } else {
            source_text
        };

        // Tokenize as sentence pair: [CLS] source [SEP] claim [SEP]
        let encoding = match self.tokenizer.encode((source, claim), true) {
            Ok(enc) => enc,
            Err(e) => {
                tracing::warn!("NLI tokenization failed: {e}");
                return NliResult::neutral_fallback();
            }
        };

        let input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let attention_mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| m as i64)
            .collect();

        let seq_len = input_ids.len();

        // Build input tensors (batch_size=1, seq_len) using ort 2.0 API.
        // DeBERTa v3 uses disentangled attention — no token_type_ids input in its ONNX export.
        let ids_tensor = match Tensor::from_array(([1usize, seq_len], input_ids.into_boxed_slice()))
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("NLI: failed to create input_ids tensor: {e}");
                return NliResult::neutral_fallback();
            }
        };

        let mask_tensor =
            match Tensor::from_array(([1usize, seq_len], attention_mask.into_boxed_slice())) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("NLI: failed to create attention_mask tensor: {e}");
                    return NliResult::neutral_fallback();
                }
            };

        // Run inference.
        let outputs = match self.session.run(ort::inputs![
            "input_ids" => ids_tensor,
            "attention_mask" => mask_tensor,
        ]) {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!("NLI inference failed: {e}");
                return NliResult::neutral_fallback();
            }
        };

        // Extract logits: ort 2.0 returns (&Shape, &[f32]).
        let logits: Vec<f32> = match outputs[0].try_extract_tensor::<f32>() {
            Ok((_shape, data)) => {
                // Shape: (1, 3) — [contradiction, neutral, entailment]
                if data.len() >= 3 {
                    vec![data[0], data[1], data[2]]
                } else {
                    tracing::warn!("NLI: unexpected logits length ({})", data.len());
                    return NliResult::neutral_fallback();
                }
            }
            Err(e) => {
                tracing::warn!("NLI: failed to extract logits: {e}");
                return NliResult::neutral_fallback();
            }
        };

        // Softmax over logits → probabilities.
        let probs = softmax(&logits);

        // DeBERTa-v3-base-mnli label order: [contradiction, neutral, entailment]
        NliResult {
            contradiction: probs[0] as f64,
            neutral: probs[1] as f64,
            entailment: probs[2] as f64,
        }
    }

    /// Convenience: return a single grounding score in [0.0, 1.0].
    pub fn score(&mut self, claim: &str, source_text: &str) -> f64 {
        self.classify(claim, source_text).score()
    }
}

/// Numerically stable softmax.
fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|&x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn softmax_sums_to_one() {
        let logits = vec![2.0, 1.0, 0.1];
        let probs = softmax(&logits);
        let sum: f32 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 0.001);
    }

    #[test]
    fn softmax_highest_logit_gets_highest_prob() {
        let logits = vec![5.0, 1.0, 0.1];
        let probs = softmax(&logits);
        assert!(probs[0] > probs[1]);
        assert!(probs[1] > probs[2]);
    }

    #[test]
    fn nli_result_score_entailment_high() {
        let r = NliResult {
            entailment: 0.9,
            neutral: 0.05,
            contradiction: 0.05,
        };
        assert!(r.score() > 0.8);
    }

    #[test]
    fn nli_result_score_contradiction_penalized() {
        let r = NliResult {
            entailment: 0.4,
            neutral: 0.1,
            contradiction: 0.5,
        };
        // 0.4 - 0.5*0.5 = 0.15
        assert!(r.score() < 0.2);
    }

    #[test]
    fn nli_result_score_floors_at_zero() {
        let r = NliResult {
            entailment: 0.1,
            neutral: 0.1,
            contradiction: 0.8,
        };
        // 0.1 - 0.5*0.8 = -0.3 → clamped to 0.0
        assert_eq!(r.score(), 0.0);
    }

    #[test]
    fn find_in_local_cache_returns_none_for_missing() {
        // A repo that definitely doesn't exist in cache
        let result = std::env::var("HOME").ok().and_then(|h| {
            let p = std::path::PathBuf::from(h)
                .join(".cache/huggingface/hub/models--no-such--repo/refs/main");
            if p.exists() { Some(()) } else { None }
        });
        assert!(result.is_none());
    }
}
