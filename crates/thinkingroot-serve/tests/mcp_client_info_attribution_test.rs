//! C6 (2026-05-22) — `clientInfo` → `Principal` attribution tests.
//!
//! These pin the mapping contract:
//!
//! 1. `ClientInfo::is_known_ai_client` returns true for every name
//!    in `KNOWN_AI_CLIENT_NAMES` (case-insensitive).
//! 2. Returns false for any unrecognised vendor name (so unknown
//!    clients land as `Principal::User`, preserving pre-C6
//!    behaviour for tests + bespoke libraries).
//! 3. Round-trip serde — ClientInfo can be deserialised from the
//!    JSON shape MCP clients send on `initialize`.
//! 4. The mapping table itself is non-empty and includes the
//!    canonical AI tools we shipped support for.
//! 5. `get_session_context` returns the stashed `client_info` once
//!    the session has been seeded (mirrors the wire flow:
//!    `initialize` populates → later `get_session_context` reads).

use serde_json::json;
use std::sync::Arc;
use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::intelligence::engram::{EngramConfig, EngramManager};
use thinkingroot_serve::intelligence::session::{
    ClientInfo, KNOWN_AI_CLIENT_NAMES, SessionContext, SessionStore,
    is_known_ai_client_name, new_session_store,
};
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

#[test]
fn every_canonical_ai_client_is_recognised() {
    for name in KNOWN_AI_CLIENT_NAMES {
        assert!(
            is_known_ai_client_name(name),
            "canonical name '{name}' should be recognised"
        );
    }
}

#[test]
fn name_matching_is_case_insensitive() {
    assert!(is_known_ai_client_name("CLAUDE-CODE"));
    assert!(is_known_ai_client_name("Cursor"));
    assert!(is_known_ai_client_name("codex"));
}

#[test]
fn unknown_vendor_returns_false() {
    assert!(!is_known_ai_client_name("acme-ai"));
    assert!(!is_known_ai_client_name(""));
    assert!(!is_known_ai_client_name("openai")); // not on our shipped list
}

#[test]
fn known_ai_client_names_includes_top_tier_tools() {
    // Pins that the well-known names actually ship in the table.
    for required in ["claude-code", "claude-desktop", "cursor", "codex"] {
        assert!(
            KNOWN_AI_CLIENT_NAMES.contains(&required),
            "KNOWN_AI_CLIENT_NAMES must include '{required}'"
        );
    }
}

#[test]
fn client_info_roundtrips_through_serde() {
    let payload = json!({
        "name": "claude-code",
        "version": "1.0.42"
    });
    let parsed: ClientInfo =
        serde_json::from_value(payload).expect("parse clientInfo");
    assert_eq!(parsed.name, "claude-code");
    assert_eq!(parsed.version, "1.0.42");
    assert!(parsed.is_known_ai_client());
}

#[test]
fn client_info_version_defaults_to_empty_when_absent() {
    // MCP spec doesn't strictly require `version` — `#[serde(default)]`
    // on our field tolerates its absence.
    let payload = json!({ "name": "cursor" });
    let parsed: ClientInfo =
        serde_json::from_value(payload).expect("parse clientInfo without version");
    assert_eq!(parsed.name, "cursor");
    assert_eq!(parsed.version, "");
}

#[tokio::test]
async fn get_session_context_returns_stashed_client_info() {
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();

    // Seed the session with a known AI client's info.
    {
        let mut store = sessions.lock().await;
        let mut sc = SessionContext::new("session-claude-code", "ws-a");
        sc.client_info = Some(ClientInfo {
            name: "claude-code".to_string(),
            version: "1.0.42".to_string(),
        });
        store.insert("session-claude-code".to_string(), sc);
    }

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
        "session-claude-code",
        &sessions,
        &engram_manager,
        None,
        CancellationToken::new(),
    )
    .await;
    let v = serde_json::to_value(&resp).expect("serialize");
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let payload: serde_json::Value = serde_json::from_str(text).expect("inner JSON");

    assert_eq!(payload["client_info"]["name"], json!("claude-code"));
    assert_eq!(payload["client_info"]["version"], json!("1.0.42"));
}

#[tokio::test]
async fn get_session_context_returns_null_client_info_for_session_without_one() {
    // Regression pin: pre-C6 callers + non-MCP REST chat sessions
    // never set client_info; the tool must report Null rather than
    // fabricating a default value.
    let engine = empty_engine();
    let sessions = empty_sessions();
    let engram_manager = empty_engram_manager();

    {
        let mut store = sessions.lock().await;
        let sc = SessionContext::new("session-no-client", "ws-a");
        store.insert("session-no-client".to_string(), sc);
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
        "session-no-client",
        &sessions,
        &engram_manager,
        None,
        CancellationToken::new(),
    )
    .await;
    let v = serde_json::to_value(&resp).expect("serialize");
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let payload: serde_json::Value = serde_json::from_str(text).expect("inner JSON");

    assert_eq!(payload["client_info"], serde_json::Value::Null);
}
