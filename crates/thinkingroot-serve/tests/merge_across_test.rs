//! Integration tests for `engine.merge_across_workspaces` — the cross-brain
//! verified merge primitive (Phase 2 of Agent State Topology).
//!
//! Proves:
//!   - claims contributed to a branch in a SOURCE workspace flow into the
//!     TARGET workspace through `merge_across_workspaces`;
//!   - the operation is idempotent (a second merge with the same source adds 0
//!     new claims);
//!   - the returned report exposes `merge_allowed` (the health gate was
//!     consulted).

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{AgentClaim, ClaimFilter, MergeAcrossReport, Principal, QueryEngine};
use thinkingroot_serve::intelligence::session::SessionStore;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Build a fresh single-workspace engine with a `main` brain at a temp path.
async fn setup_ws(engine: &mut QueryEngine, name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap();
    engine.mount(name.to_string(), root.clone()).await.unwrap();
    (dir, root)
}

fn claim(stmt: &str) -> AgentClaim {
    AgentClaim {
        statement: stmt.to_string(),
        claim_type: "fact".into(),
        confidence: Some(0.9),
        entities: vec![],
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// A claim on a branch in `agent_src` must arrive in `main` after a
/// cross-brain merge.
#[tokio::test]
async fn merge_across_moves_claim_between_brains() {
    let mut engine = QueryEngine::new();
    let sessions = SessionStore::default();

    // Mount two independent workspaces, each with their own tempdir.
    let (_dir_src, _root_src) = setup_ws(&mut engine, "agent_src").await;
    let (_dir_tgt, _root_tgt) = setup_ws(&mut engine, "main").await;

    // Fork a run branch on the source workspace and contribute a distinctive claim.
    // The run_id becomes the branch owner, so the agent principal must match it.
    let run_id = "xb-run-1";
    let branch = engine
        .fork_run_branch("agent_src", run_id, None)
        .await
        .unwrap();

    engine
        .contribute_claims_as(
            "agent_src",
            "sess-xb-1",
            Some(&branch),
            vec![claim("CrossBrainProof alpha-9981")],
            &sessions,
            Principal::Agent(run_id.into()),
        )
        .await
        .unwrap();

    // ── cross-brain merge ────────────────────────────────────────────────────
    let report: MergeAcrossReport = engine
        .merge_across_workspaces("agent_src", &branch, "main", None)
        .await
        .unwrap();

    assert!(
        report.merged,
        "cross-brain merge must succeed: {:?}",
        report
    );
    assert!(
        report.merged_claims >= 1,
        "at least one claim must have been merged; report: {:?}",
        report
    );

    // The marker must now be readable in the TARGET brain.
    let claims = engine
        .list_claims_branched("main", ClaimFilter::default(), None)
        .await
        .unwrap();
    let found = claims
        .iter()
        .any(|c| c.statement.contains("CrossBrainProof alpha-9981"));
    assert!(
        found,
        "marker claim must be readable in target 'main' after merge; found: {:#?}",
        claims
            .iter()
            .map(|c| &c.statement)
            .collect::<Vec<_>>()
    );
}

/// Running the same cross-brain merge twice must be idempotent: the second
/// call must report 0 new merged claims, and the target must contain exactly
/// one copy of the marker.
#[tokio::test]
async fn merge_across_is_idempotent() {
    let mut engine = QueryEngine::new();
    let sessions = SessionStore::default();

    let (_dir_src, _root_src) = setup_ws(&mut engine, "agent_src2").await;
    let (_dir_tgt, _root_tgt) = setup_ws(&mut engine, "main2").await;

    let run_id2 = "xb-run-2";
    let branch = engine
        .fork_run_branch("agent_src2", run_id2, None)
        .await
        .unwrap();

    engine
        .contribute_claims_as(
            "agent_src2",
            "sess-xb-2",
            Some(&branch),
            vec![claim("CrossBrainProof beta-7742")],
            &sessions,
            Principal::Agent(run_id2.into()),
        )
        .await
        .unwrap();

    // First merge — should succeed.
    let r1: MergeAcrossReport = engine
        .merge_across_workspaces("agent_src2", &branch, "main2", None)
        .await
        .unwrap();
    assert!(r1.merged, "first merge must succeed: {:?}", r1);
    assert!(r1.merged_claims >= 1);

    // Second merge — must be a no-op (all claims already present).
    let r2: MergeAcrossReport = engine
        .merge_across_workspaces("agent_src2", &branch, "main2", None)
        .await
        .unwrap();
    assert_eq!(
        r2.merged_claims, 0,
        "second merge must add 0 claims (idempotent); report: {:?}",
        r2
    );

    // Exactly one copy of the marker must exist in target.
    let claims = engine
        .list_claims_branched("main2", ClaimFilter::default(), None)
        .await
        .unwrap();
    let count = claims
        .iter()
        .filter(|c| c.statement.contains("CrossBrainProof beta-7742"))
        .count();
    assert_eq!(
        count, 1,
        "target must contain exactly one copy of the marker; found {count}"
    );
}

/// The returned report must expose `merge_allowed` proving the health gate
/// was consulted even when no claims are blocked.
#[tokio::test]
async fn merge_across_reports_gate() {
    let mut engine = QueryEngine::new();
    let sessions = SessionStore::default();

    let (_dir_src, _root_src) = setup_ws(&mut engine, "agent_src3").await;
    let (_dir_tgt, _root_tgt) = setup_ws(&mut engine, "main3").await;

    let run_id3 = "xb-run-3";
    let branch = engine
        .fork_run_branch("agent_src3", run_id3, None)
        .await
        .unwrap();

    engine
        .contribute_claims_as(
            "agent_src3",
            "sess-xb-3",
            Some(&branch),
            vec![claim("CrossBrainProof gamma-5513")],
            &sessions,
            Principal::Agent(run_id3.into()),
        )
        .await
        .unwrap();

    let report: MergeAcrossReport = engine
        .merge_across_workspaces("agent_src3", &branch, "main3", None)
        .await
        .unwrap();

    // The field must be present (health gate was consulted).
    assert!(
        report.merge_allowed,
        "merge_allowed must be true for a healthy merge; report: {:?}",
        report
    );
}
