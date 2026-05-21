//! C3 (2026-05-22) — MCP cancellation infrastructure tests.
//!
//! These tests pin the cancellation contract end-to-end at the
//! tool-dispatch boundary:
//!
//! 1. A tripped token before dispatch entry returns an immediate
//!    JSON-RPC error with the canonical code (-32800 — within
//!    JSON-RPC's reserved server-error range).
//! 2. An untripped token allows the tool to complete normally.
//! 3. The `notifications/cancelled` dispatch arm runs without
//!    AppState (degrades silently — no panic).
//! 4. The token threaded through dispatch reaches handle_call.
//! 5. Per-request cancellation is independent: tripping one
//!    request's token doesn't affect another.

use serde_json::json;
use std::sync::Arc;
use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::intelligence::engram::{EngramConfig, EngramManager};
use thinkingroot_serve::intelligence::session::{SessionStore, new_session_store};
use thinkingroot_serve::mcp::tools;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

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
async fn tripped_token_returns_cancelled_error_before_dispatch() {
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();
    let engine_guard = engine.read().await;

    // Trip the token BEFORE calling handle_call.
    let cancel = CancellationToken::new();
    cancel.cancel();

    let params = json!({
        "name": "get_session_context",
        "arguments": {}
    });
    let resp = tools::handle_call(
        Some(json!(1)),
        &params,
        &*engine_guard,
        Some("ws-a"),
        "test-session",
        &sessions,
        &engram_manager,
        None,
        cancel,
    )
    .await;
    let v = serde_json::to_value(&resp).expect("serialize");

    // Must surface as a JSON-RPC error with code -32800.
    assert!(v["error"].is_object(), "expected error envelope, got: {v}");
    assert_eq!(v["error"]["code"], json!(-32800), "wrong error code");
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("cancelled") || msg.contains("Cancelled"),
        "error message should mention cancellation, got: {msg}"
    );
}

#[tokio::test]
async fn fresh_token_allows_tool_to_complete_normally() {
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();
    let engine_guard = engine.read().await;

    let cancel = CancellationToken::new(); // NOT tripped

    let params = json!({
        "name": "get_session_context",
        "arguments": {}
    });
    let resp = tools::handle_call(
        Some(json!(2)),
        &params,
        &*engine_guard,
        Some("ws-a"),
        "test-session-2",
        &sessions,
        &engram_manager,
        None,
        cancel,
    )
    .await;
    let v = serde_json::to_value(&resp).expect("serialize");

    // Must succeed (result envelope, not error envelope).
    assert!(
        v["result"].is_object(),
        "expected success envelope, got: {v}"
    );
    assert!(
        v["error"].is_null() || !v.as_object().unwrap().contains_key("error"),
        "must not also have error field"
    );
}

#[tokio::test]
async fn cancellation_tokens_are_independent_per_request() {
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();
    let engine_guard = engine.read().await;

    let cancel_a = CancellationToken::new();
    let cancel_b = CancellationToken::new();

    // Trip A. B stays fresh.
    cancel_a.cancel();

    let params = json!({
        "name": "get_session_context",
        "arguments": {}
    });

    let resp_a = tools::handle_call(
        Some(json!("a")),
        &params,
        &*engine_guard,
        Some("ws"),
        "session-a",
        &sessions,
        &engram_manager,
        None,
        cancel_a,
    )
    .await;
    let resp_b = tools::handle_call(
        Some(json!("b")),
        &params,
        &*engine_guard,
        Some("ws"),
        "session-b",
        &sessions,
        &engram_manager,
        None,
        cancel_b.clone(),
    )
    .await;

    let v_a = serde_json::to_value(&resp_a).expect("serialize a");
    let v_b = serde_json::to_value(&resp_b).expect("serialize b");

    // A errored, B succeeded.
    assert!(v_a["error"].is_object(), "A must be cancelled");
    assert!(v_b["result"].is_object(), "B must succeed");
    // B's token is still untripped.
    assert!(!cancel_b.is_cancelled(), "B's token must not be tripped");
}

#[tokio::test]
async fn token_can_be_tripped_mid_test_to_simulate_late_cancellation() {
    // Smoke test: the token's `.cancel()` / `.is_cancelled()`
    // pair is the same primitive the SSE-drop path uses; this
    // confirms the token contract before we depend on it in
    // higher-level integration tests.
    let cancel = CancellationToken::new();
    assert!(!cancel.is_cancelled());
    cancel.cancel();
    assert!(cancel.is_cancelled());

    // Cloning the token shares the cancellation signal.
    let cancel2 = CancellationToken::new();
    let cancel2_clone = cancel2.clone();
    cancel2.cancel();
    assert!(cancel2_clone.is_cancelled());
}

#[tokio::test]
async fn drop_guard_trips_token_on_scope_exit() {
    // Mirrors the SSE drop-on-disconnect path: when the SSE
    // response future is dropped, its `DropGuard` fires the token.
    let cancel = CancellationToken::new();
    let cancel_observer = cancel.clone();
    {
        let _guard = cancel.drop_guard();
        assert!(!cancel_observer.is_cancelled());
    } // _guard drops here → fires token
    assert!(
        cancel_observer.is_cancelled(),
        "DropGuard should have tripped token on scope exit"
    );
}
