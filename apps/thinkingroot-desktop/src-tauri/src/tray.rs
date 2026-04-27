//! System tray (macOS menu bar / Windows + Linux system tray).
//!
//! Tauri 2 builds the tray off the running [`AppHandle`]; the icon
//! reuses the bundled window icon so we do not have to ship a
//! separate tray asset. The menu has three items — Show, Hide,
//! Quit — wired to lifecycle calls that target the `main` window.
//!
//! The tray persists for the lifetime of the process; quitting via
//! the menu calls `app.exit(0)` so window-state plugin's on-quit
//! save still runs.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Runtime};

/// Install the persistent tray icon. Idempotent in the sense that
/// Tauri's builder errors loudly if called twice with the same id —
/// `lib::run` calls this exactly once during `setup`.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "tray-show", "Show ThinkingRoot", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, "tray-hide", "Hide window", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "tray-quit", "Quit ThinkingRoot", true, None::<&str>)?;

    let menu = Menu::with_items(app, &[&show_item, &hide_item, &separator, &quit_item])?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    let _tray = TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("ThinkingRoot Desktop")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray-show" => focus_main_window(app),
            "tray-hide" => hide_main_window(app),
            "tray-quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

fn focus_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn hide_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}
