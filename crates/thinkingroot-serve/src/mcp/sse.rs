use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::rest::AppState;
use super::JsonRpcRequest;

/// Build the MCP SSE router (mounted at /mcp).
pub fn build_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/", post(handle_jsonrpc))
        .route("/sse", get(handle_sse_info))
        .with_state(state)
}

/// Handle a JSON-RPC request over HTTP POST.
async fn handle_jsonrpc(
    State(state): State<Arc<AppState>>,
    Json(request): Json<JsonRpcRequest>,
) -> Response {
    let engine = state.engine.read().await;
    let default_ws = engine
        .list_workspaces()
        .await
        .ok()
        .and_then(|ws| ws.first().map(|w| w.name.clone()));

    let response = super::dispatch(&request, &engine, default_ws.as_deref()).await;
    Json(response).into_response()
}

/// SSE info endpoint — returns server capabilities.
async fn handle_sse_info(State(state): State<Arc<AppState>>) -> Response {
    let engine = state.engine.read().await;
    let workspaces = engine.list_workspaces().await.unwrap_or_default();
    let ws_names: Vec<String> = workspaces.iter().map(|w| w.name.clone()).collect();

    Json(serde_json::json!({
        "server": "thinkingroot",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "MCP 2024-11-05",
        "transport": "HTTP POST to /mcp for JSON-RPC",
        "workspaces": ws_names,
    }))
    .into_response()
}
