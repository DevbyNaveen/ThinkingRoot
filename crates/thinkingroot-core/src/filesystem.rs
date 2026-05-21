//! Filesystem-noise predicates shared across the watcher surfaces.
//!
//! Single source of truth for the "is this a real source-file change
//! or just filesystem chatter" classifier used by:
//!
//! - The CLI's `root compile --watch` debouncer
//!   (`crates/thinkingroot-cli/src/watch.rs`).
//! - The serve daemon's `workspace_watcher.rs` source-tree task.
//!
//! Pre-extraction these lived as a private function in `watch.rs`; the
//! daemon's watcher would have had to either depend on the CLI crate
//! (wrong direction) or fork the noise list (drift hazard). Hoisting to
//! `thinkingroot-core` keeps the two surfaces honest by construction —
//! a future-added noise pattern only needs to land here.

use std::path::Path;

/// Returns `true` for paths the workspace watchers should **ignore**.
///
/// Excluded:
/// - Engine-internal dirs: `.thinkingroot/`, `.git/`, `target/`,
///   `node_modules/`, `.next/`, `dist/`, `build/`, `__pycache__/`,
///   `.tox/`, `.venv/`.
/// - Any path component whose final segment starts with `.` (dotfiles
///   and the dot-dirs above).
/// - Editor scratch suffixes: `.swp`, `.swo`, `.swx`, `~`-suffixed,
///   `.tmp`, `.bak`, plus vim's `4913` pre-write probe.
///
/// Pure path logic; no I/O. Mirrors `cargo`'s "build artefact" filter
/// so a freshly-built `target/` doesn't trigger a recompile.
pub fn is_workspace_noise(p: &Path) -> bool {
    const NOISE_DIRS: &[&str] = &[
        ".thinkingroot",
        ".git",
        "target",
        "node_modules",
        ".next",
        "dist",
        "build",
        "__pycache__",
        ".tox",
        ".venv",
    ];

    for component in p.components() {
        let s = component.as_os_str();
        if NOISE_DIRS.iter().any(|&d| s == d) {
            return true;
        }
    }

    let Some(file_name) = p.file_name() else {
        return true;
    };
    let name = file_name.to_string_lossy();

    if name.starts_with('.') {
        return true;
    }

    // vim's pre-write probe file — emitted on every save, never a
    // real source change.
    if name == "4913" {
        return true;
    }

    if name.ends_with('~') {
        return true;
    }

    const NOISE_EXTENSIONS: &[&str] = &["swp", "swo", "swx", "tmp", "bak"];
    if let Some(ext) = p.extension() {
        let ext = ext.to_string_lossy();
        if NOISE_EXTENSIONS.iter().any(|&e| ext == e) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn engine_dir_is_noise() {
        assert!(is_workspace_noise(&PathBuf::from("/ws/.thinkingroot/x.json")));
        assert!(is_workspace_noise(&PathBuf::from("/ws/target/debug/foo")));
        assert!(is_workspace_noise(&PathBuf::from("/ws/node_modules/x/y.js")));
        assert!(is_workspace_noise(&PathBuf::from("/ws/.git/HEAD")));
    }

    #[test]
    fn editor_scratch_files_are_noise() {
        assert!(is_workspace_noise(&PathBuf::from("/ws/src/foo.rs.swp")));
        assert!(is_workspace_noise(&PathBuf::from("/ws/src/foo.rs.swx")));
        assert!(is_workspace_noise(&PathBuf::from("/ws/4913")));
        assert!(is_workspace_noise(&PathBuf::from("/ws/foo~")));
        assert!(is_workspace_noise(&PathBuf::from("/ws/.DS_Store")));
    }

    #[test]
    fn real_source_files_are_not_noise() {
        assert!(!is_workspace_noise(&PathBuf::from("/ws/src/main.rs")));
        assert!(!is_workspace_noise(&PathBuf::from("/ws/README.md")));
        assert!(!is_workspace_noise(&PathBuf::from("/ws/docs/spec.md")));
        assert!(!is_workspace_noise(&PathBuf::from("/ws/pkg/a/b.py")));
    }

    #[test]
    fn dotfiles_outside_known_dirs_are_noise() {
        // .env, .prettierrc, etc. — they may be config but the watcher
        // treats them as noise for "did source change" purposes. The
        // workspace's `config.toml` is filtered separately by the
        // engine-dir watcher.
        assert!(is_workspace_noise(&PathBuf::from("/ws/.env")));
        assert!(is_workspace_noise(&PathBuf::from("/ws/.prettierrc")));
    }

    #[test]
    fn paths_with_no_filename_are_noise() {
        assert!(is_workspace_noise(&PathBuf::from("/")));
    }
}
