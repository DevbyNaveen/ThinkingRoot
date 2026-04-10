use std::sync::Arc;

use crate::graph::serve_graph;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};

use crate::engine::{ClaimFilter, QueryEngine};

// ─── App State ───────────────────────────────────────────────

pub struct AppState {
    pub engine: RwLock<QueryEngine>,
    pub api_key: Option<String>,
    pub mcp_sessions: crate::mcp::sse::SseSessionMap,
}

impl AppState {
    /// Create a new `AppState` wrapped in `Arc`, initialising a fresh session map.
    pub fn new(engine: QueryEngine, api_key: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            engine: RwLock::new(engine),
            api_key,
            mcp_sessions: crate::mcp::sse::new_session_map(),
        })
    }
}

// ─── Response Envelope ───────────────────────────────────────

#[derive(Serialize)]
struct ApiResponse<T: Serialize> {
    ok: bool,
    data: Option<T>,
    error: Option<ApiError>,
}

#[derive(Serialize)]
struct ApiError {
    code: String,
    message: String,
}

fn ok_response<T: Serialize>(data: T) -> Json<ApiResponse<T>> {
    Json(ApiResponse {
        ok: true,
        data: Some(data),
        error: None,
    })
}

fn err_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = ApiResponse::<()> {
        ok: false,
        data: None,
        error: Some(ApiError {
            code: code.to_string(),
            message: message.to_string(),
        }),
    };
    (status, Json(body)).into_response()
}

// ─── Query Params ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ClaimQueryParams {
    #[serde(rename = "type")]
    pub claim_type: Option<String>,
    pub entity: Option<String>,
    pub min_confidence: Option<f64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Deserialize)]
pub struct SearchQueryParams {
    pub q: String,
    pub top_k: Option<usize>,
}

// ─── Router ──────────────────────────────────────────────────

pub fn build_router(state: Arc<AppState>) -> Router {
    build_router_opts(state, true, true)
}

pub fn build_router_opts(state: Arc<AppState>, enable_rest: bool, enable_mcp: bool) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mut router = Router::new();

    // Graph explorer — always available when REST is on
    if enable_rest {
        router = router.route("/graph", get(serve_graph));
    }

    if enable_rest {
        let api_routes = Router::new()
            .route("/workspaces", get(list_workspaces))
            .route("/ws/{ws}/entities", get(list_entities))
            .route("/ws/{ws}/entities/{name}", get(get_entity))
            .route("/ws/{ws}/claims", get(list_claims))
            .route("/ws/{ws}/relations", get(get_all_relations))
            .route("/ws/{ws}/relations/{entity}", get(get_entity_relations))
            .route("/ws/{ws}/artifacts", get(list_artifacts))
            .route("/ws/{ws}/artifacts/{artifact_type}", get(get_artifact))
            .route("/ws/{ws}/health", get(get_health))
            .route("/ws/{ws}/search", get(search))
            .route("/ws/{ws}/compile", post(compile))
            .route("/ws/{ws}/verify", post(verify_ws));
        router = router.nest("/api/v1", api_routes);
    }

    if enable_mcp {
        let mcp_routes = crate::mcp::sse::build_router(state.clone());
        router = router.nest("/mcp", mcp_routes);
    }

    router
        .layer(cors)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

// ─── Auth Middleware ──────────────────────────────────────────

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if let Some(ref expected_key) = state.api_key {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        match provided {
            Some(key) if key == expected_key => {}
            _ => {
                return err_response(
                    StatusCode::UNAUTHORIZED,
                    "UNAUTHORIZED",
                    "Invalid or missing API key",
                );
            }
        }
    }
    next.run(request).await
}

// ─── Handlers ────────────────────────────────────────────────

async fn list_workspaces(State(state): State<Arc<AppState>>) -> Response {
    let engine = state.engine.read().await;
    match engine.list_workspaces().await {
        Ok(ws) => ok_response(ws).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            &e.to_string(),
        ),
    }
}

async fn list_entities(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.list_entities(&ws).await {
        Ok(entities) => ok_response(entities).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_entity(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_entity(&ws, &name).await {
        Ok(entity) => ok_response(entity).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn list_claims(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<ClaimQueryParams>,
) -> Response {
    let engine = state.engine.read().await;
    let filter = ClaimFilter {
        claim_type: params.claim_type,
        entity_name: params.entity,
        min_confidence: params.min_confidence,
        limit: params.limit,
        offset: params.offset,
    };
    match engine.list_claims(&ws, filter).await {
        Ok(claims) => ok_response(claims).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_all_relations(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.get_all_relations(&ws).await {
        Ok(rels) => {
            let data: Vec<serde_json::Value> = rels
                .into_iter()
                .map(|(from, to, rtype, strength)| {
                    serde_json::json!({
                        "from": from,
                        "to": to,
                        "relation_type": rtype,
                        "strength": strength,
                    })
                })
                .collect();
            ok_response(data).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

async fn get_entity_relations(
    State(state): State<Arc<AppState>>,
    Path((ws, entity)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_relations(&ws, &entity).await {
        Ok(rels) => ok_response(rels).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn list_artifacts(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.list_artifacts(&ws).await {
        Ok(artifacts) => ok_response(artifacts).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path((ws, artifact_type)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_artifact(&ws, &artifact_type).await {
        Ok(artifact) => {
            let wants_markdown = headers
                .get("accept")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.contains("text/markdown"))
                .unwrap_or(false);

            if wants_markdown {
                (
                    StatusCode::OK,
                    [("content-type", "text/markdown")],
                    artifact.content,
                )
                    .into_response()
            } else {
                ok_response(artifact).into_response()
            }
        }
        Err(e) => match_engine_error(e),
    }
}

async fn get_health(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.health(&ws).await {
        Ok(result) => ok_response(result).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn search(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<SearchQueryParams>,
) -> Response {
    let engine = state.engine.read().await;
    let top_k = params.top_k.unwrap_or(10);
    match engine.search(&ws, &params.q, top_k).await {
        Ok(results) => ok_response(results).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn compile(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.compile(&ws).await {
        Ok(result) => ok_response(result).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn verify_ws(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.verify(&ws).await {
        Ok(result) => ok_response(result).into_response(),
        Err(e) => match_engine_error(e),
    }
}

// ─── Error Mapping ───────────────────────────────────────────

fn match_engine_error(e: thinkingroot_core::Error) -> Response {
    match &e {
        thinkingroot_core::Error::EntityNotFound(_) => {
            err_response(StatusCode::NOT_FOUND, "NOT_FOUND", &e.to_string())
        }
        thinkingroot_core::Error::Config(_) => {
            err_response(StatusCode::NOT_FOUND, "NOT_FOUND", &e.to_string())
        }
        _ => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            &e.to_string(),
        ),
    }
}
