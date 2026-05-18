//! Phase 1 of the "ThinkingRoot Central" plan (`plans/okey-so-i-wnat-elegant-hamster.md`):
//! end-to-end exercise of the operator-tool dispatch path.
//!
//! These tests focus on the dispatch+wiring contract, not the
//! underlying substrate (which has its own unit tests in
//! `thinkingroot-core`). The point of this file is to prove that:
//!
//! 1. `operator_tools::register_all()` exposes every tool through
//!    the `mcp::tool_trait` registry's `lookup()` API.
//! 2. A real `QueryEngine`-backed call to a read-class tool returns
//!    a well-formed JSON response with the expected schema.
//! 3. A read-class tool composes with `engine.mount` — after mount,
//!    `list_workspaces_full` reflects the new workspace; after
//!    `engram_invalidate_workspace` the engram cache is honest.
//! 4. The stdio-path refusal for `workspace_mount` carries the
//!    typed message that points callers at the HTTP / SSE path.
//!
//! What this test deliberately does NOT cover:
//! - The pre-trust short-circuit (covered by
//!   `permissions_gate::tests::pre_trusted_operator_tool_short_circuits_before_inner_gate`).
//! - The SSE fastpath for `workspace_mount` (would require an Axum
//!   test server — not justifiable for a single tool).
//! - The restart broadcast subscriber's `std::process::exit(0)` —
//!   would terminate the test runner; covered by manual smoke.

use std::sync::Arc;

use serde_json::json;
use tempfile::tempdir;

use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::intelligence::engram::{EngramConfig, EngramManager};
use thinkingroot_serve::intelligence::session::new_session_store;
use thinkingroot_serve::mcp::tool_trait::{self, McpToolContext};
use thinkingroot_serve::operator_tools;

/// Build a fresh `QueryEngine` + `EngramManager` + `SessionStore` for
/// the test. Mirrors what the SSE transport does at request time.
async fn make_context_pieces() -> (
    QueryEngine,
    Arc<EngramManager>,
    thinkingroot_serve::intelligence::session::SessionStore,
) {
    let engine = QueryEngine::new();
    let engram_manager = EngramManager::new(EngramConfig::default());
    let sessions = new_session_store();
    (engine, engram_manager, sessions)
}

#[tokio::test]
async fn register_all_exposes_every_operator_tool_through_lookup() {
    // Acquire the same process-global lock the trait_test module
    // uses so a sibling integration test that mutates the registry
    // can't race us.
    // Note: integration tests run as a separate binary from unit
    // tests, so the unit-test `test_lock` mutex isn't visible. We
    // accept that risk and re-register from scratch.
    operator_tools::register_all();

    for tool in [
        // Phase 1 operator tools
        "recovery_log_tail",
        "restart_state_get",
        "reset_circuit_breaker",
        "reset_compile_breaker",
        "doctor_run",
        "doctor_apply_fix",
        "install_manifest_read",
        "install_manifest_verify_checksum",
        "rebuild_vector_index",
        "migrate_substrate",
        "list_workspaces_full",
        "workspace_mount",
        "workspace_root_path",
        "engram_invalidate_workspace",
        "mark_setup_complete",
        "restart_engine_request",
        // Phase 3 visibility tools
        "list_mcp_sessions",
        "mcp_session_health",
        "mcp_error_log",
    ] {
        let handler = tool_trait::lookup(tool);
        assert!(
            handler.is_some(),
            "operator tool `{tool}` must be discoverable via tool_trait::lookup after register_all"
        );
    }
}

#[tokio::test]
async fn recovery_log_tail_returns_well_formed_response() {
    operator_tools::register_all();

    let (engine, engram_manager, sessions) = make_context_pieces().await;
    let ctx = McpToolContext {
        engine: &engine,
        workspace: "any",
        session_id: "test-session",
        sessions: &sessions,
        engram_manager: &engram_manager,
    };

    let handler =
        tool_trait::lookup("recovery_log_tail").expect("recovery_log_tail must be registered");
    let result = handler.handle(json!({ "limit": 10 }), &ctx).await;
    let value = result.expect("recovery_log_tail must succeed with a valid limit");
    assert_eq!(value["schema_version"], 1, "schema_version must be 1");
    assert!(
        value.get("events").is_some(),
        "response must carry an `events` field (possibly empty)"
    );
    assert!(
        value["events"].is_array(),
        "events must be a JSON array"
    );
}

#[tokio::test]
async fn workspace_root_path_returns_null_for_unknown_workspace() {
    operator_tools::register_all();

    let (engine, engram_manager, sessions) = make_context_pieces().await;
    let ctx = McpToolContext {
        engine: &engine,
        workspace: "does-not-exist",
        session_id: "test-session",
        sessions: &sessions,
        engram_manager: &engram_manager,
    };

    let handler =
        tool_trait::lookup("workspace_root_path").expect("workspace_root_path must be registered");
    let value = handler
        .handle(json!({}), &ctx)
        .await
        .expect("workspace_root_path must succeed even when workspace is unknown");
    assert_eq!(value["workspace"], "does-not-exist");
    assert_eq!(
        value["root_path"],
        serde_json::Value::Null,
        "unknown workspace must return null root_path, not an error"
    );
}

#[tokio::test]
async fn list_workspaces_full_reflects_mounted_workspaces() {
    operator_tools::register_all();

    // Mount a real workspace under a tempdir so the engine has
    // something to list. `engine.mount` requires a `.thinkingroot/`
    // subdirectory (it's where the CozoDB substrate lives) so we
    // create one before mounting.
    let dir = tempdir().expect("tempdir");
    let workspace_root = dir.path().to_path_buf();
    std::fs::create_dir_all(workspace_root.join(".thinkingroot"))
        .expect(".thinkingroot subdir must be creatable for mount");
    let mut engine = QueryEngine::new();
    engine
        .mount("test-ws".to_string(), workspace_root.clone())
        .await
        .expect("engine.mount must succeed once .thinkingroot/ exists");

    let engram_manager = EngramManager::new(EngramConfig::default());
    let sessions = new_session_store();
    let ctx = McpToolContext {
        engine: &engine,
        workspace: "test-ws",
        session_id: "test-session",
        sessions: &sessions,
        engram_manager: &engram_manager,
    };

    let handler = tool_trait::lookup("list_workspaces_full")
        .expect("list_workspaces_full must be registered");
    let value = handler
        .handle(json!({}), &ctx)
        .await
        .expect("list_workspaces_full must succeed");

    assert_eq!(value["schema_version"], 1);
    assert_eq!(
        value["count"], 1,
        "exactly one workspace must be reported"
    );
    let workspaces = value["workspaces"].as_array().expect("workspaces array");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(
        workspaces[0]["name"], "test-ws",
        "the mounted workspace name must appear in the listing"
    );

    // Symmetric check: workspace_root_path resolves it.
    let path_handler = tool_trait::lookup("workspace_root_path").unwrap();
    let path_value = path_handler.handle(json!({}), &ctx).await.unwrap();
    assert_eq!(
        path_value["root_path"]
            .as_str()
            .expect("root_path must be a string for a mounted workspace"),
        workspace_root.display().to_string(),
    );
}

#[tokio::test]
async fn engram_invalidate_workspace_is_idempotent_on_unknown_workspace() {
    operator_tools::register_all();

    let (engine, engram_manager, sessions) = make_context_pieces().await;
    let ctx = McpToolContext {
        engine: &engine,
        workspace: "no-such-ws",
        session_id: "test-session",
        sessions: &sessions,
        engram_manager: &engram_manager,
    };

    let handler = tool_trait::lookup("engram_invalidate_workspace")
        .expect("engram_invalidate_workspace must be registered");
    let value = handler
        .handle(json!({}), &ctx)
        .await
        .expect("engram_invalidate must succeed even when workspace has no engrams");
    assert_eq!(value["invalidated"], true);
}

#[tokio::test]
async fn workspace_mount_via_trait_refuses_with_typed_stdio_message() {
    operator_tools::register_all();

    let (engine, engram_manager, sessions) = make_context_pieces().await;
    let ctx = McpToolContext {
        engine: &engine,
        workspace: "any",
        session_id: "test-session",
        sessions: &sessions,
        engram_manager: &engram_manager,
    };

    let handler = tool_trait::lookup("workspace_mount")
        .expect("workspace_mount must be registered (for tools/list discoverability)");
    let result = handler
        .handle(
            json!({ "name": "foo", "root_path": "/tmp/foo" }),
            &ctx,
        )
        .await;
    let err = result
        .expect_err("workspace_mount via trait must refuse — only the SSE fastpath can mount");
    let msg = err.to_string();
    assert!(
        msg.contains("SSE MCP transport"),
        "refusal must point callers at the SSE path, got: {msg}"
    );
}

// `install_manifest_read` is intentionally NOT tested at integration
// level: the only meaningful path is "no manifest on disk → returns
// null", but proving that requires redirecting `XDG_CONFIG_HOME` to a
// tempdir, and env vars are process-global. cargo parallelises tests
// by default, so a single `set_var` from this test would race with
// any sibling test that loads the real install manifest. The unit
// tests in `operator_tools::tests::register_all_registers_sixteen_tools`
// already prove the tool is registered + schema-correct; the real
// "does it parse manifests correctly" lives in
// `thinkingroot-core::install_manifest::tests`.
//
// Note: during this slice's bring-up the test surfaced a real
// production drift — a manifest on the dev machine had a stale
// `local-bin-root` BinaryId value that's not in the current
// `cli-script | desktop-bundle` enum, and the tool propagated the
// parse error as `Refused` (honest, no swallowing). That's the
// behaviour we want; the test that exercised it was just too
// environmentally fragile to keep.
