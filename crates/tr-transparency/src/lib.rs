//! `tr-transparency` — append-only Merkle-tree log for `.tr` pack
//! publications.
//!
//! Mirrors Sigstore Rekor's design: every entry's leaf hash is the
//! SHA-256 of its canonical-JSON serialization, the log root is the
//! Merkle root over those leaves, and each entry carries an
//! inclusion proof so any third party can verify the log "saw" it
//! without trusting the operator.
//!
//! Storage is intentionally simple — one JSONL file per log under
//! `<dir>/log.jsonl`. Replicating to S3 / IPFS / a public mirror is
//! out of scope here; this crate provides the format + verification
//! primitives, and the cloud's transparency-log service writes its
//! entries through the same surface.
//!
//! The crate is split into:
//! - [`log`] — `TransparencyLog::{append, get, root, consistency_proof}`.
//! - [`proof`] — Merkle inclusion-proof verification for a single
//!   leaf against a known root.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod log;
pub mod proof;

pub use log::{LogEntry, LogEntryKind, TransparencyLog};
pub use proof::{InclusionProof, verify_inclusion};

/// Errors surfaced by `tr-transparency`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// I/O failure reading or writing the log file.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization failure.
    #[error("serde_json: {0}")]
    Json(#[from] serde_json::Error),

    /// Inclusion proof failed to reproduce the expected root.
    #[error("proof verification failed: {0}")]
    Proof(String),

    /// Requested an entry that does not exist in the log.
    #[error("entry index {0} out of range (log has {1} entries)")]
    OutOfRange(u64, u64),
}

/// `tr-transparency`'s `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
