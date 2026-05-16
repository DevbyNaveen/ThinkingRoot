//! Merge-cognition plan — Phase γ.1 of the design doc
//! (`docs/2026-05-15-cognition-commits-design.md`).
//!
//! A `MergePlan` is the **deterministic** projection of two branches'
//! divergence: which commits are unique to each side since their
//! lowest common ancestor, which witnesses each side cited or added,
//! and how much overlap there is. The plan is computed purely from
//! the cognition-commit DAG + the witnesses each commit references —
//! no LLM, no probabilistic similarity, no prose semantics.
//!
//! The plan is what the AI receives as **context** when invited to
//! synthesize a merge commit. The plan's existence as a stand-alone
//! wire type means three things:
//!
//! 1. **Reviewable.** A human can read the plan before any AI runs
//!    and see exactly what differs between the branches.
//! 2. **Replayable.** Same inputs → same plan, byte-for-byte. The
//!    Cognition Exchange (Phase ζ.2) can ship plans between machines
//!    without re-deriving them from scratch.
//! 3. **Honest.** Citation sets are *exact* (drawn from real
//!    commits), not paraphrased. The downstream synthesis prompt
//!    receives ground-truth provenance, not an LLM's recollection.
//!
//! The plan deliberately stops short of *resolving* the merge. That's
//! γ.2 (LLM-driven synthesis) and γ.3 (conflict-resolution UI). γ.1's
//! job is to lay the substrate so those layers have a stable input.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{CommitId, WitnessId};

/// Coarse classification of how the two branches relate. Computed
/// from the cognition-commit DAG alone — no inspection of prose or
/// reasoning required.
///
/// Variant ordering reflects increasing merge work:
///   - `Identical` and `Ahead` are trivial (no synthesis needed).
///   - `Diverged` is the interesting case (γ.2 will synthesize).
///   - `NoCommonHistory` is rare but possible (two branches created
///     independently from no shared root) and is reportable as such.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitDivergence {
    /// Both branches sit on the same commit. No merge work.
    Identical,
    /// The right branch is an ancestor of the left branch — left is
    /// strictly ahead. A `merge_cognition` call collapses to a
    /// fast-forward (no synthesis commit needed).
    LeftAhead,
    /// The left branch is an ancestor of the right branch — right
    /// is strictly ahead. Symmetric to `LeftAhead`.
    RightAhead,
    /// Both branches have unique commits since their lowest common
    /// ancestor. Synthesis is interesting; the AI must reconcile.
    Diverged {
        /// The lowest common ancestor commit id. `None` when both
        /// branches share no history (see `NoCommonHistory` —
        /// `Diverged` always has a Some here by construction).
        common_ancestor: CommitId,
    },
    /// The two branches share no common ancestor. This can happen
    /// when branches are created independently from imports / packs
    /// without a shared root. The plan still surfaces both sides'
    /// commits so the AI can hand-roll a synthesis; the lack of LCA
    /// is part of the honest report.
    NoCommonHistory,
}

/// Aggregated witness-set classification for a side of the divergence.
///
/// Every field is a *set* (deduped, ordered by hex ascending) of the
/// witness ids appearing in commits unique to that side. Together
/// these tell the AI:
///   - "These are witnesses the left side cited but the right side
///     did not. Either the right side missed them (gap) or the left
///     side made a citation error (refutation candidate)."
///   - "These are witnesses both sides cited. Treat as common ground
///     — agreement is the synthesis's anchor."
///
/// Plans never embed full Witness rows here — only ids. The agent
/// uses `list_witnesses` / `get_witness` to pull the byte-anchored
/// payload it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WitnessClassification {
    /// Witnesses cited by left-only commits (set).
    pub left_only_citations: Vec<WitnessId>,
    /// Witnesses cited by right-only commits (set).
    pub right_only_citations: Vec<WitnessId>,
    /// Witnesses cited by commits on both sides (set).
    pub shared_citations: Vec<WitnessId>,
    /// Witnesses added (newly emitted) by left-only commits (set).
    pub left_only_added: Vec<WitnessId>,
    /// Witnesses added by right-only commits (set).
    pub right_only_added: Vec<WitnessId>,
    /// Witnesses added by commits on both sides (set).
    pub shared_added: Vec<WitnessId>,
    /// Gap ids surfaced by left-only commits (set).
    pub left_only_gaps: Vec<String>,
    /// Gap ids surfaced by right-only commits (set).
    pub right_only_gaps: Vec<String>,
    /// Gap ids surfaced by commits on both sides (set).
    pub shared_gaps: Vec<String>,
}

impl WitnessClassification {
    /// True when both sides cited at least one witness in common.
    /// Useful as a quick triage hint for the synthesis prompt: when
    /// `false`, the branches are talking past each other and a
    /// merge will need to explicitly justify how to reconcile.
    pub fn has_shared_anchor(&self) -> bool {
        !self.shared_citations.is_empty() || !self.shared_added.is_empty()
    }

    /// Total witness ids referenced across both sides. Cheap; used
    /// in the plan's summary header so a reviewer can size the
    /// merge before reading the lists.
    pub fn total_witnesses(&self) -> usize {
        self.left_only_citations.len()
            + self.right_only_citations.len()
            + self.shared_citations.len()
            + self.left_only_added.len()
            + self.right_only_added.len()
            + self.shared_added.len()
    }
}

/// Full merge plan returned by `compute_merge_plan` /
/// `engine.compute_merge_plan` / `merge_cognition` MCP tool.
///
/// Field ordering reflects how a reviewer reads the plan: identity
/// (which branches), conflict-kind summary, then the divergence
/// details. Don't reorder lightly — REST consumers and the eventual
/// γ.3 React view pin the layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergePlan {
    /// Left branch name (as supplied to `compute_merge_plan`).
    pub left_branch: String,
    /// Right branch name.
    pub right_branch: String,
    /// Head commit on the left branch — newest commit by `created_at`.
    /// `None` when the left branch has no commits at all.
    pub left_head: Option<CommitId>,
    /// Head commit on the right branch.
    pub right_head: Option<CommitId>,
    /// Conflict-kind classification — drives whether γ.2 needs to
    /// synthesize or can fast-forward.
    pub conflict_kind: CommitDivergence,
    /// Commit ids unique to the left branch since the LCA, in
    /// descending `created_at` order (newest first). Empty when the
    /// left branch is at-or-behind the right.
    pub left_only_commits: Vec<CommitId>,
    /// Commit ids unique to the right branch since the LCA, in
    /// descending `created_at` order (newest first).
    pub right_only_commits: Vec<CommitId>,
    /// Aggregated witness + gap classification across the two
    /// divergent regions.
    pub witnesses: WitnessClassification,
    /// Wall-clock timestamp when the plan was computed. NOT part of
    /// any identity — the same plan replayed tomorrow gets a fresh
    /// `computed_at` but every other field is byte-for-byte stable.
    pub computed_at: DateTime<Utc>,
}

/// Phase γ.2 — Outcome of `synthesize_merge`.
///
/// Drives whether the React conflict-resolution view (γ.3) shows the
/// synthesis as a proposed commit, a "no merge needed" affordance, or
/// an honest "LLM unavailable / failed" empty state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SynthesisOutcome {
    /// The plan was `Identical` / `LeftAhead` / `RightAhead` — no
    /// synthesis was generated because none is needed.
    Trivial,
    /// The workspace has no LLM client configured. UI surfaces this
    /// as "wire an LLM provider in Settings to enable merge synthesis."
    LlmUnavailable,
    /// The LLM call failed. `message` is the underlying error text.
    LlmError(String),
    /// The synthesis was generated. `reasoning` + `verified_citations`
    /// are populated on the parent `MergeSynthesis`.
    Synthesized,
}

/// Phase γ.2 — Full synthesis result.
///
/// Carries the deterministic plan AND the LLM-generated synthesis (or
/// the honest empty state when LLM was unavailable / failed). The
/// caller (chat agent or React UI) decides whether to record the
/// synthesis as an actual cognition commit via `commit_cognition`;
/// γ.2 never writes the commit itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeSynthesis {
    /// What happened.
    pub outcome: SynthesisOutcome,
    /// The plan the synthesis is grounded in. Echoing it back lets a
    /// React UI render plan + synthesis in one pass without two REST
    /// round-trips.
    pub plan: MergePlan,
    /// The LLM's reasoning text. Empty on `Trivial` /
    /// `LlmUnavailable` / `LlmError`.
    pub reasoning: String,
    /// Witness ids the LLM cited that actually exist in the plan's
    /// surfaced witness sets. The honesty contract: this is the only
    /// citation set a downstream `commit_cognition` should trust.
    pub verified_citations: Vec<WitnessId>,
    /// Witness ids the LLM tried to cite but that do NOT appear in
    /// the plan. UI renders these as "ignored / fabricated" so the
    /// reviewer can see the model's hallucination rate.
    pub dropped_citations: Vec<WitnessId>,
    /// Model identifier used for the synthesis (empty when
    /// `outcome == Trivial | LlmUnavailable`).
    pub model: String,
}

impl MergeSynthesis {
    /// True when the synthesis actually produced reasoning the user
    /// could commit. UI flag for "Commit synthesis" button enabled.
    pub fn is_committable(&self) -> bool {
        matches!(self.outcome, SynthesisOutcome::Synthesized)
            && !self.reasoning.trim().is_empty()
    }
}

impl MergePlan {
    /// Build a fresh plan from already-classified pieces. The
    /// `cognition_merge` module in `thinkingroot-graph` is the
    /// canonical caller; tests can build plans by hand for fixture
    /// assertions.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        left_branch: String,
        right_branch: String,
        left_head: Option<CommitId>,
        right_head: Option<CommitId>,
        conflict_kind: CommitDivergence,
        left_only_commits: Vec<CommitId>,
        right_only_commits: Vec<CommitId>,
        witnesses: WitnessClassification,
        computed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            left_branch,
            right_branch,
            left_head,
            right_head,
            conflict_kind,
            left_only_commits,
            right_only_commits,
            witnesses,
            computed_at,
        }
    }

    /// True when no synthesis is needed — `Identical`, `LeftAhead`,
    /// or `RightAhead` resolve mechanically. Hint for the eventual
    /// γ.2 LLM-synthesis gate to skip the prompt entirely.
    pub fn is_trivial(&self) -> bool {
        matches!(
            self.conflict_kind,
            CommitDivergence::Identical
                | CommitDivergence::LeftAhead
                | CommitDivergence::RightAhead
        )
    }

    /// Count of commits the AI would need to reconcile. Sum of the
    /// two `_only_commits` lengths.
    pub fn divergent_commit_count(&self) -> usize {
        self.left_only_commits.len() + self.right_only_commits.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(byte: u8) -> CommitId {
        CommitId([byte; 32])
    }

    fn wid(byte: u8) -> WitnessId {
        WitnessId([byte; 32])
    }

    fn fixed_now() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-16T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn identical_plan_is_trivial() {
        let p = MergePlan::new(
            "main".to_string(),
            "feature/x".to_string(),
            Some(cid(1)),
            Some(cid(1)),
            CommitDivergence::Identical,
            vec![],
            vec![],
            WitnessClassification::default(),
            fixed_now(),
        );
        assert!(p.is_trivial());
        assert_eq!(p.divergent_commit_count(), 0);
    }

    #[test]
    fn left_ahead_is_trivial() {
        let p = MergePlan::new(
            "main".to_string(),
            "feature/x".to_string(),
            Some(cid(2)),
            Some(cid(1)),
            CommitDivergence::LeftAhead,
            vec![cid(2)],
            vec![],
            WitnessClassification::default(),
            fixed_now(),
        );
        assert!(p.is_trivial());
        assert_eq!(p.divergent_commit_count(), 1);
    }

    #[test]
    fn diverged_is_not_trivial() {
        let p = MergePlan::new(
            "main".to_string(),
            "feature/x".to_string(),
            Some(cid(3)),
            Some(cid(4)),
            CommitDivergence::Diverged {
                common_ancestor: cid(1),
            },
            vec![cid(3)],
            vec![cid(4)],
            WitnessClassification::default(),
            fixed_now(),
        );
        assert!(!p.is_trivial());
        assert_eq!(p.divergent_commit_count(), 2);
    }

    #[test]
    fn no_common_history_is_not_trivial() {
        let p = MergePlan::new(
            "main".to_string(),
            "feature/x".to_string(),
            Some(cid(3)),
            Some(cid(4)),
            CommitDivergence::NoCommonHistory,
            vec![cid(3)],
            vec![cid(4)],
            WitnessClassification::default(),
            fixed_now(),
        );
        assert!(!p.is_trivial());
    }

    #[test]
    fn witness_classification_has_shared_anchor_detects_overlap() {
        let mut wc = WitnessClassification::default();
        assert!(!wc.has_shared_anchor());
        wc.shared_citations.push(wid(7));
        assert!(wc.has_shared_anchor());
    }

    #[test]
    fn witness_classification_total_sums_every_bucket() {
        let wc = WitnessClassification {
            left_only_citations: vec![wid(1)],
            right_only_citations: vec![wid(2), wid(3)],
            shared_citations: vec![wid(4)],
            left_only_added: vec![wid(5)],
            right_only_added: vec![],
            shared_added: vec![wid(6)],
            left_only_gaps: vec!["gap-a".into()],
            right_only_gaps: vec![],
            shared_gaps: vec![],
        };
        // total_witnesses counts witness-id buckets only — gaps live
        // separately so callers can size "agreement vs disagreement"
        // on witnesses alone without conflating with surfaced gaps.
        assert_eq!(wc.total_witnesses(), 6);
    }

    #[test]
    fn merge_plan_serde_round_trip_diverged() {
        let plan = MergePlan::new(
            "main".to_string(),
            "feature/x".to_string(),
            Some(cid(3)),
            Some(cid(4)),
            CommitDivergence::Diverged {
                common_ancestor: cid(1),
            },
            vec![cid(3)],
            vec![cid(4)],
            WitnessClassification {
                left_only_citations: vec![wid(10)],
                shared_citations: vec![wid(20)],
                ..Default::default()
            },
            fixed_now(),
        );
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: MergePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.left_branch, plan.left_branch);
        assert_eq!(parsed.right_branch, plan.right_branch);
        assert_eq!(parsed.left_head, plan.left_head);
        assert_eq!(parsed.right_head, plan.right_head);
        assert_eq!(parsed.left_only_commits, plan.left_only_commits);
        assert_eq!(parsed.right_only_commits, plan.right_only_commits);
        assert_eq!(
            parsed.witnesses.left_only_citations,
            plan.witnesses.left_only_citations
        );
        assert_eq!(
            parsed.witnesses.shared_citations,
            plan.witnesses.shared_citations
        );
        match parsed.conflict_kind {
            CommitDivergence::Diverged { common_ancestor } => {
                assert_eq!(common_ancestor, cid(1));
            }
            other => panic!("expected Diverged, got {other:?}"),
        }
    }

    #[test]
    fn merge_plan_serde_round_trip_no_common_history() {
        let plan = MergePlan::new(
            "main".to_string(),
            "feature/x".to_string(),
            Some(cid(3)),
            Some(cid(4)),
            CommitDivergence::NoCommonHistory,
            vec![cid(3)],
            vec![cid(4)],
            WitnessClassification::default(),
            fixed_now(),
        );
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: MergePlan = serde_json::from_str(&json).unwrap();
        match parsed.conflict_kind {
            CommitDivergence::NoCommonHistory => {}
            other => panic!("expected NoCommonHistory, got {other:?}"),
        }
    }
}
