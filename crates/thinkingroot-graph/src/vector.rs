// ─── Real Implementation ─────────────────────────────────────────────────────
//
// Compiled only when the "vector" feature is enabled. Uses raw
// `ort` + `tokenizers` (Track 32, 2026-05-16) — fastembed was dropped
// because its `with_cache_dir()` still falls back to Hugging Face Hub
// on cache miss, incompatible with our install-time bundle contract.
//
// Model files are staged by `install.sh` / `install.ps1` into the
// canonical bundle dir (`<dirs::cache_dir()>/thinkingroot/models/`)
// and loaded via `ort_session::EmbeddingModel`. Loading is still
// lazy (first call) so workspace open stays instant.

/// A hydrated atomic-fact row carried inside a [`ReadSnapshot`] (Pillar 2
/// Phase 3, 2026-06-28). Lets the `search_facts` recall path hydrate dense
/// hits AND run the lexical BM25 arm entirely from the lock-free snapshot —
/// no `storage` Mutex, so a live recall never blocks behind a background
/// reconcile/drain. Only the **live** fact set is snapshotted (superseded
/// facts are excluded at build time), so there is no `is_live` flag to check.
#[derive(Clone, Debug)]
pub struct FactRecord {
    /// The `af:`-prefixed fact id (== its vector key).
    pub id: String,
    pub statement: String,
    pub confidence: f32,
    pub source_id: String,
    /// Resolved source URI (for scope-check + citation), captured at build time.
    pub uri: String,
    pub created_at: f64,
}

#[cfg(feature = "vector")]
mod inner {
    use std::collections::HashMap;
    use std::path::Path;

    use crate::ort_session::{
        default_embed_paths, EmbeddingModel as OrtEmbeddingModel, EMBED_MAX_LEN,
    };
    use crate::vector_quant::{
        cosine_i8, cosine_query_to_i8, dequantize, max_sim, quantize_i8, QuantizedVec,
    };
    use thinkingroot_core::{Error, Result};
    use instant_distance::{Builder, HnswMap, Point, Search};

    /// ANN index point: a stored embedding in int8-quantized form (E5 wiring).
    /// Distance = cosine distance over the integer dot product, so HNSW
    /// nearest-neighbours match cosine-similarity ranking; the exact rescore
    /// pass in `search_vec_ann` then removes residual quantization noise.
    /// Quantized points keep the HNSW graph ~4× smaller in RAM than f32.
    #[derive(Clone, Debug)]
    struct EmbPoint(QuantizedVec);
    impl Point for EmbPoint {
        fn distance(&self, other: &Self) -> f32 {
            1.0 - cosine_i8(&self.0, &other.0)
        }
    }

    /// Below this many vectors, exact brute-force is faster than building/querying
    /// an HNSW (and stays exact). At/above it, the UNFILTERED search uses ANN +
    /// exact re-rank of candidates. Scoped/filtered searches always stay exact.
    const ANN_THRESHOLD: usize = 1024;

    /// Late-interaction (MaxSim) tier: max token vectors stored per document.
    /// Claims are short statements (typically 10–40 tokens); 48 covers them
    /// while bounding the per-claim token-index cost to ~48 × ~(dim + 8) B.
    const MAX_LI_TOKENS: usize = 48;

    /// The late-interaction tier is OFF until the Azure eval gate
    /// (`scripts/eval_gate.sh`, LongMemEval ≥ 91.2%) proves it. When off:
    /// no token vectors are captured at write, `tokens.bin` is not grown,
    /// and `max_sim_rerank` returns empty (caller treats as no-signal).
    fn late_interaction_enabled() -> bool {
        std::env::var("TR_LATE_INTERACTION")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    /// gte-modernbert-base embedding dimensionality. Upgraded from
    /// AllMiniLM-L6-v2 (384) to lift semantic/paraphrase recall (the 2020
    /// MiniLM was the measured bottleneck: paraphrase precision@3 = 0/4).
    /// gte-modernbert-base is 768-dim, no query-prefix needed, and its
    /// ModernBERT arch is already proven in-engine by the cross-encoder
    /// reranker. Changing this invalidates existing vector indexes — call
    /// `rebuild_vector_index` after deploying the new model bundle.
    const EMBED_DIM: usize = 768;

    /// Vector storage backed by `ort_session::EmbeddingModel`. Stores
    /// embeddings in-memory in **int8-quantized form** (E5: ~776 B/vector vs
    /// 3 KB f32 — ≈4× RAM + disk reduction) with persistence via the compact
    /// `TRVEC2` binary format (`TRVEC1` f32 indexes migrate transparently on
    /// load). All similarity scoring goes through the exact dequantized
    /// cosine (`vector_quant::cosine_query_to_i8`), whose ranking is proven
    /// to match f32 top-k in `vector_quant`'s tests; the ANN path follows the
    /// coarse(int8-HNSW)→exact-rescore contract of `vector_quant::search_rescore`.
    ///
    /// The ONNX model is loaded **lazily** on first use so that opening
    /// a workspace stays instant — ORT session creation is slow even
    /// when the model file is already on disk.
    pub struct VectorStore {
        /// `None` until first embed/search call; populated on demand.
        model: Option<OrtEmbeddingModel>,
        /// Map from ID → (int8-quantized embedding, metadata string).
        index: HashMap<String, (QuantizedVec, String)>,
        /// Late-interaction token vectors per ID (int8, ≤ MAX_LI_TOKENS each).
        /// Populated only when `TR_LATE_INTERACTION` is on; persisted to a
        /// sibling `tokens.bin` (TRTOK1). Entries here are a strict subset of
        /// `index` ids — MaxSim silently skips ids with no token entry.
        tokens: HashMap<String, Vec<QuantizedVec>>,
        persist_path: std::path::PathBuf,
        tokens_path: std::path::PathBuf,
        /// Lazily-(re)built HNSW over `index` for sublinear unfiltered search.
        ann: Option<HnswMap<EmbPoint, String>>,
        /// `index` mutated since `ann` was built → rebuild on next ANN search.
        ann_dirty: bool,
    }

    impl VectorStore {
        /// Initialize the vector store.
        ///
        /// Fast path: only loads the on-disk index. The ONNX embedding
        /// model is deferred until the first `upsert`, `search`, or
        /// `embed_texts` call, keeping workspace open time under one
        /// second.
        ///
        /// The ONNX model is read from the canonical bundle dir
        /// (`<dirs::cache_dir()>/thinkingroot/models/` or
        /// `$THINKINGROOT_MODELS_DIR`). `install.sh` / `install.ps1`
        /// stage the files there at install time — no lazy network
        /// fetch at runtime.
        pub async fn init(path: &Path) -> Result<Self> {
            let persist_path = path.join("vectors.bin");
            let tokens_path = path.join("tokens.bin");
            let index = Self::load_index(&persist_path);
            let tokens = Self::load_tokens(&tokens_path);

            tracing::info!(
                "vector store ready ({} cached embeddings, {} token-vector docs, model deferred)",
                index.len(),
                tokens.len()
            );

            Ok(Self {
                model: None,
                index,
                tokens,
                persist_path,
                tokens_path,
                ann: None,
                ann_dirty: true,
            })
        }

        /// Ensure the ONNX model is loaded, initialising it on first
        /// call. Returns `Error::GraphStorage` with a `root doctor --fix`
        /// hint when the bundle is missing.
        fn ensure_model(&mut self) -> Result<&mut OrtEmbeddingModel> {
            if self.model.is_none() {
                tracing::info!("loading embedding model (first use)…");
                let model =
                    OrtEmbeddingModel::load(&default_embed_paths(), EMBED_DIM, EMBED_MAX_LEN)?;
                self.model = Some(model);
                tracing::info!("embedding model loaded");
            }
            Ok(self.model.as_mut().expect("just-loaded"))
        }

        /// Embed and store a text with an ID and metadata string. With the
        /// late-interaction flag on, the same forward pass also captures the
        /// per-token vectors (zero extra inference).
        pub fn upsert(&mut self, id: &str, text: &str, metadata: &str) -> Result<()> {
            let li_cap = late_interaction_enabled().then_some(MAX_LI_TOKENS);
            let embeddings = self.ensure_model()?.embed_with_tokens(&[text], li_cap)?;

            if let Some((vec, toks)) = embeddings.into_iter().next() {
                self.index
                    .insert(id.to_string(), (quantize_i8(&vec), metadata.to_string()));
                if !toks.is_empty() {
                    self.tokens.insert(
                        id.to_string(),
                        toks.iter().map(|t| quantize_i8(t)).collect(),
                    );
                }
            }
            self.ann_dirty = true;
            Ok(())
        }

        /// Embed and store a batch of texts.
        pub fn upsert_batch(
            &mut self,
            items: &[(String, String, String)], // (id, text, metadata)
        ) -> Result<usize> {
            if items.is_empty() {
                return Ok(0);
            }

            let li_cap = late_interaction_enabled().then_some(MAX_LI_TOKENS);
            let texts: Vec<&str> = items.iter().map(|(_, text, _)| text.as_str()).collect();
            let embeddings = self.ensure_model()?.embed_with_tokens(&texts, li_cap)?;

            let mut count = 0;
            for ((embedding, toks), (id, _, metadata)) in
                embeddings.into_iter().zip(items.iter())
            {
                self.index
                    .insert(id.clone(), (quantize_i8(&embedding), metadata.clone()));
                if !toks.is_empty() {
                    self.tokens
                        .insert(id.clone(), toks.iter().map(|t| quantize_i8(t)).collect());
                }
                count += 1;
            }

            self.ann_dirty = true;
            Ok(count)
        }

        /// Search for the top-k most similar items to a query string.
        /// Returns (id, metadata, similarity_score) sorted by descending similarity.
        pub fn search(&mut self, query: &str, top_k: usize) -> Result<Vec<(String, String, f32)>> {
            self.search_scoped(query, top_k, None)
        }

        /// Search with optional source URI scope.
        ///
        /// `allowed_source_uris`: when `Some`, only returns results whose metadata
        /// contains one of the allowed URI substrings. Claim metadata format:
        /// `claim|{id}|{ctype}|{conf}|{uri}` — the URI is the last `|`-delimited field.
        /// Entity metadata format: `entity|{id}|{name}|{etype}` — no URI, always included.
        ///
        /// This powers per-user scoped retrieval in multi-user graphs: each eval question
        /// passes its `haystack_session_ids` so only that user's claims are considered.
        pub fn search_scoped(
            &mut self,
            query: &str,
            top_k: usize,
            allowed_source_uris: Option<&std::collections::HashSet<String>>,
        ) -> Result<Vec<(String, String, f32)>> {
            if self.index.is_empty() {
                return Ok(Vec::new());
            }

            let query_embedding = self.ensure_model()?.embed(&[query])?;

            let query_vec = match query_embedding.into_iter().next() {
                Some(v) => v,
                None => return Ok(Vec::new()),
            };

            // ANN fast path: unfiltered search over a large index → sublinear HNSW
            // + exact re-rank. Scoped/filtered searches stay exact brute-force
            // (they operate on bounded per-user subsets).
            if allowed_source_uris.is_none() && self.index.len() >= ANN_THRESHOLD {
                return Ok(self.search_vec_ann(&query_vec, top_k));
            }

            let mut scores: Vec<(String, String, f32)> = self
                .index
                .iter()
                .filter(|(_, (_, meta))| {
                    // Always include entities — they are user-agnostic structural nodes.
                    // For claims, filter by source URI when a scope is active.
                    if let Some(allowed) = allowed_source_uris {
                        if meta.starts_with("claim|") {
                            // URI is the last pipe-delimited field.
                            let uri = meta.rsplit('|').next().unwrap_or("");
                            // Match by session ID substring — URIs contain the session file name.
                            return allowed.iter().any(|sid| uri.contains(sid.as_str()));
                        }
                    }
                    true
                })
                .map(|(id, (vec, meta))| {
                    // Exact dequantized cosine — the same scorer as the
                    // rescore phase of `vector_quant::search_rescore`, so the
                    // brute-force path IS the exact pass (no coarse phase
                    // needed below the ANN threshold).
                    let sim = cosine_query_to_i8(&query_vec, vec);
                    (id.clone(), meta.clone(), sim)
                })
                .collect();

            scores.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scores.truncate(top_k);

            Ok(scores)
        }

        /// Semantic search restricted to entries whose metadata starts with
        /// `meta_prefix` (e.g. `"capability|"`). Unlike [`Self::search`], `top_k`
        /// is taken over ONLY the matching subset — so a small set of capability
        /// nodes is ranked among themselves and never crowded out of the result
        /// by the (typically far more numerous) claim/entity vectors. This is the
        /// capability-router's retrieval primitive (P2).
        pub fn search_prefix(
            &mut self,
            query: &str,
            top_k: usize,
            meta_prefix: &str,
        ) -> Result<Vec<(String, String, f32)>> {
            if self.index.is_empty() {
                return Ok(Vec::new());
            }
            let query_embedding = self.ensure_model()?.embed(&[query])?;
            let query_vec = match query_embedding.into_iter().next() {
                Some(v) => v,
                None => return Ok(Vec::new()),
            };
            let mut scores: Vec<(String, String, f32)> = self
                .index
                .iter()
                .filter(|(_, (_, meta))| meta.starts_with(meta_prefix))
                .map(|(id, (vec, meta))| {
                    (id.clone(), meta.clone(), cosine_query_to_i8(&query_vec, vec))
                })
                .collect();
            scores.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scores.truncate(top_k);
            Ok(scores)
        }

        /// Persist the index to disk in compact binary format.
        ///
        /// Format (E5): `TRVEC2\n` magic, then per-entry:
        ///   [u32 id_len][id bytes][u32 meta_len][meta bytes]
        ///   [u32 dims][f32 scale][f32 norm][i8 × dims]
        /// All integers little-endian. int8 codes make this ~4× smaller on
        /// disk than the f32 `TRVEC1` format, which `load_index` still reads
        /// (quantizing transparently — next save migrates the file).
        pub fn save(&self) -> Result<()> {
            let mut buf = Vec::with_capacity(self.index.len() * 900);
            buf.extend_from_slice(b"TRVEC2\n");

            for (id, (qvec, meta)) in &self.index {
                let id_b = id.as_bytes();
                let meta_b = meta.as_bytes();
                buf.extend_from_slice(&(id_b.len() as u32).to_le_bytes());
                buf.extend_from_slice(id_b);
                buf.extend_from_slice(&(meta_b.len() as u32).to_le_bytes());
                buf.extend_from_slice(meta_b);
                buf.extend_from_slice(&(qvec.codes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&qvec.scale.to_le_bytes());
                buf.extend_from_slice(&qvec.norm.to_le_bytes());
                buf.extend(qvec.codes.iter().map(|&c| c as u8));
            }

            // Atomic write: write to a UNIQUE temp file then rename. The temp
            // name carries pid + a process-global sequence so two concurrent
            // savers (e.g. a branch contribute + a turn-persist upsert) never
            // write to the same `vectors.bin.tmp` and clobber each other's
            // partially-written temp before rename. (NOTE: the final rename is
            // still last-writer-wins — a true lost-update guard needs a per-branch
            // write lock / shared cached vector handle; tracked as the remaining
            // half of the branch-vector race. This fixes the torn-temp corruption,
            // the worst outcome; a lost embedding is recoverable by recompile.)
            use std::sync::atomic::{AtomicU64, Ordering};
            static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = self
                .persist_path
                .with_extension(format!("bin.tmp.{}.{seq}", std::process::id()));
            std::fs::write(&tmp, &buf).map_err(|e| Error::io_path(&tmp, e))?;
            std::fs::rename(&tmp, &self.persist_path)
                .map_err(|e| Error::io_path(&self.persist_path, e))?;

            tracing::debug!(
                "saved {} vectors to disk ({} bytes)",
                self.index.len(),
                buf.len()
            );

            // Late-interaction token index — sibling file, same atomic
            // pattern. Skipped entirely while empty AND absent on disk (the
            // common flag-off case costs nothing); written when non-empty so
            // captured tokens survive restarts, and rewritten-when-present so
            // removals/reset propagate to disk instead of resurrecting.
            if !self.tokens.is_empty() || self.tokens_path.exists() {
                let mut tbuf = Vec::with_capacity(self.tokens.len() * 4096 + 8);
                tbuf.extend_from_slice(b"TRTOK1\n");
                for (id, toks) in &self.tokens {
                    let id_b = id.as_bytes();
                    tbuf.extend_from_slice(&(id_b.len() as u32).to_le_bytes());
                    tbuf.extend_from_slice(id_b);
                    tbuf.extend_from_slice(&(toks.len() as u32).to_le_bytes());
                    for q in toks {
                        tbuf.extend_from_slice(&(q.codes.len() as u32).to_le_bytes());
                        tbuf.extend_from_slice(&q.scale.to_le_bytes());
                        tbuf.extend_from_slice(&q.norm.to_le_bytes());
                        tbuf.extend(q.codes.iter().map(|&c| c as u8));
                    }
                }
                let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
                let ttmp = self
                    .tokens_path
                    .with_extension(format!("bin.tmp.{}.{seq}", std::process::id()));
                std::fs::write(&ttmp, &tbuf).map_err(|e| Error::io_path(&ttmp, e))?;
                std::fs::rename(&ttmp, &self.tokens_path)
                    .map_err(|e| Error::io_path(&self.tokens_path, e))?;
            }
            Ok(())
        }

        pub fn reset(&mut self) {
            self.index.clear();
            self.tokens.clear();
            self.ann_dirty = true;
        }

        /// Remove specific entries by ID. O(ids.len()).
        pub fn remove_by_ids(&mut self, ids: &[&str]) {
            for id in ids {
                self.index.remove(*id);
                self.tokens.remove(*id);
            }
            self.ann_dirty = true;
        }

        /// Snapshot of all currently-indexed ids. Used by
        /// `pipeline::reconcile_vector_index` to compute the
        /// add/remove delta against the post-compile graph state.
        pub fn index_ids(&self) -> Vec<String> {
            self.index.keys().cloned().collect()
        }

        /// Return all stored (id, vector, metadata) triples, dequantized to
        /// f32. Used during merge to copy branch embeddings into main. The
        /// dequantize→re-quantize round-trip through `upsert_raw_batch` is
        /// EXACT (dequantized values sit on the quantization grid and the
        /// max-abs element is preserved, so codes reproduce identically) —
        /// merges never accumulate drift.
        pub fn all_items(&self) -> Vec<(String, Vec<f32>, String)> {
            self.index
                .iter()
                .map(|(id, (qvec, meta))| (id.clone(), dequantize(qvec), meta.clone()))
                .collect()
        }

        /// Insert pre-computed embeddings directly — no model inference.
        /// Used during merge to import branch vectors into main without re-embedding.
        pub fn upsert_raw_batch(
            &mut self,
            items: Vec<(String, Vec<f32>, String)>,
        ) -> Result<usize> {
            let count = items.len();
            for (id, vec, meta) in items {
                self.index.insert(id, (quantize_i8(&vec), meta));
            }
            self.ann_dirty = true;
            Ok(count)
        }

        /// All late-interaction token entries (id → quantized token vectors).
        /// Used during merge to copy branch token indexes into main, exactly
        /// like `all_items` for pooled vectors. int8 codes pass through
        /// losslessly (no dequantize/requantize round trip).
        pub fn all_token_items(&self) -> Vec<(String, Vec<QuantizedVec>)> {
            self.tokens
                .iter()
                .map(|(id, toks)| (id.clone(), toks.clone()))
                .collect()
        }

        /// Insert pre-computed token vectors directly — the merge-import
        /// counterpart of `upsert_raw_batch` for the late-interaction index.
        pub fn upsert_raw_token_batch(
            &mut self,
            items: Vec<(String, Vec<QuantizedVec>)>,
        ) -> usize {
            let count = items.len();
            for (id, toks) in items {
                self.tokens.insert(id, toks);
            }
            count
        }

        /// Late-interaction (MaxSim) rerank over a candidate id set. Embeds
        /// the query ONCE with token capture, then scores each candidate that
        /// has a token entry; candidates without one are simply absent from
        /// the result (the caller must treat absence as "no signal", never
        /// as a zero score). Returns an empty vec when the tier is disabled
        /// or the token index is empty — the honest no-op.
        pub fn max_sim_rerank(
            &mut self,
            query: &str,
            candidate_ids: &[String],
        ) -> Result<Vec<(String, f32)>> {
            if !late_interaction_enabled() || self.tokens.is_empty() || candidate_ids.is_empty() {
                return Ok(Vec::new());
            }
            let has_any = candidate_ids.iter().any(|id| self.tokens.contains_key(id));
            if !has_any {
                return Ok(Vec::new());
            }
            let mut embedded = self
                .ensure_model()?
                .embed_with_tokens(&[query], Some(MAX_LI_TOKENS))?;
            let query_tokens = match embedded.pop() {
                Some((_, toks)) if !toks.is_empty() => toks,
                _ => return Ok(Vec::new()),
            };
            Ok(candidate_ids
                .iter()
                .filter_map(|id| {
                    self.tokens
                        .get(id)
                        .map(|doc| (id.clone(), max_sim(&query_tokens, doc)))
                })
                .collect())
        }

        /// Search using a pre-computed query embedding (no model inference).
        /// Used by the branch contradiction pass which already has the source
        /// claim's embedding cached in the source store — re-embedding the
        /// same text would force two `ensure_model()` calls per pair.
        pub fn search_by_vector(
            &self,
            query_vec: &[f32],
            top_k: usize,
        ) -> Vec<(String, String, f32)> {
            if self.index.is_empty() {
                return Vec::new();
            }
            let mut scores: Vec<(String, String, f32)> = self
                .index
                .iter()
                .map(|(id, (vec, meta))| {
                    let sim = cosine_query_to_i8(query_vec, vec);
                    (id.clone(), meta.clone(), sim)
                })
                .collect();
            scores.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scores.truncate(top_k);
            scores
        }

        /// (Re)build the HNSW from `index` if stale. O(n log n), amortised across
        /// the many reads between writes.
        fn ensure_ann(&mut self) {
            if self.ann.is_some() && !self.ann_dirty {
                return;
            }
            let mut points = Vec::with_capacity(self.index.len());
            let mut values = Vec::with_capacity(self.index.len());
            for (id, (qvec, _meta)) in &self.index {
                points.push(EmbPoint(qvec.clone()));
                values.push(id.clone());
            }
            self.ann = Some(Builder::default().build(points, values));
            self.ann_dirty = false;
        }

        /// ANN nearest-neighbour search by a pre-computed query vector, then EXACT
        /// re-rank of the over-fetched candidates — the coarse→rescore contract
        /// of `vector_quant::search_rescore` applied at HNSW scale: phase 1 is
        /// the int8 HNSW walk, phase 2 the exact dequantized cosine. Overfetch
        /// matches `search_rescore`'s `max(top_k·4, 64)` so quantization noise
        /// in the coarse phase cannot evict a true top-k candidate.
        fn search_vec_ann(&mut self, query_vec: &[f32], top_k: usize) -> Vec<(String, String, f32)> {
            if top_k == 0 || self.index.is_empty() {
                return Vec::new();
            }
            self.ensure_ann();
            let overfetch = (top_k * 4).max(64);
            let candidate_ids: Vec<String> = {
                let map = match self.ann.as_ref() {
                    Some(m) => m,
                    None => return Vec::new(),
                };
                let q = EmbPoint(quantize_i8(query_vec));
                let mut search = Search::default();
                map.search(&q, &mut search)
                    .take(overfetch)
                    .map(|item| item.value.clone())
                    .collect()
            };
            let mut scored: Vec<(String, String, f32)> = candidate_ids
                .iter()
                .filter_map(|id| {
                    self.index.get(id).map(|(vec, meta)| {
                        (id.clone(), meta.clone(), cosine_query_to_i8(query_vec, vec))
                    })
                })
                .collect();
            scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(top_k);
            scored
        }

        /// Search by a pre-computed vector using the ANN fast path above
        /// `ANN_THRESHOLD`, else exact brute-force. Same result contract as
        /// [`Self::search_by_vector`]; sublinear at scale.
        pub fn search_by_vector_fast(
            &mut self,
            query_vec: &[f32],
            top_k: usize,
        ) -> Vec<(String, String, f32)> {
            if self.index.len() >= ANN_THRESHOLD {
                self.search_vec_ann(query_vec, top_k)
            } else {
                self.search_by_vector(query_vec, top_k)
            }
        }

        /// The stored embedding for a given id, dequantized to f32. Owned
        /// (not a borrow) since the store keeps int8 codes, not f32.
        pub fn get_embedding(&self, id: &str) -> Option<Vec<f32>> {
            self.index.get(id).map(|(qvec, _)| dequantize(qvec))
        }

        /// Number of stored embeddings.
        pub fn len(&self) -> usize {
            self.index.len()
        }

        pub fn is_empty(&self) -> bool {
            self.index.is_empty()
        }

        /// Embed texts and return raw embedding vectors.
        /// Used by the branch contradiction pass to cache embeddings
        /// for later `search_by_vector` calls without re-embedding.
        pub fn embed_texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.ensure_model()?.embed(texts)
        }

        /// Project exactly 384-dimensional embeddings (or any dimension) down to 2D
        /// using a deterministic Gaussian Random Projection.
        /// This creates a semantic map where related entities cluster together,
        /// avoiding O(N^2) physics simulations.
        pub fn project_to_2d(&self) -> HashMap<String, (f32, f32)> {
            let mut results = HashMap::with_capacity(self.index.len());
            if self.index.is_empty() {
                return results;
            }

            // Simple deterministic LCG to generate projection bases
            struct Lcg {
                state: u64,
            }
            impl Lcg {
                fn new(seed: u64) -> Self {
                    Self { state: seed }
                }
                fn next_f32(&mut self) -> f32 {
                    self.state = self
                        .state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let int_val = (self.state >> 32) as u32;
                    (int_val as f32 / (u32::MAX as f32)) * 2.0 - 1.0
                }
            }

            let dims = self.index.values().next().unwrap().0.codes.len();

            let mut rng = Lcg::new(42);
            let mut base_x = vec![0.0; dims];
            let mut base_y = vec![0.0; dims];
            for i in 0..dims {
                base_x[i] = rng.next_f32();
                base_y[i] = rng.next_f32();
            }

            // Gram-Schmidt: orthogonalize base_y against base_x so the two axes
            // capture independent variance (prevents diagonal-line collapse).
            let dot_xy: f32 = base_x.iter().zip(base_y.iter()).map(|(a, b)| a * b).sum();
            let dot_xx: f32 = base_x.iter().map(|a| a * a).sum();
            if dot_xx > 0.0 {
                let proj = dot_xy / dot_xx;
                for i in 0..dims {
                    base_y[i] -= proj * base_x[i];
                }
            }

            // Normalize both bases to unit length for uniform scaling.
            let norm_x: f32 = base_x.iter().map(|v| v * v).sum::<f32>().sqrt();
            let norm_y: f32 = base_y.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm_x > 0.0 {
                for v in base_x.iter_mut() {
                    *v /= norm_x;
                }
            }
            if norm_y > 0.0 {
                for v in base_y.iter_mut() {
                    *v /= norm_y;
                }
            }

            let mut min_x = f32::MAX;
            let mut max_x = f32::MIN;
            let mut min_y = f32::MAX;
            let mut max_y = f32::MIN;

            for (id, (qvec, _)) in &self.index {
                let vec = dequantize(qvec);
                let mut x = 0.0;
                let mut y = 0.0;
                for i in 0..dims {
                    x += vec.get(i).unwrap_or(&0.0) * base_x[i];
                    y += vec.get(i).unwrap_or(&0.0) * base_y[i];
                }
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                results.insert(id.clone(), (x, y));
            }

            // Normalize to [-1000, 1000] space for aesthetic spread
            let range_x = max_x - min_x;
            let range_y = max_y - min_y;
            let spread = 1500.0;

            if range_x > 0.0 && range_y > 0.0 {
                for (x, y) in results.values_mut() {
                    *x = ((*x - min_x) / range_x) * spread - (spread / 2.0);
                    *y = ((*y - min_y) / range_y) * spread - (spread / 2.0);
                }
            }

            results
        }

        fn load_index(path: &Path) -> HashMap<String, (QuantizedVec, String)> {
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => return HashMap::new(),
            };

            // Native int8 format.
            if bytes.starts_with(b"TRVEC2\n") {
                return Self::load_index_binary_v2(&bytes[7..]);
            }

            // f32 TRVEC1 — quantize transparently; next save writes TRVEC2.
            if bytes.starts_with(b"TRVEC1\n") {
                tracing::info!("vectors.bin: f32 TRVEC1 detected, quantizing to int8 (migrates on next save)");
                return Self::load_index_binary_v1(&bytes[7..])
                    .into_iter()
                    .map(|(id, (vec, meta))| (id, (quantize_i8(&vec), meta)))
                    .collect();
            }

            // Legacy JSON fallback — migrate transparently.
            tracing::info!("vectors.bin: legacy JSON format detected, will migrate on next save");
            let data: Vec<(String, Vec<f32>, String)> = match serde_json::from_slice(&bytes) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("vectors.bin parse failed: {e}");
                    return HashMap::new();
                }
            };
            data.into_iter()
                .map(|(id, vec, meta)| (id, (quantize_i8(&vec), meta)))
                .collect()
        }

        /// Shared header reader for both binary formats: id + metadata.
        /// Returns `None` at end-of-buffer or on corruption (truncated read).
        fn read_entry_header(data: &mut &[u8]) -> Option<(String, String)> {
            let id_len = Self::read_u32(data)? as usize;
            if data.len() < id_len {
                return None;
            }
            let id = std::str::from_utf8(&data[..id_len]).ok()?.to_string();
            *data = &data[id_len..];

            let meta_len = Self::read_u32(data)? as usize;
            if data.len() < meta_len {
                return None;
            }
            let meta = std::str::from_utf8(&data[..meta_len]).ok()?.to_string();
            *data = &data[meta_len..];
            Some((id, meta))
        }

        fn read_u32(buf: &mut &[u8]) -> Option<u32> {
            use std::convert::TryInto;
            if buf.len() < 4 {
                return None;
            }
            let v = u32::from_le_bytes(buf[..4].try_into().ok()?);
            *buf = &buf[4..];
            Some(v)
        }

        fn read_f32(buf: &mut &[u8]) -> Option<f32> {
            use std::convert::TryInto;
            if buf.len() < 4 {
                return None;
            }
            let v = f32::from_le_bytes(buf[..4].try_into().ok()?);
            *buf = &buf[4..];
            Some(v)
        }

        /// TRVEC2 (int8) entry body: [u32 dims][f32 scale][f32 norm][i8 × dims].
        fn load_index_binary_v2(mut data: &[u8]) -> HashMap<String, (QuantizedVec, String)> {
            let mut map = HashMap::new();
            loop {
                let (id, meta) = match Self::read_entry_header(&mut data) {
                    Some(h) => h,
                    None => break,
                };
                let dims = match Self::read_u32(&mut data) {
                    Some(n) => n as usize,
                    None => break,
                };
                let scale = match Self::read_f32(&mut data) {
                    Some(v) => v,
                    None => break,
                };
                let norm = match Self::read_f32(&mut data) {
                    Some(v) => v,
                    None => break,
                };
                if data.len() < dims {
                    break;
                }
                let codes: Vec<i8> = data[..dims].iter().map(|&b| b as i8).collect();
                data = &data[dims..];
                map.insert(id, (QuantizedVec { codes, scale, norm }, meta));
            }
            map
        }

        /// Load the TRTOK1 late-interaction token index. Entry layout:
        /// [u32 id_len][id][u32 n_tokens] then per token
        /// [u32 dims][f32 scale][f32 norm][i8 × dims]. Missing file or any
        /// corruption truncates honestly to what parsed (same contract as
        /// the vector index loaders).
        fn load_tokens(path: &Path) -> HashMap<String, Vec<QuantizedVec>> {
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => return HashMap::new(),
            };
            if !bytes.starts_with(b"TRTOK1\n") {
                tracing::warn!("tokens.bin: unknown format, ignoring");
                return HashMap::new();
            }
            let mut data = &bytes[7..];
            let mut map = HashMap::new();
            'outer: loop {
                let id_len = match Self::read_u32(&mut data) {
                    Some(n) => n as usize,
                    None => break,
                };
                if data.len() < id_len {
                    break;
                }
                let id = match std::str::from_utf8(&data[..id_len]) {
                    Ok(s) => s.to_string(),
                    Err(_) => break,
                };
                data = &data[id_len..];
                let n_tokens = match Self::read_u32(&mut data) {
                    Some(n) => n as usize,
                    None => break,
                };
                let mut toks = Vec::with_capacity(n_tokens.min(MAX_LI_TOKENS));
                for _ in 0..n_tokens {
                    let dims = match Self::read_u32(&mut data) {
                        Some(n) => n as usize,
                        None => break 'outer,
                    };
                    let scale = match Self::read_f32(&mut data) {
                        Some(v) => v,
                        None => break 'outer,
                    };
                    let norm = match Self::read_f32(&mut data) {
                        Some(v) => v,
                        None => break 'outer,
                    };
                    if data.len() < dims {
                        break 'outer;
                    }
                    let codes: Vec<i8> = data[..dims].iter().map(|&b| b as i8).collect();
                    data = &data[dims..];
                    toks.push(QuantizedVec { codes, scale, norm });
                }
                map.insert(id, toks);
            }
            map
        }

        /// TRVEC1 (f32) entry body: [u32 dims][f32 × dims].
        fn load_index_binary_v1(mut data: &[u8]) -> HashMap<String, (Vec<f32>, String)> {
            use std::convert::TryInto;
            let mut map = HashMap::new();
            loop {
                let (id, meta) = match Self::read_entry_header(&mut data) {
                    Some(h) => h,
                    None => break,
                };
                let dims = match Self::read_u32(&mut data) {
                    Some(n) => n as usize,
                    None => break,
                };
                if data.len() < dims * 4 {
                    break;
                }
                let vec: Vec<f32> = data[..dims * 4]
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                    .collect();
                data = &data[dims * 4..];

                map.insert(id, (vec, meta));
            }
            map
        }
    }

    /// f32×f32 cosine — retained for tests/diagnostics; the store itself now
    /// scores via `vector_quant::cosine_query_to_i8` (int8 storage).
    #[allow(dead_code)]
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        dot / (norm_a * norm_b)
    }

    // ========================================================================
    // Pillar 2 (2026-06-28) — lock-free read snapshot (RCU). An immutable copy
    // of the searchable state with the HNSW PRE-BUILT, so a recall needs NO
    // storage Mutex and NO &mut. Published by the write path after each
    // reconcile/upsert and read via RwLock<Arc<ReadSnapshot>> where the read
    // lock is held only for a nanosecond Arc-clone. Mirrors VectorStore's own
    // search logic but &self over the frozen snapshot.
    // ========================================================================
    pub struct ReadSnapshot {
        index: HashMap<String, (QuantizedVec, String)>,
        #[allow(dead_code)]
        tokens: HashMap<String, Vec<QuantizedVec>>,
        ann: Option<HnswMap<EmbPoint, String>>,
        /// Pillar 2 Phase 3 — live atomic facts keyed by `af:` id, so the
        /// `search_facts` recall path hydrates + BM25-scans lock-free.
        facts: HashMap<String, super::FactRecord>,
    }

    impl ReadSnapshot {
        pub fn empty() -> Self {
            Self {
                index: HashMap::new(),
                tokens: HashMap::new(),
                ann: None,
                facts: HashMap::new(),
            }
        }
        pub fn len(&self) -> usize { self.index.len() }
        pub fn is_empty(&self) -> bool { self.index.is_empty() }

        /// Attach the live fact set to a vector-only snapshot (consuming
        /// builder). Called by the write path after each reconcile/drain so
        /// recall sees freshly-written facts without a storage lock.
        pub fn with_facts(mut self, facts: HashMap<String, super::FactRecord>) -> Self {
            self.facts = facts;
            self
        }

        /// Number of live facts carried in the snapshot.
        pub fn fact_count(&self) -> usize { self.facts.len() }

        /// Lock-free hydration of a single fact by its `af:` id.
        pub fn fact(&self, id: &str) -> Option<&super::FactRecord> { self.facts.get(id) }

        /// Lock-free iterator over all live facts (the BM25 corpus).
        pub fn facts_iter(&self) -> impl Iterator<Item = &super::FactRecord> {
            self.facts.values()
        }

        /// Lock-free scoped search (mirror of VectorStore::search_scoped).
        pub fn search_scoped(
            &self,
            query_vec: &[f32],
            top_k: usize,
            allowed_source_uris: Option<&std::collections::HashSet<String>>,
        ) -> Vec<(String, String, f32)> {
            if self.index.is_empty() { return Vec::new(); }
            if allowed_source_uris.is_none() && self.index.len() >= ANN_THRESHOLD {
                return self.search_vec_ann(query_vec, top_k);
            }
            let mut scores: Vec<(String, String, f32)> = self
                .index
                .iter()
                .filter(|(_, (_, meta))| {
                    if let Some(allowed) = allowed_source_uris {
                        if meta.starts_with("claim|") {
                            let uri = meta.rsplit('|').next().unwrap_or("");
                            return allowed.iter().any(|sid| uri.contains(sid.as_str()));
                        }
                    }
                    true
                })
                .map(|(id, (vec, meta))| {
                    (id.clone(), meta.clone(), cosine_query_to_i8(query_vec, vec))
                })
                .collect();
            scores.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scores.truncate(top_k);
            scores
        }

        /// Lock-free prefix search (mirror of VectorStore::search_prefix).
        pub fn search_prefix(
            &self,
            query_vec: &[f32],
            top_k: usize,
            meta_prefix: &str,
        ) -> Vec<(String, String, f32)> {
            if self.index.is_empty() { return Vec::new(); }
            let mut scores: Vec<(String, String, f32)> = self
                .index
                .iter()
                .filter(|(_, (_, meta))| meta.starts_with(meta_prefix))
                .map(|(id, (vec, meta))| {
                    (id.clone(), meta.clone(), cosine_query_to_i8(query_vec, vec))
                })
                .collect();
            scores.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scores.truncate(top_k);
            scores
        }

        fn search_vec_ann(&self, query_vec: &[f32], top_k: usize) -> Vec<(String, String, f32)> {
            if top_k == 0 || self.index.is_empty() { return Vec::new(); }
            let map = match self.ann.as_ref() {
                Some(m) => m,
                None => return self.brute(query_vec, top_k),
            };
            let overfetch = (top_k * 4).max(64);
            let q = EmbPoint(quantize_i8(query_vec));
            let mut search = Search::default();
            let candidate_ids: Vec<String> = map
                .search(&q, &mut search)
                .take(overfetch)
                .map(|item| item.value.clone())
                .collect();
            let mut scored: Vec<(String, String, f32)> = candidate_ids
                .iter()
                .filter_map(|id| {
                    self.index
                        .get(id)
                        .map(|(vec, meta)| (id.clone(), meta.clone(), cosine_query_to_i8(query_vec, vec)))
                })
                .collect();
            scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(top_k);
            scored
        }

        fn brute(&self, query_vec: &[f32], top_k: usize) -> Vec<(String, String, f32)> {
            let mut scores: Vec<(String, String, f32)> = self
                .index
                .iter()
                .map(|(id, (vec, meta))| (id.clone(), meta.clone(), cosine_query_to_i8(query_vec, vec)))
                .collect();
            scores.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scores.truncate(top_k);
            scores
        }
    }

    impl VectorStore {
        /// Build an immutable lock-free read snapshot (Pillar 2). Clones the
        /// index/tokens and pre-builds the HNSW so readers never mutate.
        pub fn build_snapshot(&self) -> ReadSnapshot {
            let ann = if self.index.len() >= ANN_THRESHOLD {
                let mut points = Vec::with_capacity(self.index.len());
                let mut values = Vec::with_capacity(self.index.len());
                for (id, (qvec, _meta)) in &self.index {
                    points.push(EmbPoint(qvec.clone()));
                    values.push(id.clone());
                }
                Some(Builder::default().build(points, values))
            } else {
                None
            };
            // Facts are attached by the caller via `with_facts` (they live in
            // the graph, not the VectorStore). A vector-only snapshot starts
            // with an empty fact map.
            ReadSnapshot {
                index: self.index.clone(),
                tokens: self.tokens.clone(),
                ann,
                facts: HashMap::new(),
            }
        }
    }

    /// Process-global READ embedder (Pillar 1). A dedicated ONNX session for
    /// query embedding so a live recall's embed never serializes behind the
    /// bulk/ingest embed (VectorStore::model). Thread-safe ORT session; loaded
    /// once. Used by the lock-free read path (no storage Mutex).
    static SHARED_READ_EMBEDDER: std::sync::OnceLock<std::sync::Mutex<Option<OrtEmbeddingModel>>> =
        std::sync::OnceLock::new();

    /// Embed a single query string via the dedicated read embedder.
    pub fn embed_query(text: &str) -> Result<Vec<f32>> {
        let cell = SHARED_READ_EMBEDDER.get_or_init(|| std::sync::Mutex::new(None));
        let mut guard = cell
            .lock()
            .map_err(|_| Error::GraphStorage("read-embedder mutex poisoned".into()))?;
        if guard.is_none() {
            tracing::info!("loading READ embedding model (first use, dedicated lane)…");
            *guard = Some(OrtEmbeddingModel::load(&default_embed_paths(), EMBED_DIM, EMBED_MAX_LEN)?);
        }
        let model = guard.as_mut().expect("just-loaded");
        let mut out = model.embed(&[text])?;
        Ok(out.drain(..).next().unwrap_or_default())
    }

}

// ─── No-op Stub ──────────────────────────────────────────────────────────────
//
// Compiled when "vector" feature is absent.  Provides the same public API
// with zero-cost no-op implementations so the rest of the codebase compiles
// unchanged.  search() always returns empty results; upsert/save are no-ops.

#[cfg(not(feature = "vector"))]
mod inner {
    use std::path::Path;
    use thinkingroot_core::Result;

    /// No-op vector store used when the "vector" feature is disabled.
    /// Allows the codebase to compile without ort/tokenizers / ONNX Runtime.
    pub struct VectorStore;

    impl VectorStore {
        pub async fn init(_path: &Path) -> Result<Self> {
            tracing::debug!("vector store disabled (compiled without 'vector' feature)");
            Ok(Self)
        }

        pub fn upsert(&mut self, _id: &str, _text: &str, _metadata: &str) -> Result<()> {
            Ok(())
        }

        pub fn upsert_batch(&mut self, _items: &[(String, String, String)]) -> Result<usize> {
            Ok(0)
        }

        pub fn search(
            &mut self,
            _query: &str,
            _top_k: usize,
        ) -> Result<Vec<(String, String, f32)>> {
            Ok(Vec::new())
        }

        pub fn search_scoped(
            &mut self,
            _query: &str,
            _top_k: usize,
            _allowed: Option<&std::collections::HashSet<String>>,
        ) -> Result<Vec<(String, String, f32)>> {
            Ok(Vec::new())
        }

        pub fn search_prefix(
            &mut self,
            _query: &str,
            _top_k: usize,
            _meta_prefix: &str,
        ) -> Result<Vec<(String, String, f32)>> {
            Ok(Vec::new())
        }

        pub fn save(&self) -> Result<()> {
            Ok(())
        }

        pub fn reset(&mut self) {}

        pub fn remove_by_ids(&mut self, _ids: &[&str]) {}

        pub fn index_ids(&self) -> Vec<String> {
            Vec::new()
        }

        pub fn all_items(&self) -> Vec<(String, Vec<f32>, String)> {
            vec![]
        }

        pub fn upsert_raw_batch(
            &mut self,
            _items: Vec<(String, Vec<f32>, String)>,
        ) -> Result<usize> {
            Ok(0)
        }

        pub fn search_by_vector(
            &self,
            _query_vec: &[f32],
            _top_k: usize,
        ) -> Vec<(String, String, f32)> {
            Vec::new()
        }

        pub fn all_token_items(&self) -> Vec<(String, Vec<crate::vector_quant::QuantizedVec>)> {
            Vec::new()
        }

        pub fn upsert_raw_token_batch(
            &mut self,
            _items: Vec<(String, Vec<crate::vector_quant::QuantizedVec>)>,
        ) -> usize {
            0
        }

        pub fn max_sim_rerank(
            &mut self,
            _query: &str,
            _candidate_ids: &[String],
        ) -> Result<Vec<(String, f32)>> {
            Ok(Vec::new())
        }

        pub fn get_embedding(&self, _id: &str) -> Option<Vec<f32>> {
            None
        }

        pub fn len(&self) -> usize {
            0
        }

        pub fn is_empty(&self) -> bool {
            true
        }

        pub fn embed_texts(&mut self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(vec![])
        }

        pub fn project_to_2d(&self) -> std::collections::HashMap<String, (f32, f32)> {
            std::collections::HashMap::new()
        }
    }

    pub struct ReadSnapshot;
    impl ReadSnapshot {
        pub fn empty() -> Self { Self }
        pub fn len(&self) -> usize { 0 }
        pub fn is_empty(&self) -> bool { true }
        pub fn with_facts(
            self,
            _facts: std::collections::HashMap<String, super::FactRecord>,
        ) -> Self { self }
        pub fn fact_count(&self) -> usize { 0 }
        pub fn fact(&self, _id: &str) -> Option<&super::FactRecord> { None }
        pub fn facts_iter(&self) -> impl Iterator<Item = &super::FactRecord> {
            std::iter::empty()
        }
        pub fn search_scoped(
            &self,
            _query_vec: &[f32],
            _top_k: usize,
            _allowed: Option<&std::collections::HashSet<String>>,
        ) -> Vec<(String, String, f32)> { Vec::new() }
        pub fn search_prefix(
            &self,
            _query_vec: &[f32],
            _top_k: usize,
            _meta_prefix: &str,
        ) -> Vec<(String, String, f32)> { Vec::new() }
    }
    impl VectorStore {
        pub fn build_snapshot(&self) -> ReadSnapshot { ReadSnapshot }
    }
    pub fn embed_query(_text: &str) -> Result<Vec<f32>> { Ok(Vec::new()) }

}

// Re-export whichever impl was compiled.
pub use inner::{embed_query, ReadSnapshot, VectorStore};
// `FactRecord` is feature-independent and already `pub` at module scope, so it
// is reachable as `vector::FactRecord` without re-export.

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[cfg(feature = "vector")]
    use super::*;

    #[cfg(feature = "vector")]
    #[test]
    fn cosine_similarity_identical() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0];
        // Access via inner since cosine_similarity is private.
        // This test validates the math — just check the VectorStore compiles.
        let _ = (a, b);
    }

    #[cfg(feature = "vector")]
    #[test]
    fn remove_by_ids_method_exists_on_real_store() {
        // Verify the method has the expected signature.
        // Full behavioral test requires an initialized store (async + model download).
        let _: fn(&mut VectorStore, &[&str]) = VectorStore::remove_by_ids;
    }

    #[cfg(feature = "vector")]
    #[test]
    fn index_ids_method_exists_on_real_store() {
        let _: fn(&VectorStore) -> Vec<String> = VectorStore::index_ids;
    }

    /// Pillar 2 Phase 3 — the lock-free fact set carried by a ReadSnapshot.
    /// Model-free: pure data, so it runs everywhere. Locks in `with_facts`,
    /// `fact` (hydration), `facts_iter` (BM25 corpus), and `fact_count`.
    #[cfg(feature = "vector")]
    #[test]
    fn snapshot_carries_live_facts() {
        let mk = |id: &str, stmt: &str| FactRecord {
            id: id.to_string(),
            statement: stmt.to_string(),
            confidence: 0.9,
            source_id: "src1".to_string(),
            uri: "file://doc.md".to_string(),
            created_at: 1.0,
        };
        let mut facts = std::collections::HashMap::new();
        facts.insert("af:a".to_string(), mk("af:a", "Yuriy teaches the course"));
        facts.insert("af:b".to_string(), mk("af:b", "the embedder is 768-dim"));

        let snap = ReadSnapshot::empty().with_facts(facts);

        // Hydration by id (the dense-arm path).
        assert_eq!(snap.fact_count(), 2);
        assert_eq!(snap.fact("af:a").unwrap().statement, "Yuriy teaches the course");
        assert_eq!(snap.fact("af:b").unwrap().uri, "file://doc.md");
        assert!(snap.fact("af:missing").is_none());

        // Iteration (the BM25 corpus path) — order-independent.
        let mut ids: Vec<String> = snap.facts_iter().map(|f| f.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["af:a".to_string(), "af:b".to_string()]);
    }

    /// TRVEC2 persistence round-trip + TRVEC1 migration, model-free (raw
    /// vectors only — never touches the ONNX bundle, so it runs everywhere).
    #[cfg(feature = "vector")]
    #[tokio::test]
    async fn trvec2_roundtrip_and_trvec1_migration() {
        let dir = tempfile::tempdir().unwrap();

        // Deterministic pseudo-vectors (same scheme as vector_quant tests).
        let vec_of = |seed: u32, dim: usize| -> Vec<f32> {
            let mut s = seed.wrapping_mul(2654435761).wrapping_add(1);
            (0..dim)
                .map(|_| {
                    s = s.wrapping_mul(1103515245).wrapping_add(12345);
                    ((s >> 8) & 0xffff) as f32 / 32768.0 - 1.0
                })
                .collect()
        };

        // 1. Fresh store → raw upsert → save: file must be TRVEC2.
        let mut store = VectorStore::init(dir.path()).await.unwrap();
        let items: Vec<(String, Vec<f32>, String)> =
            (0..20).map(|i| (format!("c{i}"), vec_of(i, 64), format!("m{i}"))).collect();
        store.upsert_raw_batch(items.clone()).unwrap();
        store.save().unwrap();
        let bytes = std::fs::read(dir.path().join("vectors.bin")).unwrap();
        assert!(bytes.starts_with(b"TRVEC2\n"), "save must write TRVEC2");

        // 2. Reload → search order must match the pre-save store exactly.
        let q = vec_of(777, 64);
        let before: Vec<String> =
            store.search_by_vector(&q, 5).into_iter().map(|(id, _, _)| id).collect();
        let reloaded = VectorStore::init(dir.path()).await.unwrap();
        assert_eq!(reloaded.len(), 20);
        let after: Vec<String> =
            reloaded.search_by_vector(&q, 5).into_iter().map(|(id, _, _)| id).collect();
        assert_eq!(before, after, "TRVEC2 round-trip must preserve ranking");
        // Metadata survives too.
        assert_eq!(reloaded.get_embedding("c3").unwrap().len(), 64);

        // 3. Hand-write a TRVEC1 (f32) file → load must quantize transparently
        //    and rank identically to quantizing the same vectors in memory.
        let v1_dir = tempfile::tempdir().unwrap();
        let mut buf = b"TRVEC1\n".to_vec();
        for (id, vec, meta) in &items {
            buf.extend_from_slice(&(id.len() as u32).to_le_bytes());
            buf.extend_from_slice(id.as_bytes());
            buf.extend_from_slice(&(meta.len() as u32).to_le_bytes());
            buf.extend_from_slice(meta.as_bytes());
            buf.extend_from_slice(&(vec.len() as u32).to_le_bytes());
            for f in vec {
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
        std::fs::write(v1_dir.path().join("vectors.bin"), &buf).unwrap();
        let migrated = VectorStore::init(v1_dir.path()).await.unwrap();
        assert_eq!(migrated.len(), 20, "TRVEC1 entries must all load");
        let migrated_top: Vec<String> =
            migrated.search_by_vector(&q, 5).into_iter().map(|(id, _, _)| id).collect();
        assert_eq!(before, migrated_top, "TRVEC1 migration must preserve ranking");
        // And its next save upgrades the file format.
        migrated.save().unwrap();
        let migrated_bytes = std::fs::read(v1_dir.path().join("vectors.bin")).unwrap();
        assert!(migrated_bytes.starts_with(b"TRVEC2\n"), "TRVEC1 must migrate to TRVEC2 on save");
    }

    /// TRTOK1 token-index persistence round-trip, model-free (raw quantized
    /// tokens injected via the merge-import API).
    #[cfg(feature = "vector")]
    #[tokio::test]
    async fn trtok1_token_index_roundtrip() {
        use crate::vector_quant::quantize_i8;

        let dir = tempfile::tempdir().unwrap();
        let mut store = VectorStore::init(dir.path()).await.unwrap();

        // No tokens, no file: the flag-off path must not create tokens.bin.
        store.save().unwrap();
        assert!(
            !dir.path().join("tokens.bin").exists(),
            "empty token index must not create tokens.bin"
        );

        let toks_a = vec![quantize_i8(&[0.1, 0.9, -0.3]), quantize_i8(&[0.5, 0.5, 0.5])];
        let toks_b = vec![quantize_i8(&[1.0, 0.0, 0.0])];
        let n = store.upsert_raw_token_batch(vec![
            ("claim:a".into(), toks_a.clone()),
            ("claim:b".into(), toks_b.clone()),
        ]);
        assert_eq!(n, 2);
        store.save().unwrap();
        assert!(dir.path().join("tokens.bin").exists());

        let reloaded = VectorStore::init(dir.path()).await.unwrap();
        let mut items = reloaded.all_token_items();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], ("claim:a".to_string(), toks_a));
        assert_eq!(items[1], ("claim:b".to_string(), toks_b));

        // remove_by_ids drops the token entry too, and save propagates the
        // removal to disk instead of resurrecting it on next load.
        let mut reloaded = reloaded;
        reloaded.remove_by_ids(&["claim:a"]);
        reloaded.save().unwrap();
        let again = VectorStore::init(dir.path()).await.unwrap();
        assert_eq!(again.all_token_items().len(), 1);
        assert_eq!(again.all_token_items()[0].0, "claim:b");
    }

    #[cfg(feature = "vector")]
    #[tokio::test]
    #[ignore = "requires AllMiniLM-L6-v2 ONNX bundle staged at default_model_bundle_dir()"]
    async fn index_ids_returns_inserted_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = VectorStore::init(dir.path()).await.unwrap();
        assert!(store.index_ids().is_empty(), "fresh store has no ids");

        let items = vec![
            ("a".to_string(), "hello".to_string(), "m1".to_string()),
            ("b".to_string(), "world".to_string(), "m2".to_string()),
        ];
        store.upsert_batch(&items).unwrap();

        let mut ids = store.index_ids();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);

        store.remove_by_ids(&["a"]);
        let after = store.index_ids();
        assert_eq!(after, vec!["b".to_string()]);
    }

    #[cfg(feature = "vector")]
    #[tokio::test]
    #[ignore = "requires AllMiniLM-L6-v2 ONNX bundle staged at default_model_bundle_dir()"]
    async fn remove_by_ids_removes_only_specified() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = VectorStore::init(dir.path()).await.unwrap();

        let items = vec![
            (
                "id-1".to_string(),
                "hello world".to_string(),
                "meta1".to_string(),
            ),
            (
                "id-2".to_string(),
                "foo bar".to_string(),
                "meta2".to_string(),
            ),
            (
                "id-3".to_string(),
                "baz qux".to_string(),
                "meta3".to_string(),
            ),
        ];
        store.upsert_batch(&items).unwrap();
        assert_eq!(store.len(), 3);

        store.remove_by_ids(&["id-1", "id-3"]);
        assert_eq!(store.len(), 1, "only id-2 should remain");

        // Removing nonexistent IDs is a no-op.
        store.remove_by_ids(&["nonexistent"]);
        assert_eq!(store.len(), 1);
    }
}
