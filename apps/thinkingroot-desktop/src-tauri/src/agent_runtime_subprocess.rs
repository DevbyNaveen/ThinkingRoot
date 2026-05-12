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

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tauri::{AppHandle, Emitter, Manager, Runtime};
use thinkingroot_core::cortex::{self, CortexLock, EngineConnection, EngineIntent, StartedBy};
use tokio::process::{Child, Command};

use crate::cortex_bridge;
use crate::state::{AppState, SidecarHandle};

/// Default loopback port for the local sidecar. Chosen to avoid
/// the engine's 3000 default (collides with common dev tools) and
/// the cloud's 3100-grid; settable via env for tests.
const DEFAULT_PORT: u16 = 31760;
const HOST: &str = "127.0.0.1";

/// Spawn the sidecar — or attach to an already-running one.
///
/// **Cortex Protocol contract.** Before spawning, this function calls
/// `cortex_bridge::resolve_engine(EngineIntent::DesktopBoot)`. If a
/// healthy daemon already exists (started by the CLI, by `launchd`,
/// or by another desktop instance), we install a `SidecarHandle`
/// with `child = None` and return — we did NOT spawn this process,
/// so `shutdown()` must NOT kill it.
///
/// Errors are logged but not bubbled — the desktop must keep
/// running even if the engine binary is unavailable on this machine.
pub async fn spawn<R: Runtime>(app: &AppHandle<R>) {
    let port = sidecar_port();

    // ── Cortex attach path: dispatch on the bridge's decision ─────
    // Skipped silently when DESKTOP_SIDECAR_PORT is overridden to
    // something other than the cortex canonical 31760 (test
    // isolation case) — there the legacy resolve_binary() path runs
    // unconditionally so tests can drive the spawn directly.
    if port == cortex::DEFAULT_PORT {
        match cortex_bridge::resolve_engine(EngineIntent::DesktopBoot).await {
            Ok(EngineConnection::Remote {
                host,
                port: lock_port,
                started_by,
                pid,
            }) => {
                tracing::info!(
                    pid,
                    port = lock_port,
                    started_by = started_by.as_str(),
                    "cortex: existing engine found — desktop attaching as thin client"
                );
                let state = app.state::<AppState>();
                let mut guard = state.sidecar.lock().await;
                *guard = Some(SidecarHandle {
                    host,
                    port: lock_port,
                    pid: Some(pid),
                    // `child = None` means "we did not spawn this
                    // process" — `shutdown()` checks this and refuses
                    // to kill what it doesn't own.
                    child: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                });
                return;
            }
            Ok(EngineConnection::SpawnRequired {
                binary_path,
                port: spawn_port,
                host: _,
            }) => {
                // Manifest-resolved binary (T6 wired this through
                // `cortex_bridge::resolve_engine`). Spawn it attached
                // and own the Child handle for graceful shutdown.
                tracing::info!(
                    ?binary_path,
                    port = spawn_port,
                    "cortex: SpawnRequired — spawning manifest-resolved binary attached"
                );
                spawn_attached(app, &binary_path, spawn_port).await;
                return;
            }
            Ok(EngineConnection::InProcess) => {
                // Desktop's `cortex_bridge::resolve_engine` returns
                // InProcess only when there's no install manifest at
                // all AND no live daemon — i.e. a dev build running
                // from `cargo tauri dev` before any manifest entries
                // have been registered. The engine surfaces will use
                // the legacy in-process compile path (workspace_compile
                // falls back to that when sidecar = None).
                tracing::info!(
                    "cortex: InProcess — no daemon to attach, no manifest binary; \
                     engine surfaces will use in-process compile"
                );
                return;
            }
            Ok(EngineConnection::Stdio) => {
                // Cannot happen for DesktopBoot intent (only the
                // CLI's `root serve --mcp-stdio` yields Stdio). Log
                // honestly and return — there is no engine to wire.
                tracing::warn!(
                    "cortex: Stdio returned for DesktopBoot (unexpected) — \
                     engine surfaces disabled"
                );
                return;
            }
            Ok(EngineConnection::RepairNeeded { failing_check_ids }) => {
                // Install manifest is missing or broken. Do NOT silently
                // fall back to the legacy resolve_binary() path — that's
                // exactly the bug Slice C is killing. Emit the structured
                // event so Slice D's blocking-panel UI can render the
                // failing checks, then return without spawning.
                tracing::error!(
                    ?failing_check_ids,
                    "cortex: engine cannot start — install manifest missing or broken; \
                     emitting engine_status_changed for Slice D UI"
                );
                let _ = app.emit(
                    "engine_status_changed",
                    serde_json::json!({
                        "status": "repair_needed",
                        "failing_check_ids": failing_check_ids,
                    }),
                );
                return;
            }
            Err(e) => {
                // Lockfile I/O failure or other bridge-internal error.
                // Fall through to the legacy resolve_binary() path as
                // a safety net so a half-broken state directory doesn't
                // brick the desktop. Slice F revisits whether this
                // fallback should also flip to RepairNeeded.
                tracing::warn!(
                    error = %e,
                    "cortex_bridge::resolve_engine failed — proceeding with legacy spawn fallback"
                );
            }
        }
    } else {
        tracing::debug!(
            port,
            "non-default sidecar port; skipping cortex attach (test isolation mode)"
        );
    }

    // ── Legacy fallback spawn path ────────────────────────────────
    // Reached when either:
    //   * the sidecar port is overridden (test isolation), or
    //   * cortex_bridge::resolve_engine returned Err(_).
    // Resolves the binary via the pre-cortex search order
    // (env var → bundled resource_dir → PATH) and hands off to the
    // same attached-spawn helper that the SpawnRequired path uses.
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
    spawn_attached(app, &binary, port).await;
}

/// Spawn `binary` as `root serve --host 127.0.0.1 --port <port>`
/// attached to this process: stdin null, stdout/stderr piped (forwarded
/// to tracing), `kill_on_drop(true)`. Waits up to 120s for the
/// `/livez` endpoint to come up, stores the Child handle in
/// `AppState::sidecar`, writes the cortex.lock when on the canonical
/// port, and parks a watchdog that clears the handle if the child
/// exits unexpectedly.
///
/// Shared between the cortex `SpawnRequired` path (manifest-resolved
/// binary) and the legacy fallback (`resolve_binary()` output). The
/// only behavioural difference between the two callers is which
/// `binary` path arrives; everything from `cmd.spawn()` onward is
/// identical so the lifecycle invariants (shutdown ownership,
/// watchdog cleanup, lockfile write) hold in both.
async fn spawn_attached<R: Runtime>(app: &AppHandle<R>, binary: &Path, port: u16) {
    tracing::info!(
        binary = %binary.display(),
        host = HOST,
        port,
        "spawning ThinkingRoot sidecar (no existing daemon found)",
    );

    // Last-resort port cleanup. The cortex check above caught any
    // ThinkingRoot-managed daemon; this kills only non-cortex
    // processes (e.g. an unrelated dev server that grabbed 31760).
    // `cleanup_stale_sidecar` is now a narrow safety net rather
    // than the primary mechanism.
    cleanup_stale_sidecar(port).await;

    let mut cmd = Command::new(binary);
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
            tracing::debug!(
                ?e,
                "credentials.toml unreadable; sidecar inherits desktop env only"
            );
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
    let binary_buf = binary.to_path_buf();
    spawn_log_forwarders(&mut child, &binary_buf);

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

    // ── Cortex Protocol: write the lockfile so a subsequent CLI
    // invocation discovers and attaches to this daemon instead of
    // racing it for `graph.db`. Only relevant on the canonical port
    // (test isolation skips this).
    if port == cortex::DEFAULT_PORT
        && let Some(child_pid) = pid
    {
        let lock = CortexLock::new(
            port,
            StartedBy::Desktop,
            env!("CARGO_PKG_VERSION"),
            binary_buf.clone(),
        );
        // The lock's `pid` defaults to the desktop's own PID via
        // `CortexLock::new`, but we want the sidecar's PID so the
        // CLI's process_alive check sees the engine, not the GUI.
        let mut lock = lock;
        lock.pid = child_pid;
        if let Err(e) = cortex::write_lock(&lock) {
            tracing::warn!(error = %e, "failed to write cortex.lock; CLI clients may double-spawn");
        }
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
        // Cortex attach mode: this desktop did not spawn the daemon
        // (it attached to one started by the CLI or `launchd`). Do
        // NOT kill it — that would orphan the CLI users it's still
        // serving. Leave the cortex.lock alone too: whoever spawned
        // the daemon owns its lifecycle.
        tracing::info!("cortex attach mode — daemon not owned by this desktop, leaving it running");
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
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
        }
    }

    // ── Cortex Protocol: this desktop OWNED the sidecar (we hit the
    // child.take()-Some branch), so the cortex.lock we wrote on
    // spawn is now stale. Clean it up. A failure here is logged
    // but not bubbled — the next caller will treat the lock as
    // stale anyway because the PID inside it is now dead.
    if let Err(e) = cortex::remove_lock() {
        tracing::warn!(error = %e, "failed to remove cortex.lock on desktop shutdown");
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
async fn wait_for_sidecar_ready(livez_url: &str, child: &mut Child, timeout_secs: u64) -> bool {
    use tokio::time::{Duration, Instant, sleep};

    // Pre-fix: `.build().unwrap_or_default()` would silently fall back to
    // a default reqwest client with NO request timeout if builder
    // construction failed (e.g. corrupted system root-cert store on a
    // freshly-imaged box, broken DNS resolver init).  The deadline-based
    // poll loop would then call `client.get(livez_url).send()` on a
    // timeout-less client and could hang forever, never reaching the
    // outer `Instant::now() >= deadline` check.  Treat client-init
    // failure as a hard "sidecar not ready" — same contract as a child
    // crash or timeout, just with a clearer log line so the user knows
    // *why* startup failed.
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                url = %livez_url,
                error = %e,
                "sidecar readiness probe: failed to build reqwest client — \
                 aborting (likely corrupted system root-cert store or \
                 invalid system clock)"
            );
            return false;
        }
    };

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
        let pids = stale_pids_on_port(port);
        if pids.is_empty() {
            return;
        }
        use std::process::Command as StdCommand;
        for pid in pids {
            tracing::info!(pid, port, "found stale process on sidecar port; killing");
            // SIGKILL because if a process is still listening and
            // hasn't responded to prior shutdown attempts, it's
            // likely wedged.
            let _ = StdCommand::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .status();
        }
    }
}

/// Find PIDs holding a TCP listener on `port`.  Tries multiple
/// strategies because `lsof` is not always installed on minimal
/// Linux base images (Alpine, distroless, some CI runners) — pre-fix
/// the cleanup silently no-op'd on those systems and the desktop
/// would refuse to spawn its sidecar with a confusing "address in
/// use" error after a crash.
///
/// Order:
///   1. `lsof -t -i tcp:<port> -sTCP:LISTEN` (works on macOS + most Linux)
///   2. `ss -tlnp` (`ss` ships with iproute2, present on virtually every
///      modern Linux including Alpine + busybox)
///   3. `/proc/net/tcp` parse (Linux-only, always present, last resort)
///
/// Returns an empty Vec when no strategy worked OR no process holds
/// the port.
#[cfg(unix)]
fn stale_pids_on_port(port: u16) -> Vec<u32> {
    use std::process::Command as StdCommand;

    // Strategy 1: lsof.
    if let Ok(out) = StdCommand::new("lsof")
        .arg("-t")
        .arg("-i")
        .arg(format!("tcp:{}", port))
        .arg("-sTCP:LISTEN")
        .output()
        && out.status.success()
    {
        let pids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|s| s.trim().parse::<u32>().ok())
            .collect();
        // `lsof` exits success with no output when nothing matches —
        // an empty list here means "lsof ran but found nothing", not
        // "lsof failed".  Either way return early.
        return pids;
    }

    // Strategy 2: `ss -tlnp`.  Output line shape:
    //   LISTEN 0 128 0.0.0.0:8080 0.0.0.0:* users:(("root",pid=1234,fd=3))
    if let Ok(out) = StdCommand::new("ss").arg("-tlnp").output()
        && out.status.success()
    {
        let needle_v4 = format!(":{}", port);
        let mut pids = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            // Only consider listening lines that bind to our port.
            // `ss` prints the address as `0.0.0.0:<port>` or
            // `[::]:<port>`; the substring suffix check is sufficient
            // because the field is whitespace-bounded.
            let has_port = line.split_whitespace().any(|tok| {
                tok.ends_with(&needle_v4) && tok.rsplit(':').next() == Some(&port.to_string())
            });
            if !has_port {
                continue;
            }
            // Extract pid=NNN from the users:(...) field.
            for chunk in line.split(',') {
                if let Some(rest) = chunk.trim().strip_prefix("pid=")
                    && let Some(pid_str) = rest.split(|c: char| !c.is_ascii_digit()).next()
                    && let Ok(pid) = pid_str.parse::<u32>()
                {
                    pids.push(pid);
                }
            }
        }
        if !pids.is_empty() {
            return pids;
        }
        // ss ran but found no PIDs on our port — return empty (the
        // port is genuinely free, or `ss` doesn't have CAP_NET_ADMIN
        // and can't see other-user PIDs).  Either way, no kill targets.
        return Vec::new();
    }

    // Strategy 3 (Linux only): /proc/net/tcp lookup.  Each line ends
    // with the `inode` field; we then iterate /proc/*/fd/* symlinks
    // looking for `socket:[<inode>]`.  This is the same approach
    // procps uses internally and works without any extra binaries.
    #[cfg(target_os = "linux")]
    {
        if let Some(inode) = listening_inode_for_port(port) {
            return pids_owning_socket_inode(inode);
        }
    }

    Vec::new()
}

#[cfg(target_os = "linux")]
fn listening_inode_for_port(port: u16) -> Option<u64> {
    // /proc/net/tcp format: per RFC, the local-address column is
    // `<hex_ip>:<hex_port>`, state column is `0A` for LISTEN.
    let raw = std::fs::read_to_string("/proc/net/tcp").ok()?;
    let needle_port = format!(":{:04X}", port);
    for line in raw.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let _sl = cols.next()?;
        let local = cols.next()?;
        let _remote = cols.next()?;
        let state = cols.next()?;
        if state != "0A" {
            continue;
        }
        if !local.ends_with(&needle_port) {
            continue;
        }
        // Skip uid, timer, retr, expire, then the inode column.
        let _uid = cols.next()?;
        let _timer = cols.next()?;
        let _retr = cols.next()?;
        let _expire = cols.next()?;
        let inode: u64 = cols.next()?.parse().ok()?;
        return Some(inode);
    }
    None
}

#[cfg(target_os = "linux")]
fn pids_owning_socket_inode(inode: u64) -> Vec<u32> {
    let needle = format!("socket:[{}]", inode);
    let mut pids = Vec::new();
    let Ok(read_dir) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in read_dir.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd_entry in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd_entry.path())
                && target.to_string_lossy() == needle
            {
                pids.push(pid);
                break;
            }
        }
    }
    pids
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
