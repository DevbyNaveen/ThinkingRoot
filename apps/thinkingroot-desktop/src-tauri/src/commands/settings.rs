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
// Workspace-scoped LLM config — what the engine actually uses
// ────────────────────────────────────────────────────────────────────

/// Real LLM configuration for a workspace, projected from its
/// `.thinkingroot/config.toml`. The settings page renders these values
/// directly so users see the actual provider / endpoint / deployment
/// the engine is wired to — no hardcoded placeholders.
#[derive(Debug, Serialize, Clone, Default)]
pub struct WorkspaceLlmConfig {
    /// Workspace path (filesystem absolute).
    pub workspace_path: Option<String>,
    /// Workspace registry name.
    pub workspace_name: Option<String>,
    /// Default provider (e.g. "azure", "anthropic", "openai", "bedrock").
    pub provider: Option<String>,
    /// Display model name (e.g. "gpt-5.4", "claude-sonnet-4-5").
    pub extraction_model: Option<String>,
    pub compilation_model: Option<String>,
    /// Azure-specific.
    pub azure_resource_name: Option<String>,
    pub azure_endpoint_base: Option<String>,
    pub azure_deployment: Option<String>,
    pub azure_api_version: Option<String>,
    pub azure_api_key_env: Option<String>,
    /// Whether the resolved api_key env var is currently set in this
    /// process — surfaces "AZURE_OPENAI_API_KEY missing" cases without
    /// leaking the value itself.
    pub azure_api_key_env_present: bool,
    /// Existence check for the workspace `config.toml` file. False when
    /// the path doesn't have a `.thinkingroot/config.toml` yet.
    pub config_exists: bool,
}

#[tauri::command]
pub fn workspace_llm_config(workspace_path: String) -> Result<WorkspaceLlmConfig, String> {
    let root = PathBuf::from(&workspace_path);
    let cfg_path = root.join(".thinkingroot").join("config.toml");

    if !cfg_path.exists() {
        return Ok(WorkspaceLlmConfig {
            workspace_path: Some(workspace_path),
            config_exists: false,
            ..Default::default()
        });
    }

    let raw = std::fs::read_to_string(&cfg_path).map_err(|e| e.to_string())?;
    let parsed: toml::Value = toml::from_str(&raw).map_err(|e| e.to_string())?;

    let llm = parsed.get("llm");
    let provider = llm
        .and_then(|t| t.get("default_provider"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let extraction_model = llm
        .and_then(|t| t.get("extraction_model"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let compilation_model = llm
        .and_then(|t| t.get("compilation_model"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let azure = llm
        .and_then(|t| t.get("providers"))
        .and_then(|t| t.get("azure"));
    let azure_resource_name = azure
        .and_then(|t| t.get("resource_name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let azure_endpoint_base = azure
        .and_then(|t| t.get("endpoint_base"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let azure_deployment = azure
        .and_then(|t| t.get("deployment"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let azure_api_version = azure
        .and_then(|t| t.get("api_version"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let azure_api_key_env = azure
        .and_then(|t| t.get("api_key_env"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let azure_api_key_env_present = azure_api_key_env
        .as_deref()
        .or(Some("AZURE_OPENAI_API_KEY"))
        .and_then(|name| std::env::var(name).ok())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    Ok(WorkspaceLlmConfig {
        workspace_path: Some(workspace_path),
        workspace_name: None,
        provider,
        extraction_model,
        compilation_model,
        azure_resource_name,
        azure_endpoint_base,
        azure_deployment,
        azure_api_version,
        azure_api_key_env,
        azure_api_key_env_present,
        config_exists: true,
    })
}

/// Patch a workspace's `.thinkingroot/config.toml` provider block.
/// Empty string in any field = "remove this key". The patch is keyed
/// by workspace path so the user can edit any mounted workspace,
/// not just the active one.
#[derive(Debug, Deserialize)]
pub struct WorkspaceLlmWriteArgs {
    pub workspace_path: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub extraction_model: Option<String>,
    #[serde(default)]
    pub compilation_model: Option<String>,
    // Azure
    #[serde(default)]
    pub azure_resource_name: Option<String>,
    #[serde(default)]
    pub azure_endpoint_base: Option<String>,
    #[serde(default)]
    pub azure_deployment: Option<String>,
    #[serde(default)]
    pub azure_api_version: Option<String>,
    #[serde(default)]
    pub azure_api_key_env: Option<String>,
}

#[tauri::command]
pub fn workspace_llm_write(args: WorkspaceLlmWriteArgs) -> Result<String, String> {
    let root = PathBuf::from(&args.workspace_path);
    let cfg_path = root.join(".thinkingroot").join("config.toml");

    let mut doc: toml::Table = if cfg_path.exists() {
        let raw = std::fs::read_to_string(&cfg_path).map_err(|e| e.to_string())?;
        toml::from_str(&raw).map_err(|e| e.to_string())?
    } else {
        toml::Table::new()
    };

    // Helper: ensure a nested table exists; return a mutable handle.
    fn ensure_table<'a>(parent: &'a mut toml::Table, key: &str) -> &'a mut toml::Table {
        let entry = parent
            .entry(key.to_string())
            .or_insert_with(|| Value::Table(toml::Table::new()));
        if let Value::Table(t) = entry {
            t
        } else {
            *entry = Value::Table(toml::Table::new());
            if let Value::Table(t) = entry {
                t
            } else {
                unreachable!("just inserted a Table")
            }
        }
    }

    fn set_or_remove(table: &mut toml::Table, key: &str, val: &Option<String>) {
        match val {
            Some(s) if !s.trim().is_empty() => {
                table.insert(key.to_string(), Value::String(s.trim().to_string()));
            }
            Some(_) => {
                table.remove(key);
            }
            None => {}
        }
    }

    {
        let llm = ensure_table(&mut doc, "llm");
        set_or_remove(llm, "default_provider", &args.provider);
        set_or_remove(llm, "extraction_model", &args.extraction_model);
        set_or_remove(llm, "compilation_model", &args.compilation_model);
    }

    {
        let llm = ensure_table(&mut doc, "llm");
        let providers = ensure_table(llm, "providers");
        let azure = ensure_table(providers, "azure");
        set_or_remove(azure, "resource_name", &args.azure_resource_name);
        set_or_remove(azure, "endpoint_base", &args.azure_endpoint_base);
        set_or_remove(azure, "deployment", &args.azure_deployment);
        set_or_remove(azure, "api_version", &args.azure_api_version);
        set_or_remove(azure, "api_key_env", &args.azure_api_key_env);
    }

    let serialized = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = cfg_path.with_extension("toml.tmp");
    std::fs::write(&tmp, serialized).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &cfg_path).map_err(|e| e.to_string())?;
    Ok(cfg_path.to_string_lossy().into_owned())
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
