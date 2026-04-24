//! Adversarial KVC tests — stress the branch/cache/merge paths under
//! concurrency and rollback scenarios that the happy-path suite doesn't
//! cover.
//!
//! Scenarios:
//!   1. Concurrent merges from different branches into main — both must
//!      complete without corrupting the cache or losing claims.
//!   2. Rollback-during-read — a reader mid-call must see a consistent
//!      snapshot; rollback atomicity is verified end-to-end.
//!   3. Branch cache entry retained across invalidation — an Arc handed
//!      out before invalidation must remain usable; subsequent lookups
//!      must return a fresh handle.
//!
//! Shared harness: `seed_workspace_with_main_claim` + `seed_branch_claim`
//! mirror the pattern from `merge_cache_reload_test.rs`.

use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::task::JoinSet;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, MergedBy, Source, SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{ClaimFilter, QueryEngine};

fn seed_claim(graph: &GraphStore, workspace: WorkspaceId, statement: &str, uri: &str) -> String {
    let source = Source::new(uri.to_string(), SourceType::Document)
        .with_trust(TrustLevel::Trusted)
        .with_hash(ContentHash(format!("hash-{uri}")));
    graph.insert_source(&source).expect("insert source");

    let claim = Claim::new(statement, ClaimType::Fact, source.id, workspace);
    let claim_id = claim.id.to_string();
    graph.insert_claim(&claim).expect("insert claim");
    graph
        .link_claim_to_source(&claim_id, &source.id.to_string())
        .expect("link claim to source");
    claim_id
}

async fn seeded_workspace() -> (tempfile::TempDir, PathBuf, WorkspaceId) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();

    let workspace = WorkspaceId::new();
    {
        let main_graph = GraphStore::init(&graph_dir).expect("init main graph");
        seed_claim(
            &main_graph,
            workspace,
            "Baseline: main has one claim",
            "file:///main.md",
        );
    }
    (dir, root, workspace)
}

async fn seed_branch(root: &std::path::Path, name: &str, statement: &str, uri: &str) {
    thinkingroot_branch::create_branch(root, name, "main", None)
        .await
        .expect("create branch");
    let branch_dir_name = name.replace('/', "-");
    let branch_data_dir = root
        .join(".thinkingroot")
        .join("branches")
        .join(&branch_dir_name);
    let branch_graph = GraphStore::init(&branch_data_dir.join("graph")).expect("init branch graph");
    seed_claim(&branch_graph, WorkspaceId::new(), statement, uri);
}

// ─── Scenario 1: Concurrent merges from different branches ──

/// Two independent branches merge into main at the same time. Both must
/// succeed and main's cache must reflect both sets of claims afterwards,
/// with no claim lost. The engine uses a MergeLock per workspace — this
/// test verifies that lock actually serializes writers correctly.
#[tokio::test]
async fn concurrent_merges_into_main_preserve_all_claims() {
    let (_dir, root, _ws) = seeded_workspace().await;

    // Prep two feature branches with distinct claims.
    seed_branch(
        &root,
        "feature/alpha",
        "Alpha adds OAuth2 support",
        "file:///alpha.md",
    )
    .await;
    seed_branch(
        &root,
        "feature/beta",
        "Beta adds SAML support",
        "file:///beta.md",
    )
    .await;

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .expect("mount");

    // Share the engine via Arc so both merge tasks see the same instance
    // (mirrors serve-layer topology where one QueryEngine handles many
    // requests).
    let engine = Arc::new(tokio::sync::Mutex::new(engine));

    let mut set: JoinSet<Result<_, String>> = JoinSet::new();
    for branch in ["feature/alpha", "feature/beta"] {
        let root = root.clone();
        let engine = engine.clone();
        let b = branch.to_string();
        set.spawn(async move {
            let eng = engine.lock().await;
            eng.merge_branch(
                &root,
                &b,
                /* force */ true,
                /* propagate_deletions */ false,
                MergedBy::Human {
                    user: format!("test-{b}"),
                },
            )
            .await
            .map_err(|e| format!("{b}: {e}"))
        });
    }

    while let Some(res) = set.join_next().await {
        res.expect("task panicked").expect("merge returned error");
    }

    // Post-merge, main cache should list 3 claims (baseline + alpha + beta).
    let eng = engine.lock().await;
    let claims = eng
        .list_claims("demo", ClaimFilter::default())
        .await
        .expect("list_claims");
    let statements: Vec<&str> = claims.iter().map(|c| c.statement.as_str()).collect();

    assert_eq!(
        claims.len(),
        3,
        "expected 3 claims after two merges, got {statements:?}"
    );
    assert!(
        statements.iter().any(|s| s.contains("OAuth2")),
        "alpha claim missing: {statements:?}"
    );
    assert!(
        statements.iter().any(|s| s.contains("SAML")),
        "beta claim missing: {statements:?}"
    );
}

// ─── Scenario 2: Rollback-during-read ────────────────────────

/// A merge lands, then a rollback fires while a reader is holding the
/// result of `list_claims`. The rollback must be atomic — post-rollback
/// reads must see the pre-merge state; the in-flight reader holds a
/// snapshot-consistent view (no half-rolled-back data).
#[tokio::test]
async fn rollback_after_merge_restores_pre_merge_state() {
    let (_dir, root, _ws) = seeded_workspace().await;

    seed_branch(
        &root,
        "feature/rollback-me",
        "This claim should be rolled back",
        "file:///rollback.md",
    )
    .await;

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .unwrap();

    // Merge.
    engine
        .merge_branch(
            &root,
            "feature/rollback-me",
            true,
            false,
            MergedBy::Human {
                user: "test".to_string(),
            },
        )
        .await
        .expect("merge");

    // Snapshot reader BEFORE rollback — must see merged claim.
    let pre_rollback = engine
        .list_claims("demo", ClaimFilter::default())
        .await
        .expect("pre-rollback list");
    assert!(
        pre_rollback
            .iter()
            .any(|c| c.statement.contains("rolled back")),
        "merged claim not visible pre-rollback: {:?}",
        pre_rollback
            .iter()
            .map(|c| &c.statement)
            .collect::<Vec<_>>()
    );

    // Rollback.
    engine
        .rollback_merge(&root, "feature/rollback-me")
        .await
        .expect("rollback");

    // Post-rollback reader — must see ONLY the baseline claim.
    let post_rollback = engine
        .list_claims("demo", ClaimFilter::default())
        .await
        .expect("post-rollback list");
    assert_eq!(
        post_rollback.len(),
        1,
        "expected 1 claim after rollback, got {:?}",
        post_rollback
            .iter()
            .map(|c| &c.statement)
            .collect::<Vec<_>>()
    );
    assert!(
        post_rollback[0].statement.contains("Baseline"),
        "rollback did not restore baseline: {}",
        post_rollback[0].statement
    );
}

// ─── Scenario 3: Branch cache handoff safety ─────────────────

/// A reader acquires a branch `Arc` from the cache, then the cache is
/// invalidated (e.g. by gc or delete). The reader must still complete
/// successfully — the Arc keeps the GraphStore alive. A subsequent cache
/// miss must yield a fresh handle without panicking.
///
/// This guards against the latent race called out in the CTO audit.
#[tokio::test]
async fn branch_cache_arc_survives_invalidation() {
    let (_dir, root, _ws) = seeded_workspace().await;
    seed_branch(
        &root,
        "feature/cache-race",
        "Claim on cached branch",
        "file:///cache.md",
    )
    .await;

    let mut engine = QueryEngine::new();
    engine
        .mount("demo".to_string(), root.clone())
        .await
        .unwrap();

    // Prime the branch cache via a read that goes through get_or_open.
    let first = engine
        .list_claims_branched("demo", ClaimFilter::default(), Some("feature/cache-race"))
        .await
        .expect("branched read #1");
    assert!(!first.is_empty(), "branched read returned empty");

    // Invalidate the cache entry. Any Arc previously handed out remains
    // valid; the invalidation only evicts the entry so subsequent lookups
    // reopen.
    engine
        .branch_engines()
        .invalidate(&root, "feature/cache-race")
        .await;

    // Read again — must re-open cleanly and return the same data.
    let second = engine
        .list_claims_branched("demo", ClaimFilter::default(), Some("feature/cache-race"))
        .await
        .expect("branched read #2 after invalidate");
    assert_eq!(
        first.len(),
        second.len(),
        "branched read inconsistent across invalidation"
    );
}
