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
    /// Unix timestamp of the first reflect cycle in which this pattern
    /// appeared (above thresholds). Preserved across re-runs.
    pub first_seen_at: f64,
    /// How many consecutive reflect cycles this pattern has survived.
    /// Drives the confidence-damping curve for gap generation — a
    /// pattern needs `stability_runs >= stability_ramp_runs` (config,
    /// default 5) to emit gaps at full confidence.
    pub stability_runs: u32,
    /// `"local"` for single-workspace patterns, `"cross:<id>"` for
    /// cross-workspace aggregates.
    pub source_scope: String,
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

/// Summary returned by `reflect_across_graphs` — one aggregate pattern
/// set plus per-workspace gap outcomes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrossReflectResult {
    /// The scope id used to tag the patterns and gaps in each
    /// workspace's graph (shape: `"cross:<hash>"`). Stable across runs
    /// for the same set of workspace names.
    pub scope_id: String,
    /// Aggregate patterns discovered over the union of all input
    /// graphs. Pattern `sample_size` is the sum across workspaces;
    /// `frequency` is `sum(both_n) / sum(cond_n)`.
    pub aggregate_patterns: Vec<StructuralPattern>,
    /// Per-workspace gap outcome, keyed by the workspace name passed
    /// to `reflect_across_graphs`.
    pub per_workspace: std::collections::HashMap<String, ReflectResult>,
    /// Workspaces scanned (just the names, for telemetry).
    pub workspaces: Vec<String>,
}

/// T3.2 — Cross-branch reflect result.
///
/// One per-branch `ReflectResult` plus a list of `DivergentPattern`
/// rows naming patterns that fired in some branches but not others.
/// Useful for spotting "branch A consistently rooted but branch B
/// always quarantined" or "main has the API-auth pattern but
/// feature/proto-y is missing it."
///
/// We do NOT aggregate samples across branches the way
/// `CrossReflectResult` does for cross-workspace — branches share a
/// substrate and aggregating would double-count claims that survive
/// a fork.  Divergence is the actually-novel signal cross-branch
/// reflect surfaces.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrossBranchReflectResult {
    /// Workspace name the branches live in.
    pub workspace: String,
    /// Branches scanned, in the order they were requested.  Includes
    /// `"main"` when the caller passed it; the order is preserved
    /// so consumers can pin which branch is "ours" vs "theirs" in a
    /// pairwise diff.
    pub branches: Vec<String>,
    /// Per-branch reflect outcome.  Same shape as
    /// `engine.reflect_branched(ws, Some(branch))` would have
    /// returned standalone.
    pub per_branch: std::collections::HashMap<String, ReflectResult>,
    /// Patterns that fired in at least one branch but not in every
    /// branch.  Empty when every branch surfaces an identical
    /// pattern set — the common case for unmodified forks.
    pub divergent_patterns: Vec<DivergentPattern>,
}

/// One row of `CrossBranchReflectResult::divergent_patterns`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DivergentPattern {
    /// Stable identifier (`pattern.id`) of the diverging pattern.
    pub pattern_id: String,
    /// Branches where this pattern fired (sample_size ≥ threshold,
    /// frequency ≥ threshold).
    pub present_in: Vec<String>,
    /// Branches where this pattern did NOT fire.  Disjoint from
    /// `present_in`; together they cover every branch in the result.
    pub absent_from: Vec<String>,
    /// Aggregate sample size across `present_in` branches — useful
    /// for sorting "weakly-diverging" rows (small sample) below
    /// "strongly-diverging" rows (large sample).
    pub aggregate_sample_size: usize,
}

/// A gap surfaced via the `gaps` MCP tool — denormalized for agent display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GapReport {
    /// Stable identifier assigned when the gap row was first upserted
    /// into `known_unknowns`. Pass this to [`dismiss_gap`] to suppress
    /// a false-positive gap — previously the id was read from CozoDB
    /// but discarded before reaching clients, making dismissals
    /// impossible from outside this crate.
    ///
    /// [`dismiss_gap`]: crate::dismiss_gap
    pub id: String,
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
