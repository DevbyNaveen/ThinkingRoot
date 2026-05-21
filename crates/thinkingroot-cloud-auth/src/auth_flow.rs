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
use crate::deep_link_bus::{self, DeepLinkCallback};
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
static LOGIN_IN_FLIGHT: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

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
    // Concurrency guard. std::sync::Mutex keeps the critical section
    // sync so the scopeguard release below can run without spawning a
    // tokio task (which would leak the flag if the runtime shuts down
    // between the spawn and the task poll).
    {
        let mut in_flight = LOGIN_IN_FLIGHT
            .lock()
            .expect("LOGIN_IN_FLIGHT mutex poisoned");
        if *in_flight {
            return Err(CloudError::AlreadyInFlight);
        }
        *in_flight = true;
    }
    // Release the guard synchronously on drop. With std::sync::Mutex
    // the closure runs inline — no tokio::spawn, no runtime-shutdown
    // race that could leak the flag.
    let _guard = scopeguard::guard((), |()| {
        if let Ok(mut g) = LOGIN_IN_FLIGHT.lock() {
            *g = false;
        }
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

/// Browser URL for the deep-link flow (Desktop). No `callback_port`
/// query param — the hub recognises this as "fire `thinkingroot://`
/// instead of redirecting to 127.0.0.1".
fn build_browser_url_deeplink(server: &str, state: &str, surface: Surface) -> String {
    let server = server.trim_end_matches('/');
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "{server}/auth/cli?state={state}&surface={}&client_version={version}&channel=deeplink",
        surface.as_str()
    )
}

/// Run the deep-link browser-login flow. Unlike `run_browser_login`,
/// this DOES NOT spawn a localhost HTTP listener — it arms the
/// `deep_link_bus` and waits for the desktop's OS-level
/// `thinkingroot://signed-in?...` handler (registered in the Tauri
/// layer) to deliver the callback through the bus.
///
/// Same external contract as `run_browser_login`: same params, same
/// `LoginOutcome` on success, same `CloudError` variants. Internally
/// uses the same `LOGIN_IN_FLIGHT` guard so the two flows can't
/// race.
pub async fn run_browser_login_deeplink(
    server: &str,
    surface: Surface,
    cancel: CancellationToken,
) -> Result<LoginOutcome, CloudError> {
    // Concurrency guard — shared with the localhost path so the user
    // can't somehow trigger both at once.
    {
        let mut in_flight = LOGIN_IN_FLIGHT
            .lock()
            .expect("LOGIN_IN_FLIGHT mutex poisoned");
        if *in_flight {
            return Err(CloudError::AlreadyInFlight);
        }
        *in_flight = true;
    }
    let _guard = scopeguard::guard((), |()| {
        if let Ok(mut g) = LOGIN_IN_FLIGHT.lock() {
            *g = false;
        }
    });

    let state = ulid::Ulid::new().to_string();
    let rx = deep_link_bus::arm(state.clone());

    // Open the browser. We use `webbrowser` directly here (same as
    // the localhost path) so failures surface as `CloudError`.
    let url = build_browser_url_deeplink(server, &state, surface);
    info!(target: "cloud-auth", "deep-link login: opening {url}");
    if let Err(err) = webbrowser::open(&url) {
        deep_link_bus::disarm();
        return Err(CloudError::BrowserLaunch(err.to_string()));
    }

    // Wait for the deep-link handler to deliver, the user to cancel,
    // or the timeout to fire.
    let outcome = tokio::select! {
        cb = rx => {
            let cb = cb.map_err(|_| CloudError::Cancelled)?;
            finalize_login(
                server,
                CallbackParams {
                    token: cb.token,
                    handle: cb.handle,
                    tier: cb.tier,
                    expires_at: cb.expires_at,
                    azure_endpoint: cb.azure_endpoint,
                    azure_api_version: cb.azure_api_version,
                    azure_deployment: cb.azure_deployment,
                    azure_key: cb.azure_key,
                },
            ).await
        }
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => Err(CloudError::Timeout),
        _ = cancel.cancelled() => Err(CloudError::Cancelled),
    };

    deep_link_bus::disarm();
    outcome
}

/// Parse a `thinkingroot://signed-in?...` URL into the typed payload
/// the deep-link bus expects. Returns `None` if the URL is malformed
/// or doesn't match our scheme — caller should log and drop.
///
/// Expected URL shape:
///   thinkingroot://signed-in?state=<ULID>&token=<JWT>&handle=<...>
///                            &tier=<...>&expires_at=<RFC3339>
///                            [&azure_endpoint=...&azure_api_version=...
///                             &azure_deployment=...&azure_key=...]
pub fn parse_deep_link_callback(
    url: &str,
) -> Option<(String, DeepLinkCallback)> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.scheme() != "thinkingroot" {
        return None;
    }
    // host on macOS comes in as Some("signed-in"); on Windows it
    // sometimes lives in the path. Accept both shapes.
    let host_or_path = parsed.host_str().map(str::to_string).unwrap_or_else(|| {
        parsed.path().trim_start_matches('/').to_string()
    });
    if host_or_path != "signed-in" {
        return None;
    }
    let mut state = None;
    let mut token = None;
    let mut handle = None;
    let mut tier = None;
    let mut expires_at_raw = None;
    let mut azure_endpoint = None;
    let mut azure_api_version = None;
    let mut azure_deployment = None;
    let mut azure_key = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "state" => state = Some(v.into_owned()),
            "token" => token = Some(v.into_owned()),
            "handle" => handle = Some(v.into_owned()),
            "tier" => tier = Some(v.into_owned()),
            "expires_at" => expires_at_raw = Some(v.into_owned()),
            "azure_endpoint" => azure_endpoint = Some(v.into_owned()),
            "azure_api_version" => azure_api_version = Some(v.into_owned()),
            "azure_deployment" => azure_deployment = Some(v.into_owned()),
            "azure_key" => azure_key = Some(v.into_owned()),
            _ => {}
        }
    }
    let state = state?;
    let token = token?;
    let handle = handle?;
    let tier = tier?;
    let expires_at: DateTime<Utc> = expires_at_raw?
        .parse::<DateTime<Utc>>()
        .ok()?;
    Some((
        state,
        DeepLinkCallback {
            token,
            handle,
            tier,
            expires_at,
            azure_endpoint,
            azure_api_version,
            azure_deployment,
            azure_key,
        },
    ))
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
    /// Vended Azure APIM endpoint (e.g. `https://tr-prod.azure-api.net/openai`).
    /// All four `azure_*` fields are optional — `None` when the hub
    /// doesn't have APIM configured. They must arrive together or
    /// not at all; partial sets are silently dropped by `finalize_login`.
    #[serde(default)]
    azure_endpoint: Option<String>,
    #[serde(default)]
    azure_api_version: Option<String>,
    #[serde(default)]
    azure_deployment: Option<String>,
    #[serde(default)]
    azure_key: Option<String>,
}

#[derive(Debug, Clone)]
struct CallbackParams {
    token: String,
    handle: String,
    tier: String,
    expires_at: DateTime<Utc>,
    azure_endpoint: Option<String>,
    azure_api_version: Option<String>,
    azure_deployment: Option<String>,
    azure_key: Option<String>,
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
        azure_endpoint: params.azure_endpoint,
        azure_api_version: params.azure_api_version,
        azure_deployment: params.azure_deployment,
        azure_key: params.azure_key,
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
    cfg.display_name = me_resp.user.display_name.clone();
    cfg.tier = Some(me_resp.user.tier.clone());
    cfg.credit_period_end = Some(me_resp.credit_period_end);
    cfg.token_expires_at = Some(me_resp.token_expires_at);
    cfg.me_refreshed_at = Some(Utc::now());

    // Optional: prefetch credits to populate the chip.
    if let Ok(credits) = me::fetch_credits(&cfg).await {
        cfg.credits_remaining = Some(credits.remaining);
        cfg.credits_total = Some(credits.total);
    }

    // Persist the vended Azure provider. All four fields must arrive
    // together — a partial set is treated as "no managed provider"
    // rather than written incomplete. Existing managed_azure (if any)
    // is preserved when the hub omits the fields on signin, so a
    // user who's already provisioned doesn't lose their key when
    // re-authenticating against a hub that's still mid-deploy.
    if let (Some(endpoint), Some(api_version), Some(deployment), Some(api_key)) = (
        params.azure_endpoint,
        params.azure_api_version,
        params.azure_deployment,
        params.azure_key,
    ) {
        cfg.managed_azure = Some(crate::config::ManagedAzureProvider {
            endpoint,
            api_version,
            default_deployment: deployment,
            api_key,
            provisioned_at: Utc::now(),
        });
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
