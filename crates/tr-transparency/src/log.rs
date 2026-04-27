//! Append-only transparency log.
//!
//! Storage: one JSON-lines file (`log.jsonl`). Each line is the
//! canonical-JSON of one [`LogEntry`]; recomputing the leaf hash
//! is a `sha256(line_bytes)`.
//!
//! Concurrency: callers serialize their writes themselves. The
//! cloud transparency service holds a tokio Mutex around its log
//! handle; CLI usage is single-threaded by construction.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::proof::{InclusionProof, build_proof, root_hash};

/// One row in the transparency log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// What kind of artifact this entry records.
    pub kind: LogEntryKind,
    /// Pack reference, e.g. `alice/thesis@0.1.0`.
    pub pack_ref: String,
    /// Hex-encoded manifest content hash from `tr_format::Manifest`.
    pub manifest_hash: String,
    /// DID of the publishing identity, e.g. `did:web:alice.example`.
    pub author_did: String,
    /// Base64-encoded Ed25519 signature over the manifest hash.
    pub signature: String,
    /// When the entry was appended (UTC).
    pub timestamp: DateTime<Utc>,
}

/// Discriminator for [`LogEntry::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogEntryKind {
    /// A new pack version was published.
    Publish,
    /// A previously-published pack was revoked.
    Revoke,
}

/// Append-only Merkle-tree-style transparency log backed by a
/// JSON-lines file.
#[derive(Debug)]
pub struct TransparencyLog {
    path: PathBuf,
    entries: Vec<LogEntry>,
}

impl TransparencyLog {
    /// Open or create a log at `<dir>/log.jsonl`. Reads any pre-
    /// existing entries into memory so [`Self::root`] +
    /// [`Self::get`] are O(1).
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let path = dir.as_ref().join("log.jsonl");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut entries = Vec::new();
        if path.exists() {
            let file = OpenOptions::new().read(true).open(&path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let entry: LogEntry = serde_json::from_str(&line)?;
                entries.push(entry);
            }
        }
        Ok(Self { path, entries })
    }

    /// Append a new entry. Returns the leaf index it now occupies.
    pub fn append(&mut self, entry: LogEntry) -> Result<u64> {
        let line = serde_json::to_string(&entry)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        let index = self.entries.len() as u64;
        self.entries.push(entry);
        Ok(index)
    }

    /// Read an entry by index plus its inclusion proof against the
    /// current Merkle root.
    pub fn get(&self, index: u64) -> Result<(LogEntry, InclusionProof)> {
        let len = self.entries.len() as u64;
        if index >= len {
            return Err(crate::Error::OutOfRange(index, len));
        }
        let entry = self.entries[index as usize].clone();
        let leaves = self.leaf_hashes();
        let proof = build_proof(&leaves, index as usize);
        Ok((entry, proof))
    }

    /// Current Merkle root over all leaves. Returns
    /// `Some(hash, leaf_count)` once at least one entry exists.
    pub fn root(&self) -> Option<([u8; 32], u64)> {
        let leaves = self.leaf_hashes();
        let len = leaves.len() as u64;
        root_hash(&leaves).map(|h| (h, len))
    }

    /// Number of entries.
    pub fn len(&self) -> u64 {
        self.entries.len() as u64
    }

    /// `true` if the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Compute a consistency proof between two log sizes
    /// (`old_size <= new_size <= len()`). Returns the list of
    /// hashes a verifier needs to combine to derive the new root
    /// from the old.
    pub fn consistency_proof(&self, old_size: u64, new_size: u64) -> Result<Vec<[u8; 32]>> {
        if old_size > new_size || new_size > self.len() {
            return Err(crate::Error::OutOfRange(new_size, self.len()));
        }
        let leaves = self.leaf_hashes();
        Ok(crate::proof::build_consistency(&leaves, old_size as usize, new_size as usize))
    }

    fn leaf_hashes(&self) -> Vec<[u8; 32]> {
        self.entries
            .iter()
            .map(|e| {
                // Canonical bytes = serde_json::to_vec is deterministic
                // for our struct (no maps with ambiguous ordering).
                let bytes = serde_json::to_vec(e).expect("LogEntry always serialises");
                let mut h = Sha256::new();
                h.update(&bytes);
                h.finalize().into()
            })
            .collect()
    }
}
