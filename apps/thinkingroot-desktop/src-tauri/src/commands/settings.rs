//! Config + settings commands — typed, structured, shared with the CLI.
//!
//! Three sources of truth, all under `dirs::config_dir()/thinkingroot/`:
//!
//! * `config.toml`      — engine `GlobalConfig` (provider/model defaults).
//! * `credentials.toml` — engine `Credentials` (provider keys, mode 0600).
//! * `desktop.toml`     — desktop-only state (cloud token, scan roots).
//!
//! Per-workspace LLM overrides keep their existing path
//! (`<workspace>/.thinkingroot/config.toml`) and dedicated commands
//! (`workspace_llm_config` / `workspace_llm_write`).
//!
//! The previous flat-key/value `desktop.toml` (Linux-XDG style on macOS,
//! field names like `AZURE_OPENAI_KEY` that didn't match what the engine
//! actually reads) is gone. Migration is handled in `crate::config`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thinkingroot_core::{Credentials, GlobalConfig, WorkspaceRegistry};
use toml::Value;

// ────────────────────────────────────────────────────────────────────
// Global LLM config (shared with the CLI)
// ────────────────────────────────────────────────────────────────────

/// Config file paths we expose to the Settings UI so users can audit
/// where their data lives. Empty strings if the OS config dir isn't
/// resolvable (no `$HOME` etc.).
#[derive(Debug, Serialize, Clone, Default)]
pub struct ConfigPaths {
    pub config_path: Option<String>,
    pub credentials_path: Option<String>,
    pub workspaces_path: Option<String>,
    pub desktop_path: Option<String>,
}

#[tauri::command]
pub fn config_paths() -> Result<ConfigPaths, String> {
    Ok(ConfigPaths {
        config_path: GlobalConfig::path().map(|p| p.display().to_string()),
        credentials_path: GlobalConfig::credentials_path().map(|p| p.display().to_string()),
        workspaces_path: WorkspaceRegistry::path().map(|p| p.display().to_string()),
        desktop_path: crate::config::DesktopState::path().map(|p| p.display().to_string()),
    })
}

/// Projection of the global LLM config the Settings UI renders.
/// Provider keys are intentionally NOT included — `credentials_status`
/// is the right command for "is this provider keyed?".
#[derive(Debug, Serialize, Clone, Default)]
pub struct GlobalLlmConfig {
    pub default_provider: String,
    pub extraction_model: String,
    pub compilation_model: String,
    pub max_concurrent_requests: usize,
    pub request_timeout_secs: u64,
    pub azure: AzureProviderView,
    /// All known providers keyed by name, with their non-secret fields.
    /// Surfaced separately from the dedicated `azure` entry because the
    /// UI renders one provider at a time and "azure" has the richest
    /// editor.
    pub providers: BTreeMap<String, GenericProviderView>,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct AzureProviderView {
    pub configured: bool,
    pub resource_name: Option<String>,
    pub endpoint_base: Option<String>,
    pub deployment: Option<String>,
    pub api_version: Option<String>,
    pub api_key_env: Option<String>,
    /// True when the resolved env var is currently set in the desktop
    /// process. Surfacing this lets the UI explain *why* a chat request
    /// would fail before the user sends it.
    pub api_key_env_present: bool,
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct GenericProviderView {
    pub configured: bool,
    pub api_key_env: Option<String>,
    pub api_key_env_present: bool,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
}

#[tauri::command]
pub fn global_config_read() -> Result<GlobalLlmConfig, String> {
    let cfg = GlobalConfig::load().map_err(|e| e.to_string())?;
    let providers = &cfg.llm.providers;

    let azure = providers
        .azure
        .as_ref()
        .map(|az| AzureProviderView {
            configured: true,
            resource_name: az.resource_name.clone(),
            endpoint_base: az.endpoint_base.clone(),
            deployment: az.deployment.clone(),
            api_version: az.api_version.clone(),
            api_key_env: az.api_key_env.clone(),
            api_key_env_present: env_present(
                az.api_key_env.as_deref().unwrap_or("AZURE_OPENAI_API_KEY"),
            ),
        })
        .unwrap_or_default();

    let mut generic = BTreeMap::new();
    macro_rules! generic_provider {
        ($name:literal, $field:ident, $default_env:literal) => {
            if let Some(p) = providers.$field.as_ref() {
                generic.insert(
                    $name.to_string(),
                    GenericProviderView {
                        configured: true,
                        api_key_env: p.api_key_env.clone(),
                        api_key_env_present: env_present(
                            p.api_key_env.as_deref().unwrap_or($default_env),
                        ),
                        base_url: p.base_url.clone(),
                        default_model: p.default_model.clone(),
                    },
                );
            }
        };
    }
    generic_provider!("openai", openai, "OPENAI_API_KEY");
    generic_provider!("anthropic", anthropic, "ANTHROPIC_API_KEY");
    generic_provider!("groq", groq, "GROQ_API_KEY");
    generic_provider!("deepseek", deepseek, "DEEPSEEK_API_KEY");
    generic_provider!("openrouter", openrouter, "OPENROUTER_API_KEY");
    generic_provider!("together", together, "TOGETHER_API_KEY");
    generic_provider!("perplexity", perplexity, "PERPLEXITY_API_KEY");
    generic_provider!("litellm", litellm, "LITELLM_API_KEY");
    generic_provider!("custom", custom, "CUSTOM_LLM_API_KEY");
    generic_provider!("ollama", ollama, "OLLAMA_API_KEY");

    Ok(GlobalLlmConfig {
        default_provider: cfg.llm.default_provider,
        extraction_model: cfg.llm.extraction_model,
        compilation_model: cfg.llm.compilation_model,
        max_concurrent_requests: cfg.llm.max_concurrent_requests,
        request_timeout_secs: cfg.llm.request_timeout_secs,
        azure,
        providers: generic,
    })
}

/// Patch shape for the global config. Only the LLM section is exposed —
/// serve port and other engine concerns are kept off-screen so users
/// can't accidentally break their CLI from the desktop UI.
#[derive(Debug, Deserialize, Default)]
pub struct GlobalLlmWriteArgs {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub extraction_model: Option<String>,
    #[serde(default)]
    pub compilation_model: Option<String>,
    #[serde(default)]
    pub max_concurrent_requests: Option<usize>,
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
    /// Azure provider patch. Empty strings remove the field; absent
    /// fields are left untouched.
    #[serde(default)]
    pub azure: Option<AzureProviderPatch>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AzureProviderPatch {
    #[serde(default)]
    pub resource_name: Option<String>,
    #[serde(default)]
    pub endpoint_base: Option<String>,
    #[serde(default)]
    pub deployment: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[tauri::command]
pub fn global_config_write(args: GlobalLlmWriteArgs) -> Result<String, String> {
    let mut cfg = GlobalConfig::load().map_err(|e| e.to_string())?;

    if let Some(v) = args.default_provider {
        cfg.llm.default_provider = v.trim().to_string();
    }
    if let Some(v) = args.extraction_model {
        cfg.llm.extraction_model = v.trim().to_string();
    }
    if let Some(v) = args.compilation_model {
        cfg.llm.compilation_model = v.trim().to_string();
    }
    if let Some(v) = args.max_concurrent_requests {
        cfg.llm.max_concurrent_requests = v;
    }
    if let Some(v) = args.request_timeout_secs {
        cfg.llm.request_timeout_secs = v;
    }

    if let Some(patch) = args.azure {
        let azure = cfg
            .llm
            .providers
            .azure
            .get_or_insert_with(|| thinkingroot_core::config::AzureConfig {
                resource_name: None,
                endpoint_base: None,
                deployment: None,
                api_version: None,
                api_key_env: Some("AZURE_OPENAI_API_KEY".to_string()),
                api_key: None,
            });
        apply_optional(&mut azure.resource_name, patch.resource_name);
        apply_optional(&mut azure.endpoint_base, patch.endpoint_base);
        apply_optional(&mut azure.deployment, patch.deployment);
        apply_optional(&mut azure.api_version, patch.api_version);
        apply_optional(&mut azure.api_key_env, patch.api_key_env);
    }

    cfg.save().map_err(|e| e.to_string())?;
    Ok(GlobalConfig::path()
        .map(|p| p.display().to_string())
        .unwrap_or_default())
}

// ────────────────────────────────────────────────────────────────────
// Credentials (provider keys)
// ────────────────────────────────────────────────────────────────────

/// One row per known credential. Returned as a list rather than a flat
/// map so the UI gets a stable order and can render "set / not set"
/// indicators without the value ever crossing the IPC boundary.
#[derive(Debug, Serialize, Clone)]
pub struct CredentialRow {
    pub env_var: String,
    /// True when persisted in `credentials.toml`.
    pub persisted: bool,
    /// True when also live in the current process env. The desktop
    /// inherits its env from the user's shell, so a value can be
    /// "in env, not in file" (preferred for ephemeral sessions) or
    /// "in file, not in env" (preferred for installed apps that the
    /// sidecar inherits credentials from).
    pub in_process_env: bool,
}

/// Stable list of credential env var names the desktop / engine know.
/// Adding a new provider in the engine? Add its canonical env-var name
/// here so the Settings UI exposes it.
const CREDENTIAL_VARS: &[&str] = &[
    "AZURE_OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "PERPLEXITY_API_KEY",
    "LITELLM_API_KEY",
    "CUSTOM_LLM_API_KEY",
];

#[tauri::command]
pub fn credentials_status() -> Result<Vec<CredentialRow>, String> {
    let creds = Credentials::load().map_err(|e| e.to_string())?;
    Ok(CREDENTIAL_VARS
        .iter()
        .map(|name| CredentialRow {
            env_var: (*name).to_string(),
            persisted: creds.get(name).is_some(),
            in_process_env: env_present(name),
        })
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct CredentialSetArgs {
    pub env_var: String,
    pub value: String,
}

#[tauri::command]
pub fn credentials_set(args: CredentialSetArgs) -> Result<(), String> {
    if !CREDENTIAL_VARS.contains(&args.env_var.as_str()) {
        return Err(format!(
            "unknown credential `{}` — refuse to write arbitrary keys",
            args.env_var
        ));
    }
    let trimmed = args.value.trim();
    let mut creds = Credentials::load().map_err(|e| e.to_string())?;
    if trimmed.is_empty() {
        creds.remove(&args.env_var);
    } else {
        creds.set(&args.env_var, trimmed);
    }
    creds.save().map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct CredentialRemoveArgs {
    pub env_var: String,
}

#[tauri::command]
pub fn credentials_remove(args: CredentialRemoveArgs) -> Result<(), String> {
    let mut creds = Credentials::load().map_err(|e| e.to_string())?;
    creds.remove(&args.env_var);
    creds.save().map_err(|e| e.to_string())?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// Onboarding status
// ────────────────────────────────────────────────────────────────────

/// First-run summary the wizard reads: do we have at least one provider
/// keyed, do we have a workspace registered + selected, and where do
/// these files live so the UI can show the user.
#[derive(Debug, Serialize, Clone)]
pub struct OnboardingStatus {
    pub paths: ConfigPaths,
    pub has_any_provider_key: bool,
    pub workspace_count: usize,
    pub active_workspace: Option<String>,
    pub missing: Vec<String>,
}

#[tauri::command]
pub fn onboarding_status() -> Result<OnboardingStatus, String> {
    let creds = Credentials::load().map_err(|e| e.to_string())?;
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;

    let has_any_provider_key = CREDENTIAL_VARS
        .iter()
        .any(|name| creds.get(name).is_some() || env_present(name));

    let mut missing = Vec::new();
    if !has_any_provider_key {
        missing.push("provider api key".to_string());
    }
    if registry.workspaces.is_empty() {
        missing.push("workspace".to_string());
    } else if registry.active.is_none() {
        missing.push("active workspace".to_string());
    }

    Ok(OnboardingStatus {
        paths: config_paths()?,
        has_any_provider_key,
        workspace_count: registry.workspaces.len(),
        active_workspace: registry.active,
        missing,
    })
}

// ────────────────────────────────────────────────────────────────────
// Workspace-scoped LLM config (per-workspace `.thinkingroot/config.toml`)
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

    let azure_api_key_env_present = env_present(
        azure_api_key_env
            .as_deref()
            .unwrap_or("AZURE_OPENAI_API_KEY"),
    );

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
// Helpers
// ────────────────────────────────────────────────────────────────────

fn env_present(name: &str) -> bool {
    std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false)
}

fn apply_optional(slot: &mut Option<String>, patch: Option<String>) {
    if let Some(v) = patch {
        let trimmed = v.trim();
        *slot = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_vars_use_canonical_env_naming() {
        // Drift guard: every credential we surface to the UI should
        // match the `*_API_KEY` convention the engine looks up. If a
        // future provider needs a non-`_API_KEY` env name, allow-list
        // it explicitly here so the change is reviewed.
        for name in CREDENTIAL_VARS {
            assert!(
                name.ends_with("_API_KEY"),
                "non-canonical credential env var: {name}"
            );
        }
    }
}
