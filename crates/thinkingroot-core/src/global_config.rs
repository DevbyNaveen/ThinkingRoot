use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::LlmConfig;
use crate::error::{Error, Result};

/// Global ThinkingRoot configuration stored at `~/.config/thinkingroot/config.toml`.
/// Provides defaults for all workspaces; per-workspace configs override specific fields.
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

    /// Load the global config from `~/.config/thinkingroot/config.toml`.
    /// Returns `Ok(Default::default())` if the file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::path()
            .ok_or_else(|| Error::MissingConfig("cannot resolve config directory".into()))?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
        let config: GlobalConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// Save the global config to `~/.config/thinkingroot/config.toml`.
    /// Creates the directory if it does not exist.
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
path = "/Users/naveen/notes"
port = 3000

[[workspace]]
name = "work"
path = "/Users/naveen/work"
port = 3001
"#;
        let reg: WorkspaceRegistry = toml::from_str(toml_str).unwrap();
        assert_eq!(reg.workspaces.len(), 2);
        assert_eq!(reg.workspaces[0].name, "notes");
        assert_eq!(reg.workspaces[1].port, 3001);
    }
}
