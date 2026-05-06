//! Stream D — Tauri commands for branch extras (events, stats,
//! lineage, rebase, rollback). All routes already exist on the daemon
//! side; these are thin sidecar bindings.

use serde::Serialize;
use serde_json::Value;
use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

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
pub async fn branch_stats(
    app: AppHandle,
    branch: String,
) -> Result<BranchStatsView, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/stats", urlencode(&branch));
    let data: Value = client.get(&path).await?;
    Ok(BranchStatsView {
        branch: data
            .get("branch")
            .and_then(|v| v.as_str())
            .unwrap_or(&branch)
            .to_string(),
        claim_count: data.get("claim_count").and_then(|v| v.as_u64()).unwrap_or(0),
        entity_count: data
            .get("entity_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        source_count: data
            .get("source_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        event_count: data.get("event_count").and_then(|v| v.as_u64()).unwrap_or(0),
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

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
