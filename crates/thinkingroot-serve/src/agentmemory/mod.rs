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

// ── Handlers ───────────────────────────────────────────────────────

async fn livez_handler(headers: HeaderMap) -> impl IntoResponse {
    if let Err((code, msg)) = agentmemory_auth_check(&headers) {
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
    if let Err((code, msg)) = agentmemory_auth_check(&headers) {
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
    if let Err((code, msg)) = agentmemory_auth_check(&headers) {
        return (code, msg).into_response();
    }
    let engine_guard = state.engine.read().await;
    let project = match resolve_project(&engine_guard, params.project.as_deref()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
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
    if let Err((code, msg)) = agentmemory_auth_check(&headers) {
        return (code, msg).into_response();
    }
    let engine_guard = state.engine.read().await;
    let project = req.project.clone();
    if engine_guard.workspace_root_path(&project).is_none() {
        return (
            StatusCode::NOT_FOUND,
            format!("project '{project}' not mounted"),
        )
            .into_response();
    }
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
    if let Err((code, msg)) = agentmemory_auth_check(&headers) {
        return (code, msg).into_response();
    }
    let engine_guard = state.engine.read().await;
    let project = match resolve_project(&engine_guard, req.project.as_deref()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
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
    if let Err((code, msg)) = agentmemory_auth_check(&headers) {
        return (code, msg).into_response();
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
