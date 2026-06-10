use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;

use tokio::sync::RwLock;

use thinkingroot_branch::snapshot::resolve_data_dir;
use thinkingroot_core::WorkspaceRegistry;
use thinkingroot_core::cortex;
use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::rest::{AppState, build_router_opts, set_models_warm};

use crate::cortex_client;

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
///
/// Cortex Protocol contract: before binding, this function checks for
/// an existing healthy daemon via `cortex_client::resolve_engine`.
/// If one is found, we print a friendly "engine already running"
/// message and exit 0 — no second listener, no SIGKILL, no lock
/// torture. If none is found, we proceed with the original bind path
/// and write the cortex lockfile after a successful bind.
///
/// `mcp_stdio` mode bypasses cortex entirely: the editor invokes us
/// over stdin/stdout, no HTTP, no lock.
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

    // ── Cortex attach-or-bind check ────────────────────────────────
    // Skipped for --mcp-stdio (no listener, no HTTP, no lock).
    if !mcp_stdio {
        match cortex_client::resolve_engine(cortex::EngineIntent::Serve).await {
            Ok(cortex::EngineConnection::Remote { host: ref h, port: p, started_by, pid }) => {
                println!();
                println!(
                    "  {} engine already running on {}:{}",
                    console::style("ThinkingRoot").green().bold(),
                    h,
                    p,
                );
                println!(
                    "  {} pid={} started_by={}",
                    console::style("note:").dim(),
                    pid,
                    started_by.as_str(),
                );
                println!(
                    "  Use {} to inspect or {} to stop the running daemon.",
                    console::style("`curl http://127.0.0.1:31760/livez`").cyan(),
                    console::style(format!("`kill {pid}`")).cyan(),
                );
                println!();
                return Ok(());
            }
            Ok(cortex::EngineConnection::InProcess) => {
                // No daemon running — proceed with normal bind path.
            }
            Ok(cortex::EngineConnection::Stdio) => {
                // unreachable because mcp_stdio == false; handled
                // defensively to keep the match exhaustive without a
                // bare `_` arm that would mask future variants.
                unreachable!("Stdio connection only returned for McpStdio intent");
            }
            Ok(cortex::EngineConnection::SpawnRequired { .. }) => {
                unreachable!("CLI resolve_engine never returns SpawnRequired (handled internally as detached spawn)");
            }
            Ok(cortex::EngineConnection::RepairNeeded { failing_check_ids }) => {
                anyhow::bail!(
                    "ThinkingRoot engine cannot start: missing {failing_check_ids:?}. Run `root doctor --fix` to repair."
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "cortex resolve failed; falling back to direct bind");
            }
        }
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

    // Path split: mcp_stdio never binds a TCP listener and never writes a
    // cortex.lock (per .claude/rules/cortex-protocol.md "MCP stdio bypasses
    // cortex"). The HTTP path below reserves the port FIRST, writes the
    // cortex.lock IMMEDIATELY, and only then begins workspace mounts — so
    // any panic / error during mount cannot leave a bound port without a
    // matching lock (Task 8 race fix).
    if mcp_stdio {
        let mut engine = QueryEngine::new();
        let single_explicit = name.is_some() || resolved_paths.len() == 1;
        mount_workspaces_into_engine(
            &mut engine,
            &resolved_paths,
            branch.as_deref(),
            single_explicit,
        )
        .await?;

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

    // ── HTTP path: bind → write_lock → mount → accept ────────────────
    //
    // Task 8 (Slice C) race fix: prior to this reordering the listener
    // was bound after workspace mounts succeeded and the lockfile was
    // written AFTER bind. That left two distinct crash windows where
    // `root serve` could leave the port reserved by the OS while no
    // cortex.lock existed — the next surface that tried to attach saw
    // no lock and auto-spawned into a "port already in use" conflict.
    //
    // New order:
    //   1. Bind the TCP listener (OS reserves the port).
    //   2. IMMEDIATELY write cortex.lock with the listener's actual port
    //      (handles port==0 OS-assigned ports for tests).
    //   3. Install a `LockfileGuard` RAII sentinel — Drop removes the
    //      lockfile on any panic / early return between here and the
    //      accept loop terminating cleanly.
    //   4. Mount workspaces and build the runtime state.
    //   5. axum::serve(...).await — accept loop runs until shutdown.
    //   6. On clean shutdown the explicit `cortex::remove_lock()` call
    //      below removes the lock; the guard's Drop is then a no-op
    //      (remove_lock is idempotent on NotFound).
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;
    let actual_port = listener.local_addr()?.port();
    tracing::info!("server listening on {} (bound port {})", addr, actual_port);

    // Write the lockfile IMMEDIATELY. From this point on, any failure
    // before the accept loop completes successfully must clean it up —
    // hence the `LockfileGuard` below.
    let lock = cortex_client::build_cli_lock(actual_port);
    cortex::write_lock(&lock)
        .map_err(|e| anyhow::anyhow!("failed to write cortex.lock: {e}"))?;
    let _lock_guard = LockfileGuard;

    // Mount workspaces AFTER lock is written — a panic here trips the
    // guard's Drop and removes the lockfile so the next caller does not
    // observe a phantom lock pointing at a dead bind.
    let mut engine = QueryEngine::new();
    let single_explicit = name.is_some() || resolved_paths.len() == 1;
    mount_workspaces_into_engine(
        &mut engine,
        &resolved_paths,
        branch.as_deref(),
        single_explicit,
    )
    .await?;

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

    // Flow cron scheduler — triggers headless runs for flows whose definition
    // carries a `schedule` (5-field cron, UTC). Reads the workspace at tick time.
    let _flow_cron_handle = thinkingroot_serve::flow_cron::spawn_flow_cron(state.clone());

    for (ws_name, path, _) in &resolved_paths {
        state
            .mounted_workspace_roots
            .write()
            .await
            .insert(ws_name.clone(), path.clone());
        state
            .live_sync
            .register_workspace(ws_name, path.clone())
            .await;
    }

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

    // A7-SECURITY ⑥ — periodic integrity snapshots of the main graph
    // (rollback-to-known-good). No-op handle unless TR_INTEGRITY_SNAPSHOTS=1.
    let _integrity_handle = workspace_root
        .as_ref()
        .map(|root| thinkingroot_serve::maintenance::spawn_integrity_snapshots(root.clone()));

    // Learning-signal retention: prune unbounded append-only signal tables
    // (retrieval_usage / verify verdicts). Default 90d; TR_SIGNAL_RETENTION_DAYS=0 off.
    let _signal_retention_handle = workspace_root
        .as_ref()
        .map(|root| thinkingroot_serve::maintenance::spawn_signal_retention(root.clone()));

    // Idle learn-to-rank trainer (item 10): fold retrieval-usage signal into
    // per-claim usefulness priors. No-op handle unless TR_LEARNED_PRIOR=1.
    let _retrieval_prior_handle = workspace_root
        .as_ref()
        .map(|root| thinkingroot_serve::maintenance::spawn_retrieval_prior_trainer(root.clone()));

    // Slice 3 — file-system watcher for workspace lifecycle events.
    // Read the active workspace_root via the same RwLock the mount
    // handler updates so a desktop `workspace_set_active` flips the
    // watcher's target automatically.
    let watcher_active = state.clone();
    let watcher_mounted = state.clone();
    let watcher_handle = thinkingroot_serve::workspace_watcher::spawn_workspace_watcher(
        move || {
            watcher_active
                .workspace_root
                .try_read()
                .ok()
                .and_then(|g| g.clone())
        },
        move || {
            watcher_mounted
                .mounted_workspace_roots
                .try_read()
                .ok()
                .map(|g| g.values().cloned().collect())
                .unwrap_or_default()
        },
        thinkingroot_serve::workspace_watcher::WatcherConfig::default(),
    );
    let watcher_arc = std::sync::Arc::new(watcher_handle);
    state.attach_workspace_watcher(watcher_arc.clone()).await;
    // Wire the source-tree → state-actor bridge so edits under the
    // workspace root flip `fingerprint_match` to false and the
    // desktop badge surfaces "Behind" honestly.
    thinkingroot_serve::live_sync::spawn_live_sync_bridge(
        state.clone(),
        state.live_sync.clone(),
    );

    // Warm-on-boot (cloud cold-start SOTA): front-load the embed + rerank
    // ONNX models so the FIRST real query is fast and any idle-checkpoint
    // captures an already-warm memory image. Opt-in via TR_WARM_ON_BOOT=1
    // (the cloud provisioner sets it; desktop/CLI users skip the ~7 s cost
    // and keep lazy-loading). Runs before the accept loop so /livez and
    // /readyz only go green once models are resident — the provisioner then
    // checkpoints a guaranteed-warm engine.
    if std::env::var("TR_WARM_ON_BOOT").as_deref() == Ok("1") {
        let warm_started = std::time::Instant::now();
        {
            let eng = state.engine.read().await;
            for ws_entry in &resolved_paths {
                // First tuple element is the workspace name in both the
                // bound-port (2-tuple) and standard (3-tuple) serve paths.
                let ws_name = &ws_entry.0;
                match eng.warm_models(ws_name).await {
                    Ok(()) => {
                        tracing::info!(workspace = %ws_name, "warm-on-boot: models loaded")
                    }
                    Err(e) => tracing::warn!(
                        workspace = %ws_name,
                        error = %e,
                        "warm-on-boot failed (will lazy-load on first use)"
                    ),
                }
            }
        }
        // Only flip /readyz to ready if we actually warmed a workspace's
        // models. The cloud daemon boots with zero workspaces (mounted later
        // over REST), so leave the flag false here — warm-on-mount sets it
        // once those models load, keeping /readyz honest.
        if !resolved_paths.is_empty() {
            set_models_warm();
        }
        tracing::info!(
            elapsed_ms = warm_started.elapsed().as_millis() as u64,
            "warm-on-boot complete"
        );
    }

    let router = build_router_opts(state, !no_rest, !no_mcp);

    let serve_result = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await;

    // Best-effort lock cleanup on clean shutdown. `remove_lock` is
    // idempotent on NotFound, so this is harmless if `_lock_guard`
    // ends up running first on an unwind.
    if let Err(e) = cortex::remove_lock() {
        tracing::warn!(error = %e, "failed to remove cortex.lock on shutdown");
    }

    serve_result?;
    Ok(())
}

/// Mount each resolved workspace into `engine`, honouring the same
/// resilience rules as the inline loop used to: a single deleted
/// substrate is a per-workspace WARN, not a fatal error for the whole
/// daemon, *unless* the caller explicitly named one workspace (via
/// `--name <ws>` or a single `--path <p>`), in which case missing it
/// is fatal because there's nothing else to mount.
///
/// Extracted from the inline body of `run_serve` so the HTTP path can
/// defer mount-time work to AFTER the cortex.lock is written (Task 8
/// race fix) while the `mcp_stdio` path keeps mounting up-front
/// without binding a TCP listener.
async fn mount_workspaces_into_engine(
    engine: &mut QueryEngine,
    resolved_paths: &[(String, PathBuf, u16)],
    branch: Option<&str>,
    single_explicit: bool,
) -> anyhow::Result<()> {
    let mut skipped: Vec<(String, std::path::PathBuf, String)> = Vec::new();
    for (ws_name, abs_path, _ws_port) in resolved_paths {
        if let Some(branch_name) = branch {
            let data_dir = resolve_data_dir(abs_path, Some(branch_name));
            if !data_dir.exists() {
                if single_explicit {
                    anyhow::bail!(
                        "branch '{}' not found for workspace '{}' — expected data dir at {}. \
                         Run `root branch {}` first.",
                        branch_name,
                        ws_name,
                        data_dir.display(),
                        branch_name,
                    );
                }
                tracing::warn!(
                    "skipping workspace '{}': branch '{}' data dir missing at {} \
                     (run `root branch {}` to recreate it, or `root workspace remove {}` to clear the registry entry)",
                    ws_name, branch_name, data_dir.display(), branch_name, ws_name,
                );
                skipped.push((
                    ws_name.clone(),
                    abs_path.clone(),
                    format!("branch '{branch_name}' data dir missing"),
                ));
                continue;
            }
            match engine
                .mount_with_data_dir(ws_name.clone(), abs_path.clone(), data_dir.clone())
                .await
            {
                Ok(()) => tracing::info!(
                    "mounted workspace '{}' from branch '{}' ({})",
                    ws_name,
                    branch_name,
                    data_dir.display()
                ),
                Err(e) if single_explicit => return Err(e.into()),
                Err(e) => {
                    tracing::warn!(
                        "skipping workspace '{}' (branch '{}'): {e}",
                        ws_name, branch_name
                    );
                    skipped.push((ws_name.clone(), abs_path.clone(), e.to_string()));
                }
            }
        } else {
            match engine.mount(ws_name.clone(), abs_path.clone()).await {
                Ok(()) => tracing::info!(
                    "mounted workspace '{}' from {}",
                    ws_name,
                    abs_path.display()
                ),
                Err(e) if single_explicit => return Err(e.into()),
                Err(e) => {
                    tracing::warn!(
                        "skipping workspace '{}' at {}: {e}",
                        ws_name,
                        abs_path.display()
                    );
                    skipped.push((ws_name.clone(), abs_path.clone(), e.to_string()));
                }
            }
        }
    }
    if !skipped.is_empty() {
        tracing::warn!(
            "{} workspace(s) skipped at startup; daemon continues with the rest. \
             Run `root doctor` for detail or `root workspace remove <name>` to clear stale registry entries.",
            skipped.len()
        );
    }
    Ok(())
}

/// Removes cortex.lock on Drop. Used during `root serve` setup to
/// ensure that a panic or early-return between lock-write and the
/// accept-loop's clean shutdown does not leave a phantom lockfile
/// pointing at a port that no live daemon owns.
///
/// The clean shutdown path explicitly calls `cortex::remove_lock()`
/// after `axum::serve(...).await`, so by the time Drop runs the file
/// is usually already gone — `remove_lock` is idempotent on NotFound
/// (see `thinkingroot_core::cortex::remove_lock`), so the double-call
/// is harmless. This guard is defensive only; it earns its keep on
/// unwind paths where the explicit cleanup is skipped.
struct LockfileGuard;

impl Drop for LockfileGuard {
    fn drop(&mut self) {
        if let Err(e) = cortex::remove_lock() {
            tracing::warn!(error = %e, "failed to remove cortex.lock during shutdown");
        }
    }
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

    // Mirror the resilient mount loop from `run_serve` — see comment
    // there for the rationale. A registered workspace whose substrate
    // was deleted out from under us is not fatal for the whole daemon.
    let mut engine = QueryEngine::new();
    let single_explicit = name.is_some() || resolved_paths.len() == 1;
    for (ws_name, abs_path) in &resolved_paths {
        if let Some(ref branch_name) = branch {
            let data_dir = resolve_data_dir(abs_path, Some(branch_name));
            if !data_dir.exists() {
                if single_explicit {
                    anyhow::bail!(
                        "branch '{}' not found for workspace '{}' — expected data dir at {}. \
                         Run `root branch {}` first.",
                        branch_name,
                        ws_name,
                        data_dir.display(),
                        branch_name,
                    );
                }
                tracing::warn!(
                    "skipping workspace '{}': branch '{}' data dir missing at {}",
                    ws_name, branch_name, data_dir.display()
                );
                continue;
            }
            match engine
                .mount_with_data_dir(ws_name.clone(), abs_path.clone(), data_dir.clone())
                .await
            {
                Ok(()) => {}
                Err(e) if single_explicit => return Err(e.into()),
                Err(e) => tracing::warn!(
                    "skipping workspace '{}' (branch '{}'): {e}",
                    ws_name, branch_name
                ),
            }
        } else {
            match engine.mount(ws_name.clone(), abs_path.clone()).await {
                Ok(()) => {}
                Err(e) if single_explicit => return Err(e.into()),
                Err(e) => tracing::warn!(
                    "skipping workspace '{}' at {}: {e}",
                    ws_name,
                    abs_path.display()
                ),
            }
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

    // Flow cron scheduler — triggers headless runs for flows whose definition
    // carries a `schedule` (5-field cron, UTC). Reads the workspace at tick time.
    let _flow_cron_handle = thinkingroot_serve::flow_cron::spawn_flow_cron(state.clone());

    for (ws_name, path) in &resolved_paths {
        state
            .mounted_workspace_roots
            .write()
            .await
            .insert(ws_name.clone(), path.clone());
        state
            .live_sync
            .register_workspace(ws_name, path.clone())
            .await;
    }

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

    // A7-SECURITY ⑥ — periodic integrity snapshots of the main graph
    // (rollback-to-known-good). No-op handle unless TR_INTEGRITY_SNAPSHOTS=1.
    let _integrity_handle = workspace_root
        .as_ref()
        .map(|root| thinkingroot_serve::maintenance::spawn_integrity_snapshots(root.clone()));

    // Learning-signal retention: prune unbounded append-only signal tables
    // (retrieval_usage / verify verdicts). Default 90d; TR_SIGNAL_RETENTION_DAYS=0 off.
    let _signal_retention_handle = workspace_root
        .as_ref()
        .map(|root| thinkingroot_serve::maintenance::spawn_signal_retention(root.clone()));

    // Idle learn-to-rank trainer (item 10): fold retrieval-usage signal into
    // per-claim usefulness priors. No-op handle unless TR_LEARNED_PRIOR=1.
    let _retrieval_prior_handle = workspace_root
        .as_ref()
        .map(|root| thinkingroot_serve::maintenance::spawn_retrieval_prior_trainer(root.clone()));

    // Slice 3 — file-system watcher (parity with the bound-port path).
    let watcher_active = state.clone();
    let watcher_mounted = state.clone();
    let watcher_handle = thinkingroot_serve::workspace_watcher::spawn_workspace_watcher(
        move || {
            watcher_active
                .workspace_root
                .try_read()
                .ok()
                .and_then(|g| g.clone())
        },
        move || {
            watcher_mounted
                .mounted_workspace_roots
                .try_read()
                .ok()
                .map(|g| g.values().cloned().collect())
                .unwrap_or_default()
        },
        thinkingroot_serve::workspace_watcher::WatcherConfig::default(),
    );
    let watcher_arc = std::sync::Arc::new(watcher_handle);
    state.attach_workspace_watcher(watcher_arc.clone()).await;
    thinkingroot_serve::live_sync::spawn_live_sync_bridge(
        state.clone(),
        state.live_sync.clone(),
    );

    // Warm-on-boot (cloud cold-start SOTA): front-load the embed + rerank
    // ONNX models so the FIRST real query is fast and any idle-checkpoint
    // captures an already-warm memory image. Opt-in via TR_WARM_ON_BOOT=1
    // (the cloud provisioner sets it; desktop/CLI users skip the ~7 s cost
    // and keep lazy-loading). Runs before the accept loop so /livez and
    // /readyz only go green once models are resident — the provisioner then
    // checkpoints a guaranteed-warm engine.
    if std::env::var("TR_WARM_ON_BOOT").as_deref() == Ok("1") {
        let warm_started = std::time::Instant::now();
        {
            let eng = state.engine.read().await;
            for ws_entry in &resolved_paths {
                // First tuple element is the workspace name in both the
                // bound-port (2-tuple) and standard (3-tuple) serve paths.
                let ws_name = &ws_entry.0;
                match eng.warm_models(ws_name).await {
                    Ok(()) => {
                        tracing::info!(workspace = %ws_name, "warm-on-boot: models loaded")
                    }
                    Err(e) => tracing::warn!(
                        workspace = %ws_name,
                        error = %e,
                        "warm-on-boot failed (will lazy-load on first use)"
                    ),
                }
            }
        }
        // Only flip /readyz to ready if we actually warmed a workspace's
        // models. The cloud daemon boots with zero workspaces (mounted later
        // over REST), so leave the flag false here — warm-on-mount sets it
        // once those models load, keeping /readyz honest.
        if !resolved_paths.is_empty() {
            set_models_warm();
        }
        tracing::info!(
            elapsed_ms = warm_started.elapsed().as_millis() as u64,
            "warm-on-boot complete"
        );
    }

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

/// Generate and install the OS login agent so `root serve` auto-starts.
///
/// Thin shim over [`crate::service::install`] — kept so the existing
/// `root serve --install-service` flag works during the cutover.
/// Prefer `root service install` for new call sites.
pub fn install_service() -> anyhow::Result<()> {
    let outcome = crate::service::install()
        .map_err(|e| anyhow::anyhow!("install login agent: {e}"))?;
    crate::service::print_outcome(&outcome, crate::service::OutcomeKind::Install)
}
