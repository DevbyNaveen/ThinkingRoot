//! Phase A regression test: merge_branch must reload the serve-layer cache
//! so that post-merge `list_claims` reflects the merged state without
//! requiring a subsequent `compile` or `contribute`.
//!
//! Before Phase A, `execute_merge` was called inline by the MCP and REST
//! handlers without touching `KnowledgeGraph`. Post-merge reads returned
//! stale data. This test locks the contract down.

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, MergedBy, Source, SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{AgentClaim, BranchActor, ClaimFilter, QueryEngine};
use thinkingroot_serve::intelligence::session::new_session_store;

fn seed_claim(graph: &GraphStore, workspace: WorkspaceId, statement: &str, uri: &str) -> String {
    let source = Source::new(uri.to_string(), SourceType::Document)
        .with_trust(TrustLevel::Trusted)
        .with_hash(ContentHash(format!("hash-{uri}")));
    graph.insert_source(&source).expect("insert source");

    let claim = Claim::new(statement, ClaimType::Fact, source.id, workspace);
    let claim_id = claim.id.to_string();
    graph.insert_claim(&claim).expect("insert claim");
    // Claim ↔ source edge — cache reads require this junction populated.
    graph
        .link_claim_to_source(&claim_id, &source.id.to_string())
        .expect("link claim to source");
    claim_id
}

#[tokio::test]
async fn merge_branch_reloads_cache_without_compile() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();

    let workspace = WorkspaceId::new();

    // ── 1. Seed main with a baseline claim ───────────────────────────────
    {
        let main_graph = GraphStore::init(&graph_dir).expect("init main graph");
        seed_claim(
            &main_graph,
            workspace,
            "AuthService uses JWT tokens for authentication",
            "file:///main.md",
        );
    }

    // ── 2. Create a branch (copies graph.db) ─────────────────────────────
    thinkingroot_branch::create_branch(&root, "feature/oauth", "main", None)
        .await
        .expect("create branch");

    // ── 3. Add a new claim to the branch only ────────────────────────────
    let branch_data_dir = root
        .join(".thinkingroot")
        .join("branches")
        .join("feature-oauth");
    let branch_claim_id = {
        let branch_graph =
            GraphStore::init(&branch_data_dir.join("graph")).expect("init branch graph");
        seed_claim(
            &branch_graph,
            workspace,
            "AuthService also supports OAuth2 authorization code flow",
            "file:///branch.md",
        )
    };

    // ── 4. Mount the workspace and establish baseline ────────────────────
    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .expect("mount workspace");

    let baseline = engine
        .list_claims("demo", ClaimFilter::default())
        .await
        .expect("baseline list_claims");
    assert_eq!(
        baseline.len(),
        1,
        "main cache should have 1 claim before merge, got {:?}",
        baseline.iter().map(|c| &c.statement).collect::<Vec<_>>()
    );

    // ── 5. Merge the branch — this is the Bug A code path ────────────────
    let diff = engine
        .merge_branch(
            &root,
            "feature/oauth",
            /* force */ true,
            /* propagate_deletions */ false,
            MergedBy::Human {
                user: "test".to_string(),
            },
        )
        .await
        .expect("merge branch");
    assert!(
        !diff.new_claims.is_empty(),
        "compute_diff should identify the branch claim as new"
    );

    // ── 6. Verify: post-merge list_claims reflects the merged state ──────
    //     WITHOUT any intervening compile or contribute.
    let post_merge = engine
        .list_claims("demo", ClaimFilter::default())
        .await
        .expect("post-merge list_claims");
    assert_eq!(
        post_merge.len(),
        2,
        "main cache must include merged claim immediately (Bug A regression guard). got: {:?}",
        post_merge.iter().map(|c| &c.statement).collect::<Vec<_>>()
    );
    assert!(
        post_merge.iter().any(|c| c.id == branch_claim_id),
        "merged claim id {branch_claim_id} should be visible in post-merge cache"
    );
}

#[tokio::test]
async fn rollback_merge_restores_pre_merge_cache() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();

    let workspace = WorkspaceId::new();

    {
        let main_graph = GraphStore::init(&graph_dir).unwrap();
        seed_claim(
            &main_graph,
            workspace,
            "baseline claim",
            "file:///baseline.md",
        );
    }

    thinkingroot_branch::create_branch(&root, "feature/x", "main", None)
        .await
        .unwrap();

    let branch_dir = root
        .join(".thinkingroot")
        .join("branches")
        .join("feature-x")
        .join("graph");
    {
        let bg = GraphStore::init(&branch_dir).unwrap();
        seed_claim(&bg, workspace, "branch addition", "file:///branch.md");
    }

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .unwrap();

    // Merge: main now has 2 claims.
    engine
        .merge_branch(
            &root,
            "feature/x",
            true,
            false,
            MergedBy::Human {
                user: "test".to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        engine
            .list_claims("demo", ClaimFilter::default())
            .await
            .unwrap()
            .len(),
        2
    );

    // Rollback: main returns to 1 claim, AND cache reflects it.
    engine.rollback_merge(&root, "feature/x").await.unwrap();
    let post = engine
        .list_claims("demo", ClaimFilter::default())
        .await
        .unwrap();
    assert_eq!(
        post.len(),
        1,
        "post-rollback cache must show only baseline claim; got {:?}",
        post.iter().map(|c| &c.statement).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn merge_branch_missing_branch_errors() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join(".thinkingroot").join("graph")).unwrap();
    {
        let _ = GraphStore::init(&root.join(".thinkingroot").join("graph")).unwrap();
    }

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .expect("mount workspace");

    let result = engine
        .merge_branch(
            &root,
            "does/not/exist",
            true,
            false,
            MergedBy::Human {
                user: "test".to_string(),
            },
        )
        .await;

    assert!(result.is_err(), "merge on missing branch must error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("not found") || msg.contains("does/not/exist"),
        "error should mention missing branch, got: {msg}"
    );
}

#[tokio::test]
async fn merge_into_branch_updates_target_branch_without_touching_main_cache() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let workspace = WorkspaceId::new();

    {
        let main_graph = GraphStore::init(&graph_dir).unwrap();
        seed_claim(&main_graph, workspace, "baseline claim", "file:///main.md");
    }

    thinkingroot_branch::create_branch(&root, "feature/source", "main", None)
        .await
        .unwrap();
    thinkingroot_branch::create_branch(&root, "feature/target", "main", None)
        .await
        .unwrap();

    {
        let source_graph =
            GraphStore::init(&root.join(".thinkingroot/branches/feature-source/graph")).unwrap();
        seed_claim(
            &source_graph,
            workspace,
            "feature source adds Redis session storage",
            "file:///source.md",
        );
    }

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .unwrap();

    let diff = engine
        .merge_into_branch(
            &root,
            "feature/source",
            Some("feature/target"),
            true,
            false,
            MergedBy::Human {
                user: "test".to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(diff.to_branch, "feature/target");

    let target_claims = engine
        .list_claims_branched("demo", ClaimFilter::default(), Some("feature/target"))
        .await
        .unwrap();
    assert!(
        target_claims
            .iter()
            .any(|c| c.statement.contains("Redis session storage")),
        "target branch should contain merged branch-only claim"
    );

    let main_claims = engine
        .list_claims("demo", ClaimFilter::default())
        .await
        .unwrap();
    assert_eq!(main_claims.len(), 1, "main cache should remain unchanged");
}

#[tokio::test]
async fn rebase_branch_pulls_parent_claims_into_child_branch() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let workspace = WorkspaceId::new();

    {
        let main_graph = GraphStore::init(&graph_dir).unwrap();
        seed_claim(
            &main_graph,
            workspace,
            "baseline claim",
            "file:///baseline.md",
        );
    }

    thinkingroot_branch::create_branch_with_owner(
        &root,
        "feature/owned",
        "main",
        None,
        Some("alice".to_string()),
        thinkingroot_core::BranchPermissions::default(),
    )
    .await
    .unwrap();

    {
        let main_graph = GraphStore::init(&graph_dir).unwrap();
        seed_claim(
            &main_graph,
            workspace,
            "main gained OAuth support after branch creation",
            "file:///main-after.md",
        );
    }

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .unwrap();

    let diff = engine
        .rebase_branch(
            &root,
            "feature/owned",
            BranchActor::User("alice".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(diff.from_branch, "main");
    assert_eq!(diff.to_branch, "feature/owned");

    let branch_claims = engine
        .list_claims_branched("demo", ClaimFilter::default(), Some("feature/owned"))
        .await
        .unwrap();
    assert!(
        branch_claims
            .iter()
            .any(|c| c.statement.contains("OAuth support")),
        "rebased branch should now include parent-only claim"
    );
}

#[tokio::test]
async fn branch_permissions_block_non_owner_contribute_and_delete() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let workspace = WorkspaceId::new();

    {
        let main_graph = GraphStore::init(&graph_dir).unwrap();
        seed_claim(
            &main_graph,
            workspace,
            "baseline claim",
            "file:///baseline.md",
        );
    }

    thinkingroot_branch::create_branch_with_owner(
        &root,
        "feature/private",
        "main",
        None,
        Some("alice".to_string()),
        thinkingroot_core::BranchPermissions::default(),
    )
    .await
    .unwrap();

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .unwrap();
    let sessions = new_session_store();

    let err = engine
        .contribute_claims_as(
            "demo",
            "sess-1",
            Some("feature/private"),
            vec![AgentClaim {
                statement: "unauthorized claim".to_string(),
                claim_type: "fact".to_string(),
                confidence: Some(0.8),
                entities: vec!["Private".to_string()],
            }],
            &sessions,
            BranchActor::User("bob".to_string()),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("permission denied"),
        "expected permission failure for contribute, got {err}"
    );

    let err = engine
        .delete_branch_as(
            &root,
            "feature/private",
            BranchActor::User("bob".to_string()),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("permission denied"),
        "expected permission failure for delete, got {err}"
    );
}
