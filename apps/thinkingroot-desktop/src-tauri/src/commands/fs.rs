//! Filesystem commands — drives the VS Code-style file tree on the
//! Brain + Satellites surfaces.
//!
//! The frontend lazy-loads one directory at a time: it asks for the
//! immediate children of a path; each folder child carries a
//! `has_children` flag so the tree can render a chevron without a
//! recursive scan up front. Recursion would block on large workspaces
//! (a thinkingroot checkout has ~10k files); lazy-loading keeps the
//! initial paint instant and amortises the cost as the user explores.
//!
//! Hidden entries (names starting with `.`) are skipped except for
//! `.thinkingroot` itself — that one is the user's compiled artifact
//! directory and they almost always want it visible.
//!
//! ## Sandboxing
//!
//! Every path argument the webview can supply is checked against the
//! current [`thinkingroot_core::WorkspaceRegistry`] before any
//! `read_dir` call. The webview can only enumerate paths inside one
//! of the registered workspace roots; an XSS or a malicious `.tr`
//! file that triggered JS execution cannot exfiltrate the entire
//! disk's directory tree by passing `path = "/"` or `path = "../.."`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thinkingroot_core::WorkspaceRegistry;

#[derive(Debug, Serialize, Clone)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    pub kind: FsEntryKind,
    /// `true` if this entry is a directory that contains at least one
    /// non-hidden child. Lets the frontend show a chevron without a
    /// recursive scan.
    pub has_children: bool,
    /// File size in bytes. `None` for directories.
    pub size: Option<u64>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FsEntryKind {
    Directory,
    File,
    Symlink,
}

#[derive(Debug, Deserialize)]
pub struct FsListDirArgs {
    pub path: String,
}

/// One level of children for `path`. Errors are surfaced as strings
/// (not panics) so the webview can render them inline as tree nodes
/// without the whole surface unmounting.
///
/// **Sandbox**: `path` must canonically resolve inside one of the
/// registered workspace roots. An unbounded `path` argument from the
/// webview would otherwise enumerate the entire filesystem on a
/// successful XSS or a malicious file dropped on the app.
#[tauri::command]
pub fn fs_list_dir(args: FsListDirArgs) -> Result<Vec<FsEntry>, String> {
    let path = ensure_under_registered_workspace(&args.path)?;
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()));
    }
    let mut entries: Vec<FsEntry> = Vec::new();
    let read = std::fs::read_dir(&path)
        .map_err(|e| format!("read_dir({}): {e}", path.display()))?;
    for dent in read {
        let dent = match dent {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("skipping unreadable entry in {}: {e}", path.display());
                continue;
            }
        };
        let name = dent.file_name().to_string_lossy().to_string();
        if should_skip(&name) {
            continue;
        }
        let metadata = match dent.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("skipping {} (metadata): {e}", name);
                continue;
            }
        };
        let kind = if metadata.file_type().is_symlink() {
            FsEntryKind::Symlink
        } else if metadata.is_dir() {
            FsEntryKind::Directory
        } else {
            FsEntryKind::File
        };
        let entry_path = dent.path();
        let has_children = if kind == FsEntryKind::Directory {
            dir_has_visible_children(&entry_path)
        } else {
            false
        };
        let size = if kind == FsEntryKind::File {
            Some(metadata.len())
        } else {
            None
        };
        entries.push(FsEntry {
            name,
            path: entry_path.to_string_lossy().to_string(),
            kind,
            has_children,
            size,
        });
    }
    // Folders first, then files; alphabetical within each group —
    // matches the VS Code default and what humans expect from a tree.
    entries.sort_by(|a, b| match (a.kind, b.kind) {
        (FsEntryKind::Directory, FsEntryKind::File)
        | (FsEntryKind::Directory, FsEntryKind::Symlink) => std::cmp::Ordering::Less,
        (FsEntryKind::File, FsEntryKind::Directory)
        | (FsEntryKind::Symlink, FsEntryKind::Directory) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(entries)
}

/// Hide dotfiles by default — but keep `.thinkingroot` visible because
/// that's the compiled-artifact directory the user wants to see for any
/// satellite folder. Add other always-visible exceptions here.
fn should_skip(name: &str) -> bool {
    if !name.starts_with('.') {
        return false;
    }
    !matches!(name, ".thinkingroot")
}

/// Canonicalise `raw` and assert it falls inside one of the registered
/// workspace roots. Returns the canonical path on success; an error
/// string suitable for the Tauri command result on rejection.
fn ensure_under_registered_workspace(raw: &str) -> Result<PathBuf, String> {
    let registry = WorkspaceRegistry::load()
        .map_err(|e| format!("load workspace registry: {e}"))?;
    if registry.workspaces.is_empty() {
        return Err(
            "no workspace registered yet — set one up before browsing the filesystem".into(),
        );
    }

    let raw_path = PathBuf::from(raw);
    let canonical = match raw_path.canonicalize() {
        Ok(c) => c,
        Err(e) => {
            return Err(format!("resolve {}: {e}", raw_path.display()));
        }
    };

    for ws in &registry.workspaces {
        let root = match ws.path.canonicalize() {
            Ok(c) => c,
            Err(_) => ws.path.clone(),
        };
        if canonical.starts_with(&root) {
            return Ok(canonical);
        }
    }

    Err(format!(
        "path {} is outside every registered workspace — refusing to enumerate",
        canonical.display()
    ))
}

fn dir_has_visible_children(path: &Path) -> bool {
    let Ok(read) = std::fs::read_dir(path) else {
        return false;
    };
    for dent in read.flatten() {
        let name = dent.file_name().to_string_lossy().to_string();
        if !should_skip(&name) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_skip_keeps_thinkingroot_dir() {
        assert!(!should_skip(".thinkingroot"));
    }

    #[test]
    fn should_skip_hides_other_dotfiles() {
        assert!(should_skip(".git"));
        assert!(should_skip(".DS_Store"));
        assert!(should_skip(".env"));
    }

    #[test]
    fn should_skip_keeps_normal_files() {
        assert!(!should_skip("README.md"));
        assert!(!should_skip("src"));
    }
}
