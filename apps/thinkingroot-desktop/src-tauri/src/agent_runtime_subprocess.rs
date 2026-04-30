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
            for (k, v) in creds.as_env_map() {
                // Process env wins. Only seed values the desktop process
                // didn't already inherit.
                if std::env::var(&k).ok().filter(|s| !s.is_empty()).is_none() {
                    cmd.env(&k, &v);
                    count += 1;
                }
            }
            if count > 0 {
                tracing::info!(injected = count, "seeded sidecar with credentials from credentials.toml");
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

    let state = app.state::<AppState>();
    let mut guard = state.sidecar.lock().await;
    *guard = Some(SidecarHandle {
        port,
        host: HOST.to_string(),
        pid,
        child: std::sync::Arc::new(tokio::sync::Mutex::new(Some(child))),
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
        let triple = std::env::var("TARGET").ok();
        let candidate = match triple {
            Some(t) => resource_dir
                .join("binaries")
                .join(format!("thinkingroot-agent-runtime-{t}{}", exe_suffix())),
            None => resource_dir
                .join("binaries")
                .join(format!("thinkingroot-agent-runtime{}", exe_suffix())),
        };
        if candidate.exists() {
            return Some(candidate);
        }
    }

    which_root()
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
