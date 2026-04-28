//! Workspace auto-scan.
//!
//! Walks user-configured scan roots (or sensible defaults) up to a fixed
//! depth, looks for `.thinkingroot/` markers, and merges any discovered
//! workspaces into the on-disk [`WorkspaceRegistry`]. Existing entries
//! are preserved; only new paths are added.
//!
//! The UI calls this on app start (and via "Refresh" in the sidebar) so
//! the workspace tree mirrors what's actually on disk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};

use crate::config::DesktopState;

/// Maximum directory depth we descend when looking for `.thinkingroot/`.
/// Scanning is best-effort; users with deeply-nested project trees can
/// add precise roots in Settings instead of relying on the global walk.
const MAX_DEPTH: usize = 4;

/// Names we never recurse into. Skipping these is the difference between
/// a 200 ms scan and a 30 s scan on a typical dev machine.
const PRUNED: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".cache",
    ".idea",
    ".vscode",
    "Library",
    "Pictures",
    "Music",
    "Movies",
    "Applications",
];

#[derive(Debug, Serialize, Clone)]
pub struct ScanResult {
    pub roots: Vec<String>,
    pub discovered: Vec<String>,
    pub registered: Vec<String>,
    pub total: usize,
}

#[derive(Debug, Deserialize, Default)]
pub struct ScanArgs {
    /// Optional override; otherwise reads from config + defaults.
    #[serde(default)]
    pub roots: Vec<String>,
}

#[tauri::command]
pub fn workspace_scan(args: Option<ScanArgs>) -> Result<ScanResult, String> {
    let roots = resolve_scan_roots(args.unwrap_or_default().roots);

    let mut discovered: Vec<PathBuf> = Vec::new();
    for root in &roots {
        if let Ok(canon) = std::fs::canonicalize(root) {
            walk(&canon, 0, &mut discovered);
        }
    }

    let mut registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let known: HashSet<PathBuf> = registry
        .workspaces
        .iter()
        .map(|w| w.path.clone())
        .collect();

    let mut registered = Vec::new();
    for path in &discovered {
        if known.contains(path) {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string());
        let port = registry.next_available_port();
        registry.add(WorkspaceEntry {
            name: name.clone(),
            path: path.clone(),
            port,
        });
        registered.push(name);
    }
    if !registered.is_empty() {
        registry.save().map_err(|e| e.to_string())?;
    }

    Ok(ScanResult {
        roots: roots.iter().map(|p| p.display().to_string()).collect(),
        discovered: discovered.iter().map(|p| p.display().to_string()).collect(),
        registered,
        total: registry.workspaces.len(),
    })
}

fn resolve_scan_roots(explicit: Vec<String>) -> Vec<PathBuf> {
    if !explicit.is_empty() {
        return explicit
            .into_iter()
            .filter_map(|s| expand_tilde(&s))
            .collect();
    }
    // `TR_SCAN_ROOTS` env var (comma-separated) wins over persisted state —
    // matches the credential / cloud-token precedence used elsewhere.
    if let Ok(from_env) = std::env::var("TR_SCAN_ROOTS") {
        let parsed: Vec<PathBuf> = from_env
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(expand_tilde)
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    let from_state: Vec<PathBuf> = DesktopState::load()
        .unwrap_or_default()
        .scan_roots
        .into_iter()
        .filter_map(|p| expand_tilde(p.to_string_lossy().as_ref()))
        .collect();
    if !from_state.is_empty() {
        return from_state;
    }
    default_roots()
}

fn default_roots() -> Vec<PathBuf> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    [
        "Desktop",
        "Documents",
        "code",
        "dev",
        "projects",
        "src",
        "workspace",
    ]
    .iter()
    .map(|sub| home.join(sub))
    .filter(|p| p.exists())
    .collect()
}

fn expand_tilde(s: &str) -> Option<PathBuf> {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home).join(rest))
    } else if s == "~" {
        std::env::var("HOME").ok().map(PathBuf::from)
    } else {
        Some(PathBuf::from(s))
    }
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let marker = dir.join(".thinkingroot");
    if marker.is_dir() {
        out.push(dir.to_path_buf());
        return; // do not descend into a workspace
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.starts_with('.') && name != ".thinkingroot" {
            continue;
        }
        if PRUNED.contains(&name.as_str()) {
            continue;
        }
        walk(&path, depth + 1, out);
    }
}
