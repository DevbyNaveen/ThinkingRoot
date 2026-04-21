//! Phase C regression tests for `BranchEngineCache`.
//!
//! The LRU must: return the same `Arc` on repeated gets (hit), evict on
//! TTL, evict on size, invalidate on demand, and never hand out a handle
//! to a branch that doesn't exist on disk.

use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;

use thinkingroot_core::config::BranchCacheConfig;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::branch_cache::BranchEngineCache;

async fn make_ws_with_branch(branch_names: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    {
        let _ = GraphStore::init(&graph_dir).unwrap();
    }
    for name in branch_names {
        thinkingroot_branch::create_branch(&root, name, "main", None)
            .await
            .unwrap();
    }
    (dir, root)
}

#[tokio::test]
async fn repeated_get_returns_same_arc() {
    let (_dir, root) = make_ws_with_branch(&["feat/a"]).await;
    let cache = BranchEngineCache::new(&BranchCacheConfig::default());

    let h1 = cache.get_or_open(&root, "feat/a").await.unwrap();
    let h2 = cache.get_or_open(&root, "feat/a").await.unwrap();
    assert!(
        Arc::ptr_eq(&h1, &h2),
        "cache hits must return the exact same Arc instance"
    );
    assert_eq!(cache.len().await, 1);
}

#[tokio::test]
async fn different_branches_get_different_handles() {
    let (_dir, root) = make_ws_with_branch(&["feat/a", "feat/b"]).await;
    let cache = BranchEngineCache::new(&BranchCacheConfig::default());

    let a = cache.get_or_open(&root, "feat/a").await.unwrap();
    let b = cache.get_or_open(&root, "feat/b").await.unwrap();
    assert!(!Arc::ptr_eq(&a, &b));
    assert_eq!(cache.len().await, 2);
}

#[tokio::test]
async fn missing_branch_errors_without_caching() {
    let (_dir, root) = make_ws_with_branch(&[]).await;
    let cache = BranchEngineCache::new(&BranchCacheConfig::default());

    let r = cache.get_or_open(&root, "does/not/exist").await;
    assert!(r.is_err(), "missing branch must return an error");
    assert_eq!(
        cache.len().await,
        0,
        "failed opens must not leave stubs in the cache"
    );
}

#[tokio::test]
async fn invalidate_single_entry() {
    let (_dir, root) = make_ws_with_branch(&["feat/a", "feat/b"]).await;
    let cache = BranchEngineCache::new(&BranchCacheConfig::default());

    let h_a1 = cache.get_or_open(&root, "feat/a").await.unwrap();
    let _h_b1 = cache.get_or_open(&root, "feat/b").await.unwrap();
    assert_eq!(cache.len().await, 2);

    cache.invalidate(&root, "feat/a").await;
    assert_eq!(cache.len().await, 1);

    let h_a2 = cache.get_or_open(&root, "feat/a").await.unwrap();
    assert!(
        !Arc::ptr_eq(&h_a1, &h_a2),
        "post-invalidate get must return a freshly opened Arc"
    );
}

#[tokio::test]
async fn invalidate_workspace_clears_all() {
    let (_dir, root) = make_ws_with_branch(&["feat/a", "feat/b", "feat/c"]).await;
    let cache = BranchEngineCache::new(&BranchCacheConfig::default());

    cache.get_or_open(&root, "feat/a").await.unwrap();
    cache.get_or_open(&root, "feat/b").await.unwrap();
    cache.get_or_open(&root, "feat/c").await.unwrap();
    assert_eq!(cache.len().await, 3);

    cache.invalidate_workspace(&root).await;
    assert_eq!(cache.len().await, 0);
}

#[tokio::test]
async fn lru_evicts_when_over_capacity() {
    let mut names = Vec::new();
    for i in 0..5 {
        names.push(format!("feat/b{i}"));
    }
    let (_dir, root) =
        make_ws_with_branch(&names.iter().map(String::as_str).collect::<Vec<_>>()).await;

    let cfg = BranchCacheConfig {
        max_entries: 3,
        ttl_secs: 300,
        disabled: false,
    };
    let cache = BranchEngineCache::new(&cfg);

    for name in &names {
        cache.get_or_open(&root, name).await.unwrap();
    }
    assert_eq!(
        cache.len().await,
        3,
        "LRU capped at max_entries=3 despite 5 opens"
    );
}

#[tokio::test]
async fn ttl_expiry_forces_fresh_open() {
    let (_dir, root) = make_ws_with_branch(&["feat/a"]).await;
    // 1-second TTL. Tokio can fire time::sleep synchronously under
    // `time::pause` but this test asserts on real elapsed time for
    // simplicity; keep TTL tiny.
    let cfg = BranchCacheConfig {
        max_entries: 16,
        ttl_secs: 1,
        disabled: false,
    };
    let cache = BranchEngineCache::new(&cfg);

    let h1 = cache.get_or_open(&root, "feat/a").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let h2 = cache.get_or_open(&root, "feat/a").await.unwrap();
    assert!(
        !Arc::ptr_eq(&h1, &h2),
        "after TTL, cache must re-open a fresh handle"
    );
}

#[tokio::test]
async fn disabled_bypasses_cache_entirely() {
    let (_dir, root) = make_ws_with_branch(&["feat/a"]).await;
    let cfg = BranchCacheConfig {
        max_entries: 16,
        ttl_secs: 300,
        disabled: true,
    };
    let cache = BranchEngineCache::new(&cfg);

    let h1 = cache.get_or_open(&root, "feat/a").await.unwrap();
    let h2 = cache.get_or_open(&root, "feat/a").await.unwrap();
    assert!(
        !Arc::ptr_eq(&h1, &h2),
        "disabled cache must return a fresh Arc on every call"
    );
    assert_eq!(
        cache.len().await,
        0,
        "disabled cache must not retain entries"
    );
}

#[tokio::test]
async fn cached_handle_observes_writes_through_same_handle() {
    // Correctness contract: two consecutive calls going through the cache
    // see each other's writes (they hit the same DbInstance), not a
    // stale snapshot. If cozo ever changed semantics so the same
    // DbInstance buffered writes invisibly, this test would catch it.
    use thinkingroot_core::{
        Claim, ClaimType, ContentHash, Source, SourceType, TrustLevel, WorkspaceId,
    };

    let (_dir, root) = make_ws_with_branch(&["feat/writes"]).await;
    let cache = BranchEngineCache::new(&BranchCacheConfig::default());
    let workspace = WorkspaceId::new();

    let h = cache.get_or_open(&root, "feat/writes").await.unwrap();
    let source = Source::new("file:///x.md".into(), SourceType::Document)
        .with_trust(TrustLevel::Trusted)
        .with_hash(ContentHash("h".into()));
    h.graph.insert_source(&source).unwrap();
    let claim = Claim::new("hello", ClaimType::Fact, source.id, workspace);
    let cid = claim.id.to_string();
    h.graph.insert_claim(&claim).unwrap();
    h.graph.link_claim_to_source(&cid, &source.id.to_string()).unwrap();

    let h_again = cache.get_or_open(&root, "feat/writes").await.unwrap();
    let rows = h_again.graph.get_all_claims_with_sources().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "write visible through the same cached handle"
    );
    assert!(Arc::ptr_eq(&h, &h_again));
}
