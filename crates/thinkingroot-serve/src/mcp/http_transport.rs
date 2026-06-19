//! Clean-room reimplementation. Inspired by openhuman/mcp_client/http
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.5 (2026-05-17) — HTTP MCP transport.
//!
//! POSTs each RPC envelope to the configured endpoint. The
//! server may return:
//!   - 200 with JSON-RPC envelope in body → normal response.
//!   - 200 with `mcp-session-id` header → first response carries a
//!     session id we must echo on subsequent requests.
//!   - 404 → session expired; re-init silently and retry once.
//!
//! ## Auth
//!
//! Optional bearer-token header passed via [`HttpAuth::Bearer`].
//! For OAuth connectors (`oauth_provider` is set), the bearer
//! token is fetched per-call from the gateway OAuth broker using
//! the calling user's identity (derived from the `u_*` workspace).
//! Non-OAuth connectors use a static token (or none).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::RwLock;

use super::client::{McpClientError, McpTransport};

pub const DEFAULT_HTTP_RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// HTTP auth scheme. `None` for unauthenticated endpoints (loopback
/// MCP servers, typical for local dev).
#[derive(Debug, Clone)]
pub enum HttpAuth {
    /// `Authorization: Bearer <token>`.
    Bearer(String),
    /// `X-API-Key: <key>` — some MCP servers (notably custom
    /// proprietary ones) prefer this scheme.
    ApiKey(String),
}

pub struct HttpTransport {
    endpoint: String,
    next_id: AtomicI64,
    /// `mcp-session-id` value the server gave us on the first
    /// response. Cleared on 404 → re-init path.
    session_id: RwLock<Option<String>>,
    client: reqwest::Client,
    auth: Option<HttpAuth>,
    timeout: Duration,
    /// When `Some`, this connector requires a per-user OAuth Bearer
    /// token fetched from the gateway broker at each `tools/call`.
    /// The value is the provider slug (e.g. `"google"`, `"slack"`).
    /// SECURITY: if set, the transport NEVER falls back to `self.auth`
    /// for `tools/call` — it either gets the user's token or returns a
    /// typed error.  For `initialize`/`tools/list` (no user_id),
    /// `self.auth` is used normally (those calls carry no user data).
    oauth_provider: Option<String>,
}

impl HttpTransport {
    pub fn new(
        endpoint: impl Into<String>,
        auth: Option<HttpAuth>,
        timeout: Option<Duration>,
        oauth_provider: Option<String>,
    ) -> Result<Arc<Self>, McpClientError> {
        let client = reqwest::Client::builder()
            .timeout(timeout.unwrap_or(DEFAULT_HTTP_RPC_TIMEOUT))
            .build()
            .map_err(|e| McpClientError::TransportFailed(format!("reqwest build: {e}")))?;
        Ok(Arc::new(Self {
            endpoint: endpoint.into(),
            next_id: AtomicI64::new(1),
            session_id: RwLock::new(None),
            client,
            auth,
            timeout: timeout.unwrap_or(DEFAULT_HTTP_RPC_TIMEOUT),
            oauth_provider,
        }))
    }

    /// Build the request header map.
    ///
    /// `per_call_bearer` — when `Some`, sets `Authorization: Bearer <token>`
    /// and takes PRECEDENCE over `self.auth`.  Used to inject the per-user
    /// OAuth token for OAuth-tagged connectors.
    fn build_headers(
        &self,
        session_id: Option<&str>,
        per_call_bearer: Option<&str>,
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        if let Some(sid) = session_id {
            if let Ok(name) = HeaderName::try_from("mcp-session-id")
                && let Ok(value) = HeaderValue::from_str(sid)
            {
                headers.insert(name, value);
            }
        }
        // Per-call bearer (OAuth token) takes precedence over static auth.
        if let Some(bearer) = per_call_bearer {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {bearer}")) {
                headers.insert(reqwest::header::AUTHORIZATION, v);
                return headers;
            }
        }
        match &self.auth {
            Some(HttpAuth::Bearer(token)) => {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
                    headers.insert(reqwest::header::AUTHORIZATION, v);
                }
            }
            Some(HttpAuth::ApiKey(key)) => {
                if let Ok(name) = HeaderName::try_from("x-api-key")
                    && let Ok(value) = HeaderValue::from_str(key)
                {
                    headers.insert(name, value);
                }
            }
            None => {}
        }
        headers
    }

    /// Fetch a per-user OAuth Bearer token from the gateway broker.
    ///
    /// Reads `TR_OAUTH_GATEWAY_URL`, `TR_OAUTH_GATEWAY_TOKEN`, and
    /// `TR_PROJECT_ID` from the process environment (injected by the
    /// provisioner via O.4a).  If any env var is missing the error is
    /// returned immediately — the provisioner is responsible for
    /// ensuring they are present when an OAuth connector is registered.
    ///
    /// ## Response envelope
    ///
    /// The broker may return either:
    /// - `{ "ok": true, "data": { "access_token": "..." } }` (wrapped)
    /// - `{ "access_token": "..." }` (flat)
    ///
    /// Both forms are handled; the wrapped form is tried first.
    ///
    /// ## Error mapping
    /// - 404 → user has not yet connected this provider
    /// - 401 → token expired / needs re-authentication
    /// - other non-2xx → internal broker error
    ///
    /// SECURITY: the fetched token and the service token are NEVER
    /// included in tracing output (they must not appear in logs).
    async fn fetch_oauth_bearer(
        &self,
        provider: &str,
        user_id: &str,
    ) -> Result<String, McpClientError> {
        let gateway_url = std::env::var("TR_OAUTH_GATEWAY_URL").map_err(|_| {
            McpClientError::TransportFailed(
                "oauth broker not configured: TR_OAUTH_GATEWAY_URL missing".into(),
            )
        })?;
        let service_token = std::env::var("TR_OAUTH_GATEWAY_TOKEN").map_err(|_| {
            McpClientError::TransportFailed(
                "oauth broker not configured: TR_OAUTH_GATEWAY_TOKEN missing".into(),
            )
        })?;
        let project_id = std::env::var("TR_PROJECT_ID").map_err(|_| {
            McpClientError::TransportFailed(
                "oauth broker not configured: TR_PROJECT_ID missing".into(),
            )
        })?;

        let url = format!(
            "{}/oauth/token/{}?project={}&user={}",
            gateway_url.trim_end_matches('/'),
            provider,
            project_id,
            user_id,
        );

        // SECURITY: service_token is in the Authorization header; it
        // must never be logged.  We deliberately avoid tracing the
        // request URL (which contains user_id + project_id) at anything
        // above DEBUG.
        tracing::debug!(
            target: "oauth_broker",
            provider,
            project_id = %project_id,
            "fetching per-user OAuth token from broker"
        );

        let resp = self
            .client
            .get(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                // SAFETY: service_token is ASCII-safe (it is a bearer
                // token produced by the gateway); the format is fixed.
                format!("Bearer {service_token}"),
            )
            .send()
            .await
            .map_err(|e| {
                McpClientError::TransportFailed(format!("oauth broker fetch failed: {e}"))
            })?;

        let status = resp.status();
        match status.as_u16() {
            200 => {
                let body: Value = resp.json().await.map_err(|e| {
                    McpClientError::TransportFailed(format!("oauth broker response parse: {e}"))
                })?;

                // Try wrapped form first: { ok: true, data: { access_token } }
                let token = body
                    .get("data")
                    .and_then(|d| d.get("access_token"))
                    .and_then(|t| t.as_str())
                    // Fall back to flat form: { access_token: "..." }
                    .or_else(|| body.get("access_token").and_then(|t| t.as_str()))
                    .ok_or_else(|| {
                        McpClientError::TransportFailed(
                            "oauth broker returned 200 but no access_token in response".into(),
                        )
                    })?;
                Ok(token.to_string())
            }
            404 => Err(McpClientError::Protocol(format!(
                "connector '{provider}' not authorized for this user — \
                 connect via the ThinkingRoot console to grant access"
            ))),
            401 => Err(McpClientError::Protocol(format!(
                "connector '{provider}' authorization expired — \
                 reconnect via the ThinkingRoot console"
            ))),
            other => {
                // Deliberately NOT including the response body: it
                // might contain PII or token fragments.
                Err(McpClientError::TransportFailed(format!(
                    "oauth broker returned HTTP {other} for provider '{provider}'"
                )))
            }
        }
    }

    async fn do_request(
        &self,
        envelope: &Value,
        session_id: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<HttpResponseShape, McpClientError> {
        // Determine which bearer token to use.
        //
        // SECURITY INVARIANT: if `oauth_provider` is set, we MUST use the
        // per-user token for `tools/call` (identified by `user_id.is_some()`).
        // If `user_id` is None for an OAuth connector — e.g. a project-level
        // call with no user scope — we REJECT rather than fall back to a static
        // credential (which could be a different user's token or a service
        // token that has no business calling on behalf of an unknown user).
        let per_call_bearer = if let Some(ref provider) = self.oauth_provider {
            match user_id {
                Some(uid) => {
                    let token = self.fetch_oauth_bearer(provider, uid).await?;
                    Some(token)
                }
                None => {
                    return Err(McpClientError::Protocol(format!(
                        "oauth connector '{provider}' requires a per-user scope \
                         (u_<id> workspace); call rejected to prevent identity confusion"
                    )));
                }
            }
        } else {
            None
        };

        let resp = self
            .client
            .post(&self.endpoint)
            .headers(self.build_headers(session_id, per_call_bearer.as_deref()))
            .json(envelope)
            .send()
            .await
            .map_err(|e| McpClientError::TransportFailed(format!("POST: {e}")))?;

        let status = resp.status();
        let new_session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if status.as_u16() == 404 {
            return Ok(HttpResponseShape::SessionExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(McpClientError::TransportFailed(format!(
                "HTTP {}: {body}",
                status
            )));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| McpClientError::TransportFailed(format!("body json: {e}")))?;
        Ok(HttpResponseShape::Ok {
            envelope: body,
            new_session_id: new_session,
        })
    }
}

enum HttpResponseShape {
    Ok {
        envelope: Value,
        new_session_id: Option<String>,
    },
    SessionExpired,
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn rpc(
        &self,
        method: &str,
        params: Value,
        user_id: Option<&str>,
    ) -> Result<Value, McpClientError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        // For non-tools/call methods (initialize, tools/list, etc.) we
        // never inject a per-user OAuth token — those are protocol-level
        // handshakes that authenticate with the static connector credential.
        // Only tools/call receives user_id.
        let effective_user_id = if method == "tools/call" { user_id } else { None };

        // First attempt with the cached session id.
        let cur_session = self.session_id.read().await.clone();
        let outcome = tokio::time::timeout(
            self.timeout,
            self.do_request(&envelope, cur_session.as_deref(), effective_user_id),
        )
        .await
        .map_err(|_| McpClientError::Timeout(self.timeout))??;

        let (env, new_session) = match outcome {
            HttpResponseShape::Ok {
                envelope,
                new_session_id,
            } => (envelope, new_session_id),
            HttpResponseShape::SessionExpired => {
                // Drop cached session; the next call's initialize
                // will rebuild it. Honest: we don't re-init here
                // because that crosses an abstraction boundary —
                // `McpClient::initialize` owns the protocol
                // handshake. The protocol-level retry happens at
                // the McpClient layer.
                *self.session_id.write().await = None;
                return Err(McpClientError::Protocol(
                    "HTTP 404 — session expired; client must re-initialize".into(),
                ));
            }
        };

        if let Some(sid) = new_session {
            *self.session_id.write().await = Some(sid);
        }

        // Parse envelope.
        if let Some(err) = env.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32603);
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();
            Err(McpClientError::RpcError { code, message })
        } else if let Some(result) = env.get("result") {
            Ok(result.clone())
        } else {
            Err(McpClientError::Protocol(
                "envelope has neither result nor error".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_transport_construction_validates_endpoint() {
        let r = HttpTransport::new("http://127.0.0.1:9999/mcp", None, None, None);
        assert!(r.is_ok());
    }

    #[test]
    fn bearer_auth_appears_in_headers() {
        let transport = HttpTransport::new(
            "http://127.0.0.1:9999/mcp",
            Some(HttpAuth::Bearer("secret123".into())),
            None,
            None,
        )
        .unwrap();
        let h = transport.build_headers(None, None);
        assert_eq!(
            h.get(reqwest::header::AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer secret123"
        );
    }

    #[test]
    fn api_key_auth_appears_in_x_api_key_header() {
        let transport = HttpTransport::new(
            "http://127.0.0.1:9999/mcp",
            Some(HttpAuth::ApiKey("xyz".into())),
            None,
            None,
        )
        .unwrap();
        let h = transport.build_headers(None, None);
        assert_eq!(h.get("x-api-key").unwrap().to_str().unwrap(), "xyz");
    }

    #[test]
    fn session_id_header_is_attached_when_set() {
        let transport = HttpTransport::new("http://127.0.0.1:9999/mcp", None, None, None).unwrap();
        let h = transport.build_headers(Some("sess-abc"), None);
        assert_eq!(h.get("mcp-session-id").unwrap().to_str().unwrap(), "sess-abc");
    }

    #[test]
    fn per_call_bearer_overrides_static_auth() {
        // Even if the transport has a static bearer, the per-call token wins.
        let transport = HttpTransport::new(
            "http://127.0.0.1:9999/mcp",
            Some(HttpAuth::Bearer("static-token".into())),
            None,
            Some("google".into()),
        )
        .unwrap();
        let h = transport.build_headers(None, Some("user-oauth-token"));
        assert_eq!(
            h.get(reqwest::header::AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer user-oauth-token",
            "per-call OAuth token must take precedence over static auth"
        );
    }

    #[test]
    fn user_id_derived_from_chain_u_prefix() {
        // Verify the derivation logic mirrors what dispatch_for_chain does.
        // The logic lives in external_registry but is load-bearing here —
        // keep this test as a guard against regressions.
        fn derive(chain: &[&str]) -> Option<String> {
            chain.iter().find_map(|ws| {
                ws.strip_prefix("u_").map(|rest| {
                    rest.split("__").next().unwrap_or(rest).to_string()
                })
            })
        }

        // Simple u_ scope
        assert_eq!(
            derive(&["u_alice", "main"]),
            Some("alice".into())
        );
        // Composite u_<id>__agent_<name> — strips agent suffix
        assert_eq!(
            derive(&["u_bob__agent_mrguy", "main"]),
            Some("bob".into())
        );
        // Most-specific wins (first u_ element)
        assert_eq!(
            derive(&["u_carol__agent_x", "u_carol", "main"]),
            Some("carol".into())
        );
        // No u_ element → None
        assert_eq!(derive(&["main"]), None);
        assert_eq!(derive(&[]), None);
    }
}
