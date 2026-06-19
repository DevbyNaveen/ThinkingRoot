//! Integration test for `engine.agent_topology(ws, name)`.
//!
//! Proves:
//!   - an agent with `config_json = {"write_target":"per_run","merge_policy":"verified"}`
//!     resolves to `WriteTarget::PerRun` via the inheritance-chain fallback;
//!   - an unknown agent falls back to `AgentTopology::default()`.

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_core::{AgentTopology, WriteTarget};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

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
