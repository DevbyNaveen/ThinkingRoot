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
    for expected in ["reflect", "gaps", "dismiss_gap"] {
        assert!(
            names.iter().any(|n| n == expected),
            "MCP tools/list missing '{expected}'"
        );
    }
}

#[tokio::test]
async fn reflect_branched_runs_against_branch_graph() {
    let (_dir, root) = setup_ws_with_pattern().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root.clone()).await.unwrap();

    // Reflect on main — establishes 3 gaps.
    let main_result = engine.reflect_branched("demo", None).await.unwrap();
    assert_eq!(main_result.open_gaps_total, 3);

    // Create a branch. The branch's graph.db inherits main's gaps +
    // pattern state because snapshot::create_branch_layout copies it.
    thinkingroot_branch::create_branch(&root, "feat/fill-gaps", "main", None)
        .await
        .unwrap();

    // Branch-scoped list should see the same 3 gaps (inherited at fork).
    let branch_gaps = engine
        .list_gaps_branched("demo", None, 0.0, Some("feat/fill-gaps"))
        .await
        .unwrap();
    assert_eq!(
        branch_gaps.len(),
        3,
        "branch inherits main's gaps at fork time"
    );

    // Reflect on the branch without adding anything — still 3 gaps.
    let branch_result = engine
        .reflect_branched("demo", Some("feat/fill-gaps"))
        .await
        .unwrap();
    assert_eq!(branch_result.open_gaps_total, 3);
    assert_eq!(
        branch_result.gaps_created, 0,
        "branch reflect should re-discover same gaps (already present)"
    );

    // Main's state must be untouched (no cross-contamination).
    let main_gaps = engine.list_gaps("demo", None, 0.0).await.unwrap();
    assert_eq!(main_gaps.len(), 3);
}

#[tokio::test]
async fn gap_report_artifact_renders_with_patterns_and_gaps() {
    let (_dir, root) = setup_ws_with_pattern().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    // Empty state — no reflect yet. Artifact should render gracefully.
    let pre = engine.get_artifact("demo", "gap-report").await.unwrap();
    assert_eq!(pre.artifact_type, "gap-report");
    assert!(
        pre.content.contains("No patterns discovered yet"),
        "pre-reflect report must note absence of patterns; got:\n{}",
        pre.content
    );

    engine.reflect("demo").await.unwrap();

    // Post-reflect — patterns + gaps section populated.
    let post = engine.get_artifact("demo", "gap-report").await.unwrap();
    assert!(
        post.content.contains("| Service |"),
        "patterns table must include Service row; got:\n{}",
        post.content
    );
    assert!(
        post.content.contains("`ApiSignature`"),
        "patterns table must mention the condition claim type"
    );
    assert!(
        post.content.contains("`Requirement`"),
        "patterns table must mention the expected claim type"
    );
    assert!(
        post.content.contains("Open Gaps (3)"),
        "open gap count must appear in header"
    );
    assert!(
        post.content.contains("**GapSvc37**")
            && post.content.contains("**GapSvc38**")
            && post.content.contains("**GapSvc39**"),
        "all three gap entities must appear in the report"
    );
    assert!(
        post.content.contains("dismiss_gap"),
        "report must explain the dismiss workflow"
    );
}

#[tokio::test]
async fn gap_report_advertised_in_list_artifacts_as_available() {
    let (_dir, root) = setup_ws_with_pattern().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();

    let artifacts = engine.list_artifacts("demo").await.unwrap();
    let gap = artifacts
        .iter()
        .find(|a| a.artifact_type == "gap-report")
        .expect("gap-report must be advertised in list_artifacts");
    assert!(
        gap.available,
        "gap-report is dynamic — should always report available=true"
    );
}

#[tokio::test]
async fn dismiss_gap_via_engine_suppresses_gap() {
    let (dir, root) = setup_ws_with_pattern().await;
    let mut engine = QueryEngine::new();
    engine.mount("demo".to_string(), root).await.unwrap();
    engine.reflect("demo").await.unwrap();

    let gaps = engine.list_gaps("demo", None, 0.0).await.unwrap();
    assert_eq!(gaps.len(), 3);

    // Gap ids aren't exposed on `GapReport` yet — read one directly
    // from known_unknowns. In production, the `gaps` MCP payload
    // already carries the full serialized struct including ids.
    let gap_id = {
        let graph_dir = dir.path().join(".thinkingroot").join("graph");
        let g = GraphStore::init(&graph_dir).unwrap();
        let all = g.reflect_load_known_unknowns().unwrap();
        assert_eq!(all.len(), 3);
        all[0].0.clone()
    };

    engine.dismiss_gap("demo", &gap_id, None).await.unwrap();

    let after = engine.list_gaps("demo", None, 0.0).await.unwrap();
    assert_eq!(
        after.len(),
        2,
        "dismissed gap must be excluded from list_gaps"
    );

    // Re-running reflect must not re-raise the dismissed gap.
    let r2 = engine.reflect("demo").await.unwrap();
    assert_eq!(
        r2.gaps_created, 0,
        "reflect must respect prior dismissal"
    );
    let after_r2 = engine.list_gaps("demo", None, 0.0).await.unwrap();
    assert_eq!(after_r2.len(), 2);
}
