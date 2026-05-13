//! `root setup` — thin alias for `root doctor --fix --interactive`.
//!
//! Slice B Task 12 collapsed the old multi-step setup wizard into the
//! doctor substrate: there is now exactly one diagnose-and-repair
//! surface. The provider/credential helpers below are kept because
//! `provider_cmd.rs` shares the catalogue + validators with the
//! Slice E credential-wizard work.

use std::time::Duration;

use anyhow::Context as _;
use dialoguer::{Input, Select, theme::ColorfulTheme};

// ── Provider catalogue ───────────────────────────────────────────

pub(crate) struct ProviderDef {
    pub(crate) label: &'static str,
    pub(crate) id: &'static str,
    pub(crate) default_env: &'static str,
    pub(crate) base_url: Option<&'static str>,
    pub(crate) validate_url: Option<&'static str>,
}

pub(crate) static PROVIDERS: &[ProviderDef] = &[
    ProviderDef {
        label: "OpenRouter  (200+ models, one key — recommended)",
        id: "openrouter",
        default_env: "OPENROUTER_API_KEY",
        base_url: Some("https://openrouter.ai/api/v1"),
        validate_url: Some("https://openrouter.ai/api/v1/models"),
    },
    ProviderDef {
        label: "OpenAI",
        id: "openai",
        default_env: "OPENAI_API_KEY",
        base_url: Some("https://api.openai.com"),
        validate_url: Some("https://api.openai.com/v1/models"),
    },
    ProviderDef {
        label: "Azure OpenAI  (enterprise, Microsoft Azure)",
        id: "azure",
        default_env: "AZURE_OPENAI_API_KEY",
        base_url: None,
        validate_url: None,
    },
    ProviderDef {
        label: "Anthropic",
        id: "anthropic",
        default_env: "ANTHROPIC_API_KEY",
        base_url: None,
        validate_url: Some("https://api.anthropic.com/v1/models"),
    },
    ProviderDef {
        label: "AWS Bedrock  (enterprise, no data leaves AWS)",
        id: "bedrock",
        default_env: "",
        base_url: None,
        validate_url: None,
    },
    ProviderDef {
        label: "Ollama      (local, free)",
        id: "ollama",
        default_env: "",
        base_url: Some("http://localhost:11434"),
        validate_url: None,
    },
    ProviderDef {
        label: "Groq        (ultra-fast inference)",
        id: "groq",
        default_env: "GROQ_API_KEY",
        base_url: Some("https://api.groq.com/openai"),
        validate_url: Some("https://api.groq.com/openai/v1/models"),
    },
    ProviderDef {
        label: "Together AI",
        id: "together",
        default_env: "TOGETHER_API_KEY",
        base_url: Some("https://api.together.xyz/v1"),
        validate_url: Some("https://api.together.xyz/v1/models"),
    },
    ProviderDef {
        label: "DeepSeek",
        id: "deepseek",
        default_env: "DEEPSEEK_API_KEY",
        base_url: Some("https://api.deepseek.com"),
        validate_url: Some("https://api.deepseek.com/models"),
    },
    ProviderDef {
        label: "Perplexity",
        id: "perplexity",
        default_env: "PERPLEXITY_API_KEY",
        base_url: Some("https://api.perplexity.ai"),
        validate_url: Some("https://api.perplexity.ai/models"),
    },
    ProviderDef {
        label: "LiteLLM     (self-hosted proxy)",
        id: "litellm",
        default_env: "LITELLM_API_KEY",
        base_url: Some("http://localhost:4000"),
        validate_url: None,
    },
    ProviderDef {
        label: "Custom      (any OpenAI-compatible endpoint)",
        id: "custom",
        default_env: "CUSTOM_LLM_API_KEY",
        base_url: None,
        validate_url: None,
    },
    // Slice 2 Task 8: ThinkingRoot Cloud managed-model provider. The
    // bearer token comes from `~/.config/thinkingroot/auth.json`, not
    // an env var — so `default_env` is empty (same convention as
    // Bedrock + Ollama). `base_url` is None because the cloud server
    // URL also lives in auth.json (`Config.server`), keyed off
    // `root login`. `validate_url` stays None because validation
    // routes through the signed-in check in Task 9's
    // `set_provider_cloud_managed`, not a public `/models` GET.
    //
    // Spec: docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md §6.6.
    ProviderDef {
        label: "ThinkingRoot Cloud  (managed, no API key — sign in with `root login`)",
        id: "thinkingroot-cloud",
        default_env: "",
        base_url: None,
        validate_url: None,
    },
];

// ── Main entry point ─────────────────────────────────────────────

pub async fn run_setup() -> anyhow::Result<()> {
    let report = crate::doctor::run_doctor(crate::doctor::DoctorMode::FixInteractive).await?;
    print!("{}", crate::doctor::format::to_terminal(&report));
    let _ = crate::doctor::fix::apply_all(&report.checks, true);
    Ok(())
}

// ── Bedrock credential probe ─────────────────────────────────────

/// Returns true if AWS credentials are available (file or env).
pub(crate) fn bedrock_credentials_found() -> bool {
    // Env var credentials
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() {
        return true;
    }
    // Named profile
    if std::env::var("AWS_PROFILE").is_ok() {
        return true;
    }
    // Credentials file (~/.aws/credentials or ~/.aws/config)
    if let Some(home) = dirs::home_dir() {
        if home.join(".aws").join("credentials").exists() {
            return true;
        }
        if home.join(".aws").join("config").exists() {
            return true;
        }
    }
    false
}

// ── Azure validation ─────────────────────────────────────────────

/// Validate Azure AOAI credentials by sending a minimal 1-token inference request.
/// Returns Ok if the credentials are accepted (HTTP 2xx or 5xx); Err on 401/403/404.
pub(crate) async fn validate_azure(
    resource: &str,
    deployment: &str,
    api_version: &str,
    key: &str,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let url = format!(
        "https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
    );

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "temperature": 0,
    });

    let resp = client
        .post(&url)
        .header("api-key", key)
        .json(&body)
        .send()
        .await
        .context("network error reaching Azure endpoint")?;

    match resp.status().as_u16() {
        401 | 403 => anyhow::bail!("invalid API key (HTTP {})", resp.status().as_u16()),
        404 => anyhow::bail!(
            "resource or deployment not found (HTTP 404). \
             Check resource name '{}' and deployment '{}'.",
            resource,
            deployment
        ),
        _ => Ok(()),
    }
}

// ── Credentials persistence ──────────────────────────────────────

/// Write a single credential to `~/.config/thinkingroot/credentials.toml`.
/// Silently skips empty values (e.g. Ollama / Bedrock).
/// Errors are printed but not fatal — the user can always set an env var as fallback.
pub(crate) fn persist_credential(env_var: &str, value: &str) {
    use console::style;
    use thinkingroot_core::global_config::Credentials;
    if value.is_empty() {
        return;
    }
    let mut creds = Credentials::load().unwrap_or_default();
    creds.set(env_var, value);
    if let Err(e) = creds.save() {
        eprintln!(
            "  {} Could not save credential to credentials.toml: {e}",
            style("!").yellow().bold()
        );
        eprintln!(
            "  Add {} to your shell profile as a fallback.",
            style(format!("export {env_var}=\"...\"")).cyan()
        );
    }
}

// ── Live model fetching ───────────────────────────────────────────

/// Maximum number of models shown in the interactive picker.
/// Keeps the terminal list manageable even for providers with 200+ models.
const MODEL_LIST_LIMIT: usize = 30;

/// Fetch the live model list for a provider.
/// Returns `None` on any error (network, auth, parse, timeout) — callers fall back to catalogue.
pub(crate) async fn fetch_provider_models(
    pdef: &ProviderDef,
    api_key: &str,
) -> Option<Vec<String>> {
    // Ollama is local — uses a tag-listing endpoint, no API key
    if pdef.id == "ollama" {
        let base = pdef.base_url.unwrap_or("http://localhost:11434");
        return fetch_ollama_models(base).await;
    }

    // All other fetchable providers expose their list at validate_url
    let url = pdef.validate_url?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;

    let req = if pdef.id == "anthropic" {
        client
            .get(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        client
            .get(url)
            .header("Authorization", format!("Bearer {api_key}"))
    };

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let json: serde_json::Value = resp.json().await.ok()?;

    // Together AI returns a bare array; every other provider wraps in {"data": [...]}
    let items = if let Some(arr) = json.as_array() {
        arr.clone()
    } else {
        json["data"].as_array()?.clone()
    };

    let mut models: Vec<String> = items
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            // OpenAI: drop non-chat models (embeddings, whisper, tts, dall-e, etc.)
            if pdef.id == "openai" && is_non_chat_openai(&id) {
                return None;
            }
            // Together AI: only include chat-type models
            if pdef.id == "together" && m["type"].as_str() != Some("chat") {
                return None;
            }
            Some(id)
        })
        .collect();

    models.sort();
    models.dedup();
    models.truncate(MODEL_LIST_LIMIT);

    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

fn is_non_chat_openai(id: &str) -> bool {
    let id = id.to_lowercase();
    id.contains("embed")
        || id.contains("whisper")
        || id.contains("tts")
        || id.contains("dall-e")
        || id.contains("moderation")
        || id.contains("realtime")
        || id.starts_with("babbage")
        || id.starts_with("davinci")
        || id.starts_with("text-ada")
}

async fn fetch_ollama_models(base_url: &str) -> Option<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;

    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let json: serde_json::Value = resp.json().await.ok()?;
    let models: Vec<String> = json["models"]
        .as_array()?
        .iter()
        .filter_map(|m| m["name"].as_str().map(str::to_string))
        .collect();

    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

// ── Model selection helper ────────────────────────────────────────

pub(crate) fn select_model_from_list(
    theme: &ColorfulTheme,
    default_models: &[&str],
) -> anyhow::Result<String> {
    if !default_models.is_empty() {
        let mut items: Vec<&str> = default_models.to_vec();
        items.push("Enter model ID manually");
        let idx = Select::with_theme(theme)
            .with_prompt("Extraction model")
            .items(&items)
            .default(0)
            .interact()?;
        if idx == items.len() - 1 {
            return Ok(Input::with_theme(theme)
                .with_prompt("Model ID")
                .interact_text()?);
        }
        return Ok(items[idx].to_string());
    }
    Ok(Input::with_theme(theme)
        .with_prompt("Model ID")
        .interact_text()?)
}

// ── Key validation (generic providers) ───────────────────────────

/// Validate an API key by GETting the provider's /models endpoint.
/// Returns Ok(()) if the key is accepted (HTTP 2xx, 404, or 405 — key valid, endpoint may differ).
/// Returns Err if HTTP 401/403 (bad key) or network error.
pub(crate) async fn validate_key_http(
    url: &str,
    provider_id: &str,
    key: &str,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let mut req = client.get(url);
    req = if provider_id == "anthropic" {
        req.header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
    } else {
        req.header("Authorization", format!("Bearer {}", key))
    };

    let resp = req
        .send()
        .await
        .context("network error during key validation")?;

    match resp.status().as_u16() {
        401 | 403 => anyhow::bail!("Invalid API key (HTTP {})", resp.status().as_u16()),
        _ => Ok(()),
    }
}
