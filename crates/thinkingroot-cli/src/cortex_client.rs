//! CLI-side async wrapper for the Cortex Protocol.
//!
//! The sync types and lockfile I/O live in
//! `thinkingroot_core::cortex` so that crate stays free of `tokio` and
//! `reqwest` dependencies (it ships in lightweight consumers like
//! `tr-format` + `tr-verify` that have no business owning an async
//! runtime). This module composes them with HTTP `/livez` probing
//! and detached-daemon spawning to deliver `resolve_engine()` —
//! the universal entry point every stateful CLI subcommand calls
//! before opening CozoDB.
//!
//! Spec: `docs/2026-05-02-unified-singleton-runtime.md` §4.3, §4.4.

use std::path::{Path, PathBuf};
use std::time::Duration;

use thinkingroot_core::cortex::{
    self, CortexError, CortexLock, Decision, DecisionInputs, EngineConnection, EngineIntent,
    ProbeResult, StartedBy,
};
use thinkingroot_core::install_manifest::InstallManifest;

/// How long `health_check` waits for `/livez`. Short enough to feel
/// instant on the happy path; long enough to survive a momentary
/// scheduler hiccup on a busy machine.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum time we wait for a freshly-spawned daemon to bind its
/// listener and start serving `/livez`. 30 s covers a cold-start
/// fastembed-model download + workspace registry mount on a slow
/// machine; on a warm machine the actual time is 1–2 s.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors specific to the async cortex resolution layer. Wraps the
/// sync `CortexError` and adds spawn-side failures.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Cortex(#[from] CortexError),

    #[error("failed to spawn daemon: {0}")]
    Spawn(#[source] std::io::Error),

    #[error(
        "daemon spawned but did not become ready within {timeout:?}: \
         check ~/.thinkingroot/serve.log"
    )]
    DaemonStartTimeout { timeout: Duration },
}

/// Resolve the engine connection for the given intent. This is the
/// universal entry point — every stateful CLI subcommand calls it
/// before opening CozoDB.
///
/// Implementation: gathers the sync inputs `decide()` needs
/// (`cortex.lock`, PID liveness, install-manifest preferred binary)
/// plus one async `/livez` probe via the shared
/// `thinkingroot_cortex_async::probe_livez`, then maps the pure
/// `Decision` to an `EngineConnection`. The decision policy itself
/// lives in `thinkingroot_core::cortex::decide` so CLI + desktop
/// agree byte-for-byte about whether to attach or spawn.
///
/// Returns:
/// - `Remote { .. }` — attach to an already-running daemon (either
///   pre-existing or freshly spawned by this call).
/// - `InProcess` — caller is `root serve` and no daemon is running.
///   Caller should bind the listener and call `cortex::write_lock`.
/// - `Stdio` — caller is `root serve --mcp-stdio`; bypass cortex.
/// - `RepairNeeded { .. }` — install-time prerequisites are missing
///   (e.g. no install-manifest entry; the binary that registered
///   itself has since vanished). Caller surfaces the failing check
///   ids — see `try_resolve_remote` in `main.rs`.
///
/// `SpawnRequired` is NOT returned from the CLI's `resolve_engine`
/// — that variant is reserved for desktop, which owns its child
/// handle. The CLI performs the detached spawn itself and returns
/// `Remote` after `/livez` comes green.
pub async fn resolve_engine(intent: EngineIntent) -> Result<EngineConnection, ResolveError> {
    // 1. Read the lock + check PID liveness synchronously.
    let lock = cortex::read_lock()?;
    let lock_pid_alive = lock
        .as_ref()
        .map(|l| cortex::process_alive(l.pid))
        .unwrap_or(false);

    // 2. Probe /livez only when there's a live-PID lock to probe.
    //    No lock → no probe; dead PID → no probe (the daemon is
    //    already known dead, no network round-trip needed).
    let probe_result = match (&lock, lock_pid_alive) {
        (Some(l), true) => {
            thinkingroot_cortex_async::probe_livez(&l.host, l.port, HEALTH_CHECK_TIMEOUT).await
        }
        _ => ProbeResult::NotProbed,
    };

    // 3. Resolve the preferred install-manifest binary, if any. A
    //    missing / corrupt / empty manifest yields None — decide()
    //    then surfaces RepairNeeded for spawn intents.
    let manifest_preferred_binary = load_preferred_manifest_binary();

    // 4. Run the pure decision. CLI never threads --in-process
    //    through here — main.rs short-circuits to the in-process
    //    path before calling resolve_engine.
    let decision = cortex::decide(DecisionInputs {
        intent,
        lock: lock.clone(),
        lock_pid_alive,
        probe_result,
        manifest_preferred_binary,
        in_process_flag: false,
    });

    // 5. Map the decision → connection. Side effects (stale-lock
    //    cleanup, detached spawn, /livez wait) live here, not in
    //    decide().
    match decision {
        Decision::Attach { host, port, version: _ } => {
            // decide() only returns Attach when lock + lock_pid_alive
            // + Healthy/Degraded probe all held — lock is therefore
            // present; reuse its started_by + pid so we don't lose
            // provenance info that isn't on Decision.
            let lock = lock.expect("decide() Attach implies lock present");
            tracing::debug!(
                pid = lock.pid,
                port = lock.port,
                started_by = lock.started_by.as_str(),
                "cortex: attached to existing daemon"
            );
            Ok(EngineConnection::Remote {
                host,
                port,
                started_by: lock.started_by,
                pid: lock.pid,
            })
        }
        Decision::Spawn { binary_path, port, host } => {
            // Before spawning, clean up any stale lock the decision
            // tree saw (dead-PID lock, or Unhealthy probe). Without
            // the cleanup the freshly-spawned daemon's bind would
            // race against the stale lockfile.
            if let Some(stale) = &lock {
                tracing::info!(
                    pid = stale.pid,
                    port = stale.port,
                    "cortex: removing stale lock before spawn"
                );
                cortex::remove_lock()?;
            }
            spawn_detached_daemon(&binary_path, &host, port).await?;
            wait_for_livez(&host, port, DAEMON_START_TIMEOUT).await?;
            // Re-read the lock the freshly-spawned daemon just
            // wrote on bind — that's where started_by + pid come
            // from for the caller.
            let new_lock = cortex::read_lock()?.ok_or_else(|| {
                ResolveError::Cortex(CortexError::Io(std::io::Error::other(
                    "daemon bound /livez but cortex.lock not present",
                )))
            })?;
            Ok(EngineConnection::Remote {
                host: new_lock.host,
                port: new_lock.port,
                started_by: new_lock.started_by,
                pid: new_lock.pid,
            })
        }
        Decision::InProcess => Ok(EngineConnection::InProcess),
        Decision::Stdio => Ok(EngineConnection::Stdio),
        Decision::RepairNeeded { failing_check_ids } => {
            Ok(EngineConnection::RepairNeeded { failing_check_ids })
        }
    }
}

/// Resolve the install-manifest's preferred binary path, returning
/// `None` when the manifest is absent, corrupt, has no preferred
/// entry, or that entry's binary has vanished from disk. The pure
/// `decide()` then surfaces `RepairNeeded` for spawn intents, which
/// is the user-visible signal "run `root doctor`".
fn load_preferred_manifest_binary() -> Option<PathBuf> {
    match InstallManifest::load() {
        Ok(Some(manifest)) => {
            let preferred_id = manifest.preferred?;
            manifest
                .binaries
                .into_iter()
                .find(|e| e.id == preferred_id)
                .filter(|e| e.path.exists())
                .map(|e| e.path)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "cortex: install manifest unreadable; treating as absent");
            None
        }
    }
}

/// HTTP GET `<host>:<port>/livez` with a 1s timeout. Returns `true`
/// on a 2xx response; any error (timeout, refused, non-2xx) returns
/// `false`. Never panics; `reqwest` builder failures fall through to
/// `false` so a wedged TLS root store can't crash the CLI.
pub async fn health_check(host: &str, port: u16) -> bool {
    let url = format!("http://{host}:{port}{}", cortex::LIVENESS_PATH);
    let client = match reqwest::Client::builder()
        .timeout(HEALTH_CHECK_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cortex: reqwest client build failed");
            return false;
        }
    };
    match client.get(&url).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Spawn `root serve` in a detached process group from the given
/// binary path. The child outlives the spawning shell; stdout/stderr
/// redirect to `<config_dir>/thinkingroot/serve.log`.
///
/// Used only by `resolve_engine` when `decide()` returns
/// `Decision::Spawn` — the binary path comes from the install
/// manifest (via `load_preferred_manifest_binary`), not from
/// `current_exe()`, so a CLI invocation from a one-off path doesn't
/// inadvertently spawn an unregistered binary. Pre-`resolve_engine`
/// paths should never call this directly — `resolve_engine` owns
/// the lock-then-spawn race coordination.
pub async fn spawn_detached_daemon(
    binary_path: &Path,
    host: &str,
    port: u16,
) -> Result<(), ResolveError> {
    // Append-mode log file under the same dir as cortex.lock. mode
    // 0o600 on Unix because the log can contain credential errors.
    let log_path = serve_log_path()?;
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(ResolveError::Spawn)?;
    }
    let log_file = open_append_secure(&log_path).map_err(ResolveError::Spawn)?;
    let log_clone = log_file.try_clone().map_err(ResolveError::Spawn)?;

    let mut cmd = tokio::process::Command::new(binary_path);
    cmd.arg("serve")
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_clone));

    // Detach: child goes into its own process group so closing the
    // spawning shell does not deliver SIGHUP. The kill_on_drop bit
    // is intentionally OFF — we WANT the child to outlive this CLI
    // invocation.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }

    // Spawn and IMMEDIATELY drop the Child so the parent has no
    // handle keeping it alive — the OS-level detachment is what
    // matters; the tokio Child is just a dropping convenience for
    // attached children (which we explicitly don't want here).
    let child = cmd.spawn().map_err(ResolveError::Spawn)?;
    let pid = child.id();
    tracing::info!(
        pid,
        binary = %binary_path.display(),
        host,
        port,
        log = %log_path.display(),
        "cortex: spawned detached daemon"
    );

    // Don't await the child — that would block until it exits. Just
    // drop the handle so kill_on_drop (which we did NOT set) cannot
    // fire on the child. This is the only safe pattern for a
    // detached spawn.
    drop(child);

    Ok(())
}

/// Poll `/livez` until the daemon is ready or the timeout elapses.
/// Used after `spawn_detached_daemon` to gate the return of
/// `resolve_engine` on actual readiness, not just "process forked".
async fn wait_for_livez(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<(), ResolveError> {
    let deadline = tokio::time::Instant::now() + timeout;
    // Exponential backoff: 100 ms, 200 ms, 400 ms, 500 ms (capped).
    let mut delay = Duration::from_millis(100);
    let max_delay = Duration::from_millis(500);

    loop {
        if health_check(host, port).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ResolveError::DaemonStartTimeout { timeout });
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    }
}

/// Path of the daemon's log file. Co-located with `cortex.lock` so
/// `~/.thinkingroot/` is the single discoverable directory for
/// "what is my engine doing right now".
fn serve_log_path() -> Result<PathBuf, ResolveError> {
    let lock = cortex::lock_path()?;
    let parent = lock
        .parent()
        .expect("cortex.lock_path always has a thinkingroot/ parent")
        .to_path_buf();
    Ok(parent.join("serve.log"))
}

/// Open a log file in append mode. On Unix the file is created with
/// mode 0o600 so credential errors written to the log are not
/// world-readable. On Windows the default ACL is used (the log lives
/// under `%APPDATA%` which is per-user already).
fn open_append_secure(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Convenience: build a `CortexLock` for `started_by = Cli` using
/// the current binary's metadata. Called by `serve.rs` after a
/// successful bind.
pub fn build_cli_lock(port: u16) -> CortexLock {
    let binary_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("root"));
    CortexLock::new(
        port,
        StartedBy::Cli,
        env!("CARGO_PKG_VERSION"),
        binary_path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_check_fails_for_closed_port() {
        // Port 1 is reserved and never bound on a normal system;
        // health_check must return false rather than blocking.
        let alive = health_check("127.0.0.1", 1).await;
        assert!(!alive);
    }

    #[tokio::test]
    async fn health_check_returns_quickly_when_unreachable() {
        let start = std::time::Instant::now();
        let _ = health_check("127.0.0.1", 1).await;
        let elapsed = start.elapsed();
        // 1s timeout + reqwest internal overhead ≤ ~1.5s on a
        // slow machine. The contract: we never block longer than
        // the configured HEALTH_CHECK_TIMEOUT plus a small slop.
        assert!(
            elapsed < Duration::from_secs(3),
            "health_check took {elapsed:?}, expected < 3s"
        );
    }

    #[test]
    fn build_cli_lock_uses_cli_provenance() {
        let lock = build_cli_lock(31760);
        assert_eq!(lock.started_by, StartedBy::Cli);
        assert_eq!(lock.port, 31760);
        assert_eq!(lock.pid, std::process::id());
    }

    #[tokio::test]
    async fn resolve_with_mcp_stdio_returns_stdio() {
        let conn = resolve_engine(EngineIntent::McpStdio).await.unwrap();
        assert!(matches!(conn, EngineConnection::Stdio));
    }
}
