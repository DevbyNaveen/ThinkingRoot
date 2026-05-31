//! Branches as graph nodes + the multi-agent branch-brain lifecycle.
//!
//! Proves on a real engine:
//!   - durable branches sync as `active` nodes; ephemeral `stream/*` do NOT;
//!   - the capsule carries branch-context ("you're on X, parent main");
//!   - spawn agent = fork branch-brain → work on it → gated merge-back flips the
//!     node `active → merged` and lands the agent's claims in the shared brain.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::{
    AgentClaim, CapsuleSpec, ClaimFilter, Principal, QueryEngine,
};
use thinkingroot_serve::intelligence::session::SessionStore;

fn mem(stmt: &str) -> AgentClaim {
    AgentClaim {
        statement: stmt.to_string(),
        claim_type: "fact".into(),
        confidence: Some(0.9),
        entities: vec![],
    }
}

async fn setup() -> (tempfile::TempDir, PathBuf, QueryEngine) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap();

    let mut engine = QueryEngine::new();
    engine.mount("brain".to_string(), root.clone()).await.unwrap();
    // A compiled prompt so the capsule has a system frame to compile.
    engine
        .prompt_put_template("brain", "assistant", "You are the {{org}} agent.")
        .await
        .unwrap();
    (dir, root, engine)
}

#[tokio::test]
async fn agent_branch_lifecycle_and_capsule_context() {
    let (_d, _root, engine) = setup().await;
    let ws = "brain";
    let sessions = SessionStore::default();

    // ── spawn agent = fork its branch-brain → durable node, active ──────
    let branch = engine.spawn_agent_branch(ws, "alice", None).await.unwrap();
    assert_eq!(branch, "agent/alice");
    let n = engine
        .list_branch_nodes(ws)
        .await
        .unwrap()
        .into_iter()
        .find(|b| b.name == "agent/alice")
        .expect("agent branch node exists");
    assert_eq!(n.status, "active");
    assert_eq!(n.parent.as_deref(), Some("main"));

    // ── agent works on its own branch ──────────────────────────────────
    engine
        .contribute_claims_as(
            ws,
            "sess-alice",
            Some("agent/alice"),
            vec![mem("Alice discovered the retry bug")],
            &sessions,
            Principal::Agent("alice".into()),
        )
        .await
        .unwrap();

    // ── capsule on the agent branch carries branch-context ─────────────
    let cap = engine
        .compile_capsule(
            ws,
            CapsuleSpec {
                prompt_name: "assistant".into(),
                vars: BTreeMap::from([("org".to_string(), "Acme".to_string())]),
                query: "what did I find?".into(),
                branch: Some("agent/alice".into()),
                top_k: 5,
                max_tools: 3,
                session_id: None,
            },
        )
        .await
        .unwrap();
    let bc = cap.branch_context.expect("capsule carries branch context");
    assert_eq!(bc.name, "agent/alice");
    assert_eq!(bc.status, "active");
    assert_eq!(bc.parent.as_deref(), Some("main"));

    // ── finish agent = gated merge-back (verify-before-merge) ──────────
    let report = engine.finish_agent_branch(ws, "alice", 0, true).await.unwrap();
    assert!(
        report.checks.iter().any(|(n, p, _)| n == "health_score" && *p),
        "health_score must pass: {:?}",
        report.checks
    );
    assert!(report.merged, "agent work must merge once checks pass: {}", report.note);

    // node flipped active → merged (honesty rule)
    let n = engine
        .list_branch_nodes(ws)
        .await
        .unwrap()
        .into_iter()
        .find(|b| b.name == "agent/alice")
        .unwrap();
    assert_eq!(n.status, "merged");
    assert!(n.merged_at.is_some());

    // the agent's claim is now in the shared brain (main)
    let main_claims = engine.list_claims(ws, ClaimFilter::default()).await.unwrap();
    assert!(
        main_claims.iter().any(|c| c.statement.contains("Alice discovered the retry bug")),
        "agent claim must land in main after merge"
    );
}

#[tokio::test]
async fn ephemeral_stream_branches_are_not_node_ified() {
    let (_d, root, engine) = setup().await;
    let ws = "brain";

    // A durable topic branch + an ephemeral stream branch, both synced.
    thinkingroot_branch::create_branch(&root, "topic/auth", "main", None)
        .await
        .unwrap();
    engine
        .sync_branch_created(&root, "topic/auth", Some("main"), Some("topic"), 1.0)
        .await
        .unwrap();
    thinkingroot_branch::create_branch(&root, "stream/s1", "main", None)
        .await
        .unwrap();
    engine
        .sync_branch_created(&root, "stream/s1", Some("main"), Some("stream"), 1.0)
        .await
        .unwrap();

    let names: Vec<String> = engine
        .list_branch_nodes(ws)
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert!(names.contains(&"topic/auth".to_string()), "durable branch node present");
    assert!(
        !names.iter().any(|n| n == "stream/s1"),
        "ephemeral stream branch must NOT be node-ified: {names:?}"
    );
}
