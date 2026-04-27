//! Desktop config loader.
//!
//! ThinkingRoot Desktop reads its workspace pointer from a TOML file
//! that the in-app Settings pane writes to. The same file is honoured
//! when set via the `THINKINGROOT_DESKTOP_CONFIG` env var (useful for
//! tests + corporate deployments).
//!
//! Path precedence (highest first):
//!   1. `$THINKINGROOT_DESKTOP_CONFIG` env var.
//!   2. `$XDG_CONFIG_HOME/thinkingroot/desktop.toml`.
//!   3. `$HOME/.config/thinkingroot/desktop.toml`.
//!
//! Once loaded, `AppConfig::env_or(KEY)` returns the process env var
//! first, then the config file — same convention `clap` uses with
//! `#[arg(env = "...")]`.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Loaded key→value pairs from the config file.
#[derive(Debug, Default, Clone)]
pub struct AppConfig {
    entries: BTreeMap<String, toml::Value>,
}

impl AppConfig {
    /// Read the config file from its default location. Returns an
    /// empty config when no file exists. Parse errors are loud.
    pub fn load() -> anyhow::Result<Self> {
        let Some(path) = resolve_path() else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let entries: BTreeMap<String, toml::Value> = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        Ok(Self { entries })
    }

    /// Borrow the value of a string-typed key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        match self.entries.get(key)? {
            toml::Value::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Env var first, then config file.
    #[must_use]
    pub fn env_or(&self, key: &str) -> Option<String> {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return Some(v);
            }
        }
        self.get(key).map(String::from)
    }
}

fn resolve_path() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("THINKINGROOT_DESKTOP_CONFIG") {
        if !override_path.is_empty() {
            return Some(PathBuf::from(override_path));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("thinkingroot").join("desktop.toml"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("thinkingroot")
            .join("desktop.toml"),
    )
}
