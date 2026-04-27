// Tauri entry point — delegate to the lib crate's `run()` so the
// reusable surface (Tauri commands, plugin wiring) lives in lib.rs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    thinkingroot_desktop_app::run();
}
