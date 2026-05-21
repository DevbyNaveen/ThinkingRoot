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
mod install_manifest_bridge;
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
        // Auto-update: signed updates pulled from the latest GitHub
        // release. Verified against the public key pinned in
        // tauri.conf.json. See commands::updater::check_for_updates.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Deep-link plugin — registers the `thinkingroot://` URL
        // scheme with the OS (Info.plist on macOS, Registry on
        // Windows, .desktop on Linux). The handler in the setup
        // closure below routes incoming URLs through the cloud-auth
        // deep_link_bus so an in-flight browser-login can resume.
        .plugin(tauri_plugin_deep_link::init())
        .manage::<commands::cloud::LoginInFlightState>(std::sync::Arc::new(
            tokio::sync::Mutex::new(commands::cloud::LoginInFlight::default()),
        ))
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

            // Register this desktop bundle in the install manifest
            // (idempotent — safe every launch).  Sync — fast enough
            // (one BLAKE3 over the bundled binary plus a JSON
            // round-trip) to do before the user can open any
            // engine surface.  Failures are logged + swallowed.
            install_manifest_bridge::register_desktop_bundle(app.handle());

            // One-time migration of pre-Slice-1 cloud session state
            // from `desktop.toml` into the cloud-auth crate's
            // `auth.json`. No-op when there's nothing to migrate or
            // when a new auth.json already exists. Async because the
            // best-effort `/me` verification call blocks.
            tauri::async_runtime::spawn(async {
                if let Err(e) =
                    config::migrate_legacy_cloud_fields_on_first_run().await
                {
                    tracing::warn!(error = %e, "legacy cloud field migration failed");
                }
            });

            // Launch-time auto-update check. Pulls latest.json from
            // the GitHub release, verifies against the pinned pubkey,
            // and silently installs any available update. Emits an
            // `update-installed` event when done so the webview can
            // surface a "Relaunch to update" affordance.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                commands::updater::check_for_updates(handle).await;
            });

            // Deep-link handler. Every `thinkingroot://signed-in?...`
            // URL the OS hands us flows through cloud_auth's parser
            // and bus. Drop URLs we don't recognise.
            use tauri_plugin_deep_link::DeepLinkExt;
            app.deep_link().on_open_url(|event| {
                for url in event.urls() {
                    let url_str = url.to_string();
                    match thinkingroot_cloud_auth::auth_flow::parse_deep_link_callback(
                        &url_str,
                    ) {
                        Some((state, callback)) => {
                            let outcome =
                                thinkingroot_cloud_auth::deep_link_bus::deliver(
                                    &state, callback,
                                );
                            tracing::info!(
                                target: "deep_link",
                                ?outcome,
                                "received {url_str}"
                            );
                        }
                        None => {
                            tracing::warn!(
                                target: "deep_link",
                                "ignoring unrecognised URL: {url_str}"
                            );
                        }
                    }
                }
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
                    // Two consumers of drag-drop:
                    //   1. `.tr` pack install sheet — listens for
                    //      `tr-file-opened` (single path payload).
                    //   2. Playground DropZone — listens for
                    //      `playground-files-dropped` (Vec<String>
                    //      of every non-`.tr` path in the drop).
                    let mut other_paths: Vec<String> = Vec::new();
                    for path in paths {
                        let is_tr = path
                            .extension()
                            .and_then(|s| s.to_str())
                            .map(|s| s.eq_ignore_ascii_case("tr"))
                            .unwrap_or(false);
                        if is_tr {
                            let payload = path.display().to_string();
                            let _ = window.emit("tr-file-opened", payload);
                        } else {
                            other_paths.push(path.display().to_string());
                        }
                    }
                    if !other_paths.is_empty() {
                        let _ = window.emit("playground-files-dropped", other_paths);
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
            commands::settings::get_setup_complete_at,
            commands::settings::global_config_read,
            commands::settings::global_config_write,
            commands::settings::credentials_status,
            commands::settings::credentials_set,
            commands::settings::credentials_remove,
            commands::settings::mark_setup_complete,
            commands::settings::workspace_llm_config,
            commands::settings::workspace_compilation_config,
            commands::settings::workspace_compilation_write,
            commands::settings::workspace_llm_write,
            commands::workspaces::workspace_list,
            commands::workspaces::workspace_add,
            commands::workspaces::workspace_remove,
            commands::workspaces::workspace_set_active,
            commands::workspaces::workspace_compile,
            commands::workspaces::workspace_compile_stop,
            commands::workspaces::workspace_compile_status,
            commands::fs::fs_list_dir,
            commands::fs::fs_read_text,
            commands::git::git_branches,
            commands::install_tr::install_tr_file,
            commands::pack_export::pack_estimate,
            commands::pack_export::pack_export,
            commands::doctor::doctor_run,
            commands::doctor::doctor_check,
            commands::doctor::doctor_apply_fix,
            commands::mcp_local::mcp_status,
            commands::mcp_local::mcp_get_config_snippet,
            commands::mcp_local::mcp_configure_tool,
            commands::mcp_local::mcp_list_connected,
            commands::privacy::privacy_summary,
            commands::privacy::privacy_forget,
            commands::scan::workspace_scan,
            // Cloud-auth surface (Task 15 — replaces legacy
            // commands::auth::auth_state; Task 16 deletes commands/auth.rs).
            commands::cloud::auth_state,
            commands::cloud::cloud_login_start,
            commands::cloud::cloud_login_cancel,
            commands::cloud::cloud_logout,
            commands::cloud::cloud_refresh_me,
            commands::cloud::cloud_credits_poll,
            commands::cloud::cloud_open_upgrade,
            commands::cloud::cloud_push_workspace,
            commands::cloud::cloud_pull_pack,
            commands::conversations::conversations_list,
            commands::conversations::conversations_create,
            commands::conversations::conversations_get,
            commands::conversations::conversations_append_message,
            commands::conversations::conversations_delete,
            commands::conversations::conversations_rename,
            commands::conversation_title::conversations_generate_title,
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
            commands::providers::list_providers,
            commands::providers::provider_validate_key,
            commands::providers::provider_fetch_models,
            commands::providers::provider_fetch_models_stored,
            commands::providers::provider_set_active_model,
            commands::providers::provider_save,
            commands::recovery::get_circuit_breaker_status,
            commands::recovery::reset_circuit_breaker,
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
            commands::playground::paper_get,
            commands::playground::playground_drop,
            commands::playground::playground_sources,
            commands::playground::playground_witnesses_by_source,
            commands::playground::playground_source_witnesses,
            commands::playground::playground_save_note,
            commands::playground::playground_open_proposal,
            commands::playground::playground_branch_conversation,
            commands::playground::playground_quiz,
            commands::playground::playground_export_tr,
            commands::playground::playground_handoff_url,
            commands::playground::playground_gaps,
            commands::playground::paper_regenerate,
            commands::playground_fs::playground_list_directory,
            commands::playground_fs::playground_create_folder,
            commands::playground_fs::playground_rename,
            commands::playground_fs::playground_move,
            commands::playground_fs::playground_trash,
            commands::playground_fs::playground_list_trash,
            commands::playground_fs::playground_restore,
            commands::playground_fs::playground_empty_trash,
            commands::playground_fs::playground_preview,
            commands::commits::commit_list,
            commands::commits::commit_get,
            commands::commits::commit_record,
            commands::commits::commit_merge_plan,
            commands::commits::commit_synthesize_merge,
            commands::substrate_bus::substrate_bus_start,
            commands::substrate_bus::substrate_bus_stop,
            commands::substrate_bus::substrate_bus_reports,
            commands::substrate_bus::substrate_bus_run_now,
            commands::updater::updater_check_now,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ThinkingRoot Desktop");
}
