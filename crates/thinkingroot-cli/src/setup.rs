use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use indicatif::{ProgressBar, ProgressStyle};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};
use thinkingroot_core::global_config::{GlobalConfig, ServeConfig};
use thinkingroot_core::config::{LlmConfig, ProviderConfig, ProvidersConfig};

// ── Provider catalogue ───────────────────────────────────────────

struct ProviderDef {
    label: &'static str,
    id: &'static str,
    default_env: &'static str,
    base_url: Option<&'static str>,
    default_models: &'static [&'static str],
    validate_url: Option<&'static str>,
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
        base_url: Some("https://api.groq.com/openai"),
        default_models: &["llama-3.1-8b-instant", "mixtral-8x7b-32768"],
        validate_url: Some("https://api.groq.com/openai/v1/models"),
    },
    ProviderDef {
        label: "AWS Bedrock  (enterprise, no data leaves AWS)",
        id: "bedrock",
        default_env: "",
        base_url: None,
        default_models: &["amazon.nova-micro-v1:0", "anthropic.claude-3-haiku-20240307-v1:0"],
        validate_url: None,
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
        base_url: None,
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
            let path_str: String = Input::with_theme(theme)
                .with_prompt("Path")
                .interact_text()?;
            let path = PathBuf::from(path_str);
            crate::workspace::run_workspace_add(path, None, None)?;
        }
        2 => {
            let port = WorkspaceRegistry::load()
                .ok()
                .and_then(|r| r.workspaces.first().map(|w| w.port))
                .unwrap_or(3000);
            crate::mcp_config::run_connect(None, port, false, false)?;
        }
        3 => {
            if let Some(p) = GlobalConfig::path() {
                if p.exists() { std::fs::remove_file(&p)?; }
            }
            if let Some(p) = WorkspaceRegistry::path() {
                if p.exists() { std::fs::remove_file(&p)?; }
            }
            Box::pin(run_setup()).await?;
        }
        _ => {}
    }

    Ok(())
}

// ── Provider helpers ─────────────────────────────────────────────

async fn configure_provider(
    theme: &ColorfulTheme,
    provider: &ProviderDef,
) -> anyhow::Result<(String, String)> {
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
        Input::with_theme(theme)
            .with_prompt("Model ID")
            .interact_text()?
    };

    Ok((api_key, model))
}

fn set_provider_config(llm: &mut LlmConfig, provider: &ProviderDef, api_key: &str) {
    if provider.default_env.is_empty() {
        return;
    }
    let env_var = provider.default_env;
    // Set the key as an env var for this process so it's usable immediately
    // SAFETY: single-threaded at this point in setup; no other threads reading env
    unsafe { std::env::set_var(env_var, api_key); }

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

    req = if provider_id == "anthropic" {
        req.header("x-api-key", key)
           .header("anthropic-version", "2023-06-01")
    } else {
        req.header("Authorization", format!("Bearer {}", key))
    };

    let resp = req.send().await.context("network error during key validation")?;

    match resp.status().as_u16() {
        401 | 403 => anyhow::bail!("Invalid API key (HTTP {})", resp.status().as_u16()),
        _ => Ok(()),
    }
}

fn print_banner() {
    println!();
    println!("  {}", style("ThinkingRoot").green().bold());
    println!("  {}", style("First-time setup").dim());
    println!();
}
