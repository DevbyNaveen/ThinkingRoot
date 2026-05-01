use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use console::style;
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use thinkingroot_core::config::{
    AzureConfig, BedrockConfig, LlmConfig, ProviderConfig, ProvidersConfig,
};
use thinkingroot_core::global_config::{Credentials, GlobalConfig, ServeConfig};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};

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
];

// ── Provider setup result (carries all collected data) ───────────

struct ProviderSetup {
    api_key: String,
    model: String,
    // Used by providers with no fixed base_url (e.g. "custom")
    base_url: Option<String>,
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
    let config_path =
        GlobalConfig::path().ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;

    println!(
        "\n  {}",
        style("[1/5] Global config location").cyan().bold()
    );
    println!(
        "  Settings will be saved to: {}",
        style(config_path.display()).white()
    );
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
    if detected.is_empty() {
        println!("  No supported AI tools detected.");
        println!("  You can run {} later.", style("root connect").cyan());
    } else {
        println!("  Found {} tool(s):", detected.len());
        for tool in &detected {
            println!(
                "    {} {}",
                style("·").dim(),
                style(tool.name).white().bold()
            );
        }
    }
    println!();

    let connect = if !detected.is_empty() {
        Confirm::with_theme(&theme)
            .with_prompt(format!(
                "Connect all {} tool(s) to ThinkingRoot now?",
                detected.len()
            ))
            .default(true)
            .interact()?
    } else {
        false
    };

    // ── Step 5: Compile ───────────────────────────────────────────
    println!(
        "\n  {}",
        style("[5/5] Compile knowledge base").cyan().bold()
    );

    let compile_choices = &["Yes, compile now", "Skip — I'll run `root compile` later"];
    let compile_now = Select::with_theme(&theme)
        .with_prompt("Compile now?")
        .items(compile_choices)
        .default(0)
        .interact()?
        == 0;

    // ── Apply all changes ─────────────────────────────────────────
    println!();
    global.save()?;
    registry.save()?;

    // Connect tools — show per-tool feedback in real time
    if connect {
        println!("  Connecting AI tools...");
        for tool in &detected {
            match crate::mcp_config::write_tool_config(tool, ws_port, false) {
                Ok(result) => println!(
                    "    {} {}  {}",
                    style("✓").green().bold(),
                    style(tool.name).white().bold(),
                    style(result.path.display().to_string()).dim()
                ),
                Err(e) => println!(
                    "    {} {}  {}",
                    style("✗").red().bold(),
                    style(tool.name).white().bold(),
                    style(format!("failed: {e}")).red()
                ),
            }
        }
        println!();
    }

    // Compile — reuse the same world-class progress display used by `root compile`.
    if compile_now {
        println!("  Compiling {}...\n", abs_ws_path.display());
        match crate::progress::run_compile_progress(
            &abs_ws_path,
            None,
            /* no_rooting= */ false,
        )
        .await
        {
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
        )
        .dim()
    );
    println!("  Workspace       {}/.thinkingroot/", abs_ws_path.display());

    if connect && !detected.is_empty() {
        println!();
        println!("  Connected tools (stdio — no server needed):");
        for tool in &detected {
            println!(
                "    {} {}  {}",
                style("✓").green(),
                style(tool.name).white().bold(),
                style(tool.config_path.display().to_string()).dim()
            );
        }
    }

    println!();
    println!("  Next steps:");
    if !connect || detected.is_empty() {
        println!(
            "    {}  wire AI tools (Claude, Cursor, VS Code…)",
            style("root connect").cyan()
        );
    }
    println!(
        "    {}  start the knowledge server (REST + MCP HTTP)",
        style("root serve").cyan()
    );
    println!(
        "    {}  add more folders",
        style("root workspace add <path>").cyan()
    );
    println!(
        "    {}  answer a question from your knowledge base",
        style("root ask \"…\"").cyan()
    );

    // Show credentials file location so users know where the key is stored.
    if !provider.default_env.is_empty() && provider.id != "bedrock" && provider.id != "ollama" {
        if let Some(creds_path) = GlobalConfig::credentials_path() {
            println!("  Credentials      {}", style(creds_path.display()).dim());
        }
        println!(
            "  {}  (mode 0600 — readable only by you)",
            style("Key saved to credentials.toml").green()
        );
        println!(
            "  You can also set {} in your shell profile to override.",
            style(format!("export {}=\"...\"", provider.default_env)).cyan()
        );
    }

    println!();

    Ok(())
}

// ── Update flow (idempotent re-run) ──────────────────────────────

async fn run_setup_update(theme: &ColorfulTheme) -> anyhow::Result<()> {
    let global = GlobalConfig::load()?;
    let registry = WorkspaceRegistry::load()?;

    println!(
        "\n  {} ThinkingRoot is already configured.\n",
        style("✓").green().bold()
    );
    println!(
        "  Provider:   {} / {}",
        global.llm.default_provider, global.llm.extraction_model
    );
    println!("  Workspaces: {} total\n", registry.workspaces.len());

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
            if !provider.default_env.is_empty()
                && provider.id != "bedrock"
                && provider.id != "ollama"
            {
                println!(
                    "  {} Key saved to credentials.toml (mode 0600).",
                    style("✓").green()
                );
                println!(
                    "  You can also set {} in your shell profile to override.",
                    style(format!("export {}=\"...\"", provider.default_env)).cyan()
                );
            }
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
            if let Some(p) = GlobalConfig::path()
                && p.exists()
            {
                std::fs::remove_file(&p)?;
            }
            if let Some(p) = WorkspaceRegistry::path()
                && p.exists()
            {
                std::fs::remove_file(&p)?;
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
        "bedrock" => configure_bedrock(theme).await,
        "azure" => configure_azure(theme).await,
        _ => configure_generic(theme, provider).await,
    }
}

// ── Bedrock configure flow ────────────────────────────────────────

async fn configure_bedrock(theme: &ColorfulTheme) -> anyhow::Result<ProviderSetup> {
    println!();
    println!("  {}", style("AWS Bedrock").bold());
    println!("  Runs inference inside your AWS account — no data leaves AWS.\n");

    // 1. Check for existing credentials
    if !bedrock_credentials_found() {
        println!(
            "  {} AWS credentials not found.",
            style("!").yellow().bold()
        );
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
    println!(
        "  {} Use cross-region inference IDs (e.g. {}) for ~4x higher quota.",
        style("Tip:").dim(),
        style("us.anthropic.claude-haiku-4-5-20251001-v1:0").cyan()
    );
    let region: String = Input::with_theme(theme)
        .with_prompt("AWS region")
        .default("us-east-1".to_string())
        .interact_text()?;

    // 3. Model — Bedrock has no public model-list API; user enters the ID directly.
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
    let model: String = Input::with_theme(theme)
        .with_prompt("Model ID")
        .interact_text()?;

    Ok(ProviderSetup {
        api_key: String::new(),
        model,
        base_url: None,
        azure_resource: None,
        azure_deployment: None,
        azure_api_version: None,
        bedrock_region: Some(region),
    })
}

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
        base_url: None,
        azure_resource: Some(resource),
        azure_deployment: Some(deployment),
        azure_api_version: Some(api_version),
        bedrock_region: None,
    })
}

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

// ── Generic (OpenAI-compatible) configure flow ────────────────────

async fn configure_generic(
    theme: &ColorfulTheme,
    provider: &ProviderDef,
) -> anyhow::Result<ProviderSetup> {
    let api_key = if !provider.default_env.is_empty() {
        let key: String = Password::with_theme(theme)
            .with_prompt(format!(
                "{} API key",
                provider
                    .label
                    .split_whitespace()
                    .next()
                    .unwrap_or(provider.id)
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

    // Custom provider has no default base_url — must be collected from the user.
    let base_url = if provider.id == "custom" {
        println!();
        println!("  Enter the base URL of your OpenAI-compatible endpoint.");
        println!(
            "  {}",
            style("Example: https://my-api.example.com/v1").dim()
        );
        let url: String = dialoguer::Input::with_theme(theme)
            .with_prompt("Base URL")
            .interact_text()?;
        Some(url)
    } else {
        None
    };

    // Fetch live model list — show a spinner, fall back silently to catalogue on any error
    let pb = {
        let pb = indicatif::ProgressBar::new_spinner();
        pb.set_style(
            indicatif::ProgressStyle::default_spinner()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        pb.set_message("Fetching available models...");
        pb.enable_steady_tick(Duration::from_millis(80));
        pb
    };
    let live_models = fetch_provider_models(provider, &api_key).await;
    pb.finish_and_clear();

    let effective: Vec<&str> = live_models
        .as_deref()
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let model = select_model_from_list(theme, &effective)?;

    Ok(ProviderSetup {
        api_key,
        model,
        base_url,
        azure_resource: None,
        azure_deployment: None,
        azure_api_version: None,
        bedrock_region: None,
    })
}

// ── Config writer ─────────────────────────────────────────────────

/// Populate `llm` provider slots from the collected setup data AND save the API
/// key to `credentials.toml` (mode 0600) so every subsequent command works
/// in a fresh shell without requiring the user to export env vars.
///
/// Key resolution order at runtime (see `resolve_key` in thinkingroot-extract):
///   1. Live env var (highest — allows CI/CD override)
///   2. Value in credentials.toml (set here)
///   3. Hard error → `root setup`
fn set_provider_config(llm: &mut LlmConfig, provider: &ProviderDef, setup: &ProviderSetup) {
    match provider.id {
        "bedrock" => {
            // Bedrock uses AWS SDK credential chain — no API key to store.
            llm.providers.bedrock = Some(BedrockConfig {
                region: setup.bedrock_region.clone(),
                profile: None,
            });
        }
        "azure" => {
            // Inject into env for this process session.
            unsafe {
                std::env::set_var("AZURE_OPENAI_API_KEY", &setup.api_key);
            }
            persist_credential("AZURE_OPENAI_API_KEY", &setup.api_key);
            llm.providers.azure = Some(AzureConfig {
                resource_name: setup.azure_resource.clone(),
                endpoint_base: None,
                deployment: setup.azure_deployment.clone(),
                api_version: setup.azure_api_version.clone(),
                api_key_env: Some("AZURE_OPENAI_API_KEY".to_string()),
                api_key: None, // never stored in config.toml — lives in credentials.toml
            });
        }
        _ => {
            if provider.default_env.is_empty() {
                // Ollama / LiteLLM (no API key required).
                let resolved_base_url = setup
                    .base_url
                    .clone()
                    .or_else(|| provider.base_url.map(str::to_string));
                let cfg = ProviderConfig {
                    api_key_env: None,
                    api_key: None,
                    base_url: resolved_base_url,
                    default_model: None,
                };
                match provider.id {
                    "ollama" => llm.providers.ollama = Some(cfg),
                    "litellm" => llm.providers.litellm = Some(cfg),
                    _ => {}
                }
                return;
            }
            // Standard API-key providers.
            let env_var = provider.default_env;
            // Inject into env for this process session.
            unsafe {
                std::env::set_var(env_var, &setup.api_key);
            }
            // Persist to credentials.toml so future invocations work without
            // requiring `export KEY=...` in the shell profile.
            persist_credential(env_var, &setup.api_key);
            let resolved_base_url = setup
                .base_url
                .clone()
                .or_else(|| provider.base_url.map(str::to_string));
            let cfg = ProviderConfig {
                api_key_env: Some(env_var.to_string()),
                api_key: None, // never stored in config.toml — lives in credentials.toml
                base_url: resolved_base_url,
                default_model: None,
            };
            match provider.id {
                "openrouter" => llm.providers.openrouter = Some(cfg),
                "openai" => llm.providers.openai = Some(cfg),
                "anthropic" => llm.providers.anthropic = Some(cfg),
                "groq" => llm.providers.groq = Some(cfg),
                "together" => llm.providers.together = Some(cfg),
                "deepseek" => llm.providers.deepseek = Some(cfg),
                "perplexity" => llm.providers.perplexity = Some(cfg),
                "litellm" => llm.providers.litellm = Some(cfg),
                "custom" => llm.providers.custom = Some(cfg),
                _ => {}
            }
        }
    }
}

/// Write a single credential to `~/.config/thinkingroot/credentials.toml`.
/// Silently skips empty values (e.g. Ollama / Bedrock).
/// Errors are printed but not fatal — the user can always set an env var as fallback.
pub(crate) fn persist_credential(env_var: &str, value: &str) {
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

fn print_banner() {
    println!();
    println!("  {}", style("ThinkingRoot").green().bold());
    println!("  {}", style("First-time setup").dim());
    println!();
}
