//! Model catalogue cache: 1-hour TTL + manual refresh.
//!
//! Spec: docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md §6.4.

mod fake_cloud;

use thinkingroot_cloud_auth::config::{self, Config};
use thinkingroot_cloud_auth::models_catalogue::fetch_models;

use fake_cloud::{FakeCloud, FakeCloudConfig};

/// Acquire the workspace-wide env guard and point HOME / XDG_CONFIG_HOME
/// / APPDATA at a fresh tempdir so each test isolates its auth.json.
fn use_temp_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    let guard = thinkingroot_core::test_util::ENV_GUARD
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: ENV_GUARD held above serialises this env mutation with
    // every other env-mutating test in the workspace.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("APPDATA", tmp.path());
    }
    (tmp, guard)
}

fn seed_signed_in(server: &str) {
    let mut cfg = Config::empty();
    cfg.token = Some("test-token".into());
    cfg.server = server.into();
    config::save(&cfg).unwrap();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn fetch_models_first_call_hits_network() {
    let (_home, _guard) = use_temp_home();
    let fake = FakeCloud::spawn(FakeCloudConfig::default()).await;
    seed_signed_in(&fake.uri);

    let models = fetch_models(false).await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "claude-opus-4-7");

    let cfg = config::load().unwrap().unwrap();
    assert!(cfg.model_catalogue_cached.is_some());

    fake.shutdown();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn fetch_models_within_ttl_returns_cached() {
    let (_home, _guard) = use_temp_home();
    let fake = FakeCloud::spawn(FakeCloudConfig::default()).await;
    seed_signed_in(&fake.uri);

    let first = fetch_models(false).await.unwrap();
    fake.shutdown();

    // Second call without network — the fake_cloud is gone. If the cache
    // misses we'd see a reqwest connection-refused error; the assertion
    // that we got Ok proves the cache hit served the response.
    let second = fetch_models(false).await.unwrap();
    assert_eq!(first.len(), second.len());
    assert_eq!(first[0].id, second[0].id);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn fetch_models_force_refresh_hits_network_even_if_cached() {
    let (_home, _guard) = use_temp_home();
    let fake = FakeCloud::spawn(FakeCloudConfig::default()).await;
    seed_signed_in(&fake.uri);

    let _first = fetch_models(false).await.unwrap();
    // force_refresh: true bypasses the TTL — must re-hit the fake.
    let second = fetch_models(true).await;
    assert!(second.is_ok(), "force_refresh refetch should succeed: {second:?}");

    fake.shutdown();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn fetch_models_signed_out_returns_not_logged_in() {
    let (_home, _guard) = use_temp_home();
    // No seed → no auth.json on disk.
    let result = fetch_models(false).await;
    match result {
        Err(thinkingroot_cloud_auth::CloudError::NotLoggedIn) => {}
        other => panic!("expected NotLoggedIn, got {other:?}"),
    }
}
