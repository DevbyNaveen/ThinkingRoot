use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thinkingroot_core::{Error, Result};

/// Stores per-source extraction fingerprints for early cutoff.
/// If a source's extraction output fingerprint is unchanged from the previous run,
/// downstream processing (linking, compilation) can be skipped.
///
/// Stored as JSON: `{data_dir}/fingerprints.json` → HashMap<uri, fingerprint_hex>
pub struct FingerprintStore {
    path: PathBuf,
    fingerprints: HashMap<String, String>,
}

impl FingerprintStore {
    /// Load existing fingerprints from disk, or create empty store.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("fingerprints.json");
        let fingerprints = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, fingerprints }
    }

    /// Compute a fingerprint for extraction output.
    /// The fingerprint is blake3 of the JSON-serialized ExtractionResult.
    pub fn compute(extraction_json: &[u8]) -> String {
        blake3::hash(extraction_json).to_hex().to_string()
    }

    /// Check if a source's extraction fingerprint is unchanged.
    /// Returns true if the new fingerprint matches the stored one (early cutoff).
    pub fn is_unchanged(&self, uri: &str, new_fingerprint: &str) -> bool {
        self.fingerprints
            .get(uri)
            .is_some_and(|stored| stored == new_fingerprint)
    }

    /// Update the stored fingerprint for a source.
    pub fn update(&mut self, uri: &str, fingerprint: String) {
        self.fingerprints.insert(uri.to_string(), fingerprint);
    }

    /// Remove the fingerprint for a deleted source.
    pub fn remove(&mut self, uri: &str) {
        self.fingerprints.remove(uri);
    }

    /// Persist to disk.
    pub fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.fingerprints)
            .map_err(|e| Error::GraphStorage(format!("fingerprint serialize failed: {e}")))?;
        std::fs::write(&self.path, bytes).map_err(|e| Error::io_path(&self.path, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_load_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FingerprintStore::load(dir.path());

        assert!(!store.is_unchanged("file.md", "abc123"));

        store.update("file.md", "abc123".to_string());
        assert!(store.is_unchanged("file.md", "abc123"));
        assert!(!store.is_unchanged("file.md", "different"));

        store.save().unwrap();

        // Reload from disk.
        let reloaded = FingerprintStore::load(dir.path());
        assert!(reloaded.is_unchanged("file.md", "abc123"));
    }

    #[test]
    fn compute_is_deterministic() {
        let data = b"{\"claims\":[],\"entities\":[],\"relations\":[]}";
        let fp1 = FingerprintStore::compute(data);
        let fp2 = FingerprintStore::compute(data);
        assert_eq!(fp1, fp2);

        let fp3 = FingerprintStore::compute(b"different data");
        assert_ne!(fp1, fp3);
    }

    #[test]
    fn remove_deletes_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FingerprintStore::load(dir.path());

        store.update("file.md", "abc".to_string());
        assert!(store.is_unchanged("file.md", "abc"));

        store.remove("file.md");
        assert!(!store.is_unchanged("file.md", "abc"));
    }
}
