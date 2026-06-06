//! M2 — co-located `ctx.prompt` + `ctx.branch` for Root Functions.
//!
//! A function can assemble a compiled, versioned prompt and fork/merge an
//! isolated branch of its own cognition graph, in-process. All assertions are
//! model-independent (prompt assembly + branch ops are graph/fs only).
//! Requires `--features root-functions`.

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

/// ctx.prompt assembles a versioned template with the function's variables.
#[tokio::test]
async fn ctx_prompt_assembles_compiled_template() {
    let (engine, _root, _dir) = engine_with_ws("acme").await;
    engine
        .prompt_put_template("acme", "greet", "Hello {{name}}, welcome to {{place}}.")
        .await
        .unwrap();

    const F: &str = r#"
        async (i, ctx) => ({
          msg: await ctx.prompt("greet", { name: i.who, place: "ThinkingRoot" })
        })
    "#;
    engine.put_function("acme", "greeter", F, "js").await.unwrap();

    let out = engine
        .invoke_function("acme", "greeter", &serde_json::json!({ "who": "Ada" }))
        .await
        .unwrap();
    assert_eq!(
        out["msg"],
        serde_json::json!("Hello Ada, welcome to ThinkingRoot."),
        "ctx.prompt must assemble the template with the function's vars: {out}"
    );
}

/// ctx.branch.fork creates an isolated branch and is idempotent on replay.
#[tokio::test]
async fn ctx_branch_fork_creates_and_is_idempotent() {
    let (engine, root, _dir) = engine_with_ws("acme").await;

    const F: &str = r#"async (i, ctx) => await ctx.branch.fork(i.name)"#;
    engine.put_function("acme", "forker", F, "js").await.unwrap();

    let input = serde_json::json!({ "name": "exp/aggressive-emails" });
    let out1 = engine
        .run_function_with_id("acme", "forker", &input, "run_fork_1")
        .await
        .unwrap();
    assert_eq!(out1["name"], serde_json::json!("exp/aggressive-emails"));
    assert_eq!(out1["parent"], serde_json::json!("main"));

    // The branch really exists on disk.
    let branches = thinkingroot_branch::list_branches(&root).unwrap();
    let count = branches
        .iter()
        .filter(|b| b.name == "exp/aggressive-emails")
        .count();
    assert_eq!(count, 1, "exactly one branch created");

    // Replay of the SAME run returns the journaled result; no second branch.
    let out2 = engine
        .run_function_with_id("acme", "forker", &input, "run_fork_1")
        .await
        .unwrap();
    assert_eq!(out1["name"], out2["name"], "fork must be replay-stable");
    let branches2 = thinkingroot_branch::list_branches(&root).unwrap();
    assert_eq!(
        branches2
            .iter()
            .filter(|b| b.name == "exp/aggressive-emails")
            .count(),
        1,
        "replay must NOT create a duplicate branch"
    );
}

/// ctx.branch.merge merges a forked branch back into main and returns a
/// summary (empty branch → zero new claims, nothing needing review).
#[tokio::test]
async fn ctx_branch_merge_returns_summary() {
    let (engine, _root, _dir) = engine_with_ws("acme").await;

    const F: &str = r#"
        async (i, ctx) => {
          await ctx.branch.fork("exp/x");
          return await ctx.branch.merge("exp/x", "main");
        }
    "#;
    engine.put_function("acme", "experiment", F, "js").await.unwrap();

    let out = engine
        .invoke_function("acme", "experiment", &serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out["to_branch"], serde_json::json!("main"), "merged into main: {out}");
    assert_eq!(out["from_branch"], serde_json::json!("exp/x"));
    assert!(out["new_claims"].is_number(), "merge summary carries counts: {out}");
}
