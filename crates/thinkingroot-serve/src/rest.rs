use std::collections::HashMap;
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
use tokio::sync::{RwLock, broadcast};
use tower_http::cors::{Any, CorsLayer};

use crate::engine::{ClaimFilter, QueryEngine};
use thinkingroot_core::BranchEvent;

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
    /// RARP / Active Engram Protocol manager — owns per-session
    /// materialised Engrams and serves the 4 new MCP tools.
    pub engram_manager: Arc<crate::intelligence::engram::EngramManager>,
    /// T1.6 — per-branch broadcast channels for live SSE event streams.
    /// Keyed by branch name within `workspace_root`.  Each channel is
    /// created lazily on first subscriber, has capacity 64 (branch
    /// events are infrequent — one per merge / abandon /
    /// redaction-update — so the small buffer keeps slow consumers from
    /// blocking the writer), and is reused across reconnects.  The
    /// previously-shipped `BranchEvent` log on `BranchRef` remains the
    /// source of truth; this hub is a fan-out for clients that prefer
    /// live updates over polling `/branches/{branch}/events`.
    pub branch_event_hub: Arc<RwLock<HashMap<String, broadcast::Sender<BranchEvent>>>>,
    /// T1.5 — in-flight merge `CancellationToken`s keyed by merge id
    /// (a ULID generated at handler entry).  `POST /merges/{id}/cancel`
    /// looks up and trips the matching token; the merge phase-boundary
    /// check inside `execute_merge_into_cancellable` returns
    /// `Error::Cancelled` at the next safe point.  Tokens are removed
    /// from the map on every exit path (success, failure, cancellation)
    /// by the merge handler so a long-cancelled merge never leaks.
    pub active_merges: Arc<RwLock<HashMap<String, tokio_util::sync::CancellationToken>>>,
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
            engram_manager: crate::intelligence::engram::EngramManager::new(
                crate::intelligence::engram::EngramConfig::default(),
            ),
            branch_event_hub: Arc::new(RwLock::new(HashMap::new())),
            active_merges: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// T1.6 — get-or-create the broadcast channel for a branch.  The
    /// returned `Sender` is cloneable; subscribers call `subscribe()`
    /// on it to obtain a `Receiver`.  Capacity is fixed at 64; on
    /// overflow the oldest events are dropped and slow subscribers see
    /// `RecvError::Lagged` — surfaced to the SSE client as a `lagged`
    /// event so they can refetch via the polling endpoint.
    pub async fn branch_event_sender(&self, branch: &str) -> broadcast::Sender<BranchEvent> {
        if let Some(tx) = self.branch_event_hub.read().await.get(branch).cloned() {
            return tx;
        }
        let mut map = self.branch_event_hub.write().await;
        map.entry(branch.to_string())
            .or_insert_with(|| broadcast::channel(64).0)
            .clone()
    }
}

/// T1.6 — read the latest event for a branch from the on-disk
/// registry and broadcast it on the corresponding channel.
///
/// Called after every successful branch mutation handler.  No-op when
/// the branch has no events (defensive — registries written before
/// the audit log shipped have an empty `events` vector and round-trip
/// via `#[serde(default)]`).
///
/// `broadcast::Sender::send` only fails when the channel has zero
/// receivers; we ignore that error because subscribers may attach at
/// any time and the polling endpoint serves them the full history.
///
/// `pub` so integration tests can drive it directly without wiring
/// every mutation handler into the test setup.  Production code
/// already calls it from inside the rest crate after every successful
/// branch mutation.
pub async fn publish_latest_branch_event(state: &AppState, branch: &str) {
    let Some(root) = state.workspace_root.as_ref() else {
        return;
    };
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::branch::BranchRegistry;
    let Ok(registry) = BranchRegistry::load_or_create(&refs_dir) else {
        return;
    };
    let Some(event) = registry
        .all()
        .into_iter()
        .find(|b| b.name == branch)
        .and_then(|b| b.events.last().cloned())
    else {
        return;
    };
    let tx = state.branch_event_sender(branch).await;
    let _ = tx.send(event);
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

fn request_user(headers: &HeaderMap) -> Option<String> {
    ["x-thinkingroot-user", "x-user"]
        .into_iter()
        .find_map(|name| headers.get(name))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
            .route(
                "/workspaces",
                get(list_workspaces).post(mount_workspace_handler),
            )
            .route(
                "/workspaces/{name}",
                delete(unmount_workspace_handler),
            )
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
            .route("/ws/{ws}/search/hybrid", post(hybrid_search_handler))
            // T3.2 — cross-branch reflect.  Body is `{ branches: [...] }`;
            // returns a `CrossBranchReflectResult` JSON with per-branch
            // outcomes + divergent-pattern rows.
            .route(
                "/ws/{ws}/reflect/across-branches",
                post(reflect_across_branches_handler),
            )
            // T2.4 — bitemporal "as-of" claim list.  Query parameter
            // `as_of` carries an ISO-8601 timestamp; the optional
            // `branch` query parameter scopes to a non-main branch.
            // Returns the claims that existed at or before that
            // moment (i.e. their `created_at <= as_of`).
            .route("/ws/{ws}/claims/as-of", get(claims_as_of_handler))
            // RARP / Active Engram Protocol — engram lifecycle endpoints
            // mirror the 4 MCP tools (`materialize_engram`, `probe_engram`,
            // `list_engrams`, `expire_engram`) so HTTP-only consumers
            // (Python/TS SDKs, CLI scripts) reach AEP without an MCP
            // transport. Session id is required and passed via
            // `X-TR-Session-Id` header — matches the SSE-MCP pattern.
            .route(
                "/ws/{ws}/engrams",
                get(list_engrams_handler).post(materialize_engram_handler),
            )
            .route(
                "/ws/{ws}/engrams/{ptr}",
                delete(expire_engram_handler),
            )
            .route(
                "/ws/{ws}/engrams/{ptr}/probe",
                post(probe_engram_handler),
            )
            .route("/ws/{ws}/ask", post(ask_handler))
            .route("/ws/{ws}/ask/stream", post(ask_stream_handler))
            .route(
                "/ws/{ws}/ask/approval/{tool_use_id}",
                post(ask_approval_handler),
            )
            .route("/ws/{ws}/galaxy", get(get_galaxy))
            .route("/ws/{ws}/compile", post(compile))
            .route("/ws/{ws}/compile/stream", post(compile_stream))
            .route("/ws/{ws}/verify", post(verify_ws))
            // Branch endpoints
            .route(
                "/branches",
                get(list_branches_handler).post(create_branch_handler),
            )
            .route("/branches/{branch}/diff", get(diff_branch_handler))
            .route("/branches/{branch}/merge", post(merge_branch_handler))
            .route(
                "/branches/{source}/merge-into/{target}",
                post(merge_into_branch_handler),
            )
            // T1.5 — cancel an in-flight merge by id.  The id is the
            // ULID returned in the merge response; cancellation flips
            // the token so the merge exits with `Error::Cancelled` at
            // the next phase boundary.
            .route("/merges/{id}/cancel", post(cancel_merge_handler))
            .route("/branches/{branch}/rebase", post(rebase_branch_handler))
            .route("/branches/{branch}/rollback", post(rollback_merge_handler))
            .route("/branches/{branch}/checkout", post(checkout_branch_handler))
            .route("/branches/{branch}", delete(delete_branch_handler))
            // T0.7 — connector-attributed bulk contribute with idempotency.
            .route(
                "/branches/{branch}/contribute-bulk",
                post(contribute_bulk_handler),
            )
            // T2.6 — per-branch outbound redaction policy.
            .route(
                "/branches/{branch}/redaction",
                post(set_branch_redaction_handler),
            )
            // T1.3 — branch audit log. Every state-changing mutation
            // appends a `BranchEvent` to the BranchRef's events vec;
            // this route exposes that log read-only.
            .route("/branches/{branch}/events", get(list_branch_events_handler))
            // T1.6 — live SSE stream of branch events.  Subscribers
            // attach to a per-branch broadcast channel; mutations in
            // the registry publish to it after they commit.  Pairs
            // with `/branches/{branch}/events` (history) so a client
            // can backfill on connect, then follow the live stream.
            .route(
                "/branches/{branch}/events/stream",
                get(stream_branch_events_handler),
            )
            // T1.2 — fast per-branch stats (claims/entities/sources)
            // without running a full diff.
            .route("/branches/{branch}/stats", get(branch_stats_handler))
            // T1.7 — lineage DAG aggregating fork/merge edges across
            // every branch in the registry (active + merged +
            // abandoned).
            .route("/branches/lineage", get(branch_lineage_handler))
            // T2.5 — tag create + list. Writes are rejected by the
            // immutability gate at `engine::ensure_branch_permission`
            // (lives since T0.6); this surface is what gives the gate
            // live data to gate against.
            .route("/tags", get(list_tags_handler).post(create_tag_handler))
            .route("/tags/{name}", get(get_tag_handler))
            // T3.7 — branch templates.  CRUD for the
            // workspace-scoped `branch_templates.toml`; consumed by
            // `POST /branches { template: "..." }` to materialise a
            // pre-baked merge policy / kind / TTL bundle.
            .route(
                "/branch-templates",
                get(list_branch_templates_handler).post(upsert_branch_template_handler),
            )
            .route(
                "/branch-templates/{name}",
                get(get_branch_template_handler).delete(delete_branch_template_handler),
            )
            // T0.4 — Knowledge Proposal lifecycle. The
            // `RequiresProposal` merge gate (`merge.rs:336`) consults
            // `find_approved_proposal` on these files; routes here are
            // the only way to advance a proposal through the
            // open→review→approve states.
            .route(
                "/branches/{branch}/proposals",
                get(list_branch_proposals_handler).post(open_proposal_handler),
            )
            .route("/proposals", get(list_all_proposals_handler))
            .route("/proposals/{id}", get(get_proposal_handler))
            .route(
                "/proposals/{id}/reviews",
                post(review_proposal_handler),
            )
            .route("/proposals/{id}/close", post(close_proposal_handler))
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

// ─── Workspace mount/unmount (cortex-aware tr-mount target) ─────
//
// `POST /api/v1/workspaces` accepts `{ name, root_path }` and mounts
// the workspace into the running daemon's engine. This is the seam
// the `root mount` CLI subcommand uses after unpacking a `.tr` pack
// — the unpacked `<dir>/.thinkingroot/` becomes a workspace the
// cortex daemon can serve to MCP clients (Claude Desktop, Cursor,
// etc.) without restart.
//
// `DELETE /api/v1/workspaces/{name}` is the symmetric unmount. Both
// honour the cortex contract: they mutate `engine.workspaces` under
// the engine write-lock, which serialises with the read paths used
// by every other handler (search, claims, AEP) so a concurrent
// query never observes a half-mounted workspace.

#[derive(Debug, Deserialize)]
struct MountWorkspaceRequest {
    name: String,
    root_path: String,
    /// Optional explicit data directory (defaults to
    /// `<root_path>/.thinkingroot/`). Set this when the data dir
    /// lives outside the workspace root — for example, the tr-mount
    /// flow stages `XDG_DATA_HOME/thinkingroot/mounts/<hash>/` and
    /// passes both as separate paths.
    #[serde(default)]
    data_dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct MountWorkspaceResponse {
    name: String,
    root_path: String,
    entity_count: usize,
    claim_count: usize,
    source_count: usize,
    /// Public REST root for this workspace — clients append entity /
    /// claim / engram paths under this prefix.
    rest_url: String,
    /// MCP SSE endpoint (clients connect over SSE for the standard
    /// RARP/Hybrid tool surface).
    mcp_url: String,
}

async fn mount_workspace_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MountWorkspaceRequest>,
) -> Response {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "BAD_REQUEST", "name is required");
    }
    let root_path = PathBuf::from(&body.root_path);
    if !root_path.is_absolute() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "root_path must be absolute",
        );
    }

    let mut engine = state.engine.write().await;
    let mount_result = match body.data_dir.as_deref() {
        Some(dd) => {
            let data_dir = PathBuf::from(dd);
            engine
                .mount_with_data_dir(name.clone(), root_path.clone(), data_dir)
                .await
        }
        None => engine.mount(name.clone(), root_path.clone()).await,
    };
    if let Err(e) = mount_result {
        return match_engine_error(e);
    }

    // Pull a fresh count so the response carries the substrate size
    // the SDK can show in its connection summary.
    let info = match engine.list_workspaces().await {
        Ok(list) => list.into_iter().find(|w| w.name == name),
        Err(_) => None,
    };
    let (entity_count, claim_count, source_count) = info
        .map(|w| (w.entity_count, w.claim_count, w.source_count))
        .unwrap_or((0, 0, 0));

    drop(engine);

    // Emit RARP-aware invalidation so any pre-existing engrams pinned
    // to a same-named workspace are dropped — defends against the
    // "remount under the same name returns stale claim ids" case.
    state.engram_manager.invalidate_workspace(&name).await;

    let rest_url = format!("/api/v1/ws/{name}/");
    let mcp_url = "/mcp/sse".to_string();
    ok_response(MountWorkspaceResponse {
        name,
        root_path: root_path.display().to_string(),
        entity_count,
        claim_count,
        source_count,
        rest_url,
        mcp_url,
    })
    .into_response()
}

async fn unmount_workspace_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let mut engine = state.engine.write().await;
    if let Err(e) = engine.unmount(&name) {
        return match_engine_error(e);
    }
    drop(engine);

    state.engram_manager.invalidate_workspace(&name).await;

    ok_response(serde_json::json!({ "unmounted": name })).into_response()
}

// ─── RARP / Active Engram Protocol REST endpoints ───────────────
//
// These mirror the 4 MCP tools (`materialize_engram`, `probe_engram`,
// `list_engrams`, `expire_engram`) so HTTP-only consumers can reach
// the AEP read path. Session id is mandatory and travels in the
// `X-TR-Session-Id` header — same lifetime contract as the SSE-MCP
// session: idle TTL eviction, cache-dirty invalidation, max engrams
// per session enforced by `EngramManager`.

const SESSION_HEADER: &str = "X-TR-Session-Id";

fn require_session_id(headers: &HeaderMap) -> Result<String, Response> {
    match headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(err_response(
            StatusCode::BAD_REQUEST,
            "MISSING_SESSION",
            &format!("{SESSION_HEADER} header is required"),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct MaterializeEngramRequest {
    topic: String,
    /// Optional explicit seed entity ids; falls back to a vector
    /// search against the workspace if absent. Mirrors the MCP
    /// behaviour at mcp/tools.rs::handle_materialize_engram.
    #[serde(default)]
    seed_entity_ids: Option<Vec<String>>,
    /// Optional engram scope (depth_hops, event_window_days,
    /// clearance, seed_claim_ids, score_with_hybrid).
    #[serde(default)]
    scope: Option<serde_json::Value>,
}

async fn materialize_engram_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MaterializeEngramRequest>,
) -> Response {
    let session_id = match require_session_id(&headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let topic = body.topic.trim().to_string();
    if topic.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "BAD_REQUEST", "topic is required");
    }

    let engine = state.engine.read().await;
    let seed_entity_ids = match body.seed_entity_ids {
        Some(ids) => ids,
        None => match engine.search(&ws, &topic, 10).await {
            Ok(result) => result.entities.into_iter().map(|e| e.id).collect(),
            Err(e) => return match_engine_error(e),
        },
    };
    if seed_entity_ids.is_empty() {
        return err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "NO_ANCHORS",
            &format!("no semantic anchors for topic '{topic}'"),
        );
    }

    let scope = crate::mcp::tools::parse_scope(body.scope.as_ref());

    let graph = match engine.graph_store(&ws).await {
        Some(g) => g,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "WORKSPACE_NOT_MOUNTED",
                &format!("workspace '{ws}' not mounted"),
            );
        }
    };

    match state
        .engram_manager
        .materialize_engram(&session_id, &ws, &topic, &graph, seed_entity_ids, scope, None)
        .await
    {
        Ok((pointer, summary)) => ok_response(serde_json::json!({
            "pointer": pointer,
            "summary": &*summary,
        }))
        .into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Debug, Deserialize)]
struct ProbeEngramRequest {
    question: String,
    #[serde(default)]
    clearance: Option<Vec<String>>,
    #[serde(default)]
    probe_kind: Option<String>,
    /// AEP × Hybrid composition flag. When `true`, the probe answer's
    /// rows are reordered by `hybrid_retrieve` before being returned.
    /// Spec: docs/2026-05-02-hybrid-retrieval-spec.md §11.
    #[serde(default)]
    score_with_hybrid: bool,
}

async fn probe_engram_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, ptr)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ProbeEngramRequest>,
) -> Response {
    let session_id = match require_session_id(&headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if body.question.trim().is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "BAD_REQUEST", "question is required");
    }

    let engine = state.engine.read().await;
    let graph = match engine.graph_store(&ws).await {
        Some(g) => g,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "WORKSPACE_NOT_MOUNTED",
                &format!("workspace '{ws}' not mounted"),
            );
        }
    };
    let byte_store = match engine.byte_store(&ws) {
        Some(b) => b,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "WORKSPACE_NO_BYTE_STORE",
                &format!("workspace '{ws}' has no byte store"),
            );
        }
    };

    let clearance: Option<Vec<thinkingroot_core::types::Sensitivity>> = body
        .clearance
        .as_ref()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| crate::mcp::tools::parse_sensitivity_str(s))
                .collect()
        });
    let probe_kind = body
        .probe_kind
        .as_deref()
        .and_then(crate::mcp::tools::parse_probe_kind_str);

    let probe_clearance = clearance.clone();
    let mut answer = match state
        .engram_manager
        .probe_engram(
            &session_id,
            &ptr,
            &body.question,
            clearance,
            &graph,
            byte_store.as_ref(),
            probe_kind,
        )
        .await
    {
        Ok(a) => a,
        Err(e) => return match_engine_error(e),
    };

    if body.score_with_hybrid && !answer.claim_ids.is_empty() {
        let req = crate::engine::RetrievalRequest {
            query_text: body.question.clone(),
            typed_predicates: vec![],
            session_id: session_id.clone(),
            clearance: probe_clearance
                .unwrap_or_else(|| vec![thinkingroot_core::types::Sensitivity::Public]),
            top_k: answer.claim_ids.len(),
            time_window: None,
            scoring_profile: crate::engine::ScoringProfile::default(),
            require_certificate: false,
            include_test_origin: true,
            include_quarantined: false,
            require_provenance_verified: false,
            now: None,
            scoped_claim_ids: Some(answer.claim_ids.clone()),
        };
        match engine.hybrid_retrieve(&ws, req, None).await {
            Ok(resp) => {
                let new_order: Vec<String> =
                    resp.hits.iter().map(|h| h.claim_id.clone()).collect();
                crate::mcp::tools::reorder_probe_answer_in_place(&mut answer, &new_order);
            }
            Err(e) => {
                // Fall back to Datalog order rather than failing the
                // probe — matches the MCP path's tolerant behaviour.
                tracing::warn!("hybrid composition fallback: {e}");
            }
        }
    }

    ok_response(answer).into_response()
}

async fn list_engrams_handler(
    State(state): State<Arc<AppState>>,
    Path(_ws): Path<String>,
    headers: HeaderMap,
) -> Response {
    let session_id = match require_session_id(&headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let refs = state.engram_manager.list_engrams(&session_id).await;
    ok_response(refs).into_response()
}

async fn expire_engram_handler(
    State(state): State<Arc<AppState>>,
    Path((_ws, ptr)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let session_id = match require_session_id(&headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let expired = state.engram_manager.expire_engram(&session_id, &ptr).await;
    ok_response(serde_json::json!({ "expired": expired, "pointer": ptr })).into_response()
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

/// `POST /api/v1/ws/{ws}/search/hybrid` — Hybrid Retrieval (vector × Datalog
/// × BLAKE3 × 11-component score fusion). Single-shot JSON response, not
/// SSE; the <25ms p95 budget makes streaming overhead net-negative. Cancel
/// on client disconnect via the same `CancellationToken + DropGuard`
/// pattern as the SSE compile route.
async fn hybrid_search_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(req): Json<crate::engine::RetrievalRequest>,
) -> Response {
    let cancel = tokio_util::sync::CancellationToken::new();
    let _drop_guard = cancel.clone().drop_guard();
    let engine = state.engine.read().await;
    match engine.hybrid_retrieve(&ws, req, Some(cancel)).await {
        Ok(resp) => ok_response(resp).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// T2.4 — `GET /api/v1/ws/{ws}/claims/as-of?as_of=2026-04-15T00:00:00Z[&branch=feat/x]`.
/// Returns claims whose `created_at` ≤ the supplied timestamp.
#[derive(Deserialize)]
struct AsOfQuery {
    as_of: String,
    #[serde(default)]
    branch: Option<String>,
}

async fn claims_as_of_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(query): Query<AsOfQuery>,
) -> Response {
    let parsed: chrono::DateTime<chrono::Utc> = match query.as_of.parse() {
        Ok(ts) => ts,
        Err(e) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "INVALID_AS_OF",
                &format!(
                    "as_of parameter must be ISO-8601 (e.g. 2026-04-15T00:00:00Z): {e}"
                ),
            );
        }
    };
    let engine = state.engine.read().await;
    match engine
        .list_claims_as_of_branched(&ws, query.branch.as_deref(), parsed)
        .await
    {
        Ok(claims) => ok_response(serde_json::json!({
            "workspace": ws,
            "branch": query.branch.unwrap_or_else(|| "main".to_string()),
            "as_of": query.as_of,
            "claim_count": claims.len(),
            "claims": claims,
        }))
        .into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// T3.2 — `POST /api/v1/ws/{ws}/reflect/across-branches`.  Body:
/// `{ "branches": ["main", "feature/foo", ...] }`.  Runs reflect
/// against each named branch and returns the union of per-branch
/// results plus divergent patterns.
#[derive(Deserialize)]
struct ReflectAcrossBranchesRequest {
    branches: Vec<String>,
}

async fn reflect_across_branches_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(req): Json<ReflectAcrossBranchesRequest>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.reflect_across_branches(&ws, &req.branches).await {
        Ok(result) => ok_response(result).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn compile(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    // The audit flagged that this read guard is held for the entire
    // compile (multi-minute).  Concurrent *readers* (search,
    // brain_load, etc.) are unaffected — `RwLock::read` is shared.
    // The only callers blocked are *writers* of the engine itself,
    // which are `mount`/`unmount` on the workspace map.  Those run
    // exactly once per workspace add/remove and are never on a UI hot
    // path, so the practical contention surface is empty.
    //
    // Releasing the guard mid-compile would require changing
    // `QueryEngine::compile`'s signature to `compile(Arc<Self>)`
    // because the returned Future captures `&self` from the guard;
    // that's a public API break we're not pulling forward without
    // observed contention.
    let engine = state.engine.read().await;
    match engine.compile(&ws).await {
        Ok(result) => {
            // Plan §3.10: when compile dirties the cache, drop every Engram
            // pointing at this workspace. Without this hook a probe after a
            // writing compile can return rows whose claim ids were just GC'd.
            // Mirrors the existing `branch_engines.invalidate_workspace`
            // call inside `QueryEngine::compile` (engine.rs:2987).
            if result.cache_dirty {
                state.engram_manager.invalidate_workspace(&ws).await;
            }
            ok_response(result).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

// ─── Streaming compile (P4 / H5) ─────────────────────────────
//
// `POST /api/v1/ws/{ws}/compile/stream` runs the v3 pipeline in this
// process and streams every `ProgressEvent` to the client as an SSE
// frame, plus a single `done`/`failed`/`cancelled` terminator.  Used
// by the desktop to route compile through the managed sidecar so the
// desktop process is never the writer of `graph.db` — pre-fix the
// in-process compile froze the desktop's Brain view because both the
// pipeline (writer) and `MountedMemory` (reader) shared the desktop's
// CozoDB instance.
//
// The handler doesn't go through `QueryEngine::compile` — that path
// requires the workspace to be mounted in this server's engine and
// does its own cache reload.  The desktop's sidecar is launched
// without `--path` args (workspaces are managed via the registry,
// not CLI flags), so we run `run_pipeline_with_options` directly
// against the explicit `root_path` from the request body.  This
// keeps the contract simple: the client tells the server what to
// compile; the server doesn't need its own mount table.
//
// Cancellation is wired via a `CancellationToken` whose
// `DropGuard` lives inside the SSE stream.  When the client
// disconnects, axum drops the response body future, the guard
// drops, the token trips, and the running pipeline exits at the
// next phase boundary with `Error::Cancelled` (which we surface
// as a `cancelled` SSE event for callers that race the disconnect).
#[derive(Debug, Deserialize)]
struct CompileStreamRequest {
    /// Absolute path to the workspace root.  Required when this
    /// server was started without `--path`; defaults to
    /// `state.workspace_root` otherwise.
    root_path: Option<String>,
    /// Optional branch — `None` resolves to the active head.
    branch: Option<String>,
    /// Skip Phase 6.5 (Rooting admission gate).  Mirrors
    /// `PipelineOptions::no_rooting` and the CLI's `--no-rooting`
    /// flag.
    #[serde(default)]
    no_rooting: bool,
}

async fn compile_stream(
    State(state): State<Arc<AppState>>,
    Path(_ws): Path<String>,
    Json(body): Json<CompileStreamRequest>,
) -> Response {
    use crate::pipeline::{PipelineOptions, ProgressEvent, run_pipeline_with_options};
    use tokio_util::sync::CancellationToken;

    let root_path = match (body.root_path.as_deref(), state.workspace_root.as_ref()) {
        (Some(p), _) => PathBuf::from(p),
        (None, Some(r)) => r.clone(),
        (None, None) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "MISSING_ROOT_PATH",
                "request body must include root_path when the server has no --path arg",
            );
        }
    };

    if !root_path.is_dir() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "ROOT_PATH_NOT_DIR",
            &format!("root_path is not a directory: {}", root_path.display()),
        );
    }

    let branch = body.branch.clone();
    let no_rooting = body.no_rooting;

    // The DropGuard fires the cancel token when the SSE stream is
    // dropped (client disconnect, axum body cancellation, etc.).
    // The pipeline task receives the same token and bails at the
    // next phase boundary.
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let drop_guard = cancel.drop_guard();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let root_for_task = root_path.clone();

    let pipeline_handle = tokio::spawn(async move {
        run_pipeline_with_options(
            &root_for_task,
            branch.as_deref(),
            Some(tx),
            PipelineOptions {
                cancel: cancel_for_task,
                no_rooting,
                skip_byte_audit: false,
                no_incremental: false,
            },
        )
        .await
    });

    let stream = async_stream::stream! {
        // Keep the drop guard alive for the lifetime of the stream.
        // Dropping it (client disconnect / response cancel) trips
        // the pipeline's CancellationToken — that's the cleanup
        // path that turns "user closed the modal" into "stop the
        // pipeline cleanly" without requiring an explicit
        // cancel-by-id route.
        let _guard = drop_guard;
        let mut handle = Some(pipeline_handle);

        while let Some(event) = rx.recv().await {
            let payload = match serde_json::to_string(&event) {
                Ok(s) => s,
                Err(e) => {
                    // Should not happen — every ProgressEvent variant
                    // is composed of primitives. If it does, surface
                    // the error rather than silently swallowing the
                    // event so the desktop can show a real failure
                    // instead of an incomplete progress stream.
                    let payload = serde_json::json!({
                        "error": format!("progress event encode failed: {e}"),
                    })
                    .to_string();
                    yield Ok::<Event, std::convert::Infallible>(
                        Event::default().event("failed").data(payload),
                    );
                    return;
                }
            };
            yield Ok(Event::default().event("progress").data(payload));
        }

        // Channel closed → the pipeline task has finished. Await
        // its outcome and emit a single terminator event.
        if let Some(h) = handle.take() {
            match h.await {
                Ok(Ok(result)) => {
                    let payload = serde_json::to_string(&result)
                        .unwrap_or_else(|_| "{}".to_string());
                    yield Ok(Event::default().event("done").data(payload));
                }
                Ok(Err(e)) if e.is_cancelled() => {
                    yield Ok(Event::default().event("cancelled").data("{}"));
                }
                Ok(Err(e)) => {
                    let payload =
                        serde_json::json!({ "error": e.to_string() }).to_string();
                    yield Ok(Event::default().event("failed").data(payload));
                }
                Err(e) => {
                    let payload = serde_json::json!({
                        "error": format!("pipeline task panicked: {e}"),
                    })
                    .to_string();
                    yield Ok(Event::default().event("failed").data(payload));
                }
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
    /// T0.6 — optional explicit BranchKind. Defaults to Feature.
    #[serde(default)]
    kind: Option<thinkingroot_core::BranchKind>,
    /// T0.6 — optional explicit MergePolicy. Defaults to Manual.
    #[serde(default)]
    merge_policy: Option<thinkingroot_core::MergePolicy>,
    /// T2.6 — optional redaction policy. Defaults to no redaction.
    #[serde(default)]
    redaction: Option<thinkingroot_core::RedactionPolicy>,
    /// T3.7 — apply the named template's defaults to any field on
    /// this request that the caller did not explicitly set.  Explicit
    /// fields always win — the template never overrides a value the
    /// caller asked for.
    #[serde(default)]
    template: Option<String>,
}

async fn create_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
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

    // T3.7 — apply template defaults to any field the caller left
    // unset.  An invalid template name returns 400 rather than
    // silently materialising the branch with engine defaults — the
    // caller asked for a template, give them a clear error.
    let mut kind = body.kind;
    let mut merge_policy = body.merge_policy;
    let mut redaction = body.redaction;
    let mut permissions: Option<thinkingroot_core::BranchPermissions> = None;
    let mut max_age_secs: Option<u64> = None;
    if let Some(template_name) = body.template.as_deref() {
        use thinkingroot_branch::templates::TemplateRegistry;
        let refs_dir = root.join(".thinkingroot-refs");
        let registry = match TemplateRegistry::load_or_seed(&refs_dir) {
            Ok(r) => r,
            Err(e) => {
                return err_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "TEMPLATE_LOAD_FAILED",
                    &e.to_string(),
                );
            }
        };
        let Some(template) = registry.get(template_name) else {
            return err_response(
                StatusCode::NOT_FOUND,
                "TEMPLATE_NOT_FOUND",
                &format!("no branch template named '{template_name}'"),
            );
        };
        kind.get_or_insert_with(|| template.kind.clone());
        if merge_policy.is_none() {
            merge_policy = Some(template.merge_policy.clone());
        }
        if redaction.is_none() {
            redaction = template.redaction.clone();
        }
        permissions = template.permissions.clone();
        max_age_secs = template.max_age_secs;
    }

    match thinkingroot_branch::create_branch_full(
        &root,
        &body.name,
        parent,
        body.description,
        request_user(&headers),
        permissions.unwrap_or_default(),
        kind.unwrap_or_default(),
        merge_policy.unwrap_or_default(),
        redaction,
    )
    .await
    {
        Ok(branch) => {
            // T3.7 — when the template specified a TTL, apply it now
            // via the post-create setter.  The TTL is the only
            // template field that can't be passed to
            // `create_branch_full` directly because the registry path
            // for it predates templates.
            if let Some(ttl) = max_age_secs {
                if let Err(e) = thinkingroot_branch::set_branch_max_age_secs(
                    &root,
                    &branch.name,
                    Some(ttl),
                ) {
                    return err_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "TEMPLATE_TTL_APPLY_FAILED",
                        &e.to_string(),
                    );
                }
            }
            // T1.6 — publish the `Created` event the registry just
            // appended on the new broadcast channel so any client
            // already subscribed to `/branches/{name}/events/stream`
            // picks it up live.
            publish_latest_branch_event(&state, &branch.name).await;
            ok_response(serde_json::json!({ "branch": branch })).into_response()
        }
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

// ─── T0.7: contribute-bulk ────────────────────────────────────────────

#[derive(Deserialize)]
struct ContributeBulkRequest {
    /// Workspace name (matches the mounted workspace identifier).
    workspace: String,
    /// Optional session id for turn-calendar attribution. When absent,
    /// the synthetic session id derived from the connector identity is
    /// used.
    #[serde(default)]
    session_id: Option<String>,
    /// Connector identifier (e.g. `"github"`, `"slack"`, `"notion"`).
    connector_id: String,
    /// Per-install identifier (`"alice-acme-prod"`).
    install_id: String,
    /// Idempotency key (the connector picks this; typically the
    /// webhook delivery id or the upstream event id).
    idempotency_key: String,
    /// When `true`, skip per-claim rooting (deferred to end of batch).
    #[serde(default)]
    backfill: bool,
    /// The batch of claims being contributed.
    claims: Vec<crate::engine::AgentClaim>,
}

async fn contribute_bulk_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
    Json(body): Json<ContributeBulkRequest>,
) -> impl IntoResponse {
    let principal = crate::engine::Principal::Connector {
        connector_id: body.connector_id.clone(),
        install_id: body.install_id.clone(),
    };
    let session_id = body.session_id.unwrap_or_else(|| {
        format!(
            "connector:{}:{}:{}",
            body.connector_id, body.install_id, body.idempotency_key
        )
    });
    let branch_arg = if branch == "main" {
        None
    } else {
        Some(branch.as_str())
    };

    let engine = state.engine.read().await;
    match engine
        .contribute_bulk(
            &body.workspace,
            &session_id,
            branch_arg,
            body.claims,
            &state.sessions,
            principal,
            &body.idempotency_key,
            body.backfill,
        )
        .await
    {
        Ok(result) => {
            // T1.6 — `contribute_bulk` appends a `ContributeBulk`
            // BranchEvent on success.  Broadcast it for live
            // subscribers (only meaningful when contributing to a
            // named branch — `main` has no per-branch broadcast key,
            // but we publish anyway for symmetry).
            drop(engine);
            publish_latest_branch_event(&state, &branch).await;
            ok_response(serde_json::json!(result)).into_response()
        }
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "CONTRIBUTE_BULK_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── T2.6: per-branch redaction policy ────────────────────────────────

#[derive(Deserialize)]
struct SetRedactionRequest {
    /// `null` clears the policy; an object sets it.
    policy: Option<thinkingroot_core::RedactionPolicy>,
}

async fn set_branch_redaction_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
    Json(body): Json<SetRedactionRequest>,
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
    match thinkingroot_branch::set_branch_redaction(&root, &branch, body.policy) {
        Ok(updated) => {
            // T1.6 — `set_branch_redaction` appends `RedactionUpdated`.
            publish_latest_branch_event(&state, &updated.name).await;
            ok_response(serde_json::json!({ "branch": updated })).into_response()
        }
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "REDACTION_UPDATE_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── T1.3: Branch audit log ──────────────────────────────────────────
//
// Returns the append-only `events` vec on a `BranchRef`, oldest-first.
// Useful for "who changed this branch when?" UX and as the source of
// truth for the lineage DAG (T1.7) and SSE stream (T1.6).

async fn list_branch_events_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> Response {
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
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::branch::BranchRegistry;
    let registry = match BranchRegistry::load_or_create(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BRANCH_ERROR",
                &e.to_string(),
            );
        }
    };
    // Look across both active + abandoned + merged so a merged branch
    // still answers its history (lineage UI walks merged branches).
    let events = registry
        .all()
        .into_iter()
        .find(|b| b.name == branch)
        .map(|b| b.events.clone())
        .unwrap_or_default();
    ok_response(serde_json::json!({
        "branch": branch,
        "events": events,
    }))
    .into_response()
}

// ─── T1.6: Live SSE branch events ────────────────────────────────────
//
// Subscribers connect to `GET /branches/{branch}/events/stream`,
// receive the per-branch broadcast channel, and forward every
// `BranchEvent` published after a successful mutation as one SSE
// `branch_event` frame.  No backfill — the polling endpoint
// `/branches/{branch}/events` is the source of historical events.
// Channel-lag (a slow consumer that fell more than 64 events behind)
// is surfaced as a `lagged` event so the client can refetch via the
// polling endpoint and resume.

async fn stream_branch_events_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> Response {
    use tokio_stream::StreamExt as _;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

    let tx = state.branch_event_sender(&branch).await;
    let rx = tx.subscribe();
    let stream = BroadcastStream::new(rx).map(move |res| match res {
        Ok(event) => {
            let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            Ok::<Event, std::convert::Infallible>(
                Event::default().event("branch_event").data(payload),
            )
        }
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            let payload = serde_json::json!({ "missed": n }).to_string();
            Ok(Event::default().event("lagged").data(payload))
        }
    });

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ─── T1.2: Branch stats ──────────────────────────────────────────────
//
// Cheap per-branch probe — claim / entity / source counts — without
// running a full `compute_diff`.  Reads the branch's own GraphStore;
// avoids the AEP path so the substrate cost is bounded by table
// scans, not Datalog.

#[derive(Serialize)]
struct BranchStatsResponse {
    branch: String,
    /// Number of claims in the branch's graph.db (post any merges).
    claim_count: usize,
    /// Number of entities.
    entity_count: usize,
    /// Number of source rows.
    source_count: usize,
    /// Number of audit-log entries currently retained on this branch
    /// (capped by `MAX_EVENTS`).
    event_count: usize,
    /// Lifecycle state (active / merged / abandoned) for the row.
    status: String,
}

async fn branch_stats_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> Response {
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
    use thinkingroot_branch::branch::BranchRegistry;
    use thinkingroot_branch::snapshot::resolve_data_dir;
    use thinkingroot_graph::graph::GraphStore;

    let refs_dir = root.join(".thinkingroot-refs");
    let registry = match BranchRegistry::load_or_create(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BRANCH_ERROR",
                &e.to_string(),
            );
        }
    };
    let branch_ref = match registry.all().into_iter().find(|b| b.name == branch) {
        Some(b) => b.clone(),
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "BRANCH_NOT_FOUND",
                &format!("branch '{branch}' not found"),
            );
        }
    };

    let branch_arg = if branch == "main" { None } else { Some(branch.as_str()) };
    let data_dir = resolve_data_dir(&root, branch_arg);
    if !data_dir.exists() {
        // Branch entry exists but data dir is gone (abandoned branches
        // have their dir removed by gc).  Report what we know about
        // the audit-log without lying about substrate counts.
        return ok_response(BranchStatsResponse {
            branch: branch.clone(),
            claim_count: 0,
            entity_count: 0,
            source_count: 0,
            event_count: branch_ref.events.len(),
            status: branch_status_label(&branch_ref.status),
        })
        .into_response();
    }

    let graph = match GraphStore::init(&data_dir.join("graph")) {
        Ok(g) => g,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &e.to_string(),
            );
        }
    };
    let claims = graph.get_all_claims_with_sources().unwrap_or_default();
    let entities = graph.get_all_entities().unwrap_or_default();
    let sources = graph.get_all_sources().unwrap_or_default();

    ok_response(BranchStatsResponse {
        branch: branch.clone(),
        claim_count: claims.len(),
        entity_count: entities.len(),
        source_count: sources.len(),
        event_count: branch_ref.events.len(),
        status: branch_status_label(&branch_ref.status),
    })
    .into_response()
}

fn branch_status_label(status: &thinkingroot_core::BranchStatus) -> String {
    use thinkingroot_core::BranchStatus;
    match status {
        BranchStatus::Active => "active".into(),
        BranchStatus::Merged { .. } => "merged".into(),
        BranchStatus::Abandoned { .. } => "abandoned".into(),
    }
}

// ─── T2.5: Tag create / list / get ───────────────────────────────────
//
// `POST /api/v1/tags` registers an immutable [`BranchKind::Tag`].
// `GET /api/v1/tags` lists every active tag.
// `GET /api/v1/tags/{name}` returns one.

#[derive(Deserialize)]
struct CreateTagRequest {
    /// User-visible tag name (e.g. `"v1.0.0"`, `"q1-snapshot"`).
    name: String,
    /// Internal ref pointer (e.g. `"refs/tags/v1.0.0"`); free-form.
    ref_name: String,
    /// Pinned target — typically a BLAKE3 commit hash matching
    /// `BranchRef::parent_commit_hash`.
    target: String,
    #[serde(default)]
    description: Option<String>,
}

async fn create_tag_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateTagRequest>,
) -> Response {
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
    match thinkingroot_branch::create_tag(
        &root,
        &body.name,
        &body.ref_name,
        &body.target,
        request_user(&headers),
        body.description,
    ) {
        Ok(tag) => ok_response(serde_json::json!({ "tag": tag })).into_response(),
        Err(thinkingroot_core::Error::BranchAlreadyExists(_)) => err_response(
            StatusCode::CONFLICT,
            "TAG_ALREADY_EXISTS",
            &format!("tag '{}' already exists", body.name),
        ),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "TAG_CREATE_FAILED",
            &e.to_string(),
        ),
    }
}

async fn list_tags_handler(State(state): State<Arc<AppState>>) -> Response {
    let root = match &state.workspace_root {
        Some(r) => r.clone(),
        None => {
            // Empty list rather than error — matches list_branches
            // behaviour and lets unconfigured daemons stay
            // 200-OK-with-empty for monitoring scrapers.
            return ok_response(serde_json::json!({ "tags": Vec::<()>::new() }))
                .into_response();
        }
    };
    match thinkingroot_branch::list_tags(&root) {
        Ok(tags) => ok_response(serde_json::json!({ "tags": tags })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TAG_LIST_FAILED",
            &e.to_string(),
        ),
    }
}

async fn get_tag_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
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
    match thinkingroot_branch::list_tags(&root) {
        Ok(tags) => match tags.into_iter().find(|t| t.name == name) {
            Some(t) => ok_response(serde_json::json!({ "tag": t })).into_response(),
            None => err_response(
                StatusCode::NOT_FOUND,
                "TAG_NOT_FOUND",
                &format!("tag '{name}' not found"),
            ),
        },
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TAG_LOOKUP_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── T3.7: Branch templates ──────────────────────────────────────────
//
// Read/write surface for `branch_templates.toml`.  POST creates or
// overwrites a template by name; GET (collection) lists; GET (item)
// fetches one; DELETE removes one.  Consumers wire `template: "..."`
// on `POST /branches` to materialise the bundled defaults.

async fn list_branch_templates_handler(State(state): State<Arc<AppState>>) -> Response {
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
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::templates::TemplateRegistry;
    match TemplateRegistry::load_or_seed(&refs_dir) {
        Ok(r) => ok_response(serde_json::json!({ "templates": r.list() })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TEMPLATE_LOAD_FAILED",
            &e.to_string(),
        ),
    }
}

async fn upsert_branch_template_handler(
    State(state): State<Arc<AppState>>,
    Json(template): Json<thinkingroot_branch::templates::BranchTemplate>,
) -> Response {
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
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::templates::TemplateRegistry;
    let mut registry = match TemplateRegistry::load_or_seed(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "TEMPLATE_LOAD_FAILED",
                &e.to_string(),
            );
        }
    };
    let name = template.name.clone();
    match registry.upsert(template) {
        Ok(existed) => {
            let status_code = if existed {
                "updated"
            } else {
                "created"
            };
            ok_response(serde_json::json!({
                "name": name,
                "status": status_code,
            }))
            .into_response()
        }
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "TEMPLATE_INVALID",
            &e.to_string(),
        ),
    }
}

async fn get_branch_template_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
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
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::templates::TemplateRegistry;
    match TemplateRegistry::load_or_seed(&refs_dir) {
        Ok(r) => match r.get(&name) {
            Some(t) => ok_response(serde_json::json!({ "template": t })).into_response(),
            None => err_response(
                StatusCode::NOT_FOUND,
                "TEMPLATE_NOT_FOUND",
                &format!("no branch template named '{name}'"),
            ),
        },
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TEMPLATE_LOAD_FAILED",
            &e.to_string(),
        ),
    }
}

async fn delete_branch_template_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
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
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::templates::TemplateRegistry;
    let mut registry = match TemplateRegistry::load_or_seed(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "TEMPLATE_LOAD_FAILED",
                &e.to_string(),
            );
        }
    };
    match registry.remove(&name) {
        Ok(true) => ok_response(serde_json::json!({ "deleted": name })).into_response(),
        Ok(false) => err_response(
            StatusCode::NOT_FOUND,
            "TEMPLATE_NOT_FOUND",
            &format!("no branch template named '{name}'"),
        ),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TEMPLATE_DELETE_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── T1.7: Branch lineage DAG ────────────────────────────────────────
//
// Aggregates `(parent, child)` fork edges + `(child, into)` merge
// edges across every branch in the registry.  Consumers (Brain
// surface, dashboards) render this as a DAG; the layout is theirs to
// pick — we just hand back the edge list with timestamps so they can
// time-order siblings.

#[derive(Serialize)]
struct LineageEdge {
    /// `"fork"` or `"merge"`.
    kind: &'static str,
    from: String,
    to: String,
    at: chrono::DateTime<chrono::Utc>,
    /// For merge edges: the proposal id (when the merge was gated).
    #[serde(skip_serializing_if = "Option::is_none")]
    authorising_proposal_id: Option<String>,
}

#[derive(Serialize)]
struct LineageNode {
    name: String,
    status: String,
    kind: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize)]
struct LineageResponse {
    nodes: Vec<LineageNode>,
    edges: Vec<LineageEdge>,
}

async fn branch_lineage_handler(State(state): State<Arc<AppState>>) -> Response {
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
    use thinkingroot_branch::branch::BranchRegistry;
    use thinkingroot_core::BranchEvent;

    let refs_dir = root.join(".thinkingroot-refs");
    let registry = match BranchRegistry::load_or_create(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BRANCH_ERROR",
                &e.to_string(),
            );
        }
    };

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for branch in registry.all() {
        nodes.push(LineageNode {
            name: branch.name.clone(),
            status: branch_status_label(&branch.status),
            kind: kind_label(&branch.kind),
            created_at: branch.created_at,
        });
        // Fork edge: parent → branch, timestamped at the Created event
        // when present (T1.3 wires this on every new branch); falls
        // back to `created_at` when reading a pre-T1.3 registry.
        let forked_at = branch
            .events
            .iter()
            .find_map(|e| match e {
                BranchEvent::Created { at, .. } => Some(*at),
                _ => None,
            })
            .unwrap_or(branch.created_at);
        edges.push(LineageEdge {
            kind: "fork",
            from: branch.parent.clone(),
            to: branch.name.clone(),
            at: forked_at,
            authorising_proposal_id: None,
        });
        // Merge edges: one per Merged event (typically only one, but
        // a branch could in theory be merged-then-reopened in a future
        // workflow; loop covers the general case).
        for ev in &branch.events {
            if let BranchEvent::Merged {
                at,
                into,
                authorising_proposal_id,
                ..
            } = ev
            {
                edges.push(LineageEdge {
                    kind: "merge",
                    from: branch.name.clone(),
                    to: into.clone(),
                    at: *at,
                    authorising_proposal_id: authorising_proposal_id.clone(),
                });
            }
        }
    }

    ok_response(LineageResponse { nodes, edges }).into_response()
}

fn kind_label(kind: &thinkingroot_core::BranchKind) -> String {
    use thinkingroot_core::BranchKind;
    match kind {
        BranchKind::Main => "main".into(),
        BranchKind::Feature => "feature".into(),
        BranchKind::Stream { .. } => "stream".into(),
        BranchKind::Sandbox { .. } => "sandbox".into(),
        BranchKind::Tag { .. } => "tag".into(),
    }
}

// ─── T0.4: Knowledge Proposal handlers ────────────────────────────────
//
// These five routes wire the `thinkingroot-pr` crate (the proposal
// lifecycle layer) into HTTP. A workspace's proposals all live under
// `<workspace>/.thinkingroot-refs/proposals/`; routes that need a
// `refs_dir` derive it from `state.workspace_root` and bail early if
// the daemon was started without `--path`.

#[derive(Deserialize)]
struct OpenProposalRequest {
    /// Optional explicit target branch; `None` (or omitted) means main.
    #[serde(default)]
    target_branch: Option<String>,
    /// Free-form description supplied by the proposing principal.
    #[serde(default)]
    description: Option<String>,
    /// Distinct approving reviewers required. Reads from the source
    /// branch's `MergePolicy::RequiresProposal { min_reviewers }` when
    /// omitted (`None`); falls back to `1` if no policy is set so this
    /// route stays usable for branches that haven't opted into proposal
    /// gating yet.
    #[serde(default)]
    min_reviewers: Option<u8>,
    /// Required-checks list to freeze on the proposal at open time.
    /// When omitted, copied from the source branch's policy if set,
    /// otherwise empty.
    #[serde(default)]
    required_checks: Option<Vec<String>>,
}

fn refs_dir_from_state(state: &AppState) -> std::result::Result<PathBuf, Response> {
    let root = state.workspace_root.as_ref().ok_or_else(|| {
        err_response(
            StatusCode::BAD_REQUEST,
            "NOT_CONFIGURED",
            "workspace_root not set",
        )
    })?;
    Ok(root.join(".thinkingroot-refs"))
}

/// Look up the source branch's `RequiresProposal` policy values so
/// callers don't have to mirror them on every open request. Returns
/// `(min_reviewers, required_checks)`. Defaults to `(1, vec![])` when
/// the branch has any other policy or when it can't be loaded — the
/// proposal still gets created, the merge gate just won't honour it
/// unless the policy is also `RequiresProposal`.
fn proposal_policy_defaults(
    refs_dir: &std::path::Path,
    source_branch: &str,
) -> (u8, Vec<String>) {
    use thinkingroot_branch::branch::BranchRegistry;
    use thinkingroot_core::MergePolicy;
    if let Ok(registry) = BranchRegistry::load_or_create(refs_dir)
        && let Some(branch) = registry.get(source_branch)
        && let MergePolicy::RequiresProposal {
            min_reviewers,
            required_checks,
        } = &branch.merge_policy
    {
        return (*min_reviewers, required_checks.clone());
    }
    (1, Vec::new())
}

async fn open_proposal_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(branch): Path<String>,
    Json(body): Json<OpenProposalRequest>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state) {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let author = match request_user(&headers) {
        Some(u) => u,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "MISSING_PRINCIPAL",
                "X-TR-User header is required to open a proposal",
            );
        }
    };

    let (default_min, default_checks) =
        proposal_policy_defaults(&refs_dir, &branch);
    let min_reviewers = body.min_reviewers.unwrap_or(default_min);
    let required_checks = body.required_checks.unwrap_or(default_checks);

    match thinkingroot_pr::open_proposal(
        &refs_dir,
        &branch,
        body.target_branch.as_deref(),
        &author,
        body.description,
        min_reviewers,
        required_checks,
    ) {
        Ok(p) => ok_response(serde_json::json!({ "proposal": p })).into_response(),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "PROPOSAL_OPEN_FAILED",
            &e.to_string(),
        ),
    }
}

async fn list_branch_proposals_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state) {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    match thinkingroot_pr::list_proposals(&refs_dir) {
        Ok(all) => {
            let filtered: Vec<_> = all
                .into_iter()
                .filter(|p| p.source_branch == branch)
                .collect();
            ok_response(serde_json::json!({ "proposals": filtered })).into_response()
        }
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "PROPOSAL_LIST_FAILED",
            &e.to_string(),
        ),
    }
}

async fn list_all_proposals_handler(State(state): State<Arc<AppState>>) -> Response {
    let refs_dir = match refs_dir_from_state(&state) {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    match thinkingroot_pr::list_proposals(&refs_dir) {
        Ok(all) => ok_response(serde_json::json!({ "proposals": all })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "PROPOSAL_LIST_FAILED",
            &e.to_string(),
        ),
    }
}

async fn get_proposal_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state) {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    match thinkingroot_pr::read_proposal(&refs_dir, &id) {
        Ok(Some(p)) => ok_response(serde_json::json!({ "proposal": p })).into_response(),
        Ok(None) => err_response(
            StatusCode::NOT_FOUND,
            "PROPOSAL_NOT_FOUND",
            &format!("proposal `{id}` not found"),
        ),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "PROPOSAL_READ_FAILED",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct ReviewProposalRequest {
    /// `"approve"`, `"request_changes"`, or `"comment"`.
    decision: String,
    #[serde(default)]
    comment: Option<String>,
}

async fn review_proposal_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ReviewProposalRequest>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state) {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let reviewer = match request_user(&headers) {
        Some(u) => u,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "MISSING_PRINCIPAL",
                "X-TR-User header is required to review a proposal",
            );
        }
    };
    let decision = match body.decision.to_ascii_lowercase().as_str() {
        "approve" => thinkingroot_pr::ReviewDecision::Approve,
        "request_changes" | "request-changes" | "changes_requested" => {
            thinkingroot_pr::ReviewDecision::RequestChanges
        }
        "comment" => thinkingroot_pr::ReviewDecision::Comment,
        other => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "BAD_DECISION",
                &format!(
                    "decision must be one of approve|request_changes|comment, got `{other}`"
                ),
            );
        }
    };
    match thinkingroot_pr::review_proposal(&refs_dir, &id, &reviewer, decision, body.comment) {
        Ok(p) => ok_response(serde_json::json!({ "proposal": p })).into_response(),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "PROPOSAL_REVIEW_FAILED",
            &e.to_string(),
        ),
    }
}

async fn close_proposal_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state) {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let closer = match request_user(&headers) {
        Some(u) => u,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "MISSING_PRINCIPAL",
                "X-TR-User header is required to close a proposal",
            );
        }
    };
    match thinkingroot_pr::close_proposal(&refs_dir, &id, &closer) {
        Ok(p) => ok_response(serde_json::json!({ "proposal": p })).into_response(),
        Err(e) => err_response(
            StatusCode::FORBIDDEN,
            "PROPOSAL_CLOSE_FAILED",
            &e.to_string(),
        ),
    }
}

async fn delete_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
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
    let actor = request_user(&headers)
        .map(crate::engine::BranchActor::User)
        .unwrap_or(crate::engine::BranchActor::System);
    match engine.delete_branch_as(&root, &branch, actor).await {
        Ok(_) => {
            // T1.6 — `delete_branch_as` calls `abandon_branch` which
            // appended an `Abandoned` event; broadcast it before
            // dropping the engine read-lock.
            drop(engine);
            publish_latest_branch_event(&state, &branch).await;
            ok_response(serde_json::json!({ "deleted": branch })).into_response()
        }
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

#[derive(Deserialize, Default)]
struct MergeQuery {
    /// T1.5 — when true, compute the diff that would land in target
    /// without mutating anything.  Returns the same shape as the
    /// committing path plus the diff body so callers can preview.
    #[serde(default)]
    dry_run: bool,
}

async fn merge_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(branch): Path<String>,
    Query(query): Query<MergeQuery>,
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

    // T1.5 — dry-run path.  `dry_run_merge_into` runs the same
    // diff-computation chain (two-way + vector pass, three-way when
    // an LCA is present) the committing merge would, but never
    // touches the target graph or the registry.  Default `target`
    // matches the committing path's `None → "main"`.
    if query.dry_run {
        match thinkingroot_branch::dry_run_merge_into(&root, &branch, "main", force).await {
            Ok(diff) => return ok_response(serde_json::json!({
                "dry_run": true,
                "diff": diff,
                "merge_allowed": diff.merge_allowed,
                "blocking_reasons": diff.blocking_reasons,
                "new_claims": diff.new_claims.len(),
                "new_entities": diff.new_entities.len(),
                "auto_resolved": diff.auto_resolved.len(),
                "needs_review": diff.needs_review.len(),
            }))
            .into_response(),
            Err(e) => {
                return err_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "DRY_RUN_FAILED",
                    &e.to_string(),
                );
            }
        }
    }

    // T1.5 — register a CancellationToken so `POST /merges/{id}/cancel`
    // can trip it.  The id is returned to the caller in the success
    // response and surfaced in error responses too so a hung merge can
    // be aborted by an out-of-band client.
    let merge_id = format!(
        "merge_{}",
        chrono::Utc::now().timestamp_micros().to_string()
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    state
        .active_merges
        .write()
        .await
        .insert(merge_id.clone(), cancel.clone());

    let engine = state.engine.read().await;
    let result = engine
        .merge_into_branch_cancellable(
            &root,
            &branch,
            None,
            force,
            propagate_deletions,
            MergedBy::Human {
                user: request_user(&headers).unwrap_or_else(|| "api".to_string()),
            },
            Some(cancel.clone()),
        )
        .await;
    drop(engine);

    // Always remove the token so the map doesn't grow unbounded — we
    // do this before deciding response shape so a slow publish below
    // can't leak the entry.
    state.active_merges.write().await.remove(&merge_id);

    match result {
        Ok(diff) => {
            publish_latest_branch_event(&state, &branch).await;
            ok_response(serde_json::json!({
                "merged": branch,
                "merge_id": merge_id,
                "new_claims": diff.new_claims.len(),
                "new_entities": diff.new_entities.len(),
                "auto_resolved": diff.auto_resolved.len(),
            }))
            .into_response()
        }
        Err(thinkingroot_core::Error::EntityNotFound(msg)) => {
            err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &msg)
        }
        Err(e) if e.is_cancelled() => err_response(
            StatusCode::GONE,
            "MERGE_CANCELLED",
            &format!("merge {merge_id} cancelled before completion"),
        ),
        Err(e) => err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "MERGE_BLOCKED",
            &e.to_string(),
        ),
    }
}

async fn merge_into_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((source, target)): Path<(String, String)>,
    Query(query): Query<MergeQuery>,
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

    // T1.5 — dry-run path mirrors `merge_branch_handler`.
    if query.dry_run {
        match thinkingroot_branch::dry_run_merge_into(&root, &source, &target, force).await {
            Ok(diff) => return ok_response(serde_json::json!({
                "dry_run": true,
                "source": source,
                "target": target,
                "diff": diff,
                "merge_allowed": diff.merge_allowed,
                "blocking_reasons": diff.blocking_reasons,
                "new_claims": diff.new_claims.len(),
                "new_entities": diff.new_entities.len(),
                "auto_resolved": diff.auto_resolved.len(),
                "needs_review": diff.needs_review.len(),
            }))
            .into_response(),
            Err(e) => {
                return err_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "DRY_RUN_FAILED",
                    &e.to_string(),
                );
            }
        }
    }

    let merge_id = format!(
        "merge_{}",
        chrono::Utc::now().timestamp_micros().to_string()
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    state
        .active_merges
        .write()
        .await
        .insert(merge_id.clone(), cancel.clone());

    let engine = state.engine.read().await;
    let result = engine
        .merge_into_branch_cancellable(
            &root,
            &source,
            Some(&target),
            force,
            propagate_deletions,
            MergedBy::Human {
                user: request_user(&headers).unwrap_or_else(|| "api".to_string()),
            },
            Some(cancel.clone()),
        )
        .await;
    drop(engine);
    state.active_merges.write().await.remove(&merge_id);

    match result {
        Ok(diff) => {
            publish_latest_branch_event(&state, &source).await;
            ok_response(serde_json::json!({
                "merged": source,
                "target": target,
                "merge_id": merge_id,
                "new_claims": diff.new_claims.len(),
                "new_entities": diff.new_entities.len(),
                "auto_resolved": diff.auto_resolved.len(),
            }))
            .into_response()
        }
        Err(thinkingroot_core::Error::EntityNotFound(msg)) => {
            err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &msg)
        }
        Err(e) if e.is_cancelled() => err_response(
            StatusCode::GONE,
            "MERGE_CANCELLED",
            &format!("merge {merge_id} cancelled before completion"),
        ),
        Err(e) => err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "MERGE_BLOCKED",
            &e.to_string(),
        ),
    }
}

// ─── T1.5 — cancel an in-flight merge by id ──────────────────────────

async fn cancel_merge_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if let Some(token) = state.active_merges.read().await.get(&id).cloned() {
        token.cancel();
        ok_response(serde_json::json!({ "cancelled": id })).into_response()
    } else {
        err_response(
            StatusCode::NOT_FOUND,
            "MERGE_NOT_ACTIVE",
            &format!("no in-flight merge with id '{id}' (already finished or never started)"),
        )
    }
}

async fn rebase_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
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

    let actor = request_user(&headers)
        .map(crate::engine::BranchActor::User)
        .unwrap_or(crate::engine::BranchActor::System);
    let engine = state.engine.read().await;
    match engine.rebase_branch(&root, &branch, actor).await {
        Ok(diff) => ok_response(serde_json::json!({
            "rebased": branch,
            "from_branch": diff.from_branch,
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
            "REBASE_BLOCKED",
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
fn decode_history(payload: &[ChatTurnPayload]) -> Vec<crate::intelligence::synthesizer::ChatTurn> {
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

    // ── Cortex Protocol cancellation contract ──────────────────────
    // The ask path is single-task today: dropping the response
    // future also drops the LLM synthesis call inside `ask()`, so
    // client disconnect IS cancellation. The explicit
    // CancellationToken + DropGuard pair below documents the
    // invariant and provides a hookpoint for the day `ask()` grows
    // its own `Option<CancellationToken>` argument (mirroring
    // `hybrid_retrieve`). Until then it is a no-op guard — the
    // important property is that the pattern matches every other
    // stateful endpoint so a future audit can grep for it
    // uniformly.
    let _cancel = tokio_util::sync::CancellationToken::new();
    let _drop_guard = _cancel.clone().drop_guard();

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
async fn agent_stream_response(state: Arc<AppState>, ws: String, body: AskRequest) -> Response {
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
    let agent_messages = crate::intelligence::agent::chat_turns_to_messages(&chat_history);

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
            reason: body.reason.unwrap_or_else(|| "user declined".to_string()),
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
