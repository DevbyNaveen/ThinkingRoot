//! P2 — developmental age reflects verified capability + knowledge, NOT uptime.

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{Claim, ClaimType, ContentHash, Source, SourceType, TrustLevel, WorkspaceId};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

#[tokio::test]
async fn age_is_infant_when_empty() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    {
        let _g = GraphStore::init(&graph_dir).unwrap();
    }

    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let age = engine.developmental_age("demo").await.unwrap();
    assert_eq!(age.total_capabilities, 0);
    assert_eq!(age.verified_capabilities, 0);
    assert_eq!(age.claims, 0);
    assert_eq!(age.stage, "infant");
}

#[tokio::test]
async fn age_grows_with_capabilities_and_claims() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();

    let ws = WorkspaceId::new();
    {
        let graph = GraphStore::init(&graph_dir).unwrap();
        let source = Source::new("file:///x.md".into(), SourceType::Document)
            .with_trust(TrustLevel::Trusted)
            .with_hash(ContentHash("h".into()));
        let source_id = source.id;
        graph.insert_source(&source).unwrap();

        for i in 0..6 {
            let c = Claim::new(&format!("fact {i}"), ClaimType::Fact, source_id, ws)
                .with_confidence(0.8);
            let id = c.id.to_string();
            graph.insert_claim(&c).unwrap();
            graph.link_claim_to_source(&id, &source_id.to_string()).unwrap();
        }

        graph.put_function("greeter", "() => 'hi'", "javascript").unwrap();
        graph.put_function("adder", "(a, b) => a + b", "javascript").unwrap();
    }

    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let age = engine.developmental_age("demo").await.unwrap();
    assert_eq!(age.total_capabilities, 2, "two functions deployed");
    assert_eq!(age.claims, 6, "six claims");
    assert_eq!(age.verified_capabilities, 0, "no successful runs yet");
    assert!(age.developmental_age > 0.0, "age must be positive with content");
    assert_ne!(age.stage, "infant", "6 claims (>=5) → past infant");
}

#[tokio::test]
async fn drives_infant_is_maximally_curious() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    {
        let _g = GraphStore::init(&graph_dir).unwrap();
    }
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let d = engine.drives("demo").await.unwrap();
    assert_eq!(d.stage, "infant");
    assert!(d.curiosity > 0.9, "infant is maximally curious, got {}", d.curiosity);
    assert!(d.exploration_rate > 0.9);
    assert!(d.frontier_focus < 0.1);
}

#[tokio::test]
async fn drives_curiosity_decays_as_the_being_matures() {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    let ws = WorkspaceId::new();
    {
        let graph = GraphStore::init(&graph_dir).unwrap();
        let source = Source::new("file:///x.md".into(), SourceType::Document)
            .with_trust(TrustLevel::Trusted)
            .with_hash(ContentHash("h".into()));
        let source_id = source.id;
        graph.insert_source(&source).unwrap();
        for i in 0..6 {
            let c = Claim::new(&format!("fact {i}"), ClaimType::Fact, source_id, ws)
                .with_confidence(0.8);
            let id = c.id.to_string();
            graph.insert_claim(&c).unwrap();
            graph.link_claim_to_source(&id, &source_id.to_string()).unwrap();
        }
        graph.put_function("greeter", "() => 'hi'", "javascript").unwrap();
        graph.put_function("adder", "(a, b) => a + b", "javascript").unwrap();
    }
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let d = engine.drives("demo").await.unwrap();
    // 2 capabilities + 6 claims → maturity rises → curiosity well below an infant's.
    assert!(d.curiosity < 0.7, "matured being is less curious, got {}", d.curiosity);
    assert!(d.frontier_focus > 0.3, "matured being focuses more on frontier");
}
