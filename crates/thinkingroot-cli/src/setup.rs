use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use console::style;
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};
use thinkingroot_core::global_config::{GlobalConfig, ServeConfig};
use thinkingroot_core::config::{AzureConfig, BedrockConfig, LlmConfig, ProviderConfig, ProvidersConfig};

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
        label: "Azure OpenAI  (enterprise, Microsoft Azure)",
        id: "azure",
        default_env: "AZURE_OPENAI_API_KEY",
        base_url: None,
        default_models: &["gpt-4o-mini", "gpt-4o", "gpt-35-turbo"],
        validate_url: None,
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
        label: "AWS Bedrock  (enterprise, no data leaves AWS)",
        id: "bedrock",
        default_env: "",
        base_url: None,
        default_models: &[
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "amazon.nova-micro-v1:0",
            "anthropic.claude-3-haiku-20240307-v1:0",
        ],
        validate_url: None,
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
        label: "Together AI",
        id: "together",
        default_env: "TOGETHER_API_KEY",
        base_url: Some("https://api.together.xyz/v1"),
        default_models: &[
            "meta-llama/Meta-Llama-3.1-8B-Instruct-Turbo",
            "mistralai/Mixtral-8x7B-Instruct-v0.1",
        ],
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
        default_models: &[
            "llama-3.1-sonar-small-128k-online",
            "llama-3.1-sonar-large-128k-online",
        ],
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

// ── Provider setup result (carries all collected data) ───────────

struct ProviderSetup {
    api_key: String,
    model: String,
    // Azure-specific (set only for provider id = "azure")
    azure_resource: Option<String>,
    azure_deployment: Option<String>,
    azure_api_version: Option<String>,
    // Bedrock-specific (set only for provider id = "bedrock")
    bedrock_region: Option<String>,
}

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
    let setup = configure_provider(&theme, provider).await?;

    // Build global config
    let mut llm = LlmConfig {
        default_provider: provider.id.to_string(),
        extraction_model: setup.model.clone(),
        compilation_model: setup.model.clone(),
        max_concurrent_requests: 5,
        request_timeout_secs: 120,
        providers: ProvidersConfig::default(),
    };
    set_provider_config(&mut llm, provider, &setup);

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

    // Compile — reuse the same world-class progress display used by `root compile`.
    if compile_now {
        println!("  Compiling {}...\n", abs_ws_path.display());
        match crate::progress::run_compile_progress(&abs_ws_path, None).await {
            Ok(result) => {
                println!(
                    "  {} {} claims · {} entities · {} relations\n",
                    style("✓").green().bold(),
                    result.claims_count,
                    result.entities_count,
                    result.relations_count,
                );
            }
            Err(e) => {
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
        style(
            GlobalConfig::path()
                .unwrap_or_else(|| std::path::PathBuf::from("~/.config/thinkingroot/config.toml"))
                .display()
                .to_string()
        ).dim()
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
            let setup = configure_provider(theme, provider).await?;
            let mut new_global = global.clone();
            new_global.llm.default_provider = provider.id.to_string();
            new_global.llm.extraction_model = setup.model.clone();
            new_global.llm.compilation_model = setup.model.clone();
            set_provider_config(&mut new_global.llm, provider, &setup);
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

// ── Provider dispatch ─────────────────────────────────────────────

async fn configure_provider(
    theme: &ColorfulTheme,
    provider: &ProviderDef,
) -> anyhow::Result<ProviderSetup> {
    match provider.id {
        "bedrock" => configure_bedrock(theme, provider).await,
        "azure"   => configure_azure(theme).await,
        _         => configure_generic(theme, provider).await,
    }
}

// ── Bedrock configure flow ────────────────────────────────────────

async fn configure_bedrock(
    theme: &ColorfulTheme,
    provider: &ProviderDef,
) -> anyhow::Result<ProviderSetup> {
    println!();
    println!("  {}", style("AWS Bedrock").bold());
    println!("  Runs inference inside your AWS account — no data leaves AWS.\n");

    // 1. Check for existing credentials
    if !bedrock_credentials_found() {
        println!("  {} AWS credentials not found.", style("!").yellow().bold());
        println!();
        println!("  Bedrock uses your AWS credentials. Configure them with:");
        println!();
        println!("    Option A — AWS CLI (recommended):");
        println!("      {}", style("aws configure").cyan());
        println!("      (install: https://aws.amazon.com/cli/)");
        println!();
        println!("    Option B — environment variables:");
        println!(
            "      {} = <your access key>",
            style("AWS_ACCESS_KEY_ID").cyan()
        );
        println!(
            "      {} = <your secret>",
            style("AWS_SECRET_ACCESS_KEY").cyan()
        );
        println!();

        // Wait for the user to set up credentials, then re-check.
        Input::<String>::with_theme(theme)
            .with_prompt("Press Enter after configuring AWS credentials")
            .default(String::new())
            .allow_empty(true)
            .interact_text()?;

        if !bedrock_credentials_found() {
            anyhow::bail!(
                "AWS credentials not found.\n  \
                 Run `aws configure` or set AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY, \
                 then re-run `root setup`."
            );
        }
    }
    println!("  {} AWS credentials found.", style("✓").green());

    // 2. Region
    println!();
    println!("  {} Use cross-region inference IDs (e.g. {}) for ~4x higher quota.", style("Tip:").dim(), style("us.anthropic.claude-haiku-4-5-20251001-v1:0").cyan());
    let region: String = Input::with_theme(theme)
        .with_prompt("AWS region")
        .default("us-east-1".to_string())
        .interact_text()?;

    // 3. Model
    let model = select_model_from_list(theme, provider.default_models)?;

    Ok(ProviderSetup {
        api_key: String::new(),
        model,
        azure_resource: None,
        azure_deployment: None,
        azure_api_version: None,
        bedrock_region: Some(region),
    })
}

/// Returns true if AWS credentials are available (file or env).
fn bedrock_credentials_found() -> bool {
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

// ── Azure configure flow ──────────────────────────────────────────

async fn configure_azure(theme: &ColorfulTheme) -> anyhow::Result<ProviderSetup> {
    println!();
    println!("  {}", style("Azure OpenAI").bold());
    println!("  Requires an Azure OpenAI resource with a deployed model.\n");
    println!(
        "  Find these in Azure Portal → {} → your resource.",
        style("Azure OpenAI").cyan()
    );
    println!();

    // Resource name
    let resource: String = Input::with_theme(theme)
        .with_prompt("Azure resource name  (e.g., my-company-openai)")
        .interact_text()?;

    // Deployment name
    let deployment: String = Input::with_theme(theme)
        .with_prompt("Deployment name      (e.g., gpt-4o-mini-deploy)")
        .interact_text()?;

    // API version
    let api_version: String = Input::with_theme(theme)
        .with_prompt("API version")
        .default("2024-02-01".to_string())
        .interact_text()?;

    // API key
    let key: String = Password::with_theme(theme)
        .with_prompt("Azure API key")
        .interact()?;

    // Validate endpoint
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::default_spinner()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message("Validating Azure endpoint...");
    pb.enable_steady_tick(Duration::from_millis(80));

    match validate_azure(&resource, &deployment, &api_version, &key).await {
        Ok(()) => pb.finish_with_message(format!("{} Azure endpoint valid", style("✓").green())),
        Err(e) => {
            pb.finish_with_message(format!("{} Validation failed", style("✗").red()));
            anyhow::bail!(
                "Azure validation failed: {e}\n\n  \
                 Check:\n  \
                 • Resource name: {resource}\n  \
                 • Deployment name: {deployment}\n  \
                 • API version: {api_version}\n  \
                 Re-run `root setup` to try again."
            );
        }
    }

    // The deployment name IS the model identifier for Azure.
    // Offer the deployment name as default so the user can override the display name.
    let model: String = Input::with_theme(theme)
        .with_prompt("Model name for display (usually your deployment name)")
        .default(deployment.clone())
        .interact_text()?;

    Ok(ProviderSetup {
        api_key: key,
        model,
        azure_resource: Some(resource),
        azure_deployment: Some(deployment),
        azure_api_version: Some(api_version),
        bedrock_region: None,
    })
}

/// Validate Azure AOAI credentials by sending a minimal 1-token inference request.
/// Returns Ok if the credentials are accepted (HTTP 2xx or 5xx); Err on 401/403/404.
async fn validate_azure(
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

// ── Generic (OpenAI-compatible) configure flow ────────────────────

async fn configure_generic(
    theme: &ColorfulTheme,
    provider: &ProviderDef,
) -> anyhow::Result<ProviderSetup> {
    let api_key = if !provider.default_env.is_empty() {
        let key: String = Password::with_theme(theme)
            .with_prompt(format!(
                "{} API key",
                provider.label.split_whitespace().next().unwrap_or(provider.id)
            ))
            .interact()?;

        if let Some(validate_url) = provider.validate_url {
            let pb = indicatif::ProgressBar::new_spinner();
            pb.set_style(
                indicatif::ProgressStyle::default_spinner()
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            pb.set_message("Validating key...");
            pb.enable_steady_tick(Duration::from_millis(80));

            match validate_key_http(validate_url, provider.id, &key).await {
                Ok(()) => pb.finish_with_message(format!("{} Key valid", style("✓").green())),
                Err(e) => {
                    pb.finish_with_message(format!("{} Validation failed", style("✗").red()));
                    anyhow::bail!(
                        "Key validation failed: {}\nRe-run `root setup` to try again.",
                        e
                    );
                }
            }
        }
        key
    } else {
        String::new()
    };

    let model = select_model_from_list(theme, provider.default_models)?;

    Ok(ProviderSetup {
        api_key,
        model,
        azure_resource: None,
        azure_deployment: None,
        azure_api_version: None,
        bedrock_region: None,
    })
}

// ── Config writer ─────────────────────────────────────────────────

fn set_provider_config(llm: &mut LlmConfig, provider: &ProviderDef, setup: &ProviderSetup) {
    match provider.id {
        "bedrock" => {
            llm.providers.bedrock = Some(BedrockConfig {
                region: setup.bedrock_region.clone(),
                profile: None,
            });
        }
        "azure" => {
            // Store the key in env immediately so it's usable in this process.
            // SAFETY: single-threaded at this point in setup.
            unsafe { std::env::set_var("AZURE_OPENAI_API_KEY", &setup.api_key); }
            llm.providers.azure = Some(AzureConfig {
                resource_name: setup.azure_resource.clone(),
                endpoint_base: None, // set manually for AIServices/cognitiveservices resources
                deployment: setup.azure_deployment.clone(),
                api_version: setup.azure_api_version.clone(),
                api_key_env: Some("AZURE_OPENAI_API_KEY".to_string()),
            });
        }
        _ => {
            if provider.default_env.is_empty() {
                // Ollama: no key, but may have base_url.
                if let Some(base) = provider.base_url {
                    let cfg = ProviderConfig {
                        api_key_env: None,
                        base_url: Some(base.to_string()),
                        default_model: None,
                    };
                    if provider.id == "ollama" {
                        llm.providers.ollama = Some(cfg);
                    }
                }
                return;
            }
            // Standard API-key providers.
            let env_var = provider.default_env;
            // SAFETY: single-threaded at this point in setup.
            unsafe { std::env::set_var(env_var, &setup.api_key); }
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
    }
}

// ── Model selection helper ────────────────────────────────────────

fn select_model_from_list(
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
async fn validate_key_http(url: &str, provider_id: &str, key: &str) -> anyhow::Result<()> {
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
