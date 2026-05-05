//! T2.4 — Bitemporal "as-of" claim query.
//!
//! Pins:
//!
//! 1. `list_claims_as_of_branched` returns claims whose
//!    `created_at` is ≤ the supplied tx_time.
//! 2. Claims inserted AFTER the tx_time are excluded.
//! 3. The query works against both main and non-main branches.

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, Source, SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

async fn setup_with_two_claim_eras() -> (tempfile::TempDir, PathBuf) {
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

    // Era 1: claims with created_at = 2026-01-01.
    let mut early = Claim::new("early fact", ClaimType::Fact, source_id, workspace);
    early.created_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let early_id = early.id.to_string();
    graph.insert_claim(&early).unwrap();
    graph
        .link_claim_to_source(&early_id, &source_id.to_string())
        .unwrap();

    // Era 2: claims with created_at = 2026-06-01.
    let mut late = Claim::new("late fact", ClaimType::Fact, source_id, workspace);
    late.created_at = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let late_id = late.id.to_string();
    graph.insert_claim(&late).unwrap();
    graph
        .link_claim_to_source(&late_id, &source_id.to_string())
        .unwrap();

    (dir, root)
}

#[tokio::test]
async fn as_of_returns_only_claims_created_at_or_before_tx_time() {
    let (_dir, root) = setup_with_two_claim_eras().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    // Pinpoint between the two eras.  Only "early fact" should
    // surface; "late fact" was inserted after this moment.
    let mid_2026 = chrono::DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let early = engine
        .list_claims_as_of_branched("demo", None, mid_2026)
        .await
        .expect("as-of query");
    let stmts: Vec<&str> = early.iter().map(|c| c.statement.as_str()).collect();
    assert!(
        stmts.contains(&"early fact"),
        "early-era claim must surface (got {stmts:?})"
    );
    assert!(
        !stmts.contains(&"late fact"),
        "late-era claim must NOT surface (got {stmts:?})"
    );
}

#[tokio::test]
async fn as_of_at_far_future_returns_every_claim() {
    let (_dir, root) = setup_with_two_claim_eras().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let future = chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let all = engine
        .list_claims_as_of_branched("demo", None, future)
        .await
        .expect("as-of query");
    let stmts: Vec<&str> = all.iter().map(|c| c.statement.as_str()).collect();
    assert!(stmts.contains(&"early fact"));
    assert!(stmts.contains(&"late fact"));
}

#[tokio::test]
async fn as_of_at_far_past_returns_empty() {
    let (_dir, root) = setup_with_two_claim_eras().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let prehistoric = chrono::DateTime::parse_from_rfc3339("1900-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let none = engine
        .list_claims_as_of_branched("demo", None, prehistoric)
        .await
        .expect("as-of query");
    assert!(
        none.is_empty(),
        "no claim should pre-date 1900 (got {:?})",
        none.iter().map(|c| &c.statement).collect::<Vec<_>>()
    );
}
