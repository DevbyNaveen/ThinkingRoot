use std::sync::Arc;

use thinkingroot_core::config::{AzureConfig, LlmConfig, ProviderConfig};
use thinkingroot_core::{Error, Result};

use crate::prompts;
use crate::scheduler::{HeaderRateLimits, ThroughputScheduler};
use crate::schema::ExtractionResult;

/// Output of a single provider chat call.
struct ChatOutput {
    text: String,
    truncated: bool,
    /// Rate limit headers from the response (empty for Bedrock/Ollama).
    limits: HeaderRateLimits,
}

// ── Model-aware output token limits ─────────────────────────────

/// Returns the maximum output tokens for a known model.
/// Falls back to a conservative 8_192 for unknown models.
pub fn model_max_output_tokens(model: &str) -> i32 {
    let m = model.to_lowercase();

    // Claude Haiku 4.5 — 64k output
    if m.contains("haiku-4-5") || m.contains("haiku-4.5") {
        return 64_000;
    }
    // Claude Haiku 3 — 4k output
    if m.contains("haiku") {
        return 4_096;
    }
    // Claude Sonnet / Opus 4.x — 8k output
    if m.contains("sonnet") || m.contains("opus") {
        return 8_192;
    }
    // GPT-4.1 family (2025) — 32k output
    if m.contains("gpt-4.1") || m.contains("gpt-4-1") {
        return 32_768;
    }
    // GPT-4o family — 16k output
    if m.contains("gpt-4o") || m.contains("gpt-4-turbo") {
        return 16_384;
    }
    // GPT-3.5 — 4k output
    if m.contains("gpt-3.5") || m.contains("gpt-35") {
        return 4_096;
    }
    // Llama 3.x (Groq, Together, Ollama)
    if m.contains("llama-3") || m.contains("llama3") {
        return 8_192;
    }
    // Mistral / Mixtral
    if m.contains("mistral") || m.contains("mixtral") {
        return 8_192;
    }
    // DeepSeek
    if m.contains("deepseek") {
        return 8_192;
    }
    // Nova (Bedrock)
    if m.contains("nova") {
        return 5_120;
    }

    // Unknown model — safe default that works everywhere
    8_192
}

// ── Provider Enum (enum dispatch — zero-cost, no dyn) ────────────

enum Provider {
    Bedrock(BedrockProvider),
    Azure(AzureProvider),
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    Ollama(OllamaProvider),
}

impl Provider {
    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        match self {
            Provider::Bedrock(p) => p.chat(system, user).await,
            Provider::Azure(p) => p.chat(system, user).await,
            Provider::OpenAi(p) => p.chat(system, user).await,
            Provider::Anthropic(p) => p.chat(system, user).await,
            Provider::Ollama(p) => p.chat(system, user).await,
        }
    }

    fn model_name(&self) -> &str {
        match self {
            Provider::Bedrock(p) => &p.model,
            Provider::Azure(p) => &p.model,
            Provider::OpenAi(p) => &p.model,
            Provider::Anthropic(p) => &p.model,
            Provider::Ollama(p) => &p.model,
        }
    }

    fn provider_name(&self) -> &str {
        match self {
            Provider::Bedrock(_) => "bedrock",
            Provider::Azure(_) => "azure",
            Provider::OpenAi(p) => p.provider_name.as_str(),
            Provider::Anthropic(_) => "anthropic",
            Provider::Ollama(_) => "ollama",
        }
    }
}

// ── Bedrock Provider (AWS) ───────────────────────────────────────

struct BedrockProvider {
    client: aws_sdk_bedrockruntime::Client,
    model: String,
    max_output_tokens: i32,
}

impl BedrockProvider {
    async fn new(model: &str, region: &str) -> Result<Self> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        let client = aws_sdk_bedrockruntime::Client::new(&config);
        let max_output_tokens = model_max_output_tokens(model);
        Ok(Self {
            client,
            model: model.to_string(),
            max_output_tokens,
        })
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        use aws_sdk_bedrockruntime::types::{
            ContentBlock, ConversationRole, InferenceConfiguration, Message, SystemContentBlock,
        };

        tracing::debug!(
            "bedrock: sending request to {} (input ~{} chars, max_output={})",
            self.model,
            user.len(),
            self.max_output_tokens
        );

        let response = self
            .client
            .converse()
            .model_id(&self.model)
            .system(SystemContentBlock::Text(system.to_string()))
            .inference_config(
                InferenceConfiguration::builder()
                    .max_tokens(self.max_output_tokens)
                    .temperature(0.1_f32)
                    .build(),
            )
            .messages(
                Message::builder()
                    .role(ConversationRole::User)
                    .content(ContentBlock::Text(user.to_string()))
                    .build()
                    .map_err(|e| Error::LlmProvider {
                        provider: "bedrock".into(),
                        message: format!("failed to build message: {e}"),
                    })?,
            )
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: format!("bedrock/{}", self.model),
                message: e.to_string(),
            })?;

        // Detect truncation via stop reason.
        let truncated = matches!(
            response.stop_reason(),
            aws_sdk_bedrockruntime::types::StopReason::MaxTokens
        );

        if truncated {
            tracing::warn!(
                "bedrock: output truncated for model {} (hit {} token limit)",
                self.model,
                self.max_output_tokens
            );
        } else {
            tracing::debug!("bedrock: got complete response");
        }

        let output = response.output().ok_or_else(|| Error::LlmProvider {
            provider: "bedrock".into(),
            message: "no output in response".into(),
        })?;

        match output {
            aws_sdk_bedrockruntime::types::ConverseOutput::Message(msg) => {
                for block in msg.content() {
                    if let ContentBlock::Text(text) = block {
                        return Ok(ChatOutput {
                            text: text.clone(),
                            truncated,
                            limits: HeaderRateLimits::default(), // Bedrock uses SDK, no HTTP headers
                        });
                    }
                }
                Err(Error::LlmProvider {
                    provider: "bedrock".into(),
                    message: "no text in response".into(),
                })
            }
            _ => Err(Error::LlmProvider {
                provider: "bedrock".into(),
                message: "unexpected output type".into(),
            }),
        }
    }
}

// ── Azure OpenAI Provider ────────────────────────────────────────
// Auth: `api-key` header (not `Authorization: Bearer`).
// URL:  https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}
// The `model` field is OMITTED from the request body — it is implied by the deployment.

struct AzureProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,          // deployment name; used for display/logging
    endpoint_url: String,   // pre-built full URL with api-version query param
    max_output_tokens: i32,
}

impl AzureProvider {
    fn new(
        api_key: &str,
        model: &str,
        cfg: &AzureConfig,
    ) -> Result<Self> {
        let deployment = cfg.deployment.as_deref().ok_or_else(|| {
            Error::MissingConfig(
                "set [llm.providers.azure].deployment in your config".into(),
            )
        })?;
        let api_version = cfg.api_version.as_deref().unwrap_or("2024-12-01-preview");

        // endpoint_base overrides resource_name — used for AIServices/Foundry resources
        // that expose cognitiveservices.azure.com instead of openai.azure.com.
        let base = if let Some(base) = cfg.endpoint_base.as_deref() {
            base.trim_end_matches('/').to_string()
        } else {
            let resource = cfg.resource_name.as_deref().ok_or_else(|| {
                Error::MissingConfig(
                    "set [llm.providers.azure].resource_name or endpoint_base in your config".into(),
                )
            })?;
            format!("https://{resource}.openai.azure.com")
        };

        let endpoint_url = format!(
            "{base}/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
        );
        let max_output_tokens = model_max_output_tokens(model);

        Ok(Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            endpoint_url,
            max_output_tokens,
        })
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        // Azure AOAI: no `model` field in body — deployment is in the URL.
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user",   "content": user},
            ],
            "temperature": 0.1,
            "max_tokens": self.max_output_tokens,
        });

        let resp = self
            .client
            .post(&self.endpoint_url)
            .header("api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "azure".into(),
                message: e.to_string(),
            })?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: "azure".into(),
                retry_after_ms: retry_after,
            });
        }

        // Azure returns the same OpenAI rate-limit headers.
        let limits = HeaderRateLimits::from_headers(resp.headers());

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "azure".into(),
            message: e.to_string(),
        })?;

        let finish_reason = json["choices"][0]["finish_reason"].as_str().unwrap_or("");
        let truncated = finish_reason == "length";

        if truncated {
            tracing::warn!(
                "azure: output truncated for deployment {} (finish_reason=length, max_tokens={})",
                self.model,
                self.max_output_tokens,
            );
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| ChatOutput {
                text: s.to_string(),
                truncated,
                limits,
            })
            .ok_or_else(|| Error::LlmProvider {
                provider: "azure".into(),
                message: format!("unexpected response: {json}"),
            })
    }
}

// ── OpenAI-compatible Provider ───────────────────────────────────

struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    provider_name: String,
    max_output_tokens: i32,
}

impl OpenAiProvider {
    fn new(api_key: &str, model: &str, base_url: &str, provider_name: &str) -> Self {
        let max_output_tokens = model_max_output_tokens(model);
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            provider_name: provider_name.to_string(),
            max_output_tokens,
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.1,
            "max_tokens": self.max_output_tokens,
        });

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: self.provider_name.clone(),
                message: e.to_string(),
            })?;

        // Detect rate-limit before consuming body.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: self.provider_name.clone(),
                retry_after_ms: retry_after,
            });
        }

        // Capture rate limit headers before consuming body.
        let limits = HeaderRateLimits::from_headers(resp.headers());

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: self.provider_name.clone(),
            message: e.to_string(),
        })?;

        // Detect truncation via finish_reason.
        let finish_reason = json["choices"][0]["finish_reason"].as_str().unwrap_or("");
        let truncated = finish_reason == "length";

        if truncated {
            tracing::warn!(
                "{}: output truncated for model {} (finish_reason=length, max_tokens={})",
                self.provider_name,
                self.model,
                self.max_output_tokens
            );
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| ChatOutput {
                text: s.to_string(),
                truncated,
                limits,
            })
            .ok_or_else(|| Error::LlmProvider {
                provider: self.provider_name.clone(),
                message: format!("unexpected response: {json}"),
            })
    }
}

// ── Anthropic Provider ───────────────────────────────────────────

struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_output_tokens: i32,
}

impl AnthropicProvider {
    fn new(api_key: &str, model: &str) -> Self {
        let max_output_tokens = model_max_output_tokens(model);
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            max_output_tokens,
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_output_tokens,
            "temperature": 0.1,
            "system": system,
            "messages": [
                {"role": "user", "content": user},
            ],
        });

        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "anthropic".into(),
                message: e.to_string(),
            })?;

        // Detect rate-limit (429) or overloaded (529).
        let status = resp.status().as_u16();
        if status == 429 || status == 529 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(0);
            return Err(Error::RateLimited {
                provider: "anthropic".into(),
                retry_after_ms: retry_after,
            });
        }

        // Capture rate limit headers before consuming body.
        let limits = HeaderRateLimits::from_headers(resp.headers());

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "anthropic".into(),
            message: e.to_string(),
        })?;

        // Detect truncation via stop_reason.
        let stop_reason = json["stop_reason"].as_str().unwrap_or("");
        let truncated = stop_reason == "max_tokens";

        if truncated {
            tracing::warn!(
                "anthropic: output truncated for model {} (stop_reason=max_tokens, max_tokens={})",
                self.model,
                self.max_output_tokens
            );
        }

        json["content"][0]["text"]
            .as_str()
            .map(|s| ChatOutput {
                text: s.to_string(),
                truncated,
                limits,
            })
            .ok_or_else(|| Error::LlmProvider {
                provider: "anthropic".into(),
                message: format!("unexpected response: {json}"),
            })
    }
}

// ── Ollama Provider ──────────────────────────────────────────────

struct OllamaProvider {
    client: reqwest::Client,
    model: String,
    base_url: String,
    max_output_tokens: i32,
}

impl OllamaProvider {
    fn new(model: &str, base_url: &str) -> Self {
        let max_output_tokens = model_max_output_tokens(model);
        Self {
            client: reqwest::Client::new(),
            model: model.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            max_output_tokens,
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<ChatOutput> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "stream": false,
            "options": {
                "num_predict": self.max_output_tokens,
            },
        });

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "ollama".into(),
                message: e.to_string(),
            })?;

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "ollama".into(),
            message: e.to_string(),
        })?;

        let finish_reason = json["choices"][0]["finish_reason"].as_str().unwrap_or("");
        let truncated = finish_reason == "length";

        if truncated {
            tracing::warn!(
                "ollama: output truncated for model {} (finish_reason=length)",
                self.model
            );
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| ChatOutput {
                text: s.to_string(),
                truncated,
                limits: HeaderRateLimits::default(), // Ollama has no rate limits
            })
            .ok_or_else(|| Error::LlmProvider {
                provider: "ollama".into(),
                message: format!("unexpected response: {json}"),
            })
    }
}

// ── Provider config helpers ──────────────────────────────────────

fn resolve_key(cfg: Option<&ProviderConfig>, default_env: &str) -> Result<String> {
    let env_var = cfg
        .and_then(|p| p.api_key_env.as_deref())
        .unwrap_or(default_env);
    std::env::var(env_var).map_err(|_| {
        Error::MissingConfig(format!("set the {} environment variable", env_var))
    })
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
        .ok_or_else(|| {
            Error::MissingConfig(format!(
                "set [llm.providers.{provider}].base_url in your config"
            ))
        })
}


// ── LLM Client (unified wrapper with retry + truncation handling) ─

pub struct LlmClient {
    provider: Provider,
    max_retries: u32,
    /// Pre-emptive throughput scheduler — gates every send to stay under provider limits.
    pub(crate) scheduler: Option<Arc<ThroughputScheduler>>,
}

impl LlmClient {
    /// Create a new LLM client from config. Auto-detects provider.
    pub async fn new(config: &LlmConfig) -> Result<Self> {
        let provider = match config.default_provider.as_str() {
            "bedrock" => {
                let region = config
                    .providers
                    .bedrock
                    .as_ref()
                    .and_then(|b| b.region.as_deref())
                    .unwrap_or("us-east-1");
                Provider::Bedrock(BedrockProvider::new(&config.extraction_model, region).await?)
            }
            "openai" => {
                let key = resolve_key(config.providers.openai.as_ref(), "OPENAI_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.openai.as_ref(),
                    "https://api.openai.com",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "openai",
                ))
            }
            "azure" => {
                let azure_cfg = config.providers.azure.as_ref().ok_or_else(|| {
                    Error::MissingConfig(
                        "azure provider requires [llm.providers.azure] in your config".into(),
                    )
                })?;
                let key_env = azure_cfg.api_key_env.as_deref().unwrap_or("AZURE_OPENAI_API_KEY");
                let key = std::env::var(key_env).map_err(|_| {
                    Error::MissingConfig(format!("set the {key_env} environment variable"))
                })?;
                Provider::Azure(AzureProvider::new(&key, &config.extraction_model, azure_cfg)?)
            }
            "anthropic" => {
                let key = resolve_key(config.providers.anthropic.as_ref(), "ANTHROPIC_API_KEY")?;
                Provider::Anthropic(AnthropicProvider::new(&key, &config.extraction_model))
            }
            "ollama" => {
                let base_url = resolve_base_url(
                    config.providers.ollama.as_ref(),
                    "http://localhost:11434",
                );
                Provider::Ollama(OllamaProvider::new(&config.extraction_model, &base_url))
            }
            "groq" => {
                let key = resolve_key(config.providers.groq.as_ref(), "GROQ_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.groq.as_ref(),
                    "https://api.groq.com/openai",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "groq",
                ))
            }
            "deepseek" => {
                let key = resolve_key(config.providers.deepseek.as_ref(), "DEEPSEEK_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.deepseek.as_ref(),
                    "https://api.deepseek.com",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "deepseek",
                ))
            }
            "openrouter" => {
                let key =
                    resolve_key(config.providers.openrouter.as_ref(), "OPENROUTER_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.openrouter.as_ref(),
                    "https://openrouter.ai/api/v1",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "openrouter",
                ))
            }
            "together" => {
                let key = resolve_key(config.providers.together.as_ref(), "TOGETHER_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.together.as_ref(),
                    "https://api.together.xyz/v1",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "together",
                ))
            }
            "perplexity" => {
                let key =
                    resolve_key(config.providers.perplexity.as_ref(), "PERPLEXITY_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.perplexity.as_ref(),
                    "https://api.perplexity.ai",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "perplexity",
                ))
            }
            "litellm" => {
                let key = resolve_key_optional(config.providers.litellm.as_ref());
                let base_url = resolve_base_url(
                    config.providers.litellm.as_ref(),
                    "http://localhost:4000",
                );
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "litellm",
                ))
            }
            "custom" => {
                let key = resolve_key(config.providers.custom.as_ref(), "CUSTOM_LLM_API_KEY")?;
                let base_url =
                    resolve_base_url_required(config.providers.custom.as_ref(), "custom")?;
                Provider::OpenAi(OpenAiProvider::new(
                    &key,
                    &config.extraction_model,
                    &base_url,
                    "custom",
                ))
            }
            other => {
                return Err(Error::MissingConfig(format!(
                    "unsupported provider: {other}. Supported: bedrock, azure, openai, anthropic, ollama, groq, deepseek, openrouter, together, perplexity, litellm, custom"
                )));
            }
        };

        tracing::info!(
            "LLM provider: {} / {} (max_output_tokens={})",
            config.default_provider,
            config.extraction_model,
            model_max_output_tokens(&config.extraction_model),
        );

        Ok(Self {
            provider,
            max_retries: 3,
            scheduler: None,
        })
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn with_scheduler(mut self, s: Arc<ThroughputScheduler>) -> Self {
        self.scheduler = Some(s);
        self
    }

    /// Extract knowledge from a chunk of text.
    ///
    /// If the provider signals truncation, returns `Error::TruncatedOutput`
    /// so the caller can split the chunk and retry each half.
    ///
    /// **Rate-limit handling:** rate-limit errors (429, throttle, etc.)
    /// get up to `max_retries * 2` attempts with exponential backoff
    /// (1s → 2s → 4s → …, capped at 60s) plus random jitter.
    /// Non-rate-limit errors use the standard `max_retries` with shorter
    /// delays. When a rate-limit is detected and `AdaptiveConcurrency` is
    /// attached, the effective concurrency is also halved.
    pub async fn extract(&self, content: &str, context: &str) -> Result<ExtractionResult> {
        let user_prompt = prompts::build_extraction_prompt(content, context);
        let mut last_error = None;

        // Rate-limit errors get double the retries.
        let max_rl_retries = self.max_retries * 2;
        let mut rl_attempts: u32 = 0;
        let mut normal_attempts: u32 = 0;

        loop {
            // Stop if we've exhausted both budgets.
            if normal_attempts >= self.max_retries && rl_attempts >= max_rl_retries {
                break;
            }

            // Gate every send through the throughput scheduler.
            // This is the pre-emptive layer — prevents 429s from ever occurring.
            if let Some(ref sched) = self.scheduler {
                sched.wait_for_slot().await;
            }

            match self.provider.chat(prompts::SYSTEM_PROMPT, &user_prompt).await {
                Ok(output) => {
                    if output.truncated {
                        return Err(Error::TruncatedOutput {
                            provider: self.provider.provider_name().to_string(),
                            model: self.provider.model_name().to_string(),
                        });
                    }

                    // Record success: update rolling token average and recalibrate send rate.
                    // Include system prompt in the estimate — on TPM-bound providers,
                    // missing it makes the scheduler run hotter than it thinks.
                    let tokens = (prompts::SYSTEM_PROMPT.len()
                        + user_prompt.len()
                        + output.text.len()) as u64
                        / 4;
                    if let Some(ref sched) = self.scheduler {
                        sched.record_success(tokens, &output.limits).await;
                    }

                    match parse_extraction_result(&output.text) {
                        Ok(result) => {
                            return Ok(result);
                        }
                        Err(e) => {
                            normal_attempts += 1;
                            tracing::warn!(
                                attempt = normal_attempts,
                                max = self.max_retries,
                                "failed to parse LLM response: {e}"
                            );
                            last_error = Some(e);
                            if normal_attempts >= self.max_retries {
                                break;
                            }
                        }
                    }
                }
                Err(e) if e.is_rate_limited() => {
                    rl_attempts += 1;

                    // Safety net: scheduler should have prevented this, but providers
                    // can be inconsistent. Double the send interval immediately.
                    if let Some(ref sched) = self.scheduler {
                        sched.record_throttle();
                    }

                    // Get provider-suggested delay, or compute our own.
                    let provider_hint = match &e {
                        Error::RateLimited { retry_after_ms, .. } if *retry_after_ms > 0 => {
                            *retry_after_ms
                        }
                        _ => 0,
                    };

                    // Exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s, capped at 60s.
                    let backoff_ms = (1000u64 * 2u64.pow(rl_attempts.saturating_sub(1)))
                        .min(60_000);
                    let base_delay = if provider_hint > 0 { provider_hint } else { backoff_ms };

                    // Add jitter: ±25% random spread to prevent thundering herd.
                    let jitter = (base_delay as f64 * 0.25 * (rand_jitter() - 0.5)) as i64;
                    let delay = (base_delay as i64 + jitter).max(500) as u64;

                    tracing::warn!(
                        attempt = rl_attempts,
                        max = max_rl_retries,
                        delay_ms = delay,
                        "rate-limited by {} — backing off",
                        self.provider.provider_name()
                    );

                    last_error = Some(e);
                    if rl_attempts >= max_rl_retries {
                        break;
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Err(e) => {
                    normal_attempts += 1;
                    tracing::warn!(
                        attempt = normal_attempts,
                        max = self.max_retries,
                        "LLM request failed: {e}"
                    );
                    last_error = Some(e);
                    if normal_attempts >= self.max_retries {
                        break;
                    }

                    // Short backoff for non-rate-limit errors.
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * 2u64.pow(normal_attempts.saturating_sub(1)),
                    ))
                    .await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::Extraction {
            source_id: String::new(),
            message: "all retries exhausted".to_string(),
        }))
    }
}

/// Cheap pseudo-random jitter in [0.0, 2.0) — no external crate needed.
/// Uses the current time's nanosecond component as entropy source.
fn rand_jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Map nanoseconds to [0.0, 2.0)
    (nanos as f64 / u32::MAX as f64) * 2.0
}

// ── Response parsing ─────────────────────────────────────────────

fn parse_extraction_result(text: &str) -> Result<ExtractionResult> {
    if let Ok(result) = serde_json::from_str::<ExtractionResult>(text) {
        return Ok(result);
    }

    let json_str = extract_json_from_text(text);
    if let Ok(result) = serde_json::from_str::<ExtractionResult>(json_str) {
        return Ok(result);
    }

    // Some models (Nova, older Claude) emit trailing commas which are invalid JSON.
    // Strip them and retry before giving up.
    let cleaned = strip_trailing_commas(json_str);
    serde_json::from_str::<ExtractionResult>(&cleaned).map_err(|e| Error::StructuredOutput {
        message: format!(
            "failed to parse extraction result: {e}\nRaw response: {}",
            &text[..text.len().min(200)]
        ),
    })
}

/// Remove trailing commas before `]` or `}` — handles non-standard JSON from some LLMs.
/// Pure char scan, no regex dependency.
fn strip_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            // Peek ahead past whitespace to see if the next token closes an array/object.
            let mut j = i + 1;
            while j < bytes.len()
                && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r')
            {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b']' | b'}') {
                i += 1; // skip the comma
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extract_json_from_text(text: &str) -> &str {
    let text = text.trim();

    if let Some(start) = text.find("```json") {
        let content_start = start + 7;
        if let Some(end) = text[content_start..].find("```") {
            return text[content_start..content_start + end].trim();
        }
    }

    if let Some(start) = text.find("```") {
        let content_start = start + 3;
        let content_start = text[content_start..]
            .find('\n')
            .map(|i| content_start + i + 1)
            .unwrap_or(content_start);
        if let Some(end) = text[content_start..].find("```") {
            return text[content_start..content_start + end].trim();
        }
    }

    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            return &text[start..=end];
        }
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_max_tokens_haiku_45() {
        assert_eq!(model_max_output_tokens("eu.anthropic.claude-haiku-4-5-20251001-v1:0"), 64_000);
        assert_eq!(model_max_output_tokens("claude-haiku-4-5-20251001"), 64_000);
    }

    #[test]
    fn model_max_tokens_haiku_3() {
        assert_eq!(model_max_output_tokens("claude-3-haiku-20240307"), 4_096);
        assert_eq!(model_max_output_tokens("anthropic.claude-3-haiku-20240307-v1:0"), 4_096);
    }

    #[test]
    fn model_max_tokens_sonnet() {
        assert_eq!(model_max_output_tokens("claude-sonnet-4-6"), 8_192);
        assert_eq!(model_max_output_tokens("claude-3-5-sonnet-20241022"), 8_192);
    }

    #[test]
    fn model_max_tokens_gpt4o() {
        assert_eq!(model_max_output_tokens("gpt-4o"), 16_384);
        assert_eq!(model_max_output_tokens("gpt-4o-mini"), 16_384);
    }

    #[test]
    fn model_max_tokens_unknown_falls_back() {
        assert_eq!(model_max_output_tokens("some-unknown-model-v99"), 8_192);
    }

    #[test]
    fn resolve_key_uses_default_env_when_config_is_none() {
        unsafe {
            std::env::set_var("TEST_DEFAULT_KEY", "mykey");
        }
        let result = resolve_key(None, "TEST_DEFAULT_KEY").unwrap();
        assert_eq!(result, "mykey");
        unsafe {
            std::env::remove_var("TEST_DEFAULT_KEY");
        }
    }

    #[test]
    fn resolve_key_uses_config_env_when_set() {
        unsafe {
            std::env::set_var("MY_CUSTOM_ENV", "customkey");
        }
        let cfg = thinkingroot_core::config::ProviderConfig {
            api_key_env: Some("MY_CUSTOM_ENV".to_string()),
            base_url: None,
            default_model: None,
        };
        let result = resolve_key(Some(&cfg), "IGNORED_DEFAULT").unwrap();
        assert_eq!(result, "customkey");
        unsafe {
            std::env::remove_var("MY_CUSTOM_ENV");
        }
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

    #[test]
    fn parse_valid_json() {
        let json = r#"{"claims":[],"entities":[],"relations":[]}"#;
        let result = parse_extraction_result(json).unwrap();
        assert!(result.claims.is_empty());
    }

    #[test]
    fn parse_json_with_trailing_commas() {
        // Some LLMs (Nova, older Claude) emit trailing commas — must not fail.
        let json = "{\"claims\":[],\"entities\":[],\"relations\":[],}";
        let result = parse_extraction_result(json).unwrap();
        assert!(result.claims.is_empty());
    }

    #[test]
    fn parse_json_in_code_block() {
        let text =
            "Here's the result:\n```json\n{\"claims\":[],\"entities\":[],\"relations\":[]}\n```";
        let result = parse_extraction_result(text).unwrap();
        assert!(result.claims.is_empty());
    }

    #[test]
    fn extract_json_from_text_with_preamble() {
        let text =
            "Sure! Here is the extraction:\n\n{\"claims\":[],\"entities\":[],\"relations\":[]}";
        let result = parse_extraction_result(text).unwrap();
        assert!(result.claims.is_empty());
    }
}
