//! Tauri commands (IPC handlers) grouped by surface.
//!
//! `#[tauri::command]` expands to a hidden `__cmd__<name>` item in
//! the same module as the handler function. `tauri::generate_handler!`
//! in `lib.rs` references each command by its fully-qualified path
//! (e.g. `commands::memory::memory_list`) rather than through a
//! re-export here — re-exports would resolve to the visible function
//! but hide the macro-generated helper.

pub mod fs;
pub mod git;
pub mod install_tr;
pub mod mcp_local;
pub mod memory;
pub mod meta;
pub mod privacy;
pub mod settings;
pub mod workspaces;
