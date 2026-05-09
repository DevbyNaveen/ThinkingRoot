//! T3.7 branch templates — `branch_template_{list,get,upsert,delete,apply}`.

use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[tauri::command]
pub async fn branch_template_list(app: AppHandle) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active_for_branches(&app).await?;
    sc.get::<serde_json::Value>("/api/v1/branch-templates")
        .await
}

#[tauri::command]
pub async fn branch_template_get(
    app: AppHandle,
    name: String,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branch-templates/{name}");
    sc.get::<serde_json::Value>(&path).await
}

#[tauri::command]
pub async fn branch_template_upsert(
    app: AppHandle,
    template: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active_for_branches(&app).await?;
    sc.post::<_, serde_json::Value>("/api/v1/branch-templates", &template)
        .await
}

#[tauri::command]
pub async fn branch_template_delete(
    app: AppHandle,
    name: String,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branch-templates/{name}");
    sc.delete::<serde_json::Value>(&path).await
}

/// Materialise a new branch from a template. Goes through the standard
/// `POST /api/v1/branches` route with `template` set; the daemon
/// applies the template's defaults to any unset field.
#[tauri::command]
pub async fn branch_template_apply(
    app: AppHandle,
    template: String,
    branch: String,
    description: Option<String>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active_for_branches(&app).await?;
    let body = serde_json::json!({
        "name": branch,
        "template": template,
        "description": description,
    });
    sc.post::<_, serde_json::Value>("/api/v1/branches", &body)
        .await
}
