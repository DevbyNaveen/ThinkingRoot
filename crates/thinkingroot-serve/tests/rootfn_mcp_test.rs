//! M3 — two-way MCP for Root Functions.
//!
//!  * A deployed Root Function is advertised as an MCP tool (`function::<name>`)
//!    in a workspace-scoped `tools/list` — so any agent can call it.
//!  * `ctx.mcp.call(tool, args)` reaches the external-MCP registry, gated to
//!    project-configured servers (an unknown tool errors honestly).
//!
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

/// A deployed function is advertised as `function::<name>` in the
/// workspace-scoped tools/list (so an agent can invoke it over MCP).
#[tokio::test]
async fn deployed_function_is_advertised_as_mcp_tool() {
    let (engine, _root, _dir) = engine_with_ws("acme").await;
    engine
        .put_function("acme", "double", "(i, ctx) => i.n * 2", "js")
        .await
        .unwrap();

    let resp =
        thinkingroot_serve::mcp::tools::handle_list_for_ws(None, &engine, Some("acme")).await;
    let result = resp.result.expect("tools/list returns a result");
    let tools = result["tools"].as_array().expect("tools is an array");

    let found = tools
        .iter()
        .any(|t| t["name"].as_str() == Some("function::double"));
    assert!(
        found,
        "deployed function must be advertised as 'function::double' in tools/list"
    );

    // The bare (context-free) catalog does NOT carry per-workspace functions.
    let bare = thinkingroot_serve::mcp::tools::handle_list(None).await;
    let bare_tools = bare.result.unwrap();
    let bare_arr = bare_tools["tools"].as_array().unwrap();
    assert!(
        !bare_arr
            .iter()
            .any(|t| t["name"].as_str() == Some("function::double")),
        "context-free catalog must not include workspace functions"
    );
}

/// ctx.mcp.call is wired and gated: calling a tool from a server the project
/// has NOT configured returns an honest "not found" error (never a silent
/// success, never a "ctx.mcp is unavailable" wiring failure).
#[tokio::test]
async fn ctx_mcp_call_unknown_tool_errors_honestly() {
    let (engine, _root, _dir) = engine_with_ws("acme").await;
    const F: &str = r#"async (i, ctx) => await ctx.mcp.call("ghost::send", { to: "x" })"#;
    engine.put_function("acme", "caller", F, "js").await.unwrap();

    let res = engine
        .invoke_function("acme", "caller", &serde_json::json!({}))
        .await;
    let err = res.expect_err("calling an unconfigured MCP tool must error");
    let msg = err.to_string();
    assert!(
        !msg.contains("ctx.mcp is unavailable"),
        "ctx.mcp must be wired into the isolate, got: {msg}"
    );
    assert!(
        msg.contains("not found"),
        "expected an honest registry 'not found' error, got: {msg}"
    );
}

/// A1 — stored per-function capability grants gate the invoke path. The same
/// function flips from "reaches the MCP registry" (unrestricted default) to
/// an honest capability denial once a stored grant omits `can_mcp` — proving
/// the invoke site loads the stored CapSet and that a grant can only narrow.
#[tokio::test]
async fn stored_capset_gates_the_invoke_path() {
    let (engine, _root, _dir) = engine_with_ws("acme").await;
    const F: &str = r#"async (i, ctx) => await ctx.mcp.call("ghost::send", { to: "x" })"#;
    engine.put_function("acme", "caller", F, "js").await.unwrap();

    // Unrestricted default: the call passes the cap gate and dies at the
    // registry with the honest "not found".
    let err = engine
        .invoke_function("acme", "caller", &serde_json::json!({}))
        .await
        .expect_err("unknown tool must error");
    assert!(
        err.to_string().contains("not found"),
        "default grant must reach the registry, got: {err}"
    );

    // Narrow the grant: everything EXCEPT mcp (omitted → deny by contract).
    let caps = thinkingroot_serve::engine::CapSet::from_json(
        r#"{"can_recall":true,"can_remember":true,"can_prompt":true,"can_branch":true,"can_acquire":true}"#,
    )
    .expect("valid grant document");
    engine.set_function_caps("acme", "caller", caps).await.unwrap();

    let err = engine
        .invoke_function("acme", "caller", &serde_json::json!({}))
        .await
        .expect_err("denied capability must error");
    let msg = err.to_string();
    assert!(
        msg.contains("not granted"),
        "expected a capability denial after narrowing the grant, got: {msg}"
    );

    // Granting caps to an undeployed function is rejected (typo guard).
    assert!(
        engine.set_function_caps("acme", "ghost-fn", caps).await.is_err(),
        "caps on a non-deployed function must be rejected"
    );
}

/// A6 — a failed verification verdict corrects the router's learned
/// experience. A function that COMPLETES but answers wrong is over-credited
/// by the invoke path (status 'ok' → positive evidence); recording the
/// failed verdict applies the missing negative bump, and the verdict row
/// persists durably for the trainer.
#[tokio::test]
async fn failed_verdict_corrects_router_experience() {
    let (engine, root, _dir) = engine_with_ws("acme").await;
    // Completes fine — but (per the test expectation) the answer is WRONG.
    engine
        .put_function("acme", "adder", "(i, ctx) => i.a + i.a", "js")
        .await
        .unwrap();

    let input = serde_json::json!({ "a": 2, "b": 3 });
    // Invoke completes → the run path credits (class, adder) with a success.
    engine.invoke_function("acme", "adder", &input).await.unwrap();

    // The verify layer catches the wrong answer and reports the verdict.
    engine
        .record_function_verdict("acme", "adder", &input, false, "expected 5, got 4")
        .await
        .unwrap();

    // The durable verdict row exists, and the experience now carries the
    // corrective failure alongside the invoke's completion-success.
    let graph_dir = root.join(".thinkingroot").join("graph");
    let graph = GraphStore::init(&graph_dir).unwrap();
    let verdicts = graph.list_verify_verdicts("adder", 10).unwrap();
    assert_eq!(verdicts.len(), 1, "verdict must persist");
    assert!(!verdicts[0].2);
    assert_eq!(verdicts[0].3, "expected 5, got 4");

    let report = engine.capability_routing_report("acme").await.unwrap();
    let adder = report["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "adder")
        .cloned()
        .expect("adder appears in the routing report");
    let class_row = adder["classes"]
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .expect("experience row exists for the invoke's input class");
    assert_eq!(class_row["n_success"], 1, "invoke completion credited once");
    assert_eq!(
        class_row["n_fail"], 1,
        "failed verdict must add the corrective negative"
    );

    // Verdicts on an undeployed function are rejected.
    assert!(
        engine
            .record_function_verdict("acme", "ghost", &input, true, "")
            .await
            .is_err()
    );
}

/// Self-extension: a running function deploys a NEW function at runtime via
/// `ctx.acquire` (supplied body — model-independent), and that function is then
/// invocable and correct. This is the engine-side spine for JIT/self-improving
/// agents — a function grows the brain a new capability mid-run.
#[tokio::test]
async fn ctx_acquire_deploys_a_new_function_at_runtime() {
    let (engine, _root, _dir) = engine_with_ws("acme").await;
    const F: &str = r#"async (input, ctx) => {
        const r = await ctx.acquire({
            name: "doubler",
            body: "async (input, ctx) => ({ doubled: (input.n || 0) * 2 })"
        });
        return { acquired: r.name, authored: r.authored };
    }"#;
    engine.put_function("acme", "extender", F, "js").await.unwrap();

    let res = engine
        .invoke_function("acme", "extender", &serde_json::json!({}))
        .await
        .expect("the extender function runs and acquires a capability");
    assert_eq!(res["acquired"], "doubler");
    assert_eq!(res["authored"], false, "supplied body must not be LLM-authored");

    // The acquired capability now exists in the brain and runs correctly.
    let out = engine
        .invoke_function("acme", "doubler", &serde_json::json!({ "n": 21 }))
        .await
        .expect("the runtime-acquired function is now invocable");
    assert_eq!(out["doubled"], 42, "the function a function deployed must work");
}
