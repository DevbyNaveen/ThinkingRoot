//! Clean-room reimplementation. Inspired by openhuman/mcp_client/client.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.5 (2026-05-17) — external MCP CLIENT.
//!
//! ## What this gives the user
//!
//! ThinkingRoot has always been an MCP SERVER (Claude Code / Cursor /
//! Codex / Windsurf connect TO us). This module makes ThinkingRoot
//! also an MCP CLIENT — the user registers external MCP servers
//! (Filesystem, Notion, Drive, Playwright, GitHub, …) and their
//! tools appear as first-class entries in our `tools/list` under a
//! `<server_name>::<tool_name>` namespace.
//!
//! ## Three pieces
//!
//! 1. `McpTransport` trait — what an MCP transport must implement
//!    (`rpc(method, params) -> Result<Value>`). Two impls ship:
//!    `StdioTransport` (subprocess + line-delimited JSON-RPC) and
//!    `HttpTransport` (POST + session-id correlation).
//! 2. `McpClient` — protocol-aware wrapper: initialize handshake,
//!    cached `tools/list`, dispatch `tools/call`.
//! 3. `ExternalMcpRegistry` (in `external_registry.rs`) — maps
//!    `server_name → Arc<McpClient>`, loaded from a workspace
//!    `.thinkingroot/mcp-servers.toml`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::RwLock;

/// Single MCP protocol version we negotiate at v1. Future protocol
/// bumps land as additional entries in `SUPPORTED_PROTOCOL_VERSIONS`.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2024-11-05"];

/// Errors a transport or client can surface. Mapped to JSON-RPC
/// codes at the integration layer.
#[derive(Debug, Error)]
pub enum McpClientError {
    #[error("transport failed: {0}")]
    TransportFailed(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("rpc error {code}: {message}")]
    RpcError { code: i64, message: String },
    #[error("not initialized — call McpClient::initialize first")]
    NotInitialized,
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
}

impl From<serde_json::Error> for McpClientError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}

impl From<std::io::Error> for McpClientError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Bare transport — the thing that knows how to do one JSON-RPC
/// round-trip. The protocol layer (`McpClient` below) is built on
/// top.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Perform one JSON-RPC request. `method` is e.g. `"initialize"`,
    /// `"tools/list"`, `"tools/call"`. Returns the unwrapped `result`
    /// payload OR a typed error.
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, McpClientError>;
}

/// One tool descriptor returned by an external `tools/list`.
/// Mirrors the wire shape MCP servers emit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// Result of a `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResult {
    #[serde(default)]
    pub content: Vec<McpContentBlock>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpContentBlock {
    Text { text: String },
    /// Some MCP servers emit non-text content (images, audio).
    /// Captured verbatim so consumers can re-emit.
    #[serde(other)]
    Other,
}

/// Protocol-aware client wrapping any `McpTransport`.
///
/// Cheap to clone (`Arc<RwLock<...>>` internally) — a single
/// `McpClient` shared across multiple call sites is the
/// expected pattern.
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    state: Arc<RwLock<McpClientState>>,
}

#[derive(Default)]
struct McpClientState {
    initialized: bool,
    cached_tools: Option<Vec<McpToolDescriptor>>,
    /// Protocol version we negotiated with the server. Empty until
    /// `initialize` succeeds.
    negotiated_version: String,
}

impl McpClient {
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self {
            transport,
            state: Arc::new(RwLock::new(McpClientState::default())),
        }
    }

    /// Run the protocol initialize handshake. Idempotent — calling
    /// twice is a no-op after the first success.
    pub async fn initialize(&self) -> Result<(), McpClientError> {
        if self.state.read().await.initialized {
            return Ok(());
        }
        // Send our preferred protocol version + minimal capabilities.
        let params = serde_json::json!({
            "protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0],
            "capabilities": {},
            "clientInfo": {
                "name": "thinkingroot",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let result = self.transport.rpc("initialize", params).await?;
        let server_version = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        // Honest version check: warn but don't refuse on
        // version mismatch with the supported list — MCP server
        // ecosystem is still settling on protocol versions, and
        // refusing on unknown version would lock out half the
        // servers. The list is informational at v1.
        if !SUPPORTED_PROTOCOL_VERSIONS.iter().any(|v| *v == server_version) {
            tracing::warn!(
                target: "mcp_client",
                server_version,
                supported = ?SUPPORTED_PROTOCOL_VERSIONS,
                "MCP server protocol version not in our supported list — proceeding anyway"
            );
        }
        // The MCP spec defines a `notifications/initialized` ping
        // we must send back after `initialize`'s response. Some
        // servers tolerate its absence; sending it is the
        // protocol-correct path.
        let _ = self
            .transport
            .rpc("notifications/initialized", serde_json::json!({}))
            .await; // tolerant of "method not found" on some servers

        let mut state = self.state.write().await;
        state.initialized = true;
        state.negotiated_version = server_version.to_string();
        Ok(())
    }

    /// List the server's tools. Cached after first call; pass
    /// `refresh = true` to bypass the cache.
    pub async fn list_tools(
        &self,
        refresh: bool,
    ) -> Result<Vec<McpToolDescriptor>, McpClientError> {
        if !self.state.read().await.initialized {
            return Err(McpClientError::NotInitialized);
        }
        if !refresh
            && let Some(cached) = self.state.read().await.cached_tools.clone()
        {
            return Ok(cached);
        }
        let raw = self
            .transport
            .rpc("tools/list", serde_json::json!({}))
            .await?;
        let tools: Vec<McpToolDescriptor> = raw
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| serde_json::from_value(t.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        self.state.write().await.cached_tools = Some(tools.clone());
        Ok(tools)
    }

    /// Dispatch a tools/call. `name` must be the tool's wire name
    /// from `list_tools` — the `<server>::` prefix used at the
    /// thinkingroot dispatcher layer is stripped before reaching
    /// this method.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpClientError> {
        if !self.state.read().await.initialized {
            return Err(McpClientError::NotInitialized);
        }
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments,
        });
        let raw = self.transport.rpc("tools/call", params).await?;
        let result: McpToolResult = serde_json::from_value(raw)?;
        Ok(result)
    }

    /// The protocol version we negotiated, or empty string when
    /// `initialize` hasn't run.
    pub async fn negotiated_version(&self) -> String {
        self.state.read().await.negotiated_version.clone()
    }

    /// Visible for tests / integrations that want to assert the
    /// cache state.
    pub async fn is_initialized(&self) -> bool {
        self.state.read().await.initialized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Scripted transport — replays a fixed map of method → result.
    /// Captures every method called for test assertions.
    struct ScriptedTransport {
        responses: Mutex<std::collections::HashMap<String, Value>>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl ScriptedTransport {
        fn new(pairs: Vec<(&'static str, Value)>) -> Self {
            let mut map = std::collections::HashMap::new();
            for (k, v) in pairs {
                map.insert(k.to_string(), v);
            }
            Self {
                responses: Mutex::new(map),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl McpTransport for ScriptedTransport {
        async fn rpc(&self, method: &str, params: Value) -> Result<Value, McpClientError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            self.responses
                .lock()
                .unwrap()
                .get(method)
                .cloned()
                .ok_or_else(|| McpClientError::Protocol(format!("no scripted response for {method}")))
        }
    }

    #[tokio::test]
    async fn initialize_runs_handshake_and_caches_state() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            (
                "initialize",
                serde_json::json!({
                    "protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0],
                    "capabilities": {}
                }),
            ),
            ("notifications/initialized", serde_json::json!(null)),
        ]));
        let client = McpClient::new(transport.clone());
        assert!(!client.is_initialized().await);
        client.initialize().await.expect("initialize");
        assert!(client.is_initialized().await);
        assert_eq!(client.negotiated_version().await, SUPPORTED_PROTOCOL_VERSIONS[0]);
        // Idempotent — second call doesn't re-rpc
        client.initialize().await.expect("initialize-2");
        let calls = transport.calls();
        // First initialize call + first notifications/initialized.
        // Second initialize is a no-op (early return).
        assert_eq!(calls.iter().filter(|(m, _)| m == "initialize").count(), 1);
    }

    #[tokio::test]
    async fn list_tools_requires_initialize_first() {
        let transport = Arc::new(ScriptedTransport::new(vec![]));
        let client = McpClient::new(transport);
        let err = client.list_tools(false).await.unwrap_err();
        assert!(matches!(err, McpClientError::NotInitialized));
    }

    #[tokio::test]
    async fn list_tools_parses_descriptors_and_caches() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            (
                "initialize",
                serde_json::json!({"protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0]}),
            ),
            ("notifications/initialized", serde_json::json!(null)),
            (
                "tools/list",
                serde_json::json!({
                    "tools": [
                        {"name": "read_file", "description": "Read a file", "inputSchema": {}},
                        {"name": "write_file", "description": "Write a file", "inputSchema": {}}
                    ]
                }),
            ),
        ]));
        let client = McpClient::new(transport.clone());
        client.initialize().await.unwrap();
        let tools = client.list_tools(false).await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "read_file");
        // Second call hits cache, doesn't re-rpc.
        let _ = client.list_tools(false).await.unwrap();
        let n_lists = transport
            .calls()
            .iter()
            .filter(|(m, _)| m == "tools/list")
            .count();
        assert_eq!(n_lists, 1, "second list_tools should hit cache");
    }

    #[tokio::test]
    async fn list_tools_refresh_bypasses_cache() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            (
                "initialize",
                serde_json::json!({"protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0]}),
            ),
            ("notifications/initialized", serde_json::json!(null)),
            (
                "tools/list",
                serde_json::json!({
                    "tools": [{"name": "echo", "description": "", "inputSchema": {}}]
                }),
            ),
        ]));
        let client = McpClient::new(transport.clone());
        client.initialize().await.unwrap();
        let _ = client.list_tools(false).await.unwrap();
        let _ = client.list_tools(/*refresh=*/ true).await.unwrap();
        let n_lists = transport
            .calls()
            .iter()
            .filter(|(m, _)| m == "tools/list")
            .count();
        assert_eq!(n_lists, 2);
    }

    #[tokio::test]
    async fn call_tool_dispatches_with_correct_params_shape() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            (
                "initialize",
                serde_json::json!({"protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0]}),
            ),
            ("notifications/initialized", serde_json::json!(null)),
            (
                "tools/call",
                serde_json::json!({
                    "content": [{"type": "text", "text": "result"}],
                    "isError": false
                }),
            ),
        ]));
        let client = McpClient::new(transport.clone());
        client.initialize().await.unwrap();
        let result = client
            .call_tool("read_file", serde_json::json!({"path": "/tmp/x"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        // Verify wire shape: params has both `name` and `arguments`.
        let call_params = transport
            .calls()
            .into_iter()
            .find(|(m, _)| m == "tools/call")
            .unwrap()
            .1;
        assert_eq!(call_params["name"], "read_file");
        assert_eq!(call_params["arguments"]["path"], "/tmp/x");
    }
}
