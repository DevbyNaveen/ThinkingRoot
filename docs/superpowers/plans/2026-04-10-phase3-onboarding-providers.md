# Phase 3 — Onboarding, Provider Expansion & Multi-Tool Connection

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add OpenRouter/Together/Perplexity/LiteLLM/custom LLM providers, a git-style global config hierarchy, `root setup` wizard, `root connect` MCP auto-wiring for 7 AI tools, `root workspace` registry, and `root serve --install-service`.

**Architecture:** Global config at `~/.config/thinkingroot/config.toml` provides LLM defaults; per-workspace `.thinkingroot/config.toml` overrides. All new providers reuse the existing `OpenAiProvider` struct. New CLI commands (`setup`, `connect`, `workspace`) are each isolated source files calling shared helpers.

**Tech Stack:** Rust, Clap 4 (derive), dialoguer 0.11, dirs 5, serde_json (merge), reqwest (key validation), indicatif 0.17 (already present), tokio (already present).

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `Cargo.toml` | Modify | Add `dirs`, `dialoguer` to `[workspace.dependencies]` |
| `crates/thinkingroot-core/Cargo.toml` | Modify | Add `dirs` dep |
| `crates/thinkingroot-cli/Cargo.toml` | Modify | Add `dirs`, `dialoguer` deps |
| `crates/thinkingroot-core/src/config.rs` | Modify | Add 5 provider fields; add `Config::load_merged()` |
| `crates/thinkingroot-core/src/global_config.rs` | **Create** | `GlobalConfig`, `ServeConfig`, `WorkspaceRegistry`, `WorkspaceEntry` |
| `crates/thinkingroot-core/src/lib.rs` | Modify | Export `global_config` module |
| `crates/thinkingroot-extract/src/llm.rs` | Modify | `resolve_key` helpers; 5 new match arms |
| `crates/thinkingroot-cli/src/workspace.rs` | **Create** | Registry CRUD, `run_workspace_*` functions |
| `crates/thinkingroot-cli/src/mcp_config.rs` | **Create** | Tool detection, config write/remove/dry-run |
| `crates/thinkingroot-cli/src/serve.rs` | Modify | Registry fallback, `--name`, `--install-service` |
| `crates/thinkingroot-cli/src/setup.rs` | **Create** | 5-step interactive wizard |
| `crates/thinkingroot-cli/src/main.rs` | Modify | `Setup`, `Connect`, `Workspace` commands; update `Serve` flags |

---

## Task 1: Add `dirs` and `dialoguer` workspace deps

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/thinkingroot-core/Cargo.toml`
- Modify: `crates/thinkingroot-cli/Cargo.toml`

- [ ] **Step 1: Add to workspace deps**

In `Cargo.toml`, inside `[workspace.dependencies]` after the `console` line:

```toml
dirs = "5"
dialoguer = "0.11"
```

- [ ] **Step 2: Add to thinkingroot-core**

In `crates/thinkingroot-core/Cargo.toml`, add after `tracing`:

```toml
dirs = { workspace = true }
```

- [ ] **Step 3: Add to thinkingroot-cli**

In `crates/thinkingroot-cli/Cargo.toml`, add after `anyhow`:

```toml
dirs = { workspace = true }
dialoguer = { workspace = true }
```

- [ ] **Step 4: Verify**

```bash
cargo check -p thinkingroot-core --no-default-features
cargo check -p thinkingroot-cli --no-default-features
```

Expected: no errors.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/thinkingroot-core/Cargo.toml crates/thinkingroot-cli/Cargo.toml
git commit -m "chore: add dirs and dialoguer workspace deps"
```

---

## Task 2: Extend `ProvidersConfig` with 5 new providers

**Files:**
- Modify: `crates/thinkingroot-core/src/config.rs`

- [ ] **Step 1: Write the failing test**

In `crates/thinkingroot-core/src/config.rs`, inside `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn new_providers_roundtrip_toml() {
    let toml = r#"
[llm]
default_provider = "openrouter"
extraction_model = "anthropic/claude-3-haiku"
compilation_model = "anthropic/claude-3-haiku"
max_concurrent_requests = 5
request_timeout_secs = 120

[llm.providers.openrouter]
api_key_env = "OPENROUTER_API_KEY"

[llm.providers.together]
api_key_env = "TOGETHER_API_KEY"

[llm.providers.perplexity]
api_key_env = "PERPLEXITY_API_KEY"

[llm.providers.litellm]
base_url = "http://localhost:4000"

[llm.providers.custom]
api_key_env = "CUSTOM_LLM_API_KEY"
base_url = "https://my-endpoint.com/v1"
"#;
    let config: Config = toml::from_str(toml).unwrap();
    assert_eq!(config.llm.default_provider, "openrouter");
    assert_eq!(
        config.llm.providers.openrouter.as_ref().unwrap().api_key_env.as_deref(),
        Some("OPENROUTER_API_KEY")
    );
    assert_eq!(
        config.llm.providers.together.as_ref().unwrap().api_key_env.as_deref(),
        Some("TOGETHER_API_KEY")
    );
    assert_eq!(
        config.llm.providers.perplexity.as_ref().unwrap().api_key_env.as_deref(),
        Some("PERPLEXITY_API_KEY")
    );
    assert_eq!(
        config.llm.providers.litellm.as_ref().unwrap().base_url.as_deref(),
        Some("http://localhost:4000")
    );
    assert_eq!(
        config.llm.providers.custom.as_ref().unwrap().base_url.as_deref(),
        Some("https://my-endpoint.com/v1")
    );
}
```

- [ ] **Step 2: Run test — expect FAIL**

```bash
cargo test -p thinkingroot-core --no-default-features new_providers_roundtrip_toml
```

Expected: FAIL — `ProvidersConfig` has no fields `openrouter`, `together`, etc.

- [ ] **Step 3: Add 5 fields to `ProvidersConfig`**

Replace the `ProvidersConfig` struct in `config.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    pub bedrock:    Option<BedrockConfig>,
    pub openai:     Option<ProviderConfig>,
    pub anthropic:  Option<ProviderConfig>,
    pub ollama:     Option<ProviderConfig>,
    pub groq:       Option<ProviderConfig>,
    pub deepseek:   Option<ProviderConfig>,
    pub openrouter: Option<ProviderConfig>,
    pub together:   Option<ProviderConfig>,
    pub perplexity: Option<ProviderConfig>,
    pub litellm:    Option<ProviderConfig>,
    pub custom:     Option<ProviderConfig>,
}
```

- [ ] **Step 4: Run test — expect PASS**

```bash
cargo test -p thinkingroot-core --no-default-features new_providers_roundtrip_toml
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-core/src/config.rs
git commit -m "feat(core): add openrouter/together/perplexity/litellm/custom provider fields"
```

---

## Task 3: `resolve_key` helpers + 5 new match arms in `llm.rs`

**Files:**
- Modify: `crates/thinkingroot-extract/src/llm.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/thinkingroot-extract/src/llm.rs`, inside the `#[cfg(test)] mod tests` block, add:

```rust
#[test]
fn resolve_key_uses_default_env_when_config_is_none() {
    std::env::set_var("TEST_DEFAULT_KEY", "mykey");
    let result = resolve_key(None, "TEST_DEFAULT_KEY").unwrap();
    assert_eq!(result, "mykey");
    std::env::remove_var("TEST_DEFAULT_KEY");
}

#[test]
fn resolve_key_uses_config_env_when_set() {
    std::env::set_var("MY_CUSTOM_ENV", "customkey");
    let cfg = thinkingroot_core::config::ProviderConfig {
        api_key_env: Some("MY_CUSTOM_ENV".to_string()),
        base_url: None,
        default_model: None,
    };
    let result = resolve_key(Some(&cfg), "IGNORED_DEFAULT").unwrap();
    assert_eq!(result, "customkey");
    std::env::remove_var("MY_CUSTOM_ENV");
}

#[test]
fn resolve_base_url_returns_default_when_config_has_none() {
    let result = resolve_base_url(None, "https://default.example.com");
    assert_eq!(result, "https://default.example.com");
}

#[test]
fn resolve_base_url_returns_config_url_when_set() {
    let cfg = thinkingroot_core::config::ProviderConfig {
        api_key_env: None,
        base_url: Some("https://custom.example.com".to_string()),
        default_model: None,
    };
    let result = resolve_base_url(Some(&cfg), "https://default.example.com");
    assert_eq!(result, "https://custom.example.com");
}
```

- [ ] **Step 2: Run tests — expect FAIL**

```bash
cargo test -p thinkingroot-extract --no-default-features resolve_key
cargo test -p thinkingroot-extract --no-default-features resolve_base_url
```

Expected: FAIL — functions not defined.

- [ ] **Step 3: Add helper functions above `LlmClient`**

Add these four functions just before the `// ── LLM Client` section in `llm.rs`:

```rust
// ── Provider config helpers ──────────────────────────────────────

fn resolve_key(cfg: Option<&ProviderConfig>, default_env: &str) -> Result<String> {
    let env_var = cfg
        .and_then(|p| p.api_key_env.as_deref())
        .unwrap_or(default_env);
    std::env::var(env_var).map_err(|_| Error::MissingConfig(
        format!("set the {} environment variable", env_var)
    ))
}

fn resolve_key_optional(cfg: Option<&ProviderConfig>) -> String {
    cfg.and_then(|p| p.api_key_env.as_deref())
        .and_then(|env| std::env::var(env).ok())
        .unwrap_or_default()
}

fn resolve_base_url(cfg: Option<&ProviderConfig>, default: &str) -> String {
    cfg.and_then(|p| p.base_url.as_deref())
        .unwrap_or(default)
        .to_string()
}

fn resolve_base_url_required(cfg: Option<&ProviderConfig>, provider: &str) -> Result<String> {
    cfg.and_then(|p| p.base_url.as_deref())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::MissingConfig(
            format!("set [llm.providers.{provider}].base_url in your config")
        ))
}
```

- [ ] **Step 4: Add `use` import for `ProviderConfig`**

At the top of `llm.rs`, the existing import is `use thinkingroot_core::config::LlmConfig;`. Replace it with:

```rust
use thinkingroot_core::config::{LlmConfig, ProviderConfig};
```

- [ ] **Step 5: Refactor existing arms to use helpers**

In `LlmClient::new()`, replace the existing `"openai"` arm:

```rust
"openai" => {
    let key = resolve_key(config.providers.openai.as_ref(), "OPENAI_API_KEY")?;
    let base_url = resolve_base_url(
        config.providers.openai.as_ref(),
        "https://api.openai.com",
    );
    Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
}
```

Replace the `"anthropic"` arm:

```rust
"anthropic" => {
    let key = resolve_key(config.providers.anthropic.as_ref(), "ANTHROPIC_API_KEY")?;
    Provider::Anthropic(AnthropicProvider::new(&key, &config.extraction_model))
}
```

Replace the `"ollama"` arm:

```rust
"ollama" => {
    let base_url = resolve_base_url(
        config.providers.ollama.as_ref(),
        "http://localhost:11434",
    );
    Provider::Ollama(OllamaProvider::new(&config.extraction_model, &base_url))
}
```

Replace the `"groq"` arm:

```rust
"groq" => {
    let key = resolve_key(config.providers.groq.as_ref(), "GROQ_API_KEY")?;
    Provider::OpenAi(OpenAiProvider::new(
        &key, &config.extraction_model, "https://api.groq.com/openai",
    ))
}
```

Replace the `"deepseek"` arm:

```rust
"deepseek" => {
    let key = resolve_key(config.providers.deepseek.as_ref(), "DEEPSEEK_API_KEY")?;
    Provider::OpenAi(OpenAiProvider::new(
        &key, &config.extraction_model, "https://api.deepseek.com",
    ))
}
```

- [ ] **Step 6: Add 5 new match arms**

Add these 5 arms immediately after the `"deepseek"` arm, before the `other =>` fallback:

```rust
"openrouter" => {
    let key = resolve_key(config.providers.openrouter.as_ref(), "OPENROUTER_API_KEY")?;
    Provider::OpenAi(OpenAiProvider::new(
        &key, &config.extraction_model, "https://openrouter.ai/api/v1",
    ))
}
"together" => {
    let key = resolve_key(config.providers.together.as_ref(), "TOGETHER_API_KEY")?;
    Provider::OpenAi(OpenAiProvider::new(
        &key, &config.extraction_model, "https://api.together.xyz/v1",
    ))
}
"perplexity" => {
    let key = resolve_key(config.providers.perplexity.as_ref(), "PERPLEXITY_API_KEY")?;
    Provider::OpenAi(OpenAiProvider::new(
        &key, &config.extraction_model, "https://api.perplexity.ai",
    ))
}
"litellm" => {
    let key = resolve_key_optional(config.providers.litellm.as_ref());
    let base_url = resolve_base_url(
        config.providers.litellm.as_ref(),
        "http://localhost:4000",
    );
    Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
}
"custom" => {
    let key = resolve_key(config.providers.custom.as_ref(), "CUSTOM_LLM_API_KEY")?;
    let base_url = resolve_base_url_required(config.providers.custom.as_ref(), "custom")?;
    Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
}
```

- [ ] **Step 7: Update the error message for unknown providers**

Replace the `other =>` arm's error message:

```rust
other => {
    return Err(Error::MissingConfig(format!(
        "unsupported provider: {other}. Supported: bedrock, openai, anthropic, ollama, groq, deepseek, openrouter, together, perplexity, litellm, custom"
    )));
}
```

- [ ] **Step 8: Run all tests**

```bash
cargo test -p thinkingroot-extract --no-default-features
```

Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add crates/thinkingroot-extract/src/llm.rs
git commit -m "feat(extract): add openrouter/together/perplexity/litellm/custom providers"
```

---

## Task 4: Create `global_config.rs` in thinkingroot-core

**Files:**
- Create: `crates/thinkingroot-core/src/global_config.rs`
- Modify: `crates/thinkingroot-core/src/lib.rs`

- [ ] **Step 1: Write the failing tests first**

Create `crates/thinkingroot-core/src/global_config.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        assert_eq!(reg.workspaces[1].path, std::path::PathBuf::from("/tmp/notes2"));

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
            path: std::path::PathBuf::from("/a"),
            port: 3000,
        });
        reg.add(WorkspaceEntry {
            name: "b".to_string(),
            path: std::path::PathBuf::from("/b"),
            port: 3001,
        });
        assert_eq!(reg.next_available_port(), 3002);
    }

    #[test]
    fn global_config_roundtrip_toml() {
        let toml = r#"
[llm]
default_provider = "openrouter"
extraction_model = "anthropic/claude-3-haiku"
compilation_model = "anthropic/claude-3-haiku"
max_concurrent_requests = 5
request_timeout_secs = 120

[llm.providers.openrouter]
api_key_env = "OPENROUTER_API_KEY"

[serve]
default_port = 3000
default_host = "127.0.0.1"
"#;
        let config: GlobalConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.llm.default_provider, "openrouter");
        assert_eq!(config.serve.default_port, 3000);
        let out = toml::to_string_pretty(&config).unwrap();
        let reparsed: GlobalConfig = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.llm.default_provider, "openrouter");
    }

    #[test]
    fn workspace_registry_roundtrip_toml() {
        let toml = r#"
[[workspace]]
name = "notes"
path = "/Users/naveen/notes"
port = 3000

[[workspace]]
name = "work"
path = "/Users/naveen/work"
port = 3001
"#;
        let reg: WorkspaceRegistry = toml::from_str(toml).unwrap();
        assert_eq!(reg.workspaces.len(), 2);
        assert_eq!(reg.workspaces[0].name, "notes");
        assert_eq!(reg.workspaces[1].port, 3001);
    }
}
```

- [ ] **Step 2: Run test — expect compile error**

```bash
cargo test -p thinkingroot-core --no-default-features 2>&1 | head -20
```

Expected: compile error — `global_config` module not found (we haven't added the `mod` yet).

- [ ] **Step 3: Write the full `global_config.rs`**

Replace the file with the complete implementation:

```rust
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{LlmConfig, LlmConfig as _};
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
        let content = std::fs::read_to_string(&path)
            .map_err(|e| Error::io_path(&path, e))?;
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
        let content = std::fs::read_to_string(&path)
            .map_err(|e| Error::io_path(&path, e))?;
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
        let used: std::collections::HashSet<u16> =
            self.workspaces.iter().map(|w| w.port).collect();
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

        reg.add(WorkspaceEntry {
            name: "notes".to_string(),
            path: std::path::PathBuf::from("/tmp/notes2"),
            port: 3000,
        });
        assert_eq!(reg.workspaces.len(), 2);
        assert_eq!(reg.workspaces[1].path, std::path::PathBuf::from("/tmp/notes2"));

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
        reg.add(WorkspaceEntry { name: "a".to_string(), path: PathBuf::from("/a"), port: 3000 });
        reg.add(WorkspaceEntry { name: "b".to_string(), path: PathBuf::from("/b"), port: 3001 });
        assert_eq!(reg.next_available_port(), 3002);
    }

    #[test]
    fn global_config_roundtrip_toml() {
        let toml = r#"
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
        let config: GlobalConfig = toml::from_str(toml).unwrap();
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
```

- [ ] **Step 4: Fix the unused import — remove the erroneous `use crate::config::LlmConfig as _`**

The `use crate::config::{LlmConfig, LlmConfig as _};` line is wrong. Replace the imports at the top of the file with:

```rust
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::LlmConfig;
use crate::error::{Error, Result};
```

- [ ] **Step 5: Export from `lib.rs`**

Add to `crates/thinkingroot-core/src/lib.rs`:

```rust
pub mod global_config;
```

And add to the `pub use` section:

```rust
pub use global_config::{GlobalConfig, ServeConfig, WorkspaceEntry, WorkspaceRegistry};
```

- [ ] **Step 6: Run tests — expect PASS**

```bash
cargo test -p thinkingroot-core --no-default-features
```

Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-core/src/global_config.rs crates/thinkingroot-core/src/lib.rs
git commit -m "feat(core): add GlobalConfig, ServeConfig, WorkspaceRegistry, WorkspaceEntry"
```

---

## Task 5: `Config::load_merged()` in `config.rs`

**Files:**
- Modify: `crates/thinkingroot-core/src/config.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `config.rs`:

```rust
#[test]
fn load_merged_uses_global_llm_when_workspace_has_no_llm_section() {
    use crate::global_config::{GlobalConfig, ServeConfig};

    // Build a fake GlobalConfig with openrouter
    let global = GlobalConfig {
        llm: LlmConfig {
            default_provider: "openrouter".to_string(),
            extraction_model: "anthropic/claude-3-haiku".to_string(),
            compilation_model: "anthropic/claude-3-haiku".to_string(),
            max_concurrent_requests: 5,
            request_timeout_secs: 120,
            providers: ProvidersConfig::default(),
        },
        serve: ServeConfig::default(),
    };

    // Workspace config has NO llm section — raw TOML has only [workspace]
    let workspace_toml = r#"
[workspace]
name = "myproject"
"#;

    let merged = Config::merge_with_global(
        toml::from_str(workspace_toml).unwrap(),
        workspace_toml,
        &global,
    );
    assert_eq!(merged.llm.default_provider, "openrouter");
    assert_eq!(merged.workspace.name, Some("myproject".to_string()));
}

#[test]
fn load_merged_workspace_llm_overrides_global() {
    use crate::global_config::{GlobalConfig, ServeConfig};

    let global = GlobalConfig {
        llm: LlmConfig {
            default_provider: "openrouter".to_string(),
            extraction_model: "anthropic/claude-3-haiku".to_string(),
            compilation_model: "anthropic/claude-3-haiku".to_string(),
            max_concurrent_requests: 5,
            request_timeout_secs: 120,
            providers: ProvidersConfig::default(),
        },
        serve: ServeConfig::default(),
    };

    let workspace_toml = r#"
[workspace]
name = "myproject"

[llm]
default_provider = "ollama"
extraction_model = "llama3"
compilation_model = "llama3"
max_concurrent_requests = 2
request_timeout_secs = 60
"#;

    let merged = Config::merge_with_global(
        toml::from_str(workspace_toml).unwrap(),
        workspace_toml,
        &global,
    );
    assert_eq!(merged.llm.default_provider, "ollama");
    assert_eq!(merged.llm.extraction_model, "llama3");
}
```

- [ ] **Step 2: Run — expect FAIL**

```bash
cargo test -p thinkingroot-core --no-default-features load_merged
```

Expected: FAIL — `merge_with_global` not defined.

- [ ] **Step 3: Add `merge_with_global` and `load_merged` to `Config`**

Add these methods to the `impl Config` block in `config.rs`:

```rust
/// Merge a parsed workspace config with the global config.
/// If the raw workspace TOML contains no `[llm]` section, the global LLM config wins.
/// If the workspace has an `[llm]` section, it wins — but individual provider credentials
/// from the global are inherited for any provider slot left as `None`.
pub fn merge_with_global(
    mut workspace: Config,
    raw_toml: &str,
    global: &crate::global_config::GlobalConfig,
) -> Config {
    let has_llm_section = toml::from_str::<toml::Value>(raw_toml)
        .ok()
        .and_then(|v| v.as_table().map(|t| t.contains_key("llm")))
        .unwrap_or(false);

    if !has_llm_section {
        workspace.llm = global.llm.clone();
    } else {
        // Workspace set its own LLM section — inherit individual provider creds from global
        macro_rules! inherit {
            ($field:ident) => {
                if workspace.llm.providers.$field.is_none() {
                    workspace.llm.providers.$field = global.llm.providers.$field.clone();
                }
            };
        }
        inherit!(openai);
        inherit!(anthropic);
        inherit!(ollama);
        inherit!(groq);
        inherit!(deepseek);
        inherit!(openrouter);
        inherit!(together);
        inherit!(perplexity);
        inherit!(litellm);
        inherit!(custom);
    }
    workspace
}

/// Load workspace config merged with global config.
/// Priority: per-workspace `.thinkingroot/config.toml` > global `~/.config/thinkingroot/config.toml` > defaults.
pub fn load_merged(workspace_path: &Path) -> Result<Self> {
    let global = crate::global_config::GlobalConfig::load().unwrap_or_default();
    let config_path = workspace_path.join(".thinkingroot").join("config.toml");

    if !config_path.exists() {
        let mut config = Self::default();
        config.llm = global.llm;
        return Ok(config);
    }

    let raw = std::fs::read_to_string(&config_path)
        .map_err(|e| Error::io_path(&config_path, e))?;
    let workspace: Config = toml::from_str(&raw)?;
    Ok(Self::merge_with_global(workspace, &raw, &global))
}
```

- [ ] **Step 4: Run tests — expect PASS**

```bash
cargo test -p thinkingroot-core --no-default-features
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-core/src/config.rs
git commit -m "feat(core): add Config::load_merged() with global config inheritance"
```

---

## Task 6: `workspace.rs` + `root workspace` command

**Files:**
- Create: `crates/thinkingroot-cli/src/workspace.rs`
- Modify: `crates/thinkingroot-cli/src/main.rs`

- [ ] **Step 1: Write failing test for workspace add/list/remove**

Create `crates/thinkingroot-cli/src/workspace.rs` with test module only:

```rust
#[cfg(test)]
mod tests {
    use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};
    use std::path::PathBuf;

    #[test]
    fn add_workspace_increments_port_automatically() {
        let mut reg = WorkspaceRegistry::default();
        // Simulate what run_workspace_add does
        let port = reg.next_available_port();
        assert_eq!(port, 3000);
        reg.add(WorkspaceEntry {
            name: "first".to_string(),
            path: PathBuf::from("/first"),
            port,
        });
        let port2 = reg.next_available_port();
        assert_eq!(port2, 3001);
    }

    #[test]
    fn remove_nonexistent_workspace_prints_error() {
        let reg = WorkspaceRegistry::default();
        assert!(!reg.workspaces.iter().any(|w| w.name == "ghost"));
    }
}
```

- [ ] **Step 2: Run — expect PASS (tests use core only)**

```bash
cargo test -p thinkingroot-cli --no-default-features workspace
```

Expected: PASS.

- [ ] **Step 3: Write `workspace.rs`**

Replace the file with the full implementation:

```rust
use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};

pub fn run_workspace_add(
    path: PathBuf,
    name: Option<String>,
    port: Option<u16>,
) -> anyhow::Result<()> {
    let abs_path = std::fs::canonicalize(&path)
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut registry = WorkspaceRegistry::load()?;

    let ws_name = name.unwrap_or_else(|| {
        abs_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string())
    });

    let ws_port = port.unwrap_or_else(|| registry.next_available_port());

    registry.add(WorkspaceEntry {
        name: ws_name.clone(),
        path: abs_path.clone(),
        port: ws_port,
    });
    registry.save()?;

    println!();
    println!(
        "  {} workspace \"{}\"",
        style("✓ Registered").green().bold(),
        style(&ws_name).white().bold()
    );
    println!("    Path:  {}", abs_path.display());
    println!("    Port:  {}", ws_port);
    println!(
        "\n  Run {} to compile it.",
        style(format!("root compile {}", abs_path.display())).cyan()
    );
    Ok(())
}

pub fn run_workspace_list() -> anyhow::Result<()> {
    let registry = WorkspaceRegistry::load()?;

    if registry.workspaces.is_empty() {
        println!();
        println!("  No workspaces registered.");
        println!(
            "  Run {} to add one.",
            style("root workspace add <path>").cyan()
        );
        return Ok(());
    }

    println!();
    println!(
        "  {:<20} {:<45} {:<6} {}",
        style("Name").bold(),
        style("Path").bold(),
        style("Port").bold(),
        style("Status").bold()
    );
    println!("  {}", style("─".repeat(80)).dim());

    for ws in &registry.workspaces {
        let data_dir = ws.path.join(".thinkingroot");
        let status = if data_dir.join("graph.db").exists() {
            style("compiled ✓").green().to_string()
        } else {
            style("not compiled").yellow().to_string()
        };
        println!(
            "  {:<20} {:<45} {:<6} {}",
            ws.name,
            ws.path.display(),
            ws.port,
            status
        );
    }
    println!();
    Ok(())
}

pub fn run_workspace_remove(name: &str) -> anyhow::Result<()> {
    let mut registry = WorkspaceRegistry::load()?;

    if !registry.remove(name) {
        anyhow::bail!(
            "workspace \"{}\" not found. Run `root workspace list` to see registered workspaces.",
            name
        );
    }

    registry.save()?;
    println!(
        "  {} workspace \"{}\"",
        style("✓ Removed").green().bold(),
        name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::WorkspaceRegistry;

    #[test]
    fn add_workspace_increments_port_automatically() {
        let mut reg = WorkspaceRegistry::default();
        let port = reg.next_available_port();
        assert_eq!(port, 3000);
        reg.add(WorkspaceEntry {
            name: "first".to_string(),
            path: PathBuf::from("/first"),
            port,
        });
        let port2 = reg.next_available_port();
        assert_eq!(port2, 3001);
    }

    #[test]
    fn remove_nonexistent_workspace_returns_false() {
        let mut reg = WorkspaceRegistry::default();
        assert!(!reg.remove("ghost"));
    }
}
```

- [ ] **Step 4: Add `Workspace` command to `main.rs`**

In `main.rs`, add to the `use` imports at the top:

```rust
mod workspace;
```

Add the `WorkspaceAction` enum and `Workspace` variant. In the `Commands` enum, add after `Graph`:

```rust
/// Manage registered workspaces
Workspace {
    #[command(subcommand)]
    action: WorkspaceAction,
},
```

After the `Commands` enum, add:

```rust
#[derive(Subcommand)]
enum WorkspaceAction {
    /// Register a directory as a workspace
    Add {
        /// Path to the directory
        path: PathBuf,
        /// Workspace name (defaults to directory name)
        #[arg(long)]
        name: Option<String>,
        /// Port for this workspace's server (defaults to next available)
        #[arg(long)]
        port: Option<u16>,
    },
    /// List all registered workspaces
    List,
    /// Remove a workspace from the registry
    Remove {
        /// Workspace name to remove
        name: String,
    },
}
```

In the `match cli.command` block, add:

```rust
Some(Commands::Workspace { action }) => match action {
    WorkspaceAction::Add { path, name, port } => {
        workspace::run_workspace_add(path, name, port)?;
    }
    WorkspaceAction::List => {
        workspace::run_workspace_list()?;
    }
    WorkspaceAction::Remove { name } => {
        workspace::run_workspace_remove(&name)?;
    }
},
```

- [ ] **Step 5: Run tests and check**

```bash
cargo test -p thinkingroot-cli --no-default-features workspace
cargo check -p thinkingroot-cli --no-default-features
```

Expected: all pass, no errors.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-cli/src/workspace.rs crates/thinkingroot-cli/src/main.rs
git commit -m "feat(cli): add root workspace add/list/remove commands"
```

---

## Task 7: `mcp_config.rs` + `root connect` command

**Files:**
- Create: `crates/thinkingroot-cli/src/mcp_config.rs`
- Modify: `crates/thinkingroot-cli/src/main.rs`

- [ ] **Step 1: Write failing tests**

Create `crates/thinkingroot-cli/src/mcp_config.rs` with tests only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_mcp_servers_inserts_entry_preserving_others() {
        let mut existing = json!({
            "mcpServers": {
                "github": { "command": "npx", "args": ["-y", "@github/mcp"] }
            }
        });
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        assert!(existing["mcpServers"]["github"].is_object());
        assert_eq!(
            existing["mcpServers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
    }

    #[test]
    fn merge_servers_format_for_vscode() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::Servers, 3001);
        assert_eq!(existing["servers"]["thinkingroot"]["type"], "sse");
        assert_eq!(
            existing["servers"]["thinkingroot"]["url"],
            "http://localhost:3001/mcp/sse"
        );
    }

    #[test]
    fn merge_context_servers_format_for_zed() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::ContextServers, 3000);
        assert_eq!(
            existing["context_servers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
    }

    #[test]
    fn remove_entry_leaves_other_servers_intact() {
        let mut existing = json!({
            "mcpServers": {
                "github": { "command": "npx" },
                "thinkingroot": { "url": "http://localhost:3000/mcp/sse" }
            }
        });
        remove_entry(&mut existing, ConfigFormat::McpServers);
        assert!(existing["mcpServers"]["github"].is_object());
        assert!(existing["mcpServers"]["thinkingroot"].is_null());
    }

    #[test]
    fn merge_into_empty_file() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        assert!(existing["mcpServers"]["thinkingroot"].is_object());
    }
}
```

- [ ] **Step 2: Run — expect FAIL**

```bash
cargo test -p thinkingroot-cli --no-default-features mcp_config
```

Expected: FAIL — types and functions not defined.

- [ ] **Step 3: Write the full `mcp_config.rs`**

```rust
use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use serde_json::{json, Value};

/// The JSON key / format that each tool uses for MCP server configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConfigFormat {
    /// `{ "mcpServers": { "thinkingroot": { "url": "..." } } }`
    McpServers,
    /// `{ "servers": { "thinkingroot": { "type": "sse", "url": "..." } } }`
    Servers,
    /// `{ "context_servers": { "thinkingroot": { "url": "..." } } }`
    ContextServers,
    /// Individual file, same JSON as McpServers
    ContinueDev,
}

/// A detected AI tool with its resolved config file path.
pub struct DetectedTool {
    pub name: &'static str,
    pub config_path: PathBuf,
    pub format: ConfigFormat,
}

pub enum WriteAction {
    Written,
    DryRun(String), // what would be written
    Removed,
    Skipped(&'static str), // reason
}

pub struct WriteResult {
    pub tool: &'static str,
    pub path: PathBuf,
    pub action: WriteAction,
}

// ── Tool detection ───────────────────────────────────────────────

/// Detect all installed AI tools by checking whether their config directories exist.
pub fn detect_tools() -> Vec<DetectedTool> {
    tool_defs()
        .into_iter()
        .filter_map(|(name, path_fn, format)| {
            path_fn().map(|path| DetectedTool { name, config_path: path, format })
        })
        .filter(|t| {
            // Detect by parent directory existing (file itself may not exist yet)
            t.config_path.parent().map(|p| p.exists()).unwrap_or(false)
        })
        .collect()
}

fn tool_defs() -> Vec<(&'static str, Box<dyn Fn() -> Option<PathBuf>>, ConfigFormat)> {
    vec![
        (
            "Claude Desktop",
            Box::new(|| {
                dirs::config_dir().map(|d| d.join("Claude").join("claude_desktop_config.json"))
            }),
            ConfigFormat::McpServers,
        ),
        (
            "Cursor",
            Box::new(|| dirs::home_dir().map(|d| d.join(".cursor").join("mcp.json"))),
            ConfigFormat::McpServers,
        ),
        (
            "VS Code",
            Box::new(|| {
                dirs::config_dir().map(|d| d.join("Code").join("User").join("mcp.json"))
            }),
            ConfigFormat::Servers,
        ),
        (
            "Windsurf",
            Box::new(|| {
                dirs::home_dir()
                    .map(|d| d.join(".codeium").join("windsurf").join("mcp_config.json"))
            }),
            ConfigFormat::McpServers,
        ),
        (
            "Zed",
            Box::new(|| {
                // Zed uses ~/.config/zed/settings.json on all platforms
                // (not dirs::config_dir() on macOS which points to Library/Application Support)
                #[cfg(target_os = "macos")]
                {
                    dirs::home_dir().map(|d| d.join(".config").join("zed").join("settings.json"))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    dirs::config_dir().map(|d| d.join("zed").join("settings.json"))
                }
            }),
            ConfigFormat::ContextServers,
        ),
        (
            "Cline",
            Box::new(|| {
                dirs::config_dir().map(|d| {
                    d.join("Code")
                        .join("User")
                        .join("globalStorage")
                        .join("saoudrizwan.claude-dev")
                        .join("settings")
                        .join("cline_mcp_settings.json")
                })
            }),
            ConfigFormat::McpServers,
        ),
        (
            "Continue.dev",
            Box::new(|| {
                dirs::home_dir()
                    .map(|d| d.join(".continue").join("mcpServers").join("thinkingroot.json"))
            }),
            ConfigFormat::ContinueDev,
        ),
    ]
}

// ── JSON helpers (pub for tests) ─────────────────────────────────

pub fn apply_entry(existing: &mut Value, format: ConfigFormat, port: u16) {
    let servers_key = match format {
        ConfigFormat::McpServers | ConfigFormat::ContinueDev => "mcpServers",
        ConfigFormat::Servers => "servers",
        ConfigFormat::ContextServers => "context_servers",
    };

    let entry = match format {
        ConfigFormat::Servers => json!({
            "type": "sse",
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
        _ => json!({
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
    };

    if !existing[servers_key].is_object() {
        existing[servers_key] = json!({});
    }
    existing[servers_key]["thinkingroot"] = entry;
}

pub fn remove_entry(existing: &mut Value, format: ConfigFormat) {
    let servers_key = match format {
        ConfigFormat::McpServers | ConfigFormat::ContinueDev => "mcpServers",
        ConfigFormat::Servers => "servers",
        ConfigFormat::ContextServers => "context_servers",
    };
    if let Some(obj) = existing[servers_key].as_object_mut() {
        obj.remove("thinkingroot");
    }
}

// ── File I/O ─────────────────────────────────────────────────────

pub fn write_tool_config(tool: &DetectedTool, port: u16, dry_run: bool) -> anyhow::Result<WriteResult> {
    let path = &tool.config_path;

    let mut existing: Value = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).unwrap_or(json!({}))
    } else {
        json!({})
    };

    apply_entry(&mut existing, tool.format, port);
    let json_out = serde_json::to_string_pretty(&existing)?;

    if dry_run {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::DryRun(json_out),
        });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, &json_out)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteResult { tool: tool.name, path: path.clone(), action: WriteAction::Written })
}

pub fn remove_tool_config(tool: &DetectedTool, dry_run: bool) -> anyhow::Result<WriteResult> {
    let path = &tool.config_path;
    if !path.exists() {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::Skipped("config file not found"),
        });
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut existing: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
    remove_entry(&mut existing, tool.format);
    let json_out = serde_json::to_string_pretty(&existing)?;

    if dry_run {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::DryRun(json_out),
        });
    }

    std::fs::write(path, &json_out)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteResult { tool: tool.name, path: path.clone(), action: WriteAction::Removed })
}

// ── run_connect entry point ───────────────────────────────────────

pub fn run_connect(
    tool_filter: Option<&str>,
    port: u16,
    dry_run: bool,
    remove: bool,
) -> anyhow::Result<()> {
    println!();
    println!("  {} AI tools...", style("Scanning for").cyan().bold());
    println!();

    let all_tools = detect_tools();
    if all_tools.is_empty() {
        println!("  No supported AI tools detected.");
        println!("  Supported: Claude Desktop, Cursor, VS Code, Windsurf, Zed, Cline, Continue.dev");
        return Ok(());
    }

    let tools_to_process: Vec<&DetectedTool> = match tool_filter {
        Some(filter) => {
            let filtered: Vec<&DetectedTool> = all_tools
                .iter()
                .filter(|t| t.name.to_lowercase().contains(&filter.to_lowercase()))
                .collect();
            if filtered.is_empty() {
                anyhow::bail!(
                    "no tool matching '{}' detected. Run `root connect` to see all detected tools.",
                    filter
                );
            }
            filtered
        }
        None => all_tools.iter().collect(),
    };

    if dry_run {
        println!("  {} (no files will be changed)\n", style("Dry run").yellow().bold());
    }

    for tool in tools_to_process {
        let result = if remove {
            remove_tool_config(tool, dry_run)?
        } else {
            write_tool_config(tool, port, dry_run)?
        };

        match &result.action {
            WriteAction::Written => println!(
                "  {} {:<20} → {}",
                style("✓").green().bold(),
                result.tool,
                style(result.path.display()).dim()
            ),
            WriteAction::DryRun(content) => {
                println!(
                    "  {} {:<20} → {} (would write)",
                    style("~").yellow().bold(),
                    result.tool,
                    style(result.path.display()).dim()
                );
                println!("{}", style(content).dim());
            }
            WriteAction::Removed => println!(
                "  {} {:<20} → entry removed",
                style("✓").green().bold(),
                result.tool
            ),
            WriteAction::Skipped(reason) => println!(
                "  {} {:<20} → {}",
                style("!").yellow().bold(),
                result.tool,
                reason
            ),
        }
    }

    if !dry_run && !remove {
        println!();
        println!(
            "  All connected to {}",
            style(format!("http://localhost:{}/mcp/sse", port)).cyan()
        );
        println!("  Restart your AI tools to pick up the new config.");
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_mcp_servers_inserts_entry_preserving_others() {
        let mut existing = json!({
            "mcpServers": {
                "github": { "command": "npx", "args": ["-y", "@github/mcp"] }
            }
        });
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        assert!(existing["mcpServers"]["github"].is_object());
        assert_eq!(
            existing["mcpServers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
    }

    #[test]
    fn merge_servers_format_for_vscode() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::Servers, 3001);
        assert_eq!(existing["servers"]["thinkingroot"]["type"], "sse");
        assert_eq!(
            existing["servers"]["thinkingroot"]["url"],
            "http://localhost:3001/mcp/sse"
        );
    }

    #[test]
    fn merge_context_servers_format_for_zed() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::ContextServers, 3000);
        assert_eq!(
            existing["context_servers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
    }

    #[test]
    fn remove_entry_leaves_other_servers_intact() {
        let mut existing = json!({
            "mcpServers": {
                "github": { "command": "npx" },
                "thinkingroot": { "url": "http://localhost:3000/mcp/sse" }
            }
        });
        remove_entry(&mut existing, ConfigFormat::McpServers);
        assert!(existing["mcpServers"]["github"].is_object());
        assert!(existing["mcpServers"]["thinkingroot"].is_null());
    }

    #[test]
    fn merge_into_empty_file() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        assert!(existing["mcpServers"]["thinkingroot"].is_object());
    }
}
```

- [ ] **Step 4: Add `Connect` command to `main.rs`**

Add at top of `main.rs`:

```rust
mod mcp_config;
```

Add to `Commands` enum:

```rust
/// Write MCP configuration to detected AI tools
Connect {
    /// Only connect this specific tool (e.g. "claude", "cursor")
    #[arg(long)]
    tool: Option<String>,
    /// Port the ThinkingRoot server is running on
    #[arg(long, default_value = "3000")]
    port: u16,
    /// Show what would be written without changing any files
    #[arg(long)]
    dry_run: bool,
    /// Remove ThinkingRoot entry from all tool configs
    #[arg(long)]
    remove: bool,
},
```

Add to the `match cli.command` block:

```rust
Some(Commands::Connect { tool, port, dry_run, remove }) => {
    mcp_config::run_connect(tool.as_deref(), port, dry_run, remove)?;
}
```

- [ ] **Step 5: Run tests and check**

```bash
cargo test -p thinkingroot-cli --no-default-features mcp_config
cargo check -p thinkingroot-cli --no-default-features
```

Expected: all tests pass, no compile errors.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-cli/src/mcp_config.rs crates/thinkingroot-cli/src/main.rs
git commit -m "feat(cli): add root connect command with MCP auto-wiring for 7 AI tools"
```

---

## Task 8: Extend `serve.rs` — registry fallback + `--install-service`

**Files:**
- Modify: `crates/thinkingroot-cli/src/serve.rs`
- Modify: `crates/thinkingroot-cli/src/main.rs`

- [ ] **Step 1: Update the `Serve` command in `main.rs`**

Replace the `Serve` variant in `Commands`:

```rust
/// Start the REST API and MCP server
Serve {
    /// Port to bind (ignored when loading from registry — each workspace has its own port)
    #[arg(long, default_value = "3000")]
    port: u16,
    /// Host to bind
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Optional API key for bearer authentication
    #[arg(long)]
    api_key: Option<String>,
    /// Workspace paths to mount (repeatable; if omitted, reads from registry)
    #[arg(long = "path")]
    paths: Vec<PathBuf>,
    /// Mount a single workspace by registry name
    #[arg(long)]
    name: Option<String>,
    /// Run as MCP stdio server (single workspace, no HTTP)
    #[arg(long)]
    mcp_stdio: bool,
    /// Disable REST API (MCP only)
    #[arg(long)]
    no_rest: bool,
    /// Disable MCP endpoints (REST only)
    #[arg(long)]
    no_mcp: bool,
    /// Generate and install an OS-native service file (launchd/systemd/Windows)
    #[arg(long)]
    install_service: bool,
},
```

Update the match arm to pass `name` and `install_service`:

```rust
Some(Commands::Serve { port, host, api_key, paths, name, mcp_stdio, no_rest, no_mcp, install_service }) => {
    if install_service {
        serve::install_service()?;
        return Ok(());
    }
    serve::run_serve(port, host, api_key, paths, name, mcp_stdio, no_rest, no_mcp).await?;
}
```

- [ ] **Step 2: Add registry resolution to `run_serve`**

In `crates/thinkingroot-cli/src/serve.rs`, add this import at the top:

```rust
use thinkingroot_core::{WorkspaceRegistry};
```

Change the `run_serve` signature to accept `name: Option<String>`:

```rust
pub async fn run_serve(
    port: u16,
    host: String,
    api_key: Option<String>,
    paths: Vec<PathBuf>,
    name: Option<String>,
    mcp_stdio: bool,
    no_rest: bool,
    no_mcp: bool,
) -> anyhow::Result<()> {
```

Replace the `for path in &paths` block with registry-aware resolution at the start of `run_serve`, just after the `if no_rest && no_mcp` check:

```rust
// Resolve workspace paths: explicit --path > --name > registry
let resolved_paths: Vec<(String, PathBuf, u16)> = if !paths.is_empty() {
    // Explicit --path flags: use directory name and supplied port
    paths.iter().map(|p| {
        let abs = std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.clone());
        let ws_name = abs
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string());
        (ws_name, abs, port)
    }).collect()
} else {
    // Load from registry
    let registry = WorkspaceRegistry::load()?;
    let workspaces = if let Some(ref ws_name) = name {
        // --name filter
        let entry = registry.workspaces.iter()
            .find(|w| w.name == *ws_name)
            .ok_or_else(|| anyhow::anyhow!(
                "workspace \"{}\" not found. Run `root workspace list` to see registered workspaces.",
                ws_name
            ))?;
        vec![(entry.name.clone(), entry.path.clone(), entry.port)]
    } else {
        registry.workspaces.iter()
            .map(|w| (w.name.clone(), w.path.clone(), w.port))
            .collect()
    };

    if workspaces.is_empty() {
        anyhow::bail!(
            "No workspaces registered. Run `root setup` or `root workspace add <path>`."
        );
    }
    workspaces
};
```

Replace the old `for path in &paths` engine-mounting loop with:

```rust
let mut engine = QueryEngine::new();
for (ws_name, abs_path, _ws_port) in &resolved_paths {
    engine.mount(ws_name.clone(), abs_path.clone()).await?;
    tracing::info!("mounted workspace '{}' from {}", ws_name, abs_path.display());
}
```

Update the multi-workspace banner to show per-workspace ports from the registry:

```rust
for (ws_name, _path, ws_port) in &resolved_paths {
    println!(
        "  Workspace: {} → http://{}:{}/api/v1/ws/{}/",
        ws_name, host, ws_port, ws_name
    );
}
```

Also update `run_graph` to pass `None` for the new `name` parameter:

```rust
run_serve(port, "127.0.0.1".into(), None, vec![path], None, false, false, false).await
```

- [ ] **Step 3: Add `install_service` function to `serve.rs`**

Add at the end of `serve.rs`:

```rust
/// Generate and install an OS-native service file so `root serve` starts on login.
pub fn install_service() -> anyhow::Result<()> {
    let binary = std::env::current_exe()
        .context("cannot resolve current executable path")?
        .display()
        .to_string();

    let log_path = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?
        .join("thinkingroot")
        .join("serve.log");

    #[cfg(target_os = "macos")]
    {
        let agents_dir = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home dir"))?
            .join("Library")
            .join("LaunchAgents");
        std::fs::create_dir_all(&agents_dir)?;
        let plist_path = agents_dir.join("dev.thinkingroot.plist");

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>             <string>dev.thinkingroot</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>         <true/>
    <key>KeepAlive</key>         <true/>
    <key>StandardOutPath</key>   <string>{log}</string>
    <key>StandardErrorPath</key> <string>{log}</string>
</dict>
</plist>"#,
            binary = binary,
            log = log_path.display()
        );

        std::fs::write(&plist_path, plist)?;
        println!();
        println!("  {} {}", console::style("✓ Service file:").green().bold(), plist_path.display());
        println!();
        println!("  To start now:");
        println!("    launchctl load {}", plist_path.display());
        println!("    launchctl start dev.thinkingroot");
        println!();
        println!("  ThinkingRoot will start automatically on login.");
        println!("  Logs: {}", log_path.display());
    }

    #[cfg(target_os = "linux")]
    {
        let systemd_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?
            .join("systemd")
            .join("user");
        std::fs::create_dir_all(&systemd_dir)?;
        let service_path = systemd_dir.join("thinkingroot.service");

        let unit = format!(
            "[Unit]\nDescription=ThinkingRoot Knowledge Server\nAfter=network.target\n\n\
             [Service]\nExecStart={binary} serve\nRestart=on-failure\n\
             StandardOutput=append:{log}\nStandardError=append:{log}\n\n\
             [Install]\nWantedBy=default.target\n",
            binary = binary,
            log = log_path.display()
        );

        std::fs::write(&service_path, unit)?;
        println!();
        println!("  {} {}", console::style("✓ Service file:").green().bold(), service_path.display());
        println!();
        println!("  To enable:");
        println!("    systemctl --user daemon-reload");
        println!("    systemctl --user enable thinkingroot");
        println!("    systemctl --user start thinkingroot");
        println!();
        println!("  Logs: {}", log_path.display());
    }

    #[cfg(target_os = "windows")]
    {
        let ps_path = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home dir"))?
            .join("thinkingroot-service.ps1");

        let script = format!(
            "# ThinkingRoot Windows Service — run as Administrator\r\n\
             sc.exe create \"ThinkingRoot\" binPath= \"{binary} serve\" start= auto\r\n\
             sc.exe start \"ThinkingRoot\"\r\n",
            binary = binary
        );

        std::fs::write(&ps_path, script)?;
        println!();
        println!("  {} {}", console::style("✓ Script:").green().bold(), ps_path.display());
        println!();
        println!("  Run as Administrator:");
        println!("    powershell -ExecutionPolicy Bypass -File {}", ps_path.display());
    }

    Ok(())
}
```

- [ ] **Step 4: Add `use anyhow::Context as _` and `use dirs` to `serve.rs`**

The `context()` method requires this import. Add at the top of `serve.rs`:

```rust
use dirs;
```

(The `anyhow::Context as _` import is already present via `use anyhow::Context as _;`.)

- [ ] **Step 5: Build and check**

```bash
cargo check -p thinkingroot-cli --no-default-features
```

Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-cli/src/serve.rs crates/thinkingroot-cli/src/main.rs
git commit -m "feat(cli): serve reads workspace registry; add --name, --install-service flags"
```

---

## Task 9: `setup.rs` + `root setup` command

**Files:**
- Create: `crates/thinkingroot-cli/src/setup.rs`
- Modify: `crates/thinkingroot-cli/src/main.rs`

- [ ] **Step 1: Create `setup.rs`**

Create `crates/thinkingroot-cli/src/setup.rs`:

```rust
use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use indicatif::{ProgressBar, ProgressStyle};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry, global_config::{GlobalConfig, ServeConfig}};
use thinkingroot_core::config::{LlmConfig, ProviderConfig, ProvidersConfig};

// ── Provider catalogue ───────────────────────────────────────────

struct ProviderDef {
    label: &'static str,       // shown in menu
    id: &'static str,          // matches LlmConfig.default_provider
    default_env: &'static str, // env var name shown to user
    base_url: Option<&'static str>, // None = hardcoded in llm.rs
    default_models: &'static [&'static str],
    validate_url: Option<&'static str>, // GET /models URL for key validation
}

static PROVIDERS: &[ProviderDef] = &[
    ProviderDef {
        label: "OpenRouter  (200+ models, one key — recommended)",
        id: "openrouter",
        default_env: "OPENROUTER_API_KEY",
        base_url: Some("https://openrouter.ai/api/v1"),
        default_models: &[
            "anthropic/claude-3-haiku",
            "openai/gpt-4o-mini",
            "meta-llama/llama-3.1-8b-instruct:free",
        ],
        validate_url: Some("https://openrouter.ai/api/v1/models"),
    },
    ProviderDef {
        label: "OpenAI",
        id: "openai",
        default_env: "OPENAI_API_KEY",
        base_url: Some("https://api.openai.com"),
        default_models: &["gpt-4o-mini", "gpt-4o", "gpt-3.5-turbo"],
        validate_url: Some("https://api.openai.com/v1/models"),
    },
    ProviderDef {
        label: "Anthropic",
        id: "anthropic",
        default_env: "ANTHROPIC_API_KEY",
        base_url: None,
        default_models: &["claude-3-haiku-20240307", "claude-3-5-sonnet-20241022"],
        validate_url: Some("https://api.anthropic.com/v1/models"),
    },
    ProviderDef {
        label: "Ollama      (local, free)",
        id: "ollama",
        default_env: "",
        base_url: Some("http://localhost:11434"),
        default_models: &["llama3", "mistral", "phi3"],
        validate_url: None,
    },
    ProviderDef {
        label: "Groq        (ultra-fast inference)",
        id: "groq",
        default_env: "GROQ_API_KEY",
        base_url: Some("https://api.groq.com/openai/v1"),
        default_models: &["llama-3.1-8b-instant", "mixtral-8x7b-32768"],
        validate_url: Some("https://api.groq.com/openai/v1/models"),
    },
    ProviderDef {
        label: "AWS Bedrock  (enterprise, no data leaves AWS)",
        id: "bedrock",
        default_env: "",
        base_url: None,
        default_models: &["amazon.nova-micro-v1:0", "anthropic.claude-3-haiku-20240307-v1:0"],
        validate_url: None, // uses AWS credentials, not an API key
    },
    ProviderDef {
        label: "Together AI",
        id: "together",
        default_env: "TOGETHER_API_KEY",
        base_url: Some("https://api.together.xyz/v1"),
        default_models: &["meta-llama/Meta-Llama-3.1-8B-Instruct-Turbo", "mistralai/Mixtral-8x7B-Instruct-v0.1"],
        validate_url: Some("https://api.together.xyz/v1/models"),
    },
    ProviderDef {
        label: "DeepSeek",
        id: "deepseek",
        default_env: "DEEPSEEK_API_KEY",
        base_url: Some("https://api.deepseek.com"),
        default_models: &["deepseek-chat", "deepseek-coder"],
        validate_url: Some("https://api.deepseek.com/models"),
    },
    ProviderDef {
        label: "Perplexity",
        id: "perplexity",
        default_env: "PERPLEXITY_API_KEY",
        base_url: Some("https://api.perplexity.ai"),
        default_models: &["llama-3.1-sonar-small-128k-online", "llama-3.1-sonar-large-128k-online"],
        validate_url: Some("https://api.perplexity.ai/models"),
    },
    ProviderDef {
        label: "LiteLLM     (self-hosted proxy)",
        id: "litellm",
        default_env: "LITELLM_API_KEY",
        base_url: Some("http://localhost:4000"),
        default_models: &["gpt-4o-mini", "claude-3-haiku"],
        validate_url: None,
    },
    ProviderDef {
        label: "Custom      (any OpenAI-compatible endpoint)",
        id: "custom",
        default_env: "CUSTOM_LLM_API_KEY",
        base_url: None, // user provides
        default_models: &[],
        validate_url: None,
    },
];

// ── Main entry point ─────────────────────────────────────────────

pub async fn run_setup() -> anyhow::Result<()> {
    let theme = ColorfulTheme::default();

    print_banner();

    // Idempotency: detect existing config
    if GlobalConfig::path().map(|p| p.exists()).unwrap_or(false) {
        return run_setup_update(&theme).await;
    }

    // ── Step 1: Confirm config location ──────────────────────────
    let config_path = GlobalConfig::path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;

    println!("\n  {}", style("[1/5] Global config location").cyan().bold());
    println!("  Settings will be saved to: {}", style(config_path.display()).white());
    println!();

    if !Confirm::with_theme(&theme)
        .with_prompt("Continue?")
        .default(true)
        .interact()?
    {
        return Ok(());
    }

    // ── Step 2: LLM provider ──────────────────────────────────────
    println!("\n  {}", style("[2/5] LLM Provider").cyan().bold());
    println!("  Used for knowledge extraction from your files.\n");

    let provider_labels: Vec<&str> = PROVIDERS.iter().map(|p| p.label).collect();
    let provider_idx = Select::with_theme(&theme)
        .with_prompt("Which provider?")
        .items(&provider_labels)
        .default(0)
        .interact()?;

    let provider = &PROVIDERS[provider_idx];

    let (api_key, model) = configure_provider(&theme, provider).await?;

    // Build global config
    let mut llm = LlmConfig {
        default_provider: provider.id.to_string(),
        extraction_model: model.clone(),
        compilation_model: model.clone(),
        max_concurrent_requests: 5,
        request_timeout_secs: 120,
        providers: ProvidersConfig::default(),
    };
    set_provider_config(&mut llm, provider, &api_key);

    let global = GlobalConfig {
        llm,
        serve: ServeConfig::default(),
    };

    // ── Step 3: First workspace ───────────────────────────────────
    println!("\n  {}", style("[3/5] First workspace").cyan().bold());
    println!("  A folder to compile as your knowledge base.\n");

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let default_path = cwd.display().to_string();

    let ws_path_str: String = Input::with_theme(&theme)
        .with_prompt("Path")
        .default(default_path)
        .interact_text()?;

    let ws_path = PathBuf::from(&ws_path_str);
    let abs_ws_path = std::fs::canonicalize(&ws_path)
        .with_context(|| format!("path not found: {}", ws_path.display()))?;

    let default_name = abs_ws_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    let ws_name: String = Input::with_theme(&theme)
        .with_prompt("Workspace name")
        .default(default_name)
        .interact_text()?;

    let mut registry = WorkspaceRegistry::default();
    let ws_port = registry.next_available_port();
    registry.add(WorkspaceEntry {
        name: ws_name.clone(),
        path: abs_ws_path.clone(),
        port: ws_port,
    });

    // ── Step 4: Connect AI tools ──────────────────────────────────
    println!("\n  {}", style("[4/5] Connect AI tools").cyan().bold());
    println!("  Scanning for installed tools...\n");

    let detected = crate::mcp_config::detect_tools();
    for tool in &detected {
        println!("  {} {}", style("✓").green(), tool.name);
    }
    if detected.is_empty() {
        println!("  No supported tools detected. You can run `root connect` later.");
    }
    println!();

    let connect = if !detected.is_empty() {
        Confirm::with_theme(&theme)
            .with_prompt("Connect detected tools now?")
            .default(true)
            .interact()?
    } else {
        false
    };

    // ── Step 5: Compile ───────────────────────────────────────────
    println!("\n  {}", style("[5/5] Compile knowledge base").cyan().bold());

    let compile_choices = &["Yes, compile now", "Skip — I'll run `root compile` later"];
    let compile_now = Select::with_theme(&theme)
        .with_prompt("Compile now?")
        .items(compile_choices)
        .default(0)
        .interact()? == 0;

    // ── Apply all changes ─────────────────────────────────────────
    println!();
    global.save()?;
    registry.save()?;

    // Initialize workspace .thinkingroot/config.toml
    let ws_config = thinkingroot_core::Config::default();
    ws_config.save(&abs_ws_path)?;

    // Connect tools
    if connect {
        for tool in &detected {
            if let Err(e) = crate::mcp_config::write_tool_config(tool, ws_port, false) {
                eprintln!("  Warning: failed to configure {}: {}", tool.name, e);
            }
        }
    }

    // Compile
    if compile_now {
        println!("  Compiling {}...\n", abs_ws_path.display());
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap(),
        );
        pb.set_message("Compiling knowledge base...");
        pb.enable_steady_tick(std::time::Duration::from_millis(80));

        match crate::pipeline::run_pipeline(&abs_ws_path).await {
            Ok(result) => {
                pb.finish_and_clear();
                println!(
                    "  {} {} claims · {} entities · {} relations\n",
                    style("✓").green().bold(),
                    result.claims_count,
                    result.entities_count,
                    result.relations_count,
                );
            }
            Err(e) => {
                pb.finish_and_clear();
                println!("  {} Compilation failed: {}", style("!").yellow(), e);
                println!("  Run `root compile {}` to retry.", abs_ws_path.display());
            }
        }
    }

    // ── Summary ───────────────────────────────────────────────────
    println!("  {}", style("─".repeat(56)).dim());
    println!("  {}", style("Setup complete!").green().bold());
    println!();
    println!(
        "  Global config   {}",
        style(GlobalConfig::path().unwrap().display()).dim()
    );
    println!(
        "  Workspace       {}/.thinkingroot/",
        abs_ws_path.display()
    );
    println!(
        "  MCP endpoint    {}",
        style(format!("http://localhost:{}/mcp/sse", ws_port)).cyan()
    );

    if connect && !detected.is_empty() {
        println!();
        println!("  Connected tools:");
        for tool in &detected {
            println!("    {} {}", style("✓").green(), tool.name);
        }
    }

    println!();
    println!("  Next steps:");
    println!("    {}  start the knowledge server", style("root serve").cyan());
    println!(
        "    {}  add more folders",
        style("root workspace add <path>").cyan()
    );
    println!(
        "    {}  wire more AI tools",
        style("root connect").cyan()
    );
    println!();

    Ok(())
}

// ── Update flow (idempotent re-run) ──────────────────────────────

async fn run_setup_update(theme: &ColorfulTheme) -> anyhow::Result<()> {
    let global = GlobalConfig::load()?;
    let registry = WorkspaceRegistry::load()?;

    println!("\n  {} ThinkingRoot is already configured.\n", style("✓").green().bold());
    println!("  Provider:   {} / {}", global.llm.default_provider, global.llm.extraction_model);
    println!(
        "  Workspaces: {} total\n",
        registry.workspaces.len()
    );

    let choices = &[
        "Change LLM provider",
        "Add a workspace",
        "Connect more AI tools",
        "Reconfigure from scratch",
        "Cancel",
    ];

    let choice = Select::with_theme(theme)
        .with_prompt("What would you like to update?")
        .items(choices)
        .default(4)
        .interact()?;

    match choice {
        0 => {
            // Change provider
            let provider_labels: Vec<&str> = PROVIDERS.iter().map(|p| p.label).collect();
            let idx = Select::with_theme(theme)
                .with_prompt("New provider?")
                .items(&provider_labels)
                .default(0)
                .interact()?;
            let provider = &PROVIDERS[idx];
            let (api_key, model) = configure_provider(theme, provider).await?;
            let mut new_global = global.clone();
            new_global.llm.default_provider = provider.id.to_string();
            new_global.llm.extraction_model = model.clone();
            new_global.llm.compilation_model = model;
            set_provider_config(&mut new_global.llm, provider, &api_key);
            new_global.save()?;
            println!("  {} Provider updated.", style("✓").green().bold());
        }
        1 => {
            // Add workspace
            let path_str: String = Input::with_theme(theme)
                .with_prompt("Path")
                .interact_text()?;
            let path = PathBuf::from(path_str);
            crate::workspace::run_workspace_add(path, None, None)?;
        }
        2 => {
            // Connect tools
            crate::mcp_config::run_connect(None, 3000, false, false)?;
        }
        3 => {
            // Wipe and re-run — remove global config so setup runs fresh
            if let Some(p) = GlobalConfig::path() {
                if p.exists() { std::fs::remove_file(&p)?; }
            }
            if let Some(p) = WorkspaceRegistry::path() {
                if p.exists() { std::fs::remove_file(&p)?; }
            }
            Box::pin(run_setup()).await?;
        }
        _ => {} // Cancel
    }

    Ok(())
}

// ── Provider helpers ─────────────────────────────────────────────

async fn configure_provider(
    theme: &ColorfulTheme,
    provider: &ProviderDef,
) -> anyhow::Result<(String, String)> {
    // API key (skip for Ollama, Bedrock, LiteLLM without auth)
    let api_key = if !provider.default_env.is_empty() {
        let key: String = Password::with_theme(theme)
            .with_prompt(format!("{} API key", provider.label.split_whitespace().next().unwrap_or(provider.id)))
            .interact()?;

        if let Some(validate_url) = provider.validate_url {
            let pb = indicatif::ProgressBar::new_spinner();
            pb.set_message("Validating key...");
            pb.enable_steady_tick(std::time::Duration::from_millis(80));

            match validate_key_http(validate_url, provider.id, &key).await {
                Ok(()) => pb.finish_with_message(format!("{} Key valid", style("✓").green())),
                Err(e) => {
                    pb.finish_with_message(format!("{} Validation failed", style("✗").red()));
                    anyhow::bail!("Key validation failed: {}\nRe-run `root setup` to try again.", e);
                }
            }
        }
        key
    } else {
        String::new()
    };

    // Model selection
    let model = if !provider.default_models.is_empty() {
        let mut model_items: Vec<&str> = provider.default_models.to_vec();
        model_items.push("Enter model ID manually");
        let midx = Select::with_theme(theme)
            .with_prompt("Extraction model")
            .items(&model_items)
            .default(0)
            .interact()?;
        if midx == model_items.len() - 1 {
            Input::with_theme(theme)
                .with_prompt("Model ID")
                .interact_text()?
        } else {
            model_items[midx].to_string()
        }
    } else {
        // Custom / no presets
        Input::with_theme(theme)
            .with_prompt("Model ID")
            .interact_text()?
    };

    Ok((api_key, model))
}

fn set_provider_config(llm: &mut LlmConfig, provider: &ProviderDef, api_key: &str) {
    if provider.default_env.is_empty() {
        return; // Ollama, Bedrock — no key needed
    }
    let env_var = provider.default_env;
    // Set the key as an env var for this process so it's usable immediately
    std::env::set_var(env_var, api_key);

    let cfg = ProviderConfig {
        api_key_env: Some(env_var.to_string()),
        base_url: provider.base_url.map(str::to_string),
        default_model: None,
    };
    match provider.id {
        "openrouter" => llm.providers.openrouter = Some(cfg),
        "openai"     => llm.providers.openai     = Some(cfg),
        "anthropic"  => llm.providers.anthropic  = Some(cfg),
        "groq"       => llm.providers.groq        = Some(cfg),
        "together"   => llm.providers.together    = Some(cfg),
        "deepseek"   => llm.providers.deepseek    = Some(cfg),
        "perplexity" => llm.providers.perplexity  = Some(cfg),
        "litellm"    => llm.providers.litellm     = Some(cfg),
        "custom"     => llm.providers.custom      = Some(cfg),
        _            => {}
    }
}

/// Validate an API key by GETting the provider's /models endpoint.
/// Returns Ok(()) if the key is accepted (HTTP 2xx, 404, or 405 — key valid, endpoint may differ).
/// Returns Err if HTTP 401/403 (bad key) or network error.
async fn validate_key_http(url: &str, provider_id: &str, key: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let mut req = client.get(url);

    // Anthropic uses a different auth header
    req = if provider_id == "anthropic" {
        req.header("x-api-key", key)
           .header("anthropic-version", "2023-06-01")
    } else {
        req.header("Authorization", format!("Bearer {}", key))
    };

    let resp = req.send().await.context("network error during key validation")?;

    match resp.status().as_u16() {
        401 | 403 => anyhow::bail!("Invalid API key (HTTP {})", resp.status().as_u16()),
        _ => Ok(()), // 200, 404, 405, 429 — key was accepted
    }
}

fn print_banner() {
    println!();
    println!("  {}", style("ThinkingRoot").green().bold());
    println!("  {}", style("First-time setup").dim());
    println!();
}
```

- [ ] **Step 2: Add `Setup` command to `main.rs`**

Add at top of `main.rs`:

```rust
mod setup;
```

Add to `Commands` enum:

```rust
/// First-time guided setup wizard
Setup,
```

Add to the `match cli.command` block:

```rust
Some(Commands::Setup) => {
    setup::run_setup().await?;
}
```

- [ ] **Step 3: Add `reqwest` to `thinkingroot-cli/Cargo.toml`**

`reqwest` is a workspace dep already. Add to CLI's `[dependencies]`:

```toml
reqwest = { workspace = true }
```

- [ ] **Step 4: Build and verify**

```bash
cargo check -p thinkingroot-cli --no-default-features
```

Expected: no errors.

- [ ] **Step 5: Full test run**

```bash
cargo test --no-default-features
```

Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-cli/src/setup.rs crates/thinkingroot-cli/src/main.rs crates/thinkingroot-cli/Cargo.toml
git commit -m "feat(cli): add root setup wizard with 5-step onboarding flow"
```

---

## Task 10: Final integration check

- [ ] **Step 1: Full workspace build**

```bash
cargo build --no-default-features
```

Expected: compiles cleanly.

- [ ] **Step 2: Full test suite**

```bash
cargo test --no-default-features
```

Expected: all tests pass.

- [ ] **Step 3: Smoke test the binary**

```bash
./target/debug/root --help
```

Expected: output includes `setup`, `connect`, `workspace` in the commands list.

```bash
./target/debug/root workspace --help
```

Expected: shows `add`, `list`, `remove` subcommands.

```bash
./target/debug/root connect --dry-run
```

Expected: scans for tools, prints dry-run output, exits 0.

- [ ] **Step 4: Clippy**

```bash
cargo clippy --workspace --no-default-features -- -D warnings
```

Fix any warnings before committing.

- [ ] **Step 5: Final commit**

```bash
git add -u
git commit -m "chore: phase 3 onboarding complete — setup, connect, workspace, 11 LLM providers"
```

---

## Self-Review Checklist

- [x] **Spec coverage:** All spec requirements have a task: 5 new providers ✓, global config ✓, load_merged ✓, workspace registry ✓, MCP auto-wiring for 7 tools ✓, root setup wizard ✓, root serve registry fallback ✓, --install-service ✓
- [x] **No placeholders:** All code blocks are complete
- [x] **Type consistency:** `WorkspaceEntry`, `WorkspaceRegistry`, `GlobalConfig`, `ServeConfig` defined in Task 4 and used consistently in Tasks 6, 8, 9; `ConfigFormat` defined in Task 7 and used in tests
- [x] **resolve_key helpers** defined in Task 3 before they are used in the same task
- [x] **`run_graph` updated** in Task 8 to pass `None` for the new `name` parameter
- [x] **`reqwest` dep** added to CLI Cargo.toml in Task 9
