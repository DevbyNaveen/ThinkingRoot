//! End-to-end test: configure a CloudManaged LlmClient + fake_cloud,
//! stream a chat completion, assert content + credit-balance update.
//!
//! This integration test exercises the full path:
//!   Provider::CloudManaged → send_openai_compat → fake_cloud
//!   → parse_managed_headers → persist_managed_headers → auth.json.
//!
//! All four scenarios run real against fake_cloud once Task 9 wires
//! the `"thinkingroot-cloud"` arm into `LlmClient::new`.
//!
//! Spec: docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md
//! §6.2, §6.5.

#[path = "../../thinkingroot-cloud-auth/tests/fake_cloud.rs"]
mod fake_cloud;

use fake_cloud::{FakeCloud, FakeCloudConfig};

use thinkingroot_core::config::{LlmConfig, ProvidersConfig};

/// Acquire the workspace-wide env guard and point HOME / XDG_CONFIG_HOME
/// / APPDATA at a fresh tempdir so each test isolates its auth.json.
fn use_temp_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    let guard = thinkingroot_core::test_util::ENV_GUARD
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: ENV_GUARD held for the test's scope serialises this
    // mutation across every other env-mutating test in the workspace.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("APPDATA", tmp.path());
    }
    (tmp, guard)
}

fn seed_auth_json(server: &str) {
    let mut cfg = thinkingroot_cloud_auth::config::Config::empty();
    cfg.token = Some("test-token".into());
    cfg.server = server.into();
    cfg.tier = Some("pro".into());
    cfg.credits_remaining = Some(50_000);
    cfg.credits_total = Some(50_000);
    thinkingroot_cloud_auth::config::save(&cfg).expect("seed auth.json");
}

fn cloud_llm_config(model: &str) -> LlmConfig {
    LlmConfig {
        default_provider: "thinkingroot-cloud".into(),
        extraction_model: model.into(),
        compilation_model: model.into(),
        max_concurrent_requests: 1,
        request_timeout_secs: 30,
        providers: ProvidersConfig::default(),
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn cloud_managed_chat_completion_streams_and_updates_credits() {
    let (_home, _guard) = use_temp_home();
    let fake = FakeCloud::spawn(FakeCloudConfig {
        credits_remaining_after_completion: 49_975,
        credits_total_after_completion: 50_000,
        ..Default::default()
    })
    .await;
    seed_auth_json(&fake.uri);

    let client = thinkingroot_extract::llm::LlmClient::new(&cloud_llm_config("claude-opus-4-7"))
        .await
        .expect("LlmClient::new should accept thinkingroot-cloud after Task 9");

    // fake_cloud's success path emits SSE deltas with "Hello " / "world!".
    // We exercise the streaming surface because that's what fake_cloud
    // serves on 200; the non-stream `chat()` path expects a JSON envelope
    // that fake_cloud does not produce.
    use futures::StreamExt;
    let mut stream = client
        .chat_stream("you are a helpful assistant", "say hi")
        .await
        .expect("cloud chat_stream ok");
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("stream chunk ok");
        text.push_str(&chunk.text);
        if chunk.finish.is_some() {
            break;
        }
    }
    assert!(text.contains("Hello"), "stream should include the canned text, got `{text}`");

    let cfg = thinkingroot_cloud_auth::config::load()
        .expect("load")
        .expect("config exists");
    assert_eq!(cfg.credits_remaining, Some(49_975));
    assert_eq!(cfg.credits_total, Some(50_000));

    fake.shutdown();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn cloud_managed_402_surfaces_credits_exhausted() {
    let (_home, _guard) = use_temp_home();
    let fake = FakeCloud::spawn(FakeCloudConfig {
        completion_status: Some(402),
        completion_body: Some(
            r#"{"error":"credits_exhausted","needed":100,"remaining":5}"#.into(),
        ),
        ..Default::default()
    })
    .await;
    seed_auth_json(&fake.uri);

    let client = thinkingroot_extract::llm::LlmClient::new(&cloud_llm_config("claude-opus-4-7"))
        .await
        .expect("LlmClient::new should accept thinkingroot-cloud after Task 9");

    let err = client
        .chat("sys", "user")
        .await
        .expect_err("402 should surface as typed error");
    match err {
        thinkingroot_core::Error::CreditsExhausted {
            needed: 100,
            remaining: 5,
        } => {}
        other => panic!("expected CreditsExhausted{{100,5}}, got {other:?}"),
    }

    fake.shutdown();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn cloud_managed_401_surfaces_auth_expired() {
    let (_home, _guard) = use_temp_home();
    let fake = FakeCloud::spawn(FakeCloudConfig {
        completion_status: Some(401),
        completion_body: Some(r#"{"error":"token_invalid"}"#.into()),
        ..Default::default()
    })
    .await;
    seed_auth_json(&fake.uri);

    let client = thinkingroot_extract::llm::LlmClient::new(&cloud_llm_config("claude-opus-4-7"))
        .await
        .expect("LlmClient::new should accept thinkingroot-cloud after Task 9");

    let err = client
        .chat("sys", "user")
        .await
        .expect_err("401 should surface as AuthExpired");
    match err {
        thinkingroot_core::Error::AuthExpired => {}
        other => panic!("expected AuthExpired, got {other:?}"),
    }

    fake.shutdown();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn cloud_managed_no_auth_json_surfaces_not_logged_in() {
    let (_home, _guard) = use_temp_home();
    // Deliberately do NOT seed auth.json — the user is signed out.
    let fake = FakeCloud::spawn(FakeCloudConfig::default()).await;

    let client = thinkingroot_extract::llm::LlmClient::new(&cloud_llm_config("claude-opus-4-7"))
        .await
        .expect("LlmClient::new should accept thinkingroot-cloud after Task 9");

    let err = client
        .chat("sys", "user")
        .await
        .expect_err("missing auth.json should surface as NotLoggedIn");
    match err {
        thinkingroot_core::Error::NotLoggedIn => {}
        other => panic!("expected NotLoggedIn, got {other:?}"),
    }

    fake.shutdown();
}
