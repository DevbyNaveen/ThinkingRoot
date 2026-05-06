//! Hybrid retrieve Tauri command — `retrieve_hybrid`. Mirrors
//! `POST /api/v1/ws/{ws}/search/hybrid` so the desktop's Brain view
//! and chat surfaces can run the full 11-component fused score query
//! without re-opening the engine in-process.

use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[tauri::command]
pub async fn retrieve_hybrid(
    app: AppHandle,
    query: String,
    top_k: Option<usize>,
    branch: Option<String>,
    profile: Option<String>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/search/hybrid", sc.workspace);
    let mut body = serde_json::json!({
        "query": query,
        "top_k": top_k.unwrap_or(20),
    });
    if let Some(b) = branch {
        body["branch"] = serde_json::json!(b);
    }
    if let Some(p) = profile {
        body["profile"] = serde_json::json!(p);
    }
    sc.post::<_, serde_json::Value>(&path, &body).await
}
