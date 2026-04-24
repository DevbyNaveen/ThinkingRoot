use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::LlmConfig;
use crate::error::{Error, Result};

/// Global ThinkingRoot configuration stored at the OS config directory:
/// - macOS: `~/Library/Application Support/thinkingroot/config.toml`
/// - Linux: `~/.config/thinkingroot/config.toml`
/// - Windows: `%APPDATA%\thinkingroot\config.toml`
///
/// Provides defaults for all workspaces. No per-workspace config.toml is required —
/// `root init` only creates the `.thinkingroot/` directory and inherits everything from here.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub llm: LlmConfig,

    #[serde(default)]
    pub serve: ServeConfig,
}

impl GlobalConfig {
    /// Returns the path to the global config file, or `None` if the config dir cannot be resolved.
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("thinkingroot").join("config.toml"))
    }

    /// Returns the path to the credentials file (stored separately from config.toml
    /// so the main config can be safely inspected/shared without leaking keys).
    pub fn credentials_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("thinkingroot").join("credentials.toml"))
    }

    /// Load the global config from the OS config directory (see struct doc).
    /// Returns `Ok(Default::default())` if the file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::path()
            .ok_or_else(|| Error::MissingConfig("cannot resolve config directory".into()))?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
        let mut config: GlobalConfig = toml::from_str(&content)?;
        // Merge stored credentials into the in-memory config so that callers that
        // read config.llm.providers.X.api_key see the value without needing to
        // separately load credentials.
        if let Ok(creds) = Credentials::load() {
            creds.inject_into(&mut config.llm);
        }
        Ok(config)
    }

    /// Save the global config to the OS config directory (see struct doc).
    /// Creates the directory if it does not exist.
    /// API key values are stripped from the config before writing so that
    /// config.toml never contains plaintext credentials.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()
            .ok_or_else(|| Error::MissingConfig("cannot resolve config directory".into()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io_path(parent, e))?;
        }
        // Write config without embedded key values — keys go to credentials.toml only.
        let config_to_write = self.without_keys();
        let content = toml::to_string_pretty(&config_to_write)?;
        std::fs::write(&path, content).map_err(|e| Error::io_path(&path, e))?;
        Ok(())
    }

    /// Return a copy of self with all `api_key` fields cleared so they are
    /// never serialized into the main config.toml.
    fn without_keys(&self) -> Self {
        let mut c = self.clone();
        let p = &mut c.llm.providers;
        if let Some(ref mut cfg) = p.openai {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.anthropic {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.groq {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.deepseek {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.openrouter {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.together {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.perplexity {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.litellm {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.custom {
            cfg.api_key = None;
        }
        if let Some(ref mut cfg) = p.azure {
            cfg.api_key = None;
        }
        c
    }
}

// ── Credentials file ─────────────────────────────────────────────
//
// Stored at `~/.config/thinkingroot/credentials.toml` with mode 0600.
// Flat map of env-var-name → value, matching what `ProviderConfig.api_key_env`
// names so that injection is O(n) with no provider-specific logic.
//
// Example file:
//   OPENAI_API_KEY = "sk-..."
//   GROQ_API_KEY   = "gsk_..."

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Credentials {
    /// Map from env var name (e.g. "OPENAI_API_KEY") to its value.
    #[serde(flatten)]
    pub keys: HashMap<String, String>,
}

impl Credentials {
    /// Load credentials from disk. Returns an empty map if the file does not exist.
    pub fn load() -> Result<Self> {
        let path = GlobalConfig::credentials_path()
            .ok_or_else(|| Error::MissingConfig("cannot resolve config directory".into()))?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
        let creds: Credentials = toml::from_str(&content)?;
        Ok(creds)
    }

    /// Persist credentials to disk with restrictive file permissions (0600 on Unix).
    pub fn save(&self) -> Result<()> {
        let path = GlobalConfig::credentials_path()
            .ok_or_else(|| Error::MissingConfig("cannot resolve config directory".into()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io_path(parent, e))?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, &content).map_err(|e| Error::io_path(&path, e))?;
        // Set 0600 permissions on Unix so only the owner can read the file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&path, perms).map_err(|e| Error::io_path(&path, e))?;
        }
        Ok(())
    }

    /// Set or update a single credential by its env var name.
    pub fn set(&mut self, env_var: &str, value: &str) {
        self.keys.insert(env_var.to_string(), value.to_string());
    }

    /// Remove a single credential.
    pub fn remove(&mut self, env_var: &str) {
        self.keys.remove(env_var);
    }

    /// Return the stored value for an env var name, if present.
    pub fn get(&self, env_var: &str) -> Option<&str> {
        self.keys.get(env_var).map(String::as_str)
    }

    /// Inject stored values into provider config `api_key` fields so that
    /// `resolve_key` can find them without reading the credentials file again.
    pub fn inject_into(&self, llm: &mut crate::config::LlmConfig) {
        let p = &mut llm.providers;
        inject_provider(&mut p.openai, "OPENAI_API_KEY", &self.keys);
        inject_provider(&mut p.anthropic, "ANTHROPIC_API_KEY", &self.keys);
        inject_provider(&mut p.groq, "GROQ_API_KEY", &self.keys);
        inject_provider(&mut p.deepseek, "DEEPSEEK_API_KEY", &self.keys);
        inject_provider(&mut p.openrouter, "OPENROUTER_API_KEY", &self.keys);
        inject_provider(&mut p.together, "TOGETHER_API_KEY", &self.keys);
        inject_provider(&mut p.perplexity, "PERPLEXITY_API_KEY", &self.keys);
        inject_provider(&mut p.litellm, "LITELLM_API_KEY", &self.keys);
        inject_provider(&mut p.custom, "CUSTOM_LLM_API_KEY", &self.keys);
        // Azure uses a different struct type but the same pattern.
        if let Some(ref mut az) = p.azure {
            let env_var = az.api_key_env.as_deref().unwrap_or("AZURE_OPENAI_API_KEY");
            if az.api_key.is_none() {
                az.api_key = self.keys.get(env_var).cloned();
            }
        }
    }

    /// Expose credentials as env-var-name → value pairs for injecting into
    /// MCP stdio subprocess configs. Returns only the entries that match the
    /// known CREDENTIAL_VARS list so unrelated keys are never forwarded.
    pub fn as_env_map(&self) -> HashMap<String, String> {
        self.keys.clone()
    }
}

fn inject_provider(
    slot: &mut Option<crate::config::ProviderConfig>,
    default_env: &str,
    keys: &HashMap<String, String>,
) {
    if let Some(cfg) = slot
        && cfg.api_key.is_none()
    {
        let env_var = cfg.api_key_env.as_deref().unwrap_or(default_env);
        cfg.api_key = keys.get(env_var).cloned();
    }
}

/// Server defaults stored in the global config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeConfig {
    pub default_port: u16,
    pub default_host: String,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            default_port: 3000,
            default_host: "127.0.0.1".to_string(),
        }
    }
}

/// Registry of known workspaces, stored at `~/.config/thinkingroot/workspaces.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspaceRegistry {
    /// TOML key is `workspace` (plural via `Vec`) rendered as `[[workspace]]` array.
    #[serde(default, rename = "workspace")]
    pub workspaces: Vec<WorkspaceEntry>,
}

/// A single registered workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: PathBuf,
    pub port: u16,
}

impl WorkspaceRegistry {
    /// Returns the path to the workspace registry file.
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("thinkingroot").join("workspaces.toml"))
    }

    /// Load the registry. Returns empty registry if file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::path()
            .ok_or_else(|| Error::MissingConfig("cannot resolve config directory".into()))?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
        let registry: WorkspaceRegistry = toml::from_str(&content)?;
        Ok(registry)
    }

    /// Save the registry, creating the config directory if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()
            .ok_or_else(|| Error::MissingConfig("cannot resolve config directory".into()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io_path(parent, e))?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content).map_err(|e| Error::io_path(&path, e))?;
        Ok(())
    }

    /// Add or replace a workspace entry (matched by name).
    pub fn add(&mut self, entry: WorkspaceEntry) {
        self.workspaces.retain(|w| w.name != entry.name);
        self.workspaces.push(entry);
    }

    /// Remove a workspace entry by name. Returns `true` if it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.workspaces.len();
        self.workspaces.retain(|w| w.name != name);
        self.workspaces.len() < before
    }

    /// Next port not already used by any registered workspace, starting at 3000.
    pub fn next_available_port(&self) -> u16 {
        let used: std::collections::HashSet<u16> = self.workspaces.iter().map(|w| w.port).collect();
        let mut port = 3000u16;
        while used.contains(&port) {
            port += 1;
        }
        port
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_registry_add_and_remove() {
        let mut reg = WorkspaceRegistry::default();
        reg.add(WorkspaceEntry {
            name: "notes".to_string(),
            path: std::path::PathBuf::from("/tmp/notes"),
            port: 3000,
        });
        assert_eq!(reg.workspaces.len(), 1);

        reg.add(WorkspaceEntry {
            name: "work".to_string(),
            path: std::path::PathBuf::from("/tmp/work"),
            port: 3001,
        });
        assert_eq!(reg.workspaces.len(), 2);

        // Adding same name replaces
        reg.add(WorkspaceEntry {
            name: "notes".to_string(),
            path: std::path::PathBuf::from("/tmp/notes2"),
            port: 3000,
        });
        assert_eq!(reg.workspaces.len(), 2);
        assert_eq!(
            reg.workspaces[1].path,
            std::path::PathBuf::from("/tmp/notes2")
        );

        assert!(reg.remove("notes"));
        assert_eq!(reg.workspaces.len(), 1);
        assert!(!reg.remove("nonexistent"));
    }

    #[test]
    fn next_available_port_starts_at_3000() {
        let reg = WorkspaceRegistry::default();
        assert_eq!(reg.next_available_port(), 3000);
    }

    #[test]
    fn next_available_port_skips_used() {
        let mut reg = WorkspaceRegistry::default();
        reg.add(WorkspaceEntry {
            name: "a".to_string(),
            path: PathBuf::from("/a"),
            port: 3000,
        });
        reg.add(WorkspaceEntry {
            name: "b".to_string(),
            path: PathBuf::from("/b"),
            port: 3001,
        });
        assert_eq!(reg.next_available_port(), 3002);
    }

    #[test]
    fn global_config_roundtrip_toml() {
        let toml_str = r#"
[llm]
default_provider = "openrouter"
extraction_model = "anthropic/claude-3-haiku"
compilation_model = "anthropic/claude-3-haiku"
max_concurrent_requests = 5
request_timeout_secs = 120

[serve]
default_port = 3000
default_host = "127.0.0.1"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.llm.default_provider, "openrouter");
        assert_eq!(config.serve.default_port, 3000);
        let out = toml::to_string_pretty(&config).unwrap();
        let reparsed: GlobalConfig = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.llm.default_provider, "openrouter");
    }

    #[test]
    fn workspace_registry_roundtrip_toml() {
        let toml_str = r#"
[[workspace]]
name = "notes"
path = "/home/alice/notes"
port = 3000

[[workspace]]
name = "work"
path = "/home/alice/work"
port = 3001
"#;
        let reg: WorkspaceRegistry = toml::from_str(toml_str).unwrap();
        assert_eq!(reg.workspaces.len(), 2);
        assert_eq!(reg.workspaces[0].name, "notes");
        assert_eq!(reg.workspaces[1].port, 3001);
    }
}
