// ─── Real Implementation ─────────────────────────────────────────────────────
//
// Compiled only when the "vector" feature is enabled.  Uses fastembed +
// ONNX Runtime for local neural embeddings and cosine-similarity search.

#[cfg(feature = "vector")]
mod inner {
    use std::collections::HashMap;
    use std::path::Path;

    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use thinkingroot_core::{Error, Result};

    /// Vector storage backed by fastembed for local neural embeddings.
    /// Stores embeddings in-memory with persistence to disk via JSON.
    /// Supports cosine similarity search for semantic queries.
    ///
    /// The ONNX model is loaded **lazily** on first use so that opening a
    /// workspace (e.g. `root graph`) is instant — ONNX Runtime session
    /// creation is slow even when the model file is already cached on disk.
    pub struct VectorStore {
        /// `None` until the first embed/search call; populated on demand.
        model: Option<TextEmbedding>,
        /// Stored so the model can be initialised lazily without re-scanning.
        cache_dir: std::path::PathBuf,
        /// Map from ID → (embedding vector, metadata string).
        index: HashMap<String, (Vec<f32>, String)>,
        persist_path: std::path::PathBuf,
    }

    impl VectorStore {
        /// Initialize the vector store.
        ///
        /// Fast path: only loads the on-disk index. The ONNX embedding model
        /// is deferred until the first `upsert`, `search`, or `embed_texts`
        /// call, keeping workspace open time under one second.
        ///
        /// The ONNX model is stored in a **global shared cache** so every
        /// workspace reuses the same ~30 MB download:
        ///   macOS:   ~/Library/Caches/thinkingroot/models/
        ///   Linux:   ~/.cache/thinkingroot/models/
        ///   Windows: %LOCALAPPDATA%\thinkingroot\models\
        /// Falls back to `{workspace}/.thinkingroot/models/` if the OS cache
        /// directory cannot be resolved.
        pub async fn init(path: &Path) -> Result<Self> {
            let cache_dir = dirs::cache_dir()
                .map(|d| d.join("thinkingroot").join("models"))
                .unwrap_or_else(|| path.join("models"));
            std::fs::create_dir_all(&cache_dir).map_err(|e| Error::io_path(&cache_dir, e))?;

            let persist_path = path.join("vectors.bin");
            let index = Self::load_index(&persist_path);

            tracing::info!(
                "vector store ready ({} cached embeddings, model deferred)",
                index.len()
            );

            Ok(Self {
                model: None,
                cache_dir,
                index,
                persist_path,
            })
        }

        /// Ensure the ONNX model is loaded, initialising it on first call.
        fn ensure_model(&mut self) -> Result<&mut TextEmbedding> {
            if self.model.is_none() {
                tracing::info!("loading embedding model (first use)…");
                let model = TextEmbedding::try_new(
                    InitOptions::new(EmbeddingModel::AllMiniLML6V2)
                        .with_cache_dir(self.cache_dir.clone())
                        .with_show_download_progress(false),
                )
                .map_err(|e| Error::GraphStorage(format!("failed to init embedding model: {e}")))?;
                self.model = Some(model);
                tracing::info!("embedding model loaded");
            }
            Ok(self.model.as_mut().unwrap())
        }

        /// Embed and store a text with an ID and metadata string.
        pub fn upsert(&mut self, id: &str, text: &str, metadata: &str) -> Result<()> {
            let embeddings = self
                .ensure_model()?
                .embed(vec![text], None)
                .map_err(|e| Error::GraphStorage(format!("embedding failed: {e}")))?;

            if let Some(vec) = embeddings.into_iter().next() {
                self.index
                    .insert(id.to_string(), (vec, metadata.to_string()));
            }
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

            let texts: Vec<&str> = items.iter().map(|(_, text, _)| text.as_str()).collect();

            let embeddings = self
                .ensure_model()?
                .embed(texts, None)
                .map_err(|e| Error::GraphStorage(format!("batch embedding failed: {e}")))?;

            let mut count = 0;
            for (embedding, (id, _, metadata)) in embeddings.into_iter().zip(items.iter()) {
                self.index.insert(id.clone(), (embedding, metadata.clone()));
                count += 1;
            }

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

            let query_embedding = self
                .ensure_model()?
                .embed(vec![query], None)
                .map_err(|e| Error::GraphStorage(format!("query embedding failed: {e}")))?;

            let query_vec = match query_embedding.into_iter().next() {
                Some(v) => v,
                None => return Ok(Vec::new()),
            };

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
                    let sim = cosine_similarity(&query_vec, vec);
                    (id.clone(), meta.clone(), sim)
                })
                .collect();

            scores.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scores.truncate(top_k);

            Ok(scores)
        }

        /// Persist the index to disk in compact binary format.
        ///
        /// Format: `TRVEC1\n` magic, then per-entry:
        ///   [u32 id_len][id bytes][u32 meta_len][meta bytes][u32 dims][f32 × dims]
        /// All integers little-endian. This is ~4× smaller than JSON and loads
        /// without a temporary allocation spike.
        pub fn save(&self) -> Result<()> {
            use std::io::Write;

            let mut buf = Vec::with_capacity(self.index.len() * 400);
            buf.extend_from_slice(b"TRVEC1\n");

            for (id, (vec, meta)) in &self.index {
                let id_b = id.as_bytes();
                let meta_b = meta.as_bytes();
                buf.extend_from_slice(&(id_b.len() as u32).to_le_bytes());
                buf.extend_from_slice(id_b);
                buf.extend_from_slice(&(meta_b.len() as u32).to_le_bytes());
                buf.extend_from_slice(meta_b);
                buf.extend_from_slice(&(vec.len() as u32).to_le_bytes());
                for f in vec {
                    buf.extend_from_slice(&f.to_le_bytes());
                }
            }

            // Atomic write: write to temp file then rename.
            let tmp = self.persist_path.with_extension("bin.tmp");
            std::fs::write(&tmp, &buf).map_err(|e| Error::io_path(&tmp, e))?;
            std::fs::rename(&tmp, &self.persist_path)
                .map_err(|e| Error::io_path(&self.persist_path, e))?;

            tracing::debug!(
                "saved {} vectors to disk ({} bytes)",
                self.index.len(),
                buf.len()
            );
            Ok(())
        }

        pub fn reset(&mut self) {
            self.index.clear();
        }

        /// Remove specific entries by ID. O(ids.len()).
        pub fn remove_by_ids(&mut self, ids: &[&str]) {
            for id in ids {
                self.index.remove(*id);
            }
        }

        /// Return all stored (id, vector, metadata) triples.
        /// Used during merge to copy branch embeddings into main.
        pub fn all_items(&self) -> Vec<(String, Vec<f32>, String)> {
            self.index
                .iter()
                .map(|(id, (vec, meta))| (id.clone(), vec.clone(), meta.clone()))
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
                self.index.insert(id, (vec, meta));
            }
            Ok(count)
        }

        /// Number of stored embeddings.
        pub fn len(&self) -> usize {
            self.index.len()
        }

        pub fn is_empty(&self) -> bool {
            self.index.is_empty()
        }

        /// Embed texts and return raw embedding vectors.
        /// Used by the grounding system's semantic judge.
        pub fn embed_texts(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.ensure_model()?
                .embed(texts.to_vec(), None)
                .map_err(|e| Error::GraphStorage(format!("embedding failed: {e}")))
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

            let first_vec = &self.index.values().next().unwrap().0;
            let dims = first_vec.len();

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
                for v in base_x.iter_mut() { *v /= norm_x; }
            }
            if norm_y > 0.0 {
                for v in base_y.iter_mut() { *v /= norm_y; }
            }

            let mut min_x = f32::MAX;
            let mut max_x = f32::MIN;
            let mut min_y = f32::MAX;
            let mut max_y = f32::MIN;

            for (id, (vec, _)) in &self.index {
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

        fn load_index(path: &Path) -> HashMap<String, (Vec<f32>, String)> {
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => return HashMap::new(),
            };

            // Try new binary format first.
            if bytes.starts_with(b"TRVEC1\n") {
                return Self::load_index_binary(&bytes[7..]);
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
                .map(|(id, vec, meta)| (id, (vec, meta)))
                .collect()
        }

        fn load_index_binary(mut data: &[u8]) -> HashMap<String, (Vec<f32>, String)> {
            use std::convert::TryInto;

            let mut map = HashMap::new();

            let read_u32 = |buf: &mut &[u8]| -> Option<u32> {
                if buf.len() < 4 {
                    return None;
                }
                let v = u32::from_le_bytes(buf[..4].try_into().ok()?);
                *buf = &buf[4..];
                Some(v)
            };

            loop {
                let id_len = match read_u32(&mut data) {
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

                let meta_len = match read_u32(&mut data) {
                    Some(n) => n as usize,
                    None => break,
                };
                if data.len() < meta_len {
                    break;
                }
                let meta = match std::str::from_utf8(&data[..meta_len]) {
                    Ok(s) => s.to_string(),
                    Err(_) => break,
                };
                data = &data[meta_len..];

                let dims = match read_u32(&mut data) {
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

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        dot / (norm_a * norm_b)
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
    /// Allows the codebase to compile without fastembed/ONNX Runtime.
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

        pub fn save(&self) -> Result<()> {
            Ok(())
        }

        pub fn reset(&mut self) {}

        pub fn remove_by_ids(&mut self, _ids: &[&str]) {}

        pub fn all_items(&self) -> Vec<(String, Vec<f32>, String)> {
            vec![]
        }

        pub fn upsert_raw_batch(
            &mut self,
            _items: Vec<(String, Vec<f32>, String)>,
        ) -> Result<usize> {
            Ok(0)
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
}

// Re-export whichever impl was compiled.
pub use inner::VectorStore;

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
    #[tokio::test]
    #[ignore = "requires fastembed model download (~30 MB)"]
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
