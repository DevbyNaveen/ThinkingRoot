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

#[derive(Debug, Deserialize)]
pub struct FsReadTextArgs {
    pub path: String,
}

/// Preview payload for small text reads in the workspace file inspector.
#[derive(Debug, Serialize)]
pub struct FsReadTextBody {
    /// Lossy-decoded UTF-8 (invalid bytes replaced).
    pub content: String,
    pub had_invalid_utf8: bool,
    pub size: u64,
}

/// Maximum file size for `fs_read_text` previews (512 KiB).
const MAX_TEXT_FILE_BYTES: u64 = 512 * 1024;

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
    if !path.exists() {
        return Err(format!("path does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()));
    }
    let mut entries: Vec<FsEntry> = Vec::new();
    let read =
        std::fs::read_dir(&path).map_err(|e| format!("read_dir({}): {e}", path.display()))?;
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

/// Read a file for the workspace inspector preview. Same sandbox as
/// [`fs_list_dir`]. Caps size at [`MAX_TEXT_FILE_BYTES`]; binary content
/// is returned as lossy UTF-8 with `had_invalid_utf8 = true`.
#[tauri::command]
pub fn fs_read_text(args: FsReadTextArgs) -> Result<FsReadTextBody, String> {
    let path = ensure_under_registered_workspace(&args.path)?;
    if !path.is_file() {
        return Err(format!(
            "not a regular file (or missing): {}",
            path.display()
        ));
    }
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    let len = meta.len();
    if len > MAX_TEXT_FILE_BYTES {
        return Err(format!(
            "file is {} bytes (max preview {} KiB)",
            len,
            MAX_TEXT_FILE_BYTES / 1024
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let had_invalid_utf8 = std::str::from_utf8(&bytes).is_err();
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Ok(FsReadTextBody {
        content,
        had_invalid_utf8,
        size: len,
    })
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

/// Resolve `raw` to a path that must fall inside one of the registered
/// workspace roots. Existing paths are canonicalised; non-existent
/// logical children (e.g. `…/workspace/.thinkingroot` before first compile)
/// are accepted by component-wise prefix match against each registered
/// root — still bounded to the workspace; `..` segments in the tail are
/// rejected.
fn ensure_under_registered_workspace(raw: &str) -> Result<PathBuf, String> {
    let registry =
        WorkspaceRegistry::load().map_err(|e| format!("load workspace registry: {e}"))?;
    if registry.workspaces.is_empty() {
        return Err(
            "no workspace registered yet — set one up before browsing the filesystem".into(),
        );
    }

    let raw_path = PathBuf::from(raw);

    if let Ok(canonical) = raw_path.canonicalize() {
        for ws in &registry.workspaces {
            let root = match ws.path.canonicalize() {
                Ok(c) => c,
                Err(_) => ws.path.clone(),
            };
            if canonical.starts_with(&root) {
                return Ok(canonical);
            }
        }
        return Err(format!(
            "path {} is outside every registered workspace — refusing to enumerate",
            canonical.display()
        ));
    }

    // Path does not exist on disk yet — allow `root/child/...` as long as
    // `root` matches a registered workspace path by components.
    for ws in &registry.workspaces {
        let root = ws.path.canonicalize().unwrap_or_else(|_| ws.path.clone());
        if let Some(rel) = strip_workspace_suffix(&raw_path, &root) {
            if rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                continue;
            }
            return Ok(root.join(rel));
        }
    }

    Err(format!(
        "path {} is outside every registered workspace — refusing to enumerate",
        raw_path.display()
    ))
}

/// If `path` is exactly `root` or a strict child (by path components),
/// return the relative tail (possibly empty). Otherwise `None`.
fn strip_workspace_suffix(path: &Path, root: &Path) -> Option<PathBuf> {
    let mut pit = path.components();
    for rc in root.components() {
        match pit.next() {
            Some(p) if p == rc => {}
            _ => return None,
        }
    }
    Some(pit.as_path().to_path_buf())
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

    #[test]
    fn strip_suffix_child() {
        use std::path::Path;
        let root = Path::new("/project/ws");
        let child = Path::new("/project/ws/.thinkingroot/cache");
        assert_eq!(
            super::strip_workspace_suffix(child, root),
            Some(Path::new(".thinkingroot/cache").to_path_buf())
        );
    }

    #[test]
    fn strip_suffix_other_branch() {
        use std::path::Path;
        let root = Path::new("/a/b");
        let sibling = Path::new("/a/c/x");
        assert!(super::strip_workspace_suffix(sibling, root).is_none());
    }
}
