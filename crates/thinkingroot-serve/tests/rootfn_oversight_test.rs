//! Subagent oversight — `ctx.agents()` roster + `ctx.putAgent()` self-creation
//! provenance + run attribution (`invoked_by`).
//!
//! Proves, model-independently (no ONNX embedder — pure graph writes/reads):
//!   1. `ctx.putAgent` creates a sub-agent at runtime AND stamps provenance
//!      (`created_by` / `parent_agent`) from the ACTING agent, derived from the
//!      run's scope — the function body cannot forge it.
//!   2. `ctx.agents()` is wired into the isolate and returns the roster with
//!      that provenance plus recent runs (incl. `invoked_by`).
//!   3. An agent-scoped run is attributed (`invoked_by` = the acting agent),
//!      while a `main` run is not (back-compatible `None`).
//!
//! Requires `--features root-functions` (the isolate must actually run).

use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_serve::engine::QueryEngine;

async fn engine_with_ws(ws: &str) -> (QueryEngine, PathBuf, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    GraphStore::init(&graph_dir).unwrap();
    let mut engine = QueryEngine::new();
    engine.mount(ws.to_string(), root.clone()).await.unwrap();
    (engine, root, dir)
}

/// ctx.putAgent stamps provenance from the acting agent (derived from the
/// scope), and ctx.agents() reads it back — the oversight roster.
#[tokio::test]
async fn put_agent_stamps_provenance_and_agents_roster_reads_it() {
    // The run executes in the agent's own brain `agent_mrguy`, so the acting
    // agent derived from the scope is "mrguy".
    let (engine, _root, _dir) = engine_with_ws("agent_mrguy").await;

    // A function MrGuy runs to spin up a research sub-agent, then list the roster.
    const OVERSEE: &str = r#"
        async (i, ctx) => {
          await ctx.putAgent({
            name: i.name,
            persona: "You research things.",
            config: { recall_k: 5 },
          });
          const roster = await ctx.agents({ recentRuns: 3 });
          return { roster };
        }
    "#;
    engine.put_function("agent_mrguy", "oversee", OVERSEE, "js").await.unwrap();

    let out = engine
        .invoke_function(
            "agent_mrguy",
            "oversee",
            &serde_json::json!({ "name": "researcher" }),
        )
        .await
        .expect("ctx.putAgent + ctx.agents must be reached + bound");

    let roster = out["roster"].as_array().expect("roster must be an array");
    let researcher = roster
        .iter()
        .find(|a| a["name"] == "researcher")
        .expect("the created sub-agent must appear in the roster");
    // Provenance was stamped from the acting agent — not forgeable by the body.
    assert_eq!(researcher["created_by"], "mrguy", "created_by = the acting agent");
    assert_eq!(researcher["parent_agent"], "mrguy", "parent_agent = the acting agent");
    assert!(researcher["persona"].is_string(), "persona surfaced in the roster");

    // And the stored agent carries the same provenance (read via the normal path).
    let stored = engine.get_agent("agent_mrguy", "researcher").await.unwrap().unwrap();
    assert_eq!(stored.created_by.as_deref(), Some("mrguy"));
    assert_eq!(stored.parent_agent.as_deref(), Some("mrguy"));
}

/// An agent-scoped run is attributed (`invoked_by`); a `main` run is not.
#[tokio::test]
async fn agent_scoped_run_is_attributed_main_run_is_not() {
    // Agent scope → attributed to the agent.
    let (engine_a, _r, _d) = engine_with_ws("agent_mrguy").await;
    const NOOP: &str = r#"async (i, ctx) => ({ ok: true })"#;
    engine_a.put_function("agent_mrguy", "noop", NOOP, "js").await.unwrap();
    engine_a
        .invoke_function("agent_mrguy", "noop", &serde_json::json!({}))
        .await
        .unwrap();
    let runs = engine_a.list_function_runs("agent_mrguy", "noop").await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].invoked_by.as_deref(),
        Some("mrguy"),
        "an agent-scoped run must be attributed to its acting agent"
    );

    // A plain `main` run carries no attribution (back-compatible None).
    let (engine_m, _r2, _d2) = engine_with_ws("main").await;
    engine_m.put_function("main", "noop", NOOP, "js").await.unwrap();
    engine_m.invoke_function("main", "noop", &serde_json::json!({})).await.unwrap();
    let main_runs = engine_m.list_function_runs("main", "noop").await.unwrap();
    assert_eq!(main_runs.len(), 1);
    assert_eq!(
        main_runs[0].invoked_by, None,
        "a non-agent run must NOT be attributed (legacy behavior)"
    );
}

/// A composite `u_<id>__agent_<name>` scope attributes to the agent (the same
/// derivation the connector/topology code uses).
#[tokio::test]
async fn composite_user_agent_scope_attributes_to_the_agent() {
    let (engine, _r, _d) = engine_with_ws("u_alice__agent_helper").await;
    const NOOP: &str = r#"async (i, ctx) => ({ ok: true })"#;
    engine
        .put_function("u_alice__agent_helper", "noop", NOOP, "js")
        .await
        .unwrap();
    engine
        .invoke_function("u_alice__agent_helper", "noop", &serde_json::json!({}))
        .await
        .unwrap();
    let runs = engine
        .list_function_runs("u_alice__agent_helper", "noop")
        .await
        .unwrap();
    assert_eq!(runs[0].invoked_by.as_deref(), Some("helper"));
}
