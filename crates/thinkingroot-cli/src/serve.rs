use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;

use tokio::sync::RwLock;

use thinkingroot_core::WorkspaceRegistry;
use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::rest::{AppState, build_router_opts};

/// Launch the interactive knowledge graph explorer in the browser.
pub async fn run_graph(port: u16, path: std::path::PathBuf) -> anyhow::Result<()> {
    let abs_path = std::fs::canonicalize(&path)
        .with_context(|| format!("path not found: {}", path.display()))?;

    let data_dir = abs_path.join(".thinkingroot");
    if !data_dir.exists() {
        anyhow::bail!(
            "No ThinkingRoot data found at {}. Run `root compile {}` first.",
            data_dir.display(),
            abs_path.display()
        );
    }

    let url = format!("http://127.0.0.1:{}/graph", port);

    println!();
    println!(
        "  {} Knowledge Graph",
        console::style("ThinkingRoot").green().bold()
    );
    println!("  {}", console::style(&url).cyan().underlined());
    println!();
    println!("  Press Ctrl+C to stop.");
    println!();

    // Open browser (best-effort, don't fail if it doesn't work)
    let _ = open_browser(&url);

    run_serve(
        port,
        "127.0.0.1".into(),
        None,
        vec![path],
        None,   // name
        false,
        false,
        false, // enable MCP
    )
    .await
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(url).spawn()?;
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("cmd")
        .args(["/C", "start", url])
        .spawn()?;
    Ok(())
}

/// Launch the ThinkingRoot server (REST API + MCP).
#[allow(clippy::too_many_arguments)]
pub async fn run_serve(
    port: u16,
    host: String,
    api_key: Option<String>,
    paths: Vec<PathBuf>,
    name: Option<String>,
    mcp_stdio: bool,
    no_rest: bool,
    no_mcp: bool,
) -> anyhow::Result<()> {
    if no_rest && no_mcp {
        anyhow::bail!("--no-rest and --no-mcp cannot be used together: nothing to serve");
    }

    // Resolve workspace paths: explicit --path > --name > registry
    let resolved_paths: Vec<(String, PathBuf, u16)> = if !paths.is_empty() {
        paths.iter().map(|p| {
            let abs = std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.clone());
            let ws_name = abs
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "default".to_string());
            (ws_name, abs, port)
        }).collect()
    } else {
        let registry = WorkspaceRegistry::load()?;
        let workspaces = if let Some(ref ws_name) = name {
            let entry = registry.workspaces.iter()
                .find(|w| w.name == *ws_name)
                .ok_or_else(|| anyhow::anyhow!(
                    "workspace \"{}\" not found. Run `root workspace list` to see registered workspaces.",
                    ws_name
                ))?;
            vec![(entry.name.clone(), entry.path.clone(), entry.port)]
        } else {
            registry.workspaces.iter()
                .map(|w| (w.name.clone(), w.path.clone(), w.port))
                .collect()
        };

        if workspaces.is_empty() {
            anyhow::bail!(
                "No workspaces registered. Run `root setup` or `root workspace add <path>`."
            );
        }
        workspaces
    };

    let mut engine = QueryEngine::new();
    for (ws_name, abs_path, _ws_port) in &resolved_paths {
        engine.mount(ws_name.clone(), abs_path.clone()).await?;
        tracing::info!("mounted workspace '{}' from {}", ws_name, abs_path.display());
    }

    if mcp_stdio {
        eprintln!(
            "ThinkingRoot MCP stdio server v{}",
            env!("CARGO_PKG_VERSION")
        );
        let workspaces = engine.list_workspaces().await?;
        for ws in &workspaces {
            eprintln!(
                "  Workspace: {} ({} entities, {} claims)",
                ws.name, ws.entity_count, ws.claim_count
            );
        }
        let default_ws = resolved_paths.first().map(|(ws_name, _, _)| ws_name.clone());
        let engine = Arc::new(RwLock::new(engine));
        thinkingroot_serve::mcp::stdio::run(engine, default_ws).await;
        return Ok(());
    }

    // Print banner.
    let auth_status = if api_key.is_some() {
        "API key required"
    } else {
        "open (no auth)"
    };

    println!();
    println!("  ThinkingRoot v{}", env!("CARGO_PKG_VERSION"));
    if !no_rest {
        println!("  REST API:  http://{}:{}/api/v1/", host, port);
    }
    if !no_mcp {
        println!("  MCP SSE:   http://{}:{}/mcp/sse", host, port);
    }
    for (ws_name, _path, _ws_port) in &resolved_paths {
        println!(
            "  Workspace: {} → http://{}:{}/api/v1/ws/{}/",
            ws_name, host, port, ws_name
        );
    }
    println!("  Auth:      {}", auth_status);
    println!();

    // Build and start server.
    let state = AppState::new(engine, api_key);

    let router = build_router_opts(state, !no_rest, !no_mcp);
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

/// Generate and install an OS-native service file so `root serve` starts on login.
pub fn install_service() -> anyhow::Result<()> {
    let binary = std::env::current_exe()
        .context("cannot resolve current executable path")?
        .display()
        .to_string();

    let log_path = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?
        .join("thinkingroot")
        .join("serve.log");

    #[cfg(target_os = "macos")]
    {
        let agents_dir = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home dir"))?
            .join("Library")
            .join("LaunchAgents");
        std::fs::create_dir_all(&agents_dir)?;
        let plist_path = agents_dir.join("dev.thinkingroot.plist");

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>             <string>dev.thinkingroot</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>         <true/>
    <key>KeepAlive</key>         <true/>
    <key>StandardOutPath</key>   <string>{log}</string>
    <key>StandardErrorPath</key> <string>{log}</string>
</dict>
</plist>"#,
            binary = binary,
            log = log_path.display()
        );

        std::fs::write(&plist_path, plist)?;
        println!();
        println!("  {} {}", console::style("✓ Service file:").green().bold(), plist_path.display());
        println!();
        println!("  To start now:");
        println!("    launchctl load {}", plist_path.display());
        println!("    launchctl start dev.thinkingroot");
        println!();
        println!("  ThinkingRoot will start automatically on login.");
        println!("  Logs: {}", log_path.display());
    }

    #[cfg(target_os = "linux")]
    {
        let systemd_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve config dir"))?
            .join("systemd")
            .join("user");
        std::fs::create_dir_all(&systemd_dir)?;
        let service_path = systemd_dir.join("thinkingroot.service");

        let unit = format!(
            "[Unit]\nDescription=ThinkingRoot Knowledge Server\nAfter=network.target\n\n\
             [Service]\nExecStart={binary} serve\nRestart=on-failure\n\
             StandardOutput=append:{log}\nStandardError=append:{log}\n\n\
             [Install]\nWantedBy=default.target\n",
            binary = binary,
            log = log_path.display()
        );

        std::fs::write(&service_path, unit)?;
        println!();
        println!("  {} {}", console::style("✓ Service file:").green().bold(), service_path.display());
        println!();
        println!("  To enable:");
        println!("    systemctl --user daemon-reload");
        println!("    systemctl --user enable thinkingroot");
        println!("    systemctl --user start thinkingroot");
        println!();
        println!("  Logs: {}", log_path.display());
    }

    #[cfg(target_os = "windows")]
    {
        let ps_path = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home dir"))?
            .join("thinkingroot-service.ps1");

        let script = format!(
            "# ThinkingRoot Windows Service — run as Administrator\r\n\
             sc.exe create \"ThinkingRoot\" binPath= \"{binary} serve\" start= auto\r\n\
             sc.exe start \"ThinkingRoot\"\r\n",
            binary = binary
        );

        std::fs::write(&ps_path, script)?;
        println!();
        println!("  {} {}", console::style("✓ Script:").green().bold(), ps_path.display());
        println!();
        println!("  Run as Administrator:");
        println!("    powershell -ExecutionPolicy Bypass -File {}", ps_path.display());
    }

    Ok(())
}
