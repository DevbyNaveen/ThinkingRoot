//! End-to-end smoke tests for `LlmClient::chat_stream`. Both tests
//! are gated on env vars per the project-wide live-test convention
//! (skipped silently when unset, hit the real provider when set).
//!
//! - `anthropic_real_sse_streams_multiple_chunks` — gated on
//!   `ANTHROPIC_API_KEY`. Exercises the Anthropic
//!   `/v1/messages?stream=true` parser.
//! - `azure_real_sse_streams_multiple_chunks` — gated on
//!   `AZURE_OPENAI_API_KEY`. Exercises the Azure
//!   `/openai/deployments/{d}/chat/completions?stream=true` parser
//!   with `requires_max_completion_tokens` honored for the GPT-5.x
//!   family. Defaults match the workspace config at
//!   `.thinkingroot/config.toml` (resource `openai-gpt-mini`,
//!   deployment `gpt-5.4`); override via `AZURE_OPENAI_RESOURCE`,
//!   `AZURE_OPENAI_DEPLOYMENT`, `AZURE_OPENAI_API_VERSION` env vars.
//!
//! Run all (gated tests skip when unset):
//!   cargo test -p thinkingroot-extract --tests
//! Or explicitly:
//!   AZURE_OPENAI_API_KEY=... cargo test -p thinkingroot-extract \
//!     --test streaming_smoke -- --nocapture

use futures::StreamExt;
use thinkingroot_core::config::{AzureConfig, LlmConfig, ProviderConfig, ProvidersConfig};
use thinkingroot_extract::llm::LlmClient;

#[tokio::test]
#[ignore = "live API call — requires ANTHROPIC_API_KEY; run with `cargo test -- --ignored`"]
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

#[tokio::test]
#[ignore = "live API call — requires AZURE_OPENAI_API_KEY; run with `cargo test -- --ignored`"]
async fn azure_real_sse_streams_multiple_chunks() {
    let key = match std::env::var("AZURE_OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!(
                "[skip] AZURE_OPENAI_API_KEY not set — azure streaming smoke \
                 test requires a live key. Re-run with the env var set to \
                 exercise the real /openai/deployments/{{d}}/chat/completions \
                 ?stream=true path."
            );
            return;
        }
    };

    let resource = std::env::var("AZURE_OPENAI_RESOURCE")
        .unwrap_or_else(|_| "openai-gpt-mini".to_string());
    let deployment = std::env::var("AZURE_OPENAI_DEPLOYMENT")
        .unwrap_or_else(|_| "gpt-5.4".to_string());
    let api_version = std::env::var("AZURE_OPENAI_API_VERSION")
        .unwrap_or_else(|_| "2025-01-01-preview".to_string());

    let cfg = LlmConfig {
        default_provider: "azure".into(),
        // GPT-5.x triggers `requires_max_completion_tokens`; the
        // streaming impl uses that check to send the right body
        // shape, so the model name must keep its `gpt-5` prefix.
        extraction_model: deployment.clone(),
        compilation_model: deployment.clone(),
        providers: ProvidersConfig {
            azure: Some(AzureConfig {
                resource_name: Some(resource),
                endpoint_base: None,
                deployment: Some(deployment.clone()),
                api_version: Some(api_version),
                api_key_env: None,
                api_key: Some(key),
            }),
            ..Default::default()
        },
        ..Default::default()
    };

    let client = LlmClient::new(&cfg).await.expect("build LlmClient");
    let mut stream = client
        .chat_stream(
            "You are a friendly assistant. Reply concisely.",
            "Count from 1 to 5, one number per line.",
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
        "expected at least 2 text chunks from Azure SSE deployment '{deployment}', got {text_chunks}; body: {accumulated:?}",
    );
    assert!(got_finish, "expected a final ChatFinish chunk");
    assert!(
        !accumulated.trim().is_empty(),
        "accumulated body must not be empty",
    );
}
