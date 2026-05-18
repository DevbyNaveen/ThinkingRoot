use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::{JsonRpcRequest, JsonRpcResponse};
use crate::rest::{AppState, UnifiedCompileOutcome, UnifiedCompileRequest, run_unified_compile};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

// ─── Session State ───────────────────────────────────────────

/// Maps session_id → channel for sending SSE events to that client.
pub type SseSessionMap = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<SseMsg>>>>;

/// Create a new empty session map.
pub fn new_session_map() -> SseSessionMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Events sent through a session's SSE channel.
pub enum SseMsg {
    /// Initial event: the URL the client should POST JSON-RPC requests to.
    Endpoint(String),
    /// A serialized JSON-RPC response to forward to the client.
    Message(String),
}

// ─── Router ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct SessionQuery {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// Build the MCP SSE sub-router (mounted at `/mcp` by rest.rs).
pub fn build_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/sse", get(handle_sse))
        .route("/", post(handle_post))
        .with_state(state)
}

// ─── Handlers ────────────────────────────────────────────────

/// GET /mcp/sse
///
/// Opens a persistent SSE stream per the MCP 2024-11-05 transport spec:
///   1. A session ID is generated and registered in the session map.
///   2. An `event: endpoint` message carrying the POST URL is sent immediately.
///   3. Subsequent `event: message` frames deliver JSON-RPC responses.
///   4. A 30-second keep-alive comment prevents proxy/firewall timeouts.
///
/// Phase 3 central-AI-plan (2026-05-18): also captures the client's
/// `User-Agent` header at open so the "AI Tools" dashboard can show
/// which AI is connected. Sessions without a User-Agent are
/// classified as `InAppAgent` (the desktop's own chat fetcher
/// doesn't set one).
async fn handle_sse(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::unbounded_channel::<SseMsg>();

    // Register before streaming so concurrent POSTs can find the session immediately.
    state
        .mcp_sessions
        .lock()
        .await
        .insert(session_id.clone(), tx.clone());

    // Phase 3 — capture telemetry. User-Agent identifies the
    // connecting tool ("Cursor/1.0", "Claude-Code/0.5", etc.).
    // A missing or empty UA is treated as the desktop's in-app
    // agent — its fetch is internal to the daemon's process.
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let principal = if user_agent.is_empty() {
        crate::mcp::telemetry::PrincipalKind::InAppAgent
    } else {
        crate::mcp::telemetry::PrincipalKind::McpClient {
            user_agent: user_agent.clone(),
        }
    };
    crate::mcp::telemetry::record_session_opened(
        &state.mcp_session_telemetry,
        session_id.clone(),
        crate::mcp::telemetry::TransportKind::Sse,
        principal,
    )
    .await;

    // Queue the endpoint URL — MCP clients use this to discover the POST address.
    let endpoint_url = format!("/mcp?sessionId={session_id}");
    let _ = tx.send(SseMsg::Endpoint(endpoint_url));

    // Session reaper. When the SSE stream's receiver is dropped (the
    // client disconnected, network flapped, browser refreshed before
    // sending any POST), the existing send-failure cleanup at
    // `handle_post` only fires if a POST arrives. Without this
    // watchdog, a session that never receives a POST leaks the
    // `UnboundedSender` slot in `mcp_sessions` forever.
    //
    // `UnboundedSender::closed()` resolves the moment the receiver
    // half is dropped. The clone we hold here doesn't prevent the
    // stream's `rx` from dropping when the stream ends — clones are
    // independent senders and `rx.recv()` returns None only when all
    // senders go away. The reaper holds one of those clones to await
    // the close signal, then removes the entry.
    {
        let session_id = session_id.clone();
        let sessions = state.mcp_sessions.clone();
        let telemetry = state.mcp_session_telemetry.clone();
        let tx_watch = tx.clone();
        tokio::spawn(async move {
            tx_watch.closed().await;
            sessions.lock().await.remove(&session_id);
            // Phase 3 — persist the final telemetry snapshot to
            // `mcp-sessions.jsonl` and drop the live entry. The
            // reaper firing means the receiver was dropped (clean
            // client disconnect, network drop, browser refresh) —
            // always reason ChannelClosed.
            crate::mcp::telemetry::record_session_closed(
                &telemetry,
                &session_id,
                crate::mcp::telemetry::DisconnectReason::ChannelClosed,
            )
            .await;
            tracing::debug!(
                target: "mcp_sse",
                "session reaper: removed entry after receiver drop"
            );
        });
    }

    let stream = UnboundedReceiverStream::new(rx).map(|msg| {
        let event = match msg {
            SseMsg::Endpoint(url) => Event::default().event("endpoint").data(url),
            SseMsg::Message(json) => Event::default().event("message").data(json),
        };
        Ok::<Event, Infallible>(event)
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("keep-alive"),
    )
}

/// If `request` is a `tools/call name="compile"` payload, run the
/// unified compile flow and return the synthesised JSON-RPC response.
/// Otherwise return `None` so the caller falls through to the normal
/// MCP dispatch.
///
/// This bypasses `mcp::dispatch` for the compile tool specifically so
/// that the unified post-compile reconciliation (remount, vector
/// rebuild, workspace_status actor, engram invalidation) runs against
/// the same `Arc<AppState>` the SSE transport already owns. The
/// legacy stdio transport (which has no AppState) continues to hit
/// the `engine.compile()` arm inside `tools::handle_call`.
async fn compile_request_fastpath(
    request: &JsonRpcRequest,
    state: &Arc<AppState>,
) -> Option<JsonRpcResponse> {
    if request.method != "tools/call" {
        return None;
    }
    let tool_name = request.params.get("name").and_then(|v| v.as_str())?;
    if tool_name != "compile" {
        return None;
    }
    let id = request.id.clone();
    let arguments = request
        .params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    // Resolve the workspace argument against the live engine, then
    // look up its registered root path. Hold the read lock just long
    // enough to read both — drop before the helper write-locks engine.
    let (ws, root_path) = {
        let engine = state.engine.read().await;
        let default_ws = engine
            .list_workspaces()
            .await
            .ok()
            .and_then(|list| list.first().map(|w| w.name.clone()));
        let ws = crate::mcp::tools::resolve_workspace_arg(
            arguments.get("workspace").and_then(|v| v.as_str()),
            default_ws.as_deref(),
            &engine,
        );
        let root_path = engine.workspace_root_path(&ws);
        (ws, root_path)
    };

    let root_path = match root_path {
        Some(p) => p,
        None => {
            return Some(JsonRpcResponse::error(
                id,
                -32602,
                format!("workspace `{ws}` is not mounted on this daemon"),
            ));
        }
    };

    // Cancellation flows through `_drop_guard` — when this future is
    // dropped (agent turn cancellation, transport disconnect), the
    // guard fires the token and `run_unified_compile`'s pipeline
    // bails at the next phase boundary.
    let cancel = CancellationToken::new();
    let _drop_guard = cancel.clone().drop_guard();

    let req = UnifiedCompileRequest {
        ws_url_alias: ws,
        root_path,
        branch: arguments
            .get("branch")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        no_rooting: arguments
            .get("no_rooting")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    };

    let (_status_name, outcome) =
        run_unified_compile(state.clone(), req, None, cancel).await;

    let response = match outcome {
        UnifiedCompileOutcome::Done(result) => {
            // Match the legacy `mcp_text_result` shape exactly: a
            // single text content block whose payload is the
            // pretty-printed PipelineResult JSON. The agent's tool
            // result parser is already tuned to this shape, and
            // re-using it preserves wire-format parity with stdio
            // MCP clients that still hit `tools::handle_call`'s
            // legacy `engine.compile(ws)` arm.
            match serde_json::to_string_pretty(&result) {
                Ok(content) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": content }],
                    }),
                ),
                Err(e) => {
                    JsonRpcResponse::error(id, -32603, format!("serialize result: {e}"))
                }
            }
        }
        UnifiedCompileOutcome::Cancelled => {
            JsonRpcResponse::error(id, -32603, "compile cancelled".to_string())
        }
        UnifiedCompileOutcome::Failed(msg) => JsonRpcResponse::error(id, -32603, msg),
    };
    Some(response)
}

/// Phase 1 central-AI-plan (2026-05-18) — `workspace_mount` fastpath.
///
/// Mounting a workspace requires `&mut QueryEngine` (the engine's
/// internal `workspaces: HashMap<...>` is mutated). The shared
/// `mcp::tools::handle_call` only gets `&QueryEngine`, so the
/// `WorkspaceMount` trait handler refuses honestly when called from
/// stdio. SSE goes through this fastpath instead, which acquires
/// `state.engine.write().await` and calls `engine.mount` directly.
///
/// Returns `None` when the request isn't a `workspace_mount` call,
/// allowing the dispatcher to fall through to normal MCP dispatch.
async fn workspace_mount_fastpath(
    request: &JsonRpcRequest,
    state: &Arc<AppState>,
) -> Option<JsonRpcResponse> {
    if request.method != "tools/call" {
        return None;
    }
    let tool_name = request.params.get("name").and_then(|v| v.as_str())?;
    if tool_name != "workspace_mount" {
        return None;
    }
    let id = request.id.clone();
    let arguments = request
        .params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            return Some(JsonRpcResponse::error(
                id,
                -32602,
                "workspace_mount: missing or empty `name` argument".to_string(),
            ));
        }
    };
    let root_path_raw = match arguments.get("root_path").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Some(JsonRpcResponse::error(
                id,
                -32602,
                "workspace_mount: missing `root_path` argument".to_string(),
            ));
        }
    };
    let root_path = std::path::PathBuf::from(&root_path_raw);
    if !root_path.is_absolute() {
        return Some(JsonRpcResponse::error(
            id,
            -32602,
            format!("workspace_mount: root_path `{root_path_raw}` must be absolute"),
        ));
    }
    let data_dir = arguments
        .get("data_dir")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);

    let mount_result = {
        let mut engine = state.engine.write().await;
        match data_dir {
            Some(dd) => {
                engine
                    .mount_with_data_dir(name.clone(), root_path.clone(), dd)
                    .await
            }
            None => engine.mount(name.clone(), root_path.clone()).await,
        }
    };
    if let Err(e) = mount_result {
        return Some(JsonRpcResponse::error(
            id,
            -32603,
            format!("workspace_mount failed: {e}"),
        ));
    }

    // Refresh counts so the response is useful for the in-app AI to
    // confirm the mount took. Read-lock is fine here — the write
    // guard above has been dropped.
    let info = {
        let engine = state.engine.read().await;
        engine
            .list_workspaces()
            .await
            .ok()
            .and_then(|list| list.into_iter().find(|w| w.name == name))
    };

    let (entity_count, claim_count, source_count) = info
        .map(|w| (w.entity_count, w.claim_count, w.source_count))
        .unwrap_or((0, 0, 0));

    let payload = serde_json::json!({
        "schema_version": 1,
        "workspace": name,
        "root_path": root_path.display().to_string(),
        "entity_count": entity_count,
        "claim_count": claim_count,
        "source_count": source_count,
    });
    let response = match serde_json::to_string_pretty(&payload) {
        Ok(content) => JsonRpcResponse::success(
            id,
            serde_json::json!({
                "content": [{ "type": "text", "text": content }],
            }),
        ),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("serialize result: {e}")),
    };
    Some(response)
}

/// POST /mcp?sessionId=X
///
/// Receives a JSON-RPC request, dispatches it, and routes the response back
/// through the session's SSE stream. Returns 202 Accepted so the client can
/// continue sending without waiting for the (async) SSE response.
async fn handle_post(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SessionQuery>,
    Json(request): Json<JsonRpcRequest>,
) -> Response {
    // Per JSON-RPC 2.0, notifications have no `id` and must not generate responses.
    if request.id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    let session_id = match params.session_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing 'sessionId' query parameter"})),
            )
                .into_response();
        }
    };

    // Compile fast-path — when the chat agent calls the `compile` MCP
    // tool, route through `run_unified_compile` instead of the legacy
    // `engine.compile()` dispatch arm. The unified helper performs the
    // post-compile remount + vector-index rebuild + workspace_status
    // actor messages + engram cache invalidation that the legacy arm
    // skips. Without this fast-path, an agent-driven compile would
    // succeed but leave every downstream query path returning the
    // pre-compile empty view until the workspace is manually
    // remounted.
    //
    // The early-drop is structural: `run_unified_compile` write-locks
    // `state.engine` for remount, which deadlocks if a read guard is
    // still held in this scope. We resolve the workspace path while
    // holding the read briefly, drop it, then run the unified compile.
    // Phase 3 central-AI-plan (2026-05-18): capture the tool name
    // BEFORE dispatch so we can record telemetry against it
    // regardless of which branch (fastpath vs. standard) handles
    // the request.
    let dispatched_tool_name = if request.method == "tools/call" {
        request
            .params
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };

    let response = if let Some(fastpath) = compile_request_fastpath(&request, &state).await {
        fastpath
    } else if let Some(fastpath) = workspace_mount_fastpath(&request, &state).await {
        // Phase 1 central-AI-plan (2026-05-18) — workspace_mount needs
        // `&mut QueryEngine` so it can't go through the standard
        // `mcp::tools::handle_call` path. The trait handler in
        // `operator_tools.rs` refuses honestly for stdio; SSE comes here.
        fastpath
    } else {
        let engine = state.engine.read().await;
        let default_ws = engine
            .list_workspaces()
            .await
            .ok()
            .and_then(|ws| ws.first().map(|w| w.name.clone()));

        let response = super::dispatch(
            &request,
            &engine,
            default_ws.as_deref(),
            &session_id,
            &state.sessions,
            &state.engram_manager,
        )
        .await;
        drop(engine);
        response
    };

    // Phase 3 central-AI-plan (2026-05-18): bump telemetry counters.
    // Every `tools/call` increments `tool_calls_total`; if the
    // response carries a JSON-RPC error envelope, we additionally
    // record an error against the session. We use the public
    // `is_error_response` helper-shape from `JsonRpcResponse` — when
    // `response.error` is Some, it's a structured error.
    if let Some(tool_name) = dispatched_tool_name.as_deref() {
        crate::mcp::telemetry::record_tool_call(
            &state.mcp_session_telemetry,
            &session_id,
        )
        .await;
        if let Some(err) = response.error.as_ref() {
            crate::mcp::telemetry::record_error(
                &state.mcp_session_telemetry,
                &session_id,
                tool_name.to_string(),
                err.code as i64,
                err.message.clone(),
            )
            .await;
        }
    }

    let json_str = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to serialize MCP response: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Route the response to the session's SSE stream. The lock guard
    // is held across the entire send-and-cleanup path so that a
    // reconnect carrying the same `session_id` cannot slot itself
    // into the map between our `is_err()` check and the `.remove()`,
    // which would otherwise evict the new (live) session instead of
    // the dead one. Worst case the lock is held for a single
    // synchronous `tx.send` — non-blocking on `UnboundedSender`.
    let mut sessions = state.mcp_sessions.lock().await;
    match sessions.get(&session_id) {
        Some(tx) => {
            let send_result = tx.send(SseMsg::Message(json_str));
            if send_result.is_err() {
                // The SSE stream closed between registration and this POST.
                // Clean up the dead session entry under the same guard.
                tracing::warn!("MCP session {session_id}: SSE stream closed, removing session");
                sessions.remove(&session_id);
                return StatusCode::GONE.into_response();
            }
            StatusCode::ACCEPTED.into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("session '{session_id}' not found")})),
        )
            .into_response(),
    }
}
