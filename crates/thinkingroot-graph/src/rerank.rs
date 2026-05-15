// SOTA Lever 1 — cross-encoder reranker via `ort_session::CrossEncoderModel`.
//
// Wires `gte-reranker-modernbert-base` (149M params, ~300 MB on disk,
// 8192-tok context, Apache-2.0, Alibaba-NLP) as **Layer 6.5** of the
// hybrid-retrieval pipeline. Default-ON in `ScoringProfile::default()`
// — RAM budget allows it (~700 MB resident with both models loaded,
// less than Cursor / VS Code-with-AI), latency ~150-250 ms top-20 on
// modern CPU which is invisible behind LLM streaming TTFT.
//
// Replaces the prior Jina-Reranker-v1-Turbo wiring (Track 26, commit
// `d4df1fe`) — Jina was the path-of-least-resistance pick because
// fastembed v5 had it built-in; the swap is the deferred follow-up
// the Track 26 commit body explicitly called out. gte-modernbert
// scores higher on independent benchmarks (Hit@1 83% vs Jina's
// ~75-80%) and supports 8× the context (8192 vs 512 tokens), which
// actually matters for paper / code rerank.
//
// On systems where the model bundle is missing (fresh install where
// the user opted out via TR_SKIP_MODELS or model files manually
// deleted), `CrossEncoder::new` itself succeeds (deferred-init) and
// the first `rerank` call returns an `Err(Error::GraphStorage)`
// with a `root doctor --fix` repair hint. Callers in
// `intelligence/hybrid.rs` treat rerank failure as "fall back to
// fused score order" so retrieval never goes dark — only the
// rerank lift is lost.

#[cfg(feature = "vector")]
mod inner {
    use std::path::Path;
    use std::sync::Mutex;

    use crate::ort_session::{
        default_rerank_paths, CrossEncoderModel as OrtCrossEncoderModel, OrtModelPaths,
        RERANK_MAX_LEN,
    };
    use thinkingroot_core::{Error, Result};

    /// Cross-encoder reranker, lazy-loaded.
    ///
    /// `model` is initialised on the first `rerank` call so workspace
    /// open stays instant (~ms) — ORT session creation is slow even
    /// when the model file is already on disk.
    ///
    /// `model` lives behind a `Mutex` so the rerank entry point can
    /// take `&self` (matches `VectorStore::search_by_vector`'s
    /// shared-borrow shape; the underlying `OrtCrossEncoderModel`
    /// itself is `&self`-callable, but we need interior mutability
    /// for the Option-replace pattern).
    pub struct CrossEncoder {
        model: Mutex<Option<OrtCrossEncoderModel>>,
        paths: OrtModelPaths,
        max_length: usize,
    }

    impl CrossEncoder {
        /// Construct a deferred-init reranker pointing at the canonical
        /// bundle dir (`<cache_dir>/thinkingroot/models/rerank.{onnx,tokenizer.json}`,
        /// or `$THINKINGROOT_MODELS_DIR` override).
        ///
        /// `workspace_path` is retained in the signature for caller
        /// compatibility but not used — model files live in the
        /// process-global bundle dir, shared across workspaces, so a
        /// single ~300 MB download serves every workspace.
        pub fn new(_workspace_path: &Path) -> Result<Self> {
            Ok(Self {
                model: Mutex::new(None),
                paths: default_rerank_paths(),
                max_length: RERANK_MAX_LEN,
            })
        }

        /// Override the bundle paths (tests, future per-workspace
        /// overrides). Must be called before the first `rerank`.
        pub fn with_paths(mut self, paths: OrtModelPaths) -> Self {
            self.paths = paths;
            self
        }

        /// Override the max-length cap (advanced tuning; default
        /// `RERANK_MAX_LEN = 1024`). Higher = more context but more
        /// latency; ModernBERT supports up to 8192.
        pub fn with_max_length(mut self, max_length: usize) -> Self {
            self.max_length = max_length;
            self
        }

        /// Score `(query, document)` pairs and return blended scores in
        /// the order of `documents`. Higher is more relevant.
        ///
        /// **Latency** (top-20 on M-series CPU, gte-modernbert FP16):
        ///   - ~150-250 ms warm
        ///   - +2-3 s cold first load (ORT session creation), hidden
        ///     behind warm-on-boot in `serve.rs` post-Phase-B
        ///
        /// Returns an empty `Vec` when `documents` is empty (saves the
        /// model-load cost on no-op calls).
        pub fn rerank(&self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
            if documents.is_empty() {
                return Ok(Vec::new());
            }

            let mut guard = self
                .model
                .lock()
                .map_err(|_| Error::GraphStorage("rerank mutex poisoned".into()))?;

            if guard.is_none() {
                tracing::info!(
                    target: "rerank",
                    "loading cross-encoder model (first use)…"
                );
                let model = OrtCrossEncoderModel::load(&self.paths, self.max_length)?;
                *guard = Some(model);
                tracing::info!(target: "rerank", "cross-encoder loaded");
            }

            let model = guard.as_mut().expect("just-loaded");
            model.rerank(query, documents)
        }

        /// Whether the model has been loaded yet. Useful for telemetry
        /// and for tests that want to verify lazy-load behaviour
        /// without triggering inference.
        pub fn is_loaded(&self) -> bool {
            self.model.lock().map(|g| g.is_some()).unwrap_or(false)
        }
    }
}

#[cfg(not(feature = "vector"))]
mod inner {
    use std::path::Path;
    use thinkingroot_core::Result;

    use crate::ort_session::OrtModelPaths;

    /// No-op reranker compiled when the `vector` feature is disabled.
    /// Matches the public surface of the real reranker so downstream
    /// callers (e.g. `intelligence/hybrid.rs`) need no `cfg!` guards.
    pub struct CrossEncoder;

    impl CrossEncoder {
        pub fn new(_workspace_path: &Path) -> Result<Self> {
            Ok(Self)
        }

        pub fn with_paths(self, _paths: OrtModelPaths) -> Self {
            self
        }

        pub fn with_max_length(self, _max_length: usize) -> Self {
            self
        }

        /// Returns an empty score vec — callers detect the no-op via
        /// `scores.is_empty()` and fall back to their pre-rerank
        /// ordering. (Returning zero-filled scores would cause the
        /// blend formula `0.7 * ce + 0.3 * fused` to silently
        /// down-weight everything to 0.3× — that's exactly the
        /// silent-degradation failure mode we forbid.)
        pub fn rerank(&self, _query: &str, _documents: &[&str]) -> Result<Vec<f32>> {
            Ok(Vec::new())
        }

        pub fn is_loaded(&self) -> bool {
            false
        }
    }
}

pub use inner::CrossEncoder;

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_encoder_construct_without_model_load() {
        // The constructor must NOT load the model — that happens on
        // the first `rerank` call. This test guarantees workspace
        // open stays instant.
        let dir = tempfile::tempdir().unwrap();
        let ce = CrossEncoder::new(dir.path()).unwrap();
        assert!(!ce.is_loaded(), "model must lazy-load on first rerank call");
    }

    #[test]
    fn empty_documents_returns_empty_scores() {
        // Empty inputs are a hot path during system warm-up. Don't
        // pay the model-load cost when there's nothing to score.
        let dir = tempfile::tempdir().unwrap();
        let ce = CrossEncoder::new(dir.path()).unwrap();
        let scores = ce.rerank("anything", &[]).unwrap();
        assert!(scores.is_empty());
        assert!(!ce.is_loaded(), "empty input must not trigger model load");
    }

    #[cfg(not(feature = "vector"))]
    #[test]
    fn noop_stub_returns_empty_when_vector_disabled() {
        // When the `vector` feature is disabled, the rerank entry
        // point is a no-op that returns an empty Vec. Callers detect
        // this shape (`scores.is_empty() && !documents.is_empty()`)
        // and fall back to their pre-rerank ordering — never
        // silently collapsing scores to a degenerate value.
        let dir = tempfile::tempdir().unwrap();
        let ce = CrossEncoder::new(dir.path()).unwrap();
        let docs = vec!["doc one", "doc two", "doc three"];
        let scores = ce.rerank("query", &docs).unwrap();
        assert!(scores.is_empty());
    }

    #[cfg(feature = "vector")]
    #[test]
    #[ignore = "requires gte-reranker-modernbert-base ONNX bundle staged at default_model_bundle_dir()"]
    fn gte_modernbert_reranks_relevant_higher_than_irrelevant() {
        let dir = tempfile::tempdir().unwrap();
        let ce = CrossEncoder::new(dir.path()).unwrap();
        let docs = vec![
            "The capital of France is Paris.",
            "I ate a sandwich for lunch.",
            "Paris is the largest city in France.",
        ];
        let scores = ce
            .rerank("What is the capital of France?", &docs)
            .expect("rerank failed");
        assert_eq!(scores.len(), 3);
        // Doc 0 and Doc 2 are about Paris; Doc 1 is unrelated.
        assert!(
            scores[0] > scores[1],
            "Paris doc must outrank sandwich doc (got {scores:?})"
        );
        assert!(
            scores[2] > scores[1],
            "Paris doc 2 must outrank sandwich doc (got {scores:?})"
        );
        // Sigmoid-normalised — every score must fall in [0, 1].
        for (i, s) in scores.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(s),
                "score {i} = {s} outside [0, 1] — sigmoid normalisation broken"
            );
        }
    }
}
