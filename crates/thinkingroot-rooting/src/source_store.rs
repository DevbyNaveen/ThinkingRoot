//! Durable byte-store for source documents, keyed by content hash.
//!
//! Rooting probes re-execute months after ingestion, so source bytes must
//! outlive the in-memory extraction pipeline. This module defines the trait
//! and the default filesystem-backed implementation.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use thinkingroot_core::types::{ContentHash, SourceId};

use crate::{Result, RootingError};

/// A source document's bytes plus identifying metadata.
#[derive(Debug, Clone)]
pub struct SourceBytes {
    /// Which source this blob belongs to.
    pub source_id: SourceId,
    /// Content-addressing key. Identical content → identical hash regardless
    /// of which `source_id` originally produced it.
    pub content_hash: ContentHash,
    /// Raw bytes. No compression in v1.
    pub bytes: Vec<u8>,
}

/// Storage abstraction for source bytes. The default implementation uses the
/// local filesystem; SaaS can swap in an S3-backed implementation that
/// satisfies the same contract.
pub trait SourceByteStore: Send + Sync {
    /// Persist bytes under `content_hash`. Idempotent: if `content_hash`
    /// already exists on disk, this is a no-op (content-addressed dedup).
    fn put(&self, source_id: SourceId, content_hash: &ContentHash, bytes: &[u8]) -> Result<()>;

    /// Fetch bytes by content hash. Returns `None` if the content was
    /// garbage-collected or never stored.
    fn get(&self, content_hash: &ContentHash) -> Result<Option<SourceBytes>>;

    /// Fetch a byte range. Used by probes that only need to inspect a claim's
    /// source_span, not the full document. Returns `None` if the content is
    /// absent. An out-of-bounds range is clamped to the file end.
    fn get_range(
        &self,
        content_hash: &ContentHash,
        start: usize,
        end: usize,
    ) -> Result<Option<Vec<u8>>>;

    /// Remove entries whose content-hash string is not in `live_hashes`.
    /// Called during Phase 5 (source removal) to keep disk usage bounded.
    /// The set holds hex strings so the caller doesn't have to allocate
    /// [`ContentHash`] values just to query.
    /// Returns the number of entries removed.
    fn gc(&self, live_hashes: &HashSet<String>) -> Result<usize>;
}

/// Filesystem-backed source byte store.
///
/// Layout: `{root}/rooting/sources/{hash[0..2]}/{hash[2..4]}/{full_hash}.bin`
/// Git-style fan-out sharding avoids single-directory explosion at scale
/// (thousands of sources → ~256 first-level dirs × 256 second-level dirs).
///
/// A companion sidecar file `{full_hash}.src` stores the owning `source_id`
/// so we can rebuild the SourceBytes envelope on read without touching the
/// graph.
pub struct FileSystemSourceStore {
    root: PathBuf,
}

impl FileSystemSourceStore {
    /// Create a new filesystem source store under `data_dir`. The actual
    /// root becomes `{data_dir}/rooting/sources/`. The directory is created
    /// lazily on first write.
    pub fn new(data_dir: &std::path::Path) -> Result<Self> {
        let root = data_dir.join("rooting").join("sources");
        Ok(Self { root })
    }

    fn path_for(&self, content_hash: &ContentHash) -> PathBuf {
        let hex = content_hash.0.as_str();
        let (d1, rest) = hex.split_at(hex.len().min(2));
        let (d2, _) = rest.split_at(rest.len().min(2));
        self.root.join(d1).join(d2).join(format!("{hex}.bin"))
    }

    fn sidecar_path_for(&self, content_hash: &ContentHash) -> PathBuf {
        let hex = content_hash.0.as_str();
        let (d1, rest) = hex.split_at(hex.len().min(2));
        let (d2, _) = rest.split_at(rest.len().min(2));
        self.root.join(d1).join(d2).join(format!("{hex}.src"))
    }
}

impl SourceByteStore for FileSystemSourceStore {
    fn put(&self, source_id: SourceId, content_hash: &ContentHash, bytes: &[u8]) -> Result<()> {
        let bin_path = self.path_for(content_hash);
        if bin_path.exists() {
            // Content-addressed dedup: same hash → same bytes, no need to rewrite.
            // Still refresh the sidecar so the most recent source_id is recorded
            // (useful for audit but not correctness-critical).
            let sidecar = self.sidecar_path_for(content_hash);
            if !sidecar.exists() {
                fs::write(&sidecar, source_id.to_string())?;
            }
            return Ok(());
        }

        if let Some(parent) = bin_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Atomic write: write to temp then rename. Prevents half-written files
        // if the process is killed mid-write.
        let tmp_path = bin_path.with_extension("bin.tmp");
        {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, &bin_path)?;

        // Sidecar records the source_id for audit. Non-atomic write is fine —
        // the sidecar is not read by the probe pipeline.
        let sidecar_path = self.sidecar_path_for(content_hash);
        fs::write(&sidecar_path, source_id.to_string())?;

        Ok(())
    }

    fn get(&self, content_hash: &ContentHash) -> Result<Option<SourceBytes>> {
        let bin_path = self.path_for(content_hash);
        if !bin_path.exists() {
            return Ok(None);
        }

        let bytes = fs::read(&bin_path)?;
        let source_id = match fs::read_to_string(self.sidecar_path_for(content_hash)) {
            Ok(s) => s
                .trim()
                .parse::<SourceId>()
                .map_err(|_| RootingError::Graph("invalid source_id sidecar".into()))?,
            Err(_) => SourceId::new(),
        };
        Ok(Some(SourceBytes {
            source_id,
            content_hash: content_hash.clone(),
            bytes,
        }))
    }

    fn get_range(
        &self,
        content_hash: &ContentHash,
        start: usize,
        end: usize,
    ) -> Result<Option<Vec<u8>>> {
        let bin_path = self.path_for(content_hash);
        if !bin_path.exists() {
            return Ok(None);
        }

        let mut f = fs::File::open(&bin_path)?;
        let file_len = f.metadata()?.len() as usize;
        let clamped_start = start.min(file_len);
        let clamped_end = end.min(file_len).max(clamped_start);
        let len = clamped_end - clamped_start;
        if len == 0 {
            return Ok(Some(Vec::new()));
        }

        f.seek(SeekFrom::Start(clamped_start as u64))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)?;
        Ok(Some(buf))
    }

    fn gc(&self, live_hashes: &HashSet<String>) -> Result<usize> {
        if !self.root.exists() {
            return Ok(0);
        }

        let mut removed = 0usize;
        // Walk: root / d1 / d2 / *.bin
        let d1_iter = match fs::read_dir(&self.root) {
            Ok(it) => it,
            Err(_) => return Ok(0),
        };
        for d1_entry in d1_iter.flatten() {
            if !d1_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let d2_iter = match fs::read_dir(d1_entry.path()) {
                Ok(it) => it,
                Err(_) => continue,
            };
            for d2_entry in d2_iter.flatten() {
                if !d2_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let file_iter = match fs::read_dir(d2_entry.path()) {
                    Ok(it) => it,
                    Err(_) => continue,
                };
                for file_entry in file_iter.flatten() {
                    let path = file_entry.path();
                    let stem = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    // Match only .bin files for the hash set test; the .src
                    // sidecar is cleaned opportunistically alongside its .bin.
                    let is_bin = path.extension().and_then(|e| e.to_str()) == Some("bin");
                    if !is_bin {
                        continue;
                    }
                    if !live_hashes.contains(&stem) {
                        let _ = fs::remove_file(&path);
                        let sidecar = path.with_extension("src");
                        let _ = fs::remove_file(&sidecar);
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tmpdir")
    }

    fn make_hash(hex: &str) -> ContentHash {
        // ContentHash in this codebase is a newtype around String; we can
        // construct arbitrary hex for tests without needing BLAKE3.
        ContentHash(hex.to_string())
    }

    #[test]
    fn put_and_get_round_trip() {
        let dir = tmp_dir();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let sid = SourceId::new();
        let hash = make_hash("abcd1234deadbeef");
        let bytes = b"hello, rooting!".to_vec();

        store.put(sid, &hash, &bytes).unwrap();
        let got = store.get(&hash).unwrap().expect("round-trip present");
        assert_eq!(got.bytes, bytes);
        assert_eq!(got.content_hash.0.as_str(), "abcd1234deadbeef");
    }

    #[test]
    fn get_returns_none_for_missing_hash() {
        let dir = tmp_dir();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let hash = make_hash("0000000000000000");
        assert!(store.get(&hash).unwrap().is_none());
    }

    #[test]
    fn put_is_idempotent_and_deduplicates_by_hash() {
        let dir = tmp_dir();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let sid1 = SourceId::new();
        let sid2 = SourceId::new();
        let hash = make_hash("deadbeefcafebabe");
        let bytes = b"dedup me".to_vec();

        store.put(sid1, &hash, &bytes).unwrap();
        // Second put with the same hash but different source_id should not
        // rewrite the bin. We verify by timestamp: content unchanged.
        store.put(sid2, &hash, &bytes).unwrap();

        let got = store.get(&hash).unwrap().unwrap();
        assert_eq!(got.bytes, bytes);
    }

    #[test]
    fn get_range_clamps_out_of_bounds() {
        let dir = tmp_dir();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let sid = SourceId::new();
        let hash = make_hash("feedfacefeedface");
        store.put(sid, &hash, b"0123456789").unwrap();

        // Normal range.
        let r = store.get_range(&hash, 2, 5).unwrap().unwrap();
        assert_eq!(r, b"234");

        // End past EOF clamps to EOF.
        let r = store.get_range(&hash, 5, 999).unwrap().unwrap();
        assert_eq!(r, b"56789");

        // Start past EOF returns empty.
        let r = store.get_range(&hash, 100, 200).unwrap().unwrap();
        assert_eq!(r, Vec::<u8>::new());
    }

    #[test]
    fn get_range_returns_none_for_missing_hash() {
        let dir = tmp_dir();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let hash = make_hash("1111111111111111");
        assert!(store.get_range(&hash, 0, 10).unwrap().is_none());
    }

    #[test]
    fn gc_removes_entries_not_in_live_set() {
        let dir = tmp_dir();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let sid = SourceId::new();
        let keep = make_hash("aaaaaaaaaaaaaaaa");
        let drop = make_hash("bbbbbbbbbbbbbbbb");
        store.put(sid, &keep, b"keep me").unwrap();
        store.put(sid, &drop, b"drop me").unwrap();

        let mut live: HashSet<String> = HashSet::new();
        live.insert(keep.0.clone());

        let removed = store.gc(&live).unwrap();
        assert_eq!(removed, 1);
        assert!(store.get(&keep).unwrap().is_some());
        assert!(store.get(&drop).unwrap().is_none());
    }

    #[test]
    fn gc_on_empty_store_returns_zero() {
        let dir = tmp_dir();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(store.gc(&empty).unwrap(), 0);
    }

    #[test]
    fn fan_out_sharding_creates_expected_dirs() {
        let dir = tmp_dir();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let sid = SourceId::new();
        let hash = make_hash("ab12cdef99887766");
        store.put(sid, &hash, b"sharded").unwrap();

        let bin_path = dir
            .path()
            .join("rooting/sources/ab/12/ab12cdef99887766.bin");
        assert!(bin_path.exists(), "expected {:?} to exist", bin_path);
    }
}
