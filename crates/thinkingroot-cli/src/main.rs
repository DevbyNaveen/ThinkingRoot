use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use console::style;
use tracing_subscriber::EnvFilter;

mod mcp_config;
mod pipeline;
mod serve;
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
        /// Port to bind (ignored when loading from registry — each workspace has its own port)
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
    },
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

    // Initialize tracing.
    let filter = if cli.verbose {
        EnvFilter::new("thinkingroot=debug,root=debug")
    } else {
        EnvFilter::new("thinkingroot=info,root=info")
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();

    match cli.command {
        Some(Commands::Compile { path }) => {
            run_compile(&path).await?;
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
        }) => {
            if install_service {
                serve::install_service()?;
                return Ok(());
            }
            serve::run_serve(port, host, api_key, paths, name, mcp_stdio, no_rest, no_mcp).await?;
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
        Some(Commands::Connect { tool, port, dry_run, remove }) => {
            mcp_config::run_connect(tool.as_deref(), port, dry_run, remove)?;
        }
        None => {
            // `root ./path` shorthand — same as `root compile ./path`.
            if let Some(path) = cli.path {
                run_compile(&path).await?;
            } else {
                // No args: compile current directory.
                run_compile(&PathBuf::from(".")).await?;
            }
        }
    }

    Ok(())
}

async fn run_compile(path: &PathBuf) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("path not found: {}", path.display()))?;

    print_banner();
    println!(
        "  {} {}\n",
        style("Compiling").cyan().bold(),
        style(path.display()).white()
    );

    let start = Instant::now();
    let result = pipeline::run_pipeline(&path).await?;

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

    let config = thinkingroot_core::Config::load(&path)?;
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

fn run_init(path: &PathBuf) -> anyhow::Result<()> {
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
