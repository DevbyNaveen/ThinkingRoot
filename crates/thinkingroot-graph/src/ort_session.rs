// ─── ONNX Runtime + Tokenizer plumbing ─────────────────────────────
//
// Direct ort + tokenizers integration, replacing fastembed v5
// (Track 32, 2026-05-16). Two model kinds:
//
//   - `EmbeddingModel`     — sentence-transformer-style; mean-pool
//                            + L2-normalise; default `AllMiniLM-L6-v2`
//                            384-dim, 256-token context.
//   - `CrossEncoderModel`  — pair-encoded reranker; sigmoid logits;
//                            default `gte-reranker-modernbert-base`
//                            (149M, 8192-token context).
//
// Why drop fastembed: its `InitOptions::with_cache_dir` still falls
// back to Hugging Face Hub on cache miss — incompatible with our
// install-time bundle contract (`install.sh` stages model files,
// no lazy network at runtime). We talk to ort + tokenizers directly,
// take ONNX + tokenizer paths from explicit `OrtModelPaths`, and
// loud-fail when those files are absent (caller routes the error
// into `Decision::RepairNeeded { failing_check_ids: ["models.bundle_present"] }`).

#[cfg(feature = "vector")]
mod inner {
    use std::path::{Path, PathBuf};

    use ort::session::{Session, builder::GraphOptimizationLevel};
    use ort::value::TensorRef;
    use thinkingroot_core::{Error, Result};
    use tokenizers::{
        EncodeInput, InputSequence, PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer,
        TruncationDirection, TruncationParams, TruncationStrategy,
    };

    /// Default embedding tokenizer cap. gte-modernbert-base supports long
    /// context (up to 8192); 512 comfortably covers claim-sized statements
    /// without truncation while bounding per-call memory/compute.
    pub const EMBED_MAX_LEN: usize = 512;

    /// Default cross-encoder cap. ModernBERT supports 8192 but we
    /// cap at 1024 to stay inside the ~250 ms top-20 latency budget
    /// on M-series CPU. Real per-document length is `(1024 - query_len)`.
    pub const RERANK_MAX_LEN: usize = 1024;

    /// Explicit, on-disk paths to the ONNX file + tokenizer file. The
    /// `InstallManifest::ModelBundle` records these at install time;
    /// callers obtain them from there and pass into `EmbeddingModel::load`
    /// / `CrossEncoderModel::load`.
    #[derive(Clone, Debug)]
    pub struct OrtModelPaths {
        pub onnx_path: PathBuf,
        pub tokenizer_path: PathBuf,
    }

    impl OrtModelPaths {
        pub fn new(onnx: impl AsRef<Path>, tokenizer: impl AsRef<Path>) -> Self {
            Self {
                onnx_path: onnx.as_ref().to_path_buf(),
                tokenizer_path: tokenizer.as_ref().to_path_buf(),
            }
        }

        /// Loud-fail when any file is missing. Caller maps this to
        /// `Decision::RepairNeeded { failing_check_ids: ["models.bundle_present"] }`
        /// so EngineGate shows the "Download models" repair panel.
        pub fn verify_present(&self) -> Result<()> {
            if !self.onnx_path.exists() {
                return Err(Error::GraphStorage(format!(
                    "ONNX model file missing: {} — run `root doctor --fix` to fetch the model bundle",
                    self.onnx_path.display()
                )));
            }
            if !self.tokenizer_path.exists() {
                return Err(Error::GraphStorage(format!(
                    "tokenizer file missing: {} — run `root doctor --fix` to fetch the model bundle",
                    self.tokenizer_path.display()
                )));
            }
            Ok(())
        }
    }

    fn build_tokenizer(path: &Path, max_length: usize) -> Result<Tokenizer> {
        let mut tokenizer = Tokenizer::from_file(path).map_err(|e| {
            Error::GraphStorage(format!(
                "failed to load tokenizer from {}: {e}",
                path.display()
            ))
        })?;

        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "[PAD]".to_string(),
        }));

        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length,
                strategy: TruncationStrategy::LongestFirst,
                stride: 0,
                direction: TruncationDirection::Right,
            }))
            .map_err(|e| {
                Error::GraphStorage(format!("tokenizer truncation setup failed: {e}"))
            })?;

        Ok(tokenizer)
    }

    fn build_session(path: &Path) -> Result<Session> {
        // Optimization level is env-tunable. Level3 (full fusion) balloons
        // load-time memory on large ModernBERT graphs (gte-modernbert embed +
        // reranker) — observed OOM (exit 137) loading the 298MB model on a
        // memory-constrained host, while the 22MB MiniLM loaded fine. Level1
        // (basic) keeps inference correct with a far smaller load-time spike;
        // override with TR_ORT_OPT_LEVEL=disable|1|2|3.
        let opt = match std::env::var("TR_ORT_OPT_LEVEL").ok().as_deref() {
            Some("3") => GraphOptimizationLevel::Level3,
            Some("2") => GraphOptimizationLevel::Level2,
            Some("disable") | Some("0") => GraphOptimizationLevel::Disable,
            _ => GraphOptimizationLevel::Level1,
        };
        Session::builder()
            .map_err(|e| Error::GraphStorage(format!("ort session builder failed: {e}")))?
            .with_optimization_level(opt)
            .map_err(|e| {
                Error::GraphStorage(format!("ort optimization level failed: {e}"))
            })?
            .commit_from_file(path)
            .map_err(|e| {
                Error::GraphStorage(format!(
                    "ort load failed for {}: {e}",
                    path.display()
                ))
            })
    }

    /// Detect whether the session expects a `token_type_ids` input.
    /// BERT-family (incl. AllMiniLM-L6) emits it; ModernBERT (gte-reranker)
    /// skips it. Determined by querying the session's input metadata —
    /// no hardcoded model-family branching.
    fn detect_token_type_ids(session: &Session) -> bool {
        session
            .inputs()
            .iter()
            .any(|i| i.name() == "token_type_ids")
    }

    /// Find the canonical output name. Prefers `last_hidden_state`
    /// for embeddings / `logits` for cross-encoders; falls back to
    /// the first declared output (rare custom exports) with a tracing
    /// warn so the operator sees the substitution.
    fn pick_output_name(session: &Session, preferred: &str) -> Result<String> {
        if let Some(o) = session
            .outputs()
            .iter()
            .find(|o| o.name() == preferred)
        {
            return Ok(o.name().to_string());
        }
        let fallback = session.outputs().first().ok_or_else(|| {
            Error::GraphStorage("ONNX model declares no outputs — corrupt file".into())
        })?;
        tracing::warn!(
            target: "ort_session",
            preferred = preferred,
            fallback = fallback.name(),
            "preferred output not present; using first declared output"
        );
        Ok(fallback.name().to_string())
    }

    /// Embedding model wrapper. Inference takes `&mut self` because
    /// `ort::Session::run` is `&mut self` in rc.11 (the session's
    /// internal allocator scratchpad is borrowed during execution).
    /// Wrap in `Arc<Mutex<EmbeddingModel>>` if concurrent access is
    /// needed; `VectorStore` wraps via `&mut self`-only methods.
    pub struct EmbeddingModel {
        session: Session,
        tokenizer: Tokenizer,
        output_name: String,
        embedding_dim: usize,
        has_token_type_ids: bool,
        max_length: usize,
    }

    impl EmbeddingModel {
        /// Load from explicit paths. `dim` must match what the ONNX
        /// file produces — checked at first inference call (early
        /// dim mismatch surfaces a typed error rather than poisoning
        /// the on-disk `vectors.bin` index).
        pub fn load(paths: &OrtModelPaths, dim: usize, max_length: usize) -> Result<Self> {
            paths.verify_present()?;
            let tokenizer = build_tokenizer(&paths.tokenizer_path, max_length)?;
            let session = build_session(&paths.onnx_path)?;
            let has_token_type_ids = detect_token_type_ids(&session);
            let output_name = pick_output_name(&session, "last_hidden_state")?;

            tracing::info!(
                target: "ort_session",
                onnx = %paths.onnx_path.display(),
                dim,
                max_length,
                token_type_ids = has_token_type_ids,
                output = %output_name,
                "embedding model loaded"
            );

            Ok(Self {
                session,
                tokenizer,
                output_name,
                embedding_dim: dim,
                has_token_type_ids,
                max_length,
            })
        }

        pub fn dim(&self) -> usize {
            self.embedding_dim
        }

        pub fn max_length(&self) -> usize {
            self.max_length
        }

        /// Embed a batch of texts. Returns one `Vec<f32>` per input,
        /// L2-normalised, in input order.
        ///
        /// Pooling: mean over tokens where `attention_mask[i, j] == 1`.
        /// This is the canonical sentence-transformers convention —
        /// CLS-only or last-token pooling would diverge from the
        /// fastembed wire shape and invalidate `vectors.bin`.
        pub fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if texts.is_empty() {
                return Ok(Vec::new());
            }

            let inputs: Vec<InputSequence> =
                texts.iter().map(|s| InputSequence::from(*s)).collect();
            let encodings = self.tokenizer.encode_batch(inputs, true).map_err(|e| {
                Error::GraphStorage(format!("tokenizer encode_batch failed: {e}"))
            })?;

            let batch = encodings.len();
            let seq = encodings
                .iter()
                .map(|e| e.get_ids().len())
                .max()
                .unwrap_or(0);
            if seq == 0 {
                return Err(Error::GraphStorage(
                    "tokenizer produced empty encoding — input texts may all be empty".into(),
                ));
            }

            let mut input_ids: Vec<i64> = Vec::with_capacity(batch * seq);
            let mut attention_mask: Vec<i64> = Vec::with_capacity(batch * seq);
            let mut token_type_ids: Vec<i64> = if self.has_token_type_ids {
                Vec::with_capacity(batch * seq)
            } else {
                Vec::new()
            };

            for enc in &encodings {
                let ids = enc.get_ids();
                let mask = enc.get_attention_mask();
                let tti = enc.get_type_ids();
                // PaddingStrategy::BatchLongest guarantees uniform length.
                debug_assert_eq!(ids.len(), seq, "padding contract broken");
                input_ids.extend(ids.iter().map(|&x| x as i64));
                attention_mask.extend(mask.iter().map(|&x| x as i64));
                if self.has_token_type_ids {
                    token_type_ids.extend(tti.iter().map(|&x| x as i64));
                }
            }

            let shape = [batch as i64, seq as i64];
            let ids_tensor = TensorRef::from_array_view((&shape[..], input_ids.as_slice()))
                .map_err(|e| Error::GraphStorage(format!("input_ids tensor: {e}")))?;
            let mask_tensor =
                TensorRef::from_array_view((&shape[..], attention_mask.as_slice()))
                    .map_err(|e| Error::GraphStorage(format!("attention_mask tensor: {e}")))?;

            let outputs = if self.has_token_type_ids {
                let tti_tensor =
                    TensorRef::from_array_view((&shape[..], token_type_ids.as_slice()))
                        .map_err(|e| {
                            Error::GraphStorage(format!("token_type_ids tensor: {e}"))
                        })?;
                self.session.run(ort::inputs![
                    "input_ids" => ids_tensor,
                    "attention_mask" => mask_tensor,
                    "token_type_ids" => tti_tensor,
                ])
            } else {
                self.session.run(ort::inputs![
                    "input_ids" => ids_tensor,
                    "attention_mask" => mask_tensor,
                ])
            }
            .map_err(|e| Error::GraphStorage(format!("ort inference failed: {e}")))?;

            let output = outputs
                .get(self.output_name.as_str())
                .ok_or_else(|| {
                    Error::GraphStorage(format!(
                        "ort output `{}` missing from result set",
                        self.output_name
                    ))
                })?;
            let (shape_arr, data) = output
                .try_extract_tensor::<f32>()
                .map_err(|e| {
                    Error::GraphStorage(format!("failed to extract embedding tensor: {e}"))
                })?;

            if shape_arr.len() != 3 {
                return Err(Error::GraphStorage(format!(
                    "embedding output expected 3D [B,T,D], got {}D ({:?})",
                    shape_arr.len(),
                    shape_arr
                )));
            }
            let b = shape_arr[0] as usize;
            let t = shape_arr[1] as usize;
            let d = shape_arr[2] as usize;
            if b != batch || t != seq {
                return Err(Error::GraphStorage(format!(
                    "embedding shape mismatch: expected [{batch},{seq},_], got [{b},{t},{d}]"
                )));
            }
            if d != self.embedding_dim {
                return Err(Error::GraphStorage(format!(
                    "embedding dim mismatch: model produces {d}, expected {} — \
                     bundle may be from a different model than configured",
                    self.embedding_dim
                )));
            }

            // Mean-pool with attention mask, then L2-normalise.
            let mut out: Vec<Vec<f32>> = Vec::with_capacity(b);
            for i in 0..b {
                let mut pooled = vec![0.0_f32; d];
                let mut count = 0.0_f32;
                for j in 0..t {
                    let m = attention_mask[i * seq + j];
                    if m > 0 {
                        for k in 0..d {
                            pooled[k] += data[i * t * d + j * d + k];
                        }
                        count += 1.0;
                    }
                }
                if count > 0.0 {
                    let inv = 1.0 / count;
                    for v in pooled.iter_mut() {
                        *v *= inv;
                    }
                }
                let norm: f32 = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > 0.0 {
                    let inv = 1.0 / norm;
                    for v in pooled.iter_mut() {
                        *v *= inv;
                    }
                }
                out.push(pooled);
            }
            Ok(out)
        }
    }

    /// Cross-encoder reranker. Pair-encodes `(query, doc)` and returns
    /// sigmoid-normalised relevance scores in `[0, 1]`. Inference takes
    /// `&mut self` (see `EmbeddingModel` docstring for why).
    pub struct CrossEncoderModel {
        session: Session,
        tokenizer: Tokenizer,
        output_name: String,
        has_token_type_ids: bool,
        max_length: usize,
    }

    impl CrossEncoderModel {
        /// Load from explicit paths. Default `max_length` is
        /// `RERANK_MAX_LEN = 1024` — see constant docstring for why.
        pub fn load(paths: &OrtModelPaths, max_length: usize) -> Result<Self> {
            paths.verify_present()?;
            let tokenizer = build_tokenizer(&paths.tokenizer_path, max_length)?;
            let session = build_session(&paths.onnx_path)?;
            let has_token_type_ids = detect_token_type_ids(&session);
            let output_name = pick_output_name(&session, "logits")?;

            tracing::info!(
                target: "ort_session",
                onnx = %paths.onnx_path.display(),
                max_length,
                token_type_ids = has_token_type_ids,
                output = %output_name,
                "cross-encoder model loaded"
            );

            Ok(Self {
                session,
                tokenizer,
                output_name,
                has_token_type_ids,
                max_length,
            })
        }

        pub fn max_length(&self) -> usize {
            self.max_length
        }

        /// Score `(query, document)` pairs. Returns one score per
        /// document, in input order, sigmoid-normalised to `[0, 1]`.
        /// Higher = more relevant.
        pub fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            if docs.is_empty() {
                return Ok(Vec::new());
            }

            let pairs: Vec<EncodeInput> = docs
                .iter()
                .map(|d| {
                    let q: InputSequence = query.into();
                    let dd: InputSequence = (*d).into();
                    EncodeInput::Dual(q, dd)
                })
                .collect();

            let encodings = self.tokenizer.encode_batch(pairs, true).map_err(|e| {
                Error::GraphStorage(format!("tokenizer encode_batch failed: {e}"))
            })?;

            let batch = encodings.len();
            let seq = encodings
                .iter()
                .map(|e| e.get_ids().len())
                .max()
                .unwrap_or(0);
            if seq == 0 {
                return Err(Error::GraphStorage(
                    "tokenizer produced empty encoding for rerank pairs".into(),
                ));
            }

            let mut input_ids: Vec<i64> = Vec::with_capacity(batch * seq);
            let mut attention_mask: Vec<i64> = Vec::with_capacity(batch * seq);
            let mut token_type_ids: Vec<i64> = if self.has_token_type_ids {
                Vec::with_capacity(batch * seq)
            } else {
                Vec::new()
            };

            for enc in &encodings {
                let ids = enc.get_ids();
                let mask = enc.get_attention_mask();
                let tti = enc.get_type_ids();
                debug_assert_eq!(ids.len(), seq);
                input_ids.extend(ids.iter().map(|&x| x as i64));
                attention_mask.extend(mask.iter().map(|&x| x as i64));
                if self.has_token_type_ids {
                    token_type_ids.extend(tti.iter().map(|&x| x as i64));
                }
            }

            let shape = [batch as i64, seq as i64];
            let ids_tensor = TensorRef::from_array_view((&shape[..], input_ids.as_slice()))
                .map_err(|e| Error::GraphStorage(format!("input_ids tensor: {e}")))?;
            let mask_tensor =
                TensorRef::from_array_view((&shape[..], attention_mask.as_slice()))
                    .map_err(|e| Error::GraphStorage(format!("attention_mask tensor: {e}")))?;

            let outputs = if self.has_token_type_ids {
                let tti_tensor =
                    TensorRef::from_array_view((&shape[..], token_type_ids.as_slice()))
                        .map_err(|e| {
                            Error::GraphStorage(format!("token_type_ids tensor: {e}"))
                        })?;
                self.session.run(ort::inputs![
                    "input_ids" => ids_tensor,
                    "attention_mask" => mask_tensor,
                    "token_type_ids" => tti_tensor,
                ])
            } else {
                self.session.run(ort::inputs![
                    "input_ids" => ids_tensor,
                    "attention_mask" => mask_tensor,
                ])
            }
            .map_err(|e| Error::GraphStorage(format!("ort inference failed: {e}")))?;

            let output = outputs
                .get(self.output_name.as_str())
                .ok_or_else(|| {
                    Error::GraphStorage(format!(
                        "ort output `{}` missing from result set",
                        self.output_name
                    ))
                })?;
            let (shape_arr, data) = output
                .try_extract_tensor::<f32>()
                .map_err(|e| {
                    Error::GraphStorage(format!("failed to extract logits tensor: {e}"))
                })?;

            // Acceptable shapes: [B], [B, 1]. Anything else is a
            // wrong model.
            let logits: &[f32] = match shape_arr.len() {
                1 if shape_arr[0] as usize == batch => data,
                2 if shape_arr[0] as usize == batch && shape_arr[1] == 1 => data,
                _ => {
                    return Err(Error::GraphStorage(format!(
                        "cross-encoder output expected [B] or [B,1], got shape {shape_arr:?} (batch={batch})"
                    )));
                }
            };
            if logits.len() != batch {
                return Err(Error::GraphStorage(format!(
                    "cross-encoder produced {} scores for batch {batch}",
                    logits.len()
                )));
            }

            // Sigmoid → [0, 1]. We use the numerically stable form
            // for large positive logits (1 / (1 + e^-x)) and
            // (e^x / (1 + e^x)) for large negatives — but f32 sigmoid
            // is fine without the split for any realistic logit
            // magnitude (|x| < 35).
            Ok(logits
                .iter()
                .map(|&logit| 1.0 / (1.0 + (-logit).exp()))
                .collect())
        }
    }
}

#[cfg(not(feature = "vector"))]
mod inner {
    use std::path::{Path, PathBuf};
    use thinkingroot_core::{Error, Result};

    pub const EMBED_MAX_LEN: usize = 256;
    pub const RERANK_MAX_LEN: usize = 1024;

    #[derive(Clone, Debug)]
    pub struct OrtModelPaths {
        pub onnx_path: PathBuf,
        pub tokenizer_path: PathBuf,
    }

    impl OrtModelPaths {
        pub fn new(onnx: impl AsRef<Path>, tokenizer: impl AsRef<Path>) -> Self {
            Self {
                onnx_path: onnx.as_ref().to_path_buf(),
                tokenizer_path: tokenizer.as_ref().to_path_buf(),
            }
        }
        pub fn verify_present(&self) -> Result<()> {
            Ok(())
        }
    }

    pub struct EmbeddingModel;

    impl EmbeddingModel {
        pub fn load(
            _paths: &OrtModelPaths,
            _dim: usize,
            _max_length: usize,
        ) -> Result<Self> {
            Err(Error::GraphStorage(
                "embedding model unavailable: `vector` feature disabled at compile time".into(),
            ))
        }
        pub fn dim(&self) -> usize {
            0
        }
        pub fn max_length(&self) -> usize {
            0
        }
        pub fn embed(&mut self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(Vec::new())
        }
    }

    pub struct CrossEncoderModel;

    impl CrossEncoderModel {
        pub fn load(_paths: &OrtModelPaths, _max_length: usize) -> Result<Self> {
            Err(Error::GraphStorage(
                "cross-encoder unavailable: `vector` feature disabled at compile time".into(),
            ))
        }
        pub fn max_length(&self) -> usize {
            0
        }
        pub fn rerank(&mut self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            Ok(vec![0.0; docs.len()])
        }
    }
}

pub use inner::{
    CrossEncoderModel, EMBED_MAX_LEN, EmbeddingModel, OrtModelPaths, RERANK_MAX_LEN,
};

use std::path::PathBuf;

/// Resolve the canonical model-bundle directory.
///
/// Resolution order:
///   1. `THINKINGROOT_MODELS_DIR` env var (tests + portable installs)
///   2. `<dirs::cache_dir()>/thinkingroot/models/`
///      (macOS: `~/Library/Caches/thinkingroot/models/`;
///       Linux: `~/.cache/thinkingroot/models/`;
///       Windows: `%LOCALAPPDATA%\thinkingroot\models\`)
///   3. `./.thinkingroot-models/` relative fallback when no cache dir
///      can be resolved (rare; defensive only)
///
/// Doesn't verify existence — pair with `OrtModelPaths::verify_present()`.
pub fn default_model_bundle_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("THINKINGROOT_MODELS_DIR") {
        if !override_dir.is_empty() {
            return PathBuf::from(override_dir);
        }
    }
    dirs::cache_dir()
        .map(|d| d.join("thinkingroot").join("models"))
        .unwrap_or_else(|| PathBuf::from(".thinkingroot-models"))
}

/// Canonical paths for the embedding model bundle entry.
/// File names match what `install.sh` / `install.ps1` download from
/// `github.com/DevbyNaveen/releases/releases/download/models-v1/`.
pub fn default_embed_paths() -> OrtModelPaths {
    let dir = default_model_bundle_dir();
    OrtModelPaths::new(dir.join("embed.onnx"), dir.join("embed.tokenizer.json"))
}

/// Canonical paths for the cross-encoder reranker bundle entry.
pub fn default_rerank_paths() -> OrtModelPaths {
    let dir = default_model_bundle_dir();
    OrtModelPaths::new(dir.join("rerank.onnx"), dir.join("rerank.tokenizer.json"))
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ort_model_paths_construct() {
        let p = OrtModelPaths::new("/tmp/embed.onnx", "/tmp/embed.tokenizer.json");
        assert!(p.onnx_path.ends_with("embed.onnx"));
        assert!(p.tokenizer_path.ends_with("embed.tokenizer.json"));
    }

    #[test]
    fn verify_present_returns_typed_error_when_missing() {
        let p = OrtModelPaths::new(
            "/tmp/definitely-does-not-exist-thinkingroot-test.onnx",
            "/tmp/definitely-does-not-exist-thinkingroot-test.json",
        );
        let result = p.verify_present();
        #[cfg(feature = "vector")]
        {
            assert!(result.is_err(), "missing files must surface an error");
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("model file missing") || msg.contains("tokenizer file missing"),
                "expected helpful repair hint in error message, got: {msg}"
            );
            assert!(
                msg.contains("root doctor --fix"),
                "error must direct user to the repair flow, got: {msg}"
            );
        }
        #[cfg(not(feature = "vector"))]
        {
            assert!(
                result.is_ok(),
                "verify_present is a no-op when `vector` feature is disabled"
            );
        }
    }

    #[cfg(not(feature = "vector"))]
    #[test]
    fn stubs_return_empty_when_vector_disabled() {
        let p = OrtModelPaths::new("/tmp/a.onnx", "/tmp/a.json");
        assert!(EmbeddingModel::load(&p, 384, EMBED_MAX_LEN).is_err());
        assert!(CrossEncoderModel::load(&p, RERANK_MAX_LEN).is_err());
    }
}
