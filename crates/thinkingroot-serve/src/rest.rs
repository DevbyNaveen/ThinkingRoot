use std::path::PathBuf;
use std::sync::Arc;

use crate::graph::serve_graph;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};

use crate::engine::{ClaimFilter, QueryEngine};

// ─── App State ───────────────────────────────────────────────

pub struct AppState {
    pub engine: RwLock<QueryEngine>,
    pub api_key: Option<String>,
    pub mcp_sessions: crate::mcp::sse::SseSessionMap,
    /// Per-agent session state for the intelligent serve layer.
    pub sessions: crate::intelligence::session::SessionStore,
    /// Workspace root path for branch operations (None when multiple workspaces are mounted).
    pub workspace_root: Option<PathBuf>,
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
            engine: RwLock::new(engine),
            api_key,
            mcp_sessions: crate::mcp::sse::new_session_map(),
            sessions: crate::intelligence::session::new_session_store(),
            workspace_root,
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
            .route("/ws/{ws}/search", get(search))
            .route("/ws/{ws}/ask", post(ask_handler))
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
    };

    let result = ask(&engine, llm, &req).await;

    ok_response(AskResponseBody {
        answer: result.answer,
        claims_used: result.claims_used,
        category: result.category,
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
