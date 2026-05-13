//! Embedded fake hub server for integration testing.
//!
//! Mirrors every cloud contract from spec §5 + §6 so OSS slices can
//! ship green without requiring `~/Desktop/thinkingroot-cloud/` to
//! exist.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Default)]
pub struct FakeCloudConfig {
    pub me_status: Option<u16>,
    pub credits_status: Option<u16>,
    pub credits_remaining: u64,
    pub credits_total: u64,
    pub canned_token: String,
}

pub struct FakeCloud {
    pub uri: String,
    pub callback_count: Arc<AtomicU64>,
    handle: JoinHandle<()>,
}

impl FakeCloud {
    pub async fn spawn(cfg: FakeCloudConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr: SocketAddr = listener.local_addr().unwrap();
        let uri = format!("http://{local_addr}");

        let shared = Arc::new(SharedFake {
            cfg,
            callback_count: Arc::new(AtomicU64::new(0)),
        });
        let callback_count = shared.callback_count.clone();

        let app = Router::new()
            .route("/auth/cli", get(auth_cli_page))
            .route("/auth/cli/complete", post(auth_cli_complete))
            .route("/me", get(me))
            .route("/credits/balance", get(credits_balance))
            .with_state(shared.clone());

        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            uri,
            callback_count,
            handle,
        }
    }

    pub fn shutdown(self) {
        self.handle.abort();
    }
}

struct SharedFake {
    cfg: FakeCloudConfig,
    callback_count: Arc<AtomicU64>,
}

#[derive(Deserialize)]
struct AuthCliQuery {
    state: String,
    callback_port: u16,
    #[serde(default)]
    surface: String,
}

/// Simulates the hub's `/auth/cli` page: instead of rendering an HTML
/// login form, immediately 302-redirects to the localhost callback
/// with a fixed canned token. Tests that need the user-interaction
/// step use `/auth/cli/complete` instead.
async fn auth_cli_page(
    State(state): State<Arc<SharedFake>>,
    Query(q): Query<AuthCliQuery>,
) -> impl IntoResponse {
    state.callback_count.fetch_add(1, Ordering::SeqCst);
    let token = if state.cfg.canned_token.is_empty() {
        "tr_test_fake_token".to_string()
    } else {
        state.cfg.canned_token.clone()
    };
    let _ = q.surface; // silence unused warning while we accept the param
    let url = format!(
        "http://127.0.0.1:{}/callback?state={}&token={}&handle=tester&tier=pro&expires_at=2026-08-11T00:00:00Z",
        q.callback_port, q.state, token,
    );
    Redirect::temporary(&url)
}

async fn auth_cli_complete() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true}))
}

async fn me(State(state): State<Arc<SharedFake>>) -> impl IntoResponse {
    let status = state.cfg.me_status.unwrap_or(200);
    let body = if status == 200 {
        serde_json::json!({
            "user": {
                "id": "user_01HFAKE",
                "handle": "tester",
                "display_name": null,
                "tier": "pro"
            },
            "credit_period_end": "2026-06-13T00:00:00Z",
            "token_expires_at": "2026-08-11T00:00:00Z"
        })
    } else {
        serde_json::json!({"error": "token_invalid"})
    };
    (axum::http::StatusCode::from_u16(status).unwrap(), Json(body))
}

async fn credits_balance(State(state): State<Arc<SharedFake>>) -> impl IntoResponse {
    let status = state.cfg.credits_status.unwrap_or(200);
    let body = if status == 200 {
        serde_json::json!({
            "remaining": state.cfg.credits_remaining,
            "total": state.cfg.credits_total,
            "period_end": "2026-06-13T00:00:00Z"
        })
    } else {
        serde_json::json!({"error": "token_invalid"})
    };
    (axum::http::StatusCode::from_u16(status).unwrap(), Json(body))
}

#[tokio::test]
async fn fake_cloud_me_returns_canned_response() {
    let fake = FakeCloud::spawn(FakeCloudConfig::default()).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/me", fake.uri))
        .bearer_auth("anything")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["handle"], "tester");
    fake.shutdown();
}
