//! Browser-flow login orchestrator with localhost callback listener.
//!
//! Spec: `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md`
//! §5.1 - §5.9.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::{self, Config};
use crate::error::CloudError;
use crate::me;

/// Which surface (CLI or Desktop) is initiating the login. Only used
/// to inform the hub's success-page copy; OSS-side behavior is
/// identical.
#[derive(Debug, Clone, Copy)]
pub enum Surface {
    Cli,
    Desktop,
}

impl Surface {
    fn as_str(&self) -> &'static str {
        match self {
            Surface::Cli => "cli",
            Surface::Desktop => "desktop",
        }
    }
}

/// What `run_browser_login` returns on success.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    pub handle: String,
    pub tier: String,
    pub credits_remaining: Option<u64>,
}

/// Process-global guard: only one in-flight login at a time.
static LOGIN_IN_FLIGHT: Mutex<bool> = Mutex::const_new(false);

const LOGIN_TIMEOUT: Duration = Duration::from_secs(60);

/// Run the browser-flow login. Binds a localhost callback listener on
/// `127.0.0.1:0` (kernel-picked port), generates a 26-char ULID state
/// nonce, opens the browser, waits up to 60s for the callback, then
/// persists the session to auth.json + fetches `/me` to populate the
/// tier+credits fields.
///
/// Cancellation: pass a `CancellationToken`; cancelling it shuts the
/// listener and returns `Err(CloudError::Cancelled)`.
pub async fn run_browser_login(
    server: &str,
    surface: Surface,
    cancel: CancellationToken,
) -> Result<LoginOutcome, CloudError> {
    // Concurrency guard.
    {
        let mut in_flight = LOGIN_IN_FLIGHT.lock().await;
        if *in_flight {
            return Err(CloudError::AlreadyInFlight);
        }
        *in_flight = true;
    }
    // Release the guard via a drop-bomb so panics + early returns clear it.
    let _guard = scopeguard::guard((), |()| {
        let _ = tokio::spawn(async {
            let mut in_flight = LOGIN_IN_FLIGHT.lock().await;
            *in_flight = false;
        });
    });

    let state = ulid::Ulid::new().to_string();
    let (callback_tx, callback_rx) = tokio::sync::oneshot::channel::<CallbackParams>();
    let shared = Arc::new(SharedState {
        expected_state: state.clone(),
        tx: Mutex::new(Some(callback_tx)),
    });

    let app = Router::new()
        .route("/callback", get(handle_callback))
        .with_state(shared.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(CloudError::BindFailed)?;
    let local_addr: SocketAddr = listener.local_addr().map_err(CloudError::BindFailed)?;
    let port = local_addr.port();

    let server_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            warn!(error = %e, "auth_flow: axum server exited with error");
        }
    });

    let url = build_browser_url(server, &state, port, surface);
    info!(url = %url, port, "auth_flow: opening browser");

    let browser_result = webbrowser::open(&url);
    if browser_result.is_err() {
        warn!(
            url = %url,
            "auth_flow: webbrowser::open failed; listener still waiting"
        );
        eprintln!("\nCould not open browser automatically. Please open this URL:\n\n  {url}\n");
    }

    let outcome = tokio::select! {
        params = callback_rx => {
            let params = params.map_err(|_| CloudError::Cancelled)?;
            finalize_login(server, params).await
        }
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => Err(CloudError::Timeout),
        _ = cancel.cancelled() => Err(CloudError::Cancelled),
    };

    server_handle.abort();
    let _ = server_handle.await;

    outcome
}

fn build_browser_url(server: &str, state: &str, port: u16, surface: Surface) -> String {
    let server = server.trim_end_matches('/');
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "{server}/auth/cli?state={state}&callback_port={port}&surface={}&client_version={version}",
        surface.as_str()
    )
}

struct SharedState {
    expected_state: String,
    tx: Mutex<Option<tokio::sync::oneshot::Sender<CallbackParams>>>,
}

#[derive(Debug, Clone, Deserialize)]
struct CallbackQuery {
    state: String,
    token: String,
    handle: String,
    tier: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct CallbackParams {
    token: String,
    handle: String,
    tier: String,
    expires_at: DateTime<Utc>,
}

async fn handle_callback(
    State(state): State<Arc<SharedState>>,
    Query(params): Query<CallbackQuery>,
) -> Result<Html<&'static str>, (axum::http::StatusCode, &'static str)> {
    if params.state != state.expected_state {
        warn!("auth_flow: state nonce mismatch on callback — refusing");
        return Err((axum::http::StatusCode::BAD_REQUEST, "state nonce mismatch"));
    }

    let mut tx_slot = state.tx.lock().await;
    let Some(tx) = tx_slot.take() else {
        return Err((axum::http::StatusCode::GONE, "callback already consumed"));
    };
    let _ = tx.send(CallbackParams {
        token: params.token,
        handle: params.handle,
        tier: params.tier,
        expires_at: params.expires_at,
    });

    Ok(Html(
        "<!doctype html><html><head><meta charset=utf-8><title>Signed in</title>\
         <style>body{font-family:system-ui;margin:64px;text-align:center;}</style>\
         </head><body><h1>Signed in</h1><p>You can close this window and return to your app.</p>\
         </body></html>",
    ))
}

async fn finalize_login(
    server: &str,
    params: CallbackParams,
) -> Result<LoginOutcome, CloudError> {
    let mut cfg = config::load()?.unwrap_or_else(Config::empty);
    cfg.token = Some(params.token);
    cfg.server = server.to_string();
    cfg.handle = Some(params.handle.clone());
    cfg.tier = Some(params.tier.clone());
    cfg.token_expires_at = Some(params.expires_at);
    cfg.me_refreshed_at = Some(Utc::now());

    // Verify token via /me; populate user_id + period_end + token_expires_at
    // authoritatively.
    let me_resp = me::fetch_me(&cfg).await?;
    cfg.user_id = Some(me_resp.user.id.clone());
    cfg.handle = Some(me_resp.user.handle.clone());
    cfg.tier = Some(me_resp.user.tier.clone());
    cfg.credit_period_end = Some(me_resp.credit_period_end);
    cfg.token_expires_at = Some(me_resp.token_expires_at);
    cfg.me_refreshed_at = Some(Utc::now());

    // Optional: prefetch credits to populate the chip.
    if let Ok(credits) = me::fetch_credits(&cfg).await {
        cfg.credits_remaining = Some(credits.remaining);
        cfg.credits_total = Some(credits.total);
    }

    config::save(&cfg)?;

    Ok(LoginOutcome {
        handle: cfg.handle.clone().unwrap_or_default(),
        tier: cfg.tier.clone().unwrap_or_default(),
        credits_remaining: cfg.credits_remaining,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_includes_all_query_params() {
        let url = build_browser_url("https://hub.example.com", "01HXYZ", 49251, Surface::Cli);
        assert!(url.contains("https://hub.example.com/auth/cli?"));
        assert!(url.contains("state=01HXYZ"));
        assert!(url.contains("callback_port=49251"));
        assert!(url.contains("surface=cli"));
        assert!(url.contains("client_version="));
    }

    #[test]
    fn build_url_strips_trailing_slash() {
        let url = build_browser_url("https://hub.example.com/", "X", 1, Surface::Desktop);
        assert!(!url.contains(".com//auth"));
        assert!(url.contains("surface=desktop"));
    }
}
