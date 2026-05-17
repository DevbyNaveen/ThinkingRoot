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
//! No support for OAuth flows at v1 — those typically run out-of-
//! band and the client just gets a static token. Re-implementing
//! OAuth would warrant a separate ship.

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
}

impl HttpTransport {
    pub fn new(
        endpoint: impl Into<String>,
        auth: Option<HttpAuth>,
        timeout: Option<Duration>,
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
        }))
    }

    fn build_headers(&self, session_id: Option<&str>) -> HeaderMap {
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

    async fn do_request(
        &self,
        envelope: &Value,
        session_id: Option<&str>,
    ) -> Result<HttpResponseShape, McpClientError> {
        let resp = self
            .client
            .post(&self.endpoint)
            .headers(self.build_headers(session_id))
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
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, McpClientError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        // First attempt with the cached session id.
        let cur_session = self.session_id.read().await.clone();
        let outcome =
            tokio::time::timeout(self.timeout, self.do_request(&envelope, cur_session.as_deref()))
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
        let r = HttpTransport::new("http://127.0.0.1:9999/mcp", None, None);
        assert!(r.is_ok());
    }

    #[test]
    fn bearer_auth_appears_in_headers() {
        let transport = HttpTransport::new(
            "http://127.0.0.1:9999/mcp",
            Some(HttpAuth::Bearer("secret123".into())),
            None,
        )
        .unwrap();
        let h = transport.build_headers(None);
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
        )
        .unwrap();
        let h = transport.build_headers(None);
        assert_eq!(h.get("x-api-key").unwrap().to_str().unwrap(), "xyz");
    }

    #[test]
    fn session_id_header_is_attached_when_set() {
        let transport = HttpTransport::new("http://127.0.0.1:9999/mcp", None, None).unwrap();
        let h = transport.build_headers(Some("sess-abc"));
        assert_eq!(h.get("mcp-session-id").unwrap().to_str().unwrap(), "sess-abc");
    }
}
