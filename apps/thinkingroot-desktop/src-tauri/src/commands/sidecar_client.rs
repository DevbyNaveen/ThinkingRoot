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

    /// Idempotent — POST /api/v1/workspaces with the desktop's active
    /// workspace name + path. Daemon's `mount_workspace_handler` is a
    /// remount-overwrite so calling repeatedly is safe; it also pins
    /// `state.workspace_root` to this path (Stream A — see rest.rs)
    /// so branch operations target the right repo.
    async fn ensure_workspace_mounted(&self, root_path: &Path) -> Result<(), String> {
        let url = format!("http://{}:{}/api/v1/workspaces", self.host, self.port);
        let body = serde_json::json!({
            "name": self.workspace,
            "root_path": root_path.display().to_string(),
        });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("mount workspace request: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "mount workspace failed ({status}): {body}"
            ));
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
}

/// Wait for the sidecar metadata slot to populate, returning host/port.
/// Mirrors the boot wait in `commands/workspaces.rs::workspace_compile`
/// — a fresh-launch desktop may issue a Brain probe before the sidecar
/// has finished mounting workspaces, and 60 s of polling is far better
/// than failing the user's first action with `sidecar not running`.
async fn resolve_sidecar(app: &AppHandle) -> Result<(String, u16), String> {
    let state = app.state::<AppState>();
    for _ in 0..SIDECAR_BOOT_MAX_ATTEMPTS {
        {
            let guard = state.sidecar.lock().await;
            if let Some(h) = guard.as_ref() {
                return Ok((h.host.clone(), h.port));
            }
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
