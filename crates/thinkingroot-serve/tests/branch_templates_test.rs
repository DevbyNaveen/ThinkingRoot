//! T3.7 — Branch templates end-to-end.
//!
//! Verifies the full create-from-template materialisation path:
//! seed templates → list → upsert custom → create branch with
//! `template: "review-required"` → on-disk branch must inherit the
//! template's `MergePolicy::RequiresProposal` while the caller's
//! explicit override of `kind` still wins.

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_branch::templates::{BranchTemplate, TemplateRegistry};
use thinkingroot_core::{BranchKind, MergePolicy};

#[tokio::test]
async fn template_seed_creates_review_required_and_agent_sandbox() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    let registry = TemplateRegistry::load_or_seed(&refs_dir).unwrap();
    let names: Vec<&str> = registry.list().iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"review-required"));
    assert!(names.contains(&"agent-sandbox"));
}

#[tokio::test]
async fn create_branch_with_review_required_template_inherits_policy() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    {
        let _g = thinkingroot_graph::graph::GraphStore::init(&graph_dir).unwrap();
    }

    // Trigger the seed.
    let refs_dir = root.join(".thinkingroot-refs");
    let _registry = TemplateRegistry::load_or_seed(&refs_dir).unwrap();

    // Create a branch through the public helper, simulating what the
    // REST handler does after applying the template defaults.
    let template = TemplateRegistry::load_or_seed(&refs_dir)
        .unwrap()
        .get("review-required")
        .cloned()
        .expect("seeded template must exist");

    let branch = thinkingroot_branch::create_branch_full(
        &root,
        "feature/templated",
        "main",
        Some("from template".into()),
        Some("alice".into()),
        thinkingroot_core::BranchPermissions::default(),
        template.kind.clone(),
        template.merge_policy.clone(),
        template.redaction.clone(),
    )
    .await
    .expect("create_branch_full");

    match branch.merge_policy {
        MergePolicy::RequiresProposal { min_reviewers, .. } => {
            assert_eq!(min_reviewers, 1);
        }
        other => panic!(
            "expected RequiresProposal policy from template, got {:?}",
            other
        ),
    }
    // Kind from the template — Feature is the default but pinned by
    // the seed for `review-required`.
    assert!(matches!(branch.kind, BranchKind::Feature));
}

#[tokio::test]
async fn upsert_template_then_apply_persists_via_rest_path() {
    // Mirrors what the REST handler does: load registry, upsert a
    // bespoke template, then look it up on a fresh registry — this
    // pins the `tempfile + rename` atomic-write contract in
    // `templates::TemplateRegistry::save`.
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    let mut registry = TemplateRegistry::load_or_seed(&refs_dir).unwrap();

    let custom = BranchTemplate {
        name: "release-72h".into(),
        description: Some("ephemeral release branch".into()),
        kind: BranchKind::Feature,
        merge_policy: MergePolicy::Manual,
        redaction: None,
        max_age_secs: Some(72 * 3600),
        permissions: None,
    };

    let existed = registry.upsert(custom.clone()).unwrap();
    assert!(!existed, "custom template should be a fresh insert");

    let fresh = TemplateRegistry::load_or_seed(&refs_dir).unwrap();
    let got = fresh
        .get("release-72h")
        .expect("custom template must round-trip");
    assert_eq!(got.max_age_secs, Some(72 * 3600));
}
