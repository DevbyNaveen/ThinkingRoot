//! T1.6 — Live SSE branch-event stream.
//!
//! Two integration tests:
//!
//! 1. `branch_event_sender_returns_same_channel_for_same_branch` —
//!    pins the lazy-create-and-reuse contract.  Two calls with the
//!    same branch name must return senders pointing to the same
//!    underlying channel so a subscriber attached after the first
//!    event still picks up the second.
//!
//! 2. `publish_latest_branch_event_delivers_to_live_subscriber` —
//!    end-to-end through `AppState::publish_latest_branch_event`:
//!    creates a branch on disk (which appends a `Created` audit
//!    event), subscribes to the channel, then triggers a publish —
//!    the subscriber must receive the same `BranchEvent`.

use std::path::PathBuf;
use std::time::Duration;
use tempfile::tempdir;

use thinkingroot_core::{BranchEvent, BranchKind, BranchPermissions, MergePolicy};
use thinkingroot_serve::engine::QueryEngine;
use thinkingroot_serve::rest::AppState;

async fn build_state_with_root(root: PathBuf) -> std::sync::Arc<AppState> {
    let engine = QueryEngine::new();
    AppState::new_with_root(engine, None, Some(root))
}

#[tokio::test]
async fn branch_event_sender_returns_same_channel_for_same_branch() {
    let dir = tempdir().unwrap();
    let state = build_state_with_root(dir.path().to_path_buf()).await;

    // Two calls with the same branch name must map to the same
    // channel (otherwise a subscriber that attached before the first
    // publish would miss every later publish).
    let tx1 = state.branch_event_sender("feature/x").await;
    let tx2 = state.branch_event_sender("feature/x").await;

    let mut rx = tx1.subscribe();
    let event = BranchEvent::Abandoned {
        at: chrono::Utc::now(),
        actor: "test".into(),
    };
    tx2.send(event.clone()).expect("send should reach rx");
    let received = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("recv timeout")
        .expect("recv error");
    assert_eq!(received, event);

    // Distinct branches get distinct channels — events for "foo"
    // must not leak into the "bar" stream.
    let tx_bar = state.branch_event_sender("feature/bar").await;
    let mut rx_bar = tx_bar.subscribe();
    tx1.send(BranchEvent::Abandoned {
        at: chrono::Utc::now(),
        actor: "x".into(),
    })
    .ok();
    let res = tokio::time::timeout(Duration::from_millis(200), rx_bar.recv()).await;
    assert!(
        res.is_err(),
        "events on feature/x must not deliver to feature/bar subscribers"
    );
}

#[tokio::test]
async fn publish_latest_branch_event_delivers_to_live_subscriber() {
    use thinkingroot_serve::rest::publish_latest_branch_event;

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let graph_dir = root.join(".thinkingroot").join("graph");
    std::fs::create_dir_all(&graph_dir).unwrap();
    {
        // Init the main graph so create_branch_full's snapshot copy succeeds.
        let _g = thinkingroot_graph::graph::GraphStore::init(&graph_dir).unwrap();
    }

    // Creating a branch through the public helper appends a
    // `BranchEvent::Created` to the new branch's events log on disk.
    let branch_name = "feature/sse";
    thinkingroot_branch::create_branch_full(
        &root,
        branch_name,
        "main",
        Some("ssetest".into()),
        Some("alice".into()),
        BranchPermissions::default(),
        BranchKind::Feature,
        MergePolicy::Manual,
        None,
    )
    .await
    .expect("create_branch_full");

    let state = build_state_with_root(root).await;

    // Subscribe BEFORE publishing so we exercise the live path.
    let tx = state.branch_event_sender(branch_name).await;
    let mut rx = tx.subscribe();

    publish_latest_branch_event(&state, branch_name).await;

    let received = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("recv timeout")
        .expect("recv error");
    match received {
        BranchEvent::Created { actor, parent, .. } => {
            assert_eq!(actor, "alice");
            assert_eq!(parent, "main");
        }
        other => panic!("expected Created, got {other:?}"),
    }

    // Republishing without an intervening mutation must redeliver
    // the SAME latest event — the function is idempotent against the
    // on-disk registry, never invents events.
    let mut rx2 = tx.subscribe();
    publish_latest_branch_event(&state, branch_name).await;
    let again = tokio::time::timeout(Duration::from_secs(1), rx2.recv())
        .await
        .expect("recv timeout")
        .expect("recv error");
    assert!(matches!(again, BranchEvent::Created { .. }));
}

#[tokio::test]
async fn publish_latest_branch_event_is_no_op_when_branch_missing() {
    // Defensive: requesting a publish for a branch that does not
    // exist on disk must not panic, must not leak events into other
    // subscribers, and must return cleanly.
    use thinkingroot_serve::rest::publish_latest_branch_event;

    let dir = tempdir().unwrap();
    let state = build_state_with_root(dir.path().to_path_buf()).await;

    let tx = state.branch_event_sender("never/exists").await;
    let mut rx = tx.subscribe();
    publish_latest_branch_event(&state, "never/exists").await;
    let res = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(res.is_err(), "no event should be published for unknown branch");
}
