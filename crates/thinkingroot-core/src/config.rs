use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{Error, Result};

/// Top-level configuration for a ThinkingRoot workspace.
/// Stored at `.thinkingroot/config.toml` inside the target directory.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    #[serde(default)]
    pub llm: LlmConfig,

    #[serde(default)]
    pub extraction: ExtractionConfig,

    #[serde(default)]
    pub compilation: CompilationConfig,

    #[serde(default)]
    pub verification: VerificationConfig,

    #[serde(default)]
    pub parsers: ParserConfig,

    #[serde(default)]
    pub merge: MergeConfig,

    #[serde(default)]
    pub streams: StreamsConfig,
}

impl Config {
    /// Load config from a `.thinkingroot/config.toml` file.
    pub fn load(root_path: &Path) -> Result<Self> {
        let config_path = root_path.join(".thinkingroot").join("config.toml");
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)
                .map_err(|e| Error::io_path(&config_path, e))?;
            let config: Config = toml::from_str(&content)?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

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
            // Workspace set its own LLM section — inherit individual provider slots from global
            // when the workspace leaves them as None (whole-slot inheritance).
            macro_rules! inherit {
                ($field:ident) => {
                    if workspace.llm.providers.$field.is_none() {
                        workspace.llm.providers.$field = global.llm.providers.$field.clone();
                    }
                };
            }
            inherit!(azure);
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

            // Field-level api_key_env inheritance: if workspace defines a provider slot
            // but leaves api_key_env as None, pull it from the global slot so credentials
            // set globally are not silently dropped by a partial workspace override.
            macro_rules! inherit_key {
                ($field:ident) => {
                    if let (Some(ws), Some(g)) = (
                        workspace.llm.providers.$field.as_mut(),
                        global.llm.providers.$field.as_ref(),
                    ) {
                        if ws.api_key_env.is_none() {
                            ws.api_key_env = g.api_key_env.clone();
                        }
                    }
                };
            }
            inherit_key!(openai);
            inherit_key!(anthropic);
            inherit_key!(ollama);
            inherit_key!(groq);
            inherit_key!(deepseek);
            inherit_key!(openrouter);
            inherit_key!(together);
            inherit_key!(perplexity);
            inherit_key!(litellm);
            inherit_key!(custom);
            // Azure uses a different struct type but has the same api_key_env field.
            if let (Some(ws), Some(g)) = (
                workspace.llm.providers.azure.as_mut(),
                global.llm.providers.azure.as_ref(),
            )
                && ws.api_key_env.is_none() {
                    ws.api_key_env = g.api_key_env.clone();
                }
        }
        workspace
    }

    /// Load workspace config merged with global config.
    /// Priority: per-workspace `.thinkingroot/config.toml` > global `~/.config/thinkingroot/config.toml` > defaults.
    pub fn load_merged(workspace_path: &std::path::Path) -> Result<Self> {
        let global = crate::global_config::GlobalConfig::load().unwrap_or_else(|e| {
            eprintln!("  Warning: could not load global config, using defaults: {e}");
            crate::global_config::GlobalConfig::default()
        });
        let config_path = workspace_path.join(".thinkingroot").join("config.toml");

        if !config_path.exists() {
            let config = Self {
                llm: global.llm,
                ..Default::default()
            };
            return Ok(config);
        }

        let raw =
            std::fs::read_to_string(&config_path).map_err(|e| Error::io_path(&config_path, e))?;
        let workspace: Config = toml::from_str(&raw)?;
        Ok(Self::merge_with_global(workspace, &raw, &global))
    }

    /// Save config to the `.thinkingroot/config.toml` file.
    pub fn save(&self, root_path: &Path) -> Result<()> {
        let dir = root_path.join(".thinkingroot");
        std::fs::create_dir_all(&dir).map_err(|e| Error::io_path(&dir, e))?;
        let config_path = dir.join("config.toml");
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, content).map_err(|e| Error::io_path(&config_path, e))?;
        Ok(())
    }
}

fn default_data_dir() -> String {
    ".thinkingroot".to_string()
}

/// Workspace-level settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub name: Option<String>,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            name: None,
            data_dir: ".thinkingroot".to_string(),
        }
    }
}

/// LLM provider configuration. Supports multiple providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// The default provider to use (e.g. "openai", "anthropic", "ollama").
    pub default_provider: String,
    /// The model to use for claim extraction.
    pub extraction_model: String,
    /// The model to use for compilation / summarization.
    pub compilation_model: String,
    /// Maximum concurrent LLM requests.
    pub max_concurrent_requests: usize,
    /// Request timeout in seconds.
    pub request_timeout_secs: u64,
    /// Provider-specific overrides.
    #[serde(default)]
    pub providers: ProvidersConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            default_provider: String::new(),
            extraction_model: String::new(),
            compilation_model: String::new(),
            max_concurrent_requests: 5,
            request_timeout_secs: 120,
            providers: ProvidersConfig::default(),
        }
    }
}

impl LlmConfig {
    /// Returns true if a provider and model have been configured (i.e. `root setup` has been run).
    pub fn is_configured(&self) -> bool {
        !self.default_provider.is_empty() && !self.extraction_model.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    pub bedrock: Option<BedrockConfig>,
    pub azure: Option<AzureConfig>,
    pub openai: Option<ProviderConfig>,
    pub anthropic: Option<ProviderConfig>,
    pub ollama: Option<ProviderConfig>,
    pub groq: Option<ProviderConfig>,
    pub deepseek: Option<ProviderConfig>,
    pub openrouter: Option<ProviderConfig>,
    pub together: Option<ProviderConfig>,
    pub perplexity: Option<ProviderConfig>,
    pub litellm: Option<ProviderConfig>,
    pub custom: Option<ProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Name of the environment variable that holds the API key
    /// (e.g. `"OPENAI_API_KEY"`). Checked first at runtime.
    pub api_key_env: Option<String>,
    /// Actual API key value stored in credentials.toml (mode 0600).
    /// Used as fallback when the env var is absent — lets `root compile`
    /// and `root serve --mcp-stdio` work in a fresh shell without requiring
    /// `export KEY=...` in the shell profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BedrockConfig {
    pub region: Option<String>,
    pub profile: Option<String>,
}

/// Azure OpenAI provider configuration.
/// Uses deployment-based URLs and `api-key` header auth (not Bearer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AzureConfig {
    /// Azure OpenAI resource name (e.g., "my-company-openai").
    /// Used to construct `https://{resource_name}.openai.azure.com` when
    /// `endpoint_base` is not set.
    pub resource_name: Option<String>,
    /// Full base URL override (e.g., "https://myresource.cognitiveservices.azure.com").
    /// Use this for AIServices/Foundry resources that use `.cognitiveservices.azure.com`
    /// instead of `.openai.azure.com`. When set, `resource_name` is ignored for URL
    /// construction but still stored for display purposes.
    pub endpoint_base: Option<String>,
    /// Deployment name (e.g., "gpt-4o-mini-prod").
    pub deployment: Option<String>,
    /// API version string (e.g., "2024-12-01-preview").
    pub api_version: Option<String>,
    /// Environment variable holding the api-key.
    pub api_key_env: Option<String>,
    /// Actual API key value stored in credentials.toml (mode 0600).
    /// Fallback when the env var named by `api_key_env` is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Extraction pipeline settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionConfig {
    /// Maximum tokens per chunk sent to the LLM.
    pub max_chunk_tokens: usize,
    /// Minimum confidence threshold for extracted claims.
    pub min_confidence: f64,
    /// Whether to extract relations in addition to claims and entities.
    pub extract_relations: bool,
    /// Maximum retries per extraction request.
    pub max_retries: u32,
    /// Number of chunks to pack into a single LLM batch call.
    /// When set, overrides the automatic per-model calculation.
    /// Automatic sizing uses context window + output cap for the configured model.
    /// Set to 1 to disable batching entirely.
    #[serde(default)]
    pub extraction_batch_size: Option<usize>,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            max_chunk_tokens: 2000,
            min_confidence: 0.5,
            extract_relations: true,
            max_retries: 3,
            extraction_batch_size: None,
        }
    }
}

/// Compilation settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilationConfig {
    /// Which artifact types to generate.
    pub enabled_artifacts: Vec<String>,
    /// Output directory for artifact files (relative to .thinkingroot/).
    pub output_dir: String,
}

impl Default for CompilationConfig {
    fn default() -> Self {
        Self {
            enabled_artifacts: vec![
                "entity_page".to_string(),
                "architecture_map".to_string(),
                "contradiction_report".to_string(),
                "health_report".to_string(),
            ],
            output_dir: "artifacts".to_string(),
        }
    }
}

/// Verification settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationConfig {
    /// Days after which a claim is considered stale.
    pub staleness_days: u32,
    /// Minimum freshness score to pass verification.
    pub min_freshness: f64,
    /// Whether to auto-resolve contradictions when signals are clear (>80%).
    pub auto_resolve: bool,
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            staleness_days: 90,
            min_freshness: 0.5,
            auto_resolve: true,
        }
    }
}

/// Parser configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParserConfig {
    /// File extensions to include (empty = all supported).
    pub include_extensions: Vec<String>,
    /// Glob patterns to exclude.
    pub exclude_patterns: Vec<String>,
    /// Whether to respect .gitignore rules.
    pub respect_gitignore: bool,
    /// Maximum file size in bytes to parse.
    pub max_file_size: u64,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            include_extensions: vec![],
            exclude_patterns: vec![
                "target/**".to_string(),
                "node_modules/**".to_string(),
                ".git/**".to_string(),
                ".thinkingroot/**".to_string(),
                "*.lock".to_string(),
                "package-lock.json".to_string(),
                "*.min.js".to_string(),
                "*.min.css".to_string(),
                "*.map".to_string(),
                "dist/**".to_string(),
                ".next/**".to_string(),
                "build/**".to_string(),
            ],
            respect_gitignore: true,
            max_file_size: 1_048_576, // 1 MB
        }
    }
}

/// Configuration for knowledge branch merge behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConfig {
    /// Maximum allowed health score drop during merge (default: 0.05 = 5%).
    #[serde(default = "MergeConfig::default_max_health_drop")]
    pub max_health_drop: f64,
    /// Block merge if unresolved contradictions remain (default: true).
    #[serde(default = "MergeConfig::default_block_on_contradictions")]
    pub block_on_contradictions: bool,
    /// Auto-resolve contradictions when confidence delta exceeds this threshold (default: 0.15).
    #[serde(default = "MergeConfig::default_auto_resolve_threshold")]
    pub auto_resolve_threshold: f64,
    /// Require human approval before merge (default: false).
    #[serde(default)]
    pub require_approval: bool,
}

impl MergeConfig {
    fn default_max_health_drop() -> f64 {
        0.05
    }
    fn default_block_on_contradictions() -> bool {
        true
    }
    fn default_auto_resolve_threshold() -> f64 {
        0.15
    }
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            max_health_drop: Self::default_max_health_drop(),
            block_on_contradictions: Self::default_block_on_contradictions(),
            auto_resolve_threshold: Self::default_auto_resolve_threshold(),
            require_approval: false,
        }
    }
}

/// Configuration for agent session streaming behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamsConfig {
    /// When `true`, each new MCP agent session automatically creates a
    /// `stream/{session_id}` branch so contributed claims are isolated.
    /// Disabled by default — enables safe multi-agent write concurrency.
    #[serde(default)]
    pub auto_session_branch: bool,
}

/// Configuration for a single source connector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    pub name: String,
    pub source_type: String,
    pub path: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_merged_uses_global_llm_when_workspace_has_no_llm_section() {
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

    // ── is_configured() ──────────────────────────────────────────

    #[test]
    fn default_llm_config_is_not_configured() {
        // After removing hardcoded defaults, bare Config::default() must be
        // unconfigured — so `root compile` without `root setup` fails fast.
        let config = Config::default();
        assert!(!config.llm.is_configured());
        assert_eq!(config.llm.default_provider, "");
        assert_eq!(config.llm.extraction_model, "");
    }

    #[test]
    fn llm_config_is_configured_when_provider_and_model_set() {
        let llm = LlmConfig {
            default_provider: "openai".to_string(),
            extraction_model: "gpt-4o".to_string(),
            compilation_model: "gpt-4o".to_string(),
            max_concurrent_requests: 5,
            request_timeout_secs: 60,
            providers: ProvidersConfig::default(),
        };
        assert!(llm.is_configured());
    }

    #[test]
    fn llm_config_not_configured_when_provider_empty() {
        let llm = LlmConfig {
            default_provider: String::new(),
            extraction_model: "gpt-4o".to_string(),
            compilation_model: "gpt-4o".to_string(),
            max_concurrent_requests: 5,
            request_timeout_secs: 60,
            providers: ProvidersConfig::default(),
        };
        assert!(!llm.is_configured());
    }

    #[test]
    fn llm_config_not_configured_when_model_empty() {
        let llm = LlmConfig {
            default_provider: "openai".to_string(),
            extraction_model: String::new(),
            compilation_model: String::new(),
            max_concurrent_requests: 5,
            request_timeout_secs: 60,
            providers: ProvidersConfig::default(),
        };
        assert!(!llm.is_configured());
    }

    // ── non-LLM defaults still correct ───────────────────────────

    #[test]
    fn default_non_llm_config_is_valid() {
        let config = Config::default();
        assert_eq!(config.extraction.max_chunk_tokens, 2000);
        assert!(config.parsers.respect_gitignore);
    }

    // ── merge_with_global: credential inheritance ─────────────────

    #[test]
    fn workspace_llm_inherits_global_credentials() {
        use crate::config::ProviderConfig;
        use crate::global_config::{GlobalConfig, ServeConfig};

        let global = GlobalConfig {
            llm: LlmConfig {
                default_provider: "openai".to_string(),
                extraction_model: "gpt-4o".to_string(),
                compilation_model: "gpt-4o".to_string(),
                max_concurrent_requests: 5,
                request_timeout_secs: 60,
                providers: ProvidersConfig {
                    openai: Some(ProviderConfig {
                        api_key_env: Some("OPENAI_API_KEY".to_string()),
                        api_key: None,
                        base_url: Some("https://api.openai.com".to_string()),
                        default_model: None,
                    }),
                    ..Default::default()
                },
            },
            serve: ServeConfig::default(),
        };

        // Workspace overrides provider+model but has no credentials
        let workspace_toml = r#"
[llm]
default_provider = "openai"
extraction_model = "gpt-4o-mini"
compilation_model = "gpt-4o-mini"
max_concurrent_requests = 5
request_timeout_secs = 60
"#;
        let merged = Config::merge_with_global(
            toml::from_str(workspace_toml).unwrap(),
            workspace_toml,
            &global,
        );
        // Workspace model wins
        assert_eq!(merged.llm.extraction_model, "gpt-4o-mini");
        // Global credentials inherited
        assert_eq!(
            merged
                .llm
                .providers
                .openai
                .as_ref()
                .unwrap()
                .api_key_env
                .as_deref(),
            Some("OPENAI_API_KEY")
        );
    }

    #[test]
    fn workspace_provider_overrides_global_provider() {
        use crate::config::ProviderConfig;
        use crate::global_config::{GlobalConfig, ServeConfig};

        let global = GlobalConfig {
            llm: LlmConfig {
                default_provider: "openai".to_string(),
                extraction_model: "gpt-4o".to_string(),
                compilation_model: "gpt-4o".to_string(),
                max_concurrent_requests: 5,
                request_timeout_secs: 60,
                providers: ProvidersConfig {
                    openai: Some(ProviderConfig {
                        api_key_env: Some("OPENAI_API_KEY".to_string()),
                        api_key: None,
                        base_url: None,
                        default_model: None,
                    }),
                    anthropic: Some(ProviderConfig {
                        api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                        api_key: None,
                        base_url: None,
                        default_model: None,
                    }),
                    ..Default::default()
                },
            },
            serve: ServeConfig::default(),
        };

        // Workspace switches to anthropic locally
        let workspace_toml = r#"
[llm]
default_provider = "anthropic"
extraction_model = "claude-sonnet-4-6"
compilation_model = "claude-sonnet-4-6"
max_concurrent_requests = 5
request_timeout_secs = 60
"#;
        let merged = Config::merge_with_global(
            toml::from_str(workspace_toml).unwrap(),
            workspace_toml,
            &global,
        );
        assert_eq!(merged.llm.default_provider, "anthropic");
        assert_eq!(merged.llm.extraction_model, "claude-sonnet-4-6");
        // Both provider credential slots inherited from global
        assert!(merged.llm.providers.anthropic.is_some());
        assert!(merged.llm.providers.openai.is_some());
    }

    #[test]
    fn no_llm_section_in_workspace_means_global_wins_entirely() {
        use crate::global_config::{GlobalConfig, ServeConfig};

        let global = GlobalConfig {
            llm: LlmConfig {
                default_provider: "groq".to_string(),
                extraction_model: "llama-3.1-8b-instant".to_string(),
                compilation_model: "llama-3.1-8b-instant".to_string(),
                max_concurrent_requests: 5,
                request_timeout_secs: 30,
                providers: ProvidersConfig::default(),
            },
            serve: ServeConfig::default(),
        };

        let workspace_toml = r#"
[workspace]
name = "test"
"#;
        let merged = Config::merge_with_global(
            toml::from_str(workspace_toml).unwrap(),
            workspace_toml,
            &global,
        );
        assert_eq!(merged.llm.default_provider, "groq");
        assert_eq!(merged.llm.extraction_model, "llama-3.1-8b-instant");
    }

    #[test]
    fn config_roundtrip_toml() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.llm.default_provider, config.llm.default_provider);
    }

    #[test]
    fn azure_config_roundtrip_toml() {
        let toml = r#"
[llm]
default_provider = "azure"
extraction_model = "gpt-4o-mini-deploy"
compilation_model = "gpt-4o-mini-deploy"
max_concurrent_requests = 5
request_timeout_secs = 120

[llm.providers.azure]
resource_name = "my-company-openai"
deployment = "gpt-4o-mini-deploy"
api_version = "2024-02-01"
api_key_env = "AZURE_OPENAI_API_KEY"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let azure = config.llm.providers.azure.as_ref().unwrap();
        assert_eq!(azure.resource_name.as_deref(), Some("my-company-openai"));
        assert_eq!(azure.deployment.as_deref(), Some("gpt-4o-mini-deploy"));
        assert_eq!(azure.api_version.as_deref(), Some("2024-02-01"));
        assert_eq!(azure.api_key_env.as_deref(), Some("AZURE_OPENAI_API_KEY"));

        // Roundtrip
        let out = toml::to_string_pretty(&config).unwrap();
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            reparsed
                .llm
                .providers
                .azure
                .as_ref()
                .unwrap()
                .resource_name
                .as_deref(),
            Some("my-company-openai")
        );
    }

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
            config
                .llm
                .providers
                .openrouter
                .as_ref()
                .unwrap()
                .api_key_env
                .as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            config
                .llm
                .providers
                .together
                .as_ref()
                .unwrap()
                .api_key_env
                .as_deref(),
            Some("TOGETHER_API_KEY")
        );
        assert_eq!(
            config
                .llm
                .providers
                .perplexity
                .as_ref()
                .unwrap()
                .api_key_env
                .as_deref(),
            Some("PERPLEXITY_API_KEY")
        );
        assert_eq!(
            config
                .llm
                .providers
                .litellm
                .as_ref()
                .unwrap()
                .base_url
                .as_deref(),
            Some("http://localhost:4000")
        );
        assert_eq!(
            config
                .llm
                .providers
                .custom
                .as_ref()
                .unwrap()
                .base_url
                .as_deref(),
            Some("https://my-endpoint.com/v1")
        );
    }
}
