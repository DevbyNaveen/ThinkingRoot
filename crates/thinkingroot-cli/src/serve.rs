use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;

use tokio::sync::RwLock;

use thinkingroot_branch::snapshot::resolve_data_dir;
use thinkingroot_core::WorkspaceRegistry;
use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::rest::{AppState, build_router_opts};

/// Launch the interactive knowledge graph explorer in the browser.
///
/// Tries the requested port first, then scans up to 9 consecutive ports if it
/// is already taken. The browser is opened only after the port binds
/// successfully — never before.
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

    // ── Port probe: try the requested port, then up to 9 more ──────────
    const MAX_ATTEMPTS: u16 = 10;
    let mut listener = None;
    let mut actual_port = port;

    for attempt in 0..MAX_ATTEMPTS {
        let candidate = port.saturating_add(attempt);
        let addr = format!("127.0.0.1:{}", candidate);
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => {
                actual_port = candidate;
                listener = Some(l);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if attempt == 0 {
                    eprintln!(
                        "  {} port {} in use, scanning for an available port...",
                        console::style("!").yellow().bold(),
                        candidate
                    );
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to bind port {}: {}", candidate, e));
            }
        }
    }

    let listener = listener.ok_or_else(|| {
        anyhow::anyhow!(
            "all ports {}-{} are in use. Free a port or specify one with --port",
            port,
            port.saturating_add(MAX_ATTEMPTS - 1)
        )
    })?;

    // ── Banner (shows the ACTUAL port, not the requested one) ──────────
    let url = format!("http://127.0.0.1:{}/graph", actual_port);

    println!();
    println!(
        "  {} Knowledge Graph",
        console::style("ThinkingRoot").green().bold()
    );
    println!("  {}", console::style(&url).cyan().underlined());
    if actual_port != port {
        println!(
            "  {} default port {} was busy — using {} instead",
            console::style("note:").dim(),
            port,
            actual_port
        );
    }
    println!();
    println!("  Press Ctrl+C to stop.");
    println!();

    // Browser opens AFTER successful bind — never to a dead port.
    let _ = open_browser(&url);

    run_serve_with_listener(listener, None, vec![path], None, false, false, false, None).await
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
    branch: Option<String>,
) -> anyhow::Result<()> {
    if no_rest && no_mcp {
        anyhow::bail!("--no-rest and --no-mcp cannot be used together: nothing to serve");
    }

    // Resolve workspace paths: explicit --path > --name > registry
    let resolved_paths: Vec<(String, PathBuf, u16)> = if !paths.is_empty() {
        paths
            .iter()
            .map(|p| {
                let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                let ws_name = abs
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "default".to_string());
                (ws_name, abs, port)
            })
            .collect()
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
            registry
                .workspaces
                .iter()
                .map(|w| (w.name.clone(), w.path.clone(), w.port))
                .collect()
        };

        if workspaces.is_empty() {
            // When launched as the desktop sidecar (no --path, no --name),
            // bail only when the user explicitly named a workspace that doesn't
            // exist.  For the zero-workspaces case we start with an empty
            // engine so port 31760 binds and the compile/stream endpoint
            // remains reachable (it accepts root_path in the POST body and
            // doesn't need a pre-mounted workspace).  Crashing here caused the
            // sidecar to exit before binding the port; the desktop still held
            // a SidecarHandle pointing at the dead process, so every compile
            // attempt got "connection refused".
            if name.is_some() {
                anyhow::bail!(
                    "No workspaces registered. Run `root setup` or `root workspace add <path>`."
                );
            }
            tracing::warn!(
                "workspace registry is empty — starting server with no mounted workspaces. \
                 Compile and query endpoints that require a workspace name will return errors \
                 until you run `root workspace add <path>`."
            );
        }
        workspaces
    };

    let mut engine = QueryEngine::new();
    for (ws_name, abs_path, _ws_port) in &resolved_paths {
        if let Some(ref branch_name) = branch {
            let data_dir = resolve_data_dir(abs_path, Some(branch_name));
            if !data_dir.exists() {
                anyhow::bail!(
                    "branch '{}' not found for workspace '{}' — expected data dir at {}. \
                     Run `root branch {}` first.",
                    branch_name,
                    ws_name,
                    data_dir.display(),
                    branch_name,
                );
            }
            engine
                .mount_with_data_dir(ws_name.clone(), abs_path.clone(), data_dir.clone())
                .await?;
            tracing::info!(
                "mounted workspace '{}' from branch '{}' ({})",
                ws_name,
                branch_name,
                data_dir.display()
            );
        } else {
            engine.mount(ws_name.clone(), abs_path.clone()).await?;
            tracing::info!(
                "mounted workspace '{}' from {}",
                ws_name,
                abs_path.display()
            );
        }
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
        let default_ws = resolved_paths
            .first()
            .map(|(ws_name, _, _)| ws_name.clone());
        let engine = Arc::new(RwLock::new(engine));
        let sessions = thinkingroot_serve::mcp::stdio::new_stdio_sessions();
        thinkingroot_serve::mcp::stdio::run(engine, default_ws, sessions).await;
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
        println!(
            "  MCP HTTP:  http://{}:{}/mcp/sse  (Gemini CLI)",
            host, port
        );
        println!(
            "  MCP stdio: root serve --mcp-stdio  (Claude Code, Codex, Cursor, VS Code, Windsurf, Zed)"
        );
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
    // Pass workspace_root for branch API endpoints when exactly one workspace is mounted.
    let workspace_root = if resolved_paths.len() == 1 {
        Some(resolved_paths[0].1.clone())
    } else {
        None
    };
    let state = AppState::new_with_root(engine, api_key, workspace_root.clone());

    // Spawn background stream-branch cleanup for single-workspace mounts.
    // Multi-workspace mounts skip this — a single cleanup task can't safely
    // scan across heterogeneous workspace roots with different configs.
    let _cleanup_handle = if let Some(ref root) = workspace_root {
        let ws_name = resolved_paths[0].0.clone();
        let engine = state.engine.read().await;
        let streams_cfg = engine
            .workspace_streams_config(&ws_name)
            .unwrap_or_default();
        let branch_engines = Some(engine.branch_engines_arc());
        drop(engine);
        Some(thinkingroot_serve::maintenance::spawn_stream_cleanup(
            state.sessions.clone(),
            root.clone(),
            streams_cfg,
            branch_engines,
        ))
    } else {
        None
    };

    let router = build_router_opts(state, !no_rest, !no_mcp);
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("server listening on {}", addr);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Variant of [`run_serve`] that accepts a pre-bound [`tokio::net::TcpListener`].
///
/// Used by `run_graph` which probes for an available port before calling this.
#[allow(clippy::too_many_arguments)]
async fn run_serve_with_listener(
    listener: tokio::net::TcpListener,
    api_key: Option<String>,
    paths: Vec<PathBuf>,
    name: Option<String>,
    no_rest: bool,
    no_mcp: bool,
    mcp_stdio: bool,
    branch: Option<String>,
) -> anyhow::Result<()> {
    if no_rest && no_mcp {
        anyhow::bail!("--no-rest and --no-mcp cannot be used together: nothing to serve");
    }

    // Resolve workspace paths: explicit --path > --name > registry
    let resolved_paths: Vec<(String, PathBuf)> = if !paths.is_empty() {
        paths
            .iter()
            .map(|p| {
                let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                let ws_name = abs
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "default".to_string());
                (ws_name, abs)
            })
            .collect()
    } else {
        let registry = WorkspaceRegistry::load()?;
        let workspaces = if let Some(ref ws_name) = name {
            let entry = registry
                .workspaces
                .iter()
                .find(|w| w.name == *ws_name)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "workspace \"{}\" not found. Run `root workspace list`.",
                        ws_name
                    )
                })?;
            vec![(entry.name.clone(), entry.path.clone())]
        } else {
            registry
                .workspaces
                .iter()
                .map(|w| (w.name.clone(), w.path.clone()))
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
    for (ws_name, abs_path) in &resolved_paths {
        if let Some(ref branch_name) = branch {
            let data_dir = resolve_data_dir(abs_path, Some(branch_name));
            if !data_dir.exists() {
                anyhow::bail!(
                    "branch '{}' not found for workspace '{}' — expected data dir at {}. \
                     Run `root branch {}` first.",
                    branch_name,
                    ws_name,
                    data_dir.display(),
                    branch_name,
                );
            }
            engine
                .mount_with_data_dir(ws_name.clone(), abs_path.clone(), data_dir.clone())
                .await?;
        } else {
            engine.mount(ws_name.clone(), abs_path.clone()).await?;
        }
    }

    if mcp_stdio {
        let default_ws = resolved_paths.first().map(|(ws_name, _)| ws_name.clone());
        let engine = Arc::new(RwLock::new(engine));
        let sessions = thinkingroot_serve::mcp::stdio::new_stdio_sessions();
        thinkingroot_serve::mcp::stdio::run(engine, default_ws, sessions).await;
        return Ok(());
    }

    let workspace_root = if resolved_paths.len() == 1 {
        Some(resolved_paths[0].1.clone())
    } else {
        None
    };
    let state = AppState::new_with_root(engine, api_key, workspace_root.clone());

    let _cleanup_handle = if let Some(ref root) = workspace_root {
        let ws_name = resolved_paths[0].0.clone();
        let engine = state.engine.read().await;
        let streams_cfg = engine
            .workspace_streams_config(&ws_name)
            .unwrap_or_default();
        let branch_engines = Some(engine.branch_engines_arc());
        drop(engine);
        Some(thinkingroot_serve::maintenance::spawn_stream_cleanup(
            state.sessions.clone(),
            root.clone(),
            streams_cfg,
            branch_engines,
        ))
    } else {
        None
    };

    let router = build_router_opts(state, !no_rest, !no_mcp);

    tracing::info!("server listening on {:?}", listener.local_addr());

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
        println!(
            "  {} {}",
            console::style("✓ Service file:").green().bold(),
            plist_path.display()
        );
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
        println!(
            "  {} {}",
            console::style("✓ Service file:").green().bold(),
            service_path.display()
        );
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
        println!(
            "  {} {}",
            console::style("✓ Script:").green().bold(),
            ps_path.display()
        );
        println!();
        println!("  Run as Administrator:");
        println!(
            "    powershell -ExecutionPolicy Bypass -File {}",
            ps_path.display()
        );
    }

    Ok(())
}
