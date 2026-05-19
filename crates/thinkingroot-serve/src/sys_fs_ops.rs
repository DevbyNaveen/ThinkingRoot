//! System-wide (absolute-path) filesystem operations — the sibling
//! of `fs_ops` for paths OUTSIDE the workspace root.
//!
//! Why this exists: `fs_ops::{list_directory, move_paths, rename_path,
//! create_folder}` are workspace-scoped — every path is `rel` under
//! `engine.workspace_root_path(ws)`. That covers "organise this
//! workspace's inbox" but not "move my folder from `~/Desktop` to
//! `~/Documents`". For the broader case the in-app agent needs a tool
//! family that takes absolute paths, with `PermissionsGate`'s
//! DEFAULT_DENY (the `~/.ssh`, `~/.aws`, `~/Library/Keychains` short-
//! list) honoured directly here for the read-class tools that don't go
//! through the gate's approval flow.
//!
//! Honesty contract carries over: collisions surface as
//! `skipped_conflict`, never silent overwrites; "engine-managed"
//! protections from `fs_ops` aren't applicable here (we're outside
//! workspace roots), but the sensitive-path block IS.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// One entry in an absolute-path directory listing. Same shape as
/// `fs_ops::DirEntry` except `path` is absolute (forward-slash
/// regardless of OS, but rooted at the listed directory).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysDirEntry {
    pub name: String,
    /// Absolute path to this entry, forward-slash separated.
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    /// Unix seconds since epoch.
    pub modified: f64,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysDirListing {
    /// The absolute path that was listed (canonicalised).
    pub path: String,
    /// Parent directory's absolute path, or `None` at filesystem root.
    pub parent_path: Option<String>,
    pub entries: Vec<SysDirEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysStat {
    pub path: String,
    pub exists: bool,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub size_bytes: u64,
    pub modified: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysMoveOutcome {
    pub moved: u64,
    pub skipped_conflict: u64,
    pub skipped_invalid: u64,
    /// Absolute path each moved item now lives at.
    pub moved_paths: Vec<String>,
    /// One human-readable reason per skipped item, in the same order
    /// the sources were given. Empty string = moved cleanly. Lets the
    /// caller surface "skipped because destination already had a
    /// `foo.txt`" instead of just a count.
    pub per_source_reason: Vec<String>,
}

#[derive(Debug)]
pub enum SysFsError {
    InvalidPath(String),
    SensitivePath(String),
    NotFound(String),
    NotADirectory(String),
    AlreadyExists(String),
    Io(String, std::io::Error),
}

impl std::fmt::Display for SysFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath(p) => write!(f, "invalid path: `{p}`"),
            Self::SensitivePath(p) => write!(
                f,
                "refusing to access sensitive path `{p}` — covered by PermissionsGate DEFAULT_DENY (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/Library/Keychains`, `/etc`)"
            ),
            Self::NotFound(p) => write!(f, "`{p}` does not exist"),
            Self::NotADirectory(p) => write!(f, "`{p}` is not a directory"),
            Self::AlreadyExists(p) => write!(f, "`{p}` already exists"),
            Self::Io(p, e) => write!(f, "io error on `{p}`: {e}"),
        }
    }
}

impl std::error::Error for SysFsError {}

/// Hardcoded "always refuse" prefixes — matches the spirit of
/// `PermissionsGate`'s DEFAULT_DENY rules without needing to plumb
/// the gate through the read-class handlers. Resolved against the
/// current user's HOME (and well-known system locations) at call
/// time so test environments with a sandboxed HOME aren't accidentally
/// gated against the real user's `~/.ssh`.
fn sensitive_prefixes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".ssh"));
        out.push(home.join(".aws"));
        out.push(home.join(".gnupg"));
        out.push(home.join(".config").join("gh"));
        out.push(home.join("Library").join("Keychains"));
        out.push(home.join("Library").join("Application Support").join("Google").join("Chrome"));
        out.push(home.join("Library").join("Safari"));
        out.push(home.join("Library").join("Cookies"));
    }
    out.push(PathBuf::from("/etc"));
    out.push(PathBuf::from("/var/db"));
    out
}

/// True iff `candidate` (after canonicalising its parent — leaf may
/// not exist) is at-or-below any sensitive prefix.
pub fn is_sensitive_path(candidate: &Path) -> bool {
    let prefixes = sensitive_prefixes();
    let canon = canonicalise_best_effort(candidate);
    for prefix in &prefixes {
        let canon_prefix = prefix.canonicalize().unwrap_or_else(|_| prefix.clone());
        if canon.starts_with(&canon_prefix) {
            return true;
        }
    }
    false
}

/// Canonicalise when possible; if leaf doesn't exist, canonicalise
/// parent + rejoin leaf (so we still resolve symlinks in the parent
/// chain). Mirrors `fs_ops::safe_path_within`'s fallback discipline.
fn canonicalise_best_effort(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    if let Some(parent) = path.parent() {
        if let Ok(cp) = parent.canonicalize() {
            if let Some(leaf) = path.file_name() {
                return cp.join(leaf);
            }
            return cp;
        }
    }
    path.to_path_buf()
}

/// Reject empty / non-absolute paths. Expand a leading `~` to HOME
/// so the LLM can pass `~/Desktop/foo` literally without first
/// resolving it (mirrors `PermissionsGate::canonicalize_subject`'s
/// tilde handling).
pub fn parse_absolute_input(raw: &str) -> Result<PathBuf, SysFsError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SysFsError::InvalidPath("(empty)".into()));
    }
    let expanded = if let Some(stripped) = trimmed.strip_prefix('~') {
        let home = dirs::home_dir()
            .ok_or_else(|| SysFsError::InvalidPath("HOME not resolvable".into()))?;
        // `~` alone, `~/foo`, or `~foo` — only the first two are
        // honoured; `~user` is unsupported (rare on macOS desktop,
        // would require getpwnam).
        if stripped.is_empty() {
            home
        } else if let Some(rest) = stripped.strip_prefix('/') {
            home.join(rest)
        } else {
            // `~foo` — treat as literal `~foo` (probably an error
            // upstream, but don't silently rewrite into `$HOME/foo`).
            PathBuf::from(trimmed)
        }
    } else {
        PathBuf::from(trimmed)
    };
    if !expanded.is_absolute() {
        return Err(SysFsError::InvalidPath(format!(
            "path must be absolute (got `{raw}`)"
        )));
    }
    // Reject paths containing `..` components — defends against the
    // LLM constructing `/Users/.../Desktop/../.ssh/id_rsa` to skirt
    // the sensitive-path check (canonicalisation handles this for
    // existing paths; the explicit reject handles non-existent
    // create-folder destinations).
    for comp in expanded.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(SysFsError::InvalidPath(format!(
                "path may not contain `..` components (got `{raw}`)"
            )));
        }
    }
    Ok(expanded)
}

pub fn sys_stat(raw_path: &str) -> Result<SysStat, SysFsError> {
    let path = parse_absolute_input(raw_path)?;
    if is_sensitive_path(&path) {
        return Err(SysFsError::SensitivePath(path.display().to_string()));
    }
    let display = path.display().to_string();
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            let is_symlink = meta.file_type().is_symlink();
            // For symlinks, follow once for is_dir/is_file
            // classification; fall back to symlink-time meta if
            // the target is missing (broken link).
            let target_meta = if is_symlink {
                fs::metadata(&path).ok()
            } else {
                None
            };
            let m = target_meta.as_ref().unwrap_or(&meta);
            Ok(SysStat {
                path: display,
                exists: true,
                is_dir: m.is_dir(),
                is_file: m.is_file(),
                is_symlink,
                size_bytes: m.len(),
                modified: modified_secs(m),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SysStat {
            path: display,
            exists: false,
            is_dir: false,
            is_file: false,
            is_symlink: false,
            size_bytes: 0,
            modified: 0.0,
        }),
        Err(e) => Err(SysFsError::Io(display, e)),
    }
}

pub fn sys_list(raw_path: &str) -> Result<SysDirListing, SysFsError> {
    let path = parse_absolute_input(raw_path)?;
    if is_sensitive_path(&path) {
        return Err(SysFsError::SensitivePath(path.display().to_string()));
    }
    let canon = path.canonicalize().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SysFsError::NotFound(path.display().to_string()),
        _ => SysFsError::Io(path.display().to_string(), e),
    })?;
    if !canon.is_dir() {
        return Err(SysFsError::NotADirectory(canon.display().to_string()));
    }
    let mut entries: Vec<SysDirEntry> = Vec::new();
    let iter = fs::read_dir(&canon).map_err(|e| SysFsError::Io(canon.display().to_string(), e))?;
    for raw in iter {
        let Ok(entry) = raw else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let abs = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        entries.push(SysDirEntry {
            name: name.clone(),
            path: abs.display().to_string(),
            is_dir: meta.is_dir(),
            size_bytes: meta.len(),
            modified: modified_secs(&meta),
            kind: classify_kind(&name).to_string(),
        });
    }
    // Stable order — dirs first then files, both name-sorted.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    let parent_path = canon
        .parent()
        .map(|p| p.display().to_string())
        .filter(|s| !s.is_empty());
    Ok(SysDirListing {
        path: canon.display().to_string(),
        parent_path,
        entries,
    })
}

pub fn sys_create_folder(raw_path: &str) -> Result<String, SysFsError> {
    let path = parse_absolute_input(raw_path)?;
    if is_sensitive_path(&path) {
        return Err(SysFsError::SensitivePath(path.display().to_string()));
    }
    if path.exists() {
        return Err(SysFsError::AlreadyExists(path.display().to_string()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| SysFsError::InvalidPath(path.display().to_string()))?;
    if !parent.exists() {
        return Err(SysFsError::NotFound(parent.display().to_string()));
    }
    fs::create_dir(&path).map_err(|e| SysFsError::Io(path.display().to_string(), e))?;
    Ok(path.display().to_string())
}

pub fn sys_rename(raw_path: &str, new_name: &str) -> Result<String, SysFsError> {
    let path = parse_absolute_input(raw_path)?;
    if is_sensitive_path(&path) {
        return Err(SysFsError::SensitivePath(path.display().to_string()));
    }
    if new_name.is_empty()
        || new_name.contains('/')
        || new_name.contains('\\')
        || new_name == "."
        || new_name == ".."
    {
        return Err(SysFsError::InvalidPath(format!(
            "invalid new_name `{new_name}`"
        )));
    }
    if !path.exists() {
        return Err(SysFsError::NotFound(path.display().to_string()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| SysFsError::InvalidPath(path.display().to_string()))?;
    let dest = parent.join(new_name);
    if dest.exists() {
        return Err(SysFsError::AlreadyExists(dest.display().to_string()));
    }
    if is_sensitive_path(&dest) {
        return Err(SysFsError::SensitivePath(dest.display().to_string()));
    }
    fs::rename(&path, &dest).map_err(|e| SysFsError::Io(path.display().to_string(), e))?;
    Ok(dest.display().to_string())
}

pub fn sys_move(
    raw_sources: &[String],
    raw_dest_folder: &str,
) -> Result<SysMoveOutcome, SysFsError> {
    let dest = parse_absolute_input(raw_dest_folder)?;
    if is_sensitive_path(&dest) {
        return Err(SysFsError::SensitivePath(dest.display().to_string()));
    }
    if !dest.exists() {
        return Err(SysFsError::NotFound(dest.display().to_string()));
    }
    if !dest.is_dir() {
        return Err(SysFsError::NotADirectory(dest.display().to_string()));
    }
    let mut moved = 0u64;
    let mut skipped_conflict = 0u64;
    let mut skipped_invalid = 0u64;
    let mut moved_paths: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    for raw in raw_sources {
        let source = match parse_absolute_input(raw) {
            Ok(p) => p,
            Err(e) => {
                skipped_invalid += 1;
                reasons.push(format!("invalid source: {e}"));
                continue;
            }
        };
        if is_sensitive_path(&source) {
            skipped_invalid += 1;
            reasons.push(format!("source is a sensitive path: {}", source.display()));
            continue;
        }
        if !source.exists() {
            skipped_invalid += 1;
            reasons.push(format!("source does not exist: {}", source.display()));
            continue;
        }
        let Some(leaf) = source.file_name() else {
            skipped_invalid += 1;
            reasons.push(format!("source has no leaf: {}", source.display()));
            continue;
        };
        let target = dest.join(leaf);
        if target == source {
            skipped_conflict += 1;
            reasons.push("destination is the same path as source".into());
            continue;
        }
        if target.exists() {
            skipped_conflict += 1;
            reasons.push(format!(
                "destination already exists: {}",
                target.display()
            ));
            continue;
        }
        // Prevent moving a folder into itself or its descendants.
        if source.is_dir() && target.starts_with(&source) {
            skipped_invalid += 1;
            reasons.push(format!(
                "cannot move `{}` into its own descendant `{}`",
                source.display(),
                target.display()
            ));
            continue;
        }
        // EXDEV (errno 18 on macOS/Linux) signals a cross-device
        // rename — fall back to copy-then-delete so external drives
        // and `~/Desktop → /Volumes/foo` work without the agent
        // having to detect filesystem boundaries.
        const EXDEV: i32 = 18;
        match fs::rename(&source, &target) {
            Ok(()) => {
                moved += 1;
                moved_paths.push(target.display().to_string());
                reasons.push(String::new());
            }
            Err(e) if e.raw_os_error() == Some(EXDEV) => match cross_device_move(&source, &target)
            {
                Ok(()) => {
                    moved += 1;
                    moved_paths.push(target.display().to_string());
                    reasons.push(String::new());
                }
                Err(e) => {
                    skipped_invalid += 1;
                    reasons.push(format!("cross-device move failed: {e}"));
                }
            },
            Err(e) => {
                skipped_invalid += 1;
                reasons.push(format!("rename failed: {e}"));
            }
        }
    }
    Ok(SysMoveOutcome {
        moved,
        skipped_conflict,
        skipped_invalid,
        moved_paths,
        per_source_reason: reasons,
    })
}

fn cross_device_move(source: &Path, target: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        copy_dir_all(source, target)?;
        fs::remove_dir_all(source)?;
    } else {
        fs::copy(source, target)?;
        fs::remove_file(source)?;
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if ty.is_symlink() {
            // Best-effort: skip symlinks rather than risk dangling
            // pointers post-move. The agent learns via the reason
            // string if anything was skipped.
            continue;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn modified_secs(metadata: &fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_absolute_input_expands_tilde() {
        // `~` alone → HOME
        let p = parse_absolute_input("~").unwrap();
        assert_eq!(p, dirs::home_dir().unwrap());
        // `~/Desktop` → HOME/Desktop
        let p = parse_absolute_input("~/Desktop").unwrap();
        assert_eq!(p, dirs::home_dir().unwrap().join("Desktop"));
    }

    #[test]
    fn parse_absolute_input_rejects_relative() {
        assert!(parse_absolute_input("foo/bar").is_err());
        assert!(parse_absolute_input("./foo").is_err());
        assert!(parse_absolute_input("").is_err());
    }

    #[test]
    fn parse_absolute_input_rejects_dotdot() {
        assert!(parse_absolute_input("/Users/x/../etc/passwd").is_err());
    }

    #[test]
    fn sensitive_path_blocks_ssh() {
        if let Some(home) = dirs::home_dir() {
            let ssh = home.join(".ssh").join("id_rsa");
            // Construct the path (it may or may not exist on this
            // machine — the check uses canonicalise_best_effort).
            assert!(
                is_sensitive_path(&ssh),
                "~/.ssh/id_rsa must be classed sensitive"
            );
        }
    }

    #[test]
    fn sys_stat_returns_exists_false_for_missing() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.txt").display().to_string();
        let s = sys_stat(&missing).unwrap();
        assert!(!s.exists);
        assert!(!s.is_dir);
        assert!(!s.is_file);
    }

    #[test]
    fn sys_stat_reports_dir() {
        let tmp = TempDir::new().unwrap();
        let s = sys_stat(&tmp.path().display().to_string()).unwrap();
        assert!(s.exists);
        assert!(s.is_dir);
    }

    #[test]
    fn sys_list_returns_entries_dirs_first() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        fs::write(tmp.path().join("file.txt"), b"hi").unwrap();
        let listing = sys_list(&tmp.path().display().to_string()).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["subdir", "file.txt"]);
    }

    #[test]
    fn sys_create_folder_makes_directory() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("new-folder").display().to_string();
        let made = sys_create_folder(&target).unwrap();
        assert_eq!(made, target);
        assert!(tmp.path().join("new-folder").is_dir());
    }

    #[test]
    fn sys_create_folder_refuses_existing() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("a")).unwrap();
        let err = sys_create_folder(&tmp.path().join("a").display().to_string()).unwrap_err();
        assert!(matches!(err, SysFsError::AlreadyExists(_)));
    }

    #[test]
    fn sys_rename_round_trip() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("old.txt"), b"hi").unwrap();
        let renamed =
            sys_rename(&tmp.path().join("old.txt").display().to_string(), "new.txt").unwrap();
        assert_eq!(renamed, tmp.path().join("new.txt").display().to_string());
        assert!(tmp.path().join("new.txt").exists());
        assert!(!tmp.path().join("old.txt").exists());
    }

    #[test]
    fn sys_rename_refuses_slash_in_name() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hi").unwrap();
        let err = sys_rename(
            &tmp.path().join("a.txt").display().to_string(),
            "b/c.txt",
        )
        .unwrap_err();
        assert!(matches!(err, SysFsError::InvalidPath(_)));
    }

    #[test]
    fn sys_move_into_existing_dir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("src").join("note.txt"), b"hi").unwrap();
        let outcome = sys_move(
            &[tmp.path().join("src").display().to_string()],
            &tmp.path().join("dst").display().to_string(),
        )
        .unwrap();
        assert_eq!(outcome.moved, 1);
        assert_eq!(outcome.skipped_conflict, 0);
        assert_eq!(outcome.skipped_invalid, 0);
        assert!(tmp.path().join("dst").join("src").join("note.txt").exists());
        assert!(!tmp.path().join("src").exists());
    }

    #[test]
    fn sys_move_skips_conflict_honestly() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("dst")).unwrap();
        fs::write(tmp.path().join("a.txt"), b"new").unwrap();
        fs::write(tmp.path().join("dst").join("a.txt"), b"existing").unwrap();
        let outcome = sys_move(
            &[tmp.path().join("a.txt").display().to_string()],
            &tmp.path().join("dst").display().to_string(),
        )
        .unwrap();
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.skipped_conflict, 1);
        assert!(tmp.path().join("a.txt").exists()); // source untouched
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("dst").join("a.txt")).unwrap(),
            "existing"
        );
        assert!(outcome.per_source_reason[0].contains("already exists"));
    }

    #[test]
    fn sys_move_refuses_folder_into_descendant() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("outer")).unwrap();
        fs::create_dir(tmp.path().join("outer").join("inner")).unwrap();
        let outcome = sys_move(
            &[tmp.path().join("outer").display().to_string()],
            &tmp.path().join("outer").join("inner").display().to_string(),
        )
        .unwrap();
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.skipped_invalid, 1);
        assert!(outcome.per_source_reason[0].contains("descendant"));
    }

    #[test]
    fn sys_move_rejects_sensitive_dest() {
        // Try to drop something into ~/.ssh.
        let Some(home) = dirs::home_dir() else { return };
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("x.txt"), b"data").unwrap();
        let err = sys_move(
            &[tmp.path().join("x.txt").display().to_string()],
            &home.join(".ssh").display().to_string(),
        )
        .unwrap_err();
        assert!(matches!(err, SysFsError::SensitivePath(_)));
    }
}
