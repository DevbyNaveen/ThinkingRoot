//! Local MCP exposure (Step 14).
//!
//! The sidecar from Step 10 (`agent_runtime_subprocess.rs`) already
//! runs `root serve` bound to `127.0.0.1`, which transitively serves
//! the MCP HTTP / SSE / stdio surfaces from `thinkingroot-serve`.
//! This module surfaces the sidecar's status to the UI and renders
//! ready-to-paste config snippets for the half-dozen AI tools that
//! support MCP today. It also exposes a one-click writer for the same
//! config shapes so desktop users do not have to hand-edit JSON/TOML.
//!
//! The snippets shape matches the OSS CLI's `root connect` output
//! — both ultimately point Claude Desktop / Cursor / Zed / VS Code
//! at a `root serve --mcp-stdio --path <workspace>` subprocess that
//! the AI tool spawns per session.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};
use thinkingroot_core::{atomic_write, global_config::Credentials};

use crate::state::AppState;

/// Snapshot of sidecar lifecycle state, plus the URL the user can
/// `curl` to confirm MCP is reachable.
#[derive(Debug, Serialize, Clone)]
pub struct McpStatus {
    pub host: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub running: bool,
    pub well_known_url: String,
    pub sse_url: String,
}

/// Returns the running sidecar's host/port + the well-known URLs the
/// user can verify with `curl`. `running` is `false` when the sidecar
/// failed to spawn at startup (missing `root` binary, port collision,
/// etc.) — the UI surfaces this so the user knows MCP is unavailable
/// rather than silently broken.
#[tauri::command]
pub async fn mcp_status(app: AppHandle) -> Result<McpStatus, String> {
    let state = app.state::<AppState>();
    let guard = state.sidecar.lock().await;
    let handle = guard.clone();
    drop(guard);

    let (host, port, pid, running) = match handle {
        Some(h) => (h.host, h.port, h.pid, true),
        None => ("127.0.0.1".to_string(), 0, None, false),
    };

    let well_known_url = if running {
        format!("http://{host}:{port}/.well-known/mcp")
    } else {
        String::new()
    };
    let sse_url = if running {
        format!("http://{host}:{port}/mcp/sse")
    } else {
        String::new()
    };

    Ok(McpStatus {
        host,
        port,
        pid,
        running,
        well_known_url,
        sse_url,
    })
}

/// Render a copy-pasteable config snippet for the named AI tool.
///
/// Supported `tool` values (case-insensitive):
///   - `claude-desktop`, `cursor`, `windsurf`, `cline`, `zed`,
///     `vs-code`, `claude-code`, `gemini-cli`, `codex`.
///
/// Stdio-mode tools embed a `command` + `args` pointing at the
/// bundled `root` binary (resolved via the same logic as the
/// sidecar). HTTP-only Gemini CLI gets the loopback SSE URL.
#[tauri::command]
pub async fn mcp_get_config_snippet(app: AppHandle, tool: String) -> Result<String, String> {
    let bin_path = resolve_root_binary().unwrap_or_else(|| "root".to_string());
    let workspace_path = workspace_path().unwrap_or_else(|| "<your-workspace>".to_string());

    let state = app.state::<AppState>();
    let guard = state.sidecar.lock().await;
    let port = guard.as_ref().map(|h| h.port).unwrap_or(31760);
    drop(guard);

    let key = tool.to_ascii_lowercase();
    let snippet = match key.as_str() {
        "gemini-cli" => json!({
            "mcpServers": {
                "thinkingroot": {
                    "httpUrl": format!("http://127.0.0.1:{port}/mcp/sse"),
                }
            }
        }),
        "vs-code" | "vscode" => json!({
            "servers": {
                "thinkingroot": {
                    "type": "stdio",
                    "command": bin_path,
                    "args": ["serve", "--mcp-stdio", "--path", workspace_path],
                }
            }
        }),
        "zed" => json!({
            "context_servers": {
                "thinkingroot": {
                    "command": bin_path,
                    "args": ["serve", "--mcp-stdio", "--path", workspace_path],
                }
            }
        }),
        "codex" => {
            // TOML for ~/.codex/config.toml — emit raw text so the
            // user can paste it without translating shape.
            return Ok(format!(
                "[mcp_servers.thinkingroot]\ncommand = \"{bin_path}\"\nargs = [\"serve\", \"--mcp-stdio\", \"--path\", \"{workspace_path}\"]\n"
            ));
        }
        // Claude Desktop, Cursor, Windsurf, Cline, Antigravity,
        // Claude Code (per-project) — all share the same
        // `mcpServers.command/args` shape with no `type` field.
        _ => json!({
            "mcpServers": {
                "thinkingroot": {
                    "command": bin_path,
                    "args": ["serve", "--mcp-stdio", "--path", workspace_path],
                }
            }
        }),
    };

    let pretty: Value = snippet;
    serde_json::to_string_pretty(&pretty).map_err(|e| format!("serialize snippet: {e}"))
}

#[derive(Debug, Serialize, Clone)]
pub struct McpConfigureResult {
    pub tool: String,
    pub path: String,
    pub restart_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigFormat {
    McpServers,
    Servers,
    ContextServers,
    ClaudeCode,
    CodexToml,
    GeminiCli,
}

struct ToolConfig {
    label: &'static str,
    path: PathBuf,
    format: ConfigFormat,
}

/// Write the selected tool's MCP config directly, preserving any existing
/// settings and only replacing the `thinkingroot` server entry.
#[tauri::command]
pub async fn mcp_configure_tool(
    app: AppHandle,
    tool: String,
) -> Result<McpConfigureResult, String> {
    let config = tool_config(&tool).ok_or_else(|| format!("unsupported MCP tool `{tool}`"))?;
    let bin_path = resolve_root_binary().unwrap_or_else(|| "root".to_string());
    let workspace_path = workspace_path().unwrap_or_else(|| "<your-workspace>".to_string());

    let state = app.state::<AppState>();
    let guard = state.sidecar.lock().await;
    let port = guard.as_ref().map(|h| h.port).unwrap_or(31760);
    drop(guard);

    match config.format {
        ConfigFormat::CodexToml => write_codex_config(&config.path, &bin_path, &workspace_path)?,
        _ => write_json_config(
            &config.path,
            config.format,
            &bin_path,
            &workspace_path,
            port,
        )?,
    }

    Ok(McpConfigureResult {
        tool: config.label.to_string(),
        path: config.path.display().to_string(),
        restart_required: true,
    })
}

fn tool_config(tool: &str) -> Option<ToolConfig> {
    let key = tool.to_ascii_lowercase();
    match key.as_str() {
        "claude-desktop" => Some(ToolConfig {
            label: "Claude Desktop",
            path: dirs::config_dir()?
                .join("Claude")
                .join("claude_desktop_config.json"),
            format: ConfigFormat::McpServers,
        }),
        "cursor" => Some(ToolConfig {
            label: "Cursor",
            path: dirs::home_dir()?.join(".cursor").join("mcp.json"),
            format: ConfigFormat::McpServers,
        }),
        "windsurf" => Some(ToolConfig {
            label: "Windsurf",
            path: dirs::home_dir()?
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
            format: ConfigFormat::McpServers,
        }),
        "cline" => Some(ToolConfig {
            label: "Cline",
            path: dirs::config_dir()?
                .join("Code")
                .join("User")
                .join("globalStorage")
                .join("saoudrizwan.claude-dev")
                .join("settings")
                .join("cline_mcp_settings.json"),
            format: ConfigFormat::McpServers,
        }),
        "zed" => Some(ToolConfig {
            label: "Zed",
            path: zed_settings_path()?,
            format: ConfigFormat::ContextServers,
        }),
        "vs-code" | "vscode" => Some(ToolConfig {
            label: "VS Code",
            path: dirs::config_dir()?
                .join("Code")
                .join("User")
                .join("mcp.json"),
            format: ConfigFormat::Servers,
        }),
        "claude-code" => Some(ToolConfig {
            label: "Claude Code",
            path: dirs::home_dir()?.join(".claude.json"),
            format: ConfigFormat::ClaudeCode,
        }),
        "gemini-cli" => Some(ToolConfig {
            label: "Gemini CLI",
            path: dirs::home_dir()?.join(".gemini").join("settings.json"),
            format: ConfigFormat::GeminiCli,
        }),
        "codex" => Some(ToolConfig {
            label: "Codex",
            path: dirs::home_dir()?.join(".codex").join("config.toml"),
            format: ConfigFormat::CodexToml,
        }),
        _ => None,
    }
}

fn zed_settings_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|d| d.join(".config").join("zed").join("settings.json"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::config_dir().map(|d| d.join("zed").join("settings.json"))
    }
}

fn write_json_config(
    path: &Path,
    format: ConfigFormat,
    bin_path: &str,
    workspace_path: &str,
    port: u16,
) -> Result<(), String> {
    let mut existing: Value = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    match format {
        ConfigFormat::ClaudeCode => {
            apply_claude_code_entry(&mut existing, bin_path, workspace_path)
        }
        ConfigFormat::GeminiCli => {
            if !existing["mcpServers"].is_object() {
                existing["mcpServers"] = json!({});
            }
            existing["mcpServers"]["thinkingroot"] = json!({
                "httpUrl": format!("http://127.0.0.1:{port}/mcp/sse"),
            });
        }
        ConfigFormat::Servers => {
            if !existing["servers"].is_object() {
                existing["servers"] = json!({});
            }
            existing["servers"]["thinkingroot"] = stdio_entry(bin_path, workspace_path, true);
        }
        ConfigFormat::ContextServers => {
            if !existing["context_servers"].is_object() {
                existing["context_servers"] = json!({});
            }
            existing["context_servers"]["thinkingroot"] =
                stdio_entry(bin_path, workspace_path, false);
        }
        ConfigFormat::McpServers => {
            if !existing["mcpServers"].is_object() {
                existing["mcpServers"] = json!({});
            }
            existing["mcpServers"]["thinkingroot"] = stdio_entry(bin_path, workspace_path, false);
        }
        ConfigFormat::CodexToml => unreachable!("handled by write_codex_config"),
    }

    let out = serde_json::to_string_pretty(&existing)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    write_config_atomic(path, out.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn apply_claude_code_entry(existing: &mut Value, bin_path: &str, workspace_path: &str) {
    if !existing["projects"].is_object() {
        existing["projects"] = json!({});
    }
    if !existing["projects"][workspace_path].is_object() {
        existing["projects"][workspace_path] = json!({});
    }
    if !existing["projects"][workspace_path]["mcpServers"].is_object() {
        existing["projects"][workspace_path]["mcpServers"] = json!({});
    }
    existing["projects"][workspace_path]["mcpServers"]["thinkingroot"] =
        stdio_entry(bin_path, workspace_path, false);
}

fn stdio_entry(bin_path: &str, workspace_path: &str, needs_type_field: bool) -> Value {
    let mut entry = if needs_type_field {
        json!({
            "type": "stdio",
            "command": bin_path,
            "args": ["serve", "--mcp-stdio", "--path", workspace_path],
        })
    } else {
        json!({
            "command": bin_path,
            "args": ["serve", "--mcp-stdio", "--path", workspace_path],
        })
    };
    let env = credential_env_json(true);
    if !env.is_empty() {
        entry["env"] = json!(env);
    }
    entry
}

fn write_codex_config(path: &Path, bin_path: &str, workspace_path: &str) -> Result<(), String> {
    let mut doc: toml::Value = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        raw.parse()
            .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::Value::Table(toml::map::Map::new())
    };

    if !matches!(doc, toml::Value::Table(_)) {
        doc = toml::Value::Table(toml::map::Map::new());
    }
    let root = doc
        .as_table_mut()
        .expect("doc was normalised to a TOML table");
    if !root.contains_key("mcp_servers") {
        root.insert(
            "mcp_servers".to_string(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }
    let mcp_servers = root
        .get_mut("mcp_servers")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| "mcp_servers exists but is not a TOML table".to_string())?;

    let mut entry = toml::map::Map::new();
    entry.insert(
        "command".to_string(),
        toml::Value::String(bin_path.to_string()),
    );
    entry.insert(
        "args".to_string(),
        toml::Value::Array(vec![
            toml::Value::String("serve".to_string()),
            toml::Value::String("--mcp-stdio".to_string()),
            toml::Value::String("--path".to_string()),
            toml::Value::String(workspace_path.to_string()),
        ]),
    );
    let env = credential_env_toml();
    if !env.is_empty() {
        entry.insert("env".to_string(), toml::Value::Table(env));
    }
    mcp_servers.insert("thinkingroot".to_string(), toml::Value::Table(entry));

    let out = toml::to_string_pretty(&doc)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    write_config_atomic(path, out.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

const CREDENTIAL_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_REGION",
    "AWS_REGION",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "OPENROUTER_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "TOGETHER_API_KEY",
    "PERPLEXITY_API_KEY",
    "LITELLM_API_KEY",
    "CUSTOM_LLM_API_KEY",
];

fn credential_env_json(tool_supports_var_expansion: bool) -> serde_json::Map<String, Value> {
    let stored = Credentials::load().unwrap_or_default();
    let mut map = serde_json::Map::new();
    for var in CREDENTIAL_VARS {
        let parent_has = std::env::var(var).is_ok_and(|v| !v.is_empty());
        let cred_has = stored.get(var).is_some_and(|v| !v.is_empty());
        if !parent_has && !cred_has {
            continue;
        }
        let value = if tool_supports_var_expansion && parent_has {
            json!(format!("${{{var}}}"))
        } else {
            json!("")
        };
        map.insert((*var).to_string(), value);
    }
    map
}

fn credential_env_toml() -> toml::map::Map<String, toml::Value> {
    let stored = Credentials::load().unwrap_or_default();
    let mut map = toml::map::Map::new();
    for var in CREDENTIAL_VARS {
        let parent_has = std::env::var(var).is_ok_and(|v| !v.is_empty());
        let cred_has = stored.get(var).is_some_and(|v| !v.is_empty());
        if !parent_has && !cred_has {
            continue;
        }
        let value = if parent_has {
            format!("${{{var}}}")
        } else {
            String::new()
        };
        map.insert((*var).to_string(), toml::Value::String(value));
    }
    map
}

fn write_config_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let mode = Some(0o600u32);
    #[cfg(not(unix))]
    let mode = None::<u32>;
    atomic_write(path, contents, mode).map_err(|e| match e {
        thinkingroot_core::Error::Io { source, .. } => source,
        other => std::io::Error::other(other.to_string()),
    })
}

fn resolve_root_binary() -> Option<String> {
    if let Ok(override_path) = std::env::var("THINKINGROOT_ROOT_BINARY") {
        if !override_path.is_empty() {
            return Some(override_path);
        }
    }
    let bin = if cfg!(windows) { "root.exe" } else { "root" };
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

fn workspace_path() -> Option<String> {
    if let Ok(p) = std::env::var("THINKINGROOT_WORKSPACE") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    let registry = thinkingroot_core::WorkspaceRegistry::load().ok()?;
    registry
        .active_entry()
        .map(|e| e.path.display().to_string())
}

// ─── MCP server list (sidebar "MCP TOOLS") ───────────────────────────
//
// One row per MCP tool (and optional `servers[]` entries from the
// manifest). The sidecar is the OSS engine's `root serve` — we read
// `/.well-known/mcp`, which mirrors the JSON-RPC `tools/list` catalog.
// If the sidecar is down we surface that honestly rather than fabricating
// a list.

#[derive(Debug, Serialize, Clone)]
pub struct McpServerRow {
    pub name: String,
    pub transport: String,
    pub status: String,
    pub description: Option<String>,
}

#[tauri::command]
pub async fn mcp_list_connected(app: AppHandle) -> Result<Vec<McpServerRow>, String> {
    // Use the shared lightweight resolver so the panel stays accurate
    // when a daemon was started outside this desktop session (e.g.
    // CLI `root serve`, launchd, or a fresh sidecar respawn). Returns
    // None only when no daemon is genuinely reachable.
    let Some((host, port)) = crate::commands::sidecar_client::try_resolve_endpoint(&app).await
    else {
        return Ok(Vec::new());
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;

    // Probe `/livez` to verify the sidecar is actually running before
    // surfacing any "running" status.  Pre-fix this command claimed
    // every row was "running" without ever checking — a sidecar that
    // had crashed but whose handle was still cached in `AppState`
    // would falsely advertise itself, and external MCP servers listed
    // by the manifest were marked "running" with zero verification.
    // Honesty rule: surface what we actually know.
    let livez_url = format!("http://{}:{}/livez", host, port);
    let self_status = match client.get(&livez_url).send().await {
        Ok(r) if r.status().is_success() => "running",
        Ok(_) => "unhealthy",
        Err(_) => "unreachable",
    };

    let manifest_url = format!("http://{}:{}/.well-known/mcp", host, port);
    let resp = match client.get(&manifest_url).send().await {
        Ok(r) => r,
        Err(_) => {
            return Ok(vec![McpServerRow {
                name: "local sidecar".to_string(),
                transport: "sse".to_string(),
                status: self_status.to_string(),
                description: Some(format!("Manifest unavailable — sidecar {}:{}", host, port)),
            }]);
        }
    };
    if !resp.status().is_success() {
        return Ok(vec![McpServerRow {
            name: "local sidecar".to_string(),
            transport: "sse".to_string(),
            status: self_status.to_string(),
            description: Some(format!(
                "GET /.well-known/mcp returned {} — {}:{}",
                resp.status(),
                host,
                port
            )),
        }]);
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;

    let mut rows: Vec<McpServerRow> = Vec::new();

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for t in tools {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)")
                .to_string();
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .map(String::from);
            rows.push(McpServerRow {
                name,
                transport: "sse".to_string(),
                status: self_status.to_string(),
                description,
            });
        }
    }

    if let Some(servers) = body.get("servers").and_then(Value::as_array) {
        for srv in servers {
            let name = srv
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)")
                .to_string();
            let transport = srv
                .get("transport")
                .and_then(Value::as_str)
                .unwrap_or("stdio")
                .to_string();
            let description = srv
                .get("description")
                .and_then(Value::as_str)
                .map(String::from);
            rows.push(McpServerRow {
                name,
                transport,
                status: "configured".to_string(),
                description,
            });
        }
    }

    // Always include the "local sidecar" anchor row so the panel
    // never goes dark when the daemon is reachable but the manifest
    // happens to enumerate only external servers. This is the row
    // the user looks at first to know "is the engine alive at all"
    // — without it a busy-but-tools-only manifest would hide the
    // daemon's existence behind editor-specific entries (Antigravity,
    // Codex, Cursor, …).
    rows.insert(
        0,
        McpServerRow {
            name: "local sidecar".to_string(),
            transport: "sse".to_string(),
            status: self_status.to_string(),
            description: Some(format!("ThinkingRoot engine at {}:{}", host, port)),
        },
    );

    if rows.len() == 1 {
        // Manifest had no tools and no servers — surface the empty
        // state honestly on the anchor row.
        if let Some(desc) = body.get("description").and_then(Value::as_str) {
            rows[0].description = Some(format!(
                "ThinkingRoot engine at {}:{} — {}",
                host, port, desc
            ));
        }
    }

    Ok(rows)
}
