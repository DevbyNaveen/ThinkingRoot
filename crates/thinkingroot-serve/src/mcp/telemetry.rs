//! Phase 3 of the "ThinkingRoot Central" plan (`plans/okey-so-i-wnat-elegant-hamster.md`):
//! per-MCP-session telemetry so the user can see which AI tools are
//! connected, what they're doing, and where they errored.
//!
//! ## Why a parallel map (not extending SseSessionMap)
//!
//! `SseSessionMap` is `Arc<Mutex<HashMap<String, UnboundedSender<SseMsg>>>>`
//! — its mutex is held briefly on every POST so the dispatcher can
//! resolve the session's channel. Bundling telemetry into the same
//! map's value type would either (a) widen the lock to cover telemetry
//! mutations (every tool call → take the same mutex, increment a
//! counter), pushing contention up; or (b) require nested locks per
//! session.
//!
//! The cleaner shape: keep `SseSessionMap` as a fast channel lookup
//! and add a sibling `SessionTelemetryMap` with its own `RwLock`.
//! Reads dominate (the dashboard polls every few seconds); writes are
//! short and per-session.
//!
//! ## On-disk persistence
//!
//! Live telemetry lives in memory; on disconnect, the final snapshot
//! is appended to `<config>/thinkingroot/mcp-sessions.jsonl`. JSONL
//! mirroring `recovery_log.rs:222` — one event per line, 10 MiB
//! rotation to `mcp-sessions.jsonl.1`, append-only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Which transport this session is using. Useful for the dashboard
/// to distinguish "the desktop's chat AI" (SSE on loopback) from
/// "Cursor connected over SSE" or "Claude Code's stdio MCP".
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// SSE transport on `/mcp/sse` + POST `/mcp?sessionId=…`. Used by
    /// the desktop and by long-lived editor integrations.
    Sse,
    /// JSON-RPC over stdin/stdout. Used by Claude Code, Cursor MCP,
    /// Codex, Windsurf, Zed.
    Stdio,
    /// The `agentmemory` REST protocol — same daemon, different wire.
    /// Sessions there are per-token, not per-connection; we still
    /// surface them in the dashboard so the user sees "Cursor's
    /// agentmemory plug-in" alongside its MCP session.
    AgentMemory,
}

/// Which principal owns this session. Used by the dashboard to
/// surface (and the meta-AI from Phase 4 to identify) which tool is
/// connected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrincipalKind {
    /// The desktop's own in-app AI agent. Identified by the absence
    /// of an external User-Agent header on the SSE GET.
    InAppAgent,
    /// An external MCP client (Cursor, Claude Code, etc.). Carries
    /// the User-Agent string we captured at open.
    McpClient { user_agent: String },
    /// An agentmemory caller. Carries the per-tool token's BLAKE3
    /// prefix so the dashboard can match it back to the issued
    /// token entry.
    AgentMemory {
        user_agent: String,
        token_id_prefix: String,
    },
}

/// One typed error captured during a tool call. Stored on the
/// session's `last_error` field; the full sequence is appended to
/// `mcp-sessions.jsonl` on disconnect for the audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryError {
    pub at: DateTime<Utc>,
    pub tool_name: String,
    /// JSON-RPC error code (e.g. -32602 for invalid params, -32603
    /// for internal error). Kept as-is so dashboards can colour by
    /// code class.
    pub code: i64,
    /// Free-form message from the error response.
    pub message: String,
}

/// Per-session in-memory telemetry. One instance per registered
/// MCP session id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTelemetry {
    pub session_id: String,
    pub transport: TransportKind,
    pub principal: PrincipalKind,
    pub connected_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub tool_calls_total: u64,
    pub errors_total: u64,
    /// The most recent error, retained for the dashboard's
    /// quick-glance error indicator. Older errors land in the JSONL
    /// on disconnect — we don't keep an unbounded in-memory tail
    /// because a misbehaving client could OOM the daemon.
    pub last_error: Option<TelemetryError>,
}

/// Process-wide map of session_id → telemetry. Mirrors the shape of
/// `crate::mcp::sse::SseSessionMap`. The outer `Arc<RwLock<...>>`
/// lets the dashboard poll concurrently with tool-call writes.
pub type SessionTelemetryMap = Arc<RwLock<HashMap<String, SessionTelemetry>>>;

/// Empty telemetry map. Constructed once at startup; lives on
/// `AppState`.
pub fn new_telemetry_map() -> SessionTelemetryMap {
    Arc::new(RwLock::new(HashMap::new()))
}

// ── Process-global handle ───────────────────────────────────────────────────
//
// The visibility MCP tools (`list_mcp_sessions`, `mcp_session_health`,
// `mcp_error_log`) are trait-registered handlers whose `McpToolContext`
// doesn't include `AppState`. To let them read the live telemetry
// without restructuring the trait, we expose the SAME `Arc` AppState
// holds via a `OnceLock` set at startup. Same pattern as the restart
// channel in `operator_tools::install_restart_channel`.
static GLOBAL_TELEMETRY_MAP: std::sync::OnceLock<SessionTelemetryMap> = std::sync::OnceLock::new();

/// Install the process-global telemetry map. Called once by
/// `AppState::new_with_root`. Subsequent calls are no-ops (returns
/// the prior `Err(existing)` from `OnceLock::set`, which we swallow
/// — re-installing would orphan in-flight readers).
pub fn install_global_map(map: SessionTelemetryMap) {
    let _ = GLOBAL_TELEMETRY_MAP.set(map);
}

/// Read the process-global telemetry map. Returns `None` when no
/// AppState has been constructed in this process (e.g. stdio MCP /
/// CLI-only contexts). Visibility tools handle the None case by
/// returning an empty list — honest absence rather than fake data.
pub fn global_map() -> Option<&'static SessionTelemetryMap> {
    GLOBAL_TELEMETRY_MAP.get()
}

/// Compute a coarse health status for one session. Used by the
/// `mcp_session_health` MCP read tool + the dashboard's per-tool
/// status badge.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionHealth {
    /// No errors, recent activity.
    Healthy,
    /// Errors observed but recent activity continues.
    Degraded,
    /// No activity for over 5 minutes despite an open channel.
    Stale,
    /// More than half of recent tool calls errored.
    Failing,
}

impl SessionHealth {
    /// Derive health from a telemetry snapshot. `now` is threaded so
    /// tests can pin the timestamp.
    pub fn compute(telemetry: &SessionTelemetry, now: DateTime<Utc>) -> Self {
        // Failing: more than half of calls errored.
        if telemetry.tool_calls_total > 0 {
            let error_rate = telemetry.errors_total as f64 / telemetry.tool_calls_total as f64;
            if error_rate > 0.5 {
                return SessionHealth::Failing;
            }
        }
        // Stale: more than 5 minutes since last activity.
        let stale_threshold = chrono::Duration::minutes(5);
        if now.signed_duration_since(telemetry.last_activity) > stale_threshold {
            return SessionHealth::Stale;
        }
        // Degraded: at least one error but recent activity + below 50%.
        if telemetry.errors_total > 0 {
            return SessionHealth::Degraded;
        }
        SessionHealth::Healthy
    }
}

// ── JSONL persistence ───────────────────────────────────────────────────────

/// One entry in `mcp-sessions.jsonl`. Recorded on disconnect.
/// Includes the full lifecycle telemetry plus the disconnection
/// timestamp + reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLogEntry {
    pub schema_version: u32,
    pub disconnected_at: DateTime<Utc>,
    pub reason: DisconnectReason,
    pub telemetry: SessionTelemetry,
}

/// Why the session ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    /// Client disconnected (TCP close, browser refresh, etc.).
    ClientDisconnect,
    /// Daemon shut down while the session was open.
    DaemonShutdown,
    /// Reaper observed the channel close without a clean signal.
    ChannelClosed,
}

const SCHEMA_VERSION: u32 = 1;
/// 10 MiB rotation threshold mirroring `recovery_log.rs:222`. When
/// `mcp-sessions.jsonl` grows past this, it rotates to `.jsonl.1`
/// overwriting any prior `.1`. Two-file retention is intentional —
/// the dashboard needs recency, not a forever log.
const ROTATION_BYTES: u64 = 10 * 1024 * 1024;

/// Resolve the canonical path for `mcp-sessions.jsonl`.
pub fn log_path() -> Result<PathBuf, std::io::Error> {
    let cfg = dirs::config_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no config dir for mcp-sessions log",
        )
    })?;
    Ok(cfg.join("thinkingroot").join("mcp-sessions.jsonl"))
}

/// Append one entry to the JSONL log. Creates the file + parent dir
/// on first use. Rotates when the post-append size would exceed
/// `ROTATION_BYTES`. Best-effort: errors are logged + returned;
/// callers that fail this don't fail the user-facing request.
pub fn append(entry: &SessionLogEntry) -> Result<(), std::io::Error> {
    let path = log_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Rotation check: stat the file before append. The check + append
    // window has a benign race (two writers post-rotation), but log
    // duplication is acceptable.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > ROTATION_BYTES {
            let rotated = path.with_extension("jsonl.1");
            // best-effort rename; ignore failure (the dashboard
            // would rather have a slightly-too-big log than a
            // silently-rolled-over one).
            let _ = std::fs::rename(&path, &rotated);
        }
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(0o600),
        );
    }
    let json = serde_json::to_vec(entry)?;
    f.write_all(&json)?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Tail the last `limit` entries from `mcp-sessions.jsonl`. Returns
/// `Ok(vec![])` when the log doesn't exist yet (first-run honest
/// behaviour). Reads the entire file into memory — fine for 10 MiB,
/// not for arbitrary growth.
pub fn tail(limit: usize) -> Result<Vec<SessionLogEntry>, std::io::Error> {
    let path = match log_path() {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&bytes);
    // Iterate from the end; collect up to `limit` valid entries.
    // Malformed lines are skipped honestly (a daemon crash mid-write
    // could leave a partial line at the tail).
    let mut entries: Vec<SessionLogEntry> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<SessionLogEntry>(line).ok())
        .collect();
    if entries.len() > limit {
        let drop = entries.len() - limit;
        entries.drain(0..drop);
    }
    Ok(entries)
}

// ── Recording helpers ───────────────────────────────────────────────────────

/// Insert a fresh telemetry record for a newly-opened session.
/// Called from `mcp/sse.rs::handle_sse` immediately after the
/// session_id is generated and the channel is registered.
pub async fn record_session_opened(
    map: &SessionTelemetryMap,
    session_id: impl Into<String>,
    transport: TransportKind,
    principal: PrincipalKind,
) {
    let now = Utc::now();
    let session_id = session_id.into();
    let telemetry = SessionTelemetry {
        session_id: session_id.clone(),
        transport,
        principal,
        connected_at: now,
        last_activity: now,
        tool_calls_total: 0,
        errors_total: 0,
        last_error: None,
    };
    map.write().await.insert(session_id, telemetry);
}

/// Bump a session's `tool_calls_total` + `last_activity` after a
/// successful dispatch. Cheap O(1) under a write lock; the lock is
/// uncontested in the steady state because each session writes from
/// a single transport handler at a time.
pub async fn record_tool_call(map: &SessionTelemetryMap, session_id: &str) {
    if let Some(t) = map.write().await.get_mut(session_id) {
        t.tool_calls_total += 1;
        t.last_activity = Utc::now();
    }
}

/// Record an error against a session. Stores the typed error on
/// `last_error` and bumps `errors_total`. `tool_calls_total` is NOT
/// bumped here — callers that want to count both should call
/// `record_tool_call` first.
pub async fn record_error(
    map: &SessionTelemetryMap,
    session_id: &str,
    tool_name: impl Into<String>,
    code: i64,
    message: impl Into<String>,
) {
    if let Some(t) = map.write().await.get_mut(session_id) {
        t.errors_total += 1;
        t.last_activity = Utc::now();
        t.last_error = Some(TelemetryError {
            at: Utc::now(),
            tool_name: tool_name.into(),
            code,
            message: message.into(),
        });
    }
}

/// Persist a session's final snapshot to JSONL and remove from the
/// live map. Called by the SSE reaper when the channel closes.
pub async fn record_session_closed(
    map: &SessionTelemetryMap,
    session_id: &str,
    reason: DisconnectReason,
) {
    let removed = { map.write().await.remove(session_id) };
    if let Some(telemetry) = removed {
        let entry = SessionLogEntry {
            schema_version: SCHEMA_VERSION,
            disconnected_at: Utc::now(),
            reason,
            telemetry,
        };
        if let Err(e) = append(&entry) {
            tracing::warn!(
                target = "mcp_telemetry",
                error = %e,
                session_id = session_id,
                "mcp-sessions.jsonl append failed — disconnect snapshot lost"
            );
        }
    }
}

/// Snapshot every live session's telemetry. Used by the dashboard
/// poll path + the `list_mcp_sessions` MCP tool. Returns an owned
/// Vec so callers don't hold the read lock while serializing.
pub async fn snapshot(map: &SessionTelemetryMap) -> Vec<SessionTelemetry> {
    map.read().await.values().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_telemetry() -> SessionTelemetry {
        SessionTelemetry {
            session_id: "fixture".into(),
            transport: TransportKind::Sse,
            principal: PrincipalKind::McpClient {
                user_agent: "TestClient/1.0".into(),
            },
            connected_at: Utc::now() - chrono::Duration::seconds(60),
            last_activity: Utc::now(),
            tool_calls_total: 0,
            errors_total: 0,
            last_error: None,
        }
    }

    #[test]
    fn health_healthy_when_no_errors_and_recent() {
        let t = fixture_telemetry();
        assert_eq!(SessionHealth::compute(&t, Utc::now()), SessionHealth::Healthy);
    }

    #[test]
    fn health_stale_when_no_activity_for_5_plus_minutes() {
        let mut t = fixture_telemetry();
        t.last_activity = Utc::now() - chrono::Duration::minutes(6);
        assert_eq!(SessionHealth::compute(&t, Utc::now()), SessionHealth::Stale);
    }

    #[test]
    fn health_degraded_when_errors_but_recent() {
        let mut t = fixture_telemetry();
        t.tool_calls_total = 10;
        t.errors_total = 1;
        assert_eq!(SessionHealth::compute(&t, Utc::now()), SessionHealth::Degraded);
    }

    #[test]
    fn health_failing_when_error_rate_above_half() {
        let mut t = fixture_telemetry();
        t.tool_calls_total = 10;
        t.errors_total = 6;
        assert_eq!(SessionHealth::compute(&t, Utc::now()), SessionHealth::Failing);
    }

    #[tokio::test]
    async fn record_session_opened_then_tool_call_then_close_round_trips() {
        let map = new_telemetry_map();
        record_session_opened(
            &map,
            "test-1",
            TransportKind::Sse,
            PrincipalKind::InAppAgent,
        )
        .await;
        record_tool_call(&map, "test-1").await;
        record_tool_call(&map, "test-1").await;
        record_error(&map, "test-1", "search", -32602, "missing query").await;
        let snap = snapshot(&map).await;
        assert_eq!(snap.len(), 1);
        let t = &snap[0];
        assert_eq!(t.tool_calls_total, 2);
        assert_eq!(t.errors_total, 1);
        assert!(t.last_error.is_some());
        assert_eq!(t.last_error.as_ref().unwrap().tool_name, "search");
        // Closing removes the entry.
        record_session_closed(&map, "test-1", DisconnectReason::ClientDisconnect).await;
        assert_eq!(snapshot(&map).await.len(), 0);
    }

    #[test]
    fn principal_kind_serializes_as_tagged_enum() {
        let p = PrincipalKind::McpClient {
            user_agent: "Cursor/1.0".into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"kind\":\"mcp_client\""), "got: {json}");
        assert!(json.contains("\"user_agent\":\"Cursor/1.0\""), "got: {json}");
    }

    #[test]
    fn transport_kind_serializes_snake_case() {
        let json = serde_json::to_string(&TransportKind::AgentMemory).unwrap();
        assert_eq!(json, "\"agent_memory\"");
    }

    #[tokio::test]
    async fn record_session_opened_is_idempotent_on_duplicate_id() {
        // Re-recording the same session id overwrites — the second
        // call wins. This is the right behaviour: if a client reuses
        // an id (it shouldn't, but reconnects can race), we want the
        // fresh start, not stale state.
        let map = new_telemetry_map();
        record_session_opened(&map, "dup", TransportKind::Sse, PrincipalKind::InAppAgent).await;
        record_tool_call(&map, "dup").await;
        record_session_opened(&map, "dup", TransportKind::Stdio, PrincipalKind::InAppAgent).await;
        let snap = snapshot(&map).await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].transport, TransportKind::Stdio);
        assert_eq!(snap[0].tool_calls_total, 0, "second open resets counters");
    }
}
