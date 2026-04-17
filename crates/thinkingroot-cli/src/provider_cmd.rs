use std::path::Path;
use std::time::Duration;

use console::style;
use dialoguer::{Input, Password, theme::ColorfulTheme};
use indicatif::{ProgressBar, ProgressStyle};

use thinkingroot_core::config::{
    AzureConfig, BedrockConfig, Config, LlmConfig, ProviderConfig, ProvidersConfig,
};
use thinkingroot_core::global_config::GlobalConfig;

use crate::setup::{
    PROVIDERS, ProviderDef, bedrock_credentials_found, fetch_provider_models,
    select_model_from_list, validate_azure, validate_key_http,
};

// ── root provider list ────────────────────────────────────────────

pub async fn run_provider_list(workspace_path: &Path) -> anyhow::Result<()> {
    let global = GlobalConfig::load().unwrap_or_else(|e| {
        eprintln!("  Warning: could not load global config, using defaults: {e}");
        GlobalConfig::default()
    });

    // Check for workspace override.
    let ws_config_path = workspace_path.join(".thinkingroot").join("config.toml");
    let local_config = if ws_config_path.exists() {
        Config::load_merged(workspace_path).ok()
    } else {
        None
    };

    // Effective = workspace override if present and different from global.
    let global_provider = global.llm.default_provider.as_str();
    let global_model = global.llm.extraction_model.as_str();

    let (effective_provider, effective_model, ws_override) = match &local_config {
        Some(lc)
            if lc.llm.default_provider != global_provider
                || lc.llm.extraction_model != global_model =>
        {
            (
                lc.llm.default_provider.as_str(),
                lc.llm.extraction_model.as_str(),
                true,
            )
        }
        _ => (global_provider, global_model, false),
    };

    println!();
    println!("  {}", style("Available providers").white().bold());
    println!();

    for p in PROVIDERS {
        let is_active = p.id == effective_provider;
        let marker = if is_active {
            style("▶").green().bold().to_string()
        } else {
            style(" ").dim().to_string()
        };
        let id_col = if is_active {
            style(p.id).green().bold().to_string()
        } else {
            style(p.id).white().to_string()
        };
        let model_hint = if is_active {
            format!("  {}", style(effective_model).dim())
        } else {
            String::new()
        };
        println!("  {} {:<12}{}", marker, id_col, model_hint);
    }

    println!();
    if let Some(path) = GlobalConfig::path() {
        println!(
            "  {}: {}",
            style("Config").dim(),
            style(path.display()).dim()
        );
    }
    if ws_override {
        println!(
            "  {}: {} {} (overrides global: {})",
            style("Workspace").dim(),
            style(effective_provider).green().bold(),
            style("← active here").dim(),
            style(global_provider).dim(),
        );
    }
    println!();
    println!("  Switch:  {}", style("root provider use <name>").cyan());
    println!("  Details: {}", style("root provider status").dim());
    println!();

    Ok(())
}

// ── root provider status ──────────────────────────────────────────

pub async fn run_provider_status(workspace_path: &Path) -> anyhow::Result<()> {
    let global = GlobalConfig::load().unwrap_or_else(|e| {
        eprintln!("  Warning: could not load global config, using defaults: {e}");
        GlobalConfig::default()
    });

    // Check if there's a local workspace override
    let ws_config_path = workspace_path.join(".thinkingroot").join("config.toml");
    let local_config = if ws_config_path.exists() {
        Config::load_merged(workspace_path).ok()
    } else {
        None
    };

    let has_local_llm = local_config
        .as_ref()
        .map(|c| {
            c.llm.default_provider != global.llm.default_provider
                || c.llm.extraction_model != global.llm.extraction_model
        })
        .unwrap_or(false);

    let (effective_provider, effective_model, source) = if has_local_llm {
        let c = local_config.as_ref().unwrap();
        (
            c.llm.default_provider.as_str(),
            c.llm.extraction_model.as_str(),
            "workspace",
        )
    } else {
        (
            global.llm.default_provider.as_str(),
            global.llm.extraction_model.as_str(),
            "global",
        )
    };

    println!();
    println!("  {}", style("Provider status").white().bold());
    println!();
    println!(
        "  {:<16} {}",
        style("Provider:").dim(),
        style(effective_provider).green().bold()
    );
    println!(
        "  {:<16} {}",
        style("Model:").dim(),
        style(effective_model).white()
    );
    println!(
        "  {:<16} {}",
        style("Config source:").dim(),
        style(source).dim()
    );
    println!();

    // Credential check
    let pdef = PROVIDERS.iter().find(|p| p.id == effective_provider);
    if let Some(p) = pdef {
        if p.id == "bedrock" {
            let found = bedrock_credentials_found();
            let status = if found {
                style("✓ found").green().to_string()
            } else {
                style("✗ not found — run `aws configure`").red().to_string()
            };
            println!(
                "  {:<16} AWS credentials  {}",
                style("Credentials:").dim(),
                status
            );
        } else if p.id == "ollama" {
            let reachable = ping_ollama().await;
            let status = if reachable {
                style("✓ running").green().to_string()
            } else {
                style("✗ not running — start with `ollama serve`")
                    .yellow()
                    .to_string()
            };
            println!(
                "  {:<16} localhost:11434  {}",
                style("Ollama:").dim(),
                status
            );
        } else if !p.default_env.is_empty() {
            let env_var = p.default_env;
            let is_set = std::env::var(env_var).is_ok();
            let status = if is_set {
                style("✓ set").green().to_string()
            } else {
                format!(
                    "{}  →  export {}=<your-key>",
                    style("✗ not set").red(),
                    style(env_var).cyan()
                )
            };
            println!(
                "  {:<16} {}  {}",
                style("Key env:").dim(),
                style(env_var).cyan(),
                status
            );
        }
    }

    println!();
    if has_local_llm {
        println!(
            "  {} Workspace override active at {}",
            style("ℹ").cyan(),
            style(ws_config_path.display()).dim()
        );
        println!(
            "    Remove {} section from that file to use global provider.",
            style("[llm]").cyan()
        );
    } else if let Some(path) = GlobalConfig::path() {
        println!(
            "  {:<16} {}",
            style("Global config:").dim(),
            style(path.display()).dim()
        );
    }
    println!();

    Ok(())
}

async fn ping_ollama() -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    client
        .get("http://localhost:11434/api/tags")
        .send()
        .await
        .is_ok()
}

// ── root provider use ─────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn run_provider_use(
    name: &str,
    model: Option<&str>,
    key: Option<&str>,
    base_url: Option<&str>,
    local: bool,
    workspace_path: &Path,
    no_validate: bool,
    azure_resource: Option<&str>,
    azure_deployment: Option<&str>,
    azure_api_version: Option<&str>,
) -> anyhow::Result<()> {
    let theme = ColorfulTheme::default();

    // ── 1. Resolve provider from catalogue ───────────────────────
    let pdef = PROVIDERS.iter().find(|p| p.id == name);
    if pdef.is_none() {
        let known: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        eprintln!();
        eprintln!(
            "  {} Unknown provider '{}'.",
            style("✗").red(),
            style(name).yellow()
        );
        eprintln!();
        eprintln!("  Known providers: {}", known.join(", "));
        eprintln!();
        eprintln!("  For any OpenAI-compatible endpoint, use:");
        eprintln!(
            "    {}",
            style("root provider use custom --base-url <url> --model <model>").cyan()
        );
        eprintln!();
        anyhow::bail!("unknown provider '{name}'");
    }
    let pdef = pdef.unwrap();

    println!();
    println!(
        "  {} {}",
        style("Switching to:").white(),
        style(name).green().bold()
    );
    println!();

    // ── 2. Collect credentials & build updated LlmConfig ─────────
    //
    // LOCAL mode: credentials always live in global config.
    // We only store provider + model in the workspace file.
    // No credential prompts, no validation — just pick a model.
    //
    // GLOBAL mode: full credential collection as usual.
    let new_llm = if local {
        let model_str = resolve_model(&theme, model, &[])?;
        base_llm_config(name, &model_str)
        // providers block is intentionally empty — write_to_workspace never writes it
    } else {
        match name {
            "bedrock" => collect_bedrock(&theme, model).await?,
            "azure" => {
                collect_azure(
                    &theme,
                    model,
                    key,
                    no_validate,
                    azure_resource,
                    azure_deployment,
                    azure_api_version,
                )
                .await?
            }
            "ollama" => collect_ollama(&theme, model, base_url).await?,
            _ => collect_generic(&theme, pdef, model, key, base_url, no_validate).await?,
        }
    };

    // ── 3. Write to global or local config ───────────────────────
    if local {
        write_to_workspace(workspace_path, new_llm.clone())?;
        let cfg_path = workspace_path.join(".thinkingroot").join("config.toml");
        println!();
        println!("  {} Workspace config updated", style("✓").green().bold());
        println!("  {}", style(cfg_path.display()).dim());
        println!(
            "  {} Provider/model overridden for this workspace. Credentials from global config.",
            style("ℹ").cyan()
        );
    } else {
        write_to_global(new_llm.clone())?;
        println!();
        println!("  {} Global config updated", style("✓").green().bold());
        if let Some(p) = GlobalConfig::path() {
            println!("  {}", style(p.display()).dim());
        }
        println!(
            "  {} Applies to all workspaces that don't override provider locally.",
            style("ℹ").cyan()
        );
    }

    // ── 4. Summary ────────────────────────────────────────────────
    println!();
    println!(
        "  {:<14} {}",
        style("Provider:").dim(),
        style(name).green().bold()
    );
    println!(
        "  {:<14} {}",
        style("Model:").dim(),
        style(&new_llm.extraction_model).white()
    );

    // For global switches on env-var providers: remind user to export the key.
    if !local && !pdef.default_env.is_empty() && name != "bedrock" && name != "ollama" {
        let env_var = pdef.default_env;
        if std::env::var(env_var).is_err() {
            println!();
            println!(
                "  {} Set your API key:",
                style("Action needed:").yellow().bold()
            );
            println!(
                "    {}",
                style(format!("export {env_var}=<your-key>")).cyan()
            );
            println!(
                "  Add this to your {} to persist it.",
                style("~/.zshrc or ~/.bashrc").dim()
            );
        }
    }

    println!();
    println!("  Ready. Run: {}", style("root compile .").cyan());
    println!();

    Ok(())
}

// ── Bedrock collection ────────────────────────────────────────────

async fn collect_bedrock(theme: &ColorfulTheme, model: Option<&str>) -> anyhow::Result<LlmConfig> {
    if !bedrock_credentials_found() {
        println!(
            "  {} AWS credentials not found.",
            style("!").yellow().bold()
        );
        println!();
        println!("  Configure them with:");
        println!("    Option A — AWS CLI:  {}", style("aws configure").cyan());
        println!(
            "    Option B — env vars: {} + {}",
            style("AWS_ACCESS_KEY_ID").cyan(),
            style("AWS_SECRET_ACCESS_KEY").cyan()
        );
        println!();

        Input::<String>::with_theme(theme)
            .with_prompt("Press Enter after configuring AWS credentials")
            .default(String::new())
            .allow_empty(true)
            .interact_text()?;

        if !bedrock_credentials_found() {
            anyhow::bail!("AWS credentials not found. Run `aws configure` then retry.");
        }
    }
    println!("  {} AWS credentials found.", style("✓").green());
    println!();

    let region: String = Input::with_theme(theme)
        .with_prompt("AWS region")
        .default("us-east-1".to_string())
        .interact_text()?;

    let model_str = if let Some(m) = model {
        m.to_string()
    } else {
        println!(
            "  {} Use cross-region inference IDs for ~4x higher quota.",
            style("Tip:").dim()
        );
        println!(
            "  {}  claude  →  us.anthropic.claude-haiku-4-5-20251001-v1:0",
            style("  e.g.").dim()
        );
        println!(
            "  {}  nova    →  us.amazon.nova-micro-v1:0",
            style("      ").dim()
        );
        println!();
        Input::<String>::with_theme(theme)
            .with_prompt("Model ID")
            .interact_text()?
    };

    let mut llm = base_llm_config("bedrock", &model_str);
    llm.providers.bedrock = Some(BedrockConfig {
        region: Some(region),
        profile: None,
    });
    Ok(llm)
}

// ── Azure collection ──────────────────────────────────────────────

async fn collect_azure(
    theme: &ColorfulTheme,
    model: Option<&str>,
    key: Option<&str>,
    no_validate: bool,
    resource_hint: Option<&str>,
    deployment_hint: Option<&str>,
    api_version_hint: Option<&str>,
) -> anyhow::Result<LlmConfig> {
    let pdef = PROVIDERS.iter().find(|p| p.id == "azure").unwrap();

    println!("  {}", style("Azure OpenAI").bold());
    println!("  Requires an Azure OpenAI resource with a deployed model.\n");

    // Resource name: --azure-resource flag, or prompt interactively.
    let resource: String = if let Some(r) = resource_hint {
        r.to_string()
    } else {
        Input::with_theme(theme)
            .with_prompt("Azure resource name  (e.g., my-company-openai)")
            .interact_text()?
    };

    // Deployment name: --azure-deployment flag, or prompt.
    let deployment: String = if let Some(d) = deployment_hint {
        d.to_string()
    } else {
        Input::with_theme(theme)
            .with_prompt("Deployment name      (e.g., gpt-4o-mini-deploy)")
            .interact_text()?
    };

    // API version: --azure-api-version flag, or prompt with default.
    let api_version: String = if let Some(v) = api_version_hint {
        v.to_string()
    } else {
        Input::with_theme(theme)
            .with_prompt("API version")
            .default("2024-02-01".to_string())
            .interact_text()?
    };

    let api_key = if let Some(k) = key {
        k.to_string()
    } else {
        Password::with_theme(theme)
            .with_prompt("Azure API key")
            .interact()?
    };

    if !no_validate {
        let pb = spinner("Validating Azure endpoint...");
        match validate_azure(&resource, &deployment, &api_version, &api_key).await {
            Ok(()) => {
                pb.finish_with_message(format!("{} Azure endpoint valid", style("✓").green()))
            }
            Err(e) => {
                pb.finish_with_message(format!("{} Validation failed", style("✗").red()));
                anyhow::bail!("Azure validation failed: {e}");
            }
        }
    }

    // Set the env var in this process so it's usable immediately.
    unsafe {
        std::env::set_var(pdef.default_env, &api_key);
    }

    let model_str = resolve_model(theme, model, &[&deployment])?;

    // Persist key to credentials.toml so future commands work in a fresh shell.
    crate::setup::persist_credential(pdef.default_env, &api_key);
    let mut llm = base_llm_config("azure", &model_str);
    llm.providers.azure = Some(AzureConfig {
        resource_name: Some(resource),
        endpoint_base: None,
        deployment: Some(deployment),
        api_version: Some(api_version),
        api_key_env: Some(pdef.default_env.to_string()),
        api_key: None, // stored in credentials.toml, not config.toml
    });
    Ok(llm)
}

// ── Ollama collection ─────────────────────────────────────────────

async fn collect_ollama(
    theme: &ColorfulTheme,
    model: Option<&str>,
    base_url: Option<&str>,
) -> anyhow::Result<LlmConfig> {
    let pdef = PROVIDERS.iter().find(|p| p.id == "ollama").unwrap();
    let effective_base = base_url.unwrap_or("http://localhost:11434");

    let live_models = if model.is_none() {
        let pb = spinner("Fetching installed Ollama models...");
        let result = fetch_provider_models(pdef, "").await;
        pb.finish_and_clear();
        result
    } else {
        None
    };
    let effective: Vec<&str> = live_models
        .as_deref()
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let model_str = resolve_model(theme, model, &effective)?;

    let mut llm = base_llm_config("ollama", &model_str);
    llm.providers.ollama = Some(ProviderConfig {
        api_key_env: None,
        api_key: None,
        base_url: Some(effective_base.to_string()),
        default_model: None,
    });
    Ok(llm)
}

// ── Generic (OpenAI-compatible) collection ────────────────────────

async fn collect_generic(
    theme: &ColorfulTheme,
    pdef: &ProviderDef,
    model: Option<&str>,
    key: Option<&str>,
    base_url: Option<&str>,
    no_validate: bool,
) -> anyhow::Result<LlmConfig> {
    // ── API key ───────────────────────────────────────────────────
    let api_key = if pdef.default_env.is_empty() {
        // No-auth provider (shouldn't happen for generic, but guard)
        String::new()
    } else if let Some(k) = key {
        // Key passed via --key flag
        unsafe {
            std::env::set_var(pdef.default_env, k);
        }
        k.to_string()
    } else if let Ok(k) = std::env::var(pdef.default_env) {
        // Already set in environment
        println!(
            "  {} {} is already set in environment.",
            style("✓").green(),
            style(pdef.default_env).cyan()
        );
        k
    } else {
        // Prompt interactively
        println!(
            "  {} {} is not set.",
            style("!").yellow(),
            style(pdef.default_env).cyan()
        );
        let k: String = Password::with_theme(theme)
            .with_prompt(format!(
                "{} API key",
                pdef.label.split_whitespace().next().unwrap_or(pdef.id)
            ))
            .interact()?;
        unsafe {
            std::env::set_var(pdef.default_env, &k);
        }
        k
    };

    // ── Validate key ──────────────────────────────────────────────
    if !no_validate
        && !api_key.is_empty()
        && let Some(validate_url) = pdef.validate_url
    {
        let pb = spinner("Validating key...");
        match validate_key_http(validate_url, pdef.id, &api_key).await {
            Ok(()) => pb.finish_with_message(format!("{} Key valid", style("✓").green())),
            Err(e) => {
                pb.finish_with_message(format!("{} Validation failed", style("✗").red()));
                anyhow::bail!(
                    "Key validation failed: {e}\nCheck your key or use --no-validate to skip."
                );
            }
        }
    }

    // ── Base URL ──────────────────────────────────────────────────
    // --base-url flag overrides catalogue default (useful for custom/litellm/self-hosted)
    let effective_base = base_url.or(pdef.base_url).map(str::to_string);

    // For custom provider, base_url is required
    if pdef.id == "custom" && effective_base.is_none() {
        anyhow::bail!(
            "The 'custom' provider requires --base-url.\n  Example: root provider use custom --base-url https://my.api.com/v1 --model my-model"
        );
    }

    // ── Model ─────────────────────────────────────────────────────
    // If --model was passed, use it directly. Otherwise fetch the live list.
    let live_models = if model.is_none() {
        let pb = spinner("Fetching available models...");
        let result = fetch_provider_models(pdef, &api_key).await;
        pb.finish_and_clear();
        result
    } else {
        None
    };
    let effective: Vec<&str> = live_models
        .as_deref()
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let model_str = resolve_model(theme, model, &effective)?;

    // ── Build LlmConfig ───────────────────────────────────────────
    // Persist key to credentials.toml so future commands work in a fresh shell.
    if !pdef.default_env.is_empty() && !api_key.is_empty() {
        crate::setup::persist_credential(pdef.default_env, &api_key);
    }
    let provider_cfg = ProviderConfig {
        api_key_env: if pdef.default_env.is_empty() {
            None
        } else {
            Some(pdef.default_env.to_string())
        },
        api_key: None, // stored in credentials.toml, not config.toml
        base_url: effective_base,
        default_model: None,
    };

    let mut llm = base_llm_config(pdef.id, &model_str);
    match pdef.id {
        "openrouter" => llm.providers.openrouter = Some(provider_cfg),
        "openai" => llm.providers.openai = Some(provider_cfg),
        "anthropic" => llm.providers.anthropic = Some(provider_cfg),
        "groq" => llm.providers.groq = Some(provider_cfg),
        "together" => llm.providers.together = Some(provider_cfg),
        "deepseek" => llm.providers.deepseek = Some(provider_cfg),
        "perplexity" => llm.providers.perplexity = Some(provider_cfg),
        "litellm" => llm.providers.litellm = Some(provider_cfg),
        "custom" => llm.providers.custom = Some(provider_cfg),
        _ => {}
    }
    Ok(llm)
}

// ── Config writers ────────────────────────────────────────────────

/// Update global config surgically: only the active-provider identity fields
/// and the new provider's credential slot. All other provider entries are
/// preserved so switching between providers doesn't require re-configuring them.
fn write_to_global(new_llm: LlmConfig) -> anyhow::Result<()> {
    let mut global = GlobalConfig::load()?;

    global.llm.default_provider = new_llm.default_provider.clone();
    global.llm.extraction_model = new_llm.extraction_model;
    global.llm.compilation_model = new_llm.compilation_model;
    // Apply provider-specific timeout; leave max_concurrent_requests untouched
    // so users who have tuned their concurrency don't get it reset on every switch.
    global.llm.request_timeout_secs = new_llm.request_timeout_secs;

    // Merge only the new provider's credentials — leave all others intact.
    merge_provider_slot(
        &mut global.llm.providers,
        &new_llm.providers,
        &new_llm.default_provider,
    );

    global.save()?;
    Ok(())
}

/// Update workspace config: only provider identity + model + timeout.
/// NEVER writes credentials — those always live in global config.
/// At runtime, the merged config picks up global credentials automatically.
fn write_to_workspace(workspace_path: &Path, new_llm: LlmConfig) -> anyhow::Result<()> {
    let mut config = Config::load(workspace_path)?;

    config.llm.default_provider = new_llm.default_provider;
    config.llm.extraction_model = new_llm.extraction_model;
    config.llm.compilation_model = new_llm.compilation_model;
    config.llm.request_timeout_secs = new_llm.request_timeout_secs;
    // Intentionally NO merge_provider_slot — credentials stay in global only.

    config.save(workspace_path)?;
    Ok(())
}

/// Copy only the one relevant provider slot from `incoming` into `existing`.
/// Every other slot in `existing` is left completely untouched.
fn merge_provider_slot(
    existing: &mut ProvidersConfig,
    incoming: &ProvidersConfig,
    provider_id: &str,
) {
    match provider_id {
        "bedrock" if incoming.bedrock.is_some() => existing.bedrock = incoming.bedrock.clone(),
        "azure" if incoming.azure.is_some() => existing.azure = incoming.azure.clone(),
        "openai" if incoming.openai.is_some() => existing.openai = incoming.openai.clone(),
        "anthropic" if incoming.anthropic.is_some() => {
            existing.anthropic = incoming.anthropic.clone()
        }
        "ollama" if incoming.ollama.is_some() => existing.ollama = incoming.ollama.clone(),
        "groq" if incoming.groq.is_some() => existing.groq = incoming.groq.clone(),
        "together" if incoming.together.is_some() => existing.together = incoming.together.clone(),
        "deepseek" if incoming.deepseek.is_some() => existing.deepseek = incoming.deepseek.clone(),
        "openrouter" if incoming.openrouter.is_some() => {
            existing.openrouter = incoming.openrouter.clone()
        }
        "perplexity" if incoming.perplexity.is_some() => {
            existing.perplexity = incoming.perplexity.clone()
        }
        "litellm" if incoming.litellm.is_some() => existing.litellm = incoming.litellm.clone(),
        "custom" if incoming.custom.is_some() => existing.custom = incoming.custom.clone(),
        _ => {}
    }
}

// ── root provider set-model ───────────────────────────────────────

pub fn run_provider_set_model(
    model: &str,
    local: bool,
    workspace_path: &Path,
) -> anyhow::Result<()> {
    if local {
        let mut config = Config::load(workspace_path)?;
        config.llm.extraction_model = model.to_string();
        config.llm.compilation_model = model.to_string();
        config.save(workspace_path)?;

        let cfg_path = workspace_path.join(".thinkingroot").join("config.toml");
        println!();
        println!("  {} Model updated (workspace)", style("✓").green().bold());
        println!("  {}", style(cfg_path.display()).dim());
    } else {
        let mut global = GlobalConfig::load()?;
        global.llm.extraction_model = model.to_string();
        global.llm.compilation_model = model.to_string();
        global.save()?;

        println!();
        println!("  {} Model updated (global)", style("✓").green().bold());
        if let Some(p) = GlobalConfig::path() {
            println!("  {}", style(p.display()).dim());
        }
    }

    println!();
    println!("  {:<14} {}", style("Model:").dim(), style(model).white());
    println!();

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────

/// Build a minimal LlmConfig carrying only the new provider identity and
/// the per-provider timeout. The providers block starts empty; callers fill
/// in exactly one slot. write_to_global / write_to_workspace merge this into
/// the existing config rather than replacing it wholesale.
fn base_llm_config(provider: &str, model: &str) -> LlmConfig {
    LlmConfig {
        default_provider: provider.to_string(),
        extraction_model: model.to_string(),
        compilation_model: model.to_string(),
        max_concurrent_requests: 5, // preserved from existing config on write
        request_timeout_secs: provider_timeout_secs(provider),
        providers: ProvidersConfig::default(),
    }
}

/// Per-provider sensible request timeout.
///
/// Groq: ultra-fast inference (sub-second calls) — fail quickly on errors.
/// Anthropic / OpenAI: fast HTTP APIs — 60s is more than enough.
/// Bedrock: AWS SDK + cross-region routing — add headroom.
/// Ollama: local inference latency depends on hardware — be generous.
/// Everything else: 120s covers Azure, OpenRouter, Together, etc.
fn provider_timeout_secs(provider: &str) -> u64 {
    match provider {
        "groq" => 30,
        "anthropic" => 60,
        "openai" => 60,
        "ollama" => 300,
        "bedrock" => 180,
        _ => 120,
    }
}

/// Use `--model` if provided; otherwise show interactive list or prompt.
fn resolve_model(
    theme: &ColorfulTheme,
    model: Option<&str>,
    defaults: &[&str],
) -> anyhow::Result<String> {
    if let Some(m) = model {
        return Ok(m.to_string());
    }
    select_model_from_list(theme, defaults)
}

fn spinner(msg: &'static str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg);
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}
