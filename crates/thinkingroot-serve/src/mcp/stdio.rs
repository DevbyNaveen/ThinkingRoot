use super::JsonRpcRequest;
use crate::engine::QueryEngine;
use crate::intelligence::engram::{EngramConfig, EngramManager};
use crate::intelligence::session::{SessionStore, new_session_store};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;

pub async fn run(
    engine: Arc<RwLock<QueryEngine>>,
    default_workspace: Option<String>,
    sessions: SessionStore,
) {
    // Per-process unique session id. Pre-fix this was the literal
    // string `"stdio"`, so two concurrent stdio transports (e.g.
    // Claude Code + Cursor in two terminal panes attached to the
    // same daemon) collided on every session-scoped piece of state:
    // EngramManager pointer tables, `checkout_branch` active
    // branch, `turn_provenance` window, observer buffer. The
    // session id includes the process id so logs remain readable
    // even when a UUID rolls.
    let stdio_session_id = format!(
        "stdio-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    // Stdio transport gets its own EngramManager — single client, lives
    // for the duration of the process. SSE transport uses the AppState's
    // shared manager.
    let engram_manager = EngramManager::new(EngramConfig::default());

    // Phase 1 central-AI-plan (2026-05-18) — register operator tools
    // so the stdio MCP path (editor integrations like Cursor / Claude
    // Code / Codex) also gets `recovery_log_tail`, `doctor_run`,
    // `migrate_substrate`, etc. The `restart_engine_request` tool will
    // honestly refuse here because no broadcast channel is installed —
    // stdio MCP has no sidecar to restart from this process.
    crate::operator_tools::register_all();
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                eprintln!("[mcp-stdio] stdin closed, shutting down");
                break;
            }
            Err(e) => {
                eprintln!("[mcp-stdio] read error: {}", e);
                break;
            }
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err_response =
                    super::JsonRpcResponse::error(None, -32700, format!("Parse error: {}", e));
                let json = serde_json::to_string(&err_response).unwrap_or_default();
                let _ = stdout.write_all(json.as_bytes()).await;
                let _ = stdout.write_all(b"\n").await;
                let _ = stdout.flush().await;
                continue;
            }
        };

        if request.id.is_none() && request.method.starts_with("notifications/") {
            continue;
        }

        let engine_guard = engine.read().await;
        // C3 (2026-05-22): fresh per-request token. Stdio MCP has
        // no `notifications/cancelled` listener (the transport is
        // strictly line-oriented synchronous from the editor's
        // perspective), but the token is required by the dispatch
        // signature. Long tools running over stdio aren't
        // cancellable mid-call today; an explicit Ctrl-C kills the
        // whole stdio binary.
        let cancel = tokio_util::sync::CancellationToken::new();
        let response = super::dispatch(
            &request,
            &engine_guard,
            default_workspace.as_deref(),
            &stdio_session_id,
            &sessions,
            &engram_manager,
            // Stdio has no AppState — tools that require it (e.g.
            // `get_reminder_context`) return a typed
            // "transport-not-supported" envelope.
            None,
            cancel,
        )
        .await;
        drop(engine_guard);

        let json = serde_json::to_string(&response).unwrap_or_default();
        let _ = stdout.write_all(json.as_bytes()).await;
        let _ = stdout.write_all(b"\n").await;
        let _ = stdout.flush().await;
    }
}

/// Create a fresh session store — convenience for callers that create stdio servers
/// without an `AppState` (e.g., CLI `root serve --mcp-stdio`).
pub fn new_stdio_sessions() -> SessionStore {
    new_session_store()
}
