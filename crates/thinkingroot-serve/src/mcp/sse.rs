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
async fn handle_sse(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::unbounded_channel::<SseMsg>();

    // Register before streaming so concurrent POSTs can find the session immediately.
    state
        .mcp_sessions
        .lock()
        .await
        .insert(session_id.clone(), tx.clone());

    // Queue the endpoint URL — MCP clients use this to discover the POST address.
    let endpoint_url = format!("/mcp?sessionId={session_id}");
    let _ = tx.send(SseMsg::Endpoint(endpoint_url));

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
    let response = if let Some(fastpath) = compile_request_fastpath(&request, &state).await {
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

    let json_str = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to serialize MCP response: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Route the response to the session's SSE stream.
    let sessions = state.mcp_sessions.lock().await;
    match sessions.get(&session_id) {
        Some(tx) => {
            let send_result = tx.send(SseMsg::Message(json_str));
            drop(sessions);

            if send_result.is_err() {
                // The SSE stream closed between registration and this POST.
                // Clean up the dead session entry.
                tracing::warn!("MCP session {session_id}: SSE stream closed, removing session");
                state.mcp_sessions.lock().await.remove(&session_id);
                return StatusCode::GONE.into_response();
            }

            StatusCode::ACCEPTED.into_response()
        }
        None => {
            drop(sessions);
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("session '{session_id}' not found")})),
            )
                .into_response()
        }
    }
}
