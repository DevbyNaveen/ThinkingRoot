//! Explicit sidecar lifecycle controls (restart / recovery).

use tauri::{AppHandle, Manager};
use thinkingroot_core::cortex;

use crate::agent_runtime_subprocess;
use crate::state::AppState;

/// Gracefully shut down the managed sidecar (if we own it), drop the
/// cortex singleton lock if present, and spawn a fresh `root serve`.
#[tauri::command]
pub async fn sidecar_restart(app: AppHandle) -> Result<String, String> {
    agent_runtime_subprocess::shutdown(&app).await;
    {
        let state = app.state::<AppState>();
        let mut guard = state.sidecar.lock().await;
        *guard = None;
    }
    if let Err(e) = cortex::remove_lock() {
        tracing::debug!(error = %e, "sidecar_restart: remove_lock (optional)");
    }
    agent_runtime_subprocess::spawn(&app).await;
    Ok("Local engine restarted. Wait a few seconds, then retry.".to_string())
}
