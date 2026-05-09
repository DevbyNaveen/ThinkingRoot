//! Stream A — typed HTTP client for the local `root serve` sidecar.
//!
//! Every Tauri command that needs the engine routes through this module
//! instead of opening a second in-process `QueryEngine`. That keeps the
//! daemon as the single owner of `graph.db` and matches the Cortex
//! Protocol invariant from `.claude/rules/cortex-protocol.md` —
//! *"CLI stateful commands ALSO go through the sidecar"* — for the
//! desktop too.
//!
//! Three responsibilities:
//!
//! 1. Resolve the sidecar's host/port from `AppState.sidecar` (waits
//!    briefly when the sidecar is still booting on first launch).
//! 2. Ensure the active workspace is registered with the daemon's engine
//!    via `POST /api/v1/workspaces` — idempotent, called from every
//!    command that reads or writes workspace state.
//! 3. Provide typed `get` / `post` / `delete` helpers that surface
//!    sidecar HTTP errors as `String` (matching every existing Tauri
//!    command's error type).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Serialize, de::DeserializeOwned};
use tauri::{AppHandle, Manager};
use thinkingroot_core::WorkspaceRegistry;

use crate::agent_runtime_subprocess;
use crate::cortex_bridge;
use crate::state::AppState;

/// Boot wait for the sidecar's `AppState.sidecar` slot to populate.
/// `agent_runtime_subprocess::spawn` polls `/livez` for up to 60 s; we
/// match that here so the desktop's first command on a cold start
/// doesn't fail before the sidecar finishes booting.
const SIDECAR_BOOT_MAX_ATTEMPTS: u32 = 120; // 120 * 500 ms = 60 s
const SIDECAR_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Per-request timeout for non-streaming HTTP calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One round-trip with the sidecar.
pub struct SidecarClient {
    pub host: String,
    pub port: u16,
    pub workspace: String,
    client: reqwest::Client,
}

impl SidecarClient {
    /// Resolve sidecar + active workspace, then ensure the daemon has
    /// the workspace mounted.  Every command-side entry point should
    /// start by calling this.
    pub async fn ensure_active(app: &AppHandle) -> Result<Self, String> {
        let (host, port) = resolve_sidecar(app).await?;
        let (workspace, root_path) = resolve_active_workspace()?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("build http client: {e}"))?;
        let me = Self {
            host,
            port,
            workspace,
            client,
        };
        me.ensure_workspace_mounted(&root_path).await?;
        Ok(me)
    }

    /// Variant used by branch/* commands that need workspace_root set
    /// on the daemon side but don't care about the workspace name.
    /// Equivalent to `ensure_active` today; kept as a separate entry
    /// for future divergence (e.g. cross-workspace branch operations).
    pub async fn ensure_active_for_branches(app: &AppHandle) -> Result<Self, String> {
        Self::ensure_active(app).await
    }

    /// Like [`Self::ensure_active`] but mounts a *named* workspace
    /// rather than whatever `WorkspaceRegistry::active_entry` picks.
    ///
    /// Used by the chat surface's `llm_health` pre-flight: when a
    /// workspace's substrate exists on disk (compiled badge says
    /// COMPILED) but the daemon hasn't loaded it into
    /// `engine.workspaces` yet, the daemon's `/llm/health` returns
    /// `mounted: false` and the chat banner falsely refuses to send.
    /// Calling this before the probe converges the two views — disk
    /// state and daemon state — without waiting for some other code
    /// path (Brain view, compile, etc.) to incidentally trigger a
    /// mount first.
    pub async fn ensure_workspace(
        app: &AppHandle,
        workspace_name: &str,
    ) -> Result<Self, String> {
        let (host, port) = resolve_sidecar(app).await?;
        let registry = WorkspaceRegistry::load()
            .map_err(|e| format!("load workspace registry: {e}"))?;
        let entry = registry
            .workspaces
            .iter()
            .find(|w| w.name == workspace_name)
            .ok_or_else(|| {
                format!(
                    "workspace '{workspace_name}' not in registry — run `root workspace add` or remove the stale entry"
                )
            })?;
        let root_path = entry.path.clone();
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("build http client: {e}"))?;
        let me = Self {
            host,
            port,
            workspace: workspace_name.to_string(),
            client,
        };
        me.ensure_workspace_mounted(&root_path).await?;
        Ok(me)
    }

    /// Idempotent — POST /api/v1/workspaces with the desktop's active
    /// workspace name + path. Daemon's `mount_workspace_handler` is a
    /// remount-overwrite so calling repeatedly is safe; it also pins
    /// `state.workspace_root` to this path (Stream A — see rest.rs)
    /// so branch operations target the right repo.
    ///
    /// Self-healing on transport failure: a single retry with a fresh
    /// connection is attempted when the first POST returns a transport
    /// error (TCP reset, half-open from a stale daemon, OS socket
    /// hiccup). On the second failure we surface a typed error rather
    /// than retry forever — silent retry loops are exactly the
    /// "everything looks fine but the user sees a spinner" failure
    /// mode the audit's `§3.5` gateway-fallback bug warned about.
    async fn ensure_workspace_mounted(&self, root_path: &Path) -> Result<(), String> {
        let url = format!("http://{}:{}/api/v1/workspaces", self.host, self.port);
        let body = serde_json::json!({
            "name": self.workspace,
            "root_path": root_path.display().to_string(),
        });

        let send_once = || async {
            self.client
                .post(&url)
                .json(&body)
                .send()
                .await
        };

        let resp = match send_once().await {
            Ok(r) => r,
            Err(first_err) => {
                tracing::warn!(
                    workspace = self.workspace.as_str(),
                    error = %first_err,
                    "mount workspace: first POST failed (transport), retrying once"
                );
                // Brief pause so an ongoing graceful-shutdown of a
                // stale daemon completes and the sidecar manager has a
                // chance to spawn the replacement before we retry.
                tokio::time::sleep(Duration::from_millis(750)).await;
                send_once().await.map_err(|second_err| {
                    format!(
                        "mount workspace request failed twice (first: {first_err}; retry: {second_err})"
                    )
                })?
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("mount workspace failed ({status}): {body}"));
        }
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", self.host, self.port)
    }

    /// GET <path>. Path is appended to base URL as-is; pass the leading
    /// slash. Decodes the daemon's `{ ok, data, error }` envelope and
    /// returns just the `data` field.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let url = self.url(path);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        decode_envelope(resp).await
    }

    /// POST <path> with JSON body. Same envelope decoding as `get`.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let url = self.url(path);
        let resp = self
            .client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        decode_envelope(resp).await
    }

    /// DELETE <path>. Same envelope decoding as `get`.
    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let url = self.url(path);
        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| format!("DELETE {url}: {e}"))?;
        decode_envelope(resp).await
    }

    /// GET with `X-TR-Session-Id`. AEP / engram routes require it.
    pub async fn get_with_session<T: DeserializeOwned>(
        &self,
        path: &str,
        session_id: &str,
    ) -> Result<T, String> {
        let url = self.url(path);
        let resp = self
            .client
            .get(&url)
            .header("X-TR-Session-Id", session_id)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        decode_envelope(resp).await
    }

    /// POST with `X-TR-Session-Id`.
    pub async fn post_with_session<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        session_id: &str,
        body: &B,
    ) -> Result<T, String> {
        let url = self.url(path);
        let resp = self
            .client
            .post(&url)
            .header("X-TR-Session-Id", session_id)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        decode_envelope(resp).await
    }

    /// DELETE with `X-TR-Session-Id`.
    pub async fn delete_with_session<T: DeserializeOwned>(
        &self,
        path: &str,
        session_id: &str,
    ) -> Result<T, String> {
        let url = self.url(path);
        let resp = self
            .client
            .delete(&url)
            .header("X-TR-Session-Id", session_id)
            .send()
            .await
            .map_err(|e| format!("DELETE {url}: {e}"))?;
        decode_envelope(resp).await
    }
}

/// Wait for the sidecar metadata slot to populate, returning host/port.
/// Mirrors the boot wait in `commands/workspaces.rs::workspace_compile`
/// — a fresh-launch desktop may issue a Brain probe before the sidecar
/// has finished mounting workspaces, and 60 s of polling is far better
/// than failing the user's first action with `sidecar not running`.
///
/// **Self-heal:** if we have a handle but `/livez` fails (stale port /
/// dead process — e.g. user killed `root serve` while the UI still
/// cached metadata), clear the slot, remove a stale `cortex.lock`, and
/// respawn via [`agent_runtime_subprocess::spawn`].
async fn resolve_sidecar(app: &AppHandle) -> Result<(String, u16), String> {
    let state = app.state::<AppState>();
    for _ in 0..SIDECAR_BOOT_MAX_ATTEMPTS {
        let mut stale_respawn = false;
        {
            let mut guard = state.sidecar.lock().await;
            if let Some(h) = guard.as_ref() {
                if sidecar_live(&h.host, h.port).await {
                    return Ok((h.host.clone(), h.port));
                }
                tracing::warn!(
                    host = %h.host,
                    port = h.port,
                    pid = ?h.pid,
                    "sidecar handle is stale (/livez failed) — clearing lock and respawning engine"
                );
                *guard = None;
                stale_respawn = true;
            }
        }
        if stale_respawn {
            if let Err(e) = thinkingroot_core::cortex::remove_lock() {
                tracing::debug!(error = %e, "remove_lock after stale sidecar (may be absent)");
            }
            agent_runtime_subprocess::spawn(app).await;
            tokio::time::sleep(Duration::from_millis(400)).await;
            continue;
        }
        tokio::time::sleep(SIDECAR_POLL_INTERVAL).await;
    }
    Err(
        "sidecar not running — ThinkingRoot Engine binary is unavailable. \
         Install via `cargo install thinkingroot-cli` and restart the desktop, \
         or set THINKINGROOT_ROOT_BINARY to a custom path."
            .to_string(),
    )
}

/// A few short retries — avoids flaking on a slow `/livez` right after boot.
async fn sidecar_live(host: &str, port: u16) -> bool {
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if cortex_bridge::health_check(host, port).await {
            return true;
        }
    }
    false
}

/// Lightweight read-only endpoint discovery for *status* commands
/// (the sidebar's "MCP TOOLS" panel, the chat banner's `llm_health`
/// pre-flight). Unlike [`resolve_sidecar`], this:
///
/// - never spawns a daemon (status commands shouldn't trigger a heavy
///   subprocess on a cold app launch — that's the job of the next
///   real workspace operation),
/// - returns quickly (≤ ~1 s worst case) so the status panel paints
///   without the user staring at a spinner,
/// - falls back to `cortex.lock` discovery when `state.sidecar` is
///   empty (a daemon started outside this desktop session — e.g. a
///   `root serve` from another terminal — should still surface
///   honestly), and
/// - **write-throughs** the discovered handle into `state.sidecar`
///   so subsequent calls hit the fast path.
///
/// Returns `None` when no daemon is reachable; callers MUST surface
/// the empty state honestly rather than fabricating data.
pub async fn try_resolve_endpoint(app: &AppHandle) -> Option<(String, u16)> {
    let state = app.state::<AppState>();

    // Fast path: an existing handle we trust.
    {
        let guard = state.sidecar.lock().await;
        if let Some(h) = guard.as_ref()
            && cortex_bridge::health_check(&h.host, h.port).await
        {
            return Some((h.host.clone(), h.port));
        }
    }

    // Fallback: cortex.lock left by some daemon (this desktop's prior
    // launch, a CLI `root serve`, a launchd-managed daemon).
    let lock = thinkingroot_core::cortex::read_lock().ok().flatten()?;
    if !thinkingroot_core::cortex::process_alive(lock.pid) {
        return None;
    }
    if !cortex_bridge::health_check(&lock.host, lock.port).await {
        return None;
    }

    // Write-through so the next call (`mcp_list_connected`, `llm_health`,
    // `brain_load` etc.) doesn't re-do the work. `child = None` because
    // we did not spawn this process; `shutdown()` checks this and refuses
    // to kill what it doesn't own — same contract as the cortex attach
    // path in `agent_runtime_subprocess::spawn`.
    {
        let mut guard = state.sidecar.lock().await;
        if guard.is_none() {
            *guard = Some(crate::state::SidecarHandle {
                host: lock.host.clone(),
                port: lock.port,
                pid: Some(lock.pid),
                child: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            });
        }
    }
    Some((lock.host, lock.port))
}

fn resolve_active_workspace() -> Result<(String, PathBuf), String> {
    let registry = WorkspaceRegistry::load()
        .map_err(|e| format!("load workspace registry: {e}"))?;
    let entry = registry
        .active_entry()
        .ok_or_else(|| "no active workspace selected".to_string())?;
    Ok((entry.name.clone(), entry.path.clone()))
}

#[derive(serde::Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

/// Parse the `{ ok, data, error }` envelope. Done in two stages: first
/// as `Value` so we can inspect `ok` + extract structured errors, then
/// `serde_json::from_value(data)` for the typed payload. This avoids a
/// `T: Default` bound on `decode_envelope` (which the previous
/// envelope-struct shape required).
async fn decode_envelope<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, String> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("read response body: {e}"))?;

    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("parse response envelope ({body}): {e}"))?;

    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !status.is_success() || !ok {
        if let Some(err) = value
            .get("error")
            .cloned()
            .and_then(|e| serde_json::from_value::<ApiError>(e).ok())
        {
            return Err(format!("[{}] {}", err.code, err.message));
        }
        if !status.is_success() {
            return Err(format!("HTTP {status}: {body}"));
        }
        return Err("response envelope reports ok=false with no error body".to_string());
    }

    let data = value
        .get("data")
        .cloned()
        .ok_or_else(|| "response envelope missing data field".to_string())?;
    serde_json::from_value(data)
        .map_err(|e| format!("parse response data ({body}): {e}"))
}
