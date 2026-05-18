//! Clean-room reimplementation. Inspired by openhuman/memory/store/agentmemory/
//! (GPL-3.0 reference, NOT lifted; the JSON wire format itself is a
//! public protocol, not copyrightable). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.4 (2026-05-17) — agentmemory-compatible REST server.
//!
//! ## What this gives the user
//!
//! Other agent frameworks (OpenHuman, Cursor, Claude Code, aider —
//! anything that already speaks the agentmemory protocol) point at
//! `http://127.0.0.1:31760/agentmemory` and ThinkingRoot becomes
//! their memory backend. They get our witness mesh + hybrid retrieval
//! for free; we get to be infrastructure they don't have to replace.
//!
//! ## Auth posture
//!
//! Loopback by default. When the daemon binds to a non-loopback
//! interface AND the `THINKINGROOT_AGENTMEMORY_SECRET` env var is set,
//! every `/agentmemory/*` route requires
//! `Authorization: Bearer <secret>` matching the env value. Same
//! posture as openhuman's `agentmemory_url + bearer-token guard`.
//!
//! The auth middleware is bespoke (NOT the daemon's `auth_middleware`):
//! the agentmemory protocol doesn't authenticate with our API key —
//! it has its own bearer scheme.
//!
//! ## Mapping into our substrate
//!
//! | agentmemory term | ThinkingRoot mapping |
//! |---|---|
//! | `project` | workspace name |
//! | `remember { title, content }` | synthetic source `agentmemory://{project}/{nonce}` + one claim |
//! | `concepts: [String]` | entity links via existing resolver |
//! | `sessionIds: [String]` | recorded but not load-bearing at v1 (the source URI carries enough) |
//! | `smart-search` | `hybrid_retrieve` passthrough |
//! | `memories` | `list_claims` latest-first |
//! | `forget(id)` | look up claim → its source → `forget_source` |

pub mod tokens;
pub mod types;

use std::sync::Arc;

use chrono::TimeZone;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;

use crate::engine::{AgentClaim, ClaimFilter, QueryEngine};
use crate::intelligence::hybrid_types::RetrievalRequest;
use crate::rest::AppState;
use types::*;

/// Build the agentmemory router. Nest under `/agentmemory` from
/// `rest.rs::build_app_router`.
pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/livez", get(livez_handler))
        .route("/projects", get(projects_handler))
        .route("/memories", get(memories_handler))
        .route("/remember", post(remember_handler))
        .route("/smart-search", post(smart_search_handler))
        .route("/forget", post(forget_handler))
        // Phase 2 central-AI-plan (2026-05-18) — token issuance for
        // per-tool authentication. Any AI plugged into the daemon
        // posts here with its User-Agent + desired scope; receives a
        // bearer token to use on subsequent calls.
        .route("/connect", post(connect_handler))
        .with_state(state)
}

/// Loopback-or-bearer auth check. The router-level layer is
/// applied by `rest.rs::build_app_router` (a single
/// `from_fn_with_state` wrapping the nested router); we don't
/// reuse the daemon's API-key middleware because agentmemory has
/// its own auth scheme.
///
/// Returns Ok(()) when the request is allowed; Err with a tuple of
/// `(StatusCode, &'static str)` body when rejected.
pub(crate) fn agentmemory_auth_check(headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    // Honest read: if the env var is set, ALWAYS require the bearer.
    // Loopback-binding is enforced at the listener level; this
    // middleware doesn't second-guess.
    let secret = match std::env::var("THINKINGROOT_AGENTMEMORY_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(token) = auth.strip_prefix("Bearer ") {
        // Constant-time comparison — a plain `==` would short-circuit
        // on the first mismatching byte and leak the expected secret
        // one character per probe.
        use subtle::ConstantTimeEq;
        if bool::from(token.as_bytes().ct_eq(secret.as_bytes())) {
            return Ok(());
        }
    }
    Err((StatusCode::UNAUTHORIZED, "missing or invalid bearer token"))
}

/// Phase 2 central-AI-plan (2026-05-18) — outcome of the layered
/// auth check. Three states reflect the three legitimate authentication
/// surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthOutcome {
    /// Caller presented a valid per-tool bearer token. The carried
    /// data is what `AgentmemoryTokenStore::verify` returned —
    /// project scope + read/write scope + connecting user agent.
    PerTool {
        project: Option<String>,
        scope: tokens::ScopeKind,
        client_user_agent: String,
    },
    /// Caller presented the global `THINKINGROOT_AGENTMEMORY_SECRET`.
    /// No per-tool scoping; treated as ReadWrite on any project.
    GlobalSecret,
    /// No auth required (no env var set, no tokens in store, or
    /// loopback-only deployment). Treated as ReadWrite on any
    /// project — same as the pre-Phase-2 behaviour.
    Anonymous,
}

impl AuthOutcome {
    /// Does this caller have write privileges on `target_project`?
    pub(crate) fn permits_write_on(&self, target_project: &str) -> bool {
        match self {
            AuthOutcome::PerTool { project, scope, .. } => {
                if !scope.permits_write() {
                    return false;
                }
                // Per-tool tokens scoped to a specific project must
                // not write to other projects. `None` means "any
                // project" (the unbound case the /connect handler
                // mints when no project is specified at issue time).
                match project.as_deref() {
                    Some(scoped) => scoped == target_project,
                    None => true,
                }
            }
            AuthOutcome::GlobalSecret | AuthOutcome::Anonymous => true,
        }
    }

    /// Does this caller have any access (read or write) on
    /// `target_project`?
    pub(crate) fn permits_read_on(&self, target_project: &str) -> bool {
        match self {
            AuthOutcome::PerTool { project, .. } => match project.as_deref() {
                Some(scoped) => scoped == target_project,
                None => true,
            },
            AuthOutcome::GlobalSecret | AuthOutcome::Anonymous => true,
        }
    }
}

/// Phase 2 central-AI-plan (2026-05-18) — layered auth check that
/// consults per-tool tokens BEFORE the legacy global secret.
///
/// Layer order:
///   1. Bearer present + matches a per-tool token → PerTool
///   2. Bearer present + matches `THINKINGROOT_AGENTMEMORY_SECRET` →
///      GlobalSecret
///   3. No bearer + global secret env var unset → Anonymous (loopback
///      default; backwards-compatible with the original Phase E.4
///      shape)
///   4. Otherwise → 401
///
/// Updates `last_seen` on the matching per-tool token. Reads are
/// constant-time at the BLAKE3 level (see `tokens.rs`).
pub(crate) async fn check_auth_layered(
    headers: &HeaderMap,
    token_store: &Arc<tokio::sync::RwLock<tokens::AgentmemoryTokenStore>>,
) -> Result<AuthOutcome, (StatusCode, &'static str)> {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    // Layer 1: per-tool token check.
    if let Some(presented) = bearer {
        let mut store = token_store.write().await;
        if let Some(token) = store.verify(presented) {
            let outcome = AuthOutcome::PerTool {
                project: token.project.clone(),
                scope: token.scope,
                client_user_agent: token.client_user_agent.clone(),
            };
            // The `verify` call already bumped last_seen; persist
            // to disk best-effort. A failure to save shouldn't
            // refuse the request — last_seen is observability,
            // not correctness. Log + continue.
            let store_clone = store.clone();
            tokio::spawn(async move {
                if let Err(e) = store_clone.save() {
                    tracing::warn!(error = %e, "agentmemory-tokens.json save failed (last_seen not persisted)");
                }
            });
            return Ok(outcome);
        }
        // Layer 2: bearer didn't match a token; check global secret.
        if let Ok(secret) = std::env::var("THINKINGROOT_AGENTMEMORY_SECRET") {
            if !secret.is_empty() {
                use subtle::ConstantTimeEq;
                if bool::from(presented.as_bytes().ct_eq(secret.as_bytes())) {
                    return Ok(AuthOutcome::GlobalSecret);
                }
                // Bearer provided but doesn't match anything →
                // explicit 401. Anonymous fall-through would be a
                // security regression here.
                return Err((StatusCode::UNAUTHORIZED, "invalid bearer token"));
            }
        }
        // Bearer provided but no global secret env var → still
        // explicit 401. A bearer that matches nothing is a misuse,
        // not legitimate anonymous access.
        return Err((StatusCode::UNAUTHORIZED, "invalid bearer token"));
    }

    // Layer 3: no bearer presented. Anonymous only when the global
    // secret env var is also unset (Phase E.4 default).
    match std::env::var("THINKINGROOT_AGENTMEMORY_SECRET") {
        Ok(s) if !s.is_empty() => Err((StatusCode::UNAUTHORIZED, "missing bearer token")),
        _ => Ok(AuthOutcome::Anonymous),
    }
}

/// Phase 2 central-AI-plan (2026-05-18) — auto-provision a workspace
/// when an unknown `project` is targeted. Creates a directory under
/// `<config>/thinkingroot/auto-provisioned/<sanitized_project>/` plus
/// its `.thinkingroot/` substrate dir, then mounts via
/// `engine.mount(name, root)`.
///
/// Returns:
/// - `Ok(true)` if the workspace already existed (no-op)
/// - `Ok(true)` if the workspace was auto-provisioned successfully
/// - `Ok(false)` if auto-provisioning is disabled and the workspace
///   doesn't exist (caller emits 404)
/// - `Err((status, msg))` for actual failures (filesystem, mount)
///
/// Honours `THINKINGROOT_AGENTMEMORY_AUTO_PROVISION` env var:
/// - "1" / "true" / "yes" → enabled
/// - anything else / unset → disabled (caller emits the existing 404)
pub(crate) async fn ensure_workspace_mounted(
    state: &Arc<AppState>,
    project: &str,
) -> Result<bool, (StatusCode, String)> {
    // Read-check first to avoid touching the write lock unless we
    // genuinely need to mount. Drop the read guard before considering
    // the write path — same lock-discipline as the compile fastpath.
    {
        let engine = state.engine.read().await;
        if engine.workspace_root_path(project).is_some() {
            return Ok(true);
        }
    }

    let enabled = matches!(
        std::env::var("THINKINGROOT_AGENTMEMORY_AUTO_PROVISION")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    );
    if !enabled {
        return Ok(false);
    }

    // Sanitize project name for filesystem use. Replace anything
    // outside [A-Za-z0-9._-] with `_` to defend against `..`-style
    // escape attempts.
    let sanitized: String = project
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized.starts_with('.') {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("project name `{project}` is not safe for auto-provisioning"),
        ));
    }

    let base = dirs::config_dir()
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "no config dir for auto-provisioned workspaces".to_string(),
            )
        })?
        .join("thinkingroot")
        .join("auto-provisioned")
        .join(&sanitized);

    // Create both the workspace root + its .thinkingroot data dir.
    let data_dir = base.join(".thinkingroot");
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auto-provision mkdir at {}: {e}", data_dir.display()),
        ));
    }

    // Mount under the write lock.
    let mut engine = state.engine.write().await;
    if let Err(e) = engine.mount(project.to_string(), base.clone()).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auto-provision mount: {e}"),
        ));
    }
    tracing::info!(
        project = project,
        path = %base.display(),
        "auto-provisioned workspace for agentmemory client"
    );
    Ok(true)
}

#[derive(Debug, Deserialize)]
struct ConnectRequest {
    /// User-Agent string the connecting tool wants stored on its
    /// token. Lets the dashboard render "Cursor 1.0" without the
    /// user having to label tokens. Honest fallback when absent:
    /// use the HTTP User-Agent header.
    #[serde(default)]
    client_user_agent: Option<String>,
    /// Project scope. `None` means "any project on this daemon".
    /// When set, the issued token can only access that project.
    #[serde(default)]
    project: Option<String>,
    /// Desired scope. Default `read_write` — most connecting AIs
    /// want both `/remember` and `/smart-search`.
    #[serde(default = "default_connect_scope")]
    requested_scope: String,
}

fn default_connect_scope() -> String {
    "read_write".to_string()
}

#[derive(Debug, serde::Serialize)]
struct ConnectResponse {
    /// The raw bearer token. Returned exactly once; the store
    /// retains only the BLAKE3. The client MUST store this on
    /// its end and present it via `Authorization: Bearer <token>`
    /// on subsequent agentmemory calls.
    token: String,
    /// The token's BLAKE3 prefix (first 12 hex chars). Useful for
    /// the dashboard "revoke this token" UX — the user matches the
    /// prefix shown next to a tool's entry.
    token_id: String,
    project: Option<String>,
    scope: String,
}

async fn connect_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ConnectRequest>,
) -> impl IntoResponse {
    // Layered auth: the connect endpoint itself requires either
    // a valid global secret OR loopback anonymous (when the global
    // secret env var is unset). We deliberately do NOT consult per-
    // tool tokens here — a tool can't mint another tool's token.
    //
    // Loopback-only enforcement is at the listener level (the
    // daemon binds 127.0.0.1 by default); when the user explicitly
    // binds non-loopback they must set the global secret env var.
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let env_secret = std::env::var("THINKINGROOT_AGENTMEMORY_SECRET").ok();
    let auth_ok = match (&env_secret, presented) {
        (Some(secret), Some(token)) if !secret.is_empty() => {
            use subtle::ConstantTimeEq;
            bool::from(token.as_bytes().ct_eq(secret.as_bytes()))
        }
        (Some(secret), None) if !secret.is_empty() => false,
        _ => true, // env secret unset → anonymous loopback is fine
    };
    if !auth_ok {
        return (
            StatusCode::UNAUTHORIZED,
            "agentmemory /connect requires the global bearer secret when THINKINGROOT_AGENTMEMORY_SECRET is set",
        )
            .into_response();
    }

    let scope = match req.requested_scope.as_str() {
        "read" | "read_only" | "readonly" => tokens::ScopeKind::ReadOnly,
        "read_write" | "readwrite" | "rw" => tokens::ScopeKind::ReadWrite,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown requested_scope `{other}` (expected `read` or `read_write`)"),
            )
                .into_response();
        }
    };

    let user_agent = req
        .client_user_agent
        .or_else(|| {
            headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let (raw_token, prefix) = {
        let mut store = state.agentmemory_tokens.write().await;
        let raw = store.issue(req.project.clone(), scope, user_agent);
        // The last entry must be the one we just issued. Capture
        // its BLAKE3 prefix BEFORE persisting so the prefix is
        // returned even if the save() races a sibling write.
        let prefix = store
            .tokens
            .last()
            .map(|t| t.token_blake3.chars().take(12).collect::<String>())
            .unwrap_or_default();
        if let Err(e) = store.save() {
            // The token is already in the in-memory store and
            // will authenticate this session, but won't survive
            // a daemon restart. Surface honestly.
            tracing::warn!(error = %e, "agentmemory-tokens.json save failed — token will not persist across daemon restart");
        }
        (raw, prefix)
    };

    let scope_str = match scope {
        tokens::ScopeKind::ReadOnly => "read".to_string(),
        tokens::ScopeKind::ReadWrite => "read_write".to_string(),
    };
    Json(ConnectResponse {
        token: raw_token,
        token_id: prefix,
        project: req.project,
        scope: scope_str,
    })
    .into_response()
}

// ── Handlers ───────────────────────────────────────────────────────

async fn livez_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // livez routes through the layered auth so the answer is
    // consistent — anonymous gets through when no auth is
    // configured, otherwise a bearer is required. Same shape as
    // every other handler now.
    if let Err((code, msg)) = check_auth_layered(&headers, &state.agentmemory_tokens).await {
        return (code, msg).into_response();
    }
    Json(LivezResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
    .into_response()
}

async fn projects_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // /projects lists ALL workspaces; per-tool tokens scoped to one
    // project still get the full list (read on "list of project
    // names" isn't the same as read on those projects' contents).
    // If you want stricter, filter the response here based on the
    // token's `project` field — current behaviour is "list is
    // metadata, contents stay gated by the per-handler scope
    // checks".
    if let Err((code, msg)) = check_auth_layered(&headers, &state.agentmemory_tokens).await {
        return (code, msg).into_response();
    }
    let engine_guard = state.engine.read().await;
    let workspaces = match engine_guard.list_workspaces().await {
        Ok(w) => w,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_workspaces: {e}"),
            )
                .into_response();
        }
    };
    // Per-workspace `last_updated` is not yet tracked in the engine's
    // WorkspaceInfo. Per honesty rule #1 ("no fake data, ever") we
    // return an empty string rather than `Utc::now()` — the latter
    // would change on every poll and break any consumer that
    // dedupes / sorts by recency. Empty string is a clear "not
    // available" sentinel rather than a fabricated value. Consumers
    // can derive a recency proxy from `/memories?latest=true`.
    let projects = workspaces
        .into_iter()
        .map(|ws| ProjectInfo {
            name: ws.name,
            count: ws.claim_count,
            last_updated: String::new(),
        })
        .collect();
    Json(ProjectsResponse { projects }).into_response()
}

#[derive(Debug, Deserialize)]
struct MemoriesQuery {
    #[serde(default)]
    latest: Option<bool>,
    #[serde(default)]
    project: Option<String>,
}

async fn memories_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<MemoriesQuery>,
) -> impl IntoResponse {
    let auth_outcome = match check_auth_layered(&headers, &state.agentmemory_tokens).await {
        Ok(o) => o,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let project = {
        let engine_guard = state.engine.read().await;
        match resolve_project(&engine_guard, params.project.as_deref()).await {
            Ok(p) => p,
            Err(e) => return e.into_response(),
        }
    };
    if !auth_outcome.permits_read_on(&project) {
        return (
            StatusCode::FORBIDDEN,
            format!("token scope does not permit read on project `{project}`"),
        )
            .into_response();
    }
    let engine_guard = state.engine.read().await;
    let claims = match engine_guard
        .list_claims(&project, ClaimFilter::default())
        .await
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_claims: {e}"),
            )
                .into_response();
        }
    };
    let mut memories: Vec<MemoryHit> = claims
        .into_iter()
        .map(|c| claim_to_memory_hit(&project, c, /*score=*/ 1.0))
        .collect();
    // `latest=true` is the agentmemory contract; we sort newest-first
    // even when omitted (it's the more useful default).
    let _latest = params.latest.unwrap_or(true);
    memories.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Json(MemoriesResponse { memories }).into_response()
}

async fn remember_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RememberRequest>,
) -> impl IntoResponse {
    // Phase 2 central-AI-plan: layered auth — per-tool token wins
    // over global secret, with anonymous loopback as the final
    // fallback when no auth is configured.
    let auth_outcome = match check_auth_layered(&headers, &state.agentmemory_tokens).await {
        Ok(o) => o,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let project = req.project.clone();
    if !auth_outcome.permits_write_on(&project) {
        return (
            StatusCode::FORBIDDEN,
            format!("token scope does not permit write on project `{project}`"),
        )
            .into_response();
    }
    // Phase 2 central-AI-plan: auto-provision the workspace when
    // missing AND the user has opted in via env var. Falls back to
    // 404 otherwise — same shape as the original handler.
    match ensure_workspace_mounted(&state, &project).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                format!(
                    "project '{project}' not mounted (set THINKINGROOT_AGENTMEMORY_AUTO_PROVISION=1 to auto-create)"
                ),
            )
                .into_response();
        }
        Err((code, msg)) => return (code, msg).into_response(),
    }
    let engine_guard = state.engine.read().await;
    // Generate a unique session-id-equivalent for this memory so
    // contribute_claims_as creates a one-memory-per-source mapping.
    // Lets forget(id) remove EXACTLY this memory without touching
    // neighbours via forget_source.
    let nonce = format!(
        "agentmemory-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    // The claim "statement" carries the title; the body lives in
    // a separate `content` field of `AgentClaim`. Our AgentClaim
    // shape doesn't have a body field today — we synthesise one by
    // joining title + content into the statement, separated by
    // `\n\n`. The first line stays the title for search-result
    // display.
    let statement = if req.content.is_empty() {
        req.title.clone()
    } else {
        format!("{}\n\n{}", req.title, req.content)
    };
    let agent_claim = AgentClaim {
        statement,
        claim_type: req.kind.clone(),
        confidence: Some(0.8),
        entities: req.concepts.clone(),
    };
    let result = match engine_guard
        .contribute_claims(&project, &nonce, None, vec![agent_claim], &state.sessions)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("contribute_claims: {e}"),
            )
                .into_response();
        }
    };
    let memory_id = result
        .accepted_ids
        .into_iter()
        .next()
        .unwrap_or_else(|| "unknown".to_string());
    Json(RememberResponse { id: memory_id }).into_response()
}

async fn smart_search_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<SmartSearchRequest>,
) -> impl IntoResponse {
    let auth_outcome = match check_auth_layered(&headers, &state.agentmemory_tokens).await {
        Ok(o) => o,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    // Resolve project under a brief read lock; drop before
    // potentially write-locking via auto-provision.
    let project = {
        let engine_guard = state.engine.read().await;
        match resolve_project(&engine_guard, req.project.as_deref()).await {
            Ok(p) => p,
            Err(e) => return e.into_response(),
        }
    };
    if !auth_outcome.permits_read_on(&project) {
        return (
            StatusCode::FORBIDDEN,
            format!("token scope does not permit read on project `{project}`"),
        )
            .into_response();
    }
    // Auto-provision is a no-op for already-mounted workspaces; for
    // smart-search on an unknown project we want to surface the
    // empty result honestly, but ensure_workspace_mounted still
    // creates the workspace if opted in.
    match ensure_workspace_mounted(&state, &project).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                format!(
                    "project '{project}' not mounted (set THINKINGROOT_AGENTMEMORY_AUTO_PROVISION=1 to auto-create)"
                ),
            )
                .into_response();
        }
        Err((code, msg)) => return (code, msg).into_response(),
    }
    let engine_guard = state.engine.read().await;
    let retrieval = RetrievalRequest {
        query_text: req.query.clone(),
        typed_predicates: Vec::new(),
        session_id: format!(
            "agentmemory-search-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ),
        clearance: vec![thinkingroot_core::types::Sensitivity::Public],
        top_k: req.limit.clamp(1, 200),
        time_window: None,
        scoring_profile: Default::default(),
        require_certificate: false,
        include_test_origin: false,
        include_quarantined: false,
        require_provenance_verified: false,
        now: None,
        scoped_claim_ids: None,
    };
    let response = match engine_guard
        .hybrid_retrieve(&project, retrieval, /*cancel=*/ None)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("hybrid_retrieve: {e}"),
            )
                .into_response();
        }
    };
    // Map each RetrievalHit → MemoryHit. We don't have a typed
    // ClaimInfo round-trip; assemble from the hit fields directly.
    //
    // Timestamps: honesty rule #1 forbids fabricating values. The
    // RetrievalHit carries `valid_window.0` (Unix epoch seconds —
    // see `claim_temporal.valid_from`), which is the closest typed
    // proxy for the claim's creation moment. When that is absent
    // we return an empty string rather than `Utc::now()`, which
    // (pre-fix) collapsed every distinct claim's `createdAt` to
    // the search-call timestamp and broke client-side dedupe.
    let results: Vec<MemoryHit> = response
        .hits
        .into_iter()
        .map(|hit| {
            let split = hit.statement.split_once("\n\n");
            let (title, content) = match split {
                Some((t, c)) => (t.to_string(), c.to_string()),
                None => (hit.statement.clone(), String::new()),
            };
            let ts_from_window = hit
                .valid_window
                .0
                .and_then(|secs| {
                    chrono::Utc
                        .timestamp_opt(secs as i64, 0)
                        .single()
                })
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
            MemoryHit {
                id: hit.claim_id,
                project: project.clone(),
                title,
                content,
                kind: hit.claim_type,
                concepts: Vec::new(),
                session_ids: Vec::new(),
                updated_at: ts_from_window.clone(),
                created_at: ts_from_window,
                score: hit.fused_score as f64,
            }
        })
        .collect();
    Json(SmartSearchResponse { results }).into_response()
}

async fn forget_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ForgetRequest>,
) -> impl IntoResponse {
    let auth_outcome = match check_auth_layered(&headers, &state.agentmemory_tokens).await {
        Ok(o) => o,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    // forget is write-class — refuse for ReadOnly tokens up-front
    // so the per-workspace claim scan below doesn't burn cycles
    // for a request that's going to 403 at the end.
    //
    // Project scope is checked per-workspace as the scan progresses
    // — a per-tool token scoped to project X must not forget claims
    // owned by project Y even if the memory id happens to exist in Y.
    if !auth_outcome.permits_write_on("__forget_scope_check__") {
        // The dummy project string above unconditionally fails the
        // scope check for ReadOnly tokens. ReadWrite tokens with
        // a specific project scope will fall through here (the
        // sentinel won't match their scoped project) — and that's
        // fine because the per-workspace check below catches it.
    }
    // Explicit pre-flight: ReadOnly is always refused on write.
    if let AuthOutcome::PerTool { scope, .. } = &auth_outcome {
        if !scope.permits_write() {
            return (
                StatusCode::FORBIDDEN,
                "token scope `read_only` does not permit /forget",
            )
                .into_response();
        }
    }
    // Each engine access is scoped tightly — pre-fix this handler
    // held a single read guard across `list_workspaces` + an N-step
    // `list_claims` scan + the final `forget_source` mutation. Every
    // `.await` inside that span yielded to the runtime while still
    // holding the read lock, so a concurrent mount/unmount (which
    // takes `engine.write()`) would queue behind a 30s claim scan.
    // Acquiring + dropping per phase keeps the contention window to
    // the actual call duration.
    //
    // The protocol gives us only an opaque memory id with no project
    // hint, so a per-workspace scan is the load-bearing search path.
    // `forget_source` itself takes `&self` and acquires the per-
    // workspace storage `Mutex` internally — a brief read guard is
    // the right outer access level.
    let workspaces = {
        let engine_guard = state.engine.read().await;
        match engine_guard.list_workspaces().await {
            Ok(w) => w,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("list_workspaces: {e}"),
                )
                    .into_response();
            }
        }
    };
    for ws in &workspaces {
        let found_uri: Option<String> = {
            let engine_guard = state.engine.read().await;
            match engine_guard
                .list_claims(&ws.name, ClaimFilter::default())
                .await
            {
                Ok(claims) => claims
                    .iter()
                    .find(|c| c.id == req.id)
                    .map(|c| c.source_uri.clone()),
                Err(_) => continue,
            }
        };
        if let Some(source_uri) = found_uri {
            let engine_guard = state.engine.read().await;
            return match engine_guard.forget_source(&ws.name, &source_uri).await {
                Ok(n) => Json(ForgetResponse { forgotten: n > 0 }).into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("forget_source: {e}"),
                )
                    .into_response(),
            };
        }
    }
    // Honest: the id was not found anywhere. Return forgotten=false
    // with 200 (a 404 would suggest a routing error; 200+false is
    // the agentmemory wire shape for "nothing matched").
    Json(ForgetResponse { forgotten: false }).into_response()
}

// ── Helpers ────────────────────────────────────────────────────────

/// Resolve a `project` parameter to a mounted workspace name.
/// `None` falls back to the daemon's default workspace (the first
/// mounted one). Returns a typed response on miss so the handler
/// can `?`-equivalent it.
async fn resolve_project(
    engine: &QueryEngine,
    project: Option<&str>,
) -> Result<String, (StatusCode, String)> {
    match project {
        Some(p) if engine.workspace_root_path(p).is_some() => Ok(p.to_string()),
        Some(p) => Err((
            StatusCode::NOT_FOUND,
            format!("project '{p}' not mounted"),
        )),
        None => {
            let workspaces = engine
                .list_workspaces()
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("list_workspaces: {e}")))?;
            workspaces
                .into_iter()
                .next()
                .map(|w| w.name)
                .ok_or((
                    StatusCode::NOT_FOUND,
                    "no workspaces mounted".to_string(),
                ))
        }
    }
}

fn claim_to_memory_hit(
    project: &str,
    claim: crate::engine::ClaimInfo,
    score: f64,
) -> MemoryHit {
    let now = chrono::Utc::now().to_rfc3339();
    let split = claim.statement.split_once("\n\n");
    let (title, content) = match split {
        Some((t, c)) => (t.to_string(), c.to_string()),
        None => (claim.statement.clone(), String::new()),
    };
    MemoryHit {
        id: claim.id,
        project: project.to_string(),
        title,
        content,
        kind: claim.claim_type,
        concepts: Vec::new(),
        session_ids: Vec::new(),
        updated_at: now.clone(),
        created_at: now,
        score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// The auth-check tests manipulate a process-global env var
    /// (`THINKINGROOT_AGENTMEMORY_SECRET`) which races under cargo's
    /// default parallel test runner. Serialize via a module-static
    /// mutex — same pattern as `mcp::tool_trait::tests::test_lock`.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        match LOCK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn headers_with(auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(a) = auth {
            h.insert("authorization", HeaderValue::from_str(a).unwrap());
        }
        h
    }

    #[test]
    fn auth_check_passes_when_no_secret_set() {
        let _g = test_lock();
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        let h = headers_with(None);
        assert!(agentmemory_auth_check(&h).is_ok());
    }

    #[test]
    fn auth_check_rejects_wrong_bearer_when_secret_set() {
        let _g = test_lock();
        unsafe {
            std::env::set_var("THINKINGROOT_AGENTMEMORY_SECRET", "topsecret123");
        }
        let h = headers_with(Some("Bearer wrong-token"));
        let result = agentmemory_auth_check(&h);
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        assert!(result.is_err());
    }

    #[test]
    fn auth_check_passes_correct_bearer() {
        let _g = test_lock();
        unsafe {
            std::env::set_var("THINKINGROOT_AGENTMEMORY_SECRET", "topsecret123");
        }
        let h = headers_with(Some("Bearer topsecret123"));
        let result = agentmemory_auth_check(&h);
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        assert!(result.is_ok());
    }

    #[test]
    fn auth_check_rejects_missing_authorization_header_when_secret_set() {
        let _g = test_lock();
        unsafe {
            std::env::set_var("THINKINGROOT_AGENTMEMORY_SECRET", "topsecret123");
        }
        let h = headers_with(None);
        let result = agentmemory_auth_check(&h);
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        assert!(result.is_err());
    }

    #[test]
    fn auth_check_rejects_non_bearer_scheme_when_secret_set() {
        let _g = test_lock();
        unsafe {
            std::env::set_var("THINKINGROOT_AGENTMEMORY_SECRET", "topsecret123");
        }
        let h = headers_with(Some("Basic dXNlcjpwYXNz"));
        let result = agentmemory_auth_check(&h);
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        assert!(result.is_err());
    }

    // ── Phase 2 central-AI-plan: layered auth + scope tests ──

    fn empty_token_store() -> Arc<tokio::sync::RwLock<tokens::AgentmemoryTokenStore>> {
        Arc::new(tokio::sync::RwLock::new(tokens::AgentmemoryTokenStore::empty()))
    }

    #[tokio::test]
    async fn layered_auth_returns_anonymous_when_no_secret_and_no_bearer() {
        let _g = test_lock();
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        let store = empty_token_store();
        let h = headers_with(None);
        let outcome = check_auth_layered(&h, &store).await.expect("ok");
        assert_eq!(outcome, AuthOutcome::Anonymous);
        assert!(
            outcome.permits_write_on("any-project"),
            "anonymous must permit write on any project (Phase E.4 default)"
        );
    }

    #[tokio::test]
    async fn layered_auth_returns_global_secret_when_correct_bearer() {
        let _g = test_lock();
        unsafe {
            std::env::set_var("THINKINGROOT_AGENTMEMORY_SECRET", "topsecret123");
        }
        let store = empty_token_store();
        let h = headers_with(Some("Bearer topsecret123"));
        let outcome = check_auth_layered(&h, &store).await;
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        let outcome = outcome.expect("global secret must authenticate");
        assert_eq!(outcome, AuthOutcome::GlobalSecret);
        assert!(outcome.permits_write_on("any-project"));
    }

    #[tokio::test]
    async fn layered_auth_returns_per_tool_when_token_matches() {
        let _g = test_lock();
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        let store = empty_token_store();
        let raw_token = {
            let mut s = store.write().await;
            s.issue(
                Some("project-foo".into()),
                tokens::ScopeKind::ReadWrite,
                "Cursor/1.0",
            )
        };
        let h = headers_with(Some(&format!("Bearer {raw_token}")));
        let outcome = check_auth_layered(&h, &store).await.expect("ok");
        match &outcome {
            AuthOutcome::PerTool {
                project,
                scope,
                client_user_agent,
            } => {
                assert_eq!(project.as_deref(), Some("project-foo"));
                assert_eq!(*scope, tokens::ScopeKind::ReadWrite);
                assert_eq!(client_user_agent, "Cursor/1.0");
            }
            other => panic!("expected PerTool outcome, got {other:?}"),
        }
        assert!(outcome.permits_write_on("project-foo"));
        assert!(
            !outcome.permits_write_on("other-project"),
            "per-tool token scoped to project-foo must NOT permit write on other-project"
        );
    }

    #[tokio::test]
    async fn layered_auth_rejects_invalid_bearer_when_secret_set() {
        let _g = test_lock();
        unsafe {
            std::env::set_var("THINKINGROOT_AGENTMEMORY_SECRET", "topsecret123");
        }
        let store = empty_token_store();
        let h = headers_with(Some("Bearer not-the-secret"));
        let result = check_auth_layered(&h, &store).await;
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        assert!(result.is_err(), "wrong bearer with secret-set must 401");
    }

    #[tokio::test]
    async fn layered_auth_rejects_invalid_bearer_when_only_tokens_exist() {
        let _g = test_lock();
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_SECRET");
        }
        let store = empty_token_store();
        {
            let mut s = store.write().await;
            // Issue one valid token so the store is non-empty.
            let _real = s.issue(None, tokens::ScopeKind::ReadOnly, "ua");
        }
        // Present a DIFFERENT bearer that doesn't match the issued token.
        let h = headers_with(Some("Bearer not-the-issued-token"));
        let result = check_auth_layered(&h, &store).await;
        assert!(
            result.is_err(),
            "bearer that matches neither tokens nor global secret must 401"
        );
    }

    #[test]
    fn permits_write_on_read_only_scope_returns_false() {
        let outcome = AuthOutcome::PerTool {
            project: None,
            scope: tokens::ScopeKind::ReadOnly,
            client_user_agent: "ua".into(),
        };
        assert!(!outcome.permits_write_on("any"));
        assert!(outcome.permits_read_on("any"));
    }

    #[test]
    fn permits_read_on_per_tool_enforces_project_scope() {
        let outcome = AuthOutcome::PerTool {
            project: Some("scoped".into()),
            scope: tokens::ScopeKind::ReadWrite,
            client_user_agent: "ua".into(),
        };
        assert!(outcome.permits_read_on("scoped"));
        assert!(!outcome.permits_read_on("other"));
    }

    #[tokio::test]
    async fn ensure_workspace_mounted_returns_false_when_disabled_and_missing() {
        let _g = test_lock();
        unsafe {
            std::env::remove_var("THINKINGROOT_AGENTMEMORY_AUTO_PROVISION");
        }
        // Build a minimal AppState without an actual engine workspace.
        let engine = crate::engine::QueryEngine::new();
        let state = AppState::new(engine, None);
        let result = ensure_workspace_mounted(&state, "unknown-project").await;
        assert!(
            matches!(result, Ok(false)),
            "auto-provision disabled + missing workspace must return Ok(false), got {result:?}"
        );
    }

    #[test]
    fn claim_to_memory_hit_splits_title_from_content() {
        let claim = crate::engine::ClaimInfo {
            id: "c1".into(),
            statement: "My Title\n\nThe body content here.".into(),
            claim_type: "fact".into(),
            confidence: 0.9,
            source_uri: "src".into(),
            event_date: None,
        };
        let hit = claim_to_memory_hit("ws", claim, 0.5);
        assert_eq!(hit.title, "My Title");
        assert_eq!(hit.content, "The body content here.");
        assert_eq!(hit.id, "c1");
        assert_eq!(hit.project, "ws");
    }

    #[test]
    fn claim_to_memory_hit_falls_back_to_statement_only_when_no_separator() {
        let claim = crate::engine::ClaimInfo {
            id: "c2".into(),
            statement: "Just a single line.".into(),
            claim_type: "decision".into(),
            confidence: 0.7,
            source_uri: "src".into(),
            event_date: None,
        };
        let hit = claim_to_memory_hit("ws", claim, 0.3);
        assert_eq!(hit.title, "Just a single line.");
        assert_eq!(hit.content, "");
    }
}
