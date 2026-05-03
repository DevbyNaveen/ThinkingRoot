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
//! possible bridge.

use std::time::Duration;

use thinkingroot_core::cortex::{self, CortexError, EngineConnection, EngineIntent};

/// 1s health-check timeout — same as the CLI side. Defined here
/// rather than imported because the CLI module isn't on the desktop
/// crate's path-dep graph (and shouldn't be — the CLI is a binary
/// crate, not a library).
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

/// Bridge errors. Wraps `CortexError` and adds the
/// "no-daemon-found-and-DesktopBoot" case so the caller (the sidecar
/// manager) can fall through to its spawn-and-keep-Child path.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(transparent)]
    Cortex(#[from] CortexError),
}

/// Resolve the engine connection for the desktop's sidecar manager.
///
/// Differs from the CLI's `resolve_engine` in one important way:
/// when `intent == DesktopBoot` and no daemon is running, this
/// returns `Ok(InProcess)` to signal "you should spawn one" rather
/// than auto-spawning. The desktop's sidecar manager handles the
/// spawn itself because it needs the resulting `Child` handle.
pub async fn resolve_engine(
    intent: EngineIntent,
) -> Result<EngineConnection, BridgeError> {
    if matches!(intent, EngineIntent::McpStdio) {
        return Ok(EngineConnection::Stdio);
    }

    if let Some(lock) = cortex::read_lock()? {
        if cortex::process_alive(lock.pid) && health_check(&lock.host, lock.port).await {
            tracing::debug!(
                pid = lock.pid,
                port = lock.port,
                started_by = lock.started_by.as_str(),
                "cortex_bridge: attached to existing daemon"
            );
            return Ok(EngineConnection::Remote {
                host: lock.host,
                port: lock.port,
                started_by: lock.started_by,
                pid: lock.pid,
            });
        }
        tracing::info!(
            pid = lock.pid,
            port = lock.port,
            "cortex_bridge: stale lock detected, removing"
        );
        cortex::remove_lock()?;
    }

    // No daemon. Caller (the sidecar manager) decides what to do.
    Ok(EngineConnection::InProcess)
}

/// HTTP GET `<host>:<port>/livez` with the same 1s timeout the CLI
/// uses. Returns `true` on a 2xx response.
pub async fn health_check(host: &str, port: u16) -> bool {
    let url = format!("http://{host}:{port}{}", cortex::LIVENESS_PATH);
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
