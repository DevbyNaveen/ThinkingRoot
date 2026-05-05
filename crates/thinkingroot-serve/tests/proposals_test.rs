//! Integration tests for the T0.4 Knowledge Proposal REST surface.
//!
//! These exercise the full open → review → close lifecycle through
//! the same routes the CLI and desktop will hit, so a regression in
//! the wiring fails here, not in production.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::path::PathBuf;
use tempfile::tempdir;
use tower::ServiceExt;

use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::rest::{AppState, build_router};

/// Build an in-memory router with a real workspace_root pointed at a
/// temp directory so the proposal handlers have a refs_dir to write
/// into.  The workspace root only needs to exist on disk — no
/// `.thinkingroot/` data dir is required for the proposal layer
/// (proposals live in `.thinkingroot-refs/proposals/` which the
/// crate creates lazily).
fn router_with_root(root: PathBuf) -> axum::Router {
    let engine = QueryEngine::new();
    let state = AppState::new_with_root(engine, None, Some(root));
    build_router(state)
}

async fn read_json(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn open_proposal_requires_user_header() {
    let dir = tempdir().unwrap();
    let app = router_with_root(dir.path().to_path_buf());

    let body = serde_json::json!({
        "description": "missing principal"
    })
    .to_string();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/branches/feature%2Fx/proposals")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = read_json(response).await;
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "MISSING_PRINCIPAL");
}

#[tokio::test]
async fn proposal_lifecycle_open_review_close_round_trips() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let app = router_with_root(root.clone());

    // 1. Open a proposal.
    let open_body = serde_json::json!({
        "description": "Adds the X feature.",
        "min_reviewers": 1
    })
    .to_string();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/branches/feature-x/proposals")
                .header("content-type", "application/json")
                .header("x-thinkingroot-user", "alice")
                .body(Body::from(open_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = read_json(response).await;
    assert_eq!(json["ok"], true);
    let proposal_id = json["data"]["proposal"]["id"].as_str().unwrap().to_string();
    assert_eq!(json["data"]["proposal"]["author"], "alice");
    assert_eq!(
        json["data"]["proposal"]["status"]["status"],
        "open",
        "fresh proposal must be Open, not Approved"
    );

    // 2. List the proposals for that branch — sees one entry.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/branches/feature-x/proposals")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = read_json(response).await;
    assert_eq!(json["data"]["proposals"].as_array().unwrap().len(), 1);

    // 3. Author cannot self-approve — status stays Open.
    let review_body = serde_json::json!({ "decision": "approve" }).to_string();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/proposals/{proposal_id}/reviews"))
                .header("content-type", "application/json")
                .header("x-thinkingroot-user", "alice")
                .body(Body::from(review_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = read_json(response).await;
    assert_eq!(
        json["data"]["proposal"]["status"]["status"],
        "open",
        "author self-approve must not advance status"
    );

    // 4. Bob approves — now Approved (min_reviewers = 1).
    let review_body =
        serde_json::json!({ "decision": "approve", "comment": "lgtm" }).to_string();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/proposals/{proposal_id}/reviews"))
                .header("content-type", "application/json")
                .header("x-thinkingroot-user", "bob")
                .body(Body::from(review_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = read_json(response).await;
    assert_eq!(
        json["data"]["proposal"]["status"]["status"],
        "approved",
        "non-author approve at min_reviewers=1 must reach Approved"
    );

    // 5. Closing as a non-author is forbidden.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/proposals/{proposal_id}/close"))
                .header("x-thinkingroot-user", "mallory")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // 6. Closing as the author works.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/proposals/{proposal_id}/close"))
                .header("x-thinkingroot-user", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = read_json(response).await;
    assert_eq!(json["data"]["proposal"]["status"]["status"], "closed");

    // 7. The TOML actually exists on disk under the workspace.
    let toml_path = root
        .join(".thinkingroot-refs")
        .join("proposals")
        .join(format!("{proposal_id}.toml"));
    assert!(
        toml_path.exists(),
        "proposal TOML must be written under workspace_root"
    );
}

#[tokio::test]
async fn get_proposal_404_for_unknown_id_shape() {
    let dir = tempdir().unwrap();
    let app = router_with_root(dir.path().to_path_buf());

    // Valid ULID shape but not on disk.
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/proposals/01HZN12345ABCDEFGHJKMNPQRS")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = read_json(response).await;
    assert_eq!(json["error"]["code"], "PROPOSAL_NOT_FOUND");
}

#[tokio::test]
async fn get_proposal_400_for_invalid_id_shape() {
    let dir = tempdir().unwrap();
    let app = router_with_root(dir.path().to_path_buf());

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/proposals/not-a-ulid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = read_json(response).await;
    assert_eq!(json["error"]["code"], "PROPOSAL_READ_FAILED");
}
