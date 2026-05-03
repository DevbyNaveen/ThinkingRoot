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
