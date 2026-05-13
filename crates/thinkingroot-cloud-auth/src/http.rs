//! Shared reqwest client + retry policy helpers.
//!
//! Spec: `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md` §8.5.

use std::time::Duration;

use reqwest::Response;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::CloudError;

/// Builds a reqwest client with sensible defaults — 120s overall
/// timeout, user-agent identifying the crate version, rustls-tls only.
pub fn client() -> Result<reqwest::Client, CloudError> {
    reqwest::Client::builder()
        .user_agent(format!("thinkingroot-cloud-auth/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(CloudError::Http)
}

/// GET a JSON body with bearer auth; retry policy applied via
/// `with_retry`.
pub async fn get_json<T: DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
) -> Result<T, CloudError> {
    with_retry(|| async {
        let resp = http
            .get(url)
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(CloudError::Http)?;
        handle_json(resp).await
    })
    .await
}

/// POST a JSON body with bearer auth.
pub async fn post_json<B: Serialize, T: DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
    body: &B,
) -> Result<T, CloudError> {
    with_retry(|| async {
        let resp = http
            .post(url)
            .bearer_auth(bearer)
            .json(body)
            .send()
            .await
            .map_err(CloudError::Http)?;
        handle_json(resp).await
    })
    .await
}

/// POST raw bytes with bearer auth + caller-supplied content-type.
///
/// No retry by design: the consumer is `root publish`'s large-body
/// archive upload (tar+zstd of the workspace). Retries would re-stream
/// tens of MiB; the operation is idempotent server-side (content
/// addressed by BLAKE3) but the wire cost is paid every attempt, so
/// we let the caller decide whether to re-run.
pub async fn post_bytes<T: DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<T, CloudError> {
    let resp = http
        .post(url)
        .bearer_auth(bearer)
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .map_err(CloudError::Http)?;
    handle_json(resp).await
}

/// Retry policy from spec §8.5:
/// - `CloudUnavailable`: up to 3 retries at 250ms / 1s / 4s.
/// - `RateLimited`: 1 retry respecting Retry-After (capped 30s).
/// - All others: surface immediately.
///
/// The operation may run up to 4 times total (initial + 3 retries
/// for `CloudUnavailable`).
pub async fn with_retry<F, Fut, T>(mut op: F) -> Result<T, CloudError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, CloudError>>,
{
    let backoffs = [
        Duration::from_millis(250),
        Duration::from_secs(1),
        Duration::from_secs(4),
    ];
    let mut last: CloudError;
    let mut rate_retry_used = false;

    // Initial attempt
    match op().await {
        Ok(v) => return Ok(v),
        Err(e) => last = e,
    }

    let mut backoff_idx = 0usize;
    loop {
        let should_continue = match &last {
            CloudError::CloudUnavailable { .. } => {
                if backoff_idx >= backoffs.len() {
                    false
                } else {
                    let b = backoffs[backoff_idx];
                    backoff_idx += 1;
                    tokio::time::sleep(b).await;
                    true
                }
            }
            CloudError::RateLimited { retry_after_secs } => {
                if rate_retry_used {
                    false
                } else {
                    rate_retry_used = true;
                    let wait = Duration::from_secs((*retry_after_secs as u64).min(30));
                    tokio::time::sleep(wait).await;
                    true
                }
            }
            _ => false,
        };
        if !should_continue {
            break;
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
    }

    Err(last)
}

/// Map an HTTP response to a typed result, applying status-→-CloudError
/// mapping for the well-known cloud error shapes.
pub async fn handle_json<T: DeserializeOwned>(resp: Response) -> Result<T, CloudError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());
    let body = resp.text().await.map_err(CloudError::Http)?;

    if status.is_success() {
        return serde_json::from_str::<T>(&body).map_err(CloudError::JsonParse);
    }

    Err(match status.as_u16() {
        401 | 403 => CloudError::AuthExpired,
        402 => parse_credits_exhausted(&body).unwrap_or(CloudError::HubReject {
            status: status.as_u16(),
            body,
        }),
        429 => CloudError::RateLimited {
            retry_after_secs: retry_after.unwrap_or(5),
        },
        500..=599 => CloudError::CloudUnavailable {
            last_status: status.as_u16(),
        },
        _ => CloudError::HubReject {
            status: status.as_u16(),
            body,
        },
    })
}

fn parse_credits_exhausted(body: &str) -> Option<CloudError> {
    #[derive(serde::Deserialize)]
    struct Body {
        error: String,
        needed: Option<u64>,
        remaining: Option<u64>,
    }
    let parsed: Body = serde_json::from_str(body).ok()?;
    if parsed.error == "credits_exhausted" {
        Some(CloudError::CreditsExhausted {
            needed: parsed.needed.unwrap_or(0),
            remaining: parsed.remaining.unwrap_or(0),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn parse_credits_exhausted_recognises_body() {
        let body = r#"{"error":"credits_exhausted","needed":100,"remaining":5}"#;
        match parse_credits_exhausted(body) {
            Some(CloudError::CreditsExhausted {
                needed: 100,
                remaining: 5,
            }) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_credits_exhausted_returns_none_for_other_errors() {
        let body = r#"{"error":"something_else"}"#;
        assert!(parse_credits_exhausted(body).is_none());
    }

    #[test]
    fn parse_credits_exhausted_returns_none_for_malformed_body() {
        let body = r#"not json at all"#;
        assert!(parse_credits_exhausted(body).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_after_one_5xx() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_for_op = calls.clone();
        let result: Result<u32, CloudError> = with_retry(move || {
            let calls = calls_for_op.clone();
            async move {
                let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt == 1 {
                    Err(CloudError::CloudUnavailable { last_status: 502 })
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_eventually_gives_up_on_5xx_after_3_attempts() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_for_op = calls.clone();
        let result: Result<u32, CloudError> = with_retry(move || {
            let calls = calls_for_op.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(CloudError::CloudUnavailable { last_status: 502 })
            }
        })
        .await;
        match result {
            Err(CloudError::CloudUnavailable { last_status: 502 }) => {}
            other => panic!("got {other:?}"),
        }
        // Initial + 3 retries = 4 total attempts.
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_rate_limited_uses_exactly_one_retry() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_for_op = calls.clone();
        let result: Result<u32, CloudError> = with_retry(move || {
            let calls = calls_for_op.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(CloudError::RateLimited { retry_after_secs: 1 })
            }
        })
        .await;
        match result {
            Err(CloudError::RateLimited { retry_after_secs: 1 }) => {}
            other => panic!("got {other:?}"),
        }
        // Initial + 1 retry = 2 total attempts (RateLimited only retries once).
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_auth_expired_surfaces_immediately() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_for_op = calls.clone();
        let result: Result<u32, CloudError> = with_retry(move || {
            let calls = calls_for_op.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(CloudError::AuthExpired)
            }
        })
        .await;
        match result {
            Err(CloudError::AuthExpired) => {}
            other => panic!("got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn client_builds_with_user_agent() {
        let _c = client().expect("client builds");
        // reqwest doesn't expose the configured user agent for inspection;
        // smoke test only — full coverage by integration tests.
    }
}
