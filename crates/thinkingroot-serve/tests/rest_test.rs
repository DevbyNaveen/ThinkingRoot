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
