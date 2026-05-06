//! Branch slash commands — `/branch`, `/checkout`, `/merge`,
//! `/branches`.
//!
//! Stream A — every branch command now routes through the sidecar's
//! REST surface. `branch_merge` and `branch_delete` were previously
//! in-process graph mutations racing against the daemon; both now
//! route through `POST /api/v1/branches/{branch}/merge` and
//! `DELETE /api/v1/branches/{branch}` so the daemon stays the single
//! owner of `graph.db`. `branch_list`, `branch_create`, and
//! `branch_checkout` use the parallel REST endpoints; the daemon
//! reads `branches.toml` directly so these do not require the
//! workspace to be mounted, but they still go through HTTP for a
//! single source of truth.

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::commands::sidecar_client::SidecarClient;

#[derive(Debug, Serialize, Clone, Deserialize)]
pub struct BranchView {
    pub name: String,
    pub parent: String,
    pub status: String,
    pub current: bool,
    pub description: Option<String>,
}

/// Wire shape of `BranchRef` as serialized by `list_branches_handler`
/// (rest.rs:1318). Only the fields the desktop UI surfaces are decoded.
#[derive(Debug, Deserialize)]
struct BranchRefWire {
    name: String,
    parent: String,
    status: serde_json::Value,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BranchesResponse {
    branches: Vec<BranchRefWire>,
}

#[derive(Debug, Deserialize)]
struct HeadResponse {
    head: String,
}

fn status_label(value: &serde_json::Value) -> &'static str {
    if value == "active" {
        "active"
    } else if let Some(obj) = value.as_object() {
        if obj.contains_key("Merged") {
            "merged"
        } else if obj.contains_key("Abandoned") {
            "abandoned"
        } else {
            "active"
        }
    } else {
        "active"
    }
}

#[derive(Debug, Deserialize)]
pub struct BranchListArgs {
    pub workspace: String,
}

#[tauri::command]
pub async fn branch_list(
    app: AppHandle,
    args: BranchListArgs,
) -> Result<Vec<BranchView>, String> {
    let _ = args.workspace; // accepted for backward-compat; daemon resolves from workspace_root
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let head: HeadResponse = client.get("/api/v1/head").await?;
    let resp: BranchesResponse = client.get("/api/v1/branches").await?;
    Ok(resp
        .branches
        .into_iter()
        .map(|b| BranchView {
            current: b.name == head.head,
            parent: b.parent,
            status: status_label(&b.status).to_string(),
            description: b.description,
            name: b.name,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct BranchCreateArgs {
    pub workspace: String,
    pub name: String,
    pub parent: Option<String>,
    pub description: Option<String>,
}

#[tauri::command]
pub async fn branch_create(
    app: AppHandle,
    args: BranchCreateArgs,
) -> Result<BranchView, String> {
    let _ = args.workspace;
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let body = serde_json::json!({
        "name": args.name,
        "parent": args.parent,
        "description": args.description,
    });
    let created: BranchRefWire = client.post("/api/v1/branches", &body).await?;
    Ok(BranchView {
        current: false,
        parent: created.parent,
        status: status_label(&created.status).to_string(),
        description: created.description,
        name: created.name,
    })
}

#[derive(Debug, Deserialize)]
pub struct BranchCheckoutArgs {
    pub workspace: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct CheckoutResponse {
    head: String,
}

#[tauri::command]
pub async fn branch_checkout(
    app: AppHandle,
    args: BranchCheckoutArgs,
) -> Result<String, String> {
    let _ = args.workspace;
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/checkout", urlencode(&args.name));
    let resp: CheckoutResponse = client.post(&path, &serde_json::json!({})).await?;
    Ok(resp.head)
}

#[derive(Debug, Deserialize)]
pub struct BranchMergeArgs {
    pub workspace: String,
    pub name: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub propagate_deletions: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct MergeOutcome {
    pub merged: bool,
    pub new_claims: usize,
    pub auto_resolved: usize,
    pub conflicts: usize,
    pub blocking_reasons: Vec<String>,
}

/// Wire shape of the merge response — `merge_branch_handler` returns
/// the `KnowledgeDiff` shape directly. We only extract the counts the
/// desktop UI surfaces.
#[derive(Debug, Deserialize)]
struct MergeResponse {
    #[serde(default)]
    merge_allowed: bool,
    #[serde(default)]
    new_claims: serde_json::Value,
    #[serde(default)]
    auto_resolved: serde_json::Value,
    #[serde(default)]
    needs_review: serde_json::Value,
    #[serde(default)]
    blocking_reasons: Vec<String>,
}

fn count_or_len(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Array(a) => a.len(),
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0) as usize,
        _ => 0,
    }
}

#[tauri::command]
pub async fn branch_merge(
    app: AppHandle,
    args: BranchMergeArgs,
) -> Result<MergeOutcome, String> {
    let _ = args.workspace;
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}/merge", urlencode(&args.name));
    let body = serde_json::json!({
        "force": args.force,
        "propagate_deletions": args.propagate_deletions,
    });
    let resp: MergeResponse = client.post(&path, &body).await?;
    Ok(MergeOutcome {
        merged: resp.merge_allowed,
        new_claims: count_or_len(&resp.new_claims),
        auto_resolved: count_or_len(&resp.auto_resolved),
        conflicts: count_or_len(&resp.needs_review),
        blocking_reasons: resp.blocking_reasons,
    })
}

#[derive(Debug, Deserialize)]
pub struct BranchDeleteArgs {
    pub workspace: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct DeletedResponse {
    #[allow(dead_code)]
    deleted: String,
}

#[tauri::command]
pub async fn branch_delete(
    app: AppHandle,
    args: BranchDeleteArgs,
) -> Result<bool, String> {
    let _ = args.workspace;
    let client = SidecarClient::ensure_active_for_branches(&app).await?;
    let path = format!("/api/v1/branches/{}", urlencode(&args.name));
    let _: DeletedResponse = client.delete(&path).await?;
    Ok(true)
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
