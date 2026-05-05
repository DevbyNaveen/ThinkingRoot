use std::path::Path;
use thinkingroot_branch::snapshot::{resolve_data_dir, slugify};

#[test]
fn slugify_feature_slash() {
    assert_eq!(slugify("feature/graphql"), "feature-graphql");
}

#[test]
fn slugify_spaces_and_caps() {
    assert_eq!(slugify("My Branch Name"), "my-branch-name");
}

#[test]
fn slugify_main_unchanged() {
    assert_eq!(slugify("main"), "main");
}

#[test]
fn resolve_data_dir_main_none() {
    let p = Path::new("/repo");
    assert_eq!(resolve_data_dir(p, None), p.join(".thinkingroot"));
}

#[test]
fn resolve_data_dir_main_explicit() {
    let p = Path::new("/repo");
    assert_eq!(resolve_data_dir(p, Some("main")), p.join(".thinkingroot"));
}

#[test]
fn resolve_data_dir_branch() {
    let p = Path::new("/repo");
    assert_eq!(
        resolve_data_dir(p, Some("feature/graphql")),
        p.join(".thinkingroot")
            .join("branches")
            .join("feature-graphql")
    );
}

use tempfile::tempdir;
use thinkingroot_branch::branch::{BranchRegistry, read_head, write_head};
use thinkingroot_core::BranchPermissions;

#[test]
fn registry_create_and_list() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    let mut reg = BranchRegistry::load_or_create(&refs_dir).unwrap();
    reg.create_branch("feature/x", "main", None).unwrap();

    let branches = reg.list_active();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "feature/x");
    assert_eq!(branches[0].slug, "feature-x");
    assert_eq!(branches[0].parent, "main");
}

#[test]
fn registry_duplicate_fails() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    let mut reg = BranchRegistry::load_or_create(&refs_dir).unwrap();
    reg.create_branch("feature/x", "main", None).unwrap();
    let result = reg.create_branch("feature/x", "main", None);
    assert!(result.is_err(), "duplicate branch should fail");
}

#[test]
fn registry_abandon_removes_from_active() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    let mut reg = BranchRegistry::load_or_create(&refs_dir).unwrap();
    reg.create_branch("feature/x", "main", None).unwrap();
    reg.abandon_branch("feature/x").unwrap();

    let branches = reg.list_active();
    assert_eq!(branches.len(), 0);
}

#[test]
fn registry_persists_across_loads() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    {
        let mut reg = BranchRegistry::load_or_create(&refs_dir).unwrap();
        reg.create_branch("feature/y", "main", Some("test desc".to_string()))
            .unwrap();
    }

    let reg2 = BranchRegistry::load_or_create(&refs_dir).unwrap();
    let branches = reg2.list_active();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "feature/y");
    assert_eq!(branches[0].description, Some("test desc".to_string()));
}

#[test]
fn registry_persists_owner_and_permissions() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    {
        let mut reg = BranchRegistry::load_or_create(&refs_dir).unwrap();
        reg.create_branch_with_owner(
            "feature/secure",
            "main",
            None,
            Some("alice".to_string()),
            BranchPermissions {
                readers: vec!["reader".to_string()],
                writers: vec!["writer".to_string()],
                mergers: vec!["merger".to_string()],
            },
        )
        .unwrap();
    }

    let reg2 = BranchRegistry::load_or_create(&refs_dir).unwrap();
    let branch = reg2.get("feature/secure").unwrap();
    assert_eq!(branch.owner.as_deref(), Some("alice"));
    assert_eq!(branch.permissions.writers, vec!["writer"]);
}

#[test]
fn head_roundtrip() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    write_head(&refs_dir, "feature/x").unwrap();
    assert_eq!(read_head(&refs_dir).unwrap(), "feature/x");
}

#[test]
fn head_defaults_to_main() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();
    // No HEAD file written yet
    assert_eq!(read_head(&refs_dir).unwrap(), "main");
}

use thinkingroot_branch::diff::semantic_hash;

#[test]
fn semantic_hash_normalises_whitespace_and_case() {
    let h1 = semantic_hash("AuthService  uses  JWT");
    let h2 = semantic_hash("authservice uses jwt");
    assert_eq!(
        h1, h2,
        "same fact with different spacing/casing should hash identically"
    );
}

#[test]
fn semantic_hash_different_facts_differ() {
    let h1 = semantic_hash("AuthService uses JWT");
    let h2 = semantic_hash("AuthService uses OAuth2");
    assert_ne!(h1, h2);
}

use thinkingroot_branch::{create_branch, list_branches, read_head_branch};

#[tokio::test]
async fn create_branch_creates_layout_and_registry() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Create minimal main .thinkingroot/graph/ dir with a fake db file
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();

    create_branch(root, "feature/test", "main", None)
        .await
        .unwrap();

    // Branch dir should exist with db copy
    assert!(
        root.join(".thinkingroot/branches/feature-test/graph/graph.db")
            .exists()
    );

    // Registry should have one active branch
    let branches = list_branches(root).unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "feature/test");
    assert_eq!(branches[0].parent, "main");
}

#[tokio::test]
async fn read_head_defaults_to_main() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let head = read_head_branch(root).unwrap();
    assert_eq!(head, "main");
}

// ─── T0.6: BranchKind + MergePolicy round-trip + persistence ──────────

use thinkingroot_branch::create_branch_full;
use thinkingroot_core::{BranchKind, MergePolicy};

#[tokio::test]
async fn create_branch_full_preserves_kind_and_policy() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();

    create_branch_full(
        root,
        "stream/sess-7",
        "main",
        Some("session branch".into()),
        Some("sess-7".into()),
        BranchPermissions::default(),
        BranchKind::Stream {
            session_id: "sess-7".into(),
        },
        MergePolicy::AutoOnSessionEnd,
        None,
    )
    .await
    .unwrap();

    let branches = list_branches(root).unwrap();
    assert_eq!(branches.len(), 1);
    let b = &branches[0];
    assert!(matches!(b.kind, BranchKind::Stream { ref session_id } if session_id == "sess-7"));
    assert_eq!(b.merge_policy, MergePolicy::AutoOnSessionEnd);

    // Reload from disk: TOML must round-trip the typed fields.
    let refs = root.join(".thinkingroot-refs");
    let registry = BranchRegistry::load_or_create(&refs).unwrap();
    let reloaded = registry.get("stream/sess-7").unwrap();
    assert!(matches!(reloaded.kind, BranchKind::Stream { .. }));
    assert_eq!(reloaded.merge_policy, MergePolicy::AutoOnSessionEnd);
}

#[tokio::test]
async fn ephemeral_merge_short_circuits_to_abandon() {
    use thinkingroot_branch::merge::execute_merge_into;
    use thinkingroot_core::error::Error;
    use thinkingroot_core::{
        AutoResolution, ContradictionPair, HealthScore, KnowledgeDiff, MergedBy,
    };

    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();

    create_branch_full(
        root,
        "sandbox/ephemeral",
        "main",
        None,
        None,
        BranchPermissions::default(),
        BranchKind::Sandbox {
            agent_id: "claude".into(),
        },
        MergePolicy::Ephemeral,
        None,
    )
    .await
    .unwrap();

    // Synthetic empty diff that *would* normally be allowed.
    let diff = KnowledgeDiff {
        from_branch: "sandbox/ephemeral".into(),
        to_branch: "main".into(),
        computed_at: chrono::Utc::now(),
        new_claims: vec![],
        new_entities: vec![],
        new_relations: vec![],
        auto_resolved: Vec::<AutoResolution>::new(),
        needs_review: Vec::<ContradictionPair>::new(),
        health_before: HealthScore::default(),
        health_after: HealthScore::default(),
        merge_allowed: true,
        blocking_reasons: vec![],
    };

    let result = execute_merge_into(
        root,
        "sandbox/ephemeral",
        None,
        &diff,
        MergedBy::System,
        false,
    )
    .await;

    match result {
        Err(Error::MergeBlocked(msg)) => {
            assert!(
                msg.contains("Ephemeral"),
                "expected ephemeral message, got: {msg}"
            );
        }
        other => panic!("expected MergeBlocked, got: {other:?}"),
    }

    // Branch should now be abandoned, not merged.
    let refs = root.join(".thinkingroot-refs");
    let registry = BranchRegistry::load_or_create(&refs).unwrap();
    let abandoned: Vec<_> = registry
        .list_abandoned()
        .into_iter()
        .map(|b| b.name.clone())
        .collect();
    assert!(
        abandoned.contains(&"sandbox/ephemeral".to_string()),
        "expected ephemeral branch in abandoned list, got: {abandoned:?}"
    );
}

#[tokio::test]
async fn requires_proposal_blocks_raw_merge() {
    use thinkingroot_branch::merge::execute_merge_into;
    use thinkingroot_core::error::Error;
    use thinkingroot_core::{
        AutoResolution, ContradictionPair, HealthScore, KnowledgeDiff, MergedBy,
    };

    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();

    create_branch_full(
        root,
        "feature/risky",
        "main",
        None,
        None,
        BranchPermissions::default(),
        BranchKind::Feature,
        MergePolicy::RequiresProposal {
            min_reviewers: 2,
            required_checks: vec!["health_score".into()],
        },
        None,
    )
    .await
    .unwrap();

    let diff = KnowledgeDiff {
        from_branch: "feature/risky".into(),
        to_branch: "main".into(),
        computed_at: chrono::Utc::now(),
        new_claims: vec![],
        new_entities: vec![],
        new_relations: vec![],
        auto_resolved: Vec::<AutoResolution>::new(),
        needs_review: Vec::<ContradictionPair>::new(),
        health_before: HealthScore::default(),
        health_after: HealthScore::default(),
        merge_allowed: true,
        blocking_reasons: vec![],
    };

    match execute_merge_into(root, "feature/risky", None, &diff, MergedBy::System, false).await {
        Err(Error::MergeBlocked(msg)) => {
            assert!(
                msg.contains("RequiresProposal") || msg.contains("Knowledge Proposal"),
                "expected proposal message, got: {msg}"
            );
        }
        other => panic!("expected MergeBlocked, got: {other:?}"),
    }
}

// T2.3 — TTL gate set + read round-trip.
#[tokio::test]
async fn set_branch_max_age_secs_round_trips() {
    use thinkingroot_branch::set_branch_max_age_secs;

    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();

    create_branch(root, "feature/short-lived", "main", None)
        .await
        .unwrap();

    // Set a 1-day TTL.
    let updated = set_branch_max_age_secs(root, "feature/short-lived", Some(86_400)).unwrap();
    assert_eq!(updated.max_age_secs, Some(86_400));

    // Reload from disk.
    let refs = root.join(".thinkingroot-refs");
    let reg = BranchRegistry::load_or_create(&refs).unwrap();
    assert_eq!(
        reg.get("feature/short-lived").unwrap().max_age_secs,
        Some(86_400)
    );

    // Clearing.
    set_branch_max_age_secs(root, "feature/short-lived", None).unwrap();
    let reg2 = BranchRegistry::load_or_create(&refs).unwrap();
    assert!(reg2.get("feature/short-lived").unwrap().max_age_secs.is_none());
}

// T2.5 — tag create + immutability.
#[tokio::test]
async fn create_tag_round_trips_and_is_listed() {
    use thinkingroot_branch::{create_tag, list_tags};
    use thinkingroot_core::BranchKind;

    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();

    let tag = create_tag(
        root,
        "v1.0.0",
        "refs/tags/v1.0.0",
        "deadbeefcafebabe",
        Some("alice".into()),
        Some("First stable release".into()),
    )
    .unwrap();
    assert_eq!(tag.name, "v1.0.0");
    assert!(matches!(tag.kind, BranchKind::Tag { .. }));

    let listed = list_tags(root).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "v1.0.0");

    // Re-creating fails — name conflict.
    let dup = create_tag(
        root,
        "v1.0.0",
        "refs/tags/v1.0.0",
        "anything",
        None,
        None,
    );
    assert!(dup.is_err(), "duplicate tag must error");
}

// T2.2 — protected-branches gate (configured opt-in).
//
// Negative-only: we verify the gate *fires* on a configured protected
// target.  Confirming that `force=true` lets a real merge proceed all
// the way through is covered by the `requires_proposal_merge_succeeds`
// test, which uses a real GraphStore-backed workspace.
#[tokio::test]
async fn protected_target_blocks_merge_without_force() {
    use thinkingroot_branch::merge::execute_merge_into;
    use thinkingroot_core::error::Error;
    use thinkingroot_core::{
        AutoResolution, ContradictionPair, HealthScore, KnowledgeDiff, MergedBy,
    };

    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();
    // Opt the workspace into "main is protected" via config.toml.
    std::fs::write(
        root.join(".thinkingroot/config.toml"),
        r#"
[merge]
protected_branches = ["main"]
"#,
    )
    .unwrap();

    create_branch_full(
        root,
        "feature/will-be-blocked",
        "main",
        None,
        None,
        BranchPermissions::default(),
        BranchKind::Feature,
        MergePolicy::Manual,
        None,
    )
    .await
    .unwrap();

    let diff = KnowledgeDiff {
        from_branch: "feature/will-be-blocked".into(),
        to_branch: "main".into(),
        computed_at: chrono::Utc::now(),
        new_claims: vec![],
        new_entities: vec![],
        new_relations: vec![],
        auto_resolved: Vec::<AutoResolution>::new(),
        needs_review: Vec::<ContradictionPair>::new(),
        health_before: HealthScore::default(),
        health_after: HealthScore::default(),
        merge_allowed: true,
        blocking_reasons: vec![],
    };

    // Without force the gate fires.
    match execute_merge_into(root, "feature/will-be-blocked", None, &diff, MergedBy::System, false)
        .await
    {
        Err(Error::MergeBlocked(msg)) => {
            assert!(
                msg.contains("protected"),
                "expected protected-branches message, got: {msg}"
            );
        }
        other => panic!("expected protected-branches MergeBlocked, got: {other:?}"),
    }

}

#[tokio::test]
async fn set_branch_redaction_persists() {
    use thinkingroot_branch::set_branch_redaction;
    use thinkingroot_core::{RedactionPolicy, Sensitivity};

    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"placeholder").unwrap();

    create_branch(root, "feature/with-policy", "main", None)
        .await
        .unwrap();

    let policy = RedactionPolicy {
        patterns: vec![r"\b\d{3}-\d{2}-\d{4}\b".into()],
        replacement: "[ssn]".into(),
        modes: vec![],
        min_sensitivity: Some(Sensitivity::Confidential),
        drop_above_min: true,
    };
    let updated = set_branch_redaction(root, "feature/with-policy", Some(policy.clone())).unwrap();
    assert_eq!(updated.redaction.as_ref(), Some(&policy));

    // Reload from disk.
    let refs = root.join(".thinkingroot-refs");
    let reg = BranchRegistry::load_or_create(&refs).unwrap();
    let reloaded = reg.get("feature/with-policy").unwrap();
    assert_eq!(reloaded.redaction.as_ref(), Some(&policy));

    // Clearing the policy persists too.
    set_branch_redaction(root, "feature/with-policy", None).unwrap();
    let reg2 = BranchRegistry::load_or_create(&refs).unwrap();
    assert!(reg2.get("feature/with-policy").unwrap().redaction.is_none());
}

// ─── T0.4: Knowledge Proposal authorises RequiresProposal merge ───
//
// Sister test to `requires_proposal_blocks_raw_merge` (which proves
// the negative path).  This one proves the positive path: when an
// approved Knowledge Proposal exists for the (source, target) pair,
// `execute_merge_into` lets the merge through and flips the proposal
// status to Merged.  Closes the production-blocking gap where the
// RequiresProposal gate previously had no lifecycle path forward.

#[tokio::test]
async fn requires_proposal_merge_succeeds_with_approved_proposal() {
    use thinkingroot_branch::merge::execute_merge_into;
    use thinkingroot_core::{
        AutoResolution, BranchKind, ContradictionPair, HealthScore, KnowledgeDiff, MergePolicy,
        MergedBy,
    };
    use thinkingroot_graph::graph::GraphStore;
    use thinkingroot_pr::{
        find_approved_proposal, list_proposals, mark_proposal_merged, open_proposal,
        review_proposal, ProposalStatus, ReviewDecision,
    };

    let dir = tempdir().unwrap();
    let root = dir.path();
    let main_graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&main_graph_dir).unwrap();
    {
        let _g = GraphStore::init(&main_graph_dir).expect("init main graph");
    }

    create_branch_full(
        root,
        "feature/governed",
        "main",
        None,
        None,
        BranchPermissions::default(),
        BranchKind::Feature,
        MergePolicy::RequiresProposal {
            min_reviewers: 2,
            required_checks: vec!["health_score".into()],
        },
        None,
    )
    .await
    .expect("create branch");

    let refs_dir = root.join(".thinkingroot-refs");

    let diff = KnowledgeDiff {
        from_branch: "feature/governed".into(),
        to_branch: "main".into(),
        computed_at: chrono::Utc::now(),
        new_claims: vec![],
        new_entities: vec![],
        new_relations: vec![],
        auto_resolved: Vec::<AutoResolution>::new(),
        needs_review: Vec::<ContradictionPair>::new(),
        health_before: HealthScore::default(),
        health_after: HealthScore::default(),
        merge_allowed: true,
        blocking_reasons: vec![],
    };

    // 1. Without any proposal, the gate must reject.
    let blocked = execute_merge_into(
        root,
        "feature/governed",
        None,
        &diff,
        MergedBy::System,
        false,
    )
    .await;
    assert!(
        matches!(blocked, Err(thinkingroot_core::error::Error::MergeBlocked(_))),
        "raw merge of RequiresProposal branch must be blocked when no approved proposal \
         exists, got: {blocked:?}"
    );

    // 2. Open + collect 2 distinct approves to satisfy min_reviewers.
    let proposal = open_proposal(
        &refs_dir,
        "feature/governed",
        None,
        "alice",
        Some("Adds governed change.".into()),
        2,
        vec!["health_score".into()],
    )
    .expect("open proposal");
    review_proposal(&refs_dir, &proposal.id, "bob", ReviewDecision::Approve, None)
        .expect("first approve");
    let approved = review_proposal(
        &refs_dir,
        &proposal.id,
        "carol",
        ReviewDecision::Approve,
        None,
    )
    .expect("second approve");
    assert!(
        matches!(approved.status, ProposalStatus::Approved),
        "two distinct approves must advance status to Approved, got {:?}",
        approved.status
    );

    // 3. find_approved_proposal must surface this proposal.
    let found = find_approved_proposal(&refs_dir, "feature/governed", None)
        .expect("find_approved_proposal")
        .expect("approved proposal exists");
    assert_eq!(found.id, proposal.id);

    // 4. With the approved proposal in place, the merge must succeed.
    execute_merge_into(
        root,
        "feature/governed",
        None,
        &diff,
        MergedBy::Human {
            user: "carol".into(),
        },
        false,
    )
    .await
    .expect("merge with approved proposal must succeed");

    // 5. Proposal status must now be Merged (the gate called
    //    mark_proposal_merged on success).
    let after_merge = list_proposals(&refs_dir).expect("list proposals");
    assert_eq!(after_merge.len(), 1);
    assert!(
        matches!(after_merge[0].status, ProposalStatus::Merged),
        "proposal status must flip to Merged after successful merge, got {:?}",
        after_merge[0].status
    );
    assert!(
        after_merge[0].merged_at.is_some(),
        "merged_at must be set on the proposal"
    );

    // 6. mark_proposal_merged is idempotent — calling again is a no-op.
    let again = mark_proposal_merged(&refs_dir, &proposal.id)
        .expect("mark_proposal_merged is idempotent");
    assert!(matches!(again.status, ProposalStatus::Merged));
}

// ─── A3: branch-registry write race ──────────────────────────────────
//
// Pre-fix, `BranchRegistry::create_branch_full` did load → check →
// push → save with no lock around the sequence.  Two concurrent
// callers (separate processes OR separate threads inside one
// process) could both load the same registry, both push their
// distinct new branch, and the second `save()` would atomically
// rename a file containing only its own branch over the first
// caller's write — silently losing the first branch.
//
// This test pins the new contract: 32 concurrent threads each
// creating a uniquely-named branch must all 32 land in the
// persisted registry.  Loop helps to catch flakiness — the lock has
// to hold under repeated attempts, not just one lucky run.

#[test]
fn create_branch_full_serialises_concurrent_writes() {
    use std::sync::Arc;
    use std::thread;
    use thinkingroot_core::{BranchKind, MergePolicy};

    const THREAD_COUNT: usize = 32;
    const ITERATIONS: usize = 5;

    for iteration in 0..ITERATIONS {
        let dir = tempdir().unwrap();
        let refs_dir = Arc::new(dir.path().join(".thinkingroot-refs"));
        std::fs::create_dir_all(refs_dir.as_path()).unwrap();

        let mut handles = Vec::with_capacity(THREAD_COUNT);
        for tid in 0..THREAD_COUNT {
            let refs_dir = Arc::clone(&refs_dir);
            handles.push(thread::spawn(move || {
                let mut reg = BranchRegistry::load_or_create(refs_dir.as_path()).unwrap();
                reg.create_branch_full(
                    &format!("feature/concurrent-{tid}"),
                    "main",
                    None,
                    None,
                    BranchPermissions::default(),
                    BranchKind::Feature,
                    MergePolicy::Manual,
                    None,
                )
                .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let reg = BranchRegistry::load_or_create(refs_dir.as_path()).unwrap();
        let active = reg.list_active();
        assert_eq!(
            active.len(),
            THREAD_COUNT,
            "iteration {iteration}: expected {THREAD_COUNT} branches after concurrent \
             create_branch_full, got {}: {:?}",
            active.len(),
            active.iter().map(|b| &b.name).collect::<Vec<_>>()
        );

        // Every distinct name must be present — last-writer-wins would
        // silently drop branches with no duplicate-name collision.
        let mut names: Vec<&str> = active.iter().map(|b| b.name.as_str()).collect();
        names.sort();
        for tid in 0..THREAD_COUNT {
            let expected = format!("feature/concurrent-{tid}");
            assert!(
                names.iter().any(|n| *n == expected),
                "iteration {iteration}: branch '{expected}' missing from final \
                 registry — concurrent write was silently lost. Got: {names:?}"
            );
        }
    }
}

// ─── T0.5: three-way merge surfaces real conflicts ───────────────────
//
// Two-way `compute_diff_into` cannot distinguish "added on theirs"
// from "removed from ours" — it only sees what's in each graph at
// merge time, not how each got there.  Three-way uses the LCA to
// classify true conflicts.  Pre-T0.5, two concurrent edits to the
// same claim id silently last-writer-won; this test pins the new
// contract: a `ModifyModify` conflict is surfaced and `merge_allowed`
// flips to false.

#[test]
fn compute_three_way_diff_surfaces_modify_modify_conflict() {
    use thinkingroot_branch::diff::compute_three_way_diff;
    use thinkingroot_core::{
        Claim, ClaimType, ConflictKind, ContentHash, Source, SourceType, TrustLevel,
        WorkspaceId,
    };
    use thinkingroot_graph::graph::GraphStore;

    let dir = tempdir().unwrap();
    let root = dir.path();
    let base_dir = root.join("base");
    let ours_dir = root.join("ours");
    let theirs_dir = root.join("theirs");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&ours_dir).unwrap();
    std::fs::create_dir_all(&theirs_dir).unwrap();

    let base = GraphStore::init(&base_dir).expect("init base graph");
    let ours = GraphStore::init(&ours_dir).expect("init ours graph");
    let theirs = GraphStore::init(&theirs_dir).expect("init theirs graph");

    let workspace = WorkspaceId::new();

    // Seed one shared source + one shared claim into all three so they
    // share an LCA on this claim id.  Same id, same statement → no
    // conflict yet.
    let source = Source::new("file:///auth.md".to_string(), SourceType::Document)
        .with_trust(TrustLevel::Trusted)
        .with_hash(ContentHash("hash-base".to_string()));
    let mut shared_claim = Claim::new(
        "AuthService uses JWT tokens",
        ClaimType::Fact,
        source.id,
        workspace,
    );
    // Pin the id so we can upsert it on each side.
    let shared_id = shared_claim.id;

    for g in [&base, &ours, &theirs] {
        g.insert_source(&source).expect("insert source");
        g.insert_claim(&shared_claim).expect("insert claim");
        // get_all_claims_with_sources joins on claim_source_edges, so
        // a claim without this junction is invisible to the diff —
        // mirror the merge_cache_reload_test setup pattern.
        g.link_claim_to_source(&shared_claim.id.to_string(), &source.id.to_string())
            .expect("link claim to source");
    }

    // Now diverge: `ours` modifies the same claim id to one statement;
    // `theirs` modifies it to a different statement.  Both differ
    // from base — exactly the silent-LWW class T0.5 fixes.
    shared_claim.statement = "AuthService uses OAuth2 authorization codes".to_string();
    shared_claim.id = shared_id; // keep id stable
    ours.insert_claim(&shared_claim)
        .expect("upsert claim in ours");

    shared_claim.statement = "AuthService uses session cookies".to_string();
    shared_claim.id = shared_id; // keep id stable
    theirs
        .insert_claim(&shared_claim)
        .expect("upsert claim in theirs");

    let diff = compute_three_way_diff(
        &base,
        &ours,
        &theirs,
        "feature/branch",
        Some("main"),
        0.5,  // auto_resolve_threshold
        0.25, // max_health_drop
        false, // block_on_contradictions
    )
    .expect("compute three-way diff");

    // The conflict must be in needs_review with conflict_kind set.
    let modify_modify: Vec<_> = diff
        .needs_review
        .iter()
        .filter(|c| matches!(c.conflict_kind, Some(ConflictKind::ModifyModify)))
        .collect();
    assert_eq!(
        modify_modify.len(),
        1,
        "expected exactly one ModifyModify conflict, got {} entries: {:?}",
        modify_modify.len(),
        diff.needs_review
            .iter()
            .map(|c| (&c.main_claim_id, &c.conflict_kind))
            .collect::<Vec<_>>()
    );
    let conflict = modify_modify[0];
    assert_eq!(
        conflict.main_claim_id,
        shared_id.to_string(),
        "conflict must reference the shared claim id"
    );

    // Three-way conflicts must block the merge.
    assert!(
        !diff.merge_allowed,
        "merge_allowed must flip to false when ModifyModify conflict exists"
    );
    assert!(
        diff.blocking_reasons
            .iter()
            .any(|r| r.contains("three-way conflict")),
        "blocking_reasons must explain the conflict; got: {:?}",
        diff.blocking_reasons
    );
}

// ─── A2: vector-store error promotion in merge ────────────────────────
//
// Pre-fix, `apply_branch_diff` swallowed `VectorStore::init` /
// `upsert_raw_batch` / `save` failures via `tracing::warn!("(non-fatal):
// {e}")` and continued on success.  A merge that completed with stale
// embeddings silently corrupted hybrid retrieval and AEP probes for the
// affected claim ids — exactly the silent-failure class CLAUDE.md
// honesty rule #1 forbids.
//
// This test pins the new contract: when target-side vector save fails
// during reconciliation, the merge returns `Error::VectorStorage` and
// the error message points the operator at `root branch rollback` so
// they can recover via the pre-merge snapshot.

#[tokio::test]
async fn merge_fails_loud_on_vector_save_error() {
    use thinkingroot_branch::merge::execute_merge;
    use thinkingroot_core::error::Error;
    use thinkingroot_core::{
        AutoResolution, BranchKind, ContradictionPair, HealthScore, KnowledgeDiff, MergePolicy,
        MergedBy,
    };
    use thinkingroot_graph::graph::GraphStore;
    use thinkingroot_graph::vector::VectorStore;

    let dir = tempdir().unwrap();
    let root = dir.path();
    let main_data = root.join(".thinkingroot");
    let main_graph_dir = main_data.join("graph");
    std::fs::create_dir_all(&main_graph_dir).unwrap();

    // Real (empty) main graph store so apply_branch_diff can open it.
    {
        let _g = GraphStore::init(&main_graph_dir).expect("init main graph");
    }

    create_branch_full(
        root,
        "feature/withvectors",
        "main",
        None,
        None,
        BranchPermissions::default(),
        BranchKind::Feature,
        MergePolicy::Manual,
        None,
    )
    .await
    .expect("create branch");

    // Seed the branch's vector store with one entry so the
    // `if source_data_dir.join("vectors.bin").exists()` gate at the top
    // of the reconciliation block fires and items.len() > 0 forces the
    // (poisoned) save() call below.
    let branch_data = root
        .join(".thinkingroot")
        .join("branches")
        .join("feature-withvectors");
    {
        let mut vec_store = VectorStore::init(&branch_data)
            .await
            .expect("init branch vector store");
        vec_store
            .upsert_raw_batch(vec![(
                "claim:test".into(),
                vec![0.1f32; 384],
                "{}".into(),
            )])
            .expect("seed branch vector");
        vec_store.save().expect("save branch vector");
    }
    assert!(
        branch_data.join("vectors.bin").exists(),
        "branch vectors.bin must exist for the reconciliation gate to fire"
    );

    // Poison the target's save path: pre-create vectors.bin.tmp as a
    // directory so VectorStore::save()'s atomic `write tmp + rename`
    // step fails on the write (cannot open a directory for write).
    // Pre-fix the merge would log a warn and return Ok.  Post-fix it
    // must return Err(Error::VectorStorage).
    let target_tmp_path = main_data.join("vectors.bin.tmp");
    std::fs::create_dir_all(&target_tmp_path).expect("poison target vectors.bin.tmp");

    // Empty diff with merge_allowed=true so the policy gate passes and
    // graph mutation steps are no-ops; the only work apply_branch_diff
    // performs is the (poisoned) vector reconciliation.
    let diff = KnowledgeDiff {
        from_branch: "feature/withvectors".into(),
        to_branch: "main".into(),
        computed_at: chrono::Utc::now(),
        new_claims: vec![],
        new_entities: vec![],
        new_relations: vec![],
        auto_resolved: Vec::<AutoResolution>::new(),
        needs_review: Vec::<ContradictionPair>::new(),
        health_before: HealthScore::default(),
        health_after: HealthScore::default(),
        merge_allowed: true,
        blocking_reasons: vec![],
    };

    let result = execute_merge(root, "feature/withvectors", &diff, MergedBy::System, false).await;

    match result {
        Err(Error::VectorStorage(msg)) => {
            assert!(
                msg.contains("rollback"),
                "VectorStorage error message must point operators at \
                 `root branch rollback` to restore the pre-merge snapshot, \
                 got: {msg}"
            );
            assert!(
                msg.contains("feature/withvectors"),
                "VectorStorage error message must name the source branch, \
                 got: {msg}"
            );
        }
        other => panic!(
            "expected Err(Error::VectorStorage(_)) when target vector save \
             fails — merge must fail loud, never silently corrupt the index. \
             Got: {other:?}"
        ),
    }
}

// ─── A2 × A5 end-to-end: failed merge leaves intent + recovers cleanly
//
// The strongest contract: a merge that fails mid-pipeline must leave
// the merges_in_flight.toml intent file in place AND a pre-merge
// snapshot on disk.  A subsequent `recover_orphan_merges` call must
// find both, restore the target's `graph.db` from the snapshot, and
// remove the intent — leaving the workspace in the same state as if
// the merge had never been attempted.
//
// Without this end-to-end coverage, the A2 (loud-fail) and A5
// (recovery) fixes would be tested in isolation, and a regression in
// either side could silently break the cross-cutting story.

#[tokio::test]
async fn failed_merge_leaves_intent_and_recovers() {
    use thinkingroot_branch::merge::execute_merge;
    use thinkingroot_branch::recovery::{recover_orphan_merges, INTENTS_FILE};
    use thinkingroot_core::error::Error;
    use thinkingroot_core::{
        AutoResolution, BranchKind, ContradictionPair, HealthScore, KnowledgeDiff, MergePolicy,
        MergedBy,
    };
    use thinkingroot_graph::graph::GraphStore;
    use thinkingroot_graph::vector::VectorStore;

    let dir = tempdir().unwrap();
    let root = dir.path();
    let main_data = root.join(".thinkingroot");
    let main_graph_dir = main_data.join("graph");
    std::fs::create_dir_all(&main_graph_dir).unwrap();

    // Initialize main and write recognisable bytes so we can verify
    // recovery actually restored the pre-merge snapshot.
    {
        let _g = GraphStore::init(&main_graph_dir).expect("init main graph");
    }
    let main_db = main_graph_dir.join("graph.db");
    let main_db_content_before = std::fs::read(&main_db).expect("read main graph.db");

    create_branch_full(
        root,
        "feature/recover",
        "main",
        None,
        None,
        BranchPermissions::default(),
        BranchKind::Feature,
        MergePolicy::Manual,
        None,
    )
    .await
    .expect("create branch");

    // Seed branch vectors so reconciliation runs.
    let branch_data = root
        .join(".thinkingroot")
        .join("branches")
        .join("feature-recover");
    {
        let mut vec_store = VectorStore::init(&branch_data).await.expect("init branch");
        vec_store
            .upsert_raw_batch(vec![("claim:r1".into(), vec![0.5f32; 384], "{}".into())])
            .expect("seed branch vector");
        vec_store.save().expect("save branch vector");
    }

    // Poison the target vector save path AFTER snapshot would be taken.
    std::fs::create_dir_all(main_data.join("vectors.bin.tmp"))
        .expect("poison target vectors.bin.tmp");

    let diff = KnowledgeDiff {
        from_branch: "feature/recover".into(),
        to_branch: "main".into(),
        computed_at: chrono::Utc::now(),
        new_claims: vec![],
        new_entities: vec![],
        new_relations: vec![],
        auto_resolved: Vec::<AutoResolution>::new(),
        needs_review: Vec::<ContradictionPair>::new(),
        health_before: HealthScore::default(),
        health_after: HealthScore::default(),
        merge_allowed: true,
        blocking_reasons: vec![],
    };

    // 1. Run merge — must fail with VectorStorage (A2 contract).
    let result = execute_merge(root, "feature/recover", &diff, MergedBy::System, false).await;
    assert!(
        matches!(result, Err(Error::VectorStorage(_))),
        "merge must fail loud on vector save error, got: {result:?}"
    );

    // 2. Intent must persist after failed merge.
    let intent_path = root.join(".thinkingroot-refs").join(INTENTS_FILE);
    assert!(
        intent_path.exists(),
        "merges_in_flight.toml must persist after failed merge so recovery \
         can roll back; expected file at {}",
        intent_path.display()
    );

    // 3. Pre-merge snapshot must exist on disk (taken before the poison
    //    triggered the failure).
    let snapshots: Vec<_> = std::fs::read_dir(&main_graph_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("graph.db.pre-merge-feature-recover-"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        snapshots.len(),
        1,
        "exactly one pre-merge snapshot expected, got {}: {:?}",
        snapshots.len(),
        snapshots.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );

    // 4. Clear the poison so recovery's `std::fs::copy` over graph.db
    //    can succeed (otherwise the test would re-fail at recovery).
    std::fs::remove_dir_all(main_data.join("vectors.bin.tmp")).ok();

    // 5. Recovery must roll back and clear the intent.
    let report = recover_orphan_merges(root).expect("recovery must succeed");
    assert_eq!(
        report.recovered.len(),
        1,
        "expected exactly one recovered merge, got: {:?}",
        report.recovered
    );
    assert_eq!(report.recovered[0].source_branch, "feature/recover");
    assert_eq!(report.orphaned_intents_cleared.len(), 0);

    // 6. Intent file must be gone.
    assert!(
        !intent_path.exists(),
        "intents file must be removed after successful recovery"
    );

    // 7. Live graph.db must match pre-merge content (idempotent: the
    //    apply_branch_diff in our test made no graph mutations because
    //    the diff was empty, so pre and post bytes match — but recovery
    //    still copied the snapshot back, exercising the contract).
    let main_db_content_after = std::fs::read(&main_db).expect("read main graph.db");
    assert_eq!(
        main_db_content_before, main_db_content_after,
        "after recovery, main graph.db must match its pre-merge bytes \
         (recovery uses the snapshot, not the corrupt mid-merge state)"
    );

    // 8. Idempotent re-run — recovery on a clean workspace is a no-op.
    let report2 = recover_orphan_merges(root).expect("recovery must be idempotent");
    assert_eq!(report2.recovered.len(), 0);
    assert_eq!(report2.orphaned_intents_cleared.len(), 0);
}
