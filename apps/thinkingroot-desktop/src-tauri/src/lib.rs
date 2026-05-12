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
mod cortex_bridge;
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

            // Ensure the auto-mounted "playground" workspace exists
            // and is registered. Idempotent — the second-launch call
            // is just a registry membership check. Failure is logged
            // but never aborts boot (existing workspaces still work).
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                commands::playground::ensure_playground_at_boot(&handle).await;
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::Destroyed => {
                    let handle = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        // Order matters: kill terminal children first so
                        // their PTY masters drop while the app handle is
                        // still alive (otherwise their read threads
                        // would fail to emit the final exit event).
                        commands::terminal::shutdown_all(&handle).await;
                        commands::browser::shutdown_all(&handle).await;
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
            commands::sidecar::sidecar_restart,
            commands::memory::memory_list,
            commands::memory::brain_load,
            commands::settings::config_paths,
            commands::settings::global_config_read,
            commands::settings::global_config_write,
            commands::settings::credentials_status,
            commands::settings::credentials_set,
            commands::settings::credentials_remove,
            commands::settings::onboarding_status,
            commands::settings::workspace_llm_config,
            commands::settings::workspace_llm_write,
            commands::workspaces::workspace_list,
            commands::workspaces::workspace_add,
            commands::workspaces::workspace_remove,
            commands::workspaces::workspace_set_active,
            commands::workspaces::workspace_compile,
            commands::workspaces::workspace_compile_stop,
            commands::workspaces::workspace_compile_status,
            commands::workspaces::workspace_readme,
            commands::fs::fs_list_dir,
            commands::fs::fs_read_text,
            commands::git::git_branches,
            commands::install_tr::install_tr_file,
            commands::pack_export::pack_estimate,
            commands::pack_export::pack_export,
            commands::doctor::doctor_run,
            commands::mcp_local::mcp_status,
            commands::mcp_local::mcp_get_config_snippet,
            commands::mcp_local::mcp_configure_tool,
            commands::mcp_local::mcp_list_connected,
            commands::privacy::privacy_summary,
            commands::privacy::privacy_forget,
            commands::scan::workspace_scan,
            commands::auth::auth_state,
            commands::conversations::conversations_list,
            commands::conversations::conversations_create,
            commands::conversations::conversations_get,
            commands::conversations::conversations_append_message,
            commands::conversations::conversations_delete,
            commands::conversations::conversations_rename,
            commands::chat::chat_send_stream,
            commands::chat::chat_approve,
            commands::chat::llm_health,
            commands::branch::branch_list,
            commands::branch::branch_create,
            commands::branch::branch_checkout,
            commands::branch::branch_merge,
            commands::branch::branch_delete,
            commands::branch_extras::branch_events,
            commands::branch_extras::branch_stats,
            commands::branch_extras::branch_lineage,
            commands::branch_extras::branch_rebase,
            commands::branch_extras::branch_rollback,
            commands::branch_extras::branch_diff,
            commands::branch_extras::branch_event_subscribe,
            commands::branch_extras::branch_event_unsubscribe,
            commands::tag::tag_create,
            commands::tag::tag_list,
            commands::tag::tag_get,
            commands::proposal::proposal_open,
            commands::proposal::proposal_list,
            commands::proposal::proposal_review,
            commands::proposal::proposal_close,
            commands::brain::brain_brief,
            commands::brain::brain_investigate,
            commands::retrieve::retrieve_hybrid,
            commands::claims::claims_list,
            commands::claims::claims_as_of,
            commands::claims::claims_rooted,
            commands::branch_template::branch_template_list,
            commands::branch_template::branch_template_get,
            commands::branch_template::branch_template_upsert,
            commands::branch_template::branch_template_delete,
            commands::branch_template::branch_template_apply,
            commands::branch_data::branch_contribute_bulk,
            commands::branch_data::branch_redaction_set,
            commands::engram::engram_materialize,
            commands::engram::engram_list,
            commands::engram::engram_probe,
            commands::engram::engram_expire,
            commands::workspace_status::workspace_status_get,
            commands::workspace_status::workspace_status_get_all,
            commands::workspace_status::workspace_status_refresh,
            commands::workspace_status::subscribe_workspace_status_stream,
            commands::workspace_status::unsubscribe_workspace_status_stream,
            commands::terminal::terminal_open,
            commands::terminal::terminal_write,
            commands::terminal::terminal_resize,
            commands::terminal::terminal_close,
            commands::terminal::terminal_list,
            commands::browser::browser_open,
            commands::browser::browser_navigate,
            commands::browser::browser_reload,
            commands::browser::browser_back,
            commands::browser::browser_forward,
            commands::browser::browser_set_bounds,
            commands::browser::browser_show,
            commands::browser::browser_hide,
            commands::browser::browser_focus,
            commands::browser::browser_close,
            commands::browser::browser_list,
            commands::browser::browser_devtools,
            commands::browser::browser_find,
            commands::browser::browser_find_clear,
            commands::browser::browser_zoom,
            commands::browser::browser_print,
            commands::browser::browser_scroll_to,
            commands::browser_save::browser_save_page,
            commands::browser_save::browser_extract_callback,
            commands::playground::playground_ensure,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ThinkingRoot Desktop");
}
