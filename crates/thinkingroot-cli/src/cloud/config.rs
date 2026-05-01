//! Cloud config — `~/.config/thinkingroot/auth.json`.
//!
//! Holds the API token, server URL, and the user's identity (handle,
//! user_id) cached from the last `/me` call. The cache lets `root
//! whoami` answer instantly without hitting identity. Path is shared
//! verbatim with the legacy `tr` binary so existing sessions survive
//! the rename.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const DEFAULT_SERVER: &str = "https://api.thinkingroot.dev";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Bearer used by all API calls.
    #[serde(default)]
    pub token: Option<String>,
    /// Cloud server base URL — registry, identity, compile-worker
    /// are reachable under this host (with their canonical paths).
    pub server: String,
    /// Cached handle from the last successful login.
    #[serde(default)]
    pub handle: Option<String>,
    /// Cached user_id from the last successful login.
    #[serde(default)]
    pub user_id: Option<String>,
}

impl Config {
    pub fn empty() -> Self {
        Self {
            token: None,
            server: DEFAULT_SERVER.to_string(),
            handle: None,
            user_id: None,
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    let base =
        dirs::config_dir().ok_or_else(|| anyhow!("could not determine user config directory"))?;
    Ok(base.join("thinkingroot").join("auth.json"))
}

/// Load the saved config; if missing, return an empty one with the
/// default server URL. `override_server`, if provided, replaces the
/// stored server URL — used by the global `--server` flag.
pub fn load_or_default(override_server: Option<&str>) -> Result<Config> {
    let path = config_path()?;
    let mut cfg = if path.exists() {
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice::<Config>(&bytes)
            .with_context(|| format!("parse {}", path.display()))?
    } else {
        Config::empty()
    };
    if let Some(s) = override_server {
        cfg.server = s.to_string();
    }
    if cfg.server.is_empty() {
        cfg.server = DEFAULT_SERVER.to_string();
    }
    Ok(cfg)
}

/// Persist the config to disk, creating parent directories with
/// 0700 permissions and the file with 0600 on POSIX.
pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let body = serde_json::to_vec_pretty(cfg)?;
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Return the saved token, or a friendly error if the user hasn't
/// logged in.
pub fn require_token(cfg: &Config) -> Result<&str> {
    cfg.token
        .as_deref()
        .ok_or_else(|| anyhow!("not logged in — run `root login` first"))
}
