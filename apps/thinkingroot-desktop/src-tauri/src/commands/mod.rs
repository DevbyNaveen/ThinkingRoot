//! Tauri commands (IPC handlers) grouped by surface.
//!
//! `#[tauri::command]` expands to a hidden `__cmd__<name>` item in
//! the same module as the handler function. `tauri::generate_handler!`
//! in `lib.rs` references each command by its fully-qualified path
//! (e.g. `commands::memory::memory_list`) rather than through a
//! re-export here — re-exports would resolve to the visible function
//! but hide the macro-generated helper.

// `commands::auth` is a thin re-export of `commands::cloud::auth_state`
// (Task 16 of the OSS cloud-readiness work). It does NOT define its own
// `#[tauri::command]` — the macro-generated helper lives in `commands::cloud`,
// so re-enabling this module no longer triggers the multi-definition error
// that Task 15 worked around. The re-export preserves any `use
// commands::auth;` import paths that may exist in the crate.
pub mod auth;
pub mod brain;
pub mod branch;
pub mod branch_data;
pub mod branch_extras;
pub mod branch_template;
pub mod browser;
pub mod browser_save;
pub mod chat;
pub mod claims;
pub mod cloud;
pub mod conversations;
pub mod doctor;
pub mod engram;
pub mod fs;
pub mod git;
pub mod install_tr;
pub mod mcp_local;
pub mod memory;
pub mod meta;
pub mod pack_export;
pub mod playground;
pub mod playground_fs;
pub mod privacy;
pub mod proposal;
pub mod recovery;
pub mod retrieve;
pub mod scan;
pub mod settings;
pub mod sidecar;
pub mod sidecar_client;
pub mod tag;
pub mod terminal;
pub mod updater;
pub mod workspace_status;
pub mod workspaces;
