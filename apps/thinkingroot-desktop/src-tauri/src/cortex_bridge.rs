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
            // Version skew check — if the running daemon's binary is
            // an older build than the one this desktop ships with, its
            // request handlers may have bugs that have been fixed in
            // the bundled binary. Attaching to it would let the
            // desktop's mount call hit a stale handler that returns
            // an empty body or panics mid-response (the production
            // failure mode we're closing here). We treat a version
            // mismatch as "stale" — same blast radius as a stale
            // lockfile — and let the caller respawn.
            match daemon_version(&lock.host, lock.port).await {
                Ok(running) if running == EXPECTED_DAEMON_VERSION => {
                    tracing::debug!(
                        pid = lock.pid,
                        port = lock.port,
                        started_by = lock.started_by.as_str(),
                        version = running.as_str(),
                        "cortex_bridge: attached to existing daemon"
                    );
                    return Ok(EngineConnection::Remote {
                        host: lock.host,
                        port: lock.port,
                        started_by: lock.started_by,
                        pid: lock.pid,
                    });
                }
                Ok(running) => {
                    tracing::warn!(
                        pid = lock.pid,
                        port = lock.port,
                        running_version = running.as_str(),
                        bundled_version = EXPECTED_DAEMON_VERSION,
                        "cortex_bridge: daemon version skew — caller should respawn"
                    );
                    cortex::remove_lock()?;
                }
                Err(e) => {
                    // /api/v1/version is only present on builds that
                    // include this fix. A 404 means the daemon is
                    // older than this desktop; treat as stale.
                    tracing::warn!(
                        pid = lock.pid,
                        port = lock.port,
                        error = %e,
                        "cortex_bridge: daemon does not expose /api/v1/version — treating as stale"
                    );
                    cortex::remove_lock()?;
                }
            }
        } else {
            tracing::info!(
                pid = lock.pid,
                port = lock.port,
                "cortex_bridge: stale lock detected, removing"
            );
            cortex::remove_lock()?;
        }
    }

    // No daemon. Caller (the sidecar manager) decides what to do.
    Ok(EngineConnection::InProcess)
}

/// Bundled-binary version. Matched against the running daemon's
/// `/api/v1/version` response to detect stale-binary skew.
const EXPECTED_DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// GET `<host>:<port>/api/v1/version` and parse the `data.version`
/// field from the daemon's standard `{ ok, data, error }` envelope.
async fn daemon_version(host: &str, port: u16) -> Result<String, String> {
    let url = format!("http://{host}:{port}/api/v1/version");
    let client = reqwest::Client::builder()
        .timeout(HEALTH_CHECK_TIMEOUT)
        .build()
        .map_err(|e| format!("client build: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let envelope: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    envelope
        .pointer("/data/version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("no .data.version in response from {url}"))
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
