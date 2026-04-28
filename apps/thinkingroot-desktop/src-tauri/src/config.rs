//! Desktop-only state.
//!
//! Provider keys, provider/model config, and the workspace registry all live
//! in the engine's shared format under `dirs::config_dir()/thinkingroot/`:
//!
//! * `credentials.toml` — `Credentials` (mode 0600 on Unix). Provider keys.
//! * `config.toml`      — `GlobalConfig`. Provider/model defaults.
//! * `workspaces.toml`  — `WorkspaceRegistry`. Includes the active pointer.
//!
//! That leaves a small slice of state that is genuinely desktop-only and
//! has no engine equivalent — cloud session token, scan roots, UI prefs.
//! Those land in `desktop.toml` next to the engine files (same directory,
//! same OS conventions). No more `~/.config/thinkingroot/desktop.toml`
//! Linux-XDG outlier on macOS.
//!
//! # Migration
//!
//! On first load, if the legacy file at `$HOME/.config/thinkingroot/desktop.toml`
//! exists *and* the new file doesn't, we lift any recognised fields across,
//! then rename the legacy file to `desktop.toml.legacy`. Anything we can't
//! interpret (random user-pasted env-var keys) is left in the legacy file
//! for them to inspect manually.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Structured desktop-only state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesktopState {
    /// Cloud session token (set by the in-app login flow). Stored here
    /// rather than in `credentials.toml` because it is per-user, not
    /// per-provider, and is rotated by a different code path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_token: Option<String>,
    /// Cloud API base URL override (testing / on-prem).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_api_base: Option<String>,
    /// Cloud handle / username for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_handle: Option<String>,
    /// Directories the workspace-scan command should walk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scan_roots: Vec<PathBuf>,
    /// Whether the migration shim has already run on this profile. Stops
    /// us from re-importing legacy data after the user wipes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub migrated_from_legacy: bool,
}

impl DesktopState {
    /// Resolve the canonical config file path.
    ///
    /// Honours `THINKINGROOT_DESKTOP_CONFIG` for tests / corporate deployments,
    /// then falls back to the same `dirs::config_dir()` the engine uses so
    /// the desktop and the CLI write to the same parent directory on every
    /// platform (`~/Library/Application Support/thinkingroot/` on macOS,
    /// `~/.config/thinkingroot/` on Linux, `%APPDATA%\thinkingroot\` on
    /// Windows).
    pub fn path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("THINKINGROOT_DESKTOP_CONFIG") {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
        dirs::config_dir().map(|d| d.join("thinkingroot").join("desktop.toml"))
    }

    /// Load the desktop state, migrating from the legacy `~/.config/...`
    /// location on first call if needed. Returns `Ok(default)` when no
    /// state has ever been written.
    pub fn load() -> anyhow::Result<Self> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };

        let mut state = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            toml::from_str::<DesktopState>(&raw)
                .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?
        } else {
            Self::default()
        };

        if !state.migrated_from_legacy {
            if let Some(legacy_payload) = take_legacy_desktop_toml() {
                migrate_legacy(&mut state, &legacy_payload);
                state.migrated_from_legacy = true;
                let _ = state.save();
            } else {
                // Mark as migrated even when there's nothing to import, so
                // the disk read of the legacy file is one-shot.
                state.migrated_from_legacy = true;
                if path.exists() {
                    let _ = state.save();
                }
            }
        }

        Ok(state)
    }

    /// Persist the state, creating parent directories as needed.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve desktop config path"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("serialising desktop state: {e}"))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| anyhow::anyhow!("renaming {}: {e}", path.display()))?;
        Ok(())
    }

    /// Mutate the loaded state and persist it atomically. Kept available
    /// for future call sites (e.g. an upcoming "Save scan roots" command);
    /// `#[allow(dead_code)]` rather than removed so the API stays stable.
    #[allow(dead_code)]
    pub fn update(mut f: impl FnMut(&mut DesktopState)) -> anyhow::Result<DesktopState> {
        let mut state = Self::load()?;
        f(&mut state);
        state.save()?;
        Ok(state)
    }
}

/// Try to read the legacy `~/.config/thinkingroot/desktop.toml`.
/// Returns the parsed TOML document and renames the file to
/// `desktop.toml.legacy` so we don't re-import it.
fn take_legacy_desktop_toml() -> Option<toml::Table> {
    // Resolution mirrors the original `apps/.../src/config.rs::resolve_path`
    // so we pick up exactly the same file users wrote to before this commit.
    let home = std::env::var("HOME").ok()?;
    let legacy = PathBuf::from(home)
        .join(".config")
        .join("thinkingroot")
        .join("desktop.toml");

    // If the new path resolves to the same file (Linux), there's nothing to
    // migrate — that file IS our state already. Bail early.
    if let Some(new_path) = DesktopState::path() {
        if new_path == legacy {
            return None;
        }
    }

    if !legacy.exists() {
        return None;
    }

    let raw = std::fs::read_to_string(&legacy).ok()?;
    let parsed: toml::Table = toml::from_str(&raw).ok()?;

    let renamed = legacy.with_extension("toml.legacy");
    let _ = std::fs::rename(&legacy, &renamed);

    Some(parsed)
}

/// Lift legacy fields into the new structured state. Keys we don't
/// recognise are deliberately discarded — they were probably stale
/// onboarding-wizard scratch we don't want to honour silently.
fn migrate_legacy(state: &mut DesktopState, legacy: &toml::Table) {
    // Active workspace pointer → WorkspaceRegistry.active.
    if let Some(name) = legacy.get("THINKINGROOT_WORKSPACE_NAME").and_then(|v| v.as_str()) {
        if let Ok(mut registry) = thinkingroot_core::WorkspaceRegistry::load() {
            if registry.workspaces.iter().any(|w| w.name == name)
                && registry.active.as_deref() != Some(name)
            {
                registry.active = Some(name.to_string());
                let _ = registry.save();
            }
        }
    }

    // Provider keys → Credentials, with name corrections.
    let mut creds = thinkingroot_core::Credentials::load().unwrap_or_default();
    let mut creds_dirty = false;
    let key_map: &[(&str, &str)] = &[
        // The desktop UI used to write `AZURE_OPENAI_KEY`, but every consumer
        // (engine, CLI, workspace TOML default) reads `AZURE_OPENAI_API_KEY`.
        // Migrate the value under the correct name.
        ("AZURE_OPENAI_KEY", "AZURE_OPENAI_API_KEY"),
        ("ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"),
        ("OPENAI_API_KEY", "OPENAI_API_KEY"),
        ("GEMINI_API_KEY", "GEMINI_API_KEY"),
        ("GROQ_API_KEY", "GROQ_API_KEY"),
        ("DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"),
        ("OPENROUTER_API_KEY", "OPENROUTER_API_KEY"),
        ("TOGETHER_API_KEY", "TOGETHER_API_KEY"),
        ("PERPLEXITY_API_KEY", "PERPLEXITY_API_KEY"),
    ];
    for (legacy_name, canonical_name) in key_map {
        if let Some(v) = legacy.get(*legacy_name).and_then(|v| v.as_str()) {
            if !v.is_empty() && creds.get(canonical_name).is_none() {
                creds.set(canonical_name, v);
                creds_dirty = true;
            }
        }
    }
    if creds_dirty {
        let _ = creds.save();
    }

    // Cloud session — pure pass-through.
    if let Some(v) = legacy.get("TR_CLOUD_TOKEN").and_then(|v| v.as_str()) {
        if state.cloud_token.is_none() && !v.is_empty() {
            state.cloud_token = Some(v.to_string());
        }
    }
    if let Some(v) = legacy.get("TR_CLOUD_API_BASE").and_then(|v| v.as_str()) {
        if state.cloud_api_base.is_none() && !v.is_empty() {
            state.cloud_api_base = Some(v.to_string());
        }
    }
    if let Some(v) = legacy.get("TR_CLOUD_HANDLE").and_then(|v| v.as_str()) {
        if state.cloud_handle.is_none() && !v.is_empty() {
            state.cloud_handle = Some(v.to_string());
        }
    }

    // Scan roots — accept either an array or a comma-separated string.
    if state.scan_roots.is_empty() {
        match legacy.get("TR_SCAN_ROOTS") {
            Some(toml::Value::Array(items)) => {
                state.scan_roots = items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(PathBuf::from)
                    .collect();
            }
            Some(toml::Value::String(s)) => {
                state.scan_roots = s
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from)
                    .collect();
            }
            _ => {}
        }
    }
}
