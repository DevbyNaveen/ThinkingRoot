//! B5 "sleep" — contradiction-resolution consolidation.
//!
//! A workspace with an unresolved contradiction, after `sleep_consolidate`, has
//! the older/less-confident claim superseded and the contradiction cleared — the
//! being "rests and wakes wiser", and the next recall returns the surviving truth.

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{Claim, ClaimType, ContentHash, Source, SourceType, TrustLevel, WorkspaceId};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

#[tokio::test]
async fn sleep_resolves_contradiction_by_superseding_the_weaker_claim() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();

    let ws = WorkspaceId::new();
    {
        let graph = GraphStore::init(&graph_dir).unwrap();
        let source = Source::new("file:///sky.md".into(), SourceType::Document)
            .with_trust(TrustLevel::Trusted)
            .with_hash(ContentHash("h".into()));
        let source_id = source.id;
        graph.insert_source(&source).unwrap();

        // Loser: lower confidence.
        let a = Claim::new("the sky is blue", ClaimType::Fact, source_id, ws).with_confidence(0.6);
        let a_id = a.id.to_string();
        graph.insert_claim(&a).unwrap();
        graph
            .link_claim_to_source(&a_id, &source_id.to_string())
            .unwrap();

        // Winner: higher confidence.
        let b = Claim::new("the sky is green", ClaimType::Fact, source_id, ws).with_confidence(0.9);
        let b_id = b.id.to_string();
        graph.insert_claim(&b).unwrap();
        graph
            .link_claim_to_source(&b_id, &source_id.to_string())
            .unwrap();

        graph
            .insert_contradiction("contra-1", &a_id, &b_id, "sky colour conflict")
            .unwrap();
    } // drop the GraphStore so the engine can open the same dir

    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    // Precondition: exactly one unresolved contradiction.
    let before = engine.list_contradictions("demo").await.unwrap();
    assert_eq!(
        before.len(),
        1,
        "expected one detected contradiction before sleep"
    );

    // Sleep → resolve by superseding the weaker claim.
    let report = engine.sleep_consolidate("demo", None, 0.5).await.unwrap();
    assert_eq!(
        report.contradictions_resolved, 1,
        "sleep should resolve the one contradiction"
    );
    assert_eq!(report.claims_superseded, 1);

    // Postcondition: the contradiction is cleared (the loser was superseded).
    let after = engine.list_contradictions("demo").await.unwrap();
    assert!(
        after.is_empty(),
        "contradiction should be cleared after sleep, got {after:?}"
    );

    // A second sleep is a no-op (idempotent).
    let again = engine.sleep_consolidate("demo", None, 0.5).await.unwrap();
    assert_eq!(again.contradictions_resolved, 0, "second sleep is a no-op");
}

#[tokio::test]
async fn sleep_expires_old_low_confidence_claims_only() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();

    let ws = WorkspaceId::new();
    let old = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    {
        let graph = GraphStore::init(&graph_dir).unwrap();
        let source = Source::new("file:///x.md".into(), SourceType::Document)
            .with_trust(TrustLevel::Trusted)
            .with_hash(ContentHash("h".into()));
        let source_id = source.id;
        graph.insert_source(&source).unwrap();

        // Old + low confidence → should expire.
        let mut old_weak =
            Claim::new("ancient rumor", ClaimType::Fact, source_id, ws).with_confidence(0.3);
        old_weak.created_at = old;
        let id = old_weak.id.to_string();
        graph.insert_claim(&old_weak).unwrap();
        graph.link_claim_to_source(&id, &source_id.to_string()).unwrap();

        // Old + HIGH confidence → must be kept.
        let mut old_strong =
            Claim::new("timeless truth", ClaimType::Fact, source_id, ws).with_confidence(0.95);
        old_strong.created_at = old;
        let id = old_strong.id.to_string();
        graph.insert_claim(&old_strong).unwrap();
        graph.link_claim_to_source(&id, &source_id.to_string()).unwrap();

        // Recent + low confidence → must be kept (not stale yet).
        let recent =
            Claim::new("fresh hunch", ClaimType::Fact, source_id, ws).with_confidence(0.3);
        let id = recent.id.to_string();
        graph.insert_claim(&recent).unwrap();
        graph.link_claim_to_source(&id, &source_id.to_string()).unwrap();
    }

    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    // Cutoff between the 2020 claims and "now"; floor 0.5.
    let cutoff = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
        .unwrap()
        .timestamp() as f64;
    let report = engine
        .sleep_consolidate("demo", Some(cutoff), 0.5)
        .await
        .unwrap();
    assert_eq!(
        report.stale_expired, 1,
        "only the old + low-confidence claim should expire (kept: old-strong, recent-weak)"
    );

    // Idempotent: a second pass expires nothing new.
    let again = engine
        .sleep_consolidate("demo", Some(cutoff), 0.5)
        .await
        .unwrap();
    assert_eq!(again.stale_expired, 0, "stale-expiry is idempotent");
}
