//! Stream D — Tauri commands for T2.5 immutable snapshot tags.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[derive(Debug, Serialize, Clone)]
pub struct TagView {
    pub name: String,
    pub target_commit_hash: String,
    pub message: Option<String>,
    pub created_at: Option<String>,
}

fn tag_view(v: &Value) -> TagView {
    TagView {
        name: v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        target_commit_hash: v
            .get("target_commit_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        message: v
            .get("message")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        created_at: v
            .get("created_at")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct TagCreateArgs {
    pub name: String,
    pub branch: String,
    pub message: Option<String>,
}

#[tauri::command]
pub async fn tag_create(app: AppHandle, args: TagCreateArgs) -> Result<TagView, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let body = serde_json::json!({
        "name": args.name,
        "branch": args.branch,
        "message": args.message,
    });
    let data: Value = client.post("/api/v1/tags", &body).await?;
    Ok(tag_view(&data))
}

#[tauri::command]
pub async fn tag_list(app: AppHandle) -> Result<Vec<TagView>, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let data: Value = client.get("/api/v1/tags").await?;
    let tags = data
        .get("tags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(tags.iter().map(tag_view).collect())
}

#[tauri::command]
pub async fn tag_get(app: AppHandle, name: String) -> Result<TagView, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/tags/{}", urlencode(&name));
    let data: Value = client.get(&path).await?;
    Ok(tag_view(&data))
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
