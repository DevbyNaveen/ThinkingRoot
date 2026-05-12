//! Desktop-side bridge for the Cortex Protocol.
//!
//! Mirrors the small async surface that `crates/thinkingroot-cli/src/
//! cortex_client.rs` exposes — `resolve_engine` and `health_check` —
//! but does NOT auto-spawn a daemon when the intent is `DesktopBoot`.
//! The desktop has its own sidecar manager
//! (`agent_runtime_subprocess.rs::spawn`) that wants to retain the
//! `tokio::process::Child` handle for graceful-shutdown control;
//! delegating the spawn to a CLI-style "fire and forget detached"
//! helper would lose that handle.
//!
//! The shared sync types and lockfile I/O come from
//! `thinkingroot_core::cortex` so this module stays the smallest
//! possible bridge. The async `/livez` probe lives in
//! `thinkingroot_cortex_async` so CLI + desktop share one probe
//! implementation byte-for-byte.

use std::path::PathBuf;
use std::time::Duration;

use thinkingroot_core::cortex::{
    self as cortex_core, CortexError, Decision, DecisionInputs, EngineConnection, EngineIntent,
};
use thinkingroot_core::install_manifest::InstallManifest;

/// 1s health-check timeout — same as the CLI side. Defined here
/// rather than imported because the CLI module isn't on the desktop
/// crate's path-dep graph (and shouldn't be — the CLI is a binary
/// crate, not a library).
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

/// Bundled-binary version. Compared against the running daemon's
/// reported version (carried on `Decision::Attach`) so the desktop
/// refuses to attach to a stale daemon whose handlers may have bugs
/// that have been fixed in the bundled binary.
const EXPECTED_DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Bridge errors. Wraps `CortexError` for I/O failures (filesystem,
/// lockfile parse) — the in-band decision outcomes (`SpawnRequired`,
/// `RepairNeeded`) are now expressed as `EngineConnection` variants
/// rather than errors, since they represent successful resolutions
/// that the caller must act on.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(transparent)]
    Cortex(#[from] CortexError),
}

/// Resolve the engine connection for the desktop's sidecar manager.
///
/// Mirrors `thinkingroot_cli::cortex_client::resolve_engine` in shape
/// — both gather sync inputs, run the pure `cortex::decide()`, and
/// map the resulting `Decision` to an `EngineConnection` — but
/// differs on `Decision::Spawn`: the desktop returns
/// `EngineConnection::SpawnRequired` so the caller
/// (`agent_runtime_subprocess::spawn`) can retain the resulting
/// `tokio::process::Child` handle for graceful-shutdown control. A
/// CLI-style fire-and-forget detached spawn would lose that handle.
///
/// Version-skew check stays here — the desktop is stricter than the
/// CLI about version match. If the daemon's reported version differs
/// from this desktop's bundled version, the function surfaces
/// `EngineConnection::RepairNeeded` with `daemon.version.match` as
/// the failing check id rather than attaching.
pub async fn resolve_engine(intent: EngineIntent) -> Result<EngineConnection, BridgeError> {
    // Dev escape hatch: THINKINGROOT_FORCE_IN_PROCESS=1 short-circuits
    // to InProcess without any filesystem or network probes. Mirrors
    // the CLI's `--in-process` global flag for the desktop dev loop
    // where `pnpm tauri dev` runs without a bundled sidecar.
    if std::env::var("THINKINGROOT_FORCE_IN_PROCESS").as_deref() == Ok("1") {
        tracing::info!("THINKINGROOT_FORCE_IN_PROCESS=1 — resolving as InProcess");
        return Ok(EngineConnection::InProcess);
    }

    // 1. Gather inputs synchronously.
    let lock = cortex_core::read_lock()?;
    let lock_pid_alive = lock
        .as_ref()
        .map(|l| cortex_core::process_alive(l.pid))
        .unwrap_or(false);

    // 2. Probe /livez only when there's a live-PID lock to probe.
    //    No lock → no probe; dead PID → no probe (the daemon is
    //    already known dead, no network round-trip needed).
    let probe_result = match (&lock, lock_pid_alive) {
        (Some(l), true) => {
            thinkingroot_cortex_async::probe_livez(&l.host, l.port, HEALTH_CHECK_TIMEOUT).await
        }
        _ => cortex_core::ProbeResult::NotProbed,
    };

    // 3. Resolve preferred binary from install manifest. Desktop is
    //    more forgiving than the CLI here — if the preferred entry's
    //    file is gone, fall back to the first extant binary so a
    //    half-broken install (preferred-pointer drift) still boots.
    let manifest_preferred_binary = load_preferred_or_extant_binary();

    // 4. Run the pure decision. Desktop never threads --in-process
    //    through here.
    let decision = cortex_core::decide(DecisionInputs {
        intent,
        lock: lock.clone(),
        lock_pid_alive,
        probe_result,
        manifest_preferred_binary,
        in_process_flag: false,
    });

    // 5. Map decision → connection. Side effects (version-skew
    //    rejection) live here, not in decide().
    Ok(match decision {
        Decision::Attach { host, port, version } => {
            // Version-skew check — stricter than CLI. The desktop
            // ships its bundle pinned to a specific engine version;
            // attaching to a stale daemon whose handlers may have
            // bugs fixed in the bundled binary would let the
            // desktop's mount call hit a stale handler that returns
            // an empty body or panics mid-response (the production
            // failure mode that motivated this check).
            if version != EXPECTED_DAEMON_VERSION {
                tracing::warn!(
                    daemon_version = %version,
                    expected_version = EXPECTED_DAEMON_VERSION,
                    "cortex_bridge: daemon version mismatch — desktop will not attach"
                );
                EngineConnection::RepairNeeded {
                    failing_check_ids: vec!["daemon.version.match".to_string()],
                }
            } else {
                let lock = lock.expect("decide() Attach implies lock present");
                tracing::debug!(
                    pid = lock.pid,
                    port = lock.port,
                    started_by = lock.started_by.as_str(),
                    version = version.as_str(),
                    "cortex_bridge: attached to existing daemon"
                );
                EngineConnection::Remote {
                    host,
                    port,
                    started_by: lock.started_by,
                    pid: lock.pid,
                }
            }
        }
        Decision::Spawn {
            binary_path,
            port,
            host,
        } => EngineConnection::SpawnRequired {
            binary_path,
            port,
            host,
        },
        Decision::InProcess => EngineConnection::InProcess,
        Decision::Stdio => EngineConnection::Stdio,
        Decision::RepairNeeded { failing_check_ids } => {
            EngineConnection::RepairNeeded { failing_check_ids }
        }
    })
}

/// Resolve the install-manifest's preferred binary path, falling back
/// to the first extant entry if the preferred pointer is stale (file
/// gone). Returns `None` when the manifest is absent / corrupt / has
/// no usable entries — `decide()` then surfaces `RepairNeeded` for
/// spawn intents, surfacing the user-visible "run `root doctor`"
/// signal in the blocking-panel UI (Slice D).
fn load_preferred_or_extant_binary() -> Option<PathBuf> {
    match InstallManifest::load() {
        Ok(Some(manifest)) => {
            // First try the preferred pointer, but only if the file
            // it names still exists on disk.
            let preferred = manifest
                .preferred
                .clone()
                .and_then(|id| manifest.binaries.iter().find(|e| e.id == id).cloned())
                .filter(|e| e.path.exists())
                .map(|e| e.path.clone());
            preferred.or_else(|| {
                manifest
                    .binaries
                    .into_iter()
                    .find(|e| e.path.exists())
                    .map(|e| e.path)
            })
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cortex_bridge: install manifest unreadable; treating as absent"
            );
            None
        }
    }
}

/// HTTP GET `<host>:<port>/livez` with the same 1s timeout the CLI
/// uses. Returns `true` on a 2xx response.
///
/// Kept for callers outside `resolve_engine` (e.g. ad-hoc health
/// pings from the sidecar manager). The cortex resolution path now
/// goes through `thinkingroot_cortex_async::probe_livez` instead.
#[allow(dead_code)]
pub async fn health_check(host: &str, port: u16) -> bool {
    let url = format!("http://{host}:{port}{}", cortex_core::LIVENESS_PATH);
    let client = match reqwest::Client::builder()
        .timeout(HEALTH_CHECK_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cortex_bridge: reqwest client build failed");
            return false;
        }
    };
    match client.get(&url).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_with_mcp_stdio_returns_stdio() {
        let conn = resolve_engine(EngineIntent::McpStdio).await.unwrap();
        assert!(matches!(conn, EngineConnection::Stdio));
    }

    #[tokio::test]
    async fn health_check_fails_for_closed_port() {
        // Port 1 is reserved.
        assert!(!health_check("127.0.0.1", 1).await);
    }
}
