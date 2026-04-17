use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use serde_json::{Value, json};
use thinkingroot_core::global_config::Credentials;

/// The JSON key / format that each tool uses for MCP server configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConfigFormat {
    /// `{ "mcpServers": { "thinkingroot": { "command": "...", "args": [...] } } }`
    /// Used by: Cursor, Windsurf, Cline, Antigravity, Claude Desktop
    McpServers,
    /// `{ "servers": { "thinkingroot": { "type": "stdio", "command": "...", "args": [...] } } }`
    /// Used by: VS Code (requires explicit `type` field)
    Servers,
    /// `{ "context_servers": { "thinkingroot": { "command": "...", "args": [...] } } }`
    /// Used by: Zed (no `type` field, inferred from presence of `command`)
    ContextServers,
    /// Same JSON as McpServers, written to a standalone file per server.
    /// Used by: Continue.dev (`~/.continue/mcpServers/thinkingroot.json`)
    ContinueDev,
    /// Claude Code CLI: per-project `mcpServers` nesting in `~/.claude.json`
    ClaudeCode,
    /// OpenAI Codex CLI: `~/.codex/config.toml` (TOML format, stdio)
    CodexToml,
    /// Gemini CLI: `~/.gemini/settings.json` with `httpUrl` key (HTTP-only, no stdio support)
    GeminiCli,
}

/// A detected AI tool with its resolved config file path.
pub struct DetectedTool {
    pub name: &'static str,
    pub config_path: PathBuf,
    pub format: ConfigFormat,
}

pub enum WriteAction {
    Written,
    DryRun(String),
    Removed,
    Skipped(&'static str),
}

pub struct WriteResult {
    pub tool: &'static str,
    pub path: PathBuf,
    pub action: WriteAction,
}

// ── Tool detection ───────────────────────────────────────────────

/// Detect all installed AI tools by checking whether their config directories exist.
pub fn detect_tools() -> Vec<DetectedTool> {
    tool_defs()
        .into_iter()
        .filter_map(|(name, path_fn, format)| {
            path_fn().map(|path| DetectedTool {
                name,
                config_path: path,
                format,
            })
        })
        .filter(|t| {
            // Detect by parent directory existing (file itself may not exist yet)
            t.config_path.parent().map(|p| p.exists()).unwrap_or(false)
        })
        .collect()
}

#[allow(clippy::type_complexity)]
fn tool_defs() -> Vec<(&'static str, Box<dyn Fn() -> Option<PathBuf>>, ConfigFormat)> {
    vec![
        (
            "Claude Desktop",
            Box::new(|| {
                dirs::config_dir().map(|d| d.join("Claude").join("claude_desktop_config.json"))
            }),
            ConfigFormat::McpServers,
        ),
        (
            "Cursor",
            Box::new(|| dirs::home_dir().map(|d| d.join(".cursor").join("mcp.json"))),
            ConfigFormat::McpServers,
        ),
        (
            "VS Code",
            Box::new(|| dirs::config_dir().map(|d| d.join("Code").join("User").join("mcp.json"))),
            ConfigFormat::Servers,
        ),
        (
            "Windsurf",
            Box::new(|| {
                dirs::home_dir()
                    .map(|d| d.join(".codeium").join("windsurf").join("mcp_config.json"))
            }),
            ConfigFormat::McpServers,
        ),
        (
            "Zed",
            Box::new(|| {
                // Zed uses ~/.config/zed/settings.json on all platforms
                // (not dirs::config_dir() on macOS which points to Library/Application Support)
                #[cfg(target_os = "macos")]
                {
                    dirs::home_dir().map(|d| d.join(".config").join("zed").join("settings.json"))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    dirs::config_dir().map(|d| d.join("zed").join("settings.json"))
                }
            }),
            ConfigFormat::ContextServers,
        ),
        (
            "Cline",
            Box::new(|| {
                dirs::config_dir().map(|d| {
                    d.join("Code")
                        .join("User")
                        .join("globalStorage")
                        .join("saoudrizwan.claude-dev")
                        .join("settings")
                        .join("cline_mcp_settings.json")
                })
            }),
            ConfigFormat::McpServers,
        ),
        (
            "Continue.dev",
            Box::new(|| {
                dirs::home_dir().map(|d| {
                    d.join(".continue")
                        .join("mcpServers")
                        .join("thinkingroot.json")
                })
            }),
            ConfigFormat::ContinueDev,
        ),
        (
            "Antigravity",
            Box::new(|| {
                dirs::home_dir().map(|d| {
                    d.join(".gemini")
                        .join("antigravity")
                        .join("mcp_config.json")
                })
            }),
            ConfigFormat::McpServers,
        ),
        (
            "Gemini CLI",
            Box::new(|| dirs::home_dir().map(|d| d.join(".gemini").join("settings.json"))),
            ConfigFormat::GeminiCli,
        ),
        (
            "Claude Code",
            Box::new(|| dirs::home_dir().map(|d| d.join(".claude.json"))),
            ConfigFormat::ClaudeCode,
        ),
        (
            "Codex",
            Box::new(|| dirs::home_dir().map(|d| d.join(".codex").join("config.toml"))),
            ConfigFormat::CodexToml,
        ),
    ]
}

// ── Credential forwarding ────────────────────────────────────────

/// LLM provider credential environment variables forwarded to stdio subprocesses.
/// Keeps in sync with the providers registered in the setup wizard.
pub(crate) const CREDENTIAL_VARS: &[&str] = &[
    // AWS Bedrock
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_REGION",
    "AWS_REGION",
    // API providers
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

/// Build a JSON object of credential env vars for injection into stdio subprocess configs.
///
/// Resolution order per variable:
///   1. Live env var (highest — allows CI/CD override)
///   2. Value from `~/.config/thinkingroot/credentials.toml` (set by `root setup`)
///
/// This ensures that tools like Claude Desktop get the key even when the user's
/// shell profile hasn't exported it — which is the common case for GUI apps.
fn credential_env_json() -> serde_json::Map<String, Value> {
    // Load stored credentials once; fall back to empty map on any error.
    let stored = Credentials::load().unwrap_or_default();

    let mut map = serde_json::Map::new();
    for var in CREDENTIAL_VARS {
        // Priority 1: live env var
        let value = std::env::var(var)
            .ok()
            .filter(|v| !v.is_empty())
            // Priority 2: credentials.toml
            .or_else(|| stored.get(var).map(str::to_string));

        if let Some(val) = value {
            map.insert(var.to_string(), json!(val));
        }
    }
    map
}

/// Build the stdio MCP entry object for JSON-based config files.
///
/// VS Code requires an explicit `"type": "stdio"` field; all other tools infer
/// the transport from the presence of a `command` key.
fn stdio_entry(bin_path: &str, workspace_path: &str, needs_type_field: bool) -> Value {
    let env_obj = credential_env_json();
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
    if !env_obj.is_empty() {
        entry["env"] = json!(env_obj);
    }
    entry
}

// ── JSON helpers (pub for tests) ─────────────────────────────────

pub fn apply_entry(existing: &mut Value, format: ConfigFormat, port: u16) {
    let servers_key = match format {
        ConfigFormat::McpServers | ConfigFormat::ContinueDev => "mcpServers",
        ConfigFormat::Servers => "servers",
        ConfigFormat::ContextServers => "context_servers",
        ConfigFormat::GeminiCli => "mcpServers",
        // These formats use dedicated write functions — not apply_entry.
        ConfigFormat::ClaudeCode | ConfigFormat::CodexToml => return,
    };

    let bin_path = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("root"))
        .to_string_lossy()
        .into_owned();
    let workspace_path = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .into_owned();

    let entry = match format {
        // Gemini CLI is HTTP-only; no stdio subprocess support.
        ConfigFormat::GeminiCli => json!({
            "httpUrl": format!("http://localhost:{}/mcp/sse", port)
        }),
        // VS Code requires an explicit "type": "stdio" field.
        ConfigFormat::Servers | ConfigFormat::ContinueDev => {
            stdio_entry(&bin_path, &workspace_path, true)
        }
        // All other tools (Cursor, Windsurf, Cline, Zed, Claude Desktop, Antigravity)
        // infer the transport from the presence of a "command" key — no type field needed.
        _ => stdio_entry(&bin_path, &workspace_path, false),
    };

    if !existing[servers_key].is_object() {
        existing[servers_key] = json!({});
    }
    existing[servers_key]["thinkingroot"] = entry;
}

pub fn remove_entry(existing: &mut Value, format: ConfigFormat) {
    let servers_key = match format {
        ConfigFormat::McpServers | ConfigFormat::ContinueDev => "mcpServers",
        ConfigFormat::Servers => "servers",
        ConfigFormat::ContextServers => "context_servers",
        ConfigFormat::GeminiCli => "mcpServers",
        ConfigFormat::ClaudeCode | ConfigFormat::CodexToml => return,
    };
    if let Some(obj) = existing[servers_key].as_object_mut() {
        obj.remove("thinkingroot");
    }
}

// ── File I/O ─────────────────────────────────────────────────────

pub fn write_tool_config(
    tool: &DetectedTool,
    port: u16,
    dry_run: bool,
) -> anyhow::Result<WriteResult> {
    match tool.format {
        ConfigFormat::ClaudeCode => return write_claude_code_config(tool, port, dry_run),
        ConfigFormat::CodexToml => return write_codex_config(tool, port, dry_run),
        _ => {}
    }

    let path = &tool.config_path;

    let mut existing: Value = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).unwrap_or(json!({}))
    } else {
        json!({})
    };

    apply_entry(&mut existing, tool.format, port);
    let json_out = serde_json::to_string_pretty(&existing)?;

    if dry_run {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::DryRun(json_out),
        });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, &json_out)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteResult {
        tool: tool.name,
        path: path.clone(),
        action: WriteAction::Written,
    })
}

pub fn remove_tool_config(tool: &DetectedTool, dry_run: bool) -> anyhow::Result<WriteResult> {
    match tool.format {
        ConfigFormat::ClaudeCode => return remove_claude_code_config(tool, dry_run),
        ConfigFormat::CodexToml => return remove_codex_config(tool, dry_run),
        _ => {}
    }

    let path = &tool.config_path;
    if !path.exists() {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::Skipped("config file not found"),
        });
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut existing: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
    remove_entry(&mut existing, tool.format);
    let json_out = serde_json::to_string_pretty(&existing)?;

    if dry_run {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::DryRun(json_out),
        });
    }

    std::fs::write(path, &json_out)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteResult {
        tool: tool.name,
        path: path.clone(),
        action: WriteAction::Removed,
    })
}

// ── Claude Code: per-project stdio config in ~/.claude.json ─────

pub fn apply_claude_code_entry(existing: &mut Value, _port: u16, project_dir: &str) {
    if !existing["projects"].is_object() {
        existing["projects"] = json!({});
    }
    if !existing["projects"][project_dir].is_object() {
        existing["projects"][project_dir] = json!({});
    }
    if !existing["projects"][project_dir]["mcpServers"].is_object() {
        existing["projects"][project_dir]["mcpServers"] = json!({});
    }

    let bin_path = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("root"))
        .to_string_lossy()
        .into_owned();
    let workspace_path = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .into_owned();

    // Claude Code CLI infers stdio from "command" key — no "type" field needed.
    existing["projects"][project_dir]["mcpServers"]["thinkingroot"] =
        stdio_entry(&bin_path, &workspace_path, false);
}

pub fn remove_claude_code_entry(existing: &mut Value, project_dir: &str) {
    if let Some(proj) = existing
        .get_mut("projects")
        .and_then(|p| p.get_mut(project_dir))
        .and_then(|p| p.get_mut("mcpServers"))
        .and_then(|s| s.as_object_mut())
    {
        proj.remove("thinkingroot");
    }
}

fn write_claude_code_config(
    tool: &DetectedTool,
    port: u16,
    dry_run: bool,
) -> anyhow::Result<WriteResult> {
    let path = &tool.config_path;

    let mut existing: Value = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).unwrap_or(json!({}))
    } else {
        json!({})
    };

    let cwd = std::env::current_dir()
        .context("failed to resolve current directory")?
        .display()
        .to_string();

    apply_claude_code_entry(&mut existing, port, &cwd);
    let json_out = serde_json::to_string_pretty(&existing)?;

    if dry_run {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::DryRun(json_out),
        });
    }

    std::fs::write(path, &json_out)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteResult {
        tool: tool.name,
        path: path.clone(),
        action: WriteAction::Written,
    })
}

fn remove_claude_code_config(tool: &DetectedTool, dry_run: bool) -> anyhow::Result<WriteResult> {
    let path = &tool.config_path;
    if !path.exists() {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::Skipped("config file not found"),
        });
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut existing: Value = serde_json::from_str(&raw).unwrap_or(json!({}));

    let cwd = std::env::current_dir()
        .context("failed to resolve current directory")?
        .display()
        .to_string();

    remove_claude_code_entry(&mut existing, &cwd);
    let json_out = serde_json::to_string_pretty(&existing)?;

    if dry_run {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::DryRun(json_out),
        });
    }

    std::fs::write(path, &json_out)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteResult {
        tool: tool.name,
        path: path.clone(),
        action: WriteAction::Removed,
    })
}

// ── Codex CLI: TOML config at ~/.codex/config.toml ──────────────

fn write_codex_config(
    tool: &DetectedTool,
    _port: u16,
    dry_run: bool,
) -> anyhow::Result<WriteResult> {
    let path = &tool.config_path;

    let mut doc: toml::Value = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        raw.parse::<toml::Value>()
            .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::Value::Table(toml::map::Map::new())
    };

    let bin_path = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("root"))
        .to_string_lossy()
        .into_owned();
    let workspace_path = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .into_owned();

    apply_codex_entry(&mut doc, &bin_path, &workspace_path);
    let toml_out = toml::to_string_pretty(&doc).with_context(|| "failed to serialize TOML")?;

    if dry_run {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::DryRun(toml_out),
        });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, &toml_out)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteResult {
        tool: tool.name,
        path: path.clone(),
        action: WriteAction::Written,
    })
}

pub fn apply_codex_entry(doc: &mut toml::Value, bin_path: &str, workspace_path: &str) {
    let root = doc.as_table_mut().expect("TOML root must be a table");

    if !root.contains_key("mcp_servers") {
        root.insert(
            "mcp_servers".to_string(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }

    let mcp_servers = root
        .get_mut("mcp_servers")
        .and_then(|v| v.as_table_mut())
        .expect("mcp_servers must be a table");

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
    // Forward credential env vars so the subprocess can reach LLM providers
    // even when Codex is launched outside a shell (e.g., as a GUI app).
    // Uses the same two-level resolution as credential_env_json().
    let stored = Credentials::load().unwrap_or_default();
    let mut env_map = toml::map::Map::new();
    for var in CREDENTIAL_VARS {
        let value = std::env::var(var)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| stored.get(var).map(str::to_string));
        if let Some(val) = value {
            env_map.insert(var.to_string(), toml::Value::String(val));
        }
    }
    if !env_map.is_empty() {
        entry.insert("env".to_string(), toml::Value::Table(env_map));
    }
    mcp_servers.insert("thinkingroot".to_string(), toml::Value::Table(entry));
}

pub fn remove_codex_entry(doc: &mut toml::Value) {
    if let Some(mcp) = doc
        .as_table_mut()
        .and_then(|root| root.get_mut("mcp_servers"))
        .and_then(|v| v.as_table_mut())
    {
        mcp.remove("thinkingroot");
    }
}

fn remove_codex_config(tool: &DetectedTool, dry_run: bool) -> anyhow::Result<WriteResult> {
    let path = &tool.config_path;
    if !path.exists() {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::Skipped("config file not found"),
        });
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc: toml::Value = raw
        .parse()
        .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()));

    remove_codex_entry(&mut doc);
    let toml_out = toml::to_string_pretty(&doc).with_context(|| "failed to serialize TOML")?;

    if dry_run {
        return Ok(WriteResult {
            tool: tool.name,
            path: path.clone(),
            action: WriteAction::DryRun(toml_out),
        });
    }

    std::fs::write(path, &toml_out)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(WriteResult {
        tool: tool.name,
        path: path.clone(),
        action: WriteAction::Removed,
    })
}

// ── Port helpers ─────────────────────────────────────────────────

fn is_port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Scan upward from `start` and return the first unoccupied port.
fn find_available_port(start: u16) -> u16 {
    (start..=start.saturating_add(100))
        .find(|&p| is_port_available(p))
        .unwrap_or(start)
}

/// Returns true for all formats that use a stdio subprocess.
/// Only Gemini CLI is HTTP-only (no stdio support).
fn is_http_only_tool(format: ConfigFormat) -> bool {
    matches!(format, ConfigFormat::GeminiCli)
}

// ── run_connect entry point ───────────────────────────────────────

pub fn run_connect(
    tool_filter: Option<&str>,
    port: u16,
    dry_run: bool,
    remove: bool,
) -> anyhow::Result<()> {
    println!();
    println!("  {} AI tools...", style("Scanning for").cyan().bold());
    println!();

    let all_tools = detect_tools();
    if all_tools.is_empty() {
        println!("  No supported AI tools detected.");
        println!(
            "  Supported: Claude Desktop, Claude Code, Cursor, VS Code, Windsurf, Zed, Cline, Continue.dev, Antigravity, Gemini CLI, Codex"
        );
        return Ok(());
    }

    let tools_to_process: Vec<&DetectedTool> = match tool_filter {
        Some(filter) => {
            let filtered: Vec<&DetectedTool> = all_tools
                .iter()
                .filter(|t| t.name.to_lowercase().contains(&filter.to_lowercase()))
                .collect();
            if filtered.is_empty() {
                anyhow::bail!(
                    "no tool matching '{}' detected. Run `root connect` to see all detected tools.",
                    filter
                );
            }
            filtered
        }
        None => all_tools.iter().collect(),
    };

    // Only HTTP-only tools (Gemini CLI) need a running server on a port.
    let has_http_tools = tools_to_process.iter().any(|t| is_http_only_tool(t.format));
    let effective_port = if !remove && has_http_tools && !is_port_available(port) {
        let next = find_available_port(port + 1);
        println!(
            "  {} Port {} is in use — using {} instead\n",
            style("!").yellow().bold(),
            port,
            style(next).cyan()
        );
        next
    } else {
        port
    };

    if dry_run {
        println!(
            "  {} (no files will be changed)\n",
            style("Dry run").yellow().bold()
        );
    }

    let mut stdio_connected = false;
    let mut http_connected = false;

    for tool in &tools_to_process {
        let result = if remove {
            remove_tool_config(tool, dry_run)?
        } else {
            write_tool_config(tool, effective_port, dry_run)?
        };

        match &result.action {
            WriteAction::Written => {
                println!(
                    "  {} {:<20} → {}",
                    style("✓").green().bold(),
                    result.tool,
                    style(result.path.display()).dim()
                );
                if is_http_only_tool(tool.format) {
                    http_connected = true;
                } else {
                    stdio_connected = true;
                }
            }
            WriteAction::DryRun(content) => {
                println!(
                    "  {} {:<20} → {} (would write)",
                    style("~").yellow().bold(),
                    result.tool,
                    style(result.path.display()).dim()
                );
                println!("{}", style(content).dim());
            }
            WriteAction::Removed => println!(
                "  {} {:<20} → entry removed",
                style("✓").green().bold(),
                result.tool
            ),
            WriteAction::Skipped(reason) => println!(
                "  {} {:<20} → {}",
                style("!").yellow().bold(),
                result.tool,
                reason
            ),
        }
    }

    if !dry_run && !remove {
        println!();
        if stdio_connected {
            println!("  Stdio tools connected — no server needed, spawned per session.");
        }
        if http_connected {
            println!(
                "  Gemini CLI connected to {}",
                style(format!("http://localhost:{}/mcp/sse", effective_port)).cyan()
            );
            println!(
                "  Start the server: {}",
                style("root serve --port <port> --path <workspace>").dim()
            );
        }
        println!("  Restart your AI tools to pick up the new config.");
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Serialise tests that mutate process-global environment variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ── apply_entry: McpServers (Cursor, Windsurf, etc.) ────────────

    #[test]
    fn mcpservers_entry_has_command_and_args() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        let entry = &existing["mcpServers"]["thinkingroot"];
        assert!(entry["command"].is_string(), "command must be a string");
        let args: Vec<&str> = entry["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(args[0], "serve");
        assert_eq!(args[1], "--mcp-stdio");
        assert_eq!(args[2], "--path");
        // No "type" field — inferred from presence of "command"
        assert!(
            entry["type"].is_null(),
            "McpServers must not have a type field"
        );
        // No "url" field
        assert!(
            entry["url"].is_null(),
            "McpServers must not have a url field"
        );
    }

    #[test]
    fn mcpservers_preserves_existing_servers() {
        let mut existing = json!({
            "mcpServers": {
                "github": { "command": "npx", "args": ["-y", "@github/mcp"] }
            }
        });
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        assert!(
            existing["mcpServers"]["github"].is_object(),
            "existing server preserved"
        );
        assert!(existing["mcpServers"]["thinkingroot"]["command"].is_string());
    }

    // ── apply_entry: Servers (VS Code) ───────────────────────────────

    #[test]
    fn servers_entry_has_type_stdio() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::Servers, 3001);
        let entry = &existing["servers"]["thinkingroot"];
        assert_eq!(
            entry["type"], "stdio",
            "VS Code requires explicit type:stdio"
        );
        assert!(entry["command"].is_string());
        assert!(entry["url"].is_null(), "must not have url field");
    }

    // ── apply_entry: ContextServers (Zed) ───────────────────────────

    #[test]
    fn context_servers_entry_has_command_no_type() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::ContextServers, 3000);
        let entry = &existing["context_servers"]["thinkingroot"];
        assert!(entry["command"].is_string());
        assert!(
            entry["type"].is_null(),
            "Zed infers transport from command key — no type field"
        );
        assert!(entry["url"].is_null());
    }

    // ── apply_entry: ContinueDev ─────────────────────────────────────

    #[test]
    fn continue_dev_entry_has_type_stdio() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::ContinueDev, 3000);
        let entry = &existing["mcpServers"]["thinkingroot"];
        assert_eq!(entry["type"], "stdio");
        assert!(entry["command"].is_string());
    }

    // ── apply_entry: GeminiCli (HTTP-only) ──────────────────────────

    #[test]
    fn gemini_cli_entry_uses_http_url_key() {
        let mut existing = json!({ "theme": "Default" });
        apply_entry(&mut existing, ConfigFormat::GeminiCli, 3000);
        assert_eq!(
            existing["mcpServers"]["thinkingroot"]["httpUrl"],
            "http://localhost:3000/mcp/sse"
        );
        assert!(
            existing["mcpServers"]["thinkingroot"]["url"].is_null(),
            "must use httpUrl not url"
        );
        assert!(
            existing["mcpServers"]["thinkingroot"]["command"].is_null(),
            "no command for Gemini CLI"
        );
        assert_eq!(existing["theme"], "Default", "other settings preserved");
    }

    #[test]
    fn gemini_cli_remove_leaves_other_servers() {
        let mut existing = json!({
            "mcpServers": {
                "other": { "httpUrl": "http://example.com" },
                "thinkingroot": { "httpUrl": "http://localhost:3000/mcp/sse" }
            }
        });
        remove_entry(&mut existing, ConfigFormat::GeminiCli);
        assert!(existing["mcpServers"]["other"].is_object());
        assert!(existing["mcpServers"]["thinkingroot"].is_null());
    }

    // ── remove_entry ────────────────────────────────────────────────

    #[test]
    fn remove_entry_leaves_other_servers_intact() {
        let mut existing = json!({
            "mcpServers": {
                "github": { "command": "npx" },
                "thinkingroot": { "command": "/usr/local/bin/root" }
            }
        });
        remove_entry(&mut existing, ConfigFormat::McpServers);
        assert!(existing["mcpServers"]["github"].is_object());
        assert!(existing["mcpServers"]["thinkingroot"].is_null());
    }

    #[test]
    fn merge_into_empty_file() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        assert!(existing["mcpServers"]["thinkingroot"].is_object());
    }

    // ── Claude Code: per-project stdio config ───────────────────────

    #[test]
    fn claude_code_entry_is_stdio_not_sse() {
        let mut existing = json!({
            "numStartups": 10,
            "projects": {
                "/other/project": { "mcpServers": { "github": {} } }
            }
        });
        apply_claude_code_entry(&mut existing, 3000, "/my/workspace");
        let entry = &existing["projects"]["/my/workspace"]["mcpServers"]["thinkingroot"];
        assert!(
            entry["command"].is_string(),
            "Claude Code must use stdio command"
        );
        let args: Vec<&str> = entry["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(args[0], "serve");
        assert_eq!(args[1], "--mcp-stdio");
        // No SSE url
        assert!(
            entry["url"].is_null(),
            "Claude Code must not have url field"
        );
        assert!(
            entry["type"].is_null(),
            "Claude Code infers stdio from command key"
        );
        // Other project preserved
        assert!(existing["projects"]["/other/project"]["mcpServers"]["github"].is_object());
        assert_eq!(existing["numStartups"], 10);
    }

    #[test]
    fn claude_code_remove_leaves_other_servers() {
        let mut existing = json!({
            "projects": {
                "/my/ws": {
                    "mcpServers": {
                        "github": {},
                        "thinkingroot": { "command": "/usr/local/bin/root" }
                    }
                }
            }
        });
        remove_claude_code_entry(&mut existing, "/my/ws");
        assert!(existing["projects"]["/my/ws"]["mcpServers"]["github"].is_object());
        assert!(existing["projects"]["/my/ws"]["mcpServers"]["thinkingroot"].is_null());
    }

    // ── Codex TOML ──────────────────────────────────────────────────

    #[test]
    fn codex_toml_inserts_mcp_server_entry() {
        let input = r#"
model = "gpt-4o"

[mcp_servers.playwright]
command = "npx"
args = ["@playwright/mcp@latest"]
"#;
        let mut doc: toml::Value = input.parse().unwrap();
        apply_codex_entry(&mut doc, "/usr/local/bin/root", "/workspace");
        let root = doc.as_table().unwrap();
        let mcp = root["mcp_servers"].as_table().unwrap();
        assert_eq!(
            mcp["thinkingroot"]["command"].as_str().unwrap(),
            "/usr/local/bin/root"
        );
        let args: Vec<&str> = mcp["thinkingroot"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(args, ["serve", "--mcp-stdio", "--path", "/workspace"]);
        assert!(mcp["playwright"].is_table(), "existing server preserved");
        assert_eq!(
            root["model"].as_str().unwrap(),
            "gpt-4o",
            "top-level key preserved"
        );
    }

    #[test]
    fn codex_toml_remove_leaves_other_servers() {
        let input = r#"
[mcp_servers.playwright]
command = "npx"

[mcp_servers.thinkingroot]
command = "/usr/local/bin/root"
args = ["serve", "--mcp-stdio", "--path", "/workspace"]
"#;
        let mut doc: toml::Value = input.parse().unwrap();
        remove_codex_entry(&mut doc);
        let mcp = doc["mcp_servers"].as_table().unwrap();
        assert!(mcp.contains_key("playwright"));
        assert!(!mcp.contains_key("thinkingroot"));
    }

    #[test]
    fn codex_toml_captures_env_vars_when_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let test_key = "AWS_ACCESS_KEY_ID";
        let test_value = "AKIAIOSFODNN7EXAMPLE";
        let original_val = std::env::var(test_key).ok();
        unsafe {
            std::env::set_var(test_key, test_value);
        }

        let mut doc: toml::Value = toml::Value::Table(toml::map::Map::new());
        apply_codex_entry(&mut doc, "/usr/local/bin/root", "/workspace");
        let mcp = doc["mcp_servers"]["thinkingroot"].as_table().unwrap();
        assert_eq!(mcp["command"].as_str().unwrap(), "/usr/local/bin/root");
        assert!(
            mcp.contains_key("env"),
            "env table should exist when credentials are set"
        );
        assert_eq!(mcp["env"][test_key].as_str().unwrap(), test_value);

        unsafe {
            if let Some(val) = original_val {
                std::env::set_var(test_key, val);
            } else {
                std::env::remove_var(test_key);
            }
        }
    }

    #[test]
    fn codex_toml_omits_env_table_when_no_credentials_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original_vals: Vec<(String, Option<String>)> = CREDENTIAL_VARS
            .iter()
            .map(|v| (v.to_string(), std::env::var(v).ok()))
            .collect();
        unsafe {
            for v in CREDENTIAL_VARS {
                std::env::remove_var(v);
            }
            // Also override the credentials file path to a non-existent temp dir
            // so that stored credentials on the developer's machine don't leak into the test.
            std::env::set_var("HOME", std::env::temp_dir().join("thinkingroot-test-empty"));
        }

        let mut doc: toml::Value = toml::Value::Table(toml::map::Map::new());
        apply_codex_entry(&mut doc, "/usr/local/bin/root", "/workspace");
        let mcp = doc["mcp_servers"]["thinkingroot"].as_table().unwrap();
        assert!(
            !mcp.contains_key("env"),
            "env table should be absent when no credentials are set"
        );

        unsafe {
            for (var_name, original_val) in original_vals {
                if let Some(val) = original_val {
                    std::env::set_var(&var_name, val);
                } else {
                    std::env::remove_var(&var_name);
                }
            }
            std::env::remove_var("HOME");
        }
    }
}
