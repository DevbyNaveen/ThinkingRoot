//! Phase 9 Reflect — end-to-end integration test through QueryEngine.
//!
//! Verifies the engine's `reflect` / `list_gaps` methods work against a
//! mounted workspace, the MCP tools surface advertises `reflect` + `gaps`,
//! and the verifier's coverage score discounts open gaps.

use std::path::PathBuf;
use tempfile::tempdir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, Entity, EntityType, Source, SourceType, TrustLevel,
    WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

/// Seed a workspace where 40 Service entities share `ApiSignature`; 37 also
/// have `Requirement`. Returns the root path. The uncovered 3 services
/// should surface as gaps once Reflect runs.
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
        graph.link_claim_to_source(&cid, &source_id.to_string()).unwrap();
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
async fn engine_reflect_discovers_gaps() {
    let (_dir, root) = setup_ws_with_pattern().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let result = engine.reflect("demo").await.unwrap();
    assert!(
        !result.patterns.is_empty(),
        "expected at least the ApiSignature→Requirement pattern"
    );
    assert!(result.open_gaps_total >= 3, "expected ≥3 gaps for GapSvc*");

    let gaps = engine.list_gaps("demo", None, 0.70).await.unwrap();
    assert_eq!(gaps.len(), 3);
    for g in &gaps {
        assert_eq!(g.entity_type, "Service");
        assert_eq!(g.expected_claim_type, "Requirement");
        assert!(g.entity_name.starts_with("GapSvc"));
    }

    let scoped = engine
        .list_gaps("demo", Some("GapSvc37"), 0.0)
        .await
        .unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].entity_name, "GapSvc37");
}

#[tokio::test]
async fn verifier_coverage_discounts_open_gaps() {
    let (_dir, root) = setup_ws_with_pattern().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    // Baseline verify — no gaps discovered yet.
    let before = engine.verify("demo").await.unwrap();
    let cov_before = before.health_score.coverage;

    engine.reflect("demo").await.unwrap();

    // After reflect — gaps are open, coverage should drop.
    let after = engine.verify("demo").await.unwrap();
    let cov_after = after.health_score.coverage;

    assert!(
        cov_after < cov_before,
        "coverage must drop after gaps discovered: before={cov_before:.4}, after={cov_after:.4}"
    );
    assert!(
        after
            .warnings
            .iter()
            .any(|w| w.contains("open knowledge gap")),
        "verifier must surface a warning about open gaps; got {:?}",
        after.warnings
    );
}

#[tokio::test]
async fn reflect_and_gaps_are_advertised_in_mcp_tools_list() {
    let resp = thinkingroot_serve::mcp::tools::handle_list(None).await;
    let v = serde_json::to_value(&resp).unwrap();
    let names: Vec<String> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    for expected in ["reflect", "gaps"] {
        assert!(
            names.iter().any(|n| n == expected),
            "MCP tools/list missing '{expected}'"
        );
    }
}
