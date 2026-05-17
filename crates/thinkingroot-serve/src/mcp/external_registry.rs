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
    pub async fn dispatch(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Option<Result<McpToolResult, McpClientError>> {
        let (server, tool) = prefixed_name.split_once("::")?;
        let client = self.inner.read().await.get(server).cloned()?;
        Some(client.call_tool(tool, arguments).await)
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

fn global_registry() -> &'static tokio::sync::RwLock<Arc<ExternalMcpRegistry>> {
    static REG: std::sync::OnceLock<tokio::sync::RwLock<Arc<ExternalMcpRegistry>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| tokio::sync::RwLock::new(Arc::new(ExternalMcpRegistry::empty())))
}

/// Read-only handle to the current global registry. Cheap clone.
pub async fn global() -> Arc<ExternalMcpRegistry> {
    global_registry().read().await.clone()
}

/// Replace the global with a freshly-built registry. Atomic swap;
/// in-flight `dispatch` calls against the old registry keep their
/// `Arc` alive until they return.
pub async fn install_global(reg: Arc<ExternalMcpRegistry>) {
    let mut g = global_registry().write().await;
    *g = reg;
}

/// Load workspace config + install as global. Convenience helper
/// for workspace-mount hooks. On config-parse error, returns the
/// error and leaves the existing global in place.
pub async fn load_global_from_workspace_config(
    workspace_root: &Path,
) -> Result<(), RegistryError> {
    let reg = ExternalMcpRegistry::from_workspace_config(workspace_root).await?;
    install_global(Arc::new(reg)).await;
    Ok(())
}

#[cfg(test)]
pub async fn clear_global_for_tests() {
    install_global(Arc::new(ExternalMcpRegistry::empty())).await;
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
            let transport = StdioTransport::spawn(&command, &entry.args, env, cwd)
                .await
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
            let transport = HttpTransport::new(endpoint, auth, timeout)
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

/// Resolve `${VAR}` references in `value`. A literal not matching
/// the `${...}` pattern is returned verbatim. Unresolved refs
/// produce `Err(UnresolvedEnv)` — never silent fallthrough.
fn resolve_env_ref(value: &str) -> Result<String, RegistryError> {
    let trimmed = value.trim();
    if trimmed.starts_with("${") && trimmed.ends_with('}') {
        let name = &trimmed[2..trimmed.len() - 1];
        return std::env::var(name)
            .map_err(|_| RegistryError::UnresolvedEnv(name.to_string()));
    }
    Ok(value.to_string())
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
