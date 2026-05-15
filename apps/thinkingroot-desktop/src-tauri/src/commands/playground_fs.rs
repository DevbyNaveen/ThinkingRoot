//! Playground file-management commands — folders, moves, previews, trash.
//!
//! Every operation is scoped to a registered workspace root via
//! [`WorkspaceRegistry::load`]. User-supplied relative paths are
//! normalised through [`safe_path_within`] which canonicalises BOTH
//! the workspace root and the candidate, then verifies `starts_with`
//! — defends against `../escape` attacks and symlink traversal.
//!
//! Trash convention: items move into `<workspace>/.thinkingroot/trash/`
//! with a timestamp prefix so name collisions never destroy old
//! trashed copies. Restoring inverts the rename. Empty-trash deletes
//! the directory tree. Compile naturally ignores `.thinkingroot/` so
//! trashed items don't re-extract.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::Serialize;
use thinkingroot_core::WorkspaceRegistry;

const PREVIEW_TEXT_MAX_BYTES: u64 = 1_024 * 1_024; // 1 MiB
const PREVIEW_IMAGE_MAX_BYTES: u64 = 5 * 1_024 * 1_024; // 5 MiB
const TRASH_REL: &str = ".thinkingroot/trash";

/// One entry in a directory listing.
#[derive(Debug, Clone, Serialize)]
pub struct PlaygroundDirEntry {
    pub name: String,
    /// Path relative to the workspace root, forward-slash separated
    /// regardless of OS, so the frontend can `split("/")`-parse safely.
    pub rel_path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    /// Unix seconds since epoch, as `f64` so JSON serialises cleanly
    /// without surprising integer-overflow conversions on the JS side.
    pub modified: f64,
    /// Coarse content classification driven by extension. The UI uses
    /// this for icon dispatch + preview-mode selection; downstream
    /// extractors (Witness Mesh) use their own MIME detection so a
    /// misclassification here is purely cosmetic.
    pub kind: String,
}

/// Result of [`playground_list_directory`].
#[derive(Debug, Clone, Serialize)]
pub struct PlaygroundDirListing {
    pub workspace: String,
    /// "" for workspace root, "inbox" or "inbox/sub" otherwise.
    pub rel_path: String,
    /// `None` when at workspace root; else the rel_path one level up.
    pub parent_rel_path: Option<String>,
    pub entries: Vec<PlaygroundDirEntry>,
}

/// Outcome of [`playground_move`]. Honest counts so the UI can show
/// "3 moved, 2 skipped — name conflict at destination".
#[derive(Debug, Clone, Serialize)]
pub struct PlaygroundMoveOutcome {
    pub moved: u64,
    pub skipped_conflict: u64,
    pub skipped_invalid: u64,
    pub moved_rel_paths: Vec<String>,
}

/// Outcome of [`playground_trash`].
#[derive(Debug, Clone, Serialize)]
pub struct PlaygroundTrashOutcome {
    pub trashed: u64,
    pub skipped: u64,
    /// Path of each item inside `.thinkingroot/trash/` (the trash-rel
    /// path the UI uses to drive Restore).
    pub trash_rel_paths: Vec<String>,
}

/// Preview payload — one of `kind="text" | "markdown" | "image" | "binary"`.
/// Audio / video / pdf / oversized files return `kind="binary"` with a
/// `size_bytes` value — the UI surfaces an "Open externally" affordance.
#[derive(Debug, Clone, Serialize)]
pub struct PlaygroundPreview {
    pub workspace: String,
    pub rel_path: String,
    pub kind: String,
    pub mime: Option<String>,
    pub size_bytes: u64,
    /// `Some(content)` for `kind="text" | "markdown"` (up to 1 MiB).
    pub text: Option<String>,
    /// `Some("data:image/png;base64,...")` for `kind="image"`
    /// (up to 5 MiB). Empty otherwise so the wire payload stays
    /// honest on large files.
    pub data_url: Option<String>,
    /// Absolute on-disk path. Surfaced so the UI's "Open externally"
    /// button can use the Tauri opener plugin to launch the OS
    /// default app.
    pub absolute_path: String,
    /// `true` when the file exceeds the inline-preview budget for
    /// its kind. UI shows a "too large to preview inline" message.
    pub too_large: bool,
}

/// Resolve a workspace name to its on-disk root via the registry.
fn workspace_root(workspace: &str) -> Result<PathBuf, String> {
    let registry =
        WorkspaceRegistry::load().map_err(|e| format!("workspace registry load: {e}"))?;
    registry
        .workspaces
        .into_iter()
        .find(|e| e.name == workspace)
        .map(|e| e.path)
        .ok_or_else(|| format!("workspace `{workspace}` not registered"))
}

/// Reject relative paths that would escape `root` via `..` components,
/// absolute components, or device prefixes. We do NOT canonicalise the
/// candidate (which would require it to exist) — instead we walk
/// `Component`s and refuse any non-`Normal` entry. After joining, if
/// the result canonicalises (file exists), we additionally verify it
/// starts with the canonicalised root.
///
/// Returns the canonicalised-when-possible absolute path. For
/// not-yet-existing destinations (e.g. a new folder being created),
/// the parent is checked instead.
fn safe_path_within(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        // Workspace root itself.
        return root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"));
    }
    let candidate = Path::new(rel);
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => return Err(format!("invalid path component in `{rel}`")),
        }
    }
    let joined = root.join(candidate);
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize workspace root: {e}"))?;
    match joined.canonicalize() {
        Ok(canon) => {
            if !canon.starts_with(&canonical_root) {
                return Err(format!(
                    "resolved path `{}` escapes workspace root",
                    canon.display()
                ));
            }
            Ok(canon)
        }
        Err(_) => {
            // Target doesn't exist yet (create-folder / move-destination
            // case). Verify the parent canonicalises within the root.
            let parent = joined
                .parent()
                .ok_or_else(|| format!("path `{rel}` has no parent"))?;
            let canon_parent = parent
                .canonicalize()
                .map_err(|e| format!("canonicalize parent `{}`: {e}", parent.display()))?;
            if !canon_parent.starts_with(&canonical_root) {
                return Err(format!(
                    "parent of `{rel}` escapes workspace root: {}",
                    canon_parent.display()
                ));
            }
            // The final filename component stays as the user wrote it
            // (we already rejected non-`Normal` components above).
            let leaf = joined
                .file_name()
                .ok_or_else(|| format!("path `{rel}` has no leaf component"))?;
            Ok(canon_parent.join(leaf))
        }
    }
}

/// Compute the rel_path of an absolute path against the workspace
/// root. Uses forward slashes so the frontend's `split("/")` parsing
/// is OS-independent.
fn rel_to_workspace(absolute: &Path, root: &Path) -> String {
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match absolute.strip_prefix(&canon_root) {
        Ok(rel) => rel
            .components()
            .map(|c| match c {
                Component::Normal(s) => s.to_string_lossy().into_owned(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => absolute.to_string_lossy().into_owned(),
    }
}

/// Coarse content classification by extension.
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

/// Modified time as Unix seconds (`f64`), or 0.0 if unavailable.
fn modified_secs(metadata: &fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Tauri command: list the contents of `<workspace>/<rel_path>`.
/// Hides the `.thinkingroot/` directory from listings so the user
/// doesn't accidentally rename / move engine-managed state.
#[tauri::command]
pub async fn playground_list_directory(
    workspace: String,
    rel_path: String,
) -> Result<PlaygroundDirListing, String> {
    tokio::task::spawn_blocking(move || -> Result<PlaygroundDirListing, String> {
        let root = workspace_root(&workspace)?;
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"))?;
        let target = if rel_path.is_empty() {
            canonical_root.clone()
        } else {
            safe_path_within(&root, &rel_path)?
        };

        let read = fs::read_dir(&target)
            .map_err(|e| format!("read_dir `{}`: {e}", target.display()))?;

        let mut entries: Vec<PlaygroundDirEntry> = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hide engine-managed directories — `.thinkingroot/` is
            // the only one currently — but only at the workspace
            // root. A user-named `.thinkingroot/` deeper in the tree
            // (unlikely but possible) stays visible.
            if rel_path.is_empty() && name == ".thinkingroot" {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue, // permission / race — skip honestly
            };
            let path = entry.path();
            let is_dir = metadata.is_dir();
            let size_bytes = if is_dir { 0 } else { metadata.len() };
            let modified = modified_secs(&metadata);
            let kind = if is_dir {
                "folder".to_string()
            } else {
                classify_kind(&name).to_string()
            };
            let rel = rel_to_workspace(&path, &canonical_root);
            entries.push(PlaygroundDirEntry {
                name,
                rel_path: rel,
                is_dir,
                size_bytes,
                modified,
                kind,
            });
        }
        // Sort: folders first, then by lowercased name.
        entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        let parent_rel_path = if rel_path.is_empty() {
            None
        } else {
            let parent = Path::new(&rel_path).parent().map(|p| {
                p.components()
                    .map(|c| match c {
                        Component::Normal(s) => s.to_string_lossy().into_owned(),
                        _ => String::new(),
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("/")
            });
            Some(parent.unwrap_or_default())
        };

        Ok(PlaygroundDirListing {
            workspace,
            rel_path,
            parent_rel_path,
            entries,
        })
    })
    .await
    .map_err(|e| format!("list_directory task panicked: {e}"))?
}

/// Tauri command: create a folder at
/// `<workspace>/<parent_rel_path>/<name>`. Refuses overwrite; refuses
/// names that contain path separators (must be a single segment).
#[tauri::command]
pub async fn playground_create_folder(
    workspace: String,
    parent_rel_path: String,
    name: String,
) -> Result<String, String> {
    if name.is_empty() {
        return Err("folder name must not be empty".into());
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(format!("invalid folder name: `{name}`"));
    }
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let root = workspace_root(&workspace)?;
        let target_rel = if parent_rel_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_rel_path}/{name}")
        };
        let target = safe_path_within(&root, &target_rel)?;
        if target.exists() {
            return Err(format!("`{}` already exists", target_rel));
        }
        fs::create_dir(&target)
            .map_err(|e| format!("create_dir `{}`: {e}", target.display()))?;
        Ok(target_rel)
    })
    .await
    .map_err(|e| format!("create_folder task panicked: {e}"))?
}

/// Tauri command: rename a file or folder. `rel_path` points to the
/// existing item; `new_name` is the new leaf name (single path
/// segment, no separators). Returns the new rel_path.
#[tauri::command]
pub async fn playground_rename(
    workspace: String,
    rel_path: String,
    new_name: String,
) -> Result<String, String> {
    if new_name.is_empty() {
        return Err("new name must not be empty".into());
    }
    if new_name.contains('/') || new_name.contains('\\') || new_name == "." || new_name == ".." {
        return Err(format!("invalid name: `{new_name}`"));
    }
    if rel_path.is_empty() {
        return Err("cannot rename the workspace root".into());
    }
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let root = workspace_root(&workspace)?;
        let source = safe_path_within(&root, &rel_path)?;
        if !source.exists() {
            return Err(format!("`{rel_path}` does not exist"));
        }
        let parent = source
            .parent()
            .ok_or_else(|| "source has no parent".to_string())?;
        let dest = parent.join(&new_name);
        if dest.exists() {
            return Err(format!(
                "destination `{}` already exists",
                dest.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        fs::rename(&source, &dest)
            .map_err(|e| format!("rename `{}` → `{}`: {e}", source.display(), dest.display()))?;
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"))?;
        Ok(rel_to_workspace(&dest, &canonical_root))
    })
    .await
    .map_err(|e| format!("rename task panicked: {e}"))?
}

/// Tauri command: move one or more items into a destination folder.
/// Skips items whose destination already exists (honest collision
/// handling — silent overwrite is the kind of "helpful" that loses
/// work).
#[tauri::command]
pub async fn playground_move(
    workspace: String,
    source_rel_paths: Vec<String>,
    dest_rel_folder: String,
) -> Result<PlaygroundMoveOutcome, String> {
    tokio::task::spawn_blocking(move || -> Result<PlaygroundMoveOutcome, String> {
        let root = workspace_root(&workspace)?;
        let dest_dir = if dest_rel_folder.is_empty() {
            root.canonicalize()
                .map_err(|e| format!("canonicalize workspace root: {e}"))?
        } else {
            safe_path_within(&root, &dest_rel_folder)?
        };
        if !dest_dir.is_dir() {
            return Err(format!(
                "destination `{dest_rel_folder}` is not a directory"
            ));
        }
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"))?;

        let mut moved = 0u64;
        let mut skipped_conflict = 0u64;
        let mut skipped_invalid = 0u64;
        let mut moved_rel_paths: Vec<String> = Vec::new();

        for rel in source_rel_paths {
            let source = match safe_path_within(&root, &rel) {
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
                // Moving into the same folder — no-op, count as conflict
                // for honest UI feedback.
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
                    moved_rel_paths.push(rel_to_workspace(&target, &canonical_root));
                }
                Err(_) => {
                    skipped_invalid += 1;
                }
            }
        }

        Ok(PlaygroundMoveOutcome {
            moved,
            skipped_conflict,
            skipped_invalid,
            moved_rel_paths,
        })
    })
    .await
    .map_err(|e| format!("move task panicked: {e}"))?
}

/// Tauri command: move items to `.thinkingroot/trash/<ts>-<name>`.
/// Returns the trash rel_paths so a follow-up Restore can target
/// the exact entries.
#[tauri::command]
pub async fn playground_trash(
    workspace: String,
    rel_paths: Vec<String>,
) -> Result<PlaygroundTrashOutcome, String> {
    tokio::task::spawn_blocking(move || -> Result<PlaygroundTrashOutcome, String> {
        let root = workspace_root(&workspace)?;
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"))?;
        let trash_dir = canonical_root.join(TRASH_REL);
        fs::create_dir_all(&trash_dir)
            .map_err(|e| format!("create trash dir: {e}"))?;

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut trashed = 0u64;
        let mut skipped = 0u64;
        let mut trash_rel_paths: Vec<String> = Vec::new();

        for rel in rel_paths {
            if rel.starts_with(".thinkingroot") {
                skipped += 1;
                continue;
            }
            let source = match safe_path_within(&root, &rel) {
                Ok(p) => p,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if !source.exists() {
                skipped += 1;
                continue;
            }
            let leaf = match source.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => {
                    skipped += 1;
                    continue;
                }
            };
            let trash_name = format!("{ts}-{leaf}");
            let target = trash_dir.join(&trash_name);
            match fs::rename(&source, &target) {
                Ok(()) => {
                    trashed += 1;
                    trash_rel_paths.push(format!("{TRASH_REL}/{trash_name}"));
                }
                Err(_) => {
                    skipped += 1;
                }
            }
        }

        Ok(PlaygroundTrashOutcome {
            trashed,
            skipped,
            trash_rel_paths,
        })
    })
    .await
    .map_err(|e| format!("trash task panicked: {e}"))?
}

/// Tauri command: list the workspace's trash contents.
#[tauri::command]
pub async fn playground_list_trash(workspace: String) -> Result<Vec<PlaygroundDirEntry>, String> {
    tokio::task::spawn_blocking(move || -> Result<Vec<PlaygroundDirEntry>, String> {
        let root = workspace_root(&workspace)?;
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"))?;
        let trash_dir = canonical_root.join(TRASH_REL);
        if !trash_dir.exists() {
            return Ok(Vec::new());
        }
        let read = fs::read_dir(&trash_dir)
            .map_err(|e| format!("read trash dir: {e}"))?;
        let mut entries: Vec<PlaygroundDirEntry> = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let is_dir = metadata.is_dir();
            let size_bytes = if is_dir { 0 } else { metadata.len() };
            let modified = modified_secs(&metadata);
            // Original name is everything after the first `-` (the
            // timestamp prefix). Used by the UI to surface the
            // human-readable name without exposing the prefix.
            let display = match name.split_once('-') {
                Some((_ts, rest)) if !rest.is_empty() => rest.to_string(),
                _ => name.clone(),
            };
            let kind = if is_dir {
                "folder".to_string()
            } else {
                classify_kind(&display).to_string()
            };
            entries.push(PlaygroundDirEntry {
                name: display,
                rel_path: format!("{TRASH_REL}/{name}"),
                is_dir,
                size_bytes,
                modified,
                kind,
            });
        }
        // Newest first so the most-recently-trashed lands at top.
        entries.sort_by(|a, b| b.modified.partial_cmp(&a.modified).unwrap_or(std::cmp::Ordering::Equal));
        Ok(entries)
    })
    .await
    .map_err(|e| format!("list_trash task panicked: {e}"))?
}

/// Tauri command: restore items from trash back to the workspace
/// root. The original-name reconstruction strips the timestamp
/// prefix; if a name collision occurs at the workspace root, the
/// restore is skipped (honest — the user can rename and retry).
#[tauri::command]
pub async fn playground_restore(
    workspace: String,
    trash_rel_paths: Vec<String>,
) -> Result<u64, String> {
    tokio::task::spawn_blocking(move || -> Result<u64, String> {
        let root = workspace_root(&workspace)?;
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"))?;
        let mut restored = 0u64;
        for rel in trash_rel_paths {
            if !rel.starts_with(TRASH_REL) {
                continue;
            }
            let source = match safe_path_within(&root, &rel) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !source.exists() {
                continue;
            }
            let trashed_name = match source.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let original = match trashed_name.split_once('-') {
                Some((_ts, rest)) if !rest.is_empty() => rest.to_string(),
                _ => trashed_name,
            };
            let target = canonical_root.join(&original);
            if target.exists() {
                // Name collision — keep the trashed copy.
                continue;
            }
            if fs::rename(&source, &target).is_ok() {
                restored += 1;
            }
        }
        Ok(restored)
    })
    .await
    .map_err(|e| format!("restore task panicked: {e}"))?
}

/// Tauri command: permanently delete every trashed item. Returns the
/// count of top-level entries removed (subtree deletions count as 1).
#[tauri::command]
pub async fn playground_empty_trash(workspace: String) -> Result<u64, String> {
    tokio::task::spawn_blocking(move || -> Result<u64, String> {
        let root = workspace_root(&workspace)?;
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("canonicalize workspace root: {e}"))?;
        let trash_dir = canonical_root.join(TRASH_REL);
        if !trash_dir.exists() {
            return Ok(0);
        }
        let read = fs::read_dir(&trash_dir).map_err(|e| format!("read trash dir: {e}"))?;
        let mut count = 0u64;
        for entry in read.flatten() {
            let p = entry.path();
            let removed = if p.is_dir() {
                fs::remove_dir_all(&p).is_ok()
            } else {
                fs::remove_file(&p).is_ok()
            };
            if removed {
                count += 1;
            }
        }
        Ok(count)
    })
    .await
    .map_err(|e| format!("empty_trash task panicked: {e}"))?
}

/// Tauri command: inline preview payload. Text / markdown / code
/// return UTF-8 content (up to 1 MiB). Images return a data-URL (up
/// to 5 MiB). Audio / video / pdf / oversized return a `binary` kind
/// + the absolute path so the UI can offer "Open externally" via
/// the existing Tauri opener plugin.
#[tauri::command]
pub async fn playground_preview(
    workspace: String,
    rel_path: String,
) -> Result<PlaygroundPreview, String> {
    tokio::task::spawn_blocking(move || -> Result<PlaygroundPreview, String> {
        let root = workspace_root(&workspace)?;
        let target = safe_path_within(&root, &rel_path)?;
        let metadata = fs::metadata(&target)
            .map_err(|e| format!("stat `{}`: {e}", target.display()))?;
        if metadata.is_dir() {
            return Err("preview target is a directory".into());
        }
        let size_bytes = metadata.len();
        let absolute_path = target.to_string_lossy().into_owned();
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel_path.clone());
        let kind = classify_kind(&name);
        let mime = match kind {
            "image" => guess_image_mime(&name),
            "audio" => Some("audio/*".to_string()),
            "video" => Some("video/*".to_string()),
            "markdown" => Some("text/markdown".to_string()),
            "text" | "code" => Some("text/plain".to_string()),
            "pdf" => Some("application/pdf".to_string()),
            _ => None,
        };

        match kind {
            "text" | "markdown" | "code" => {
                if size_bytes > PREVIEW_TEXT_MAX_BYTES {
                    return Ok(PlaygroundPreview {
                        workspace,
                        rel_path,
                        kind: kind.to_string(),
                        mime,
                        size_bytes,
                        text: None,
                        data_url: None,
                        absolute_path,
                        too_large: true,
                    });
                }
                let bytes = fs::read(&target).map_err(|e| format!("read: {e}"))?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                Ok(PlaygroundPreview {
                    workspace,
                    rel_path,
                    kind: kind.to_string(),
                    mime,
                    size_bytes,
                    text: Some(text),
                    data_url: None,
                    absolute_path,
                    too_large: false,
                })
            }
            "image" => {
                if size_bytes > PREVIEW_IMAGE_MAX_BYTES {
                    return Ok(PlaygroundPreview {
                        workspace,
                        rel_path,
                        kind: kind.to_string(),
                        mime,
                        size_bytes,
                        text: None,
                        data_url: None,
                        absolute_path,
                        too_large: true,
                    });
                }
                let bytes = fs::read(&target).map_err(|e| format!("read: {e}"))?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let mime_str = mime.as_deref().unwrap_or("image/*");
                let data_url = format!("data:{mime_str};base64,{b64}");
                Ok(PlaygroundPreview {
                    workspace,
                    rel_path,
                    kind: kind.to_string(),
                    mime,
                    size_bytes,
                    text: None,
                    data_url: Some(data_url),
                    absolute_path,
                    too_large: false,
                })
            }
            _ => Ok(PlaygroundPreview {
                workspace,
                rel_path,
                kind: "binary".to_string(),
                mime,
                size_bytes,
                text: None,
                data_url: None,
                absolute_path,
                too_large: false,
            }),
        }
    })
    .await
    .map_err(|e| format!("preview task panicked: {e}"))?
}

fn guess_image_mime(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    let m = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "heic" | "heif" => "image/heic",
        "tiff" => "image/tiff",
        _ => return None,
    };
    Some(m.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_workspace() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().to_path_buf();
        fs::create_dir_all(root.join("inbox")).unwrap();
        (tmp, root)
    }

    #[test]
    fn safe_path_rejects_parent_escape() {
        let (_tmp, root) = make_workspace();
        let err = safe_path_within(&root, "../etc/passwd").unwrap_err();
        assert!(err.contains("invalid path component"));
    }

    #[test]
    fn safe_path_rejects_absolute() {
        let (_tmp, root) = make_workspace();
        let err = safe_path_within(&root, "/etc/passwd").unwrap_err();
        assert!(err.contains("invalid path component"));
    }

    #[test]
    fn safe_path_accepts_nested_existing() {
        let (_tmp, root) = make_workspace();
        let nested = root.join("inbox").join("sub");
        fs::create_dir_all(&nested).unwrap();
        let resolved = safe_path_within(&root, "inbox/sub").unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            nested.canonicalize().unwrap()
        );
    }

    #[test]
    fn safe_path_accepts_pending_destination() {
        let (_tmp, root) = make_workspace();
        // `inbox/new-folder` doesn't exist yet but parent does.
        let resolved = safe_path_within(&root, "inbox/new-folder").unwrap();
        assert!(resolved.ends_with("inbox/new-folder"));
    }

    #[test]
    fn classify_kind_image_audio_video_text() {
        assert_eq!(classify_kind("photo.png"), "image");
        assert_eq!(classify_kind("song.mp3"), "audio");
        assert_eq!(classify_kind("clip.mp4"), "video");
        assert_eq!(classify_kind("notes.md"), "markdown");
        assert_eq!(classify_kind("config.toml"), "text");
        assert_eq!(classify_kind("main.rs"), "code");
        assert_eq!(classify_kind("doc.pdf"), "pdf");
        assert_eq!(classify_kind("backup.tar.gz"), "archive");
        assert_eq!(classify_kind("unknown.xyz"), "other");
    }

    #[test]
    fn rel_to_workspace_uses_forward_slashes() {
        let (_tmp, root) = make_workspace();
        let nested = root.join("inbox").join("sub").join("file.txt");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, b"hi").unwrap();
        let canonical = root.canonicalize().unwrap();
        let rel = rel_to_workspace(&nested.canonicalize().unwrap(), &canonical);
        assert_eq!(rel, "inbox/sub/file.txt");
    }
}
