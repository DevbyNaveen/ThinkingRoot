// crates/thinkingroot-branch/src/lib.rs
pub mod branch;
pub mod diff;
pub mod merge;
pub mod snapshot;

use std::path::Path;
use thinkingroot_core::{BranchRef, Result};

/// Create a new knowledge branch from a parent branch (default: main).
///
/// - Copies `{parent_data_dir}/graph/graph.db` to the new branch dir
/// - Symlinks `models/` and `cache/` from parent (avoids duplicating ~300MB)
/// - Registers the branch in `.thinkingroot-refs/branches.toml`
pub async fn create_branch(
    root_path: &Path,
    name: &str,
    parent: &str,
    description: Option<String>,
) -> Result<BranchRef> {
    let parent_data_dir = snapshot::resolve_data_dir(root_path, Some(parent));
    let branch_data_dir = snapshot::resolve_data_dir(root_path, Some(name));

    snapshot::create_branch_layout(&parent_data_dir, &branch_data_dir)?;

    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    registry.create_branch(name, parent, description)
}

/// List all active branches for a workspace.
pub fn list_branches(root_path: &Path) -> Result<Vec<BranchRef>> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    if !refs_dir.exists() {
        return Ok(vec![]);
    }
    let registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    Ok(registry.list_active().into_iter().cloned().collect())
}

/// Read the active HEAD branch name. Returns "main" if no HEAD exists.
pub fn read_head_branch(root_path: &Path) -> Result<String> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    branch::read_head(&refs_dir)
}

/// Write the active HEAD branch name.
pub fn write_head_branch(root_path: &Path, branch_name: &str) -> Result<()> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    branch::write_head(&refs_dir, branch_name)
}

/// Soft-delete a branch (mark as Abandoned, data dir kept).
pub fn delete_branch(root_path: &Path, name: &str) -> Result<()> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    registry.abandon_branch(name)
}
