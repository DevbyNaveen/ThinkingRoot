//! Meta info commands — build version + app exit.

use serde::Serialize;

/// Reported by `app_version()`.
#[derive(Serialize)]
pub struct Versions {
    /// Crate version of `thinkingroot-desktop-app`.
    pub app: &'static str,
}

#[tauri::command]
pub fn app_version() -> Versions {
    Versions {
        app: env!("CARGO_PKG_VERSION"),
    }
}

/// Close the main app window — wired to the `/quit` command palette
/// entry. Uses the normal exit path so macOS dock integration
/// behaves correctly and pending Tauri events flush.
#[tauri::command]
pub fn app_quit(app: tauri::AppHandle) {
    app.exit(0);
}
