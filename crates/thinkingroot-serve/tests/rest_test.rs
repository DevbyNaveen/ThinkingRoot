//! Integration tests for the ThinkingRoot REST API.
//!
//! Spins up an in-memory QueryEngine and verifies all REST endpoints
//! return correct status codes and envelope shapes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::rest::{AppState, build_router};

async fn empty_app(api_key: Option<String>) -> axum::Router {
    let engine = QueryEngine::new();
    let state = AppState::new(engine, api_key);
    build_router(state)
}

// ─── Workspace Listing ───────────────────────────────────────

#[tokio::test]
async fn list_workspaces_returns_ok() {
    let app = empty_app(None).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/workspaces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
}

// ─── 404 for Unknown Workspace ───────────────────────────────

#[tokio::test]
async fn missing_workspace_returns_404() {
    let app = empty_app(None).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/ws/nonexistent/entities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ─── Auth: Reject Without Key ────────────────────────────────

#[tokio::test]
async fn auth_rejects_without_key() {
    let app = empty_app(Some("secret".to_string())).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/workspaces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
}

// ─── Auth: Accept Correct Key ────────────────────────────────

#[tokio::test]
async fn auth_accepts_with_correct_key() {
    let app = empty_app(Some("secret".to_string())).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/workspaces")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

// ─── Branch Endpoints ────────────────────────────────────────

#[tokio::test]
async fn branch_list_endpoint_returns_ok() {
    let app = empty_app(None).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/branches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert!(
        json["data"]["branches"].is_array(),
        "branches should be an array"
    );
}

#[tokio::test]
async fn head_endpoint_returns_ok() {
    let app = empty_app(None).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/head")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
}

// ─── Ops endpoints (unauthenticated) ─────────────────────────

/// `/livez` is a bare-bones process-alive probe. No engine interaction.
#[tokio::test]
async fn livez_returns_ok_plain_text() {
    let app = empty_app(None).await;
    let response = app
        .oneshot(Request::builder().uri("/livez").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(std::str::from_utf8(&body).unwrap().trim(), "ok");
}

/// `/readyz` verifies the engine is reachable.
#[tokio::test]
async fn readyz_returns_ok_when_engine_healthy() {
    let app = empty_app(None).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(std::str::from_utf8(&body).unwrap().trim(), "ready");
}

/// `/metrics` emits Prometheus-format text with the expected base metrics.
#[tokio::test]
async fn metrics_emits_prometheus_text() {
    let app = empty_app(None).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let ct = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/plain"), "bad content-type: {ct}");

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("thinkingroot_up 1"),
        "missing thinkingroot_up gauge: {text}"
    );
    assert!(
        text.contains("thinkingroot_workspaces_total"),
        "missing workspaces gauge: {text}"
    );
    assert!(
        text.contains("thinkingroot_build_info"),
        "missing build_info gauge: {text}"
    );
    assert!(
        text.contains("thinkingroot_mcp_sessions_active"),
        "missing mcp_sessions gauge: {text}"
    );
}

/// Ops endpoints bypass the API-key middleware — monitoring systems
/// should be able to scrape without the key. Guards spec O-11 intent.
#[tokio::test]
async fn ops_endpoints_bypass_auth() {
    let app = empty_app(Some("secret".to_string())).await;

    // Without the bearer token, /api/v1/workspaces must 401.
    let unauth_api = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/workspaces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth_api.status(), StatusCode::UNAUTHORIZED);

    // But /livez, /readyz, /metrics must all 200 without auth.
    for path in ["/livez", "/readyz", "/metrics"] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{path} should be unauthenticated"
        );
    }
}
