//! Config + settings commands.
//!
//! Backs the onboarding wizard and the Settings surface. Reads and
//! writes `~/.config/thinkingroot/desktop.toml` (atomic + 0600 on
//! Unix) so API keys never leak through shared file perms.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use toml::Value;

use crate::config::AppConfig;

/// Raw projection of the config file. All values are either strings
/// or numbers rendered as strings — the frontend needs a uniform
/// shape to populate form fields.
#[derive(Debug, Serialize, Clone)]
pub struct ConfigRead {
    pub path: Option<String>,
    pub exists: bool,
    pub entries: BTreeMap<String, String>,
    pub masked_keys: Vec<String>,
}

/// The set of keys whose values we mask before sending to the UI.
/// Covers every provider + channel bearer token known today.
const SECRET_KEYS: &[&str] = &[
    "AZURE_OPENAI_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "TELEGRAM_BOT_TOKEN",
    "SLACK_BOT_TOKEN",
    "DISCORD_BOT_TOKEN",
];

#[tauri::command]
pub fn config_read() -> Result<ConfigRead, String> {
    let path = resolve_default_path();
    let exists = path.as_ref().is_some_and(|p| p.exists());
    let entries = if let Some(p) = &path {
        read_entries(p).map_err(|e| e.to_string())?
    } else {
        BTreeMap::new()
    };

    let masked_keys = SECRET_KEYS
        .iter()
        .filter(|k| entries.contains_key(**k))
        .map(|s| (*s).to_string())
        .collect();

    let display = entries
        .into_iter()
        .map(|(k, v)| {
            if SECRET_KEYS.contains(&k.as_str()) {
                (k, mask(&v))
            } else {
                (k, v)
            }
        })
        .collect();

    Ok(ConfigRead {
        path: path.map(|p| p.to_string_lossy().into_owned()),
        exists,
        entries: display,
        masked_keys,
    })
}

/// Patch the config file. Keys present in `updates` are upserted;
/// keys with a `None` value are removed. Returns the path written
/// to so the frontend can show it in a success toast.
#[derive(Debug, Deserialize)]
pub struct ConfigWriteArgs {
    /// Upserted keys: `{ "AZURE_OPENAI_KEY": "sk-..." }`.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Keys to delete if present.
    #[serde(default)]
    pub remove: Vec<String>,
}

#[tauri::command]
pub fn config_write(_app: AppHandle, args: ConfigWriteArgs) -> Result<String, String> {
    let path = resolve_default_path()
        .ok_or_else(|| "HOME not set; cannot resolve config path".to_string())?;

    let mut existing = if path.exists() {
        read_entries(&path).map_err(|e| e.to_string())?
    } else {
        BTreeMap::new()
    };

    for k in &args.remove {
        existing.remove(k);
    }
    for (k, v) in args.set {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            existing.remove(&k);
        } else {
            existing.insert(k, trimmed.to_string());
        }
    }

    write_entries_atomic(&path, &existing).map_err(|e| e.to_string())?;
    // Re-read via the shared AppConfig loader to invalidate any
    // cached state clients might hold.
    let _ = AppConfig::load();
    Ok(path.to_string_lossy().into_owned())
}

/// Show first-run info: whether the user has already completed
/// onboarding, and the minimum keys required for a working session.
#[derive(Debug, Serialize, Clone)]
pub struct OnboardingStatus {
    pub config_path: Option<String>,
    pub config_exists: bool,
    pub has_any_provider_key: bool,
    pub missing: Vec<String>,
}

#[tauri::command]
pub fn onboarding_status() -> Result<OnboardingStatus, String> {
    let cfg = AppConfig::load().map_err(|e| e.to_string())?;
    let path = resolve_default_path();
    let exists = path.as_ref().is_some_and(|p| p.exists());

    let has_any = ["AZURE_OPENAI_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY"]
        .iter()
        .any(|k| cfg.env_or(k).is_some());

    let mut missing = Vec::new();
    if !has_any {
        missing.push("provider api key".to_string());
    }
    if cfg.env_or("THINKINGROOT_WORKSPACE").is_none() {
        missing.push("workspace path".to_string());
    }

    Ok(OnboardingStatus {
        config_path: path.map(|p| p.to_string_lossy().into_owned()),
        config_exists: exists,
        has_any_provider_key: has_any,
        missing,
    })
}

// ────────────────────────────────────────────────────────────────────

fn resolve_default_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("THINKINGROOT_DESKTOP_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
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

fn read_entries(path: &PathBuf) -> anyhow::Result<BTreeMap<String, String>> {
    let raw = std::fs::read_to_string(path)?;
    let parsed: BTreeMap<String, Value> = toml::from_str(&raw)?;
    Ok(parsed
        .into_iter()
        .filter_map(|(k, v)| match v {
            Value::String(s) => Some((k, s)),
            Value::Integer(i) => Some((k, i.to_string())),
            Value::Float(f) => Some((k, f.to_string())),
            Value::Boolean(b) => Some((k, b.to_string())),
            _ => None,
        })
        .collect())
}

fn write_entries_atomic(
    path: &PathBuf,
    entries: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialisable: BTreeMap<&str, Value> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), Value::String(v.clone())))
        .collect();
    let body = toml::to_string_pretty(&serialisable)?;

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn mask(value: &str) -> String {
    let cleaned: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() <= 4 {
        return "****".to_string();
    }
    let prefix = &cleaned[..4];
    format!("{prefix}…({} chars)", cleaned.len())
}
