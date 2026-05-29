//! Workspace-scoped filesystem operations — list / create-folder /
//! rename / move. Lifted from `apps/thinkingroot-desktop/src-tauri/
//! src/commands/playground_fs.rs` so the same primitives drive the
//! desktop FileManager UI, the REST `/ws/{ws}/fs/*` endpoints, and
//! the MCP `move_path` / `rename_path` / `create_folder` /
//! `list_directory` tools.
//!
//! Safety: every operation is scoped to a workspace root. The caller
//! resolves `workspace -> PathBuf` via `QueryEngine::workspace_root_path`
//! and passes it in; we never trust an unrooted path. `safe_path_within`
//! refuses non-`Normal` path components (`..`, absolute, device prefix)
//! AND canonicalises both root + candidate to defend against symlink
//! traversal.
//!
//! Honesty: collisions are surfaced as `skipped_conflict` counts —
//! silent overwrite is the kind of "helpful" that loses work.
//!
//! `.thinkingroot/` is hidden from listings + refused as a rename
//! target so the user can't accidentally munge engine-managed state.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// One entry in a directory listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    /// Path relative to the workspace root, forward-slash separated
    /// regardless of OS.
    pub rel_path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    /// Unix seconds since epoch, as `f64` for clean JSON encoding.
    pub modified: f64,
    /// Coarse content classification by extension. The Witness Mesh
    /// extractors do their own MIME detection so a misclassification
    /// here is purely cosmetic.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirListing {
    pub workspace: String,
    /// "" for workspace root, else a forward-slash rel path.
    pub rel_path: String,
    /// `None` at workspace root, else the parent rel_path.
    pub parent_rel_path: Option<String>,
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveOutcome {
    pub moved: u64,
    pub skipped_conflict: u64,
    pub skipped_invalid: u64,
    pub moved_rel_paths: Vec<String>,
}

/// List `<root>/<rel>` contents. Hides the `.thinkingroot/` directory.
pub fn list_directory(root: &Path, workspace: &str, rel: &str) -> Result<DirListing, String> {
    let target = safe_path_within(root, rel)?;
    if !target.is_dir() {
        return Err(format!("`{rel}` is not a directory"));
    }
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace root: {e}"))?;

    let mut entries: Vec<DirEntry> = Vec::new();
    for raw in fs::read_dir(&target).map_err(|e| format!("read_dir `{rel}`: {e}"))? {
        let entry = match raw {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hide engine-managed state from listings.
        if name == ".thinkingroot" {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let abs = entry.path();
        entries.push(DirEntry {
            name: name.clone(),
            rel_path: rel_to_workspace(&abs, &canon_root),
            is_dir: meta.is_dir(),
            size_bytes: meta.len(),
            modified: modified_secs(&meta),
            kind: classify_kind(&name).to_string(),
        });
    }

    // Stable order: dirs first, then files, both name-sorted.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    let parent_rel_path = if rel.is_empty() {
        None
    } else {
        let trimmed = rel.trim_matches('/');
        let parent = trimmed.rsplit_once('/').map(|(p, _)| p.to_string());
        // Top-level item → parent is workspace root (rel "")
        Some(parent.unwrap_or_default())
    };

    Ok(DirListing {
        workspace: workspace.to_string(),
        rel_path: rel.to_string(),
        parent_rel_path,
        entries,
    })
}

/// Read a UTF-8 text file within the workspace (size-capped). Read-only, so —
/// unlike rename/move, which protect engine state from mutation — it may read
/// engine-managed `.thinkingroot/*` files (e.g. flow-run status JSON). Path is
/// escape-guarded by `safe_path_within`.
pub fn read_file(root: &Path, rel: &str) -> Result<String, String> {
    const MAX_BYTES: u64 = 1024 * 1024; // 1 MiB cap
    let target = safe_path_within(root, rel)?;
    if !target.is_file() {
        return Err(format!("`{rel}` is not a file"));
    }
    let len = fs::metadata(&target)
        .map_err(|e| format!("stat `{rel}`: {e}"))?
        .len();
    if len > MAX_BYTES {
        return Err(format!("`{rel}` is too large to read ({len} bytes)"));
    }
    fs::read_to_string(&target).map_err(|e| format!("read `{rel}`: {e}"))
}

/// Create `<root>/<parent_rel>/<name>` as a new directory. Returns the
/// new rel_path on success.
pub fn create_folder(root: &Path, parent_rel: &str, name: &str) -> Result<String, String> {
    validate_leaf_name(name, "folder name")?;
    let target_rel = if parent_rel.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", parent_rel.trim_matches('/'), name)
    };
    let target = safe_path_within(root, &target_rel)?;
    if target.exists() {
        return Err(format!("`{target_rel}` already exists"));
    }
    fs::create_dir(&target).map_err(|e| format!("create_dir `{}`: {e}", target.display()))?;
    Ok(target_rel)
}

/// Rename `<root>/<rel>` to `<parent>/<new_name>`. Returns the new
/// rel_path.
pub fn rename_path(root: &Path, rel: &str, new_name: &str) -> Result<String, String> {
    validate_leaf_name(new_name, "name")?;
    if rel.is_empty() {
        return Err("cannot rename the workspace root".into());
    }
    if rel == ".thinkingroot" || rel.starts_with(".thinkingroot/") {
        return Err("refusing to rename engine-managed `.thinkingroot/` state".into());
    }
    let source = safe_path_within(root, rel)?;
    if !source.exists() {
        return Err(format!("`{rel}` does not exist"));
    }
    let parent = source
        .parent()
        .ok_or_else(|| "source has no parent".to_string())?;
    let dest = parent.join(new_name);
    if dest.exists() {
        return Err(format!("destination `{new_name}` already exists"));
    }
    fs::rename(&source, &dest)
        .map_err(|e| format!("rename `{}` → `{}`: {e}", source.display(), dest.display()))?;
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace root: {e}"))?;
    Ok(rel_to_workspace(&dest, &canon_root))
}

/// Move one or more items into a destination folder. Skips collisions
/// honestly (counted as `skipped_conflict`); never silently overwrites.
pub fn move_paths(
    root: &Path,
    source_rel_paths: Vec<String>,
    dest_rel_folder: &str,
) -> Result<MoveOutcome, String> {
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace root: {e}"))?;
    let dest_dir = if dest_rel_folder.is_empty() {
        canon_root.clone()
    } else {
        safe_path_within(root, dest_rel_folder)?
    };
    if !dest_dir.is_dir() {
        return Err(format!(
            "destination `{dest_rel_folder}` is not a directory"
        ));
    }

    let mut moved = 0u64;
    let mut skipped_conflict = 0u64;
    let mut skipped_invalid = 0u64;
    let mut moved_rel_paths: Vec<String> = Vec::new();

    for rel in source_rel_paths {
        if rel.is_empty() || rel == ".thinkingroot" || rel.starts_with(".thinkingroot/") {
            skipped_invalid += 1;
            continue;
        }
        let source = match safe_path_within(root, &rel) {
            Ok(p) => p,
            Err(_) => {
                skipped_invalid += 1;
                continue;
            }
        };
        if !source.exists() {
            skipped_invalid += 1;
            continue;
        }
        let leaf = match source.file_name() {
            Some(n) => n.to_owned(),
            None => {
                skipped_invalid += 1;
                continue;
            }
        };
        let target = dest_dir.join(&leaf);
        if target == source {
            skipped_conflict += 1;
            continue;
        }
        if target.exists() {
            skipped_conflict += 1;
            continue;
        }
        // Prevent moving a folder into itself.
        if source.is_dir() && target.starts_with(&source) {
            skipped_invalid += 1;
            continue;
        }
        match fs::rename(&source, &target) {
            Ok(()) => {
                moved += 1;
                moved_rel_paths.push(rel_to_workspace(&target, &canon_root));
            }
            Err(_) => {
                skipped_invalid += 1;
            }
        }
    }

    Ok(MoveOutcome {
        moved,
        skipped_conflict,
        skipped_invalid,
        moved_rel_paths,
    })
}

// ── helpers ────────────────────────────────────────────────────────

fn validate_leaf_name(name: &str, label: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(format!("invalid {label}: `{name}`"));
    }
    Ok(())
}

/// Reject relative paths that escape `root` via `..`, absolute, or
/// device-prefix components. Canonicalise when the candidate exists
/// (defends against symlink traversal); for not-yet-existing
/// destinations, canonicalise the parent and re-join the literal leaf.
fn safe_path_within(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"));
    }
    let candidate = Path::new(rel);
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(format!("invalid path component in `{rel}`")),
        }
    }
    let joined = root.join(candidate);
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace root: {e}"))?;
    match joined.canonicalize() {
        Ok(canon) => {
            if !canon.starts_with(&canon_root) {
                return Err(format!(
                    "resolved path `{}` escapes workspace root",
                    canon.display()
                ));
            }
            Ok(canon)
        }
        Err(_) => {
            let parent = joined
                .parent()
                .ok_or_else(|| format!("path `{rel}` has no parent"))?;
            let canon_parent = parent
                .canonicalize()
                .map_err(|e| format!("canonicalize parent `{}`: {e}", parent.display()))?;
            if !canon_parent.starts_with(&canon_root) {
                return Err(format!(
                    "parent of `{rel}` escapes workspace root: {}",
                    canon_parent.display()
                ));
            }
            let leaf = joined
                .file_name()
                .ok_or_else(|| format!("path `{rel}` has no leaf component"))?;
            Ok(canon_parent.join(leaf))
        }
    }
}

fn rel_to_workspace(absolute: &Path, canon_root: &Path) -> String {
    match absolute.strip_prefix(canon_root) {
        Ok(rel) => rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => absolute.to_string_lossy().into_owned(),
    }
}

fn classify_kind(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "heic" | "heif" | "tiff" => {
            "image"
        }
        "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" | "aac" => "audio",
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "wmv" | "m4v" => "video",
        "md" | "markdown" | "mdx" => "markdown",
        "txt" | "log" | "csv" | "tsv" | "json" | "yaml" | "yml" | "toml" | "ini" | "xml"
        | "html" | "css" => "text",
        "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "go" | "java" | "kt" | "swift" | "c" | "h"
        | "cpp" | "hpp" | "rb" | "php" | "sh" | "zsh" | "bash" | "fish" | "lua" | "scala"
        | "clj" | "ex" | "exs" | "erl" | "ml" | "fs" | "dart" => "code",
        "pdf" => "pdf",
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" => "archive",
        _ => "other",
    }
}

fn modified_secs(metadata: &fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_ws() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("inbox")).unwrap();
        fs::create_dir(root.join("inbox").join("nested")).unwrap();
        fs::write(root.join("README.md"), b"# hi\n").unwrap();
        fs::write(root.join("inbox").join("note.txt"), b"plain\n").unwrap();
        fs::create_dir(root.join(".thinkingroot")).unwrap();
        fs::write(root.join(".thinkingroot").join("paper.md"), b"engine\n").unwrap();
        dir
    }

    #[test]
    fn list_root_hides_engine_state_and_sorts_dirs_first() {
        let ws = setup_ws();
        let listing = list_directory(ws.path(), "test-ws", "").unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&".thinkingroot"));
        // dirs first → inbox before README.md
        assert_eq!(names, vec!["inbox", "README.md"]);
        assert_eq!(listing.parent_rel_path, None);
    }

    #[test]
    fn list_nested_returns_parent_rel() {
        let ws = setup_ws();
        let listing = list_directory(ws.path(), "test-ws", "inbox").unwrap();
        assert_eq!(listing.parent_rel_path, Some("".to_string()));
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["nested", "note.txt"]);
    }

    #[test]
    fn list_refuses_escape_via_dotdot() {
        let ws = setup_ws();
        let err = list_directory(ws.path(), "test-ws", "../etc").unwrap_err();
        assert!(err.contains("invalid path component"), "got: {err}");
    }

    #[test]
    fn create_folder_round_trips() {
        let ws = setup_ws();
        let rel = create_folder(ws.path(), "inbox", "drafts").unwrap();
        assert_eq!(rel, "inbox/drafts");
        assert!(ws.path().join("inbox").join("drafts").is_dir());
    }

    #[test]
    fn create_folder_refuses_collision() {
        let ws = setup_ws();
        let err = create_folder(ws.path(), "", "inbox").unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
    }

    #[test]
    fn rename_moves_within_same_parent() {
        let ws = setup_ws();
        let new_rel = rename_path(ws.path(), "README.md", "ABOUT.md").unwrap();
        assert_eq!(new_rel, "ABOUT.md");
        assert!(ws.path().join("ABOUT.md").exists());
        assert!(!ws.path().join("README.md").exists());
    }

    #[test]
    fn rename_refuses_engine_state() {
        let ws = setup_ws();
        let err = rename_path(ws.path(), ".thinkingroot", "trashed").unwrap_err();
        assert!(err.contains("engine-managed"), "got: {err}");
    }

    #[test]
    fn move_into_folder_counts_collision_honestly() {
        let ws = setup_ws();
        // Create a colliding name so move skips one source and
        // accepts another.
        fs::write(ws.path().join("inbox").join("README.md"), b"dup\n").unwrap();
        let outcome = move_paths(
            ws.path(),
            vec!["README.md".to_string()],
            "inbox",
        )
        .unwrap();
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.skipped_conflict, 1);
        assert_eq!(outcome.skipped_invalid, 0);
    }

    #[test]
    fn move_succeeds_with_clean_destination() {
        let ws = setup_ws();
        let outcome = move_paths(
            ws.path(),
            vec!["README.md".to_string()],
            "inbox/nested",
        )
        .unwrap();
        assert_eq!(outcome.moved, 1);
        assert_eq!(outcome.skipped_conflict, 0);
        assert_eq!(outcome.moved_rel_paths, vec!["inbox/nested/README.md"]);
        assert!(ws.path().join("inbox").join("nested").join("README.md").exists());
    }

    #[test]
    fn move_refuses_folder_into_itself() {
        let ws = setup_ws();
        let outcome = move_paths(
            ws.path(),
            vec!["inbox".to_string()],
            "inbox/nested",
        )
        .unwrap();
        // inbox can't move into inbox/nested (its own descendant).
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.skipped_invalid, 1);
    }

    #[test]
    fn move_refuses_engine_state_source() {
        let ws = setup_ws();
        let outcome = move_paths(
            ws.path(),
            vec![".thinkingroot".to_string()],
            "inbox",
        )
        .unwrap();
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.skipped_invalid, 1);
    }
}
