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
    // Slice 2 additions:
    pub completion_status: Option<u16>,
    pub completion_body: Option<String>,
    pub completion_stream_chunks: Vec<String>,
    pub credits_remaining_after_completion: u64,
    pub credits_total_after_completion: u64,
    pub model_catalogue_status: Option<u16>,
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
            .route("/v1/models", get(v1_models))
            .route("/v1/chat/completions", post(v1_chat_completions))
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

async fn v1_models(State(state): State<Arc<SharedFake>>) -> impl IntoResponse {
    let status = state.cfg.model_catalogue_status.unwrap_or(200);
    let body = if status == 200 {
        serde_json::json!({
            "data": [
                {
                    "id": "claude-opus-4-7",
                    "owned_by": "anthropic",
                    "credits_per_1k_input_tokens": 15,
                    "credits_per_1k_output_tokens": 75,
                    "context_window": 1_000_000
                },
                {
                    "id": "gpt-5",
                    "owned_by": "openai",
                    "credits_per_1k_input_tokens": 10,
                    "credits_per_1k_output_tokens": 50,
                    "context_window": 200_000
                }
            ]
        })
    } else {
        serde_json::json!({"error": "internal_error"})
    };
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        Json(body),
    )
}

async fn v1_chat_completions(
    State(state): State<Arc<SharedFake>>,
) -> axum::http::Response<axum::body::Body> {
    let status = state.cfg.completion_status.unwrap_or(200);
    if status != 200 {
        let body = state
            .cfg
            .completion_body
            .clone()
            .unwrap_or_else(|| format!(r#"{{"error":"http_{status}"}}"#));
        let mut resp = axum::http::Response::new(axum::body::Body::from(body));
        *resp.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
        resp.headers_mut().insert(
            "x-tr-credits-remaining",
            axum::http::HeaderValue::from_str(
                &state.cfg.credits_remaining_after_completion.to_string(),
            )
            .unwrap(),
        );
        return resp;
    }

    // Successful stream — canned SSE chunks.
    let chunks = if state.cfg.completion_stream_chunks.is_empty() {
        vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n".to_string(),
            "data: {\"choices\":[{\"delta\":{\"content\":\"world!\"}}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ]
    } else {
        state.cfg.completion_stream_chunks.clone()
    };
    let body = chunks.concat();
    let mut resp = axum::http::Response::new(axum::body::Body::from(body));
    *resp.status_mut() = axum::http::StatusCode::OK;
    resp.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    resp.headers_mut().insert(
        "x-tr-credits-remaining",
        axum::http::HeaderValue::from_str(
            &state.cfg.credits_remaining_after_completion.to_string(),
        )
        .unwrap(),
    );
    resp.headers_mut().insert(
        "x-tr-credits-total",
        axum::http::HeaderValue::from_str(
            &state.cfg.credits_total_after_completion.to_string(),
        )
        .unwrap(),
    );
    resp.headers_mut().insert(
        "x-tr-tier",
        axum::http::HeaderValue::from_static("pro"),
    );
    resp.headers_mut().insert(
        "x-tr-period-end",
        axum::http::HeaderValue::from_static("2026-06-13T00:00:00Z"),
    );
    resp
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

#[tokio::test]
async fn fake_cloud_chat_returns_sse_with_credit_headers() {
    let fake = FakeCloud::spawn(FakeCloudConfig {
        credits_remaining_after_completion: 48140,
        credits_total_after_completion: 50000,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", fake.uri))
        .bearer_auth("test-token")
        .json(&serde_json::json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-tr-credits-remaining")
            .and_then(|v| v.to_str().ok()),
        Some("48140")
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("Hello"));
    assert!(body.contains("[DONE]"));
    fake.shutdown();
}

#[tokio::test]
async fn fake_cloud_models_returns_catalogue() {
    let fake = FakeCloud::spawn(FakeCloudConfig::default()).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/v1/models", fake.uri))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], "claude-opus-4-7");
    fake.shutdown();
}
