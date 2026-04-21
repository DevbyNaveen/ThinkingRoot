//! Types returned by the Reflect engine.

use serde::{Deserialize, Serialize};

/// One co-occurrence pattern discovered from graph topology.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructuralPattern {
    /// Stable id derived from (entity_type, condition, expected).
    pub id: String,
    pub entity_type: String,
    pub condition_claim_type: String,
    pub expected_claim_type: String,
    /// Fraction in [0.0, 1.0] — among entities of `entity_type` that have
    /// `condition_claim_type`, the fraction that also have `expected_claim_type`.
    pub frequency: f64,
    /// Number of entities of `entity_type` that have `condition_claim_type`.
    /// Used as the denominator for `frequency`.
    pub sample_size: usize,
    pub last_computed: f64,
    pub min_sample_threshold: usize,
}

/// One gap: an entity is missing an expected claim-type per a pattern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownUnknown {
    pub id: String,
    pub entity_id: String,
    pub pattern_id: String,
    pub expected_claim_type: String,
    pub confidence: f64,
    pub status: GapStatus,
    pub created_at: f64,
    pub resolved_at: f64,
    pub resolved_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GapStatus {
    Open,
    Resolved,
    Dismissed,
}

impl GapStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GapStatus::Open => "open",
            GapStatus::Resolved => "resolved",
            GapStatus::Dismissed => "dismissed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(GapStatus::Open),
            "resolved" => Some(GapStatus::Resolved),
            "dismissed" => Some(GapStatus::Dismissed),
            _ => None,
        }
    }
}

/// Summary returned from one `ReflectEngine::reflect` run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReflectResult {
    /// Patterns discovered in this run (all patterns with sample_size >=
    /// min_sample_threshold and frequency >= min_frequency).
    pub patterns: Vec<StructuralPattern>,
    /// Net new gaps created (status = open) this run.
    pub gaps_created: usize,
    /// Gaps that were previously open and have now been resolved by a
    /// claim matching their `expected_claim_type` on the same entity.
    pub gaps_resolved: usize,
    /// Gaps that were open in the previous state and remain open (carried
    /// forward; still missing).
    pub gaps_still_open: usize,
    /// Total open gaps after this run. Used by the health-coverage score.
    pub open_gaps_total: usize,
    /// Entity-types inspected this run (useful for telemetry).
    pub entity_types_scanned: usize,
}

/// A gap surfaced via the `gaps` MCP tool — denormalized for agent display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GapReport {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: String,
    pub expected_claim_type: String,
    pub confidence: f64,
    /// Human-readable reason ("92% of Service entities with endpoints
    /// also have auth info — PaymentService does not.").
    pub reason: String,
    pub pattern_id: String,
    pub sample_size: usize,
    pub created_at: f64,
}
