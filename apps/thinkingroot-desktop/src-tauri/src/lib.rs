//! ThinkingRoot Desktop — Tauri 2 app runtime.
//!
//! The Rust side embeds a [`thinkingroot_serve::engine::QueryEngine`]
//! for in-process workspace queries and exposes a small set of Tauri
//! commands the webview calls via `invoke()`. Chat / agent
//! orchestration is delegated to an out-of-process agent-runtime
//! sidecar (Step 10) — not in this binary.

#![forbid(unsafe_code)]

mod agent_runtime_subprocess;
mod commands;
mod config;
mod state;
mod tray;

use tauri::{Emitter, Manager};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point called from `main.rs`.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .setup(|app| {
            app.manage(state::AppState::default());
            tray::install(app.handle())?;

            // Sidecar spawn happens on the existing tokio runtime —
            // we cannot block setup on the child handshake or the
            // window stays grey while the engine boots.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                agent_runtime_subprocess::spawn(&handle).await;
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::Destroyed => {
                    let handle = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        agent_runtime_subprocess::shutdown(&handle).await;
                    });
                }
                tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) => {
                    // Forward only `.tr` paths to the front-end. The
                    // sheet UI listens for `tr-file-opened` and routes
                    // the first match to `install_tr_file`.
                    for path in paths {
                        if path
                            .extension()
                            .and_then(|s| s.to_str())
                            .map(|s| s.eq_ignore_ascii_case("tr"))
                            .unwrap_or(false)
                        {
                            let payload = path.display().to_string();
                            let _ = window.emit("tr-file-opened", payload);
                        }
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::meta::app_version,
            commands::meta::app_quit,
            commands::memory::memory_list,
            commands::memory::brain_load,
            commands::settings::config_read,
            commands::settings::config_write,
            commands::settings::onboarding_status,
            commands::workspaces::workspace_list,
            commands::workspaces::workspace_add,
            commands::workspaces::workspace_remove,
            commands::workspaces::workspace_set_active,
            commands::workspaces::workspace_compile,
            commands::fs::fs_list_dir,
            commands::git::git_branches,
            commands::install_tr::install_tr_file,
            commands::mcp_local::mcp_status,
            commands::mcp_local::mcp_get_config_snippet,
            commands::privacy::privacy_summary,
            commands::privacy::privacy_forget,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ThinkingRoot Desktop");
}
