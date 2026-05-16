//! Cognition-commit Tauri commands — Phase β.3 bridge between the
//! React `CommitDAGView` and the sidecar's `cognition_commits` REST
//! surface shipped in β.2.
//!
//! Three thin wrappers, each routing through `SidecarClient` per the
//! Cortex Protocol single-writer rule. The wire-shape mirrors the
//! sidecar's `CognitionCommit` projection exactly; the UI keeps a
//! tolerant `serde_json::Value`-flavoured contract so a future field
//! addition on the sidecar doesn't force a desktop rebuild.

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

/// One commit row as it lands on the wire. Matches the sidecar's
/// `thinkingroot_core::types::CognitionCommit` projection plus the
/// optional fields surfaced via the REST envelope.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommitRow {
    pub id: String,
    #[serde(default)]
    pub parent: Option<String>,
    pub branch: String,
    /// Author shape — the sidecar emits a `{kind: "user"|"agent", id,
    /// model?, principal?}` discriminated union. We carry it through
    /// untyped so the React side can render the variants without a
    /// separate Tauri binding for `CommitAuthor`.
    pub author: serde_json::Value,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub witnesses_added: Vec<String>,
    #[serde(default)]
    pub citations: Vec<String>,
    #[serde(default)]
    pub gaps_surfaced: Vec<String>,
    pub created_at: String,
}

/// Tauri command: list cognition commits on a branch newest-first.
/// `branch` defaults to `main` server-side when `None` — keep the
/// argument optional here so the desktop UI can lazy-load the panel
/// without pre-fetching the active branch name.
#[tauri::command]
pub async fn commit_list(
    app: AppHandle,
    branch: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<CommitRow>, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let mut path = format!("/api/v1/ws/{}/commits", urlencode(&client.workspace));
    let mut sep = '?';
    if let Some(b) = branch.as_deref().filter(|s| !s.is_empty()) {
        path.push(sep);
        path.push_str("branch=");
        path.push_str(&urlencode(b));
        sep = '&';
    }
    if let Some(n) = limit {
        path.push(sep);
        path.push_str(&format!("limit={n}"));
    }
    let rows: Vec<CommitRow> = client.get(&path).await?;
    Ok(rows)
}

/// Tauri command: fetch a single commit by id. Returns the row on
/// success; the sidecar maps unknown ids to 404 which `SidecarClient`
/// surfaces as a typed `Err` — the desktop renders that as an empty
/// "this commit was pruned" state.
#[tauri::command]
pub async fn commit_get(app: AppHandle, id: String) -> Result<CommitRow, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/commits/{}",
        urlencode(&client.workspace),
        urlencode(&id),
    );
    let row: CommitRow = client.get(&path).await?;
    Ok(row)
}

/// Tauri command: record one commit. Mirrors the REST `RecordCommitRequest`
/// body shape; citations + parent are server-verified.
#[derive(Debug, Deserialize)]
pub struct CommitRecordArgs {
    pub branch: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub author_kind: String,
    pub author_id: String,
    #[serde(default)]
    pub author_model: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub witnesses_added: Vec<String>,
    #[serde(default)]
    pub citations: Vec<String>,
    #[serde(default)]
    pub gaps_surfaced: Vec<String>,
}

#[tauri::command]
pub async fn commit_record(
    app: AppHandle,
    args: CommitRecordArgs,
) -> Result<CommitRow, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/commits", urlencode(&client.workspace));
    let body = serde_json::json!({
        "branch": args.branch,
        "parent_id": args.parent_id,
        "author_kind": args.author_kind,
        "author_id": args.author_id,
        "author_model": args.author_model,
        "prompt": args.prompt,
        "reasoning": args.reasoning,
        "witnesses_added": args.witnesses_added,
        "citations": args.citations,
        "gaps_surfaced": args.gaps_surfaced,
    });
    let row: CommitRow = client.post(&path, &body).await?;
    Ok(row)
}

/// Phase γ.2 — Tauri command for LLM-driven merge synthesis.
/// Returns the full `MergeSynthesis` JSON shape; the React conflict-
/// resolution view (γ.3) renders the plan + synthesis side by side
/// and the user decides whether to commit it via `commit_record`.
#[tauri::command]
pub async fn commit_synthesize_merge(
    app: AppHandle,
    left_branch: String,
    right_branch: String,
) -> Result<serde_json::Value, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/commits/synthesize-merge",
        urlencode(&client.workspace),
    );
    let body = serde_json::json!({
        "left_branch": left_branch,
        "right_branch": right_branch,
    });
    let synthesis: serde_json::Value = client.post(&path, &body).await?;
    Ok(synthesis)
}

/// Phase γ.1 — Tauri command for the deterministic merge plan.
/// Mirrors the sidecar's `MergePlan` JSON shape directly via
/// `serde_json::Value` so a future field addition on the sidecar
/// doesn't force a desktop rebuild (matches the `CommitRow.author`
/// pattern). The React conflict-resolution view consumes this.
#[tauri::command]
pub async fn commit_merge_plan(
    app: AppHandle,
    left_branch: String,
    right_branch: String,
) -> Result<serde_json::Value, String> {
    let client = crate::commands::sidecar_client::SidecarClient::ensure_active(&app).await?;
    let path = format!(
        "/api/v1/ws/{}/commits/merge-plan?left={}&right={}",
        urlencode(&client.workspace),
        urlencode(&left_branch),
        urlencode(&right_branch),
    );
    let plan: serde_json::Value = client.get(&path).await?;
    Ok(plan)
}

/// Minimal URL encoder for path components — same shape as the helper
/// in playground.rs; duplicated locally to keep the commits module
/// free of cross-module imports.
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
