//! Per-batch checkpoint log so a killed compile resumes from the
//! last completed batch instead of redoing 20 minutes of LLM work.
//!
//! Pre-C6 the only durable state mid-extract was the per-chunk
//! content-addressed cache at `<data_dir>/cache/extraction/<hash>.json`
//! — that already let a re-run skip individual chunks once their cache
//! entry was written, but it didn't tell the extractor *which batches*
//! it had already finished.  Without that, a re-run reissued every
//! pending LLM call (cache hits would handle the work, so cost was
//! bounded — but the wall-clock cost of N parallel cache reads per
//! batch was real).
//!
//! Wire format: JSONL at `<data_dir>/cache/extraction/.in-flight.jsonl`.
//! One line per completed batch.  Atomic append: each write is opened
//! O_APPEND and a single short `write_all` of the line bytes.  The
//! file is cleared by the orchestrator after Phase 7 (Linker) succeeds
//! — at that point CozoDB has the data and the in-flight log is
//! redundant.
//!
//! Recovery semantics:
//! - process killed mid-extract: next run loads the recorded batch
//!   indexes, the extractor's hot-loop skips them, only un-recorded
//!   batches go to the LLM.
//! - process killed between extract and link (same data_dir): the
//!   per-chunk cache is still authoritative; on the next run the
//!   fingerprint check sees no graph rows for those sources, so they
//!   re-extract from cache (cheap), then re-link.
//! - failed batches do *not* land in the in-flight log — only fully
//!   successful ones.  The C4 partial-failure counter handles their
//!   surfacing.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thinkingroot_core::{Error, Result};

const FILE_NAME: &str = ".in-flight.jsonl";
const SCHEMA_VERSION: u8 = 1;

/// One line in `.in-flight.jsonl`.  We keep the schema deliberately
/// thin — the extractor's per-chunk cache holds the actual claims, so
/// this file just needs to remember which batches are *done* and how
/// many chunks they accounted for (so the resume path can update its
/// progress bar denominator).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointEntry {
    /// Schema version.  Mismatches abort the load — better to redo
    /// than to silently misinterpret an old entry.
    v: u8,
    /// 0-indexed batch number, matching the index used by the
    /// `llm_work.chunks(batch_size).enumerate()` loop in `extract_all`.
    batch_idx: usize,
    /// Inclusive 1-indexed `range_start` (mirrors
    /// `ProgressEvent::ExtractionBatchStart::range_start`).
    range_start: usize,
    /// Inclusive 1-indexed `range_end`.
    range_end: usize,
    /// Number of original chunks the batch contained.  Used so the
    /// resume path can fast-forward `chunks_processed` and the
    /// progress denominator without re-reading the cache.
    batch_chunks: usize,
}

/// Writer side of the checkpoint log.  Held by `Extractor` for the
/// duration of `extract_all` so each completed batch can append a
/// record.  Wraps a `Mutex<File>` because we're called from the
/// concurrent collect loop where multiple batches finish in parallel.
pub struct InFlightCheckpoint {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl InFlightCheckpoint {
    /// Open (creating if necessary) the checkpoint log under
    /// `<data_dir>/cache/extraction/`.  The cache directory already
    /// exists if the extractor has a `with_cache_dir` configured;
    /// otherwise we create it lazily.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("cache").join("extraction");
        std::fs::create_dir_all(&dir).map_err(|e| Error::io_path(&dir, e))?;
        let path = dir.join(FILE_NAME);
        // O_APPEND: every write atomically positions at end-of-file
        // before writing.  This is the property we rely on for
        // concurrent-safe per-batch appends without an outer fsync
        // dance.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::io_path(&path, e))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// Record a successfully completed batch.  Atomic w.r.t. concurrent
    /// `record_batch` calls — each append is a single short write to
    /// an O_APPEND fd, which the kernel serialises.  Errors are
    /// surfaced rather than swallowed because a silently-broken
    /// checkpoint is the bug we're trying to fix.
    pub fn record_batch(
        &self,
        batch_idx: usize,
        range_start: usize,
        range_end: usize,
        batch_chunks: usize,
    ) -> Result<()> {
        let entry = CheckpointEntry {
            v: SCHEMA_VERSION,
            batch_idx,
            range_start,
            range_end,
            batch_chunks,
        };
        let mut line = serde_json::to_vec(&entry)
            .map_err(|e| Error::Config(format!("checkpoint encode: {e}")))?;
        line.push(b'\n');
        let mut guard = self
            .file
            .lock()
            .map_err(|e| Error::Config(format!("checkpoint lock poisoned: {e}")))?;
        use std::io::Write;
        guard
            .write_all(&line)
            .map_err(|e| Error::io_path(&self.path, e))?;
        Ok(())
    }

    /// Look up the set of batch indexes already completed in a previous
    /// run.  Returns `Ok(empty set)` when the file does not exist
    /// (clean state) or is empty.  Schema-version mismatches and
    /// malformed lines abort the load with `Err` so the caller can
    /// decide between "redo everything" (typical) or "bail loudly".
    pub fn load_completed_batches(data_dir: &Path) -> Result<CompletedBatches> {
        let path = data_dir.join("cache").join("extraction").join(FILE_NAME);
        if !path.exists() {
            return Ok(CompletedBatches::default());
        }
        let bytes = std::fs::read(&path).map_err(|e| Error::io_path(&path, e))?;
        let mut completed = CompletedBatches::default();
        for (line_no, raw) in bytes.split(|&b| b == b'\n').enumerate() {
            if raw.is_empty() {
                continue;
            }
            let entry: CheckpointEntry = serde_json::from_slice(raw).map_err(|e| {
                Error::Config(format!(
                    "{}: malformed checkpoint at line {}: {e}",
                    path.display(),
                    line_no + 1
                ))
            })?;
            if entry.v != SCHEMA_VERSION {
                return Err(Error::Config(format!(
                    "{}: unsupported checkpoint schema v{} (expected v{})",
                    path.display(),
                    entry.v,
                    SCHEMA_VERSION
                )));
            }
            completed.batches.insert(entry.batch_idx);
            completed.chunks_already_done += entry.batch_chunks;
        }
        Ok(completed)
    }

    /// Remove the checkpoint log.  Called by the orchestrator after
    /// Phase 7 (Linker) succeeds — at that point the working CozoDB
    /// is the source of truth and the in-flight log is dead weight.
    /// Idempotent: missing file is success.
    pub fn clear(data_dir: &Path) -> Result<()> {
        let path = data_dir.join("cache").join("extraction").join(FILE_NAME);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io_path(&path, e)),
        }
    }
}

/// Snapshot of which batches have already been processed.  Returned by
/// `InFlightCheckpoint::load_completed_batches` and consulted by
/// `Extractor::extract_all` in the resume path.
#[derive(Debug, Default, Clone)]
pub struct CompletedBatches {
    /// 0-indexed batch numbers that have been recorded as complete.
    /// `Extractor::extract_all` skips any batch whose index appears
    /// here — its claims came back from the per-chunk content-
    /// addressed cache on the prior run and are already on disk.
    pub batches: std::collections::HashSet<usize>,
    /// Sum of `batch_chunks` across all recorded entries.  The
    /// resume path uses this to fast-forward the progress bar
    /// denominator.
    pub chunks_already_done: usize,
}

impl CompletedBatches {
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    pub fn contains(&self, batch_idx: usize) -> bool {
        self.batches.contains(&batch_idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn open_in_clean_dir_returns_empty_completed_set() {
        let tmp = data_dir();
        let c = InFlightCheckpoint::load_completed_batches(tmp.path()).unwrap();
        assert!(
            c.is_empty(),
            "fresh data_dir must have no completed batches"
        );
        assert_eq!(c.chunks_already_done, 0);
    }

    #[test]
    fn record_then_load_returns_recorded_batches() {
        let tmp = data_dir();
        let ckpt = InFlightCheckpoint::open(tmp.path()).unwrap();
        ckpt.record_batch(0, 1, 6, 6).unwrap();
        ckpt.record_batch(1, 7, 12, 6).unwrap();
        ckpt.record_batch(3, 19, 24, 6).unwrap(); // sparse — batch 2 failed and is absent
        drop(ckpt);

        let loaded = InFlightCheckpoint::load_completed_batches(tmp.path()).unwrap();
        assert!(loaded.contains(0));
        assert!(loaded.contains(1));
        assert!(!loaded.contains(2), "skipped batch must not appear");
        assert!(loaded.contains(3));
        assert_eq!(loaded.chunks_already_done, 18);
    }

    #[test]
    fn clear_removes_log_idempotently() {
        let tmp = data_dir();
        let ckpt = InFlightCheckpoint::open(tmp.path()).unwrap();
        ckpt.record_batch(0, 1, 6, 6).unwrap();
        drop(ckpt);
        InFlightCheckpoint::clear(tmp.path()).unwrap();
        InFlightCheckpoint::clear(tmp.path()).unwrap(); // idempotent
        let loaded = InFlightCheckpoint::load_completed_batches(tmp.path()).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn malformed_line_aborts_load_loudly() {
        let tmp = data_dir();
        let dir = tmp.path().join("cache").join("extraction");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(FILE_NAME),
            b"{\"v\":1,\"batch_idx\":0,\"range_start\":1,\"range_end\":6,\"batch_chunks\":6}\n\
              not-actually-json\n",
        )
        .unwrap();
        let err = InFlightCheckpoint::load_completed_batches(tmp.path()).unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("malformed checkpoint"),
            "expected loud error, got {s}"
        );
    }

    #[test]
    fn schema_version_mismatch_aborts_load() {
        let tmp = data_dir();
        let dir = tmp.path().join("cache").join("extraction");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(FILE_NAME),
            b"{\"v\":99,\"batch_idx\":0,\"range_start\":1,\"range_end\":6,\"batch_chunks\":6}\n",
        )
        .unwrap();
        let err = InFlightCheckpoint::load_completed_batches(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("unsupported checkpoint schema"));
    }

    #[test]
    fn concurrent_record_batch_is_safe() {
        // The Mutex<File> wrapper plus O_APPEND on the underlying fd
        // means concurrent appends from rayon-style threads must
        // serialise cleanly — no torn lines, no lost records.
        use std::sync::Arc;
        use std::thread;
        let tmp = data_dir();
        let ckpt = Arc::new(InFlightCheckpoint::open(tmp.path()).unwrap());
        let mut handles = Vec::new();
        for i in 0..16 {
            let c = Arc::clone(&ckpt);
            handles.push(thread::spawn(move || {
                c.record_batch(i, i * 6 + 1, (i + 1) * 6, 6).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        drop(ckpt);
        let loaded = InFlightCheckpoint::load_completed_batches(tmp.path()).unwrap();
        assert_eq!(loaded.batches.len(), 16, "all 16 records must be present");
        assert_eq!(loaded.chunks_already_done, 96);
    }
}
