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

use super::JsonRpcRequest;
use crate::rest::AppState;

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
    )
    .await;
    drop(engine);

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
