//! Integration tests for `engine.agent_topology(ws, name)` and
//! `engine.fork_run_branch` / `engine.settle_run_branch`.
//!
//! Proves:
//!   - an agent with `config_json = {"write_target":"per_run","merge_policy":"verified"}`
//!     resolves to `WriteTarget::PerRun` via the inheritance-chain fallback;
//!   - an unknown agent falls back to `AgentTopology::default()`;
//!   - `settle_run_branch` with Auto policy and ok=true merges the run branch;
//!   - `settle_run_branch` with ok=false rolls back (abandons) the branch;
//!   - `settle_run_branch` with Verified policy runs the health_score check.

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_core::{AgentMergePolicy, AgentTopology, WriteTarget};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{AgentClaim, Principal, QueryEngine};
use thinkingroot_serve::intelligence::session::SessionStore;

async fn setup() -> (tempfile::TempDir, PathBuf, QueryEngine) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap();

    let mut engine = QueryEngine::new();
    engine.mount("brain".to_string(), root.clone()).await.unwrap();
    (dir, root, engine)
}

fn mem(stmt: &str) -> AgentClaim {
    AgentClaim {
        statement: stmt.to_string(),
        claim_type: "fact".into(),
        confidence: Some(0.9),
        entities: vec![],
    }
}

#[tokio::test]
async fn agent_topology_resolves_write_target_from_config_json() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";

    // Persist an agent whose config_json declares per_run isolation + verified merge.
    engine
        .put_agent(
            ws,
            "researcher",
            "You are a careful researcher.",
            "",
            r#"{"write_target":"per_run","merge_policy":"verified"}"#,
        )
        .await
        .unwrap();

    let topo = engine.agent_topology(ws, "researcher").await;
    assert_eq!(
        topo.write_target,
        WriteTarget::PerRun,
        "researcher topology must resolve PerRun from config_json"
    );
    assert_eq!(topo.merge_policy, thinkingroot_core::AgentMergePolicy::Verified);
}

#[tokio::test]
async fn agent_topology_defaults_for_unknown_agent() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";

    // No agent persisted — must return the default topology (legacy behavior).
    let topo = engine.agent_topology(ws, "ghost").await;
    assert_eq!(
        topo,
        AgentTopology::default(),
        "unknown agent must resolve to default topology"
    );
}

// ── fork_run_branch / settle_run_branch tests ──────────────────────────────

#[tokio::test]
async fn settle_auto_merges_on_success() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";
    let sessions = SessionStore::default();

    // fork an isolated run branch
    let branch = engine.fork_run_branch(ws, "run-1", None).await.unwrap();
    assert_eq!(branch, "run/run-1");

    // contribute one claim to that branch so the merge has real work
    engine
        .contribute_claims_as(
            ws,
            "sess-run-1",
            Some("run/run-1"),
            vec![mem("run-1 discovered an important fact")],
            &sessions,
            Principal::Agent("run-1".into()),
        )
        .await
        .unwrap();

    // settle with Auto policy and ok=true → must merge
    let report = engine
        .settle_run_branch(ws, &branch, AgentMergePolicy::Auto, true)
        .await
        .unwrap();
    assert!(
        report.merged,
        "Auto+ok=true must merge the run branch into main: {:?}",
        report
    );
    assert!(!report.rolled_back, "must not be rolled back on success");
}

#[tokio::test]
async fn settle_rolls_back_on_failure() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";

    // fork but do NOT contribute — the run failed
    let branch = engine.fork_run_branch(ws, "run-fail", None).await.unwrap();

    // settle with ok=false → must abandon the branch, not merge
    let report = engine
        .settle_run_branch(ws, &branch, AgentMergePolicy::Auto, false)
        .await
        .unwrap();
    assert!(
        report.rolled_back,
        "ok=false must roll back (abandon) the branch: {:?}",
        report
    );
    assert!(!report.merged, "must not be merged on failure");
}

#[tokio::test]
async fn settle_verified_runs_health_gate() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";
    let sessions = SessionStore::default();

    // fork + contribute so the health check has content to evaluate
    let branch = engine.fork_run_branch(ws, "run-v", None).await.unwrap();
    engine
        .contribute_claims_as(
            ws,
            "sess-run-v",
            Some("run/run-v"),
            vec![mem("run-v verified a critical insight")],
            &sessions,
            Principal::Agent("run-v".into()),
        )
        .await
        .unwrap();

    // settle with Verified policy — checks must run regardless of pass/fail
    let report = engine
        .settle_run_branch(ws, &branch, AgentMergePolicy::Verified, true)
        .await
        .unwrap();
    assert!(
        report.checks.iter().any(|(name, _passed, _detail)| name == "health_score"),
        "Verified policy must run the health_score check: {:?}",
        report.checks
    );
    assert!(
        report.merged || !report.note.is_empty(),
        "health gate must either merge or explain why not: {:?}",
        report
    );
}
