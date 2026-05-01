//! Managed `root serve` sidecar.
//!
//! ThinkingRoot Desktop bundles the OSS engine binary (`root`) and
//! launches it as a child process bound to `127.0.0.1`. The desktop
//! webview talks to this sidecar over loopback HTTP — never to a
//! remote service — so the app stays local-first by construction.
//!
//! Resolution order for the binary:
//!   1. `THINKINGROOT_ROOT_BINARY` env var (testing escape hatch).
//!   2. Tauri-bundled sidecar at
//!      `<resource_dir>/binaries/thinkingroot-agent-runtime-<triple>`.
//!   3. `root` from `$PATH` (dev fallback).
//!
//! The handle is parked inside [`AppState`] so the `Destroyed` event
//! can reap it when the user quits.
//!
//! Risk R1 (per `docs/phase-f-trust-verify-design.md` companion notes
//! to the OSS plan): if the bundled binary is missing or fails its
//! handshake, the sidecar startup logs a warning and the desktop
//! continues running — the engine surfaces (Brain, Privacy) just
//! return empty until the user installs `root` themselves.

use std::path::PathBuf;
use std::process::Stdio;

use tauri::{AppHandle, Manager, Runtime};
use tokio::process::{Child, Command};

use crate::state::{AppState, SidecarHandle};

/// Default loopback port for the local sidecar. Chosen to avoid
/// the engine's 3000 default (collides with common dev tools) and
/// the cloud's 3100-grid; settable via env for tests.
const DEFAULT_PORT: u16 = 31760;
const HOST: &str = "127.0.0.1";

/// Spawn the sidecar. Records the child handle into [`AppState`] so
/// it can be reaped on app exit. Errors are logged but not bubbled —
/// the desktop must keep running even if the engine binary is
/// unavailable on this machine.
pub async fn spawn<R: Runtime>(app: &AppHandle<R>) {
    let port = sidecar_port();
    let binary = match resolve_binary(app) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "no `root` binary found — sidecar disabled. Install ThinkingRoot \
                 via `cargo install thinkingroot-cli` or set \
                 THINKINGROOT_ROOT_BINARY to a custom path."
            );
            return;
        }
    };

    tracing::info!(
        binary = %binary.display(),
        host = HOST,
        port,
        "spawning ThinkingRoot sidecar",
    );

    // Ensure the port is free before we try to bind it.  `root serve`
    // bails with "Address already in use" if the port is taken.  This
    // happens frequently in dev if the previous `pnpm tauri dev` was
    // killed with SIGKILL (preventing `shutdown()` from running).
    cleanup_stale_sidecar(port).await;

    let mut cmd = Command::new(&binary);
    cmd.arg("serve")
        .arg("--host")
        .arg(HOST)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Forward provider keys from the shared credentials.toml into the
    // sidecar's process env. Without this, a user who configures Azure
    // (or any other provider) from the desktop Settings UI would have
    // their key written to credentials.toml but never reach the engine,
    // because the engine resolves keys via `std::env::var(api_key_env)`
    // first. Process env still wins when both are set — operators who
    // launch the desktop from a shell with `export AZURE_OPENAI_API_KEY=…`
    // see no behaviour change.
    match thinkingroot_core::Credentials::load() {
        Ok(creds) => {
            let mut count = 0usize;
            for (k, v_creds) in creds.as_env_map() {
                // Process env wins (kept as-is for backwards-compat with
                // operators who launch the desktop from a shell with
                // `export X=…`).  M9: when both an env var and a
                // credentials.toml entry exist for the same key but
                // disagree, log a warning so a stale shell-export
                // value doesn't silently shadow a freshly-rotated
                // credential — exactly the trap that bit our Azure
                // flow this session.
                match std::env::var(&k) {
                    Ok(v_env) if !v_env.is_empty() => {
                        if v_env != v_creds {
                            tracing::warn!(
                                env = %k,
                                "process env var differs from credentials.toml \
                                 — env wins.  If auth fails, run `unset {}` and \
                                 rely on credentials.toml.",
                                k
                            );
                        }
                        // env wins; nothing to inject.
                    }
                    _ => {
                        cmd.env(&k, &v_creds);
                        count += 1;
                    }
                }
            }
            if count > 0 {
                tracing::info!(
                    injected = count,
                    "seeded sidecar with credentials from credentials.toml"
                );
            }
        }
        Err(e) => {
            tracing::debug!(?e, "credentials.toml unreadable; sidecar inherits desktop env only");
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(%err, ?binary, "failed to spawn sidecar — engine surfaces will be empty");
            return;
        }
    };

    let pid = child.id();
    // Detach stdout/stderr line readers but keep ownership of `child`
    // here so `shutdown()` can drive a graceful stop.  Pre-fix this
    // function moved `child` into a detached `wait().await` task and
    // the SidecarHandle held only metadata, so `shutdown()` was a
    // no-op (the comment claimed kill_on_drop but the Child had
    // already been moved away from the parent's reach).
    spawn_log_forwarders(&mut child, &binary);

    // Wait for the sidecar's HTTP server to actually accept connections
    // before storing the handle.  `cmd.spawn()` only forks the OS
    // process — the engine still needs to load the workspace registry,
    // mount each workspace's CozoDB + vector index, and bind the TCP
    // listener.  That can take several seconds on a warm machine and
    // longer on first-compile cold starts.
    //
    // CRITICAL FIX: We pass `&mut child` so we can call `try_wait()`
    // during polling.  If the child exits (crash, mount failure, etc.),
    // we detect it immediately instead of burning the full 120s timeout.
    let livez_url = format!("http://{}:{}/livez", HOST, port);
    let ready = wait_for_sidecar_ready(&livez_url, &mut child, 120).await;

    if !ready {
        // Child died or never became ready.  Check if it exited.
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::error!(
                    ?status,
                    "sidecar process exited during startup — engine surfaces will be empty. \
                     Check `root workspace list` for broken workspace registrations."
                );
            }
            Ok(None) => {
                // Still running but never responded to /livez.
                tracing::error!(
                    "sidecar process is running but never became ready (no /livez response \
                     after 120s). Killing it and falling back to in-process compile."
                );
                let _ = child.kill().await;
            }
            Err(e) => {
                tracing::error!(
                    %e,
                    "failed to check sidecar process status"
                );
            }
        }
        // DO NOT store the handle — leave state.sidecar = None so
        // workspace_compile falls back to in-process compile.
        return;
    }

    // Sidecar is up and healthy.  Store the handle.
    let child_arc = std::sync::Arc::new(tokio::sync::Mutex::new(Some(child)));

    {
        let state = app.state::<AppState>();
        let mut guard = state.sidecar.lock().await;
        *guard = Some(SidecarHandle {
            port,
            host: HOST.to_string(),
            pid,
            child: child_arc.clone(),
        });
    }

    // Spawn a background watchdog: if the child exits unexpectedly at
    // any point after startup, clear `state.sidecar` so that future
    // compile requests fall back to in-process instead of hitting
    // "connection refused" against a dead process.
    let app_for_watchdog = app.app_handle().clone();
    let child_arc_for_watchdog = child_arc;
    tokio::spawn(async move {
        // Wait for the child to exit (blocks until process terminates).
        let exit_status = {
            let mut guard = child_arc_for_watchdog.lock().await;
            if let Some(ref mut c) = *guard {
                Some(c.wait().await)
            } else {
                None
            }
        };

        if let Some(status) = exit_status {
            match status {
                Ok(s) if s.success() => {
                    tracing::info!(?s, "sidecar exited cleanly");
                }
                Ok(s) => {
                    tracing::error!(
                        ?s,
                        "sidecar exited unexpectedly — clearing handle so compile \
                         falls back to in-process"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        %e,
                        "sidecar wait failed — clearing handle"
                    );
                }
            }

            // Clear the sidecar handle so future compiles go in-process.
            let state = app_for_watchdog.state::<AppState>();
            let mut guard = state.sidecar.lock().await;
            *guard = None;
        }
    });
}

/// Reap the sidecar on app exit.  Tries a graceful stop first
/// (drop stdin → wait up to 2 s) and escalates to SIGKILL if the
/// child is still running.  Idempotent: calling `shutdown` after
/// the child has already exited is a no-op.
///
/// Pre-fix this function logged "shutting down" and did literally
/// nothing else — the sidecar died only because tokio runtime
/// tear-down dropped detached tasks, which in turn dropped the
/// Child and triggered `kill_on_drop`.  That's fast on Unix but
/// gives the engine zero chance to flush a CozoDB checkpoint.
pub async fn shutdown<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<AppState>();
    let handle = {
        let mut guard = state.sidecar.lock().await;
        guard.take()
    };
    let Some(handle) = handle else {
        return;
    };
    tracing::info!(pid = ?handle.pid, "shutting down sidecar");
    let mut child_guard = handle.child.lock().await;
    let Some(mut child) = child_guard.take() else {
        // Already exited or already shut down; nothing to do.
        return;
    };
    // Drop stdin so the engine's stdin reader (when any) sees EOF
    // and can finish draining cleanly.  `root serve` doesn't read
    // stdin today but the close is harmless and future-proof.
    drop(child.stdin.take());

    match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => {
            tracing::info!(?status, "sidecar exited gracefully");
        }
        Ok(Err(err)) => {
            tracing::warn!(%err, "sidecar wait failed during shutdown; sending SIGKILL");
            let _ = child.kill().await;
        }
        Err(_) => {
            tracing::warn!("sidecar did not exit within 2s; sending SIGKILL");
            // SIGKILL.  Tokio's start_kill is non-blocking; the
            // following short wait reaps the child so we don't leak
            // a zombie.
            let _ = child.kill().await;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                child.wait(),
            )
            .await;
        }
    }
}

fn sidecar_port() -> u16 {
    std::env::var("THINKINGROOT_DESKTOP_SIDECAR_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT)
}

fn resolve_binary<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("THINKINGROOT_ROOT_BINARY") {
        if !override_path.is_empty() {
            let p = PathBuf::from(override_path);
            if p.exists() {
                return Some(p);
            }
        }
    }

    if let Ok(resource_dir) = app.path().resource_dir() {
        // Resolution order for the target triple:
        //  1. `TARGET` env var (set by `cargo build` / cross).
        //  2. `TAURI_TARGET_TRIPLE` env var (set by `tauri build`).
        //  3. The current machine's native triple via `std::env::consts`.
        //     This covers `pnpm tauri dev` which sets neither of the above.
        let triple = std::env::var("TARGET")
            .or_else(|_| std::env::var("TAURI_TARGET_TRIPLE"))
            .ok()
            .or_else(native_triple);

        let candidate = match triple {
            Some(t) => resource_dir
                .join("binaries")
                .join(format!("thinkingroot-agent-runtime-{t}{}", exe_suffix())),
            None => resource_dir
                .join("binaries")
                .join(format!("thinkingroot-agent-runtime{}", exe_suffix())),
        };
        if candidate.exists() {
            tracing::debug!(path = %candidate.display(), "resolved sidecar from resource_dir");
            return Some(candidate);
        }
    }

    which_root()
}

/// Best-effort native target triple for the current machine.
/// Returns `None` only if the OS/arch combo is not covered — unlikely
/// in practice since we only ship macOS and Linux builds.
fn native_triple() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => {
            tracing::debug!(arch = other, "unrecognised arch — skipping native triple");
            return None;
        }
    };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        other => {
            tracing::debug!(os = other, "unrecognised OS — skipping native triple");
            return None;
        }
    };
    Some(format!("{arch}-{os}"))
}

fn which_root() -> Option<PathBuf> {
    let bin = format!("root{}", exe_suffix());
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(&bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Poll `livez_url` every 500 ms until the sidecar's HTTP server
/// accepts a request, or until `timeout_secs` has elapsed.
///
/// **Critical fix:** Also checks `child.try_wait()` on each iteration.
/// If the child process has exited (crash, mount failure, port conflict,
/// etc.) we return `false` immediately instead of burning the full
/// timeout — the old code would poll a dead process's port for 60 full
/// seconds, then store a `SidecarHandle` pointing at nothing.
///
/// Returns `true` if the sidecar became ready, `false` otherwise.
async fn wait_for_sidecar_ready(
    livez_url: &str,
    child: &mut Child,
    timeout_secs: u64,
) -> bool {
    use tokio::time::{Duration, Instant, sleep};

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut attempt = 0u32;

    loop {
        // ── Check if the child has already exited ──────────────────
        // This is the key fix: if `root serve` crashes during startup
        // (e.g. a workspace mount fails, port already taken after our
        // cleanup, panic in engine init), we detect it here instead
        // of polling a dead port for the full timeout.
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::error!(
                    ?status,
                    url = %livez_url,
                    attempts = attempt,
                    "sidecar process exited during readiness probe — aborting wait"
                );
                return false;
            }
            Ok(None) => {
                // Still running — continue polling.
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    "try_wait() failed on sidecar child — continuing poll"
                );
            }
        }

        if Instant::now() >= deadline {
            tracing::warn!(
                url = %livez_url,
                timeout = timeout_secs,
                "sidecar did not become ready within timeout — \
                 compile requests may fail until it is up"
            );
            return false;
        }

        match client.get(livez_url).send().await {
            Ok(r) if r.status().is_success() => {
                tracing::info!(
                    url = %livez_url,
                    attempts = attempt + 1,
                    "sidecar HTTP server is ready"
                );
                return true;
            }
            Ok(r) => {
                tracing::debug!(
                    status = %r.status(),
                    attempt,
                    "sidecar /livez returned non-OK, retrying"
                );
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    attempt,
                    "sidecar not reachable yet, retrying"
                );
            }
        }

        attempt += 1;
        sleep(Duration::from_millis(500)).await;
    }
}

async fn cleanup_stale_sidecar(port: u16) {
    #[cfg(unix)]
    {
        use std::process::Command as StdCommand;
        // -t: terse (PID only)
        // -i: internet (port)
        // -sTCP:LISTEN: only listeners
        let output = StdCommand::new("lsof")
            .arg("-t")
            .arg("-i")
            .arg(format!("tcp:{}", port))
            .arg("-sTCP:LISTEN")
            .output();

        if let Ok(out) = output {
            let pids = String::from_utf8_lossy(&out.stdout);
            for pid_str in pids.lines() {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    tracing::info!(pid, port, "found stale process on sidecar port; killing");
                    // We use SIGKILL here because if a process is still
                    // listening and hasn't responded to the app's own
                    // previous shutdown attempts, it's likely wedged.
                    let _ = StdCommand::new("kill").arg("-9").arg(pid.to_string()).status();
                }
            }
        }
    }
}

fn exe_suffix() -> &'static str {
    if cfg!(windows) { ".exe" } else { "" }
}

/// Drain the sidecar's stdout/stderr into the host tracing layer so
/// the engine's logs surface in the desktop's debug output.  Unlike
/// the pre-fix shape, this does NOT consume the [`Child`] — it only
/// takes the line readers.  The Child stays in [`SidecarHandle`] so
/// [`shutdown`] can drive a graceful stop.
fn spawn_log_forwarders(child: &mut Child, binary: &PathBuf) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let label = binary.display().to_string();

    if let Some(out) = child.stdout.take() {
        let label = label.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target = "sidecar", source = %label, "{line}");
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let label = label.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target = "sidecar", source = %label, "{line}");
            }
        });
    }
}
