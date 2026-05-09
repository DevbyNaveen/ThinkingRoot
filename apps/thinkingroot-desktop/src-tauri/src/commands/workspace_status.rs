//! Slice 0 — desktop subscriber for the daemon's `/status/stream` SSE.
//!
//! Pre-Slice-0 the desktop had **five independent per-view probes**
//! (`pack_estimate`, `llm_health`, `mcp_list_connected`,
//! `workspace_compile_state`, right-rail substrate poll) each calling
//! its own daemon endpoint and answering "is the workspace ready?"
//! with a different boolean. They could (and did) disagree on
//! the same screen.
//!
//! This module replaces all five with one source: a tokio task that
//! holds a long-lived SSE connection to
//! `GET /api/v1/workspaces/{name}/status/stream`, parses each
//! [`WorkspaceStatusEvent`], updates [`AppState::workspace_status`],
//! and emits a Tauri event `workspace_status:{name}` to the front-end.
//! The Zustand hook on the React side subscribes once and every UI
//! surface reads from the same store — a render is no longer a
//! probe.
//!
//! # Lifecycle
//!
//! - `subscribe_workspace_status_stream(name)` is called from the
//!   front-end on every workspace-set-active event. It cancels any
//!   previous subscriber, mints a new [`CancellationToken`], spawns
//!   the loop, and stores a [`WorkspaceStatusSubscriberHandle`] in
//!   [`AppState::workspace_status_subscriber`].
//! - The loop reconnects with exponential backoff on transport
//!   errors. The daemon may restart (cortex protocol attach-or-spawn)
//!   between reconnects; the loop re-resolves the daemon endpoint
//!   via [`SidecarClient::try_resolve_endpoint`] every retry.
//! - On cancel, the loop exits at the next read boundary; the
//!   handle's `cancel` token is the only way to stop it.
//!
//! # Honesty (CLAUDE.md §honesty rule §1)
//!
//! - The cache is updated only on a real `WorkspaceStatusEvent::Snapshot`.
//!   Heartbeats are forwarded as a `workspace_status_heartbeat:{name}`
//!   Tauri event but never alter cached state.
//! - When the SSE stream drops, the cached [`WorkspaceStatus`] keeps
//!   its `as_of` timestamp — the front-end ages it and surfaces
//!   "disconnected — last seen X seconds ago" rather than implying
//!   freshness.
//! - The subscriber never fabricates a snapshot from partial data.
//!   A reconnect that fails to land before the user closes the app
//!   simply means the cache stays at the last real snapshot.

use std::sync::Arc;
use std::time::Duration;

use eventsource_stream::{Event as SseEvent, Eventsource};
use futures::stream::StreamExt;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use thinkingroot_core::types::{WorkspaceStatus, WorkspaceStatusEvent};
use tokio_util::sync::CancellationToken;

use crate::commands::sidecar_client;
use crate::state::{AppState, WorkspaceStatusSubscriberHandle};

/// Maximum backoff between reconnect attempts. Short enough that the
/// front-end perceives the recovery as "subscriber blip"; long enough
/// to avoid hammering a downed daemon.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Initial backoff. Doubles on each consecutive failure up to
/// [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// Tauri event prefix for snapshots.  The full event name is
/// `workspace_status:{name}`. Front-end listens with
/// `tauri.event.listen("workspace_status:CipherVault", …)`.
pub const SNAPSHOT_EVENT_PREFIX: &str = "workspace_status";

/// Tauri event prefix for heartbeats. Used by the front-end to
/// detect stream-alive-but-no-state-change.
pub const HEARTBEAT_EVENT_PREFIX: &str = "workspace_status_heartbeat";

/// Tauri event prefix for stream connection state.  Fires
/// `workspace_status_connection:{name}` with `{ "connected": bool,
/// "reason": Option<String> }` so the front-end can show a
/// "(disconnected)" tag without reading transport-level errors.
pub const CONNECTION_EVENT_PREFIX: &str = "workspace_status_connection";

#[derive(Debug, Clone, Serialize)]
struct ConnectionState {
    connected: bool,
    reason: Option<String>,
}

/// Read the cached status for a workspace. `None` if no subscriber
/// has run for that name (or if it was cleared by an unmount).
#[tauri::command]
pub async fn workspace_status_get(
    app: AppHandle,
    workspace: String,
) -> Result<Option<WorkspaceStatus>, String> {
    let state = app.state::<AppState>();
    let cache = state.workspace_status.read().await;
    Ok(cache.get(&workspace).cloned())
}

/// Snapshot every cached status. Used by the right-rail aggregate
/// view and by `root status` parity tests.
#[tauri::command]
pub async fn workspace_status_get_all(app: AppHandle) -> Result<Vec<WorkspaceStatus>, String> {
    let state = app.state::<AppState>();
    let cache = state.workspace_status.read().await;
    Ok(cache.values().cloned().collect())
}

/// Force the daemon to re-probe the on-disk substrate + sources axes
/// for the given workspace, then refresh the cache from the
/// resulting snapshot.
#[tauri::command]
pub async fn workspace_status_refresh(
    app: AppHandle,
    workspace: String,
) -> Result<WorkspaceStatus, String> {
    let (host, port) = sidecar_client::try_resolve_endpoint(&app)
        .await
        .ok_or_else(|| "local daemon is not reachable".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("client build: {e}"))?;
    let url = format!("http://{host}:{port}/api/v1/workspaces/{workspace}/refresh");
    let resp = client
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("refresh post: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("refresh failed: HTTP {}", resp.status()));
    }
    // Read the latest snapshot via the one-shot endpoint.
    let url = format!("http://{host}:{port}/api/v1/workspaces/{workspace}/status");
    let snap: WorkspaceStatus = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("status get: {e}"))?
        .json()
        .await
        .map_err(|e| format!("status decode: {e}"))?;
    let state = app.state::<AppState>();
    state
        .workspace_status
        .write()
        .await
        .insert(workspace.clone(), snap.clone());
    let _ = app.emit(
        &format!("{SNAPSHOT_EVENT_PREFIX}:{workspace}"),
        snap.clone(),
    );
    Ok(snap)
}

/// Start (or replace) the SSE subscriber for the given workspace.
/// Cancels the previous subscriber if its workspace is different.
/// Returns immediately after the spawn — the front-end listens for
/// the `workspace_status:{name}` event for live data.
#[tauri::command]
pub async fn subscribe_workspace_status_stream(
    app: AppHandle,
    workspace: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut slot = state.workspace_status_subscriber.lock().await;

    // If we're already subscribed to this workspace and the task is
    // alive, leave it running.
    if let Some(existing) = slot.as_ref() {
        if existing.workspace == workspace && !existing.cancel.is_cancelled() {
            return Ok(());
        }
        existing.cancel.cancel();
    }

    let cancel = CancellationToken::new();
    let handle = WorkspaceStatusSubscriberHandle {
        workspace: workspace.clone(),
        cancel: cancel.clone(),
    };
    *slot = Some(handle);
    drop(slot);

    let app_for_task = app.clone();
    let cache = state.workspace_status.clone();
    tokio::spawn(async move {
        run_subscriber(app_for_task, workspace, cancel, cache).await;
    });

    Ok(())
}

/// Cancel the active SSE subscriber. Used by `app_quit` and by
/// `unmount` paths that want to clear the cache before the next
/// workspace mounts.
#[tauri::command]
pub async fn unsubscribe_workspace_status_stream(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut slot = state.workspace_status_subscriber.lock().await;
    if let Some(handle) = slot.take() {
        handle.cancel.cancel();
    }
    Ok(())
}

async fn run_subscriber(
    app: AppHandle,
    workspace: String,
    cancel: CancellationToken,
    cache: Arc<tokio::sync::RwLock<std::collections::HashMap<String, WorkspaceStatus>>>,
) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if cancel.is_cancelled() {
            break;
        }
        match attach(&app, &workspace, &cancel, &cache).await {
            Ok(()) => {
                // Stream closed cleanly (rare — usually a server
                // shutdown). Reset backoff and retry.
                emit_connection(&app, &workspace, false, Some("stream ended".into()));
                backoff = INITIAL_BACKOFF;
            }
            Err(e) => {
                emit_connection(&app, &workspace, false, Some(e.clone()));
                tracing::debug!(
                    target: "workspace_status",
                    workspace = %workspace,
                    "subscriber retrying in {:?}: {e}",
                    backoff
                );
            }
        }
        if cancel.is_cancelled() {
            break;
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    tracing::debug!(target: "workspace_status", workspace = %workspace, "subscriber stopped");
}

/// Open the SSE connection, parse events until the stream ends or
/// cancel fires. Returns `Err` when a transport error occurs (caller
/// retries with backoff); returns `Ok` when the stream closes
/// cleanly.
async fn attach(
    app: &AppHandle,
    workspace: &str,
    cancel: &CancellationToken,
    cache: &Arc<tokio::sync::RwLock<std::collections::HashMap<String, WorkspaceStatus>>>,
) -> Result<(), String> {
    let (host, port) = sidecar_client::try_resolve_endpoint(app)
        .await
        .ok_or_else(|| "daemon endpoint not reachable".to_string())?;
    let url = format!("http://{host}:{port}/api/v1/workspaces/{workspace}/status/stream");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(0)) // long-lived stream
        .build()
        .map_err(|e| format!("client build: {e}"))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("connect: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    emit_connection(app, workspace, true, None);

    let mut stream = resp.bytes_stream().eventsource();
    while let Some(event) = stream.next().await {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let SseEvent { event, data, .. } = match event {
            Ok(e) => e,
            Err(e) => return Err(format!("sse parse: {e}")),
        };
        // Both `snapshot` and `heartbeat` events are encoded as a
        // tagged `WorkspaceStatusEvent`; we deserialize uniformly.
        let parsed: WorkspaceStatusEvent = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "workspace_status",
                    event = %event,
                    "decode failed: {e}"
                );
                continue;
            }
        };
        match parsed {
            WorkspaceStatusEvent::Snapshot(snap) => {
                cache
                    .write()
                    .await
                    .insert(workspace.to_string(), snap.clone());
                let _ = app.emit(&format!("{SNAPSHOT_EVENT_PREFIX}:{workspace}"), snap);
            }
            WorkspaceStatusEvent::Heartbeat { at, .. } => {
                let _ = app.emit(
                    &format!("{HEARTBEAT_EVENT_PREFIX}:{workspace}"),
                    serde_json::json!({ "at": at }),
                );
            }
        }
    }
    emit_connection(app, workspace, false, Some("stream eof".into()));
    Ok(())
}

fn emit_connection(app: &AppHandle, workspace: &str, connected: bool, reason: Option<String>) {
    let _ = app.emit(
        &format!("{CONNECTION_EVENT_PREFIX}:{workspace}"),
        ConnectionState { connected, reason },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::{
        BranchState, CompileState, LlmState, MountState, SourcesState, SubstrateState,
        WorkspaceStatus, WorkspaceStatusEvent,
    };

    #[test]
    fn snapshot_event_round_trips_through_subscriber_decoder() {
        // The subscriber decodes whatever JSON the daemon sent; this
        // test pins the wire-shape contract: a `Snapshot` event from
        // the daemon must deserialize back into a `Snapshot` variant.
        let snap = WorkspaceStatus::assemble(
            "demo".into(),
            std::path::PathBuf::from("/tmp/demo"),
            true,
            SubstrateState::Empty {
                graph_db_bytes: 12_288,
            },
            SourcesState::None,
            MountState::NotMounted,
            LlmState::Configured {
                provider: "anthropic".into(),
                model: None,
            },
            CompileState::Idle {
                last_finished_at: None,
                last_duration_ms: None,
                last_outcome: None,
            },
            BranchState::default(),
        );
        let ev = WorkspaceStatusEvent::Snapshot(snap.clone());
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: WorkspaceStatusEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WorkspaceStatusEvent::Snapshot(s) => assert_eq!(s.name, "demo"),
            _ => panic!("expected Snapshot variant"),
        }
    }

    #[test]
    fn event_prefix_matches_published_constant() {
        assert_eq!(SNAPSHOT_EVENT_PREFIX, "workspace_status");
        assert_eq!(HEARTBEAT_EVENT_PREFIX, "workspace_status_heartbeat");
        assert_eq!(CONNECTION_EVENT_PREFIX, "workspace_status_connection");
    }

    #[tokio::test]
    async fn cache_initially_empty() {
        // Sanity check on the slot shape — ensures the cache type
        // compiles and a fresh registry has no entries.
        let cache: Arc<tokio::sync::RwLock<std::collections::HashMap<String, WorkspaceStatus>>> =
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        assert!(cache.read().await.is_empty());
        let _slot: tokio::sync::Mutex<Option<WorkspaceStatusSubscriberHandle>> =
            tokio::sync::Mutex::new(None);
    }
}
