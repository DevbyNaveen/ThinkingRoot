//! Cryptographic verification certificates for admitted claims.
//!
//! A [`Certificate`] is a deterministic BLAKE3 hash over canonical JSON of a
//! trial's inputs and outputs. Re-running the same trial against the same
//! source bytes yields the same hash, making certificates re-verifiable by
//! any third party without re-running the LLM pipeline.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A re-verifiable proof that a claim passed the Rooting trial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Certificate {
    /// BLAKE3 hex of the canonical input struct.
    pub hash: String,
    /// Claim this certificate covers.
    pub claim_id: String,
    /// When this certificate was created.
    pub created_at: DateTime<Utc>,
    /// Canonical JSON of the probe inputs (source content hashes, predicate
    /// hash, contradiction query hash, derivation parents).
    pub probe_inputs_json: String,
    /// Canonical JSON of the per-probe outputs (name, score, passed, detail).
    pub probe_outputs_json: String,
    /// Version of the Rooter that produced this certificate.
    pub rooter_version: String,
    /// BLAKE3 of the source content at trial time. Used to detect drift:
    /// if the stored source hash no longer matches the live source, the
    /// certificate is known to be stale without re-running probes.
    pub source_content_hash: String,
}
