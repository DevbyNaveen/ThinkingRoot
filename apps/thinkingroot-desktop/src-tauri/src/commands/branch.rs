//! Branch slash commands — `/branch`, `/checkout`, `/merge`,
//! `/branches`.
//!
//! These are thin wrappers around `thinkingroot_branch` (for the
//! plumbing that doesn't touch the merged graph) and the mounted
//! [`QueryEngine`] (for `merge_branch` / `delete_branch`, which need
//! the graph cache invalidated atomically). Following the
//! `thinkingroot-cli` convention, branch names are validated by the
//! `BranchRegistry` itself — we don't pre-validate here.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use thinkingroot_core::WorkspaceRegistry;

#[derive(Debug, Serialize, Clone)]
pub struct BranchView {
    pub name: String,
    pub parent: String,
    pub status: String,
    pub current: bool,
    pub description: Option<String>,
}

fn status_label(status: &thinkingroot_core::BranchStatus) -> &'static str {
    use thinkingroot_core::BranchStatus;
    match status {
        BranchStatus::Active => "active",
        BranchStatus::Merged { .. } => "merged",
        BranchStatus::Abandoned { .. } => "abandoned",
    }
}

#[derive(Debug, Deserialize)]
pub struct BranchListArgs {
    pub workspace: String,
}

#[tauri::command]
pub fn branch_list(args: BranchListArgs) -> Result<Vec<BranchView>, String> {
    let path = workspace_path(&args.workspace)?;
    let head = thinkingroot_branch::read_head_branch(&path).unwrap_or_else(|_| "main".to_string());
    let branches = thinkingroot_branch::list_branches(&path).map_err(|e| e.to_string())?;
    Ok(branches
        .into_iter()
        .map(|b| BranchView {
            current: b.name == head,
            parent: b.parent.clone(),
            status: status_label(&b.status).to_string(),
            description: b.description.clone(),
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
pub async fn branch_create(args: BranchCreateArgs) -> Result<BranchView, String> {
    let path = workspace_path(&args.workspace)?;
    let parent = args.parent.unwrap_or_else(|| "main".to_string());
    let branch =
        thinkingroot_branch::create_branch(&path, &args.name, &parent, args.description)
            .await
            .map_err(|e| e.to_string())?;
    Ok(BranchView {
        current: false,
        parent: branch.parent.clone(),
        status: status_label(&branch.status).to_string(),
        description: branch.description.clone(),
        name: branch.name,
    })
}

#[derive(Debug, Deserialize)]
pub struct BranchCheckoutArgs {
    pub workspace: String,
    pub name: String,
}

#[tauri::command]
pub fn branch_checkout(args: BranchCheckoutArgs) -> Result<String, String> {
    let path = workspace_path(&args.workspace)?;
    thinkingroot_branch::write_head_branch(&path, &args.name).map_err(|e| e.to_string())?;
    Ok(args.name)
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

#[tauri::command]
pub async fn branch_merge(
    app: AppHandle,
    args: BranchMergeArgs,
) -> Result<MergeOutcome, String> {
    use crate::state::AppState;
    use tauri::Manager;
    let path = workspace_path(&args.workspace)?;
    let state = app.state::<AppState>();
    let engine = {
        let guard = state.memory.lock().await;
        let Some(mounted) = guard.as_ref() else {
            return Err(
                "no workspace mounted — open the workspace in Brain first".to_string(),
            );
        };
        mounted.engine.clone()
    };
    let engine = engine.read().await;
    let merged_by = thinkingroot_core::MergedBy::Human {
        user: std::env::var("USER").unwrap_or_else(|_| "desktop".to_string()),
    };
    let diff = engine
        .merge_branch(
            &path,
            &args.name,
            args.force,
            args.propagate_deletions,
            merged_by,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(MergeOutcome {
        merged: diff.merge_allowed,
        new_claims: diff.new_claims.len(),
        auto_resolved: diff.auto_resolved.len(),
        conflicts: diff.needs_review.len(),
        blocking_reasons: diff.blocking_reasons,
    })
}

#[derive(Debug, Deserialize)]
pub struct BranchDeleteArgs {
    pub workspace: String,
    pub name: String,
}

#[tauri::command]
pub async fn branch_delete(
    app: AppHandle,
    args: BranchDeleteArgs,
) -> Result<bool, String> {
    use crate::state::AppState;
    use tauri::Manager;
    let path = workspace_path(&args.workspace)?;
    let state = app.state::<AppState>();
    let engine_opt = {
        let guard = state.memory.lock().await;
        guard.as_ref().map(|m| m.engine.clone())
    };
    if let Some(engine) = engine_opt {
        let engine = engine.read().await;
        engine
            .delete_branch(&path, &args.name)
            .await
            .map_err(|e| e.to_string())?;
    } else {
        thinkingroot_branch::delete_branch(&path, &args.name).map_err(|e| e.to_string())?;
    }
    Ok(true)
}

fn workspace_path(name: &str) -> Result<PathBuf, String> {
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    registry
        .workspaces
        .iter()
        .find(|w| w.name == name)
        .map(|w| w.path.clone())
        .ok_or_else(|| format!("workspace `{name}` not found"))
}
