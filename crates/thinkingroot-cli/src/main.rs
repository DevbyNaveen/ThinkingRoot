use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use console::style;
use tracing_subscriber::EnvFilter;

mod branch_cmd;
mod mcp_config;
mod pipeline;
mod progress;
mod provider_cmd;
mod serve;
mod setup;
mod update_cmd;
mod watch;
mod workspace;

#[derive(Parser)]
#[command(
    name = "root",
    about = "ThinkingRoot — The open-source knowledge compiler for AI agents",
    version,
    long_about = "ThinkingRoot compiles your docs, code, chats, and tickets into verified, linked knowledge that agents read in 2K tokens instead of 50K."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to compile (shorthand for `root compile <path>`)
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Compile a directory through the full knowledge pipeline
    Compile {
        /// Path to the directory to compile
        path: PathBuf,
        /// Compile into a specific branch instead of main
        #[arg(long)]
        branch: Option<String>,
    },
    /// Show the knowledge health score
    Health {
        /// Path to the compiled knowledge base
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Initialize a new ThinkingRoot workspace
    Init {
        /// Path to initialize
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Query the compiled knowledge base
    Query {
        /// The query string (e.g., "what systems depend on PostgreSQL?")
        query: String,
        /// Path to the compiled knowledge base
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Number of results to show
        #[arg(short = 'n', long, default_value = "10")]
        top_k: usize,
    },
    /// Open the interactive knowledge graph in your browser
    Graph {
        /// Path to the compiled knowledge base
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Port to bind
        #[arg(long, default_value = "3001")]
        port: u16,
    },
    /// Start the REST API and MCP server
    Serve {
        /// Port to bind [default: 3000]
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
        /// Serve a specific branch instead of main
        #[arg(long)]
        branch: Option<String>,
    },
    /// First-time guided setup wizard
    Setup,
    /// Manage registered workspaces
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
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
    /// Watch for changes and recompile incrementally
    Watch {
        /// Path to the directory to watch
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Create or manage knowledge branches
    Branch {
        /// Branch name to create
        name: Option<String>,
        /// List all active branches
        #[arg(long)]
        list: bool,
        /// Delete (abandon) a branch — keeps data directory
        #[arg(long)]
        delete: Option<String>,
        /// Hard-delete a branch and remove its data directory
        #[arg(long)]
        purge: Option<String>,
        /// Remove all abandoned branch data directories (garbage collect)
        #[arg(long)]
        gc: bool,
        /// Optional description for the new branch
        #[arg(long)]
        description: Option<String>,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Set the active branch (update HEAD)
    Checkout {
        /// Branch name to check out
        name: String,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Show semantic diff between a branch and main (Knowledge PR)
    Diff {
        /// Branch name to diff against main
        branch: String,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Merge a branch into main (runs health CI gate)
    Merge {
        /// Branch name to merge
        branch: String,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Skip health CI gate
        #[arg(long)]
        force: bool,
        /// Apply claim deletions from branch to main
        #[arg(long)]
        propagate_deletions: bool,
        /// Restore main to its state before this branch was merged
        #[arg(long)]
        rollback: bool,
        /// Manually resolve a contradiction (format: <index>=keep-main|keep-branch).
        /// Index refers to the numbered list shown by `root diff`. Repeatable.
        #[arg(long = "resolve", value_name = "N=RESOLUTION")]
        resolutions: Vec<String>,
    },
    /// Show current branch and workspace status
    Status {
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Create an immutable named snapshot of the current branch
    Snapshot {
        /// Snapshot name
        name: String,
        /// Path to workspace root
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Manage and switch LLM providers
    Provider {
        #[command(subcommand)]
        action: Option<ProviderAction>,
    },
    /// Update root to the latest version
    Update,
}

#[derive(Subcommand)]
enum ProviderAction {
    /// List all available providers and show which is active (default)
    List {
        /// Workspace path to check for local overrides
        #[arg(short, long, default_value = ".", value_name = "PATH")]
        path: PathBuf,
    },
    /// Show active provider, model, and credential status
    Status {
        /// Workspace path to check for local overrides
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Switch to a different provider
    Use {
        /// Provider name: openrouter, openai, azure, anthropic, bedrock, ollama,
        /// groq, together, deepseek, perplexity, litellm, custom
        name: String,
        /// Model ID (e.g. gpt-4o-mini). Prompted if not given.
        #[arg(long)]
        model: Option<String>,
        /// API key value. Skips interactive prompt; sets the provider's env var
        /// for this session. Ignored for bedrock and ollama.
        #[arg(long, value_name = "KEY")]
        key: Option<String>,
        /// Base URL override for self-hosted or custom endpoints.
        /// Required for the 'custom' provider.
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        /// Write to .thinkingroot/config.toml instead of the global config.
        /// Overrides provider for this workspace only.
        #[arg(long)]
        local: bool,
        /// Workspace path (used with --local)
        #[arg(short, long, default_value = ".", value_name = "PATH")]
        path: PathBuf,
        /// Skip API key validation (useful in CI or offline environments)
        #[arg(long)]
        no_validate: bool,
        /// Azure resource name — skips the interactive prompt (azure only)
        #[arg(long, value_name = "NAME")]
        azure_resource: Option<String>,
        /// Azure deployment name — skips the interactive prompt (azure only)
        #[arg(long, value_name = "DEPLOYMENT")]
        azure_deployment: Option<String>,
        /// Azure API version — skips the interactive prompt (azure only)
        #[arg(long, value_name = "VERSION")]
        azure_api_version: Option<String>,
    },
    /// Change the extraction model without changing the provider
    #[command(name = "set-model")]
    SetModel {
        /// Model ID (e.g. gpt-4o, llama3, claude-3-haiku-20240307)
        model: String,
        /// Write to workspace config instead of global config
        #[arg(long)]
        local: bool,
        /// Workspace path (used with --local)
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
}

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Detect TTY *before* initialising the subscriber — the filter depends on it.
    // Progress bars and tracing INFO both write to stderr; in TTY mode we suppress
    // INFO to avoid garbling the bars (same approach as `cargo build`).
    use std::io::IsTerminal as _;
    let use_progress = !cli.verbose && std::io::stderr().is_terminal();

    let filter = if cli.verbose {
        EnvFilter::new("thinkingroot=debug,root=debug")
    } else if use_progress {
        // TTY + no --verbose: suppress everything below ERROR so progress bars
        // own stderr cleanly. WARN/INFO mixed with indicatif garbles the display.
        EnvFilter::new("thinkingroot=error,root=error")
    } else {
        // Pipe / CI: full INFO for clean log output.
        EnvFilter::new("thinkingroot=info,root=info")
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();

    match cli.command {
        Some(Commands::Compile { path, branch }) => {
            run_compile(&path, branch.as_deref(), use_progress).await?;
        }
        Some(Commands::Health { path }) => {
            run_health(&path).await?;
        }
        Some(Commands::Init { path }) => {
            run_init(&path)?;
        }
        Some(Commands::Query { query, path, top_k }) => {
            run_query(&path, &query, top_k).await?;
        }
        Some(Commands::Graph { path, port }) => {
            serve::run_graph(port, path).await?;
        }
        Some(Commands::Serve {
            port,
            host,
            api_key,
            paths,
            name,
            mcp_stdio,
            no_rest,
            no_mcp,
            install_service,
            branch,
        }) => {
            if install_service {
                serve::install_service()?;
                return Ok(());
            }
            serve::run_serve(
                port, host, api_key, paths, name, mcp_stdio, no_rest, no_mcp, branch,
            )
            .await?;
        }
        Some(Commands::Setup) => {
            setup::run_setup().await?;
        }
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
        Some(Commands::Connect {
            tool,
            port,
            dry_run,
            remove,
        }) => {
            mcp_config::run_connect(tool.as_deref(), port, dry_run, remove)?;
        }
        Some(Commands::Watch { path }) => {
            let path = std::fs::canonicalize(&path)
                .with_context(|| format!("path not found: {}", path.display()))?;
            watch::run_watch(&path).await?;
        }
        Some(Commands::Branch {
            name,
            list,
            delete,
            purge,
            gc,
            description,
            path,
        }) => {
            branch_cmd::handle_branch(
                &path,
                name.as_deref(),
                list,
                delete.as_deref(),
                purge.as_deref(),
                gc,
                description,
            )
            .await?;
        }
        Some(Commands::Checkout { name, path }) => {
            branch_cmd::handle_checkout(&path, &name).await?;
        }
        Some(Commands::Diff { branch, path }) => {
            branch_cmd::handle_diff(&path, &branch).await?;
        }
        Some(Commands::Merge {
            branch,
            path,
            force,
            propagate_deletions,
            rollback,
            resolutions,
        }) => {
            if rollback {
                branch_cmd::handle_rollback(&path, &branch)?;
            } else {
                branch_cmd::handle_merge(&path, &branch, force, propagate_deletions, &resolutions)
                    .await?;
            }
        }
        Some(Commands::Status { path }) => {
            branch_cmd::handle_status(&path).await?;
        }
        Some(Commands::Snapshot { name, path }) => {
            branch_cmd::handle_snapshot(&path, &name).await?;
        }
        Some(Commands::Provider { action }) => match action {
            None => {
                provider_cmd::run_provider_list(Path::new(".")).await?;
            }
            Some(ProviderAction::List { path }) => {
                provider_cmd::run_provider_list(&path).await?;
            }
            Some(ProviderAction::Status { path }) => {
                provider_cmd::run_provider_status(&path).await?;
            }
            Some(ProviderAction::Use {
                name,
                model,
                key,
                base_url,
                local,
                path,
                no_validate,
                azure_resource,
                azure_deployment,
                azure_api_version,
            }) => {
                provider_cmd::run_provider_use(
                    &name,
                    model.as_deref(),
                    key.as_deref(),
                    base_url.as_deref(),
                    local,
                    &path,
                    no_validate,
                    azure_resource.as_deref(),
                    azure_deployment.as_deref(),
                    azure_api_version.as_deref(),
                )
                .await?;
            }
            Some(ProviderAction::SetModel { model, local, path }) => {
                provider_cmd::run_provider_set_model(&model, local, &path)?;
            }
        },
        Some(Commands::Update) => {
            update_cmd::run_update().await?;
        }
        None => {
            // `root ./path` shorthand — same as `root compile ./path`.
            if let Some(path) = cli.path {
                run_compile(&path, None, use_progress).await?;
            } else {
                // No args: compile current directory.
                run_compile(&PathBuf::from("."), None, use_progress).await?;
            }
        }
    }

    Ok(())
}

async fn run_compile(
    path: &PathBuf,
    branch: Option<&str>,
    use_progress: bool,
) -> anyhow::Result<()> {
    if !path.exists() {
        let name = path.display().to_string();
        anyhow::bail!(
            "Unknown command or path not found: '{}'\n\nRun 'root --help' to see available commands.",
            style(name).yellow().bold()
        );
    }
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize path: {}", path.display()))?;

    print_banner();
    println!(
        "  {} {}\n",
        style("Compiling").cyan().bold(),
        style(path.display()).white()
    );

    let start = Instant::now();

    let result = if use_progress {
        progress::run_compile_progress(&path, branch).await?
    } else {
        pipeline::run_pipeline(&path, branch, None).await?
    };

    let elapsed = start.elapsed();
    println!();
    println!(
        "  {} compiled {} files in {:.1}s",
        style("ThinkingRoot").green().bold(),
        style(result.files_parsed).white().bold(),
        elapsed.as_secs_f64()
    );
    println!(
        "  {} {}%",
        style("Knowledge Health:").white().bold(),
        style(result.health_score).green().bold()
    );
    println!(
        "  {} {} claims extracted",
        style("  ├──").dim(),
        style(result.claims_count).cyan()
    );
    println!(
        "  {} {} entities identified",
        style("  ├──").dim(),
        style(result.entities_count).cyan()
    );
    println!(
        "  {} {} relations mapped",
        style("  ├──").dim(),
        style(result.relations_count).cyan()
    );
    println!(
        "  {} {} contradictions found",
        style("  ├──").dim(),
        style(result.contradictions_count).yellow()
    );
    println!(
        "  {} {} artifacts generated",
        style("  └──").dim(),
        style(result.artifacts_count).cyan()
    );
    if result.cache_hits > 0 {
        println!(
            "  {} {} extraction cache hits",
            style("  ├──").dim(),
            style(result.cache_hits).green()
        );
    }
    if result.early_cutoffs > 0 {
        println!(
            "  {} {} sources unchanged (early cutoff)",
            style("  └──").dim(),
            style(result.early_cutoffs).green()
        );
    }
    println!();

    Ok(())
}

async fn run_health(path: &PathBuf) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("path not found: {}", path.display()))?;
    let data_dir = path.join(".thinkingroot");

    if !data_dir.exists() {
        anyhow::bail!(
            "No ThinkingRoot data found at {}. Run `root compile {}` first.",
            data_dir.display(),
            path.display()
        );
    }

    let config = thinkingroot_core::Config::load_merged(&path)?;
    let storage = thinkingroot_graph::StorageEngine::init(&data_dir)
        .await
        .context("failed to open storage")?;
    let verifier = thinkingroot_verify::Verifier::new(&config);
    let result = verifier.verify(&storage.graph)?;

    print_banner();
    println!(
        "  {} {}%\n",
        style("Knowledge Health:").white().bold(),
        style(result.health_score.as_percentage()).green().bold()
    );

    if !result.warnings.is_empty() {
        println!("  {}", style("Warnings:").yellow().bold());
        for w in &result.warnings {
            println!("    {} {}", style("!").yellow(), w);
        }
    }

    Ok(())
}

async fn run_query(path: &PathBuf, query: &str, top_k: usize) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("path not found: {}", path.display()))?;
    let data_dir = path.join(".thinkingroot");

    if !data_dir.exists() {
        anyhow::bail!(
            "No ThinkingRoot data found. Run `root compile {}` first.",
            path.display()
        );
    }

    let mut storage = thinkingroot_graph::StorageEngine::init(&data_dir)
        .await
        .context("failed to open storage")?;

    if storage.vector.is_empty() {
        anyhow::bail!("No embeddings found. Run `root compile` first to build the search index.");
    }

    println!();
    println!(
        "  {} \"{}\"",
        style("Searching:").cyan().bold(),
        style(query).white()
    );
    println!();

    let results = storage.vector.search(query, top_k)?;

    if results.is_empty() {
        println!("  {} No results found.", style("!").yellow());
        return Ok(());
    }

    for (i, (_id, metadata, score)) in results.iter().enumerate() {
        if *score < 0.1 {
            break; // Skip very low relevance results.
        }

        let parts: Vec<&str> = metadata.splitn(5, '|').collect();
        match parts.first() {
            Some(&"entity") if parts.len() >= 4 => {
                let name = parts[2];
                let etype = parts[3];
                println!(
                    "  {} {} {} ({})",
                    style(format!("{}.", i + 1)).dim(),
                    style("Entity:").green().bold(),
                    style(name).white().bold(),
                    style(etype).dim()
                );
                // Show claims for this entity.
                let entity_id = parts[1];
                if let Ok(claims) = storage.graph.get_claims_with_sources_for_entity(entity_id) {
                    for (_, stmt, _, uri, conf) in claims.iter().take(3) {
                        println!(
                            "      {} {} {} [{}]",
                            style("·").dim(),
                            stmt,
                            style(format!("({:.0}%)", conf * 100.0)).dim(),
                            style(uri).dim()
                        );
                    }
                }
                println!("      {} {:.0}%", style("relevance:").dim(), score * 100.0);
                println!();
            }
            Some(&"claim") if parts.len() >= 5 => {
                let ctype = parts[2];
                let uri = parts[4];
                // The statement isn't in metadata — use the ID to look it up or
                // show what we have.
                println!(
                    "  {} {} [{}] [{}]",
                    style(format!("{}.", i + 1)).dim(),
                    style(format!("Claim ({ctype}):")).blue().bold(),
                    style(uri).dim(),
                    style(format!("{:.0}% relevance", score * 100.0)).dim(),
                );
                // Get the actual claim statement from graph.
                let claim_id = parts[1];
                if let Ok(claims) = storage.graph.get_claims_for_entity(claim_id) {
                    for (_, stmt, _) in claims.iter().take(1) {
                        println!("      {} {}", style("·").dim(), stmt);
                    }
                }
                println!();
            }
            _ => {
                println!(
                    "  {} {} (relevance: {:.0}%)",
                    style(format!("{}.", i + 1)).dim(),
                    metadata,
                    score * 100.0
                );
                println!();
            }
        }
    }

    Ok(())
}

fn run_init(path: &Path) -> anyhow::Result<()> {
    let data_dir = path.join(".thinkingroot");

    if data_dir.exists() {
        println!(
            "  {} already initialized at {}",
            style("ThinkingRoot").green().bold(),
            data_dir.display()
        );
        return Ok(());
    }

    let config = thinkingroot_core::Config::default();
    config.save(path)?;

    println!(
        "  {} initialized at {}",
        style("ThinkingRoot").green().bold(),
        data_dir.display()
    );

    let global_exists = thinkingroot_core::GlobalConfig::path()
        .map(|p| p.exists())
        .unwrap_or(false);
    if !global_exists {
        println!(
            "  {} No global config found — run {} first to configure your LLM provider.",
            style("Note:").yellow().bold(),
            style("root setup").cyan()
        );
    }

    println!(
        "  Run `root compile {}` to compile your knowledge.",
        path.display()
    );

    Ok(())
}

fn print_banner() {
    println!();
    println!("  {}", style("ThinkingRoot").green().bold());
    println!(
        "  {}",
        style("The open-source knowledge compiler for AI agents").dim()
    );
    println!();
}
