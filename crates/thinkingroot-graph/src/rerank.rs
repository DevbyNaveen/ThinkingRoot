// SOTA Lever 1 — cross-encoder reranker via fastembed v5's TextRerank.
//
// Wires `JinaRerankerV1TurboEn` (~137M params, ~280MB on disk) as an
// **opt-in** final stage for hybrid retrieval. Default-off because the
// rerank pass adds ~120-200ms p95 on top-20 CPU inference, which busts
// our `<25ms p95` instant-retrieval budget (see
// `.claude/rules/hybrid-retrieval.md` §"Routing + transport").
//
// Enabled paths today: Playground deep-mode, paper synthesis, /find-gaps
// — flows where the user already accepts >100ms latency for higher
// accuracy.
//
// Latency-budget-tight paths (chat, AEP probes, MCP `hybrid_retrieve`
// when `score_with_hybrid: true`) leave the existing BM25 reranker in
// `crates/thinkingroot-serve/src/intelligence/reranker.rs` as the final
// stage. A future ms-marco-TinyBERT-L-2-v2 ONNX wiring (~4MB, ~10ms top-20)
// would be the instant-mode reranker — that requires going below fastembed
// to `ort` + `tokenizers` directly because fastembed v5's RerankerModel
// enum (BGE/Jina families) doesn't ship TinyBERT-L-2-v2.

#[cfg(feature = "vector")]
mod inner {
    use std::path::Path;
    use std::sync::Mutex;

    use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
    use thinkingroot_core::{Error, Result};

    /// Cross-encoder reranker, lazy-loaded.
    ///
    /// `model` is initialised on the first `rerank` call so workspace
    /// open stays instant (~ms) — ONNX session creation is slow even when
    /// the model file is already cached on disk.
    ///
    /// `model` lives behind a `Mutex` so the rerank entry point can take
    /// `&self` (matches `VectorStore::search_by_vector`'s shared-borrow
    /// shape; `TextRerank::rerank` requires `&mut self` internally for
    /// inference batch state).
    pub struct CrossEncoder {
        model: Mutex<Option<TextRerank>>,
        model_kind: RerankerModel,
        cache_dir: std::path::PathBuf,
    }

    impl CrossEncoder {
        /// Construct a deferred-init reranker.
        ///
        /// Default model: `JinaRerankerV1TurboEn` — smallest reranker in
        /// fastembed v5 (~137M params), English-only. Multilingual users
        /// who need >100 languages should opt into
        /// `JinaRerankerV2BaseMultiligual` via `with_model`.
        ///
        /// Model files cache under:
        ///   macOS:   ~/Library/Caches/thinkingroot/models/
        ///   Linux:   ~/.cache/thinkingroot/models/
        ///   Windows: %LOCALAPPDATA%\thinkingroot\models\
        ///
        /// Falls back to `<workspace>/.thinkingroot/models/` when the OS
        /// cache directory cannot be resolved.
        pub fn new(workspace_path: &Path) -> Result<Self> {
            let cache_dir = dirs::cache_dir()
                .map(|d| d.join("thinkingroot").join("models"))
                .unwrap_or_else(|| workspace_path.join("models"));
            std::fs::create_dir_all(&cache_dir).map_err(|e| Error::io_path(&cache_dir, e))?;

            Ok(Self {
                model: Mutex::new(None),
                model_kind: RerankerModel::JINARerankerV1TurboEn,
                cache_dir,
            })
        }

        /// Override the model kind. Must be called before the first `rerank`.
        pub fn with_model(mut self, kind: RerankerModel) -> Self {
            self.model_kind = kind;
            self
        }

        /// Score `(query, document)` pairs and return blended scores in
        /// the order of `documents`. Higher is more relevant.
        ///
        /// **Latency** (verified May 2026, top-20 on M-series CPU):
        ///   - `JinaRerankerV1TurboEn`  : ~120-200 ms
        ///   - `JinaRerankerV2Base*`    : ~200-300 ms
        ///   - `BGERerankerBase`        : ~250-400 ms
        ///   - `BGERerankerV2M3`        : ~300-500 ms
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
                let model = TextRerank::try_new(
                    RerankInitOptions::new(self.model_kind.clone())
                        .with_cache_dir(self.cache_dir.clone())
                        .with_show_download_progress(false),
                )
                .map_err(|e| {
                    Error::GraphStorage(format!("failed to init cross-encoder: {e}"))
                })?;
                *guard = Some(model);
                tracing::info!(target: "rerank", "cross-encoder loaded");
            }

            let model = guard.as_mut().expect("just-loaded");
            let results = model
                .rerank(query, documents, true, None)
                .map_err(|e| Error::GraphStorage(format!("rerank failed: {e}")))?;

            // `fastembed::RerankResult` returns `{ index, score, document }`
            // with `index` referring to the position in the input vec. We
            // need to re-project scores into input order so the caller can
            // zip them with their existing hits.
            let mut scores = vec![0.0_f32; documents.len()];
            for r in results {
                if r.index < documents.len() {
                    scores[r.index] = r.score;
                }
            }
            Ok(scores)
        }

        /// Whether the model has been loaded yet. Useful for telemetry and
        /// for tests that want to verify lazy-load behaviour without
        /// triggering a download.
        pub fn is_loaded(&self) -> bool {
            self.model
                .lock()
                .map(|g| g.is_some())
                .unwrap_or(false)
        }
    }

    // Re-export so callers don't have to import `fastembed::RerankerModel`
    // directly. The `_` import keeps clippy quiet about the unused variant.
    pub use fastembed::RerankerModel as ModelKind;
}

#[cfg(not(feature = "vector"))]
mod inner {
    use std::path::Path;
    use thinkingroot_core::Result;

    /// No-op reranker compiled when the `vector` feature is disabled.
    /// Matches the public surface of the real reranker so downstream
    /// callers (e.g. `intelligence/hybrid.rs`) need no `cfg!` guards.
    pub struct CrossEncoder;

    impl CrossEncoder {
        pub fn new(_workspace_path: &Path) -> Result<Self> {
            Ok(Self)
        }

        pub fn with_model(self, _kind: ModelKind) -> Self {
            self
        }

        /// Returns an empty score vec — callers detect the no-op via
        /// `scores.is_empty()` and fall back to their pre-rerank ordering.
        pub fn rerank(&self, _query: &str, documents: &[&str]) -> Result<Vec<f32>> {
            // Honest no-op: tell the caller we did nothing by returning
            // a vec of zeros the same length as `documents`. The hybrid
            // path treats all-zero rerank scores as "skip blending".
            Ok(vec![0.0; documents.len()])
        }

        pub fn is_loaded(&self) -> bool {
            false
        }
    }

    /// Stand-in for `fastembed::RerankerModel` so the public API stays
    /// identical between feature-gated and stub builds.
    #[derive(Clone, Copy)]
    pub enum ModelKind {
        JinaRerankerV1TurboEn,
        JinaRerankerV2BaseMultiligual,
        BgeRerankerBase,
        BgeRerankerV2M3,
    }
}

pub use inner::{CrossEncoder, ModelKind};

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_encoder_construct_without_model_load() {
        // The constructor must NOT download the model — that happens on
        // the first `rerank` call. This test guarantees workspace open
        // stays fast.
        let dir = tempfile::tempdir().unwrap();
        let ce = CrossEncoder::new(dir.path()).unwrap();
        assert!(!ce.is_loaded(), "model must lazy-load on first rerank call");
    }

    #[test]
    fn empty_documents_returns_empty_scores() {
        // Empty inputs are a hot path during system warm-up. Don't pay the
        // model-load cost when there's nothing to score.
        let dir = tempfile::tempdir().unwrap();
        let ce = CrossEncoder::new(dir.path()).unwrap();
        let scores = ce.rerank("anything", &[]).unwrap();
        assert!(scores.is_empty());
        assert!(!ce.is_loaded(), "empty input must not trigger model load");
    }

    #[cfg(not(feature = "vector"))]
    #[test]
    fn noop_stub_returns_zero_scores_matching_input_len() {
        // When the `vector` feature is disabled, the rerank entry point
        // is a no-op that hands back zero scores. Callers detect this
        // shape and fall back to their pre-rerank ordering.
        let dir = tempfile::tempdir().unwrap();
        let ce = CrossEncoder::new(dir.path()).unwrap();
        let docs = vec!["doc one", "doc two", "doc three"];
        let scores = ce.rerank("query", &docs).unwrap();
        assert_eq!(scores.len(), 3);
        assert!(scores.iter().all(|s| *s == 0.0));
    }

    #[cfg(feature = "vector")]
    #[test]
    #[ignore = "downloads ~280MB Jina reranker model on first run"]
    fn jina_turbo_reranks_relevant_higher_than_irrelevant() {
        let dir = tempfile::tempdir().unwrap();
        let ce = CrossEncoder::new(dir.path()).unwrap();
        let docs = vec![
            "The capital of France is Paris.",
            "I ate a sandwich for lunch.",
            "Paris is the largest city in France.",
        ];
        let scores = ce.rerank("What is the capital of France?", &docs).unwrap();
        assert_eq!(scores.len(), 3);
        // Doc 0 and Doc 2 are about Paris; Doc 1 is unrelated.
        assert!(scores[0] > scores[1], "Paris doc must outrank sandwich doc");
        assert!(scores[2] > scores[1], "Paris doc 2 must outrank sandwich doc");
    }
}
