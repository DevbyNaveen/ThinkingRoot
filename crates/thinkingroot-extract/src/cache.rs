use std::path::{Path, PathBuf};

use crate::schema::ExtractionResult;
use thinkingroot_core::{Error, Result};

/// Version tag included in cache keys. Bump this constant when extraction
/// prompts change to automatically invalidate all stale cache entries.
const PROMPT_VERSION: &str = "v2";

/// Content-addressable cache for LLM extraction results.
///
/// Key: `blake3(chunk_content + ":" + PROMPT_VERSION)`.
/// Value: JSON-serialised `ExtractionResult` stored as `{dir}/{hash_hex}.json`.
///
/// The cache lives at `{data_dir}/cache/extraction/`.
pub struct ExtractionCache {
    dir: PathBuf,
}

impl ExtractionCache {
    /// Create (or reopen) the cache backed by `{data_dir}/cache/extraction/`.
    pub fn new(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("cache").join("extraction");
        std::fs::create_dir_all(&dir).map_err(|e| Error::io_path(&dir, e))?;
        Ok(Self { dir })
    }

    /// Compute the cache key for a chunk's content.
    pub fn cache_key(content: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(content.as_bytes());
        hasher.update(b":");
        hasher.update(PROMPT_VERSION.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Look up a cached extraction result. Returns `None` on cache miss.
    pub fn get(&self, content: &str) -> Option<ExtractionResult> {
        let key = Self::cache_key(content);
        let path = self.dir.join(format!("{key}.json"));
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Store an extraction result in the cache.
    pub fn put(&self, content: &str, result: &ExtractionResult) -> Result<()> {
        let key = Self::cache_key(content);
        let path = self.dir.join(format!("{key}.json"));
        let bytes = serde_json::to_vec(result)
            .map_err(|e| Error::GraphStorage(format!("cache serialize failed: {e}")))?;
        std::fs::write(&path, bytes).map_err(|e| Error::io_path(&path, e))?;
        Ok(())
    }

    /// Number of cached entries (for diagnostics / logging).
    pub fn len(&self) -> usize {
        std::fs::read_dir(&self.dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ExtractedClaim, ExtractedEntity, ExtractionResult};

    fn sample_result() -> ExtractionResult {
        ExtractionResult {
            claims: vec![ExtractedClaim {
                statement: "Rust is fast".into(),
                claim_type: "Fact".into(),
                confidence: 0.9,
                entities: vec!["Rust".into()],
                source_quote: None,
            }],
            entities: vec![ExtractedEntity {
                name: "Rust".into(),
                entity_type: "Concept".into(),
                aliases: vec![],
                description: Some("A language".into()),
            }],
            relations: vec![],
        }
    }

    #[test]
    fn cache_miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ExtractionCache::new(dir.path()).unwrap();
        assert!(cache.get("some content").is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_roundtrip_hit_after_put() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ExtractionCache::new(dir.path()).unwrap();

        let content = "fn main() { println!(\"hello\"); }";

        cache.put(content, &sample_result()).unwrap();

        let cached = cache.get(content).unwrap();
        assert_eq!(cached.claims.len(), 1);
        assert_eq!(cached.claims[0].statement, "Rust is fast");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn different_content_produces_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ExtractionCache::new(dir.path()).unwrap();

        cache.put("content A", &sample_result()).unwrap();

        assert!(cache.get("content B").is_none());
        assert!(cache.get("content A").is_some());
    }

    #[test]
    fn cache_key_is_deterministic() {
        assert_eq!(
            ExtractionCache::cache_key("hello"),
            ExtractionCache::cache_key("hello")
        );
        assert_ne!(
            ExtractionCache::cache_key("hello"),
            ExtractionCache::cache_key("world")
        );
    }
}
