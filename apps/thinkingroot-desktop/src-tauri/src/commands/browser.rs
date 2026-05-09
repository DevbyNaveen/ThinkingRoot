//! Manual in-app browser for the right rail.
//!
//! This is intentionally **not** the future agentic browser surface. It
//! is a user-controlled reading/browsing panel: tabs, address/search,
//! back/forward/reload, open externally, and real native rendering.
//!
//! Important implementation choice: this uses Tauri 2 child WebViews
//! (`Window::add_child`) rather than an iframe in the app's WebView.
//! Iframes fail on many normal sites because of `X-Frame-Options` and
//! CSP `frame-ancestors`; child WebViews are real WKWebView/WebView2/
//! WebKitGTK instances positioned over the right-rail content region.
//!
//! The UI owns browser chrome and sends viewport bounds whenever the
//! rail is resized. Rust owns the native WebView handles and emits
//! lifecycle events (`browser://event/<id>`) so the UI can keep title,
//! URL, and loading state honest.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::webview::{NewWindowResponse, PageLoadEvent, WebviewBuilder};
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, Webview, WebviewUrl,
};
use url::Url;
use uuid::Uuid;

use crate::state::AppState;

/// Logical-pixel bounds relative to the parent Tauri window.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct BrowserBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl BrowserBounds {
    fn normalized(self) -> Self {
        Self {
            x: self.x.max(0.0),
            y: self.y.max(0.0),
            width: self.width.max(1.0),
            height: self.height.max(1.0),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowserOpenRequest {
    pub url: String,
    pub bounds: BrowserBounds,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrowserSessionInfo {
    pub id: String,
    pub title: String,
    pub url: String,
    pub event: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserEvent {
    Loading {
        url: String,
    },
    Loaded {
        url: String,
    },
    Title {
        title: String,
    },
    Navigation {
        url: String,
    },
    NewWindow {
        url: String,
    },
    Download {
        url: String,
        path: Option<String>,
        success: Option<bool>,
    },
}

pub struct BrowserSession {
    pub info: BrowserSessionInfo,
    pub webview: Webview,
}

fn event_topic(id: &str) -> String {
    format!("browser://event/{id}")
}

/// Normalize human URL-bar input:
/// - `cursor.com` -> `https://cursor.com`
/// - `localhost:1420` -> `http://localhost:1420`
/// - words/spaces -> DuckDuckGo search URL
/// - `http://` allowed only for loopback/local dev hosts
fn normalize_url(input: &str) -> Result<Url, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty URL".to_string());
    }

    let candidate = if trimmed.contains("://") || trimmed.starts_with("about:") {
        trimmed.to_string()
    } else if looks_like_localhost(trimmed) {
        format!("http://{trimmed}")
    } else if looks_like_domain(trimmed) {
        format!("https://{trimmed}")
    } else {
        let encoded: String = url::form_urlencoded::byte_serialize(trimmed.as_bytes()).collect();
        format!("https://duckduckgo.com/?q={encoded}")
    };

    let url = Url::parse(&candidate).map_err(|e| format!("parse `{candidate}`: {e}"))?;
    match url.scheme() {
        "https" | "about" => Ok(url),
        "http" if is_loopback_or_local(&url) => Ok(url),
        "http" => Err("plain HTTP is allowed only for localhost / loopback URLs".to_string()),
        other => Err(format!("unsupported URL scheme `{other}`")),
    }
}

fn looks_like_localhost(s: &str) -> bool {
    s.starts_with("localhost:")
        || s.starts_with("127.0.0.1:")
        || s.starts_with("[::1]:")
        || s == "localhost"
        || s == "127.0.0.1"
        || s == "[::1]"
}

fn looks_like_domain(s: &str) -> bool {
    !s.contains(char::is_whitespace) && s.contains('.')
}

fn is_loopback_or_local(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1")
    )
}

/// Create a new native child WebView inside the main window.
#[tauri::command]
pub async fn browser_open(
    app: AppHandle,
    state: State<'_, AppState>,
    req: BrowserOpenRequest,
) -> Result<BrowserSessionInfo, String> {
    let id = Uuid::new_v4().to_string();
    let label = format!("browser-{id}");
    let url = normalize_url(&req.url)?;
    let bounds = req.bounds.normalized();
    let topic = event_topic(&id);

    let window = app
        .get_window("main")
        .ok_or_else(|| "main window not found".to_string())?;

    let app_for_new_window = app.clone();
    let app_for_title = app.clone();
    let app_for_load = app.clone();
    let app_for_download = app.clone();
    let topic_for_new_window = topic.clone();
    let topic_for_title = topic.clone();
    let topic_for_load = topic.clone();
    let topic_for_download = topic.clone();

    let builder = WebviewBuilder::new(&label, WebviewUrl::External(url.clone()))
        .on_navigation({
            let app = app.clone();
            let topic = topic.clone();
            move |url| {
                let _ = app.emit(
                    &topic,
                    BrowserEvent::Navigation {
                        url: url.to_string(),
                    },
                );
                url.scheme() == "https" || (url.scheme() == "http" && is_loopback_or_local(url))
            }
        })
        .on_new_window(move |url, _features| {
            // Do not let arbitrary pages spawn detached popup windows.
            // The UI receives this event and opens a controlled browser
            // tab or lets the user open externally.
            let _ = app_for_new_window.emit(
                &topic_for_new_window,
                BrowserEvent::NewWindow {
                    url: url.to_string(),
                },
            );
            NewWindowResponse::Deny
        })
        .on_document_title_changed(move |_webview, title| {
            let _ = app_for_title.emit(&topic_for_title, BrowserEvent::Title { title });
        })
        .on_page_load(move |_webview, payload| {
            let event = match payload.event() {
                PageLoadEvent::Started => BrowserEvent::Loading {
                    url: payload.url().to_string(),
                },
                PageLoadEvent::Finished => BrowserEvent::Loaded {
                    url: payload.url().to_string(),
                },
            };
            let _ = app_for_load.emit(&topic_for_load, event);
        })
        .on_download(move |_webview, event| {
            let payload = match event {
                tauri::webview::DownloadEvent::Requested { url, destination } => {
                    // Route downloads into a predictable folder instead
                    // of letting each platform choose a hidden default.
                    if let Some(path) = default_download_path(&url) {
                        *destination = path;
                    }
                    BrowserEvent::Download {
                        url: url.to_string(),
                        path: destination.to_str().map(|s| s.to_string()),
                        success: None,
                    }
                }
                tauri::webview::DownloadEvent::Finished { url, path, success } => {
                    BrowserEvent::Download {
                        url: url.to_string(),
                        path: path.as_ref().map(|p| p.display().to_string()),
                        success: Some(success),
                    }
                }
                _ => return true,
            };
            let _ = app_for_download.emit(&topic_for_download, payload);
            true
        });

    let webview = window
        .add_child(
            builder,
            LogicalPosition::new(bounds.x, bounds.y),
            LogicalSize::new(bounds.width, bounds.height),
        )
        .map_err(|e| format!("create browser webview: {e}"))?;

    let info = BrowserSessionInfo {
        id: id.clone(),
        title: req.title.unwrap_or_else(|| "New tab".to_string()),
        url: url.to_string(),
        event: topic,
    };
    state.browsers.write().await.insert(
        id,
        Arc::new(BrowserSession {
            info: info.clone(),
            webview,
        }),
    );
    Ok(info)
}

#[tauri::command]
pub async fn browser_navigate(
    state: State<'_, AppState>,
    id: String,
    url: String,
) -> Result<String, String> {
    let url = normalize_url(&url)?;
    let session = get_session(&state, &id).await?;
    session
        .webview
        .navigate(url.clone())
        .map_err(|e| format!("navigate: {e}"))?;
    Ok(url.to_string())
}

#[tauri::command]
pub async fn browser_reload(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let session = get_session(&state, &id).await?;
    session.webview.reload().map_err(|e| format!("reload: {e}"))
}

#[tauri::command]
pub async fn browser_back(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let session = get_session(&state, &id).await?;
    session
        .webview
        .eval("history.back()")
        .map_err(|e| format!("history.back: {e}"))
}

#[tauri::command]
pub async fn browser_forward(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let session = get_session(&state, &id).await?;
    session
        .webview
        .eval("history.forward()")
        .map_err(|e| format!("history.forward: {e}"))
}

#[tauri::command]
pub async fn browser_set_bounds(
    state: State<'_, AppState>,
    id: String,
    bounds: BrowserBounds,
) -> Result<(), String> {
    let session = get_session(&state, &id).await?;
    let bounds = bounds.normalized();
    session
        .webview
        .set_position(LogicalPosition::new(bounds.x, bounds.y))
        .map_err(|e| format!("set browser position: {e}"))?;
    session
        .webview
        .set_size(LogicalSize::new(bounds.width, bounds.height))
        .map_err(|e| format!("set browser size: {e}"))?;
    Ok(())
}

#[tauri::command]
pub async fn browser_show(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let session = get_session(&state, &id).await?;
    session.webview.show().map_err(|e| format!("show: {e}"))
}

#[tauri::command]
pub async fn browser_hide(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let session = get_session(&state, &id).await?;
    session.webview.hide().map_err(|e| format!("hide: {e}"))
}

#[tauri::command]
pub async fn browser_focus(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let session = get_session(&state, &id).await?;
    session
        .webview
        .set_focus()
        .map_err(|e| format!("focus: {e}"))
}

#[tauri::command]
pub async fn browser_close(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let session = state.browsers.write().await.remove(&id);
    if let Some(session) = session {
        let _ = session.webview.close();
    }
    Ok(())
}

#[tauri::command]
pub async fn browser_list(state: State<'_, AppState>) -> Result<Vec<BrowserSessionInfo>, String> {
    let map = state.browsers.read().await;
    Ok(map.values().map(|s| s.info.clone()).collect())
}

pub async fn shutdown_all(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let ids: Vec<String> = state.browsers.read().await.keys().cloned().collect();
    for id in ids {
        if let Err(e) = browser_close(state.clone(), id.clone()).await {
            tracing::warn!(browser = %id, error = %e, "browser_close on shutdown failed");
        }
    }
}

async fn get_session(state: &State<'_, AppState>, id: &str) -> Result<Arc<BrowserSession>, String> {
    state
        .browsers
        .read()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| format!("no browser session `{id}`"))
}

fn default_download_path(url: &Url) -> Option<PathBuf> {
    let filename = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("download");
    let mut dir = dirs::download_dir().or_else(dirs::home_dir)?;
    dir.push("ThinkingRoot");
    let _ = std::fs::create_dir_all(&dir);
    dir.push(filename);
    Some(dir)
}
