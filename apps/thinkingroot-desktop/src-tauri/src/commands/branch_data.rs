//! T0.7 connector contribute-bulk + T2.6 redaction policy — Tauri
//! commands `branch_contribute_bulk` and `branch_redaction_set`.

use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[tauri::command]
pub async fn branch_contribute_bulk(
    app: AppHandle,
    branch: String,
    connector_id: String,
    install_id: String,
    idempotency_key: String,
    session_id: Option<String>,
    backfill: Option<bool>,
    claims: Vec<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active_for_branches(&app).await?;
    let body = serde_json::json!({
        "workspace": sc.workspace,
        "session_id": session_id,
        "connector_id": connector_id,
        "install_id": install_id,
        "idempotency_key": idempotency_key,
        "backfill": backfill.unwrap_or(false),
        "claims": claims,
    });
    let path = format!("/api/v1/branches/{branch}/contribute-bulk");
    sc.post::<_, serde_json::Value>(&path, &body).await
}

/// `policy` accepts `null` to clear the policy or a `RedactionPolicy`-shaped
/// object to set one.
#[tauri::command]
pub async fn branch_redaction_set(
    app: AppHandle,
    branch: String,
    policy: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let sc = SidecarClient::ensure_active_for_branches(&app).await?;
    let body = serde_json::json!({ "policy": policy });
    let path = format!("/api/v1/branches/{branch}/redaction");
    sc.post::<_, serde_json::Value>(&path, &body).await
}
