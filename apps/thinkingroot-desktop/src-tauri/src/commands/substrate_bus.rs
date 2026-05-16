//! Substrate Bus Tauri commands — Phase δ.2 + δ.4.
//!
//! Bridges the React tray-feed UI to the sidecar's per-workspace
//! `SubAgentScheduler`. The wire shape mirrors the sidecar's
//! `SubAgentReport` JSON exactly; the desktop carries it untyped via
//! `serde_json::Value` so a future field addition on the sidecar
//! doesn't force a desktop rebuild — same pattern as `CommitRow`.

use tauri::AppHandle;

/// Start the substrate bus for the active workspace. Idempotent.
/// Returns the registered-agent name list so the UI can render
/// "Reconciler / Gap-hunter / Curator / Watcher" chips immediately.
#[tauri::command]
pub async fn substrate_bus_start(app: AppHandle) -> Result<serde_json::Value, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/substrate-bus/start",
        urlencode(&client.workspace),
    );
    let resp: serde_json::Value = client.post(&path, &serde_json::Value::Null).await?;
    Ok(resp)
}

/// Stop the substrate bus for the active workspace. Idempotent.
#[tauri::command]
pub async fn substrate_bus_stop(app: AppHandle) -> Result<serde_json::Value, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/substrate-bus/stop",
        urlencode(&client.workspace),
    );
    let resp: serde_json::Value = client.post(&path, &serde_json::Value::Null).await?;
    Ok(resp)
}

/// Snapshot of recent reports across every registered agent.
/// Newest-first within each agent slice; slices are concatenated in
/// agent-name ASCII order (stable across calls when the substrate
/// is quiet).
#[tauri::command]
pub async fn substrate_bus_reports(
    app: AppHandle,
) -> Result<Vec<serde_json::Value>, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/substrate-bus/reports",
        urlencode(&client.workspace),
    );
    let resp: Vec<serde_json::Value> = client.get(&path).await?;
    Ok(resp)
}

/// Manually trigger one tick of a registered agent. The user's
/// "Run now" affordance on the tray feed.
#[tauri::command]
pub async fn substrate_bus_run_now(
    app: AppHandle,
    agent: String,
) -> Result<serde_json::Value, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/substrate-bus/run-now",
        urlencode(&client.workspace),
    );
    let body = serde_json::json!({ "agent": agent });
    let resp: serde_json::Value = client.post(&path, &body).await?;
    Ok(resp)
}

/// Minimal URL encoder for path components — same shape as the
/// helper in commits.rs.
fn urlencode(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}
