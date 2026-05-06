//! Active Engram Protocol Tauri bindings — `engram_{materialize,list,
//! probe,expire}`.  All four route through `SidecarClient` with an
//! `X-TR-Session-Id` header (required by the daemon's AEP routes).
//!
//! The desktop caller is expected to manage its own session id (via
//! the chat session, etc.); the commands accept it as an explicit arg
//! rather than auto-minting because reusing the same id across calls
//! is the entire point of an engram session.

use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[tauri::command]
pub async fn engram_materialize(
    app: AppHandle,
    session_id: String,
    topic: String,
    seed_entity_ids: Option<Vec<String>>,
    scope: Option<String>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let mut body = serde_json::json!({ "topic": topic });
    if let Some(seeds) = seed_entity_ids
        && !seeds.is_empty()
    {
        body["seed_entity_ids"] = serde_json::json!(seeds);
    }
    if let Some(s) = scope {
        body["scope"] = serde_json::json!(s);
    }
    let path = format!("/api/v1/ws/{}/engrams", sc.workspace);
    sc.post_with_session::<_, serde_json::Value>(&path, &session_id, &body)
        .await
}

#[tauri::command]
pub async fn engram_list(
    app: AppHandle,
    session_id: String,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/engrams", sc.workspace);
    sc.get_with_session::<serde_json::Value>(&path, &session_id)
        .await
}

#[tauri::command]
pub async fn engram_probe(
    app: AppHandle,
    session_id: String,
    pointer: String,
    question: String,
    clearance: Option<Vec<String>>,
    probe_kind: Option<String>,
    score_with_hybrid: Option<bool>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let mut body = serde_json::json!({ "question": question });
    if let Some(c) = clearance
        && !c.is_empty()
    {
        body["clearance"] = serde_json::json!(c);
    }
    if let Some(k) = probe_kind {
        body["probe_kind"] = serde_json::json!(k);
    }
    if score_with_hybrid.unwrap_or(false) {
        body["score_with_hybrid"] = serde_json::json!(true);
    }
    let path = format!("/api/v1/ws/{}/engrams/{pointer}/probe", sc.workspace);
    sc.post_with_session::<_, serde_json::Value>(&path, &session_id, &body)
        .await
}

#[tauri::command]
pub async fn engram_expire(
    app: AppHandle,
    session_id: String,
    pointer: String,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/engrams/{pointer}", sc.workspace);
    sc.delete_with_session::<serde_json::Value>(&path, &session_id)
        .await
}
