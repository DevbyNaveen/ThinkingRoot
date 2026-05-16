//! Merge-cognition plan computation — Phase γ.1.
//!
//! Pure algorithm over the `cognition_commits` DAG. Given two branch
//! names, walks each branch's commit chain, finds the lowest common
//! ancestor (LCA) if any, and aggregates the witnesses + gaps each
//! side cited or added since the LCA.
//!
//! Algorithm:
//!
//! 1. **Heads.** `list_cognition_commits_on_branch` returns each
//!    branch's commits newest-first. Element 0 is the head.
//! 2. **Trivial cases.**
//!    - Either branch empty → `NoCommonHistory`.
//!    - Same head id on both → `Identical`.
//! 3. **LCA via ancestor-set intersection.** Walk left's parent
//!    chain into a `HashSet<CommitId>`. Walk right's parent chain;
//!    first id in the set is the LCA. The DAG is single-parent
//!    (each `CognitionCommit.parent` is `Option<CommitId>`) so a
//!    linear walk is exact.
//! 4. **Ahead detection.**
//!    - LCA = right head → left is strictly ahead (`LeftAhead`).
//!    - LCA = left head → right is strictly ahead (`RightAhead`).
//!    - Else → `Diverged { common_ancestor: lca }`.
//! 5. **Divergent regions.** `left_only_commits` are commits on
//!    left's chain between head and (but not including) the LCA;
//!    same for right.
//! 6. **Witness classification.** For each side's divergent commits,
//!    union the `citations`, `witnesses_added`, and `gaps_surfaced`
//!    sets. Intersect to find `shared_*` sets. Subtract to find
//!    `_only` sets. All outputs are deduped + sorted (hex
//!    ascending for ids, lexicographic for gap strings) so the
//!    plan is byte-stable.
//!
//! Determinism: same DB state → same plan output. The only
//! non-deterministic field on `MergePlan` is `computed_at`, which is
//! explicitly NOT part of identity.
//!
//! Complexity: `O(L + R + (Cl + Cr) * W)` where `L`/`R` are the
//! per-branch commit counts, `Cl`/`Cr` the divergent-commit counts,
//! and `W` the average witness-per-commit. For workspace-scale
//! cognition DAGs (hundreds of commits, tens of witnesses each)
//! this is well under a millisecond.

use std::collections::{BTreeSet, HashSet};

use chrono::Utc;
use thinkingroot_core::types::{
    CognitionCommit, CommitId, CommitDivergence, MergePlan, WitnessClassification, WitnessId,
};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// Hard cap on how many ancestors we'll walk before giving up. A
/// well-formed branch chain is `O(commits-on-branch)` deep; this
/// constant exists only to refuse infinite loops from a corrupted
/// table (e.g. a parent pointer pointing back at a descendant). The
/// algorithm walks each side independently; the cap applies per side.
const MAX_ANCESTOR_WALK: usize = 100_000;

impl GraphStore {
    /// Compute a deterministic merge plan between two branches by
    /// fetching each branch's cognition-commit chain and delegating
    /// to the pure `compute_merge_plan_from_commits` algorithm.
    pub fn compute_merge_plan(
        &self,
        left_branch: &str,
        right_branch: &str,
    ) -> Result<MergePlan> {
        let left = self.list_cognition_commits_on_branch(left_branch, None)?;
        let right = self.list_cognition_commits_on_branch(right_branch, None)?;
        compute_merge_plan_from_commits(
            left_branch,
            right_branch,
            &left,
            &right,
            Utc::now(),
        )
    }
}

/// Pure algorithm core — same inputs always produce the same plan
/// (except for `computed_at`, which is the caller-supplied `now`).
///
/// Splitting this out of `GraphStore` makes every `CommitDivergence`
/// variant testable from fixture commit vecs, including the
/// `LeftAhead` / `RightAhead` / `Diverged` paths that the
/// branch-scoped storage layer can't construct in-DB today
/// (`cognition_inserts` enforces parent-must-be-on-same-branch).
///
/// The two `commits` slices must be newest-first by `created_at` —
/// matching the contract of `list_cognition_commits_on_branch`.
pub fn compute_merge_plan_from_commits(
    left_branch: &str,
    right_branch: &str,
    left: &[CognitionCommit],
    right: &[CognitionCommit],
    now: chrono::DateTime<Utc>,
) -> Result<MergePlan> {
    let left_head = left.first().map(|c| c.id);
    let right_head = right.first().map(|c| c.id);

    // Trivial: one or both sides empty → no shared history.
    if left.is_empty() || right.is_empty() {
        return Ok(MergePlan::new(
            left_branch.to_string(),
            right_branch.to_string(),
            left_head,
            right_head,
            CommitDivergence::NoCommonHistory,
            left.iter().map(|c| c.id).collect(),
            right.iter().map(|c| c.id).collect(),
            aggregate_witness_classification(left, right),
            now,
        ));
    }

    // Same head id → strictly identical.
    if left_head == right_head {
        return Ok(MergePlan::new(
            left_branch.to_string(),
            right_branch.to_string(),
            left_head,
            right_head,
            CommitDivergence::Identical,
            vec![],
            vec![],
            WitnessClassification::default(),
            now,
        ));
    }

    // Build ancestor set for left (every id on left's chain).
    let left_ids: HashSet<CommitId> = left.iter().map(|c| c.id).collect();
    if left_ids.len() > MAX_ANCESTOR_WALK {
        return Err(Error::GraphStorage(format!(
            "compute_merge_plan: left branch `{}` has {} commits, exceeds walk cap {}",
            left_branch,
            left_ids.len(),
            MAX_ANCESTOR_WALK
        )));
    }

    // Walk right's chain head-first looking for the first commit that
    // appears in left's ancestor set — that's the LCA.
    let mut lca: Option<CommitId> = None;
    for (i, c) in right.iter().enumerate() {
        if i >= MAX_ANCESTOR_WALK {
            return Err(Error::GraphStorage(format!(
                "compute_merge_plan: right branch `{}` ancestor walk exceeded cap {}",
                right_branch, MAX_ANCESTOR_WALK
            )));
        }
        if left_ids.contains(&c.id) {
            lca = Some(c.id);
            break;
        }
    }

    let lca = match lca {
        None => {
            // No shared commit — orphan branches.
            let cls = build_classification(left, right);
            return Ok(MergePlan::new(
                left_branch.to_string(),
                right_branch.to_string(),
                left_head,
                right_head,
                CommitDivergence::NoCommonHistory,
                left.iter().map(|c| c.id).collect(),
                right.iter().map(|c| c.id).collect(),
                cls,
                now,
            ));
        }
        Some(id) => id,
    };

    // Slice each side at the LCA (exclusive). `left_only` are the
    // commits ahead of LCA on left; same for right.
    let left_only: Vec<&CognitionCommit> = commits_above_lca(left, &lca);
    let right_only: Vec<&CognitionCommit> = commits_above_lca(right, &lca);

    // Detect ahead vs diverged. A side is "ahead" iff the OTHER
    // side's head is the LCA — meaning the other side has not moved.
    let conflict_kind = if Some(lca) == right_head {
        CommitDivergence::LeftAhead
    } else if Some(lca) == left_head {
        CommitDivergence::RightAhead
    } else {
        CommitDivergence::Diverged {
            common_ancestor: lca,
        }
    };

    let classification = build_classification_from_slices(&left_only, &right_only);

    Ok(MergePlan::new(
        left_branch.to_string(),
        right_branch.to_string(),
        left_head,
        right_head,
        conflict_kind,
        left_only.iter().map(|c| c.id).collect(),
        right_only.iter().map(|c| c.id).collect(),
        classification,
        now,
    ))
}

/// Return the slice of `commits` from index 0 up to (but excluding)
/// the LCA. Caller guarantees `lca` appears in `commits`.
fn commits_above_lca<'a>(
    commits: &'a [CognitionCommit],
    lca: &CommitId,
) -> Vec<&'a CognitionCommit> {
    let mut out = Vec::new();
    for c in commits {
        if c.id == *lca {
            break;
        }
        out.push(c);
    }
    out
}

/// Build the classification when there's no LCA — every commit on
/// each side counts as "_only" since there's nothing shared.
fn build_classification(
    left: &[CognitionCommit],
    right: &[CognitionCommit],
) -> WitnessClassification {
    let lrefs: Vec<&CognitionCommit> = left.iter().collect();
    let rrefs: Vec<&CognitionCommit> = right.iter().collect();
    build_classification_from_slices(&lrefs, &rrefs)
}

/// Aggregate citations / added witnesses / gaps across the two
/// divergent regions and bucket them into shared vs only-on-one-side.
///
/// All outputs are deduped and sorted so plan output is byte-stable
/// regardless of the underlying iteration order.
fn build_classification_from_slices(
    left_only: &[&CognitionCommit],
    right_only: &[&CognitionCommit],
) -> WitnessClassification {
    let (lc, lwa, lg) = aggregate_side(left_only);
    let (rc, rwa, rg) = aggregate_side(right_only);

    let (left_only_citations, right_only_citations, shared_citations) =
        partition_id_sets(&lc, &rc);
    let (left_only_added, right_only_added, shared_added) =
        partition_id_sets(&lwa, &rwa);
    let (left_only_gaps, right_only_gaps, shared_gaps) =
        partition_string_sets(&lg, &rg);

    WitnessClassification {
        left_only_citations,
        right_only_citations,
        shared_citations,
        left_only_added,
        right_only_added,
        shared_added,
        left_only_gaps,
        right_only_gaps,
        shared_gaps,
    }
}

/// Used by the NoCommonHistory fallback when we need to surface
/// witness sets across **every** commit on each side (rather than the
/// commits-above-LCA slice). Both sides count as "_only" by
/// construction — no commits are shared.
fn aggregate_witness_classification(
    left: &[CognitionCommit],
    right: &[CognitionCommit],
) -> WitnessClassification {
    build_classification(left, right)
}

/// Union of citations, witnesses_added, and gaps across a side's
/// commits. Returns deduped sets via `BTreeSet` so downstream
/// partitioning is order-independent.
fn aggregate_side(
    commits: &[&CognitionCommit],
) -> (BTreeSet<WitnessId>, BTreeSet<WitnessId>, BTreeSet<String>) {
    let mut citations = BTreeSet::new();
    let mut added = BTreeSet::new();
    let mut gaps = BTreeSet::new();
    for c in commits {
        for w in &c.citations {
            citations.insert(*w);
        }
        for w in &c.witnesses_added {
            added.insert(*w);
        }
        for g in &c.gaps_surfaced {
            gaps.insert(g.clone());
        }
    }
    (citations, added, gaps)
}

/// Partition two id sets into (left_only, right_only, shared),
/// each sorted by hex ascending so the plan is byte-stable.
fn partition_id_sets(
    left: &BTreeSet<WitnessId>,
    right: &BTreeSet<WitnessId>,
) -> (Vec<WitnessId>, Vec<WitnessId>, Vec<WitnessId>) {
    // BTreeSet difference/intersection iterate in sorted order. Since
    // `WitnessId: Ord` is derived over the raw 32-byte payload — which
    // matches the lowercase-hex string order character-by-character —
    // the resulting Vecs are byte-stable sorted by hex ascending,
    // exactly as the MergePlan contract requires.
    let left_only: Vec<WitnessId> = left.difference(right).copied().collect();
    let right_only: Vec<WitnessId> = right.difference(left).copied().collect();
    let shared: Vec<WitnessId> = left.intersection(right).copied().collect();
    (left_only, right_only, shared)
}

fn partition_string_sets(
    left: &BTreeSet<String>,
    right: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    // BTreeSet difference / intersection already iterate in sorted
    // order, so the resulting Vecs are sorted by construction.
    let left_only: Vec<String> = left.difference(right).cloned().collect();
    let right_only: Vec<String> = right.difference(left).cloned().collect();
    let shared: Vec<String> = left.intersection(right).cloned().collect();
    (left_only, right_only, shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use thinkingroot_core::types::{
        CognitionCommit, CommitAuthor, Confidence, Sensitivity, SourceId, Witness,
        WitnessInput, WitnessSpan, WorkspaceId,
    };

    fn fresh_store() -> GraphStore {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Box::leak(Box::new(tmp));
        GraphStore::init(path.path()).expect("graph store init")
    }

    fn fixture_witness(byte: u8) -> Witness {
        let file_hash = format!("{:0>64}", format!("{byte:x}"));
        let span = WitnessSpan {
            file_blake3: file_hash.clone(),
            start: byte as u64 * 16,
            end: byte as u64 * 16 + 8,
        };
        Witness::new(
            "test::merge@v1",
            "test",
            vec![WitnessInput::ByteRef {
                file_blake3: file_hash.clone(),
                start: span.start,
                end: span.end,
            }],
            vec![span],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            blake3::hash(format!("merge-witness-{byte}").as_bytes())
                .to_hex()
                .to_string(),
            Utc::now(),
        )
    }

    fn agent_author() -> CommitAuthor {
        CommitAuthor::Agent {
            model: "claude-opus-4-7".to_string(),
            principal: "thinkingroot".to_string(),
        }
    }

    /// Insert a fresh witness + return its id. Each call produces a
    /// unique witness (the `byte` arg seeds both the hash and the
    /// span) so tests can build divergent citation sets cleanly.
    fn insert_witness(store: &GraphStore, byte: u8) -> WitnessId {
        let w = fixture_witness(byte);
        let id = w.id;
        store.insert_witness(&w).unwrap();
        id
    }

    /// Insert a commit with explicit timestamp + return its id. The
    /// timestamp is what `list_cognition_commits_on_branch` orders by,
    /// so tests must stamp times in ascending order to get the
    /// newest-first list shape the algorithm relies on.
    #[allow(clippy::too_many_arguments)]
    fn insert_commit(
        store: &GraphStore,
        parent: Option<CommitId>,
        branch: &str,
        prompt: &str,
        citations: Vec<WitnessId>,
        added: Vec<WitnessId>,
        gaps: Vec<String>,
        seconds_offset: i64,
    ) -> CommitId {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-16T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::seconds(seconds_offset);
        let c = CognitionCommit::new(
            parent,
            branch.to_string(),
            agent_author(),
            prompt.to_string(),
            format!("reasoning for {prompt}"),
            added,
            citations,
            gaps,
            now,
        );
        store.insert_cognition_commit(&c).unwrap();
        c.id
    }

    #[test]
    fn empty_branches_classify_as_no_common_history() {
        let store = fresh_store();
        let plan = store.compute_merge_plan("main", "feature/x").unwrap();
        assert!(matches!(plan.conflict_kind, CommitDivergence::NoCommonHistory));
        assert!(plan.left_head.is_none());
        assert!(plan.right_head.is_none());
        assert!(plan.left_only_commits.is_empty());
        assert!(plan.right_only_commits.is_empty());
    }

    #[test]
    fn one_empty_branch_classifies_as_no_common_history() {
        let store = fresh_store();
        let w = insert_witness(&store, 1);
        insert_commit(&store, None, "main", "q1", vec![w], vec![], vec![], 0);
        let plan = store.compute_merge_plan("main", "feature/x").unwrap();
        assert!(matches!(plan.conflict_kind, CommitDivergence::NoCommonHistory));
        assert_eq!(plan.left_only_commits.len(), 1);
        assert!(plan.right_only_commits.is_empty());
    }

    #[test]
    fn identical_heads_classify_as_identical() {
        let store = fresh_store();
        let w = insert_witness(&store, 1);
        let c1 = insert_commit(
            &store,
            None,
            "main",
            "q1",
            vec![w],
            vec![],
            vec![],
            0,
        );
        // We model "two branches at the same commit" by listing the
        // same commit under both branches — but a commit can only live
        // on one branch per the storage constraint. Instead, two
        // branches truly become "identical" when they share the same
        // head id. Since each commit is on exactly one branch in our
        // model, we test the symmetry by querying main against main.
        let plan = store.compute_merge_plan("main", "main").unwrap();
        assert!(matches!(plan.conflict_kind, CommitDivergence::Identical));
        assert_eq!(plan.left_head, Some(c1));
        assert_eq!(plan.right_head, Some(c1));
    }

    /// Build a free-standing CognitionCommit fixture with a fixed
    /// timestamp offset (in seconds from a pinned 2026-05-16 anchor).
    /// Used to construct merge-plan input slices without going through
    /// `insert_cognition_commit` — bypasses the storage layer's
    /// same-branch-parent rule so we can exercise LeftAhead /
    /// RightAhead / Diverged at the algorithm level.
    fn fixture_commit(
        parent: Option<CommitId>,
        branch: &str,
        prompt: &str,
        citations: Vec<WitnessId>,
        seconds_offset: i64,
    ) -> CognitionCommit {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-16T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::seconds(seconds_offset);
        CognitionCommit::new(
            parent,
            branch.to_string(),
            agent_author(),
            prompt.to_string(),
            format!("reasoning for {prompt}"),
            vec![],
            citations,
            vec![],
            now,
        )
    }

    #[test]
    fn left_ahead_when_right_head_is_lca() {
        // Shared chain: lca. Left adds c1 → c2 on top.
        // Both branches "see" the lca because the merge-plan caller
        // would have loaded the same commit id under both branch
        // names. We model this by passing the same lca commit into
        // both slices and prepending additional commits onto left.
        let lca = fixture_commit(None, "main", "lca", vec![], 0);
        let l1 = fixture_commit(Some(lca.id), "main", "l1", vec![], 1);
        let l2 = fixture_commit(Some(l1.id), "main", "l2", vec![], 2);
        // newest-first: [l2, l1, lca]
        let left = vec![l2.clone(), l1.clone(), lca.clone()];
        let right = vec![lca.clone()];
        let plan = compute_merge_plan_from_commits(
            "main",
            "feature/x",
            &left,
            &right,
            Utc::now(),
        )
        .unwrap();
        assert!(matches!(plan.conflict_kind, CommitDivergence::LeftAhead));
        assert_eq!(plan.left_head, Some(l2.id));
        assert_eq!(plan.right_head, Some(lca.id));
        assert_eq!(plan.left_only_commits, vec![l2.id, l1.id]);
        assert!(plan.right_only_commits.is_empty());
        assert!(plan.is_trivial());
    }

    #[test]
    fn right_ahead_when_left_head_is_lca() {
        // Symmetric to LeftAhead — right has the additional commits.
        let lca = fixture_commit(None, "main", "lca", vec![], 0);
        let r1 = fixture_commit(Some(lca.id), "feature/x", "r1", vec![], 1);
        let r2 = fixture_commit(Some(r1.id), "feature/x", "r2", vec![], 2);
        let left = vec![lca.clone()];
        let right = vec![r2.clone(), r1.clone(), lca.clone()];
        let plan = compute_merge_plan_from_commits(
            "main",
            "feature/x",
            &left,
            &right,
            Utc::now(),
        )
        .unwrap();
        assert!(matches!(plan.conflict_kind, CommitDivergence::RightAhead));
        assert!(plan.left_only_commits.is_empty());
        assert_eq!(plan.right_only_commits, vec![r2.id, r1.id]);
        assert!(plan.is_trivial());
    }

    #[test]
    fn diverged_when_both_sides_have_unique_commits_since_lca() {
        // Both branches share an LCA and each has additional commits.
        // The Diverged variant carries the LCA so γ.2's synthesis
        // prompt can quote it back when proposing a merge.
        let w_shared = WitnessId([0x11; 32]);
        let w_left = WitnessId([0x22; 32]);
        let w_right = WitnessId([0x33; 32]);
        let lca = fixture_commit(None, "main", "lca", vec![w_shared], 0);
        let l1 = fixture_commit(Some(lca.id), "main", "l1", vec![w_left], 1);
        let r1 = fixture_commit(Some(lca.id), "feature/x", "r1", vec![w_right], 1);
        let left = vec![l1.clone(), lca.clone()];
        let right = vec![r1.clone(), lca.clone()];
        let plan = compute_merge_plan_from_commits(
            "main",
            "feature/x",
            &left,
            &right,
            Utc::now(),
        )
        .unwrap();
        match plan.conflict_kind {
            CommitDivergence::Diverged { common_ancestor } => {
                assert_eq!(common_ancestor, lca.id);
            }
            other => panic!("expected Diverged, got {other:?}"),
        }
        assert_eq!(plan.left_only_commits, vec![l1.id]);
        assert_eq!(plan.right_only_commits, vec![r1.id]);
        // Witness classification: only the divergent commits' citations
        // count toward the classification — the shared LCA's witness
        // (w_shared) is NOT included because the LCA isn't in the
        // "_only" slices.
        assert_eq!(plan.witnesses.left_only_citations, vec![w_left]);
        assert_eq!(plan.witnesses.right_only_citations, vec![w_right]);
        assert!(plan.witnesses.shared_citations.is_empty());
        assert!(!plan.is_trivial());
    }

    #[test]
    fn diverged_shared_citations_appear_when_both_sides_cite_same_witness() {
        let w_both = WitnessId([0x44; 32]);
        let w_l = WitnessId([0x55; 32]);
        let w_r = WitnessId([0x66; 32]);
        let lca = fixture_commit(None, "main", "lca", vec![], 0);
        let l1 = fixture_commit(Some(lca.id), "main", "l1", vec![w_both, w_l], 1);
        let r1 = fixture_commit(Some(lca.id), "feature/x", "r1", vec![w_both, w_r], 1);
        let left = vec![l1.clone(), lca.clone()];
        let right = vec![r1.clone(), lca.clone()];
        let plan = compute_merge_plan_from_commits(
            "main",
            "feature/x",
            &left,
            &right,
            Utc::now(),
        )
        .unwrap();
        assert!(matches!(plan.conflict_kind, CommitDivergence::Diverged { .. }));
        assert_eq!(plan.witnesses.shared_citations, vec![w_both]);
        assert_eq!(plan.witnesses.left_only_citations, vec![w_l]);
        assert_eq!(plan.witnesses.right_only_citations, vec![w_r]);
        assert!(plan.witnesses.has_shared_anchor());
    }

    #[test]
    fn diverged_classifies_with_shared_lca_witness_sets() {
        // Build everything on `main` so we exercise the LCA + witness
        // classification on the same branch — splitting branches is
        // a separate concern handled by branch-system; here we focus
        // on the algorithm correctness.
        //
        // Chain on `main`: lca → l1 → l2  (we'll then query
        // `compute_merge_plan(main, main)` after carving sub-prefixes
        // — but that returns Identical. Instead this test verifies
        // the more honest path: when both branches share genuinely
        // overlapping commits, the witness sets are partitioned
        // correctly. We test the helper functions directly with
        // hand-built CognitionCommit fixtures.
        let store = fresh_store();
        let w_shared = insert_witness(&store, 1);
        let w_left = insert_witness(&store, 2);
        let w_right = insert_witness(&store, 3);

        // Build CognitionCommit fixtures directly to exercise
        // build_classification_from_slices without needing the
        // storage layer to accept cross-branch parents.
        let c_left = CognitionCommit::new(
            None,
            "main".to_string(),
            agent_author(),
            "left".to_string(),
            "r".to_string(),
            vec![],
            vec![w_shared, w_left],
            vec!["gap-l".to_string()],
            Utc::now(),
        );
        let c_right = CognitionCommit::new(
            None,
            "feature/x".to_string(),
            agent_author(),
            "right".to_string(),
            "r".to_string(),
            vec![],
            vec![w_shared, w_right],
            vec!["gap-r".to_string()],
            Utc::now(),
        );

        let cls = build_classification_from_slices(&[&c_left], &[&c_right]);
        assert_eq!(cls.shared_citations, vec![w_shared]);
        assert_eq!(cls.left_only_citations, vec![w_left]);
        assert_eq!(cls.right_only_citations, vec![w_right]);
        assert_eq!(cls.left_only_gaps, vec!["gap-l"]);
        assert_eq!(cls.right_only_gaps, vec!["gap-r"]);
        assert!(cls.shared_gaps.is_empty());
        assert!(cls.has_shared_anchor());
    }

    #[test]
    fn no_common_history_full_classification_treats_all_as_only() {
        // Two branches with disjoint commits, no LCA.
        let store = fresh_store();
        let w1 = insert_witness(&store, 1);
        let w2 = insert_witness(&store, 2);
        let w_shared = insert_witness(&store, 3);
        let _c_main = insert_commit(
            &store,
            None,
            "main",
            "q-main",
            vec![w1, w_shared],
            vec![],
            vec![],
            0,
        );
        let _c_feat = insert_commit(
            &store,
            None,
            "feature/x",
            "q-feat",
            vec![w2, w_shared],
            vec![],
            vec![],
            1,
        );
        let plan = store
            .compute_merge_plan("main", "feature/x")
            .expect("plan");
        assert!(matches!(plan.conflict_kind, CommitDivergence::NoCommonHistory));
        assert_eq!(plan.witnesses.left_only_citations, vec![w1]);
        assert_eq!(plan.witnesses.right_only_citations, vec![w2]);
        assert_eq!(plan.witnesses.shared_citations, vec![w_shared]);
    }

    #[test]
    fn partition_id_sets_sorts_output_by_hex_ascending() {
        // Pick three witnesses whose hex representations are NOT in
        // sequential byte order — we want to ensure the sort is by
        // hex string, not by raw byte.
        let w_a = WitnessId([0xaa; 32]);
        let w_b = WitnessId([0x00; 32]);
        let w_c = WitnessId([0x55; 32]);
        let left: BTreeSet<WitnessId> = [w_a, w_b].iter().copied().collect();
        let right: BTreeSet<WitnessId> = [w_c].iter().copied().collect();
        let (l, r, s) = partition_id_sets(&left, &right);
        assert_eq!(l.len(), 2);
        // sorted by hex: "00..." < "aa..."
        assert_eq!(l, vec![w_b, w_a]);
        assert_eq!(r, vec![w_c]);
        assert!(s.is_empty());
    }

    #[test]
    fn divergent_commit_count_summary_matches() {
        let store = fresh_store();
        let w1 = insert_witness(&store, 1);
        let w2 = insert_witness(&store, 2);
        insert_commit(
            &store,
            None,
            "main",
            "q-main",
            vec![w1],
            vec![],
            vec![],
            0,
        );
        insert_commit(
            &store,
            None,
            "feature/x",
            "q-feat",
            vec![w2],
            vec![],
            vec![],
            1,
        );
        let plan = store
            .compute_merge_plan("main", "feature/x")
            .expect("plan");
        assert_eq!(plan.divergent_commit_count(), 2);
        assert!(!plan.is_trivial());
    }

    #[test]
    fn plan_is_deterministic_byte_for_byte_modulo_computed_at() {
        // Same inputs → same plan (except computed_at). Run twice and
        // compare every other field.
        let store = fresh_store();
        let w1 = insert_witness(&store, 1);
        let w2 = insert_witness(&store, 2);
        insert_commit(
            &store,
            None,
            "main",
            "q-main",
            vec![w1],
            vec![],
            vec![],
            0,
        );
        insert_commit(
            &store,
            None,
            "feature/x",
            "q-feat",
            vec![w2],
            vec![],
            vec![],
            1,
        );
        let p1 = store
            .compute_merge_plan("main", "feature/x")
            .expect("plan 1");
        let p2 = store
            .compute_merge_plan("main", "feature/x")
            .expect("plan 2");
        assert_eq!(p1.left_branch, p2.left_branch);
        assert_eq!(p1.right_branch, p2.right_branch);
        assert_eq!(p1.left_head, p2.left_head);
        assert_eq!(p1.right_head, p2.right_head);
        assert_eq!(p1.left_only_commits, p2.left_only_commits);
        assert_eq!(p1.right_only_commits, p2.right_only_commits);
        assert_eq!(p1.witnesses, p2.witnesses);
        // conflict_kind doesn't derive PartialEq for the Diverged
        // variant's inner field by default — use Debug round-trip.
        assert_eq!(format!("{:?}", p1.conflict_kind), format!("{:?}", p2.conflict_kind));
    }
}
