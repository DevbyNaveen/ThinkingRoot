//! Per-claim trial outcomes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thinkingroot_core::types::{AdmissionTier, ClaimId};

use crate::probes::ProbeResult;

/// Outcome of running all five Rooting probes against one candidate claim.
/// Persisted append-only to the `trial_verdicts` relation for audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialVerdict {
    /// ULID of this verdict (not the claim).
    pub id: String,
    /// Which claim was tried.
    pub claim_id: ClaimId,
    /// When the trial ran.
    pub trial_at: DateTime<Utc>,
    /// The admission tier assigned by this trial.
    pub admission_tier: AdmissionTier,
    /// Per-probe results in fixed order: provenance, contradiction, predicate,
    /// topology, temporal. Shorter arrays indicate short-circuit on fatal failure.
    pub probes: Vec<ProbeResult>,
    /// Certificate hash (BLAKE3 hex) if a certificate was issued. `None` for
    /// Rejected-tier verdicts (no certificate for failed admission).
    pub certificate_hash: Option<String>,
    /// Short human-readable reason when tier is Quarantined or Rejected.
    pub failure_reason: Option<String>,
    /// Version of the Rooter that produced this verdict.
    pub rooter_version: String,
}

impl TrialVerdict {
    /// Whether this verdict admitted the claim (`Rooted` or `Quarantined`).
    pub fn admitted(&self) -> bool {
        matches!(
            self.admission_tier,
            AdmissionTier::Rooted | AdmissionTier::Quarantined
        )
    }
}
