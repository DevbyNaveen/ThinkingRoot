//! In-app auto-update.
//!
//! Two surfaces on the same `tauri_plugin_updater::UpdaterExt`:
//!
//! 1. [`check_for_updates`] — launch-time background task that polls
//!    the GitHub `latest.json` endpoint, downloads + installs any
//!    available update silently, and emits `update-installed` with
//!    the new version when it finishes. The frontend uses that
//!    event to surface a "Relaunch to update" banner.
//! 2. [`updater_check_now`] — manual Tauri command exposed to the
//!    webview for a "Check for updates" menu entry / button.
//!
//! Signature verification is done by the plugin against the public
//! key pinned in `tauri.conf.json::plugins.updater.pubkey`. Without
//! a matching signature, `download_and_install` rejects the package
//! before any byte is written.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_updater::UpdaterExt;

#[derive(Clone, Serialize)]
pub struct UpdateStatus {
    /// `true` when an update was found and successfully installed.
    /// The user still needs to relaunch to load the new bytes.
    pub installed: bool,
    /// Version string the package advertised, when present.
    pub version: Option<String>,
    /// Human-readable note for the frontend / logs. Empty on success.
    pub message: String,
}

/// Background launch-time check. Runs once per app launch on the
/// existing tokio runtime. All failures are logged; never panics.
pub async fn check_for_updates<R: Runtime>(app: AppHandle<R>) {
    match run_check(&app).await {
        Ok(status) => {
            if status.installed {
                tracing::info!(
                    version = ?status.version,
                    "auto-update installed; emitting update-installed event"
                );
                let _ = app.emit("update-installed", &status);
            } else {
                tracing::debug!(message = %status.message, "auto-update: no action");
            }
        }
        Err(e) => {
            // Updater failures are best-effort — a transient network
            // hiccup must not stop the app from launching. Log loudly
            // enough for `root doctor` / log inspection to find it.
            tracing::warn!(error = %e, "auto-update check failed");
        }
    }
}

/// Manual "check now" command callable from the webview. Returns the
/// same [`UpdateStatus`] shape `check_for_updates` emits, so the
/// frontend can render both call paths uniformly.
#[tauri::command]
pub async fn updater_check_now(app: AppHandle) -> Result<UpdateStatus, String> {
    run_check(&app).await.map_err(|e| e.to_string())
}

async fn run_check<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<UpdateStatus, Box<dyn std::error::Error + Send + Sync>> {
    let updater = app.updater()?;
    let pkg = updater.check().await?;
    let Some(update) = pkg else {
        return Ok(UpdateStatus {
            installed: false,
            version: None,
            message: "no update available".into(),
        });
    };
    let version = update.version.clone();
    // download_and_install streams the package, verifies the
    // signature against the pinned pubkey, and replaces the running
    // binary in place. The two closures are progress + finished
    // hooks — we don't surface progress today but the signatures
    // are load-bearing for the plugin API.
    update
        .download_and_install(|_downloaded, _total| {}, || {})
        .await?;
    Ok(UpdateStatus {
        installed: true,
        version: Some(version),
        message: "update installed — relaunch to apply".into(),
    })
}
