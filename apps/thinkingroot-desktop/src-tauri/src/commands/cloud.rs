//! Tauri commands for the cloud-auth surface.
//!
//! All commands delegate to `thinkingroot-cloud-auth`. State changes
//! are signalled to the UI via the `cloud_status_changed` event.
//!
//! Spec: `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md` §7.1.
//!
//! Concurrency note: a single in-flight login is enforced two ways —
//! a desktop-local `tauri::State<LoginInFlight>` slot guards UI-side
//! double-clicks, and the underlying `auth_flow::run_browser_login`
//! has its own process-global `AlreadyInFlight` guard. The two layers
//! agree.

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;
use thinkingroot_cloud_auth::auth_flow::{run_browser_login, Surface};
use thinkingroot_cloud_auth::config::Config;
use thinkingroot_cloud_auth::{config, me, CloudError};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const EVENT_CLOUD_STATUS_CHANGED: &str = "cloud_status_changed";

/// Returned by `auth_state` and bundled into the `signed_in` event payload.
#[derive(Debug, Clone, Serialize)]
pub struct AuthState {
    pub signed_in: bool,
    pub handle: Option<String>,
    pub tier: Option<String>,
    pub credits_remaining: Option<u64>,
    pub credits_total: Option<u64>,
    pub period_end: Option<String>,
    pub server: String,
    pub last_refresh_at: Option<String>,
    /// Token-tail preview (e.g. `••••abcd`) — surfaced to the UI so a
    /// human can compare against what they pasted. Full token is
    /// never returned across the IPC boundary.
    pub token_redacted: Option<String>,
}

impl From<Config> for AuthState {
    fn from(c: Config) -> Self {
        let token_redacted = c.token.as_deref().and_then(|t| {
            if t.is_empty() {
                None
            } else if t.len() <= 8 {
                Some("••••".to_string())
            } else {
                Some(format!("••••{}", &t[t.len() - 4..]))
            }
        });
        Self {
            signed_in: c.is_signed_in(),
            handle: c.handle.clone(),
            tier: c.tier.clone(),
            credits_remaining: c.credits_remaining,
            credits_total: c.credits_total,
            period_end: c.credit_period_end.map(|d| d.to_rfc3339()),
            server: c.server.clone(),
            last_refresh_at: c.me_refreshed_at.map(|d| d.to_rfc3339()),
            token_redacted,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CreditsSnapshot {
    pub remaining: u64,
    pub total: u64,
    pub period_end: String,
}

/// Desktop-local guard tracking the currently-running login (if any).
/// The `CancellationToken` is signalled by `cloud_login_cancel`.
#[derive(Default)]
pub struct LoginInFlight {
    pub cancel: Option<CancellationToken>,
}

pub type LoginInFlightState = Arc<Mutex<LoginInFlight>>;

#[tauri::command]
pub async fn auth_state() -> Result<AuthState, String> {
    let cfg = config::load()
        .map_err(|e| e.to_string())?
        .unwrap_or_else(Config::empty);
    Ok(AuthState::from(cfg))
}

#[tauri::command]
pub async fn cloud_login_start(
    app: AppHandle,
    state: tauri::State<'_, LoginInFlightState>,
) -> Result<(), String> {
    let cfg = config::load()
        .map_err(|e| e.to_string())?
        .unwrap_or_else(Config::empty);
    let server = cfg.server.clone();
    let cancel = CancellationToken::new();
    {
        let mut guard = state.lock().await;
        if guard.cancel.is_some() {
            return Err("login already in progress".to_string());
        }
        guard.cancel = Some(cancel.clone());
    }
    let _ = app.emit(
        EVENT_CLOUD_STATUS_CHANGED,
        serde_json::json!({
            "status": "logging_in"
        }),
    );

    let app_for_task = app.clone();
    let state_for_task = state.inner().clone();
    let cancel_for_task = cancel.clone();
    tokio::spawn(async move {
        let outcome = run_browser_login(&server, Surface::Desktop, cancel_for_task).await;
        // Clear the in-flight slot regardless of outcome.
        {
            let mut guard = state_for_task.lock().await;
            guard.cancel = None;
        }
        match outcome {
            Ok(o) => {
                let cfg = config::load().ok().flatten().unwrap_or_else(Config::empty);
                let _ = app_for_task.emit(
                    EVENT_CLOUD_STATUS_CHANGED,
                    serde_json::json!({
                        "status": "signed_in",
                        "handle": o.handle,
                        "tier": o.tier,
                        "credits_remaining": cfg.credits_remaining.unwrap_or(0),
                        "credits_total": cfg.credits_total.unwrap_or(0),
                        "period_end": cfg.credit_period_end.map(|d| d.to_rfc3339()),
                    }),
                );
            }
            Err(err) => {
                let reason = match &err {
                    CloudError::Timeout => "timeout",
                    CloudError::Cancelled => "cancelled",
                    CloudError::StateMismatch => "state_mismatch",
                    CloudError::BindFailed(_) => "bind_failed",
                    CloudError::AlreadyInFlight => "already_in_flight",
                    _ => "hub_reject",
                };
                let _ = app_for_task.emit(
                    EVENT_CLOUD_STATUS_CHANGED,
                    serde_json::json!({
                        "status": "login_failed",
                        "reason": reason,
                        "detail": err.to_string(),
                    }),
                );
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn cloud_login_cancel(
    state: tauri::State<'_, LoginInFlightState>,
) -> Result<(), String> {
    let mut guard = state.lock().await;
    if let Some(c) = guard.cancel.take() {
        c.cancel();
    }
    Ok(())
}

#[tauri::command]
pub async fn cloud_logout(app: AppHandle) -> Result<(), String> {
    config::clear().map_err(|e| e.to_string())?;
    let _ = app.emit(
        EVENT_CLOUD_STATUS_CHANGED,
        serde_json::json!({
            "status": "signed_out"
        }),
    );
    Ok(())
}

#[tauri::command]
pub async fn cloud_refresh_me(app: AppHandle) -> Result<AuthState, String> {
    let cfg = config::load()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not signed in".to_string())?;
    match me::fetch_me(&cfg).await {
        Ok(me_resp) => {
            config::update(|c| {
                c.user_id = Some(me_resp.user.id.clone());
                c.handle = Some(me_resp.user.handle.clone());
                c.tier = Some(me_resp.user.tier.clone());
                c.credit_period_end = Some(me_resp.credit_period_end);
                c.token_expires_at = Some(me_resp.token_expires_at);
                c.me_refreshed_at = Some(chrono::Utc::now());
            })
            .map_err(|e| e.to_string())?;
            let refreshed = config::load().ok().flatten().unwrap_or_else(Config::empty);
            Ok(AuthState::from(refreshed))
        }
        Err(CloudError::AuthExpired) => {
            let _ = config::clear();
            let _ = app.emit(
                EVENT_CLOUD_STATUS_CHANGED,
                serde_json::json!({
                    "status": "auth_expired"
                }),
            );
            Err("session expired — re-run login".to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn cloud_credits_poll(app: AppHandle) -> Result<CreditsSnapshot, String> {
    let cfg = config::load()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not signed in".to_string())?;
    match me::fetch_credits(&cfg).await {
        Ok(c) => {
            config::update(|cfg_mut| {
                cfg_mut.credits_remaining = Some(c.remaining);
                cfg_mut.credits_total = Some(c.total);
                cfg_mut.credit_period_end = Some(c.period_end);
            })
            .map_err(|e| e.to_string())?;
            let _ = app.emit(
                EVENT_CLOUD_STATUS_CHANGED,
                serde_json::json!({
                    "status": "credits_updated",
                    "remaining": c.remaining,
                    "total": c.total,
                }),
            );
            Ok(CreditsSnapshot {
                remaining: c.remaining,
                total: c.total,
                period_end: c.period_end.to_rfc3339(),
            })
        }
        Err(CloudError::AuthExpired) => {
            let _ = config::clear();
            let _ = app.emit(
                EVENT_CLOUD_STATUS_CHANGED,
                serde_json::json!({
                    "status": "auth_expired"
                }),
            );
            Err("session expired".to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn cloud_open_upgrade(app: AppHandle) -> Result<(), String> {
    let cfg = config::load()
        .map_err(|e| e.to_string())?
        .unwrap_or_else(Config::empty);
    let url = format!("{}/pricing", cfg.server.trim_end_matches('/'));
    // The desktop already loads `tauri_plugin_opener::init()` in
    // `lib.rs`; the `OpenerExt` trait gives us `open_url` against
    // any `Manager`. Honesty rule: if the plugin returns an error
    // (e.g. headless CI), surface it instead of pretending we
    // launched the browser.
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}
