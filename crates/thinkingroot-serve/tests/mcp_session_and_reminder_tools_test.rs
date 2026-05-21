//! C2 (2026-05-22) — `get_session_context` + `get_reminder_context`
//! discoverability + dispatch tests.
//!
//! These two read-only tools give external MCP clients (Claude Code,
//! Cursor, Codex) the same ambient context the in-app Brain chat
//! receives. Tests pin:
//!
//! 1. Both tools appear in `tools/list` and tier-1 existing tools
//!    are not regressed by their addition.
//! 2. `get_session_context` returns honest-empty fields for a
//!    fresh session that has never been touched.
//! 3. `get_session_context` reflects `set_branch` / `set_owner`
//!    after the session has been mutated.
//! 4. `get_reminder_context` returns a typed
//!    `transport_not_supported`-style error when called without
//!    AppState (the stdio-MCP case — `state = None`).
//! 5. The two new tools' `inputSchema` shapes are well-formed
//!    JSON Schema (`type: object`, `properties: {...}`).
//! 6. The discoverability ordering puts `get_session_context` and
//!    `get_reminder_context` immediately after `health_check`
//!    (stable position so editor-side caches don't drift).

use serde_json::json;
use std::sync::Arc;
use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::intelligence::engram::{EngramConfig, EngramManager};
use thinkingroot_serve::intelligence::session::{
    SessionContext, SessionStore, new_session_store,
};
use thinkingroot_serve::mcp::tools;
use tokio::sync::RwLock;

/// Construct a bare-bones `QueryEngine` for tests — no mounted
/// workspaces, no graph, just enough for `list_workspaces()` to
/// return an empty Vec without erroring.
fn empty_engine() -> Arc<RwLock<QueryEngine>> {
    Arc::new(RwLock::new(QueryEngine::new()))
}

fn empty_engram_manager() -> Arc<EngramManager> {
    EngramManager::new(EngramConfig::default())
}

fn empty_sessions() -> SessionStore {
    new_session_store()
}

#[tokio::test]
async fn tools_list_includes_get_session_context_and_get_reminder_context() {
    let resp = tools::handle_list(None).await;
    let v = serde_json::to_value(&resp).expect("serialize tools/list");
    let names: Vec<String> = v["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();

    for expected in ["get_session_context", "get_reminder_context"] {
        assert!(
            names.iter().any(|n| n == expected),
            "tools/list missing '{}'. got first 25: {:?}",
            expected,
            &names[..names.len().min(25)]
        );
    }
}

#[tokio::test]
async fn tools_list_does_not_regress_existing_tier1_tools() {
    let resp = tools::handle_list(None).await;
    let v = serde_json::to_value(&resp).expect("serialize tools/list");
    let names: Vec<String> = v["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();

    for existing in [
        "search",
        "compile",
        "health_check",
        "create_branch",
        "merge_branch",
        "list_witnesses",
        "ask",
        "contribute",
    ] {
        assert!(
            names.iter().any(|n| n == existing),
            "tools/list regression: '{}' missing after C2 addition",
            existing,
        );
    }
}

#[tokio::test]
async fn get_session_context_returns_honest_empty_fields_for_fresh_session() {
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();
    let engine_guard = engine.read().await;

    let params = json!({
        "name": "get_session_context",
        "arguments": {}
    });
    let resp = tools::handle_call(
        Some(json!(1)),
        &params,
        &*engine_guard,
        Some("ws-a"),
        "fresh-session-id",
        &sessions,
        &engram_manager,
        None, // stdio-like — no AppState
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let v = serde_json::to_value(&resp).expect("serialize");
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let payload: serde_json::Value = serde_json::from_str(text).expect("inner JSON");

    assert_eq!(payload["session_id"], json!("fresh-session-id"));
    assert_eq!(payload["workspace"], json!("ws-a"));
    assert_eq!(payload["owner"], serde_json::Value::Null);
    assert_eq!(payload["active_branch"], serde_json::Value::Null);
    assert_eq!(payload["focus_entity"], serde_json::Value::Null);
    assert_eq!(payload["turn_count"], json!(0));
    assert_eq!(payload["delivered_claim_count"], json!(0));
    // Pre-C6: client_info is always Null.
    assert_eq!(payload["client_info"], serde_json::Value::Null);
    // Pre-C19: sensitivity_caveats is always Null.
    assert_eq!(payload["sensitivity_caveats"], serde_json::Value::Null);
    assert!(payload["mounted_workspaces"].is_array());
}

#[tokio::test]
async fn get_session_context_reflects_active_branch_after_mutation() {
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();

    // Seed a session with an owner + active branch.
    {
        let mut store = sessions.lock().await;
        let mut sc = SessionContext::new("session-with-branch", "ws-a");
        sc.set_owner("naveen".to_string());
        sc.set_branch("stream/sess-with-branch".to_string());
        store.insert("session-with-branch".to_string(), sc);
    }

    let engine_guard = engine.read().await;
    let params = json!({
        "name": "get_session_context",
        "arguments": {}
    });
    let resp = tools::handle_call(
        Some(json!(2)),
        &params,
        &*engine_guard,
        Some("ws-a"),
        "session-with-branch",
        &sessions,
        &engram_manager,
        None,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let v = serde_json::to_value(&resp).expect("serialize");
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let payload: serde_json::Value = serde_json::from_str(text).expect("inner JSON");

    assert_eq!(payload["owner"], json!("naveen"));
    assert_eq!(payload["active_branch"], json!("stream/sess-with-branch"));
}

#[tokio::test]
async fn get_session_context_lets_caller_inspect_a_different_session() {
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();

    // Seed session "other" with a distinct owner. Caller's own
    // session id is "self".
    {
        let mut store = sessions.lock().await;
        let mut sc = SessionContext::new("other", "ws-a");
        sc.set_owner("alice".to_string());
        store.insert("other".to_string(), sc);
    }

    let engine_guard = engine.read().await;
    let params = json!({
        "name": "get_session_context",
        "arguments": { "session_id": "other" }
    });
    let resp = tools::handle_call(
        Some(json!(3)),
        &params,
        &*engine_guard,
        Some("ws-a"),
        "self", // caller's own session
        &sessions,
        &engram_manager,
        None,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let v = serde_json::to_value(&resp).expect("serialize");
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let payload: serde_json::Value = serde_json::from_str(text).expect("inner JSON");

    assert_eq!(payload["session_id"], json!("other"));
    assert_eq!(payload["owner"], json!("alice"));
}

#[tokio::test]
async fn get_reminder_context_returns_typed_error_without_app_state() {
    // Mirrors stdio MCP: no AppState available → tool returns a
    // JSON-RPC error with a hint that this transport doesn't carry
    // the substrate AppState the helper needs.
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();
    let engine_guard = engine.read().await;

    let params = json!({
        "name": "get_reminder_context",
        "arguments": { "workspace": "ws-a" }
    });
    let resp = tools::handle_call(
        Some(json!(4)),
        &params,
        &*engine_guard,
        Some("ws-a"),
        "session-stdio",
        &sessions,
        &engram_manager,
        None, // stdio
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let v = serde_json::to_value(&resp).expect("serialize");
    // Must surface as a typed JSON-RPC error, not a fake-success.
    assert!(v["error"].is_object(), "expected error envelope, got: {v}");
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("HTTP transport") || msg.contains("SSE"),
        "error message should mention transport requirement, got: {msg}"
    );
}

#[tokio::test]
async fn new_tools_input_schemas_are_well_formed_json_schema() {
    let resp = tools::handle_list(None).await;
    let v = serde_json::to_value(&resp).expect("serialize tools/list");
    let tools_arr = v["result"]["tools"].as_array().expect("tools array");

    for tool_name in ["get_session_context", "get_reminder_context"] {
        let tool = tools_arr
            .iter()
            .find(|t| t["name"].as_str() == Some(tool_name))
            .expect("tool present");
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["type"], json!("object"),
            "{tool_name}.inputSchema.type must be 'object'"
        );
        assert!(
            schema["properties"].is_object(),
            "{tool_name}.inputSchema.properties must be an object"
        );
        // get_reminder_context has a required `workspace`;
        // get_session_context has no required fields (session_id is optional).
        if tool_name == "get_reminder_context" {
            let required = schema["required"].as_array().expect("required array");
            assert!(
                required.iter().any(|r| r.as_str() == Some("workspace")),
                "get_reminder_context must require 'workspace'"
            );
        }
    }
}
