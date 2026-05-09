//! Stream D — Tauri commands for Knowledge Proposal (T0.4) operations.
//!
//! Each command routes through the `SidecarClient` so the daemon stays
//! the single source of truth. Frontend wrappers live in
//! `ui/src/lib/tauri.ts`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[derive(Debug, Deserialize)]
pub struct ProposalOpenArgs {
    pub branch: String,
    #[serde(default = "default_target")]
    pub target: String,
    pub description: Option<String>,
    pub min_reviewers: Option<u8>,
}

fn default_target() -> String {
    "main".to_string()
}

#[derive(Debug, Serialize, Clone)]
pub struct ProposalView {
    pub id: String,
    pub source_branch: String,
    pub target_branch: String,
    pub status: String,
}

fn proposal_view(v: &Value) -> ProposalView {
    ProposalView {
        id: v
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        source_branch: v
            .get("source_branch")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        target_branch: v
            .get("target_branch")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        status: v
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

#[tauri::command]
pub async fn proposal_open(app: AppHandle, args: ProposalOpenArgs) -> Result<ProposalView, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/proposals", urlencode(&args.branch));
    let body = serde_json::json!({
        "target": args.target,
        "description": args.description,
        "min_reviewers": args.min_reviewers,
    });
    let data: Value = client.post(&path, &body).await?;
    let proposal = data
        .get("proposal")
        .ok_or_else(|| "missing proposal field".to_string())?;
    Ok(proposal_view(proposal))
}

#[tauri::command]
pub async fn proposal_list(
    app: AppHandle,
    branch: Option<String>,
) -> Result<Vec<ProposalView>, String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = match branch.as_deref() {
        Some(b) if !b.is_empty() => {
            format!("/api/v1/branches/{}/proposals", urlencode(b))
        }
        _ => "/api/v1/proposals".to_string(),
    };
    let data: Value = client.get(&path).await?;
    let proposals = data
        .get("proposals")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(proposals.iter().map(proposal_view).collect())
}

#[derive(Debug, Deserialize)]
pub struct ProposalReviewArgs {
    pub id: String,
    /// One of `approve`, `request_changes`, `comment`.
    pub decision: String,
    pub note: Option<String>,
}

#[tauri::command]
pub async fn proposal_review(app: AppHandle, args: ProposalReviewArgs) -> Result<(), String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/proposals/{}/reviews", urlencode(&args.id));
    let body = serde_json::json!({
        "decision": args.decision,
        "note": args.note,
    });
    let _: Value = client.post(&path, &body).await?;
    Ok(())
}

#[tauri::command]
pub async fn proposal_close(app: AppHandle, id: String) -> Result<(), String> {
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/proposals/{}/close", urlencode(&id));
    let _: Value = client.post(&path, &serde_json::json!({})).await?;
    Ok(())
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
