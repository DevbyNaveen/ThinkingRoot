// crates/thinkingroot-serve/src/intelligence/environment.rs
//
// Environment-info gather + render — the substrate that fixes the
// "AI can't find Desktop" class of bugs.
//
// Claude Code's `prompts.ts::computeSimpleEnvInfo` (`~/Desktop/src/`
// audit, 2026-05-18) injects: cwd, OS, shell, platform, model, date,
// git-repo flag, knowledge cutoff. We mirror the load-bearing subset
// (cwd, OS, shell, $HOME, ~/Desktop, ~/Documents, ~/Downloads, today's
// date) into a `<system-reminder>` block via `reminder_bus.rs`.
//
// Why a separate module and not just inline strings in `reminder_bus.rs`:
//   * Gather is fallible (env vars can be missing, dirs::home_dir can
//     return None on weird platforms). The module isolates the
//     fallibility so the bus stays a pure renderer.
//   * Render is deterministic — same `EnvironmentInfo` → same bytes —
//     which keeps prompt-cache hits across turns where the user hasn't
//     moved directories.
//   * The `gather` path is cheap enough to call on every turn (single
//     digit μs: a few env var lookups + a `dirs::home_dir()` call) so
//     callers don't need to cache. The desktop sidecar runs as a
//     daemon, the env doesn't change at runtime, but we don't
//     special-case that — the daemon-vs-CLI distinction stays at the
//     call site.
//
// The cwd-discovery rule the AI follows is in `synthesizer.rs`'s new
// `TOOL_USE_PRINCIPLES` section: "When a user names a common location
// like Desktop or Documents, resolve it from the `<environment>`
// reminder block — don't ask the user to paste the path."

use std::path::PathBuf;

/// Snapshot of the host environment the agent runs against. Built by
/// [`gather`] once per turn from real OS state — no cached values, no
/// stubs in production. Tests construct values directly.
///
/// The `Option<PathBuf>` fields are present-when-resolvable; missing
/// fields render as omitted lines rather than empty values. This is
/// the Honesty Rule #1 contract — when we can't determine `~/Desktop`
/// (e.g. headless CI without HOME set), we don't fabricate; we omit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentInfo {
    /// Current working directory of the daemon (or CLI) at gather time.
    /// On the desktop sidecar this is typically the user's home; on
    /// the CLI this is wherever the user ran `root ...`. `None` only
    /// in pathological cases (deleted-while-running).
    pub cwd: Option<PathBuf>,
    /// User's home directory. `None` on platforms / setups where
    /// `dirs::home_dir()` can't resolve it (rare; e.g. systemd
    /// `--user` units without `HOME=` set).
    pub home: Option<PathBuf>,
    /// `~/Desktop` if it exists on disk. Probed via `try_exists` —
    /// not just constructed from `home + "Desktop"` — so the AI
    /// doesn't act on a path that doesn't actually exist (which
    /// happens on Linux setups using XDG dirs differently).
    pub desktop: Option<PathBuf>,
    /// `~/Documents` if it exists on disk.
    pub documents: Option<PathBuf>,
    /// `~/Downloads` if it exists on disk.
    pub downloads: Option<PathBuf>,
    /// OS family — "macos" / "linux" / "windows" / "other". Lower-cased
    /// canonical form so the LLM sees consistent strings across the
    /// three production targets.
    pub os: &'static str,
    /// Shell name from `$SHELL` if set, otherwise `None`. On Windows
    /// we don't attempt PowerShell-vs-cmd detection — the user-shell
    /// concept doesn't translate cleanly. Free-form because shell
    /// detection is best-effort.
    pub shell: Option<String>,
    /// Today's date in ISO-8601 `YYYY-MM-DD`. The AI uses this for
    /// temporal-reasoning prompts ("when did we last edit X?") and
    /// for surface-level grounding ("today's date is 2026-05-18,
    /// not 2024-01-01 — knowledge cutoff is older than that").
    pub today_iso: String,
}

/// Gather the live environment for this turn. Cheap (~μs). Never
/// panics; missing pieces become `None`.
pub fn gather() -> EnvironmentInfo {
    let cwd = std::env::current_dir().ok();
    let home = dirs::home_dir();

    let desktop = home.as_ref().and_then(|h| existing(h.join("Desktop")));
    let documents = home.as_ref().and_then(|h| existing(h.join("Documents")));
    let downloads = home.as_ref().and_then(|h| existing(h.join("Downloads")));

    let os = os_label();
    let shell = std::env::var("SHELL").ok().and_then(|s| {
        if s.is_empty() {
            None
        } else {
            // Just the basename — "/bin/zsh" → "zsh". The full path
            // isn't useful to the LLM, and trimming keeps the line
            // short for cache stability across users on different
            // distros.
            Some(
                std::path::Path::new(&s)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
                    .unwrap_or(s),
            )
        }
    });

    let today_iso = chrono::Local::now().format("%Y-%m-%d").to_string();

    EnvironmentInfo {
        cwd,
        home,
        desktop,
        documents,
        downloads,
        os,
        shell,
        today_iso,
    }
}

/// Render the environment as a `# environment` block body. The
/// `<system-reminder>` wrap happens upstream in `reminder_bus.rs::
/// wrap_reminder` — this function emits just the inner key-value
/// lines so the wrapper stays one canonical implementation.
///
/// Output is stable: same `EnvironmentInfo` → byte-identical output.
/// Lines for `None` fields are simply omitted (never `key: null`).
pub fn render_block(env: &EnvironmentInfo) -> String {
    let mut out = String::from("# environment\n");
    if let Some(cwd) = &env.cwd {
        out.push_str(&format!("cwd: {}\n", cwd.display()));
    }
    if let Some(home) = &env.home {
        out.push_str(&format!("home: {}\n", home.display()));
    }
    if let Some(desktop) = &env.desktop {
        out.push_str(&format!("desktop: {}\n", desktop.display()));
    }
    if let Some(documents) = &env.documents {
        out.push_str(&format!("documents: {}\n", documents.display()));
    }
    if let Some(downloads) = &env.downloads {
        out.push_str(&format!("downloads: {}\n", downloads.display()));
    }
    out.push_str(&format!("os: {}\n", env.os));
    if let Some(shell) = &env.shell {
        out.push_str(&format!("shell: {shell}\n"));
    }
    out.push_str(&format!("today: {}\n", env.today_iso));
    out
}

/// Canonical OS label. Three production targets get explicit names;
/// everything else is `"other"` honestly rather than misclassified.
fn os_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "other"
    }
}

/// Return `Some(path)` iff `path.try_exists()` returns `Ok(true)`.
/// Probe errors map to `None` — we never surface a path the AI might
/// try to use only to find it doesn't exist.
fn existing(path: PathBuf) -> Option<PathBuf> {
    match path.try_exists() {
        Ok(true) => Some(path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> EnvironmentInfo {
        EnvironmentInfo {
            cwd: Some(PathBuf::from("/Users/naveen/Desktop/thinkingroot")),
            home: Some(PathBuf::from("/Users/naveen")),
            desktop: Some(PathBuf::from("/Users/naveen/Desktop")),
            documents: Some(PathBuf::from("/Users/naveen/Documents")),
            downloads: Some(PathBuf::from("/Users/naveen/Downloads")),
            os: "macos",
            shell: Some("zsh".to_string()),
            today_iso: "2026-05-18".to_string(),
        }
    }

    #[test]
    fn render_block_emits_every_resolved_field() {
        let block = render_block(&fixture());
        assert!(block.starts_with("# environment\n"));
        assert!(block.contains("cwd: /Users/naveen/Desktop/thinkingroot"));
        assert!(block.contains("home: /Users/naveen"));
        assert!(block.contains("desktop: /Users/naveen/Desktop"));
        assert!(block.contains("documents: /Users/naveen/Documents"));
        assert!(block.contains("downloads: /Users/naveen/Downloads"));
        assert!(block.contains("os: macos"));
        assert!(block.contains("shell: zsh"));
        assert!(block.contains("today: 2026-05-18"));
    }

    #[test]
    fn render_block_omits_none_fields_honestly() {
        let env = EnvironmentInfo {
            cwd: None,
            home: None,
            desktop: None,
            documents: None,
            downloads: None,
            os: "linux",
            shell: None,
            today_iso: "2026-05-18".to_string(),
        };
        let block = render_block(&env);
        assert!(!block.contains("cwd:"), "cwd line must be omitted, got: {block}");
        assert!(!block.contains("home:"));
        assert!(!block.contains("desktop:"));
        assert!(!block.contains("documents:"));
        assert!(!block.contains("downloads:"));
        assert!(!block.contains("shell:"));
        // os + today are always present
        assert!(block.contains("os: linux"));
        assert!(block.contains("today: 2026-05-18"));
    }

    #[test]
    fn render_block_is_deterministic_byte_for_byte() {
        let env1 = fixture();
        let env2 = fixture();
        // Critical for prompt caching — same env → same bytes.
        assert_eq!(render_block(&env1), render_block(&env2));
    }

    #[test]
    fn gather_returns_populated_struct_on_normal_run() {
        // We're running cargo test, so cwd + os + today are
        // guaranteed. home/desktop/documents/downloads are
        // environment-dependent so we only assert they parse, not
        // their specific values.
        let env = gather();
        assert!(env.cwd.is_some(), "cargo test always has a cwd");
        assert!(!env.os.is_empty(), "os label must never be empty");
        assert_eq!(env.os.len(), env.os.trim().len(), "os label is canonical");
        assert!(
            matches!(env.os, "macos" | "linux" | "windows" | "other"),
            "unexpected os label: {}",
            env.os
        );
        // Today: 10 chars YYYY-MM-DD
        assert_eq!(env.today_iso.len(), 10);
        assert_eq!(&env.today_iso[4..5], "-");
        assert_eq!(&env.today_iso[7..8], "-");
    }

    #[test]
    fn existing_returns_none_for_nonexistent_path() {
        let result = existing(PathBuf::from("/__definitely_does_not_exist_xyz_12345__"));
        assert_eq!(result, None);
    }

    #[test]
    fn os_label_is_one_of_four_canonical_values() {
        let label = os_label();
        assert!(
            matches!(label, "macos" | "linux" | "windows" | "other"),
            "unexpected os label: {label}"
        );
    }

    #[test]
    fn shell_extracts_basename_from_full_path() {
        // The basename-extraction logic is exercised through gather()
        // when SHELL is set. We can't safely set env vars in a
        // parallel test, but we can verify the logic directly via
        // the public path module.
        let p = std::path::Path::new("/usr/bin/fish");
        let basename = p.file_name().and_then(|n| n.to_str()).unwrap();
        assert_eq!(basename, "fish");
    }
}
