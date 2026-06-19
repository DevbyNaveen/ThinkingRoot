//! Clean-room reimplementation. Inspired by openhuman/mcp_client/registry.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.5 (2026-05-17) — registry of external MCP servers.
//!
//! Loads `<workspace>/.thinkingroot/mcp-servers.toml`. Each entry
//! is one MCP server the user has registered; we open a transport
//! and an `McpClient` for it at registry-build time and cache the
//! handles in a `HashMap<server_name, Arc<McpClient>>`.
//!
//! ## Namespace bridging
//!
//! Tool names returned by `list_all_tools()` are prefixed with
//! `<server_name>::` so the MCP `tools/list` catalogue stays
//! flat from the model's POV. When the model calls
//! `filesystem::read_file`, the dispatcher splits on `::`, finds
//! the matching client, strips the prefix, and delegates.
//!
//! ## Config shape
//!
//! ```toml
//! [[server]]
//! name = "filesystem"
//! transport = "stdio"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "/Users/me/Documents"]
//!
//! [[server]]
//! name = "github"
//! transport = "http"
//! endpoint = "https://api.example.com/mcp"
//! timeout_secs = 30
//! auth = { kind = "bearer", token = "${GITHUB_TOKEN}" }
//! ```
//!
//! `${ENV_VAR}` interpolation in auth tokens happens at load time
//! so config files are safe to check in (the actual secret stays
//! in the env). Unresolved env refs (`${VAR}` where VAR isn't set)
//! produce a typed `ConfigLoad` error rather than passing through
//! the literal — the latter would silently authenticate as the
//! literal string `${GITHUB_TOKEN}`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::client::{McpClient, McpClientError, McpToolDescriptor, McpToolResult};
use super::http_transport::{HttpAuth, HttpTransport};
use super::stdio_transport::StdioTransport;

/// Top-level TOML shape.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct McpServersConfig {
    #[serde(default)]
    pub server: Vec<ServerEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerEntry {
    pub name: String,
    pub transport: TransportKind,
    // stdio-only
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    // http-only
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub auth: Option<AuthEntry>,
    /// When set, this HTTP connector requires a per-user OAuth Bearer token
    /// fetched from the gateway OAuth broker at call time.  The value is the
    /// provider slug the broker recognises (e.g. `"google"`, `"slack"`).
    /// Setting this on a `stdio` transport is a config error caught at spawn.
    #[serde(default)]
    pub oauth_provider: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AuthEntry {
    pub kind: AuthKind,
    pub token: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Bearer,
    ApiKey,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("config not found at {0}")]
    ConfigMissing(PathBuf),
    #[error("config parse: {0}")]
    ConfigParse(String),
    #[error("unresolved env reference: {0}")]
    UnresolvedEnv(String),
    #[error("malformed env reference: {0}")]
    MalformedEnv(String),
    #[error("transport startup for `{0}`: {1}")]
    TransportStartup(String, McpClientError),
}

/// The live registry. Internally `HashMap<server_name, Arc<McpClient>>`
/// gated by an `RwLock` for the rare case of dynamic add/remove
/// (the typical lifecycle is "load once at AppState construction").
pub struct ExternalMcpRegistry {
    inner: RwLock<HashMap<String, Arc<McpClient>>>,
}

impl ExternalMcpRegistry {
    pub fn empty() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Resolve `<workspace_root>/.thinkingroot/mcp-servers.toml`,
    /// parse it, and spawn every declared server. Returns the
    /// populated registry; servers that fail to start are logged
    /// at WARN and skipped — one bad config entry shouldn't take
    /// down the whole registry.
    pub async fn from_workspace_config(workspace_root: &Path) -> Result<Self, RegistryError> {
        let cfg_path = workspace_root
            .join(".thinkingroot")
            .join("mcp-servers.toml");
        if !cfg_path.exists() {
            return Ok(Self::empty());
        }
        let bytes = std::fs::read_to_string(&cfg_path).map_err(|e| {
            RegistryError::ConfigParse(format!("read {}: {e}", cfg_path.display()))
        })?;
        let config: McpServersConfig =
            toml::from_str(&bytes).map_err(|e| RegistryError::ConfigParse(e.to_string()))?;
        Self::from_config(config).await
    }

    /// Build from an already-parsed config. Useful for tests + the
    /// `root mcp add` CLI path.
    pub async fn from_config(config: McpServersConfig) -> Result<Self, RegistryError> {
        let mut inner: HashMap<String, Arc<McpClient>> = HashMap::new();
        for entry in config.server {
            match spawn_one(&entry).await {
                Ok(client) => {
                    inner.insert(entry.name.clone(), client);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "external_mcp_registry",
                        server = %entry.name,
                        "skipping server (startup failed): {e}"
                    );
                }
            }
        }
        Ok(Self {
            inner: RwLock::new(inner),
        })
    }

    /// Every registered tool prefixed with `<server_name>::`. Used
    /// by `mcp::tools::handle_list` to extend the daemon's
    /// catalogue.
    pub async fn list_all_tools(&self) -> Vec<(String, McpToolDescriptor)> {
        let mut out = Vec::new();
        let guard = self.inner.read().await;
        for (server_name, client) in guard.iter() {
            match client.list_tools(false).await {
                Ok(tools) => {
                    for tool in tools {
                        out.push((format!("{server_name}::{}", tool.name), tool));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "external_mcp_registry",
                        server = %server_name,
                        "list_tools failed: {e}"
                    );
                }
            }
        }
        out
    }

    /// Dispatch a `<server>::<tool>` call. Returns `None` when
    /// `prefixed_name` doesn't match the `<server>::<tool>` shape
    /// (caller should fall back to other resolution paths).
    ///
    /// `user_id` is forwarded to `McpClient::call_tool` so that
    /// OAuth-tagged connectors can fetch a per-user Bearer token
    /// from the gateway broker.  Pass `None` for non-user-scoped
    /// calls (e.g. from the project brain directly); an OAuth
    /// connector called with `None` will return a typed error
    /// rather than using a wrong identity.
    pub async fn dispatch(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
        user_id: Option<&str>,
    ) -> Option<Result<McpToolResult, McpClientError>> {
        let (server, tool) = prefixed_name.split_once("::")?;
        let client = self.inner.read().await.get(server).cloned()?;
        Some(client.call_tool(tool, arguments, user_id).await)
    }

    /// True iff there is at least one registered server.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// Number of registered servers.
    pub async fn server_count(&self) -> usize {
        self.inner.read().await.len()
    }
}

// ── Process-global registry accessor ───────────────────────────────
//
// `mcp/tools.rs::handle_list` and `handle_call` don't carry
// AppState; they consult this singleton for external tools. The
// singleton is initialised empty on first access; production
// callers load it via `load_global_from_workspace_config` at
// workspace mount time (or via the `root mcp add` CLI subcommand).

// Per-WORKSPACE registries (Slice 2): external MCP tools are scoped to the
// workspace whose `.thinkingroot/mcp-servers.toml` declared them, so per-user
// (`u_*`) and per-(user×agent) scopes each get THEIR OWN external tools (e.g.
// each end-user's own Composio OAuth) with NO cross-tenant leakage. Keyed by
// workspace name; a ws with no config resolves to an empty registry.
fn registry_map() -> &'static tokio::sync::RwLock<HashMap<String, Arc<ExternalMcpRegistry>>> {
    static REG: std::sync::OnceLock<
        tokio::sync::RwLock<HashMap<String, Arc<ExternalMcpRegistry>>>,
    > = std::sync::OnceLock::new();
    REG.get_or_init(|| tokio::sync::RwLock::new(HashMap::new()))
}

/// Read-only handle to a workspace's external-MCP registry. Empty when the
/// workspace has no `mcp-servers.toml` mounted (or when `ws` is "" — the
/// no-workspace context, which has no external tools). Cheap clone.
pub async fn registry_for(ws: &str) -> Arc<ExternalMcpRegistry> {
    registry_map()
        .read()
        .await
        .get(ws)
        .cloned()
        .unwrap_or_else(|| Arc::new(ExternalMcpRegistry::empty()))
}

/// Install/replace a workspace's registry. Atomic per-ws swap; in-flight
/// `dispatch` calls against the old registry keep their `Arc` alive until
/// they return.
pub async fn install_for(ws: &str, reg: Arc<ExternalMcpRegistry>) {
    registry_map().write().await.insert(ws.to_string(), reg);
}

/// Load a workspace's `mcp-servers.toml` and install it under that ws key.
/// Convenience helper for workspace-mount + `root mcp add` hooks. On
/// config-parse error, returns the error and leaves the existing registry
/// in place.
pub async fn load_workspace_config(ws: &str, workspace_root: &Path) -> Result<(), RegistryError> {
    let reg = ExternalMcpRegistry::from_workspace_config(workspace_root).await?;
    install_for(ws, Arc::new(reg)).await;
    Ok(())
}

// ── Inheritance-chain resolution (Slice 2b) ─────────────────────────
//
// A connector installed once on the shared/project brain (`main`) must be
// usable from every scope that inherits it — each agent (`agent_<name>`) and
// per-user scope (`u_<id>`/`u_<id>__agent_<name>`) — exactly like deployed
// functions/agents/prompts already cascade via `QueryEngine::inheritance_chain`.
// These helpers take that precomputed chain (MOST-specific first) and fold the
// per-ws registries into one view so the catalog, `tools/call` dispatch, the
// capability router, and Root-Function `ctx.mcp.call` all resolve identically.
//
// Resolution is **nearest-server-wins**: the first brain in the chain that
// declares a given server name owns it entirely (all its tools, its own
// token/config). So a user's own `gmail` (their OAuth, scope `u_X`) fully
// shadows the project-default `gmail` on `main`; connectors only present at an
// outer scope are inherited. Listing and dispatch share this rule, so anything
// advertised is callable.

/// Fold every brain in `chain` (most-specific first) into a SINGLE registry,
/// nearest-server-wins. Cheap: clones `Arc<McpClient>` handles, spawns nothing.
/// Empty `chain` → empty registry (the no-workspace context has no externals).
pub async fn merged_for_chain(chain: &[String]) -> Arc<ExternalMcpRegistry> {
    let mut merged: HashMap<String, Arc<McpClient>> = HashMap::new();
    for ws in chain {
        let reg = registry_for(ws).await;
        let guard = reg.inner.read().await;
        for (server, client) in guard.iter() {
            // First (nearest) scope to declare this server keeps it.
            merged.entry(server.clone()).or_insert_with(|| client.clone());
        }
    }
    Arc::new(ExternalMcpRegistry {
        inner: RwLock::new(merged),
    })
}

/// Every external tool visible to `chain`, prefixed `<server>::<tool>`,
/// nearest-server-wins. Mirrors [`dispatch_for_chain`] exactly.
pub async fn list_tools_for_chain(chain: &[String]) -> Vec<(String, McpToolDescriptor)> {
    merged_for_chain(chain).await.list_all_tools().await
}

/// Dispatch a `<server>::<tool>` call across `chain`, nearest-server-wins.
/// `None` when no brain in the chain declares the server (caller surfaces a
/// "not registered" error) — same contract as [`ExternalMcpRegistry::dispatch`].
///
/// `user_id` is derived from `chain`: the first workspace element whose
/// name starts with `"u_"` supplies the end-user identity.  The segment
/// after `"u_"` is taken up to the first `"__"` separator (e.g.
/// `u_alice__agent_x` → `alice`).  When no `u_*` element is present in
/// the chain, `user_id` is `None` — OAuth connectors will reject the
/// call with a typed "oauth connector requires a per-user scope" error.
pub async fn dispatch_for_chain(
    chain: &[String],
    prefixed_name: &str,
    arguments: serde_json::Value,
) -> Option<Result<McpToolResult, McpClientError>> {
    // Derive user_id from the most-specific `u_*` workspace in the chain.
    let user_id: Option<String> = chain.iter().find_map(|ws| {
        ws.strip_prefix("u_").map(|rest| {
            // `u_alice__agent_x` → strip the agent suffix
            rest.split("__").next().unwrap_or(rest).to_string()
        })
    });
    merged_for_chain(chain)
        .await
        .dispatch(prefixed_name, arguments, user_id.as_deref())
        .await
}

#[cfg(test)]
pub async fn clear_for_tests(ws: &str) {
    install_for(ws, Arc::new(ExternalMcpRegistry::empty())).await;
}

async fn spawn_one(entry: &ServerEntry) -> Result<Arc<McpClient>, RegistryError> {
    let client = match entry.transport {
        TransportKind::Stdio => {
            let command = entry
                .command
                .clone()
                .ok_or_else(|| RegistryError::ConfigParse("stdio server missing `command`".into()))?;
            let env = resolve_env_map(&entry.env)?;
            let cwd = entry.cwd.clone().map(PathBuf::from);
            // Honour the per-server `timeout_secs` from
            // `mcp-servers.toml`. `None` keeps the default 30s.
            let transport = match entry.timeout_secs {
                Some(secs) => {
                    StdioTransport::spawn_with_timeout(
                        &command,
                        &entry.args,
                        env,
                        cwd,
                        Duration::from_secs(secs),
                    )
                    .await
                }
                None => StdioTransport::spawn(&command, &entry.args, env, cwd).await,
            }
            .map_err(|e| RegistryError::TransportStartup(entry.name.clone(), e))?;
            McpClient::new(transport)
        }
        TransportKind::Http => {
            let endpoint = entry.endpoint.clone().ok_or_else(|| {
                RegistryError::ConfigParse("http server missing `endpoint`".into())
            })?;
            let timeout = entry.timeout_secs.map(Duration::from_secs);
            let auth = match &entry.auth {
                Some(a) => {
                    let token = resolve_env_ref(&a.token)?;
                    Some(match a.kind {
                        AuthKind::Bearer => HttpAuth::Bearer(token),
                        AuthKind::ApiKey => HttpAuth::ApiKey(token),
                    })
                }
                None => None,
            };
            let transport =
                HttpTransport::new(endpoint, auth, timeout, entry.oauth_provider.clone())
                    .map_err(|e| RegistryError::TransportStartup(entry.name.clone(), e))?;
            McpClient::new(transport)
        }
    };
    client
        .initialize()
        .await
        .map_err(|e| RegistryError::TransportStartup(entry.name.clone(), e))?;
    Ok(Arc::new(client))
}

/// Resolve `${VAR}` references inside `value`. Both whole-string
/// (`${TOKEN}`) and substring (`Bearer ${TOKEN}`, `prefix-${X}-suffix`)
/// patterns are supported. A literal containing no `${...}` is
/// returned verbatim. Unresolved refs produce `Err(UnresolvedEnv)` —
/// never a silent pass-through that would let `${MY_TOKEN}` ship
/// as the literal token string to the remote MCP server.
///
/// Variable names follow shell convention: `[A-Za-z_][A-Za-z0-9_]*`.
/// A malformed reference (unterminated `${`, empty name, or invalid
/// chars in the name) is returned as `MalformedEnv` rather than
/// being passed through, so operators get a loud error rather than a
/// silent miss.
fn resolve_env_ref(value: &str) -> Result<String, RegistryError> {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for "${" — anything else is copied byte-for-byte.
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            // Find the closing `}`.
            let name_start = i + 2;
            let Some(close_rel) = bytes[name_start..].iter().position(|&b| b == b'}') else {
                return Err(RegistryError::MalformedEnv(format!(
                    "unterminated ${{...}} reference starting at byte {i} in `{value}`"
                )));
            };
            let name_end = name_start + close_rel;
            let name = &value[name_start..name_end];
            if name.is_empty() {
                return Err(RegistryError::MalformedEnv(format!(
                    "empty ${{}} reference at byte {i} in `{value}`"
                )));
            }
            // Validate shell identifier rules — refuse mistakes
            // early instead of forwarding garbage to `std::env::var`
            // (which would surface as an opaque NotPresent).
            let valid = name.chars().enumerate().all(|(idx, c)| {
                if idx == 0 {
                    c.is_ascii_alphabetic() || c == '_'
                } else {
                    c.is_ascii_alphanumeric() || c == '_'
                }
            });
            if !valid {
                return Err(RegistryError::MalformedEnv(format!(
                    "invalid env var name `${{{name}}}` in `{value}` — must match [A-Za-z_][A-Za-z0-9_]*"
                )));
            }
            let resolved = std::env::var(name)
                .map_err(|_| RegistryError::UnresolvedEnv(name.to_string()))?;
            out.push_str(&resolved);
            i = name_end + 1;
        } else {
            // Push one UTF-8 char at a time — `bytes[i]` could be a
            // multi-byte lead, so step by char_len.
            let rest = &value[i..];
            let ch = rest.chars().next().expect("non-empty rest");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

#[cfg(test)]
mod env_ref_tests {
    use super::{RegistryError, resolve_env_ref};

    fn set(name: &str, val: &str) {
        // SAFETY: tests run in a single-threaded test harness; even
        // multi-threaded test runners only collide on the same key
        // when two tests share a variable, which is the case here
        // but isolated to this module's serial #[test] order.
        unsafe { std::env::set_var(name, val) };
    }

    fn unset(name: &str) {
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn whole_string_substitution() {
        set("TR_TEST_TOK_A", "sk-abc123");
        assert_eq!(resolve_env_ref("${TR_TEST_TOK_A}").unwrap(), "sk-abc123");
        unset("TR_TEST_TOK_A");
    }

    #[test]
    fn substring_substitution() {
        set("TR_TEST_TOK_B", "xyz");
        assert_eq!(
            resolve_env_ref("Bearer ${TR_TEST_TOK_B}").unwrap(),
            "Bearer xyz"
        );
        assert_eq!(
            resolve_env_ref("prefix-${TR_TEST_TOK_B}-suffix").unwrap(),
            "prefix-xyz-suffix"
        );
        unset("TR_TEST_TOK_B");
    }

    #[test]
    fn unresolved_substring_errors() {
        unset("TR_TEST_UNSET_XYZ");
        let err = resolve_env_ref("Bearer ${TR_TEST_UNSET_XYZ}").unwrap_err();
        assert!(matches!(err, RegistryError::UnresolvedEnv(_)));
    }

    #[test]
    fn malformed_refs_are_loud() {
        assert!(matches!(
            resolve_env_ref("Bearer ${UNCLOSED").unwrap_err(),
            RegistryError::MalformedEnv(_)
        ));
        assert!(matches!(
            resolve_env_ref("Bearer ${}").unwrap_err(),
            RegistryError::MalformedEnv(_)
        ));
        assert!(matches!(
            resolve_env_ref("Bearer ${invalid-name}").unwrap_err(),
            RegistryError::MalformedEnv(_)
        ));
    }

    #[test]
    fn literal_pass_through() {
        assert_eq!(resolve_env_ref("plain-string").unwrap(), "plain-string");
        assert_eq!(resolve_env_ref("").unwrap(), "");
    }
}

fn resolve_env_map(input: &HashMap<String, String>) -> Result<HashMap<String, String>, RegistryError> {
    let mut out = HashMap::new();
    for (k, v) in input {
        out.insert(k.clone(), resolve_env_ref(v)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::client::McpTransport;

    /// Minimal transport: handshakes + replays a fixed `tools/list`. Lets a
    /// test build a live `McpClient` with named tools, no real server.
    struct FakeToolsTransport {
        tools: Vec<&'static str>,
    }

    #[async_trait::async_trait]
    impl McpTransport for FakeToolsTransport {
        async fn rpc(
            &self,
            method: &str,
            _params: serde_json::Value,
            _user_id: Option<&str>,
        ) -> Result<serde_json::Value, McpClientError> {
            match method {
                "initialize" => Ok(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {}
                })),
                "notifications/initialized" => Ok(serde_json::json!(null)),
                "tools/list" => Ok(serde_json::json!({
                    "tools": self
                        .tools
                        .iter()
                        .map(|n| serde_json::json!({ "name": n }))
                        .collect::<Vec<_>>()
                })),
                other => Err(McpClientError::Protocol(format!("no scripted: {other}"))),
            }
        }
    }

    async fn fake_client(tools: Vec<&'static str>) -> Arc<McpClient> {
        let c = McpClient::new(Arc::new(FakeToolsTransport { tools }));
        c.initialize().await.expect("fake initialize");
        Arc::new(c)
    }

    async fn fake_registry(
        servers: Vec<(&'static str, Vec<&'static str>)>,
    ) -> Arc<ExternalMcpRegistry> {
        let mut map: HashMap<String, Arc<McpClient>> = HashMap::new();
        for (name, tools) in servers {
            map.insert(name.to_string(), fake_client(tools).await);
        }
        Arc::new(ExternalMcpRegistry {
            inner: RwLock::new(map),
        })
    }

    #[tokio::test]
    async fn chain_inherits_connectors_nearest_server_wins() {
        // Project brain has `gmail` (send) + `telegram`; the per-user scope has
        // its OWN `gmail` (draft). Chain is most-specific first: [u_1, main].
        install_for(
            "cscope_main",
            fake_registry(vec![("gmail", vec!["send"]), ("telegram", vec!["msg"])]).await,
        )
        .await;
        install_for(
            "cscope_u1",
            fake_registry(vec![("gmail", vec!["draft"])]).await,
        )
        .await;

        let chain = vec!["cscope_u1".to_string(), "cscope_main".to_string()];
        let names: std::collections::HashSet<String> = list_tools_for_chain(&chain)
            .await
            .into_iter()
            .map(|(n, _)| n)
            .collect();

        // `gmail` resolves to the NEAREST scope (u_1) → its tool wins, the
        // project default is fully shadowed (nearest-server-wins).
        assert!(names.contains("gmail::draft"), "nearest gmail wins: {names:?}");
        assert!(
            !names.contains("gmail::send"),
            "project gmail must be shadowed: {names:?}"
        );
        // `telegram` exists only on the project brain → inherited downward.
        assert!(
            names.contains("telegram::msg"),
            "telegram should cascade from main: {names:?}"
        );

        // Dispatch resolves the same way: nearest scope owning the server.
        assert!(
            dispatch_for_chain(&chain, "gmail::draft", serde_json::json!({}))
                .await
                .is_some(),
            "inherited/nearest tool must be callable"
        );

        // Empty chain = no externals (the no-workspace guarantee that keeps the
        // dispatch `None` path and builtins-only catalog honest).
        assert!(list_tools_for_chain(&[]).await.is_empty());
        assert!(
            dispatch_for_chain(&[], "gmail::send", serde_json::json!({}))
                .await
                .is_none()
        );

        clear_for_tests("cscope_main").await;
        clear_for_tests("cscope_u1").await;
    }

    #[test]
    fn empty_registry_is_empty() {
        let r = ExternalMcpRegistry::empty();
        // Use a small tokio test runtime for the async accessors.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(r.is_empty().await);
            assert_eq!(r.server_count().await, 0);
            assert!(r.list_all_tools().await.is_empty());
        });
    }

    #[test]
    fn resolve_env_ref_unwraps_set_var() {
        unsafe {
            std::env::set_var("E5_TEST_VAR", "hello");
        }
        let v = resolve_env_ref("${E5_TEST_VAR}").unwrap();
        assert_eq!(v, "hello");
        unsafe {
            std::env::remove_var("E5_TEST_VAR");
        }
    }

    #[test]
    fn resolve_env_ref_fails_loudly_on_unset_var() {
        unsafe {
            std::env::remove_var("E5_NOPE_NOT_SET");
        }
        let err = resolve_env_ref("${E5_NOPE_NOT_SET}").unwrap_err();
        assert!(matches!(err, RegistryError::UnresolvedEnv(_)));
    }

    #[test]
    fn resolve_env_ref_passes_through_literals() {
        let v = resolve_env_ref("not-a-var-ref").unwrap();
        assert_eq!(v, "not-a-var-ref");
    }

    #[test]
    fn missing_config_returns_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let r = ExternalMcpRegistry::from_workspace_config(tmp.path())
                .await
                .expect("missing config → empty");
            assert!(r.is_empty().await);
        });
    }

    #[test]
    fn config_parse_round_trips() {
        let toml = r#"
[[server]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["-y", "fs-mcp"]

[[server]]
name = "github"
transport = "http"
endpoint = "https://api.example.com/mcp"
timeout_secs = 30
auth = { kind = "bearer", token = "literal-token" }
"#;
        let config: McpServersConfig = toml::from_str(toml).expect("parse");
        assert_eq!(config.server.len(), 2);
        assert_eq!(config.server[0].name, "filesystem");
        assert_eq!(config.server[0].transport, TransportKind::Stdio);
        assert_eq!(config.server[1].transport, TransportKind::Http);
        assert!(config.server[1].auth.is_some());
    }
}
