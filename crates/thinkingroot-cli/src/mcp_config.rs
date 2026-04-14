use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use serde_json::{Value, json};

/// The JSON key / format that each tool uses for MCP server configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConfigFormat {
    /// `{ "mcpServers": { "thinkingroot": { "url": "..." } } }`
    McpServers,
    /// `{ "servers": { "thinkingroot": { "type": "sse", "url": "..." } } }`
    Servers,
    /// `{ "context_servers": { "thinkingroot": { "url": "..." } } }`
    ContextServers,
    /// Individual file, same JSON as McpServers
    ContinueDev,
    /// Claude Code CLI: `~/.claude.json` with per-project `mcpServers` nesting
    ClaudeCode,
    /// OpenAI Codex CLI: `~/.codex/config.toml` (TOML format)
    CodexToml,
    /// Gemini CLI (~/.gemini/settings.json): uses mcpServers key with httpUrl instead of url
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

    let entry = match format {
        ConfigFormat::GeminiCli => json!({
            "httpUrl": format!("http://localhost:{}/mcp/sse", port)
        }),
        ConfigFormat::ContextServers => json!({
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
        _ => json!({
            "type": "sse",
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
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

// ── Claude Code: per-project config in ~/.claude.json ────────────

pub fn apply_claude_code_entry(existing: &mut Value, port: u16, project_dir: &str) {
    if !existing["projects"].is_object() {
        existing["projects"] = json!({});
    }
    if !existing["projects"][project_dir].is_object() {
        existing["projects"][project_dir] = json!({});
    }
    if !existing["projects"][project_dir]["mcpServers"].is_object() {
        existing["projects"][project_dir]["mcpServers"] = json!({});
    }
    existing["projects"][project_dir]["mcpServers"]["thinkingroot"] = json!({
        "type": "sse",
        "url": format!("http://localhost:{}/mcp/sse", port)
    });
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
    ];
    let mut env_map = toml::map::Map::new();
    for var in CREDENTIAL_VARS {
        if let Ok(val) = std::env::var(var) {
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

fn is_stdio_tool(format: ConfigFormat) -> bool {
    matches!(format, ConfigFormat::ClaudeCode | ConfigFormat::CodexToml)
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

    // For SSE tools, check if the requested port is free and find one that is.
    let has_sse_tools = tools_to_process.iter().any(|t| !is_stdio_tool(t.format));
    let effective_port = if !remove && has_sse_tools && !is_port_available(port) {
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

    let mut sse_connected = false;
    let mut stdio_connected = false;

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
                if is_stdio_tool(tool.format) {
                    stdio_connected = true;
                } else {
                    sse_connected = true;
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
        if sse_connected {
            println!(
                "  SSE tools connected to {}",
                style(format!("http://localhost:{}/mcp/sse", effective_port)).cyan()
            );
        }
        if stdio_connected {
            println!("  Stdio tools (Codex, Claude Code) connected via subprocess.");
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

    #[test]
    fn merge_mcp_servers_inserts_entry_preserving_others() {
        let mut existing = json!({
            "mcpServers": {
                "github": { "command": "npx", "args": ["-y", "@github/mcp"] }
            }
        });
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        assert!(existing["mcpServers"]["github"].is_object());
        assert_eq!(
            existing["mcpServers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
    }

    #[test]
    fn merge_servers_format_for_vscode() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::Servers, 3001);
        assert_eq!(existing["servers"]["thinkingroot"]["type"], "sse");
        assert_eq!(
            existing["servers"]["thinkingroot"]["url"],
            "http://localhost:3001/mcp/sse"
        );
    }

    #[test]
    fn merge_context_servers_format_for_zed() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::ContextServers, 3000);
        assert_eq!(
            existing["context_servers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
        // Zed uses context_servers format which does not use a "type" discriminator
        assert!(existing["context_servers"]["thinkingroot"]["type"].is_null());
    }

    #[test]
    fn remove_entry_leaves_other_servers_intact() {
        let mut existing = json!({
            "mcpServers": {
                "github": { "command": "npx" },
                "thinkingroot": { "url": "http://localhost:3000/mcp/sse" }
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

    #[test]
    fn claude_code_inserts_project_scoped_entry() {
        let mut existing = json!({
            "numStartups": 10,
            "projects": {
                "/other/project": { "mcpServers": { "github": {} } }
            }
        });
        apply_claude_code_entry(&mut existing, 3000, "/my/workspace");
        assert_eq!(
            existing["projects"]["/my/workspace"]["mcpServers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
        assert_eq!(
            existing["projects"]["/my/workspace"]["mcpServers"]["thinkingroot"]["type"],
            "sse"
        );
        // Other project and top-level keys preserved.
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
                        "thinkingroot": { "type": "sse", "url": "http://localhost:3000/mcp/sse" }
                    }
                }
            }
        });
        remove_claude_code_entry(&mut existing, "/my/ws");
        assert!(existing["projects"]["/my/ws"]["mcpServers"]["github"].is_object());
        assert!(existing["projects"]["/my/ws"]["mcpServers"]["thinkingroot"].is_null());
    }

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
        // Existing server preserved.
        assert!(mcp["playwright"].is_table());
        // Top-level key preserved.
        assert_eq!(root["model"].as_str().unwrap(), "gpt-4o");
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
    fn mcp_servers_entry_includes_type_sse() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::McpServers, 3000);
        assert_eq!(existing["mcpServers"]["thinkingroot"]["type"], "sse");
        assert_eq!(
            existing["mcpServers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
    }

    #[test]
    fn continue_dev_entry_includes_type_sse() {
        let mut existing = json!({});
        apply_entry(&mut existing, ConfigFormat::ContinueDev, 3000);
        assert_eq!(existing["mcpServers"]["thinkingroot"]["type"], "sse");
        assert_eq!(
            existing["mcpServers"]["thinkingroot"]["url"],
            "http://localhost:3000/mcp/sse"
        );
    }

    #[test]
    fn gemini_cli_entry_uses_http_url_key() {
        let mut existing = json!({
            "theme": "Default"
        });
        apply_entry(&mut existing, ConfigFormat::GeminiCli, 3000);
        // Must use "httpUrl", not "url"
        assert_eq!(
            existing["mcpServers"]["thinkingroot"]["httpUrl"],
            "http://localhost:3000/mcp/sse"
        );
        // Must NOT have a "url" key
        assert!(existing["mcpServers"]["thinkingroot"]["url"].is_null());
        // Other settings preserved
        assert_eq!(existing["theme"], "Default");
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

    #[test]
    fn codex_toml_captures_env_vars_when_set() {
        // This test verifies that when credential env vars are set,
        // they get captured in the Codex TOML env table.
        // We check with AWS_ACCESS_KEY_ID since it's part of the CREDENTIAL_VARS list.
        let test_key = "AWS_ACCESS_KEY_ID";
        let test_value = "AKIAIOSFODNN7EXAMPLE";

        let original_val = std::env::var(test_key).ok();
        unsafe {
            std::env::set_var(test_key, test_value);
        }

        let input = r#"model = "gpt-4o""#;
        let mut doc: toml::Value = input.parse().unwrap();
        apply_codex_entry(&mut doc, "/usr/local/bin/root", "/workspace");

        let mcp = doc["mcp_servers"]["thinkingroot"].as_table().unwrap();

        // Verify basic structure: command and args exist
        assert_eq!(
            mcp["command"].as_str().unwrap(),
            "/usr/local/bin/root"
        );

        // When credentials are set, env table should be populated
        assert!(
            mcp.contains_key("env"),
            "env table should exist when credentials are set"
        );
        assert_eq!(
            mcp["env"][test_key].as_str().unwrap(),
            test_value,
            "{} should be captured in env table", test_key
        );

        // Clean up: restore original state
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
        // This test verifies that when NO credential env vars are set,
        // the env table is omitted from the TOML (not included if empty).
        // We save and restore the state of all credential vars for this test.
        const VARS: &[&str] = &[
            "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN",
            "AWS_PROFILE", "AWS_DEFAULT_REGION", "AWS_REGION",
            "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GROQ_API_KEY", "DEEPSEEK_API_KEY",
        ];

        // Save original state
        let original_vals: Vec<(String, Option<String>)> = VARS
            .iter()
            .map(|v| (v.to_string(), std::env::var(v).ok()))
            .collect();

        // Remove all credential vars for this test
        unsafe {
            for v in VARS {
                std::env::remove_var(v);
            }
        }

        let mut doc: toml::Value = toml::Value::Table(toml::map::Map::new());
        apply_codex_entry(&mut doc, "/usr/local/bin/root", "/workspace");

        let mcp = doc["mcp_servers"]["thinkingroot"].as_table().unwrap();
        assert!(
            !mcp.contains_key("env"),
            "env table should be absent when no credentials are set"
        );

        // Restore original state
        unsafe {
            for (var_name, original_val) in original_vals {
                if let Some(val) = original_val {
                    std::env::set_var(&var_name, val);
                } else {
                    std::env::remove_var(&var_name);
                }
            }
        }
    }
}
