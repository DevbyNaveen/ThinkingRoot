use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::rest::{self, AppState};

/// Launch the ThinkingRoot server (REST API + MCP).
pub async fn run_serve(
    port: u16,
    host: String,
    api_key: Option<String>,
    paths: Vec<PathBuf>,
    mcp_stdio: bool,
    _no_rest: bool,
    _no_mcp: bool,
) -> anyhow::Result<()> {
    // Build query engine and mount workspaces.
    let mut engine = QueryEngine::new();

    for path in &paths {
        let abs_path = std::fs::canonicalize(path)?;
        let name = abs_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string());

        // Handle name collisions.
        let mut ws_name = name.clone();
        let mut counter = 2;
        while engine.list_workspaces().await?.iter().any(|w| w.name == ws_name) {
            ws_name = format!("{}-{}", name, counter);
            counter += 1;
        }

        engine.mount(ws_name.clone(), abs_path.clone()).await?;
        tracing::info!("mounted workspace '{}' from {}", ws_name, abs_path.display());
    }

    if mcp_stdio {
        // MCP stdio mode: read JSON-RPC from stdin, write to stdout.
        // Implemented in Task 5.
        tracing::info!("MCP stdio mode — reading from stdin");
        // TODO: Wire up MCP stdio transport (Task 5)
        eprintln!("MCP stdio transport not yet implemented");
        return Ok(());
    }

    // Print banner.
    let workspaces = engine.list_workspaces().await?;
    let auth_status = if api_key.is_some() { "API key required" } else { "open (no auth)" };

    println!();
    println!("  ThinkingRoot v{}", env!("CARGO_PKG_VERSION"));
    println!("  REST API:  http://{}:{}/api/v1/", host, port);
    println!("  MCP SSE:   http://{}:{}/mcp/sse", host, port);
    for ws in &workspaces {
        println!(
            "  Workspace: {} ({} entities, {} claims)",
            ws.name, ws.entity_count, ws.claim_count
        );
    }
    println!("  Auth:      {}", auth_status);
    println!();

    // Build and start server.
    let state = Arc::new(AppState {
        engine: RwLock::new(engine),
        api_key,
    });

    let router = rest::build_router(state);
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("server listening on {}", addr);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    tracing::info!("shutdown signal received, stopping server...");
}
