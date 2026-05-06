//! Brain probe Tauri commands — `brain_brief` and `brain_investigate`.
//! Mirrors the new `POST /api/v1/ws/{ws}/brain/{brief,investigate}`
//! REST routes. Both go through `SidecarClient` so the desktop never
//! opens `graph.db` directly (Cortex Protocol invariant).

use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[tauri::command]
pub async fn brain_brief(
    app: AppHandle,
    branch: Option<String>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/brain/brief", sc.workspace);
    let body = serde_json::json!({ "branch": branch });
    sc.post::<_, serde_json::Value>(&path, &body).await
}

#[tauri::command]
pub async fn brain_investigate(
    app: AppHandle,
    entity: String,
    branch: Option<String>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/brain/investigate", sc.workspace);
    let body = serde_json::json!({ "entity": entity, "branch": branch });
    sc.post::<_, serde_json::Value>(&path, &body).await
}
