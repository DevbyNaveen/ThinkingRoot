// crates/thinkingroot-core/src/types/diff.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HealthScore;
use crate::types::{Claim, Entity};

/// A computed semantic diff between two knowledge branches.
///
/// Captures everything needed for a human or agent to review what changed
/// on a branch before merging it into its parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeDiff {
    /// The source branch being compared.
    pub from_branch: String,
    /// The target branch (usually "main").
    pub to_branch: String,
    /// When this diff was computed.
    pub computed_at: DateTime<Utc>,
    /// Claims present in `from_branch` but not in `to_branch`.
    pub new_claims: Vec<DiffClaim>,
    /// Entities present in `from_branch` but not in `to_branch`.
    pub new_entities: Vec<DiffEntity>,
    /// Relations present in `from_branch` but not in `to_branch`.
    pub new_relations: Vec<DiffRelation>,
    /// Contradictions that were automatically resolved by confidence heuristic.
    pub auto_resolved: Vec<AutoResolution>,
    /// Contradictions that require human or agent review before merging.
    pub needs_review: Vec<ContradictionPair>,
    /// Health score of the target branch before the merge.
    pub health_before: HealthScore,
    /// Projected health score of the target branch after the merge.
    pub health_after: HealthScore,
    /// Whether the merge is currently allowed given health gates and unresolved contradictions.
    pub merge_allowed: bool,
    /// Human-readable reasons why `merge_allowed` is false, if applicable.
    pub blocking_reasons: Vec<String>,
}

/// A single claim annotated with its diff status and entity context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffClaim {
    /// The claim itself.
    pub claim: Claim,
    /// Names of entities this claim is attributed to (for display purposes).
    pub entity_context: Vec<String>,
    /// Whether the claim was added, modified, or removed.
    pub diff_status: DiffStatus,
}

/// A single entity annotated with its diff status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEntity {
    /// The entity itself.
    pub entity: Entity,
    /// Whether the entity was added, modified, or removed.
    pub diff_status: DiffStatus,
}

/// A single relation annotated with its diff status and named entity endpoints.
///
/// Carries the entity names directly so consumers can display and merge without
/// a secondary graph lookup — the opaque `EntityId`s on `Relation` are not
/// useful in a cross-graph context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRelation {
    /// Canonical name of the source entity.
    pub from_name: String,
    /// Canonical name of the target entity.
    pub to_name: String,
    /// Relation type as a string (e.g. "depends_on", "uses").
    pub relation_type: String,
    /// Relation strength [0.0, 1.0].
    pub strength: f64,
    /// Whether the relation was added, modified, or removed.
    pub diff_status: DiffStatus,
}

/// Categorises a knowledge item's change status within a diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffStatus {
    /// Item exists in the branch but not in the base.
    Added,
    /// Item exists in both but has changed.
    Modified,
    /// Item existed in the base but was removed on the branch.
    Removed,
}

/// Records an automatically resolved contradiction between two claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoResolution {
    /// ID of the claim on the main branch.
    pub main_claim_id: String,
    /// ID of the contradicting claim on the feature branch.
    pub branch_claim_id: String,
    /// ID of the claim that was chosen as the winner.
    pub winner: String,
    /// Difference in confidence scores that triggered auto-resolution.
    pub confidence_delta: f64,
    /// Human-readable explanation of why this claim won.
    pub reason: String,
}

/// A pair of contradicting claims that require manual resolution before merging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionPair {
    /// ID of the claim on the main branch.
    pub main_claim_id: String,
    /// ID of the contradicting claim on the feature branch.
    pub branch_claim_id: String,
    /// Text of the main branch claim.
    pub main_statement: String,
    /// Text of the branch claim.
    pub branch_statement: String,
    /// Explanation of why these claims are considered contradictory.
    pub explanation: String,
}
