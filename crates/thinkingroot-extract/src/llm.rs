use thinkingroot_core::config::{LlmConfig, ProviderConfig};
use thinkingroot_core::{Error, Result};

use crate::prompts;
use crate::schema::ExtractionResult;

// ── Provider Enum (enum dispatch — zero-cost, no dyn) ────────────

enum Provider {
    Bedrock(BedrockProvider),
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    Ollama(OllamaProvider),
}

impl Provider {
    async fn chat(&self, system: &str, user: &str) -> Result<String> {
        match self {
            Provider::Bedrock(p) => p.chat(system, user).await,
            Provider::OpenAi(p) => p.chat(system, user).await,
            Provider::Anthropic(p) => p.chat(system, user).await,
            Provider::Ollama(p) => p.chat(system, user).await,
        }
    }
}

// ── Bedrock Provider (AWS) ───────────────────────────────────────

struct BedrockProvider {
    client: aws_sdk_bedrockruntime::Client,
    model: String,
}

impl BedrockProvider {
    async fn new(model: &str, region: &str) -> Result<Self> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        let client = aws_sdk_bedrockruntime::Client::new(&config);
        Ok(Self {
            client,
            model: model.to_string(),
        })
    }

    async fn chat(&self, system: &str, user: &str) -> Result<String> {
        use aws_sdk_bedrockruntime::types::{
            ContentBlock, ConversationRole, InferenceConfiguration, Message, SystemContentBlock,
        };

        tracing::debug!("bedrock: sending request to {} (input ~{} chars)", self.model, user.len());
        let response = self
            .client
            .converse()
            .model_id(&self.model)
            .system(SystemContentBlock::Text(system.to_string()))
            .inference_config(
                InferenceConfiguration::builder()
                    .max_tokens(4096)
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
        tracing::debug!("bedrock: got response");

        let output = response.output().ok_or_else(|| Error::LlmProvider {
            provider: "bedrock".into(),
            message: "no output in response".into(),
        })?;

        match output {
            aws_sdk_bedrockruntime::types::ConverseOutput::Message(msg) => {
                for block in msg.content() {
                    if let ContentBlock::Text(text) = block {
                        return Ok(text.clone());
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

// ── OpenAI-compatible Provider ───────────────────────────────────

struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiProvider {
    fn new(api_key: &str, model: &str, base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.1,
        });

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::LlmProvider {
                provider: "openai".into(),
                message: e.to_string(),
            })?;

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "openai".into(),
            message: e.to_string(),
        })?;

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| Error::LlmProvider {
                provider: "openai".into(),
                message: format!("unexpected response: {json}"),
            })
    }
}

// ── Anthropic Provider ───────────────────────────────────────────

struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    fn new(api_key: &str, model: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            model: model.to_string(),
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 4096,
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

        let json: serde_json::Value = resp.json().await.map_err(|e| Error::LlmProvider {
            provider: "anthropic".into(),
            message: e.to_string(),
        })?;

        json["content"][0]["text"]
            .as_str()
            .map(|s| s.to_string())
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
}

impl OllamaProvider {
    fn new(model: &str, base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            model: model.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "stream": false,
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

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
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

// ── LLM Client (unified wrapper with retry) ─────────────────────

pub struct LlmClient {
    provider: Provider,
    max_retries: u32,
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
                Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
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
                Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
            }
            "deepseek" => {
                let key = resolve_key(config.providers.deepseek.as_ref(), "DEEPSEEK_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.deepseek.as_ref(),
                    "https://api.deepseek.com",
                );
                Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
            }
            "openrouter" => {
                let key = resolve_key(config.providers.openrouter.as_ref(), "OPENROUTER_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.openrouter.as_ref(),
                    "https://openrouter.ai/api/v1",
                );
                Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
            }
            "together" => {
                let key = resolve_key(config.providers.together.as_ref(), "TOGETHER_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.together.as_ref(),
                    "https://api.together.xyz/v1",
                );
                Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
            }
            "perplexity" => {
                let key = resolve_key(config.providers.perplexity.as_ref(), "PERPLEXITY_API_KEY")?;
                let base_url = resolve_base_url(
                    config.providers.perplexity.as_ref(),
                    "https://api.perplexity.ai",
                );
                Provider::OpenAi(OpenAiProvider::new(&key, &config.extraction_model, &base_url))
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
            other => {
                return Err(Error::MissingConfig(format!(
                    "unsupported provider: {other}. Supported: bedrock, openai, anthropic, ollama, groq, deepseek, openrouter, together, perplexity, litellm, custom"
                )));
            }
        };

        tracing::info!(
            "LLM provider: {} / {}",
            config.default_provider,
            config.extraction_model
        );

        Ok(Self {
            provider,
            max_retries: 3,
        })
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Extract knowledge from a chunk of text.
    pub async fn extract(&self, content: &str, context: &str) -> Result<ExtractionResult> {
        let user_prompt = prompts::build_extraction_prompt(content, context);

        let mut last_error = None;

        for attempt in 0..self.max_retries {
            match self
                .provider
                .chat(prompts::SYSTEM_PROMPT, &user_prompt)
                .await
            {
                Ok(text) => match parse_extraction_result(&text) {
                    Ok(result) => return Ok(result),
                    Err(e) => {
                        tracing::warn!(attempt = attempt + 1, "failed to parse LLM response: {e}");
                        last_error = Some(e);
                    }
                },
                Err(e) => {
                    tracing::warn!(attempt = attempt + 1, "LLM request failed: {e}");
                    last_error = Some(e);

                    if attempt < self.max_retries - 1 {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            500 * 2u64.pow(attempt),
                        ))
                        .await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::Extraction {
            source_id: String::new(),
            message: "all retries exhausted".to_string(),
        }))
    }
}

// ── Response parsing ─────────────────────────────────────────────

fn parse_extraction_result(text: &str) -> Result<ExtractionResult> {
    if let Ok(result) = serde_json::from_str::<ExtractionResult>(text) {
        return Ok(result);
    }

    let json_str = extract_json_from_text(text);
    serde_json::from_str::<ExtractionResult>(json_str).map_err(|e| Error::StructuredOutput {
        message: format!(
            "failed to parse extraction result: {e}\nRaw response: {}",
            &text[..text.len().min(200)]
        ),
    })
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
    fn resolve_key_uses_default_env_when_config_is_none() {
        unsafe { std::env::set_var("TEST_DEFAULT_KEY", "mykey"); }
        let result = resolve_key(None, "TEST_DEFAULT_KEY").unwrap();
        assert_eq!(result, "mykey");
        unsafe { std::env::remove_var("TEST_DEFAULT_KEY"); }
    }

    #[test]
    fn resolve_key_uses_config_env_when_set() {
        unsafe { std::env::set_var("MY_CUSTOM_ENV", "customkey"); }
        let cfg = thinkingroot_core::config::ProviderConfig {
            api_key_env: Some("MY_CUSTOM_ENV".to_string()),
            base_url: None,
            default_model: None,
        };
        let result = resolve_key(Some(&cfg), "IGNORED_DEFAULT").unwrap();
        assert_eq!(result, "customkey");
        unsafe { std::env::remove_var("MY_CUSTOM_ENV"); }
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
