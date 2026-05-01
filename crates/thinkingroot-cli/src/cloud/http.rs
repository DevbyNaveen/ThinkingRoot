//! Shared `reqwest` client + JSON helpers for cloud subcommands.
//!
//! All public HTTP errors surface the upstream status code and body so
//! the user sees the actual server message instead of "request
//! failed". The CLI does not retry on its own — Stripe-style
//! idempotency keys aren't available on every endpoint, so retry
//! policy is a per-command decision.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("root-cli/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .build()
        .context("reqwest client")
}

pub async fn get_json<T: DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
) -> Result<T> {
    let resp = http
        .get(url)
        .bearer_auth(bearer)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    handle_json(resp, url).await
}

pub async fn post_json<B: Serialize, T: DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
    body: &B,
) -> Result<T> {
    let resp = http
        .post(url)
        .bearer_auth(bearer)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    handle_json(resp, url).await
}

pub async fn post_bytes<T: DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<T> {
    let resp = http
        .post(url)
        .bearer_auth(bearer)
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    handle_json(resp, url).await
}

async fn handle_json<T: DeserializeOwned>(resp: reqwest::Response, url: &str) -> Result<T> {
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("read body from {url}"))?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes).to_string();
        let msg = parse_error_message(&text).unwrap_or(text);
        return Err(anyhow!("{status} from {url}: {msg}"));
    }
    serde_json::from_slice::<T>(&bytes).with_context(|| format!("parse JSON from {url}"))
}

/// Try to extract `{ "error": { "message": "…" } }` from upstream
/// error bodies. Falls back to the raw body on parse failure.
fn parse_error_message(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    v.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}
