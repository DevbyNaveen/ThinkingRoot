use std::path::PathBuf;
use std::sync::Arc;

use crate::graph::serve_graph;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};

use crate::engine::{ClaimFilter, QueryEngine};

// ─── App State ───────────────────────────────────────────────

pub struct AppState {
    /// Shared engine handle. Wrapped in `Arc<RwLock<…>>` (rather than the
    /// older bare `RwLock<…>`) so the agent loop's `ToolContext` can
    /// clone the same handle into multiple tool handlers without
    /// hopping through `Arc<AppState>`. All existing call sites that
    /// did `state.engine.read().await` keep working unchanged because
    /// `Arc<RwLock<T>>` derefs to `RwLock<T>`.
    pub engine: Arc<RwLock<QueryEngine>>,
    pub api_key: Option<String>,
    pub mcp_sessions: crate::mcp::sse::SseSessionMap,
    /// Per-agent session state for the intelligent serve layer.
    pub sessions: crate::intelligence::session::SessionStore,
    /// Workspace root path for branch operations (None when multiple workspaces are mounted).
    pub workspace_root: Option<PathBuf>,
    /// Pending agent-tool approvals, keyed by `tool_use_id`. The
    /// streaming `/ask/stream` handler inserts one entry per write
    /// tool the agent proposes; the `/ask/approval/{id}` POST handler
    /// looks up and fires the matching `oneshot::Sender` so the
    /// agent's `ChannelApprovalGate` unblocks. Both sides bound this
    /// shared map; nothing else writes to it.
    pub pending_approvals: crate::intelligence::approval::PendingApprovalMap,
}

impl AppState {
    /// Create a new `AppState` wrapped in `Arc`, initialising a fresh session map.
    /// Backward-compatible — workspace_root defaults to None.
    pub fn new(engine: QueryEngine, api_key: Option<String>) -> Arc<Self> {
        Self::new_with_root(engine, api_key, None)
    }

    /// Create a new `AppState` with an explicit workspace root path for branch operations.
    pub fn new_with_root(
        engine: QueryEngine,
        api_key: Option<String>,
        workspace_root: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine: Arc::new(RwLock::new(engine)),
            api_key,
            mcp_sessions: crate::mcp::sse::new_session_map(),
            sessions: crate::intelligence::session::new_session_store(),
            workspace_root,
            pending_approvals: crate::intelligence::approval::new_pending_approval_map(),
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
            .route("/ws/{ws}/llm/health", get(llm_health_handler))
            .route("/ws/{ws}/search", get(search))
            .route("/ws/{ws}/ask", post(ask_handler))
            .route("/ws/{ws}/ask/stream", post(ask_stream_handler))
            .route(
                "/ws/{ws}/ask/approval/{tool_use_id}",
                post(ask_approval_handler),
            )
            .route("/ws/{ws}/galaxy", get(get_galaxy))
            .route("/ws/{ws}/compile", post(compile))
            .route("/ws/{ws}/verify", post(verify_ws))
            // Branch endpoints
            .route(
                "/branches",
                get(list_branches_handler).post(create_branch_handler),
            )
            .route("/branches/{branch}/diff", get(diff_branch_handler))
            .route("/branches/{branch}/merge", post(merge_branch_handler))
            .route("/branches/{branch}/rollback", post(rollback_merge_handler))
            .route("/branches/{branch}/checkout", post(checkout_branch_handler))
            .route("/branches/{branch}", delete(delete_branch_handler))
            .route("/head", get(get_head_handler));
        router = router.nest("/api/v1", api_routes);
    }

    if enable_mcp {
        let mcp_routes = crate::mcp::sse::build_router(state.clone());
        router = router.nest("/mcp", mcp_routes);
    }

    // Apply CORS + auth middleware to the routes registered above.
    // Ops endpoints (/metrics, /readyz, /livez) are added AFTER .layer()
    // so monitoring scrapers don't need the API key. Axum only applies a
    // layer to routes already registered when `.layer()` was called.
    let routed = router.layer(cors).layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ));

    routed
        .route("/metrics", get(metrics_handler))
        .route("/readyz", get(readyz_handler))
        .route("/livez", get(livez_handler))
        .with_state(state)
}

// ─── Ops endpoints (unauthenticated) ─────────────────────────

async fn livez_handler() -> Response {
    // If this handler runs, the tokio reactor is alive enough to accept
    // requests. No deeper check — that's what /readyz is for.
    (StatusCode::OK, "ok\n").into_response()
}

async fn readyz_handler(State(state): State<Arc<AppState>>) -> Response {
    // Readiness = engine's workspace registry can be read without error.
    // Distinguishes "warming up" from "serving traffic". Cheap; suitable
    // for a 1-second probe cadence.
    let engine = state.engine.read().await;
    match engine.list_workspaces().await {
        Ok(_) => (StatusCode::OK, "ready\n").into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("not-ready: {e}\n")).into_response(),
    }
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> Response {
    // Prometheus text format 0.0.4. Minimal surface for v0.1 — extended
    // once we wire a histogram backend. HelloRoot's watchdog (spec O-11)
    // is the primary consumer.
    let mut out = String::new();
    out.push_str("# HELP thinkingroot_up Process uptime indicator (always 1 while serving).\n");
    out.push_str("# TYPE thinkingroot_up gauge\n");
    out.push_str("thinkingroot_up 1\n");

    out.push_str("# HELP thinkingroot_build_info Static build information as labels.\n");
    out.push_str("# TYPE thinkingroot_build_info gauge\n");
    out.push_str(&format!(
        "thinkingroot_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION"),
    ));

    // Workspace count — cheap read; bounded by the number of mounted
    // workspaces. Does not iterate entities/claims.
    let engine = state.engine.read().await;
    let ws_count = engine.list_workspaces().await.map(|v| v.len()).unwrap_or(0);
    out.push_str("# HELP thinkingroot_workspaces_total Number of mounted workspaces.\n");
    out.push_str("# TYPE thinkingroot_workspaces_total gauge\n");
    out.push_str(&format!("thinkingroot_workspaces_total {ws_count}\n"));

    // MCP active SSE sessions (ops signal for agent concurrency).
    // `SseSessionMap` is `Arc<Mutex<HashMap<..>>>` — use lock(), not read().
    let mcp_sessions = state.mcp_sessions.lock().await.len();
    out.push_str("# HELP thinkingroot_mcp_sessions_active Live MCP SSE sessions.\n");
    out.push_str("# TYPE thinkingroot_mcp_sessions_active gauge\n");
    out.push_str(&format!(
        "thinkingroot_mcp_sessions_active {mcp_sessions}\n"
    ));

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        out,
    )
        .into_response()
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

async fn get_galaxy(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.get_galaxy_map(&ws).await {
        Ok(map) => ok_response(map).into_response(),
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

// ─── Branch Handlers ─────────────────────────────────────────

async fn list_branches_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            // No workspace root set — return empty list (server started without --path)
            let empty: Vec<serde_json::Value> = vec![];
            return ok_response(serde_json::json!({ "branches": empty })).into_response();
        }
    };
    match thinkingroot_branch::list_branches(&root) {
        Ok(branches) => ok_response(serde_json::json!({ "branches": branches })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

async fn get_head_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            return ok_response(serde_json::json!({ "head": "main" })).into_response();
        }
    };
    match thinkingroot_branch::read_head_branch(&root) {
        Ok(head) => ok_response(serde_json::json!({ "head": head })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct CreateBranchRequest {
    name: String,
    parent: Option<String>,
    description: Option<String>,
}

async fn create_branch_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateBranchRequest>,
) -> impl IntoResponse {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let parent = body.parent.as_deref().unwrap_or("main");
    match thinkingroot_branch::create_branch(&root, &body.name, parent, body.description).await {
        Ok(branch) => ok_response(serde_json::json!({ "branch": branch })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

async fn delete_branch_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let engine = state.engine.read().await;
    match engine.delete_branch(&root, &branch).await {
        Ok(_) => ok_response(serde_json::json!({ "deleted": branch })).into_response(),
        Err(e) => err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &e.to_string()),
    }
}

async fn checkout_branch_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    match thinkingroot_branch::write_head_branch(&root, &branch) {
        Ok(_) => ok_response(serde_json::json!({ "head": branch })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

async fn diff_branch_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    use thinkingroot_branch::diff::compute_diff;
    use thinkingroot_branch::snapshot::resolve_data_dir;
    use thinkingroot_core::config::Config;
    use thinkingroot_graph::graph::GraphStore;

    let config = match Config::load_merged(&root) {
        Ok(c) => c,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONFIG_ERROR",
                &e.to_string(),
            );
        }
    };
    let mc = &config.merge;
    let main_data_dir = resolve_data_dir(&root, None);
    let branch_data_dir = resolve_data_dir(&root, Some(&branch));

    if !branch_data_dir.exists() {
        return err_response(
            StatusCode::NOT_FOUND,
            "BRANCH_NOT_FOUND",
            &format!("branch '{}' not found", branch),
        );
    }

    let main_graph = match GraphStore::init(&main_data_dir.join("graph")) {
        Ok(g) => g,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &e.to_string(),
            );
        }
    };
    let branch_graph = match GraphStore::init(&branch_data_dir.join("graph")) {
        Ok(g) => g,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &e.to_string(),
            );
        }
    };

    match compute_diff(
        &main_graph,
        &branch_graph,
        &branch,
        mc.auto_resolve_threshold,
        mc.max_health_drop,
        mc.block_on_contradictions,
    ) {
        Ok(diff) => ok_response(diff).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DIFF_ERROR",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct MergeBranchRequest {
    force: Option<bool>,
    propagate_deletions: Option<bool>,
}

async fn merge_branch_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
    body: Option<Json<MergeBranchRequest>>,
) -> impl IntoResponse {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    use thinkingroot_core::MergedBy;

    let force = body.as_ref().and_then(|b| b.force).unwrap_or(false);
    let propagate_deletions = body
        .as_ref()
        .and_then(|b| b.propagate_deletions)
        .unwrap_or(false);

    let engine = state.engine.read().await;
    match engine
        .merge_branch(
            &root,
            &branch,
            force,
            propagate_deletions,
            MergedBy::Human {
                user: "api".to_string(),
            },
        )
        .await
    {
        Ok(diff) => ok_response(serde_json::json!({
            "merged": branch,
            "new_claims": diff.new_claims.len(),
            "new_entities": diff.new_entities.len(),
            "auto_resolved": diff.auto_resolved.len(),
        }))
        .into_response(),
        Err(thinkingroot_core::Error::EntityNotFound(msg)) => {
            err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &msg)
        }
        Err(e) => err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "MERGE_BLOCKED",
            &e.to_string(),
        ),
    }
}

async fn rollback_merge_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };

    let engine = state.engine.read().await;
    match engine.rollback_merge(&root, &branch).await {
        Ok(()) => ok_response(serde_json::json!({
            "rolled_back": branch,
        }))
        .into_response(),
        Err(thinkingroot_core::Error::EntityNotFound(msg)) => {
            err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &msg)
        }
        Err(e) => err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ROLLBACK_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── Intelligence Ask Endpoint ───────────────────────────────

/// POST /api/v1/ws/{workspace}/ask
///
/// Runs the full hybrid retrieval + synthesis pipeline proven at 91.2% on
/// LongMemEval-500. Returns a synthesized natural-language answer with source
/// attribution.
///
/// Body:
/// ```json
/// {
///   "question": "What time did I reach the clinic on Monday?",
///   "session_scope": ["session_001", "session_002"],  // optional
///   "question_date": "2023/05/30 (Tue) 22:10",        // optional, for temporal
///   "category_hint": "temporal-reasoning"              // optional
/// }
/// ```

#[derive(Deserialize)]
struct AskRequest {
    question: String,
    #[serde(default)]
    session_scope: Vec<String>,
    #[serde(default)]
    question_date: String,
    #[serde(default)]
    category_hint: String,
    /// Recent conversation turns (oldest-first) the synthesizer should
    /// treat as memory. Empty = single-shot mode and the wire prompt is
    /// byte-identical to v0.9.0. The desktop chat surface pins the last
    /// 6-8 turns here once Sprint S5 wires it through; the LongMemEval
    /// bench harness leaves it empty so the contract holds.
    #[serde(default)]
    history: Vec<ChatTurnPayload>,
    /// When `true`, route the chat through the multi-turn tool-using
    /// agent (S3) instead of one-shot retrieval-and-synthesise. Only
    /// honoured by `/ask/stream` and only when the workspace has a
    /// `Conversational` persona resolved. Defaults to `false` so
    /// existing CLI / API clients keep their byte-stable behaviour;
    /// the desktop chat surface flips it to `true` once the UI is
    /// wired to render `tool_call_*` SSE events.
    #[serde(default)]
    use_agent: bool,
    /// Stable identifier for this conversation. Used by the agent
    /// path as the MCP session id (which scopes
    /// `contribute_claim`'s active branch and provenance). When
    /// missing, the streaming handler synthesises a fresh UUID per
    /// request, which means each turn looks like a brand-new
    /// session — fine for stateless flows, breaks per-conversation
    /// active-branch tracking, so callers that want continuity
    /// must pass this.
    #[serde(default)]
    conversation_id: Option<String>,
}

/// Wire-format conversation turn. Mirrors the OpenAI Chat Completions /
/// Anthropic Messages role string so the JSON travels through any
/// front-end without translation. Unknown roles (i.e. `tool`, `system`)
/// are silently dropped — the synthesizer is a strict 2-role consumer.
#[derive(Deserialize)]
struct ChatTurnPayload {
    role: String,
    content: String,
}

/// Translate the wire-format `[{role, content}, ...]` history into the
/// synthesizer's internal `Vec<ChatTurn>`. Unknown roles are skipped
/// (rather than failing the request) so a misbehaving client cannot
/// take down the chat surface — the worst case is the synthesizer sees
/// fewer turns than the client thought it sent. Empty `content` strings
/// are also dropped to keep the prompt tight.
fn decode_history(
    payload: &[ChatTurnPayload],
) -> Vec<crate::intelligence::synthesizer::ChatTurn> {
    use crate::intelligence::synthesizer::{ChatRole, ChatTurn};
    payload
        .iter()
        .filter_map(|t| {
            let role = match t.role.as_str() {
                "user" => ChatRole::User,
                "assistant" => ChatRole::Assistant,
                _ => return None,
            };
            let content = t.content.trim();
            if content.is_empty() {
                return None;
            }
            Some(ChatTurn {
                role,
                content: content.to_string(),
            })
        })
        .collect()
}

#[derive(Serialize)]
struct AskResponseBody {
    answer: String,
    claims_used: usize,
    category: String,
}

async fn ask_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<AskRequest>,
) -> Response {
    use crate::intelligence::identity::build_workspace_identity;
    use crate::intelligence::synthesizer::{AskRequest as SynthAskRequest, ask};
    use std::collections::HashMap;
    use std::collections::HashSet;

    let engine = state.engine.read().await;

    // Resolve workspace root for sessions directory.
    // Prefer AppState.workspace_root (set by --path), fall back to engine's per-workspace root.
    let sessions_dir = state
        .workspace_root
        .as_ref()
        .cloned()
        .or_else(|| engine.workspace_root_path(&ws))
        .map(|p| p.join("sessions"))
        .unwrap_or_else(|| std::path::PathBuf::from("sessions"));

    // If no session_scope provided, use an empty set (no scoping — all claims allowed)
    let allowed_sources: HashSet<String> = body.session_scope.iter().cloned().collect();

    // Infer category from hint or router
    let category = if !body.category_hint.is_empty() {
        body.category_hint.clone()
    } else {
        // Use the query router to infer category
        let tmp_session = crate::intelligence::session::SessionContext::new("_ask", &ws);
        match crate::intelligence::router::classify_query(&body.question, &tmp_session) {
            crate::intelligence::router::QueryPath::Agentic => {
                let q = body.question.to_lowercase();
                if q.contains("when")
                    || q.contains(" ago")
                    || q.contains("last ")
                    || q.contains("how many days")
                {
                    "temporal-reasoning".to_string()
                } else {
                    "multi-session".to_string()
                }
            }
            crate::intelligence::router::QueryPath::Fast => "single-session-user".to_string(),
        }
    };

    // Retrieve the LLM client from the engine's workspace config
    let llm = engine.workspace_llm(&ws);

    // Workspace identity / persona — the chat-time prompt structure that
    // anchors the model to *this* workspace. Falls back to the
    // Memory/Terse default (identity=None) when the workspace isn't
    // mounted, preserving the v0.9.0 LongMemEval-91.2% wire prompt
    // for tests / harnesses.
    let snapshot = engine.workspace_chat_snapshot(&ws).await;
    let chat = snapshot
        .as_ref()
        .map(|s| s.config.chat.resolve(&s.source_kinds))
        .unwrap_or_else(SynthAskRequest::default_chat);
    let identity_owned = snapshot
        .as_ref()
        .map(|s| build_workspace_identity(s, &s.config.chat));
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let history = decode_history(&body.history);

    let req = SynthAskRequest {
        workspace: &ws,
        question: &body.question,
        category: &category,
        allowed_sources: &allowed_sources,
        question_date: &body.question_date,
        session_dates: &HashMap::new(),
        answer_sids: &body.session_scope,
        sessions_dir: &sessions_dir,
        excluded_claim_ids: &HashSet::new(),
        chat,
        identity: identity_owned.as_ref(),
        today: Some(&today),
        history: &history,
    };

    let result = ask(&engine, llm, &req).await;

    ok_response(AskResponseBody {
        answer: result.answer,
        claims_used: result.claims_used,
        category: result.category,
    })
    .into_response()
}

// ─── Streaming Ask (SSE) ─────────────────────────────────────

/// POST /api/v1/ws/{workspace}/ask/stream
///
/// Server-Sent-Events variant of `/ask`. Same retrieval pipeline,
/// same prompt — but the LLM call goes through `chat_stream` and
/// chunks are forwarded incrementally so the desktop chat surface
/// renders tokens as they arrive instead of after the full
/// synthesis finishes.
///
/// Event sequence on the wire (all `data:` is JSON):
///
/// ```text
/// event: meta
/// data: {"claims_used":12,"category":"single-session-user"}
///
/// event: token
/// data: {"text":"The"}
///
/// event: token
/// data: {"text":" answer"}
///
/// event: final
/// data: {"claims_used":12,"category":"single-session-user","truncated":false}
///
/// event: error
/// data: {"message":"connect: ..."}     # only on failure
/// ```
///
/// Static branch (no claims OR no LLM): emits one `meta` event
/// then a single `token` carrying the full fallback text plus a
/// `final` — so the desktop never has to special-case "static
/// vs streamed" on its end.
async fn ask_stream_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<AskRequest>,
) -> impl IntoResponse {
    use crate::intelligence::identity::build_workspace_identity;
    use crate::intelligence::synthesizer::{
        AskRequest as SynthAskRequest, StreamingAnswer, ask_streaming,
    };
    use futures::StreamExt;
    use std::collections::{HashMap, HashSet};

    // Agent path branches off as early as possible: it has its own
    // event stream (tool_call_* + token + final + error) and reuses
    // none of the one-shot retrieval scaffolding below.
    if body.use_agent {
        return agent_stream_response(state.clone(), ws, body).await;
    }

    let engine = state.engine.read().await;

    let sessions_dir = state
        .workspace_root
        .as_ref()
        .cloned()
        .or_else(|| engine.workspace_root_path(&ws))
        .map(|p| p.join("sessions"))
        .unwrap_or_else(|| std::path::PathBuf::from("sessions"));

    let allowed_sources: HashSet<String> = body.session_scope.iter().cloned().collect();

    let category = if !body.category_hint.is_empty() {
        body.category_hint.clone()
    } else {
        let tmp_session = crate::intelligence::session::SessionContext::new("_ask", &ws);
        match crate::intelligence::router::classify_query(&body.question, &tmp_session) {
            crate::intelligence::router::QueryPath::Agentic => {
                let q = body.question.to_lowercase();
                if q.contains("when")
                    || q.contains(" ago")
                    || q.contains("last ")
                    || q.contains("how many days")
                {
                    "temporal-reasoning".to_string()
                } else {
                    "multi-session".to_string()
                }
            }
            crate::intelligence::router::QueryPath::Fast => "single-session-user".to_string(),
        }
    };

    let llm = engine.workspace_llm(&ws);
    let answer_sids = body.session_scope.clone();

    let snapshot = engine.workspace_chat_snapshot(&ws).await;
    let chat = snapshot
        .as_ref()
        .map(|s| s.config.chat.resolve(&s.source_kinds))
        .unwrap_or_else(SynthAskRequest::default_chat);
    let identity_owned = snapshot
        .as_ref()
        .map(|s| build_workspace_identity(s, &s.config.chat));
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let history = decode_history(&body.history);

    let req = SynthAskRequest {
        workspace: &ws,
        question: &body.question,
        category: &category,
        allowed_sources: &allowed_sources,
        question_date: &body.question_date,
        session_dates: &HashMap::new(),
        answer_sids: &answer_sids,
        sessions_dir: &sessions_dir,
        excluded_claim_ids: &HashSet::new(),
        chat,
        identity: identity_owned.as_ref(),
        today: Some(&today),
        history: &history,
    };

    let outcome = ask_streaming(&engine, llm, &req).await;
    drop(engine);

    let stream = async_stream::stream! {
        match outcome {
            StreamingAnswer::Static { answer, claims_used, category } => {
                let meta = serde_json::json!({
                    "claims_used": claims_used,
                    "category": category,
                });
                yield Ok::<Event, std::convert::Infallible>(
                    Event::default().event("meta").data(meta.to_string())
                );
                if !answer.is_empty() {
                    let payload = serde_json::json!({ "text": answer });
                    yield Ok(
                        Event::default().event("token").data(payload.to_string())
                    );
                }
                let final_payload = serde_json::json!({
                    "claims_used": claims_used,
                    "category": category,
                    "truncated": false,
                });
                yield Ok(
                    Event::default().event("final").data(final_payload.to_string())
                );
            }
            StreamingAnswer::Stream { mut stream, claims_used, category } => {
                let meta = serde_json::json!({
                    "claims_used": claims_used,
                    "category": category,
                });
                yield Ok(
                    Event::default().event("meta").data(meta.to_string())
                );
                let mut truncated = false;
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(chunk) => {
                            if !chunk.text.is_empty() {
                                let payload =
                                    serde_json::json!({ "text": chunk.text });
                                yield Ok(
                                    Event::default()
                                        .event("token")
                                        .data(payload.to_string())
                                );
                            }
                            if let Some(finish) = chunk.finish {
                                truncated = finish.truncated;
                            }
                        }
                        Err(e) => {
                            let payload =
                                serde_json::json!({ "message": e.to_string() });
                            yield Ok(
                                Event::default()
                                    .event("error")
                                    .data(payload.to_string())
                            );
                            return;
                        }
                    }
                }
                let final_payload = serde_json::json!({
                    "claims_used": claims_used,
                    "category": category,
                    "truncated": truncated,
                });
                yield Ok(
                    Event::default().event("final").data(final_payload.to_string())
                );
            }
        }
    };

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ─── Agent streaming response (S5) ───────────────────────────
//
// When the request body sets `use_agent: true`, the streaming
// handler routes here instead of the one-shot retrieve-and-synthesise
// path above. The agent (S3) drives a multi-turn loop calling tools,
// gating writes through `ToolApprovalRouter` (which suspends on a
// oneshot until `/ask/approval/{id}` resolves it), and emitting
// `AgentEvent`s through an mpsc channel.
//
// Wire shape on the SSE stream — every `AgentEvent` becomes one
// `event:` line:
//
//   event: token                  # AgentEvent::Text
//   event: tool_call_proposed     # incl. {id, name, input, is_write}
//   event: tool_call_executing    # incl. {id, name}
//   event: tool_call_finished     # incl. {id, name, content, is_error}
//   event: tool_call_rejected     # incl. {id, name, reason}
//   event: final                  # AgentEvent::Done
//   event: error                  # AgentEvent::Error
//
// In addition, when the agent emits `tool_call_proposed` with
// `is_write: true`, the handler registers a oneshot in
// `state.pending_approvals` keyed by the tool_use_id and emits an
// `approval_requested` SSE event so the desktop UI can render its
// claim card. The UI then POSTs the decision to
// `/ask/approval/{tool_use_id}`.
async fn agent_stream_response(
    state: Arc<AppState>,
    ws: String,
    body: AskRequest,
) -> Response {
    use crate::intelligence::agent::AgentEvent;
    use crate::intelligence::agent_streaming::{
        StreamAgentDeps, StreamAgentRequest, agent_event_to_sse, spawn_agent_run,
    };
    use crate::intelligence::skills::SkillRegistry;
    use crate::intelligence::synthesizer::{
        AskRequest as SynthAskRequest, ChatRole, ChatTurn, build_system_prompt,
        compose_full_system_prompt,
    };

    // Snapshot engine state we need before releasing the read lock —
    // the agent path goes async via spawn() and can't hold a guard
    // across .await without serialising every concurrent agent.
    let engine = state.engine.read().await;
    let llm = match engine.workspace_llm(&ws) {
        Some(c) => c,
        None => {
            let payload = serde_json::json!({
                "message": format!("workspace '{ws}' has no LLM configured")
            });
            let stream = async_stream::stream! {
                yield Ok::<Event, std::convert::Infallible>(
                    Event::default().event("error").data(payload.to_string())
                );
            };
            return Sse::new(stream)
                .keep_alive(KeepAlive::new().text("keep-alive"))
                .into_response();
        }
    };
    let workspace_root = state
        .workspace_root
        .as_ref()
        .cloned()
        .or_else(|| engine.workspace_root_path(&ws));
    let snapshot = engine.workspace_chat_snapshot(&ws).await;
    let chat = snapshot
        .as_ref()
        .map(|s| s.config.chat.resolve(&s.source_kinds))
        .unwrap_or_else(SynthAskRequest::default_chat);
    drop(engine);

    let Some(workspace_root) = workspace_root else {
        let payload = serde_json::json!({
            "message": format!("workspace '{ws}' has no on-disk root mounted; agent path requires one")
        });
        let stream = async_stream::stream! {
            yield Ok::<Event, std::convert::Infallible>(
                Event::default().event("error").data(payload.to_string())
            );
        };
        return Sse::new(stream)
            .keep_alive(KeepAlive::new().text("keep-alive"))
            .into_response();
    };

    // Skills live at <workspace_root>/.thinkingroot/skills/. Empty
    // dir or missing dir → empty registry; skill manifest will not
    // be appended to the system prompt.
    let skill_dir = workspace_root.join(".thinkingroot/skills");
    let skills = match SkillRegistry::load_from_dir(&skill_dir) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            tracing::warn!("agent: skill load failed at {}: {e}", skill_dir.display());
            Arc::new(SkillRegistry::empty())
        }
    };

    // Compose the full system prompt: persona + (no style — styles
    // are resolved server-side from `[chat]` config in a future
    // sprint) + skill manifest.
    let system_prompt = compose_full_system_prompt(chat, None, Some(&skills));
    let _ = build_system_prompt; // re-export for callers that want raw

    // Translate wire-format history into ChatTurn → ChatMessage.
    let chat_history: Vec<ChatTurn> = body
        .history
        .iter()
        .filter_map(|t| {
            let role = match t.role.as_str() {
                "user" => ChatRole::User,
                "assistant" => ChatRole::Assistant,
                _ => return None,
            };
            let content = t.content.trim();
            if content.is_empty() {
                return None;
            }
            Some(ChatTurn {
                role,
                content: content.to_string(),
            })
        })
        .collect();
    let agent_messages =
        crate::intelligence::agent::chat_turns_to_messages(&chat_history);

    let conversation_id = body
        .conversation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let req = StreamAgentRequest {
        workspace: ws.clone(),
        workspace_root,
        session_id: conversation_id,
        agent_id: "thinkingroot".to_string(),
        system_prompt,
        user_question: body.question.clone(),
        history: agent_messages,
        skills,
    };
    let deps = StreamAgentDeps {
        engine: state.engine.clone(),
        llm,
        sessions: state.sessions.clone(),
        pending_approvals: state.pending_approvals.clone(),
        trace: None,
    };

    let (mut rx, router) = spawn_agent_run(req, deps);

    // The streaming task watches the event channel. For every
    // `tool_call_proposed` with `is_write: true`, it (1) tells the
    // ToolApprovalRouter to register a pending oneshot under the
    // tool_use_id, and (2) emits an `approval_requested` SSE event
    // so the desktop UI can render its claim card. The matching
    // POST to `/ask/approval/{id}` resolves the oneshot and the
    // agent unblocks.
    let stream = async_stream::stream! {
        // Surface a cheap meta event up front so UIs that show a
        // "category" header have something to render before tokens
        // start flowing. claims_used is unknown from the agent
        // (tools may produce many results), so we report 0 for now.
        let meta = serde_json::json!({
            "claims_used": 0,
            "category": "agentic",
        });
        yield Ok::<Event, std::convert::Infallible>(
            Event::default().event("meta").data(meta.to_string())
        );

        while let Some(event) = rx.recv().await {
            // Side effect: write proposals need a pending-id
            // registration BEFORE the agent's gate.check fires.
            // The agent emits ToolCallProposed before calling the
            // gate, so we have a small window to set this up.
            if let AgentEvent::ToolCallProposed { id, is_write, name, input } = &event {
                if *is_write {
                    router.set_pending_id(id.clone()).await;
                    let payload = serde_json::json!({
                        "id": id,
                        "name": name,
                        "input": input,
                    });
                    yield Ok(
                        Event::default()
                            .event("approval_requested")
                            .data(payload.to_string())
                    );
                }
            }

            let (kind, payload) = agent_event_to_sse(&event);
            yield Ok(
                Event::default().event(kind).data(payload.to_string())
            );

            // Terminal events end the stream.
            if matches!(event, AgentEvent::Done { .. }) {
                break;
            }
        }
    };

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ─── Approval POST handler (S5) ──────────────────────────────
//
// POST /api/v1/ws/{ws}/ask/approval/{tool_use_id}
// Body: {"decision": "approve" | "reject", "reason": "..."}
//
// Resolves the matching pending oneshot in `state.pending_approvals`,
// unblocking the agent's `ToolApprovalRouter::check`. The `ws` path
// param is currently unused (every tool_use_id is globally unique
// across workspaces) but kept in the URL so future per-workspace
// scoping is a non-breaking change.

#[derive(Deserialize)]
struct ApprovalRequestBody {
    /// Either "approve" or "reject". Anything else is treated as
    /// rejection so a malformed client can't sneak through.
    decision: String,
    /// Optional human-readable reason. Surfaced to the LLM via the
    /// `tool_call_rejected` event when the decision is "reject".
    #[serde(default)]
    reason: Option<String>,
}

async fn ask_approval_handler(
    State(state): State<Arc<AppState>>,
    Path((_ws, tool_use_id)): Path<(String, String)>,
    Json(body): Json<ApprovalRequestBody>,
) -> Response {
    use crate::intelligence::approval::{ApprovalDecision, ToolApprovalRouter};

    let decision = match body.decision.as_str() {
        "approve" | "approved" => ApprovalDecision::Approved,
        _ => ApprovalDecision::Rejected {
            reason: body
                .reason
                .unwrap_or_else(|| "user declined".to_string()),
        },
    };

    let resolved =
        ToolApprovalRouter::resolve(&state.pending_approvals, &tool_use_id, decision).await;

    if resolved {
        ok_response(serde_json::json!({"resolved": true})).into_response()
    } else {
        err_response(
            StatusCode::NOT_FOUND,
            "NO_PENDING_APPROVAL",
            &format!("no pending approval for tool_use_id '{tool_use_id}'"),
        )
    }
}

// ─── LLM Health (pre-flight) ─────────────────────────────────

/// GET /api/v1/ws/{ws}/llm/health
///
/// Cheap pre-flight the desktop calls on workspace switch. Tells the user
/// up-front whether `ask` will produce a real LLM-synthesised answer or fall
/// back to the top-claim statement, so the chat UI never spins for 120 s on a
/// silently-unconfigured workspace.
#[derive(Serialize)]
struct LlmHealthBody {
    /// True iff a provider+key resolved at workspace mount time.
    configured: bool,
    /// Provider name (e.g. "anthropic", "azure"). `None` when unconfigured.
    provider: Option<String>,
    /// Display model name. `None` when unconfigured.
    model: Option<String>,
    /// Number of claims compiled into this workspace — `0` means the engine
    /// will return the "not enough information" fallback regardless of LLM.
    claim_count: usize,
    /// Whether the workspace is mounted at all. `false` → 404-equivalent;
    /// the desktop should refuse to chat against a non-existent workspace.
    mounted: bool,
}

async fn llm_health_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;

    // Use the engine's existing workspace-info call: it returns the claim
    // count alongside identity, so one call covers `mounted` + `claim_count`.
    let info = engine
        .list_workspaces()
        .await
        .ok()
        .and_then(|list| list.into_iter().find(|w| w.name == ws));
    let Some(info) = info else {
        return ok_response(LlmHealthBody {
            configured: false,
            provider: None,
            model: None,
            claim_count: 0,
            mounted: false,
        })
        .into_response();
    };

    let llm = engine.workspace_llm(&ws);
    let configured = llm.is_some();
    let (provider, model) = match llm.as_deref() {
        Some(c) => (
            Some(c.provider_name().to_string()),
            Some(c.model_name().to_string()),
        ),
        None => (None, None),
    };

    ok_response(LlmHealthBody {
        configured,
        provider,
        model,
        claim_count: info.claim_count,
        mounted: true,
    })
    .into_response()
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
