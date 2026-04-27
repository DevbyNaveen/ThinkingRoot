//! End-to-end smoke test for `LlmClient::chat_stream`.
//!
//! `anthropic_real_sse_streams_multiple_chunks` is gated on
//! `ANTHROPIC_API_KEY`. It hits the real Anthropic Messages API with
//! `stream:true` and verifies we receive ≥2 text chunks plus a final
//! `ChatFinish`. Skipped silently when the env var is empty / unset
//! — matches the project-wide pattern for cost-bearing live tests
//! (see `crates/thinkingroot-cli/tests/pack_cmd.rs` for the Phase B
//! registry round-trips that follow the same convention).
//!
//! Run: `ANTHROPIC_API_KEY=... cargo test -p thinkingroot-extract --tests anthropic_real_sse`

use futures::StreamExt;
use thinkingroot_core::config::{LlmConfig, ProviderConfig, ProvidersConfig};
use thinkingroot_extract::llm::LlmClient;

#[tokio::test]
async fn anthropic_real_sse_streams_multiple_chunks() {
    let key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!(
                "[skip] ANTHROPIC_API_KEY not set — anthropic streaming \
                 smoke test requires a live key. Re-run with the env var \
                 set to exercise the real /v1/messages?stream=true path."
            );
            return;
        }
    };

    let cfg = LlmConfig {
        default_provider: "anthropic".into(),
        extraction_model: "claude-haiku-4-5-20251001".into(),
        compilation_model: "claude-haiku-4-5-20251001".into(),
        providers: ProvidersConfig {
            anthropic: Some(ProviderConfig {
                api_key: Some(key),
                api_key_env: None,
                base_url: None,
                default_model: None,
            }),
            ..Default::default()
        },
        ..Default::default()
    };

    let client = LlmClient::new(&cfg).await.expect("build LlmClient");
    let mut stream = client
        .chat_stream(
            "You are a friendly assistant. Reply with exactly one short sentence.",
            "Say 'hello world' twice in a row.",
        )
        .await
        .expect("open stream");

    let mut text_chunks = 0usize;
    let mut got_finish = false;
    let mut accumulated = String::new();

    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk should not be Err");
        if !chunk.text.is_empty() {
            text_chunks += 1;
            accumulated.push_str(&chunk.text);
        }
        if chunk.finish.is_some() {
            got_finish = true;
        }
    }

    assert!(
        text_chunks >= 2,
        "expected at least 2 text chunks from Anthropic SSE, got {text_chunks}; body so far: {accumulated:?}",
    );
    assert!(got_finish, "expected a final ChatFinish chunk");
    assert!(
        !accumulated.trim().is_empty(),
        "accumulated body must not be empty",
    );
}
