//! BYOK provider catalog + inline wizard helpers.
//!
//! Mirrors the catalogue in `crates/thinkingroot-cli/src/setup.rs` so the
//! desktop wizard can offer the same provider picker as `root setup`
//! without depending on the CLI binary crate. Both lists must be kept
//! in sync when a new provider is added; the credential-vars whitelist
//! in `commands/settings.rs::CREDENTIAL_VARS` is the third sibling.
//!
//! v1 inline wizard intentionally excludes Azure + Bedrock — both need
//! multiple non-key fields (resource/deployment/region) that don't fit
//! a paste-and-validate flow. The user still gets a hint to open
//! Settings for those.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thinkingroot_core::{Credentials, GlobalConfig};

#[derive(Debug, Serialize, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    pub default_env: String,
    pub base_url: Option<String>,
    pub validate_url: Option<String>,
    /// `false` for Ollama (local, no key required).
    pub requires_key: bool,
}

struct Provider {
    id: &'static str,
    label: &'static str,
    default_env: &'static str,
    base_url: Option<&'static str>,
    validate_url: Option<&'static str>,
    requires_key: bool,
}

const PROVIDERS: &[Provider] = &[
    Provider {
        id: "openrouter",
        label: "OpenRouter — 200+ models, one key",
        default_env: "OPENROUTER_API_KEY",
        base_url: Some("https://openrouter.ai/api/v1"),
        validate_url: Some("https://openrouter.ai/api/v1/models"),
        requires_key: true,
    },
    Provider {
        id: "openai",
        label: "OpenAI",
        default_env: "OPENAI_API_KEY",
        base_url: Some("https://api.openai.com"),
        validate_url: Some("https://api.openai.com/v1/models"),
        requires_key: true,
    },
    Provider {
        id: "anthropic",
        label: "Anthropic",
        default_env: "ANTHROPIC_API_KEY",
        base_url: None,
        validate_url: Some("https://api.anthropic.com/v1/models"),
        requires_key: true,
    },
    Provider {
        id: "groq",
        label: "Groq — ultra-fast inference",
        default_env: "GROQ_API_KEY",
        base_url: Some("https://api.groq.com/openai"),
        validate_url: Some("https://api.groq.com/openai/v1/models"),
        requires_key: true,
    },
    Provider {
        id: "deepseek",
        label: "DeepSeek",
        default_env: "DEEPSEEK_API_KEY",
        base_url: Some("https://api.deepseek.com"),
        validate_url: Some("https://api.deepseek.com/models"),
        requires_key: true,
    },
    Provider {
        id: "together",
        label: "Together AI",
        default_env: "TOGETHER_API_KEY",
        base_url: Some("https://api.together.xyz/v1"),
        validate_url: Some("https://api.together.xyz/v1/models"),
        requires_key: true,
    },
    Provider {
        id: "perplexity",
        label: "Perplexity",
        default_env: "PERPLEXITY_API_KEY",
        base_url: Some("https://api.perplexity.ai"),
        validate_url: Some("https://api.perplexity.ai/models"),
        requires_key: true,
    },
    Provider {
        id: "ollama",
        label: "Ollama — local, free",
        default_env: "",
        base_url: Some("http://localhost:11434"),
        validate_url: None,
        requires_key: false,
    },
    // ThinkingRoot Cloud — managed models routed via the hub's
    // signed-in bearer. No API key; the catalogue comes from
    // `GET {server}/v1/models` (gateway). v1 catalogue exposes
    // tr-gpt-5.4 / tr-claude-sonnet-4.6 / etc. — see
    // `services/gateway/src/models.rs::default_catalogue` cloud-side.
    Provider {
        id: "thinkingroot-cloud",
        label: "ThinkingRoot Cloud — managed (gpt-5.4, claude, …)",
        default_env: "",
        base_url: None,
        validate_url: None,
        requires_key: false,
    },
];

#[tauri::command]
pub fn list_providers() -> Vec<ProviderInfo> {
    PROVIDERS
        .iter()
        .map(|p| ProviderInfo {
            id: p.id.to_string(),
            label: p.label.to_string(),
            default_env: p.default_env.to_string(),
            base_url: p.base_url.map(str::to_string),
            validate_url: p.validate_url.map(str::to_string),
            requires_key: p.requires_key,
        })
        .collect()
}

fn lookup(id: &str) -> Result<&'static Provider, String> {
    PROVIDERS
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("unknown provider `{id}`"))
}

#[derive(Debug, Deserialize)]
pub struct ProviderKeyArgs {
    pub provider_id: String,
    #[serde(default)]
    pub api_key: String,
}

#[tauri::command]
pub async fn provider_validate_key(args: ProviderKeyArgs) -> Result<(), String> {
    let pdef = lookup(&args.provider_id)?;
    let key = args.api_key.trim();

    if pdef.id == "thinkingroot-cloud" {
        // Managed mode — no key to validate, just the sign-in state.
        // The downstream `provider_fetch_models` call will fail loudly
        // if the bearer is missing or rejected by the hub.
        let cfg = thinkingroot_cloud_auth::config::load()
            .map_err(|e| format!("read auth.json: {e}"))?
            .unwrap_or_else(thinkingroot_cloud_auth::config::Config::empty);
        if !cfg.is_signed_in() {
            return Err("Sign in to ThinkingRoot first".into());
        }
        return Ok(());
    }

    if pdef.id == "ollama" {
        return validate_ollama(pdef.base_url.unwrap_or("http://localhost:11434")).await;
    }

    if pdef.requires_key && key.is_empty() {
        return Err("API key required".into());
    }

    let Some(url) = pdef.validate_url else {
        // No validation endpoint (Custom / LiteLLM-style). Accept the
        // key as-is; the engine will surface a real error on first use.
        return Ok(());
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let req = if pdef.id == "anthropic" {
        client
            .get(url)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
    } else {
        client.get(url).header("Authorization", format!("Bearer {key}"))
    };

    let resp = req
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    match resp.status().as_u16() {
        401 | 403 => Err("invalid API key — provider rejected it".into()),
        s if (200..300).contains(&s) || s == 404 || s == 405 => Ok(()),
        s => Err(format!("validation failed: HTTP {s}")),
    }
}

async fn validate_ollama(base_url: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Ollama not reachable at {base_url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Ollama returned HTTP {}", resp.status()));
    }
    Ok(())
}

#[tauri::command]
pub async fn provider_fetch_models(args: ProviderKeyArgs) -> Result<Vec<String>, String> {
    let pdef = lookup(&args.provider_id)?;
    let key = args.api_key.trim();

    if pdef.id == "thinkingroot-cloud" {
        // Cached 1h in auth.json by `models_catalogue::fetch_models`.
        // We surface only the IDs to keep the wizard dropdown clean;
        // pricing detail belongs in Settings (where the full catalogue
        // can be paginated). `force_refresh=false` is intentional —
        // the user just signed in or hit "Validate"; the freshness
        // window doesn't help them right now and a free 1h cache hit
        // shaves a network round-trip on the next wizard re-entry.
        let entries =
            thinkingroot_cloud_auth::models_catalogue::fetch_models(false)
                .await
                .map_err(|e| format!("fetch managed catalogue: {e}"))?;
        return Ok(entries.into_iter().map(|m| m.id).collect());
    }

    if pdef.id == "ollama" {
        return fetch_ollama_models(pdef.base_url.unwrap_or("http://localhost:11434")).await;
    }

    let Some(url) = pdef.validate_url else {
        return Ok(vec![]);
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let req = if pdef.id == "anthropic" {
        client
            .get(url)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
    } else {
        client.get(url).header("Authorization", format!("Bearer {key}"))
    };

    let resp = req
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "models endpoint returned HTTP {}",
            resp.status()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("parse models response: {e}"))?;
    let items = if let Some(arr) = json.as_array() {
        arr.clone()
    } else {
        json["data"].as_array().cloned().unwrap_or_default()
    };

    let mut models: Vec<String> = items
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            if pdef.id == "openai" && is_non_chat_openai(&id) {
                return None;
            }
            if pdef.id == "together" && m["type"].as_str() != Some("chat") {
                return None;
            }
            Some(id)
        })
        .collect();
    models.sort();
    models.dedup();
    models.truncate(30);
    Ok(models)
}

#[tauri::command]
pub async fn provider_fetch_models_stored(provider_id: String) -> Result<Vec<String>, String> {
    // Azure is configured via deployment fields in global config, not the
    // BYOK wizard catalogue (which needs resource/endpoint/deployment).
    if provider_id == "azure" {
        let cfg = GlobalConfig::load().map_err(|e| e.to_string())?;
        let mut models: Vec<String> = Vec::new();
        if let Some(ref azure) = cfg.llm.providers.azure {
            if let Some(ref deployment) = azure.deployment {
                let d = deployment.trim();
                if !d.is_empty() {
                    models.push(d.to_string());
                }
            }
        }
        for candidate in [&cfg.llm.extraction_model, &cfg.llm.compilation_model] {
            let c = candidate.trim();
            if !c.is_empty() && !models.iter().any(|m| m == c) {
                models.push(c.to_string());
            }
        }
        return Ok(models);
    }

    let pdef = lookup(&provider_id)?;
    let api_key = if pdef.default_env.is_empty() {
        String::new()
    } else {
        Credentials::load()
            .map_err(|e| e.to_string())?
            .get(pdef.default_env)
            .unwrap_or("")
            .to_string()
    };
    provider_fetch_models(ProviderKeyArgs {
        provider_id,
        api_key,
    })
    .await
}

#[tauri::command]
pub fn provider_set_active_model(model: String) -> Result<(), String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("model required".into());
    }
    let mut cfg = GlobalConfig::load().map_err(|e| e.to_string())?;
    cfg.llm.extraction_model = model.to_string();
    cfg.llm.compilation_model = model.to_string();
    cfg.save().map_err(|e| e.to_string())?;
    Ok(())
}

fn is_non_chat_openai(id: &str) -> bool {
    let l = id.to_lowercase();
    l.contains("embed")
        || l.contains("whisper")
        || l.contains("tts")
        || l.contains("dall-e")
        || l.contains("moderation")
        || l.contains("realtime")
        || l.starts_with("babbage")
        || l.starts_with("davinci")
        || l.starts_with("text-ada")
}

async fn fetch_ollama_models(base_url: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Ollama not reachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Ollama returned HTTP {}", resp.status()));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let models: Vec<String> = json["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
}

#[derive(Debug, Deserialize)]
pub struct ProviderSaveArgs {
    pub provider_id: String,
    #[serde(default)]
    pub api_key: String,
    pub default_model: String,
}

/// Atomic "finish wizard step" — save credential, point the engine at
/// this provider, and persist the user's model pick for both extraction
/// and compilation. The doctor `credentials.any_provider` check sees
/// the key on `credentials.toml`; `models.*` reads the resolved model
/// names from `GlobalConfig`. Calling `doctor_check` after this should
/// clear both rows.
#[tauri::command]
pub fn provider_save(args: ProviderSaveArgs) -> Result<(), String> {
    let pdef = lookup(&args.provider_id)?;

    if !pdef.default_env.is_empty() {
        let key = args.api_key.trim();
        if key.is_empty() {
            return Err("API key required".into());
        }
        let mut creds = Credentials::load().map_err(|e| e.to_string())?;
        creds.set(pdef.default_env, key);
        creds.save().map_err(|e| e.to_string())?;
    }

    let model = args.default_model.trim();
    if model.is_empty() {
        return Err("default model required".into());
    }
    let mut cfg = GlobalConfig::load().map_err(|e| e.to_string())?;
    cfg.llm.default_provider = pdef.id.to_string();
    cfg.llm.extraction_model = model.to_string();
    cfg.llm.compilation_model = model.to_string();
    cfg.save().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_unique() {
        let mut ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        ids.sort();
        let len_before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), len_before, "duplicate provider ids");
    }

    #[test]
    fn key_required_providers_carry_env_name() {
        for p in PROVIDERS {
            if p.requires_key {
                assert!(
                    !p.default_env.is_empty(),
                    "provider {} marked requires_key but has no default_env",
                    p.id
                );
            }
        }
    }

    #[test]
    fn ollama_does_not_require_key() {
        let p = PROVIDERS.iter().find(|p| p.id == "ollama").unwrap();
        assert!(!p.requires_key);
        assert_eq!(p.default_env, "");
    }
}
