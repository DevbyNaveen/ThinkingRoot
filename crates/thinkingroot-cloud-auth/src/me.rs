//! `/me` and `/credits/balance` callers.
//!
//! Spec: `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md` §5.6.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::CloudError;
use crate::http;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeResponse {
    pub user: MeUser,
    pub credit_period_end: DateTime<Utc>,
    pub token_expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeUser {
    pub id: String,
    pub handle: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub tier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreditsResponse {
    pub remaining: u64,
    pub total: u64,
    pub period_end: DateTime<Utc>,
}

pub async fn fetch_me(cfg: &Config) -> Result<MeResponse, CloudError> {
    let token = cfg.token.as_deref().ok_or(CloudError::NotLoggedIn)?;
    let http = http::client()?;
    let url = format!("{}/me", cfg.server.trim_end_matches('/'));
    http::get_json(&http, &url, token).await
}

pub async fn fetch_credits(cfg: &Config) -> Result<CreditsResponse, CloudError> {
    let token = cfg.token.as_deref().ok_or(CloudError::NotLoggedIn)?;
    let http = http::client()?;
    let url = format!("{}/credits/balance", cfg.server.trim_end_matches('/'));
    http::get_json(&http, &url, token).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg_with(server: String, token: &str) -> Config {
        let mut c = Config::empty();
        c.server = server;
        c.token = Some(token.into());
        c
    }

    #[tokio::test]
    async fn fetch_me_returns_typed_response() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "user": {
                "id": "user_01HXYZ",
                "handle": "naveen",
                "display_name": null,
                "tier": "pro"
            },
            "credit_period_end": "2026-06-13T00:00:00Z",
            "token_expires_at": "2026-08-11T00:00:00Z"
        });
        Mock::given(method("GET"))
            .and(path("/me"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let cfg = cfg_with(server.uri(), "test-token");
        let me = fetch_me(&cfg).await.unwrap();
        assert_eq!(me.user.handle, "naveen");
        assert_eq!(me.user.tier, "pro");
    }

    #[tokio::test]
    async fn fetch_me_401_maps_to_auth_expired() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/me"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "token_invalid"
            })))
            .mount(&server)
            .await;

        let cfg = cfg_with(server.uri(), "bad-token");
        match fetch_me(&cfg).await {
            Err(CloudError::AuthExpired) => {}
            other => panic!("expected AuthExpired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_me_without_token_errors_not_logged_in() {
        let cfg = Config::empty();
        match fetch_me(&cfg).await {
            Err(CloudError::NotLoggedIn) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_credits_returns_balance() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/credits/balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "remaining": 48153,
                "total": 50000,
                "period_end": "2026-06-13T00:00:00Z"
            })))
            .mount(&server)
            .await;

        let cfg = cfg_with(server.uri(), "test-token");
        let credits = fetch_credits(&cfg).await.unwrap();
        assert_eq!(credits.remaining, 48153);
        assert_eq!(credits.total, 50000);
    }
}
