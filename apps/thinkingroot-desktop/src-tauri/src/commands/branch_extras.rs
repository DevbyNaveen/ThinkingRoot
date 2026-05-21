//! Stream D — Tauri commands for branch extras (events, stats,
//! lineage, rebase, rollback). All routes already exist on the daemon
//! side; these are thin sidecar bindings.

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::sync::CancellationToken;

use crate::commands::sidecar_client::SidecarClient;
use crate::state::{AppState, BranchEventSubscriberHandle};

#[derive(Debug, Serialize, Clone)]
pub struct BranchStatsView {
    pub branch: String,
    pub claim_count: u64,
    pub entity_count: u64,
    pub source_count: u64,
    pub event_count: u64,
    pub status: String,
}

#[tauri::command]
pub async fn branch_events(app: AppHandle, branch: String) -> Result<Value, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/events", urlencode(&branch));
    let data: Value = client.get(&path).await?;
    Ok(data
        .get("events")
        .cloned()
        .unwrap_or(Value::Array(Vec::new())))
}

#[tauri::command]
pub async fn branch_stats(app: AppHandle, branch: String) -> Result<BranchStatsView, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/stats", urlencode(&branch));
    let data: Value = client.get(&path).await?;
    Ok(BranchStatsView {
        branch: data
            .get("branch")
            .and_then(|v| v.as_str())
            .unwrap_or(&branch)
            .to_string(),
        claim_count: data
            .get("claim_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        entity_count: data
            .get("entity_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        source_count: data
            .get("source_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        event_count: data
            .get("event_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        status: data
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

#[tauri::command]
pub async fn branch_lineage(app: AppHandle) -> Result<Value, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let data: Value = client.get("/api/v1/branches/lineage").await?;
    Ok(data)
}

#[tauri::command]
pub async fn branch_rebase(app: AppHandle, branch: String) -> Result<(), String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/rebase", urlencode(&branch));
    let _: Value = client.post(&path, &serde_json::json!({})).await?;
    Ok(())
}

#[tauri::command]
pub async fn branch_rollback(app: AppHandle, branch: String) -> Result<(), String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/rollback", urlencode(&branch));
    let _: Value = client.post(&path, &serde_json::json!({})).await?;
    Ok(())
}

/// Compute the semantic diff between main and `branch`. Wire shape
/// is `thinkingroot_core::types::diff::KnowledgeDiff`. Used by the
/// Substrate Console's BeliefDiffPanel — the "compare to baseline"
/// command that surfaces divergent claims as conflict cards.
#[tauri::command]
pub async fn branch_diff(app: AppHandle, branch: String) -> Result<Value, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/diff", urlencode(&branch));
    let data: Value = client.get(&path).await?;
    Ok(data)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ─── Live aggregate branch-event subscription ────────────────────────
//
// Wire path:
//
//   daemon `/branch-events/stream` (SSE, broadcast hub)
//     → reqwest stream
//     → eventsource-stream parser
//     → Tauri channel `branch-event` (UI subscribes via @tauri-apps/api/event)
//
// Singleton per process: every UI surface that wants live branch
// updates (BranchTree, branch chip in ChatView, BeliefDiffPanel)
// shares this one SSE connection. `branch_event_subscribe` is
// idempotent — calling it twice is a no-op when a subscriber is
// already running.

/// Wire shape forwarded to the UI on every aggregate event. Mirrors
/// the daemon's `data:` payload from `stream_all_branch_events_handler`
/// (engine `rest.rs:2206`) plus a `kind` discriminator for "lagged"
/// notifications (when the broadcast channel buffers up faster than
/// the SSE relay drains). The UI's discriminated union mirrors this
/// shape directly.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchEventEnvelope {
    /// One branch event observed by the aggregate stream. `branch` is
    /// the branch name; `event` is the engine's `BranchEvent` JSON
    /// (passed through verbatim — wire shape owned by the daemon).
    Event {
        branch: String,
        event: Value,
    },
    /// Workspace HEAD moved (`POST /branches/{name}/checkout`). Not a
    /// `BranchEvent` on any branch; UIs must refetch `/head` and
    /// `/branches` (via `branch_list`).
    HeadChanged { head: String },
    /// The broadcast channel dropped messages because the relay fell
    /// behind. UI surfaces should refresh their state from a fresh
    /// `branch_list` call when they see this — the event log they
    /// observed is now stale.
    Lagged { missed: u64 },
    /// The SSE connection was lost / closed. Subscribers should treat
    /// the local view as stale and refresh.
    Disconnected { reason: String },
}

#[tauri::command]
pub async fn branch_event_subscribe(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut slot = state.branch_event_subscriber.lock().await;
    if let Some(existing) = slot.as_ref() {
        // Already subscribed; leave the running task alone. Token
        // liveness is the canonical signal — if the task died
        // (cancelled or stream closed), `existing.cancel.is_cancelled()`
        // is true and we replace it.
        if !existing.cancel.is_cancelled() {
            return Ok(());
        }
    }

    let client = SidecarClient::ensure_active(&app).await?;
    let url = format!("http://{}:{}/api/v1/branch-events/stream", client.host, client.port);
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let app_for_task = app.clone();
    tokio::spawn(async move {
        if let Err(e) = run_branch_event_subscription(app_for_task.clone(), url, cancel_for_task.clone()).await {
            tracing::warn!("branch-event subscriber exited with error: {e}");
            let _ = app_for_task.emit(
                "branch-event",
                BranchEventEnvelope::Disconnected { reason: e },
            );
        }
        cancel_for_task.cancel();
    });
    *slot = Some(BranchEventSubscriberHandle { cancel });
    Ok(())
}

#[tauri::command]
pub async fn branch_event_unsubscribe(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut slot = state.branch_event_subscriber.lock().await;
    if let Some(handle) = slot.take() {
        handle.cancel.cancel();
    }
    Ok(())
}

async fn run_branch_event_subscription(
    app: AppHandle,
    url: String,
    cancel: CancellationToken,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("http client init: {e}"))?;

    let resp = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        r = client.get(&url).header("accept", "text/event-stream").send() => r,
    }
    .map_err(|e| format!("connect to {url}: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("daemon returned {status}: {body}"));
    }

    let mut events = resp.bytes_stream().eventsource();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            item = events.next() => {
                let ev = match item {
                    Some(Ok(ev)) => ev,
                    Some(Err(e)) => return Err(format!("sse parse: {e}")),
                    None => return Ok(()), // upstream closed cleanly
                };
                match ev.event.as_str() {
                    "branch_event" => {
                        let json: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!("branch-event decode: {e}");
                                continue;
                            }
                        };
                        let branch = json
                            .get("branch")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let event = json.get("event").cloned().unwrap_or(Value::Null);
                        // A merge that lands on main mutates the graph
                        // BrainView reads. Re-emit on the
                        // `workspaces-changed` channel so the existing
                        // `onWorkspacesChanged` subscriber on the UI
                        // side refreshes the brain snapshot — no new
                        // event type required.
                        if merge_landed_on_main(&event) {
                            let _ = app.emit("workspaces-changed", true);
                        }
                        let _ = app.emit(
                            "branch-event",
                            BranchEventEnvelope::Event { branch, event },
                        );
                    }
                    "lagged" => {
                        let missed = serde_json::from_str::<Value>(&ev.data)
                            .ok()
                            .and_then(|v| v.get("missed").and_then(|m| m.as_u64()))
                            .unwrap_or(0);
                        let _ = app.emit(
                            "branch-event",
                            BranchEventEnvelope::Lagged { missed },
                        );
                    }
                    "head_changed" => {
                        let json: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!("head_changed decode: {e}");
                                continue;
                            }
                        };
                        let head = json
                            .get("head")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let _ = app.emit(
                            "branch-event",
                            BranchEventEnvelope::HeadChanged { head },
                        );
                    }
                    // Keep-alive comments + unknown event types are
                    // silently ignored.
                    _ => {}
                }
            }
        }
    }
}

/// True iff `event` is the JSON form of `BranchEvent::Merged` whose
/// `into` target is `main`. Extracted as a pure helper so the
/// branch-event subscriber's "should I also signal BrainView?"
/// decision is unit-testable without an `AppHandle` mock.
///
/// Wire shape from `thinkingroot_core::types::BranchEvent`'s
/// `#[serde(tag = "kind", rename_all = "snake_case")]`:
/// `{"kind": "merged", "at": "...", "actor": "...", "into": "..."}`.
fn merge_landed_on_main(event: &Value) -> bool {
    event.get("kind").and_then(|v| v.as_str()) == Some("merged")
        && event.get("into").and_then(|v| v.as_str()) == Some("main")
}

#[cfg(test)]
mod tests {
    use super::merge_landed_on_main;
    use serde_json::json;

    #[test]
    fn merged_into_main_returns_true() {
        let ev = json!({
            "kind": "merged",
            "at": "2026-05-21T00:00:00Z",
            "actor": "alice",
            "into": "main",
        });
        assert!(merge_landed_on_main(&ev));
    }

    #[test]
    fn merged_into_other_branch_returns_false() {
        let ev = json!({
            "kind": "merged",
            "at": "2026-05-21T00:00:00Z",
            "actor": "alice",
            "into": "feature/x",
        });
        assert!(!merge_landed_on_main(&ev));
    }

    #[test]
    fn non_merge_event_returns_false() {
        let created = json!({
            "kind": "created",
            "at": "2026-05-21T00:00:00Z",
            "actor": "alice",
            "parent": "main",
        });
        assert!(!merge_landed_on_main(&created));

        let abandoned = json!({
            "kind": "abandoned",
            "at": "2026-05-21T00:00:00Z",
            "actor": "alice",
        });
        assert!(!merge_landed_on_main(&abandoned));

        // Defensive: malformed event (missing fields) must not
        // accidentally trigger a brain refresh.
        let empty = json!({});
        assert!(!merge_landed_on_main(&empty));
    }
}
