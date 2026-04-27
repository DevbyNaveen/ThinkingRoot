//! Local MCP exposure (Step 14).
//!
//! The sidecar from Step 10 (`agent_runtime_subprocess.rs`) already
//! runs `root serve` bound to `127.0.0.1`, which transitively serves
//! the MCP HTTP / SSE / stdio surfaces from `thinkingroot-serve`.
//! This module surfaces the sidecar's status to the UI and renders
//! ready-to-paste config snippets for the half-dozen AI tools that
//! support MCP today.
//!
//! The snippets shape matches the OSS CLI's `root connect` output
//! — both ultimately point Claude Desktop / Cursor / Zed / VS Code
//! at a `root serve --mcp-stdio --path <workspace>` subprocess that
//! the AI tool spawns per session.

use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};

use crate::config::AppConfig;
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
pub async fn mcp_get_config_snippet(
    app: AppHandle,
    tool: String,
) -> Result<String, String> {
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
    let cfg = AppConfig::load().ok()?;
    cfg.env_or("THINKINGROOT_WORKSPACE")
}

// ─── MCP server list (sidebar "MCP TOOLS") ───────────────────────────
//
// One row per server the sidecar exposes. The sidecar is the OSS
// engine's `root serve` — we read its `/.well-known/mcp` manifest to
// avoid hard-coding the tool catalog here. If the sidecar is down we
// surface that honestly rather than fabricating a list.

#[derive(Debug, Serialize, Clone)]
pub struct McpServerRow {
    pub name: String,
    pub transport: String,
    pub status: String,
    pub description: Option<String>,
}

#[tauri::command]
pub async fn mcp_list_connected(app: AppHandle) -> Result<Vec<McpServerRow>, String> {
    let state = app.state::<AppState>();
    let handle = state.sidecar.lock().await.clone();
    let Some(sidecar) = handle else {
        return Ok(Vec::new());
    };

    let url = format!(
        "http://{}:{}/.well-known/mcp",
        sidecar.host, sidecar.port
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => {
            // Manifest may not exist on the engine yet; fall back to
            // the single self-row we know is true: the sidecar is up.
            return Ok(vec![McpServerRow {
                name: "thinkingroot".to_string(),
                transport: "sse".to_string(),
                status: "running".to_string(),
                description: Some(format!(
                    "Local sidecar at {}:{}",
                    sidecar.host, sidecar.port
                )),
            }]);
        }
    };
    if !resp.status().is_success() {
        return Ok(vec![McpServerRow {
            name: "thinkingroot".to_string(),
            transport: "sse".to_string(),
            status: "running".to_string(),
            description: Some(format!(
                "Local sidecar at {}:{}",
                sidecar.host, sidecar.port
            )),
        }]);
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;

    let mut rows = vec![McpServerRow {
        name: "thinkingroot".to_string(),
        transport: "sse".to_string(),
        status: "running".to_string(),
        description: body
            .get("description")
            .and_then(Value::as_str)
            .map(String::from)
            .or(Some(format!("Local sidecar at {}:{}", sidecar.host, sidecar.port))),
    }];

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
                status: "running".to_string(),
                description,
            });
        }
    }
    Ok(rows)
}
