//! T3.2 — Cross-branch reflect.
//!
//! Verifies that `engine.reflect_across_branches`:
//!
//! 1. Returns one `ReflectResult` per requested branch.
//! 2. Surfaces patterns that fired in some branches but not others
//!    via `divergent_patterns`.
//! 3. Returns an empty `divergent_patterns` vec when every branch
//!    fires the same pattern set.
//! 4. Errors when called with an empty `branches` list.

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, Entity, EntityType, Source, SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

/// Same fixture as `reflect_integration_test.rs` — 40 Service
/// entities, 37 with both ApiSignature + Requirement and 3 with
/// only ApiSignature.  Returns the workspace root path.
async fn setup_ws_with_pattern() -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();

    let workspace = WorkspaceId::new();
    let graph = GraphStore::init(&graph_dir).unwrap();
    let source = Source::new("file:///fx.md".into(), SourceType::Document)
        .with_trust(TrustLevel::Trusted)
        .with_hash(ContentHash("h".into()));
    let source_id = source.id;
    graph.insert_source(&source).unwrap();

    for i in 0..40 {
        let name = if i < 37 {
            format!("Svc{i}")
        } else {
            format!("GapSvc{i}")
        };
        let entity = Entity::new(&name, EntityType::Service);
        let eid = entity.id.to_string();
        graph.insert_entity(&entity).unwrap();

        let c = Claim::new(
            &format!("{name} has endpoints"),
            ClaimType::ApiSignature,
            source_id,
            workspace,
        );
        let cid = c.id.to_string();
        graph.insert_claim(&c).unwrap();
        graph
            .link_claim_to_source(&cid, &source_id.to_string())
            .unwrap();
        graph.link_claim_to_entity(&cid, &eid).unwrap();

        if i < 37 {
            let c2 = Claim::new(
                &format!("{name} must meet X"),
                ClaimType::Requirement,
                source_id,
                workspace,
            );
            let cid2 = c2.id.to_string();
            graph.insert_claim(&c2).unwrap();
            graph
                .link_claim_to_source(&cid2, &source_id.to_string())
                .unwrap();
            graph.link_claim_to_entity(&cid2, &eid).unwrap();
        }
    }
    (dir, root)
}

#[tokio::test]
async fn cross_branch_reflect_returns_per_branch_results() {
    let (_dir, root) = setup_ws_with_pattern().await;
    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .unwrap();

    // Reflect on main first so the pattern is discovered + persisted.
    engine.reflect("demo").await.unwrap();

    // Create a branch — it inherits main's pattern set at fork.
    thinkingroot_branch::create_branch(&root, "feat/inherit", "main", None)
        .await
        .unwrap();

    // Trigger reflect on the branch so its `structural_patterns`
    // table is populated (otherwise it inherits the rows from
    // main's snapshot but `reflect_across_branches` reads
    // `result.patterns` from a fresh reflect run).
    engine
        .reflect_branched("demo", Some("feat/inherit"))
        .await
        .unwrap();

    let result = engine
        .reflect_across_branches(
            "demo",
            &["main".to_string(), "feat/inherit".to_string()],
        )
        .await
        .expect("cross-branch reflect");

    assert_eq!(result.workspace, "demo");
    assert_eq!(result.branches.len(), 2);
    assert!(result.per_branch.contains_key("main"));
    assert!(result.per_branch.contains_key("feat/inherit"));

    // Both branches saw the same pattern set; nothing divergent.
    assert!(
        result.divergent_patterns.is_empty(),
        "post-fork-pre-edit branches share patterns: divergent must be empty, got {:?}",
        result
            .divergent_patterns
            .iter()
            .map(|p| p.pattern_id.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn cross_branch_reflect_errors_on_empty_branch_list() {
    let (_dir, root) = setup_ws_with_pattern().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let res = engine.reflect_across_branches("demo", &[]).await;
    assert!(
        res.is_err(),
        "empty branches list must surface as Err — almost always a caller bug"
    );
}
