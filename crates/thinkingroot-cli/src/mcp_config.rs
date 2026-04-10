use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use serde_json::{json, Value};

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
            path_fn().map(|path| DetectedTool { name, config_path: path, format })
        })
        .filter(|t| {
            // Detect by parent directory existing (file itself may not exist yet)
            t.config_path.parent().map(|p| p.exists()).unwrap_or(false)
        })
        .collect()
}

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
            Box::new(|| {
                dirs::config_dir().map(|d| d.join("Code").join("User").join("mcp.json"))
            }),
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
                dirs::home_dir()
                    .map(|d| d.join(".continue").join("mcpServers").join("thinkingroot.json"))
            }),
            ConfigFormat::ContinueDev,
        ),
    ]
}

// ── JSON helpers (pub for tests) ─────────────────────────────────

pub fn apply_entry(existing: &mut Value, format: ConfigFormat, port: u16) {
    let servers_key = match format {
        ConfigFormat::McpServers | ConfigFormat::ContinueDev => "mcpServers",
        ConfigFormat::Servers => "servers",
        ConfigFormat::ContextServers => "context_servers",
    };

    let entry = match format {
        ConfigFormat::Servers => json!({
            "type": "sse",
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
        _ => json!({
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
    };
    if let Some(obj) = existing[servers_key].as_object_mut() {
        obj.remove("thinkingroot");
    }
}

// ── File I/O ─────────────────────────────────────────────────────

pub fn write_tool_config(tool: &DetectedTool, port: u16, dry_run: bool) -> anyhow::Result<WriteResult> {
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

    Ok(WriteResult { tool: tool.name, path: path.clone(), action: WriteAction::Written })
}

pub fn remove_tool_config(tool: &DetectedTool, dry_run: bool) -> anyhow::Result<WriteResult> {
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

    Ok(WriteResult { tool: tool.name, path: path.clone(), action: WriteAction::Removed })
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
        println!("  Supported: Claude Desktop, Cursor, VS Code, Windsurf, Zed, Cline, Continue.dev");
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

    if dry_run {
        println!("  {} (no files will be changed)\n", style("Dry run").yellow().bold());
    }

    for tool in tools_to_process {
        let result = if remove {
            remove_tool_config(tool, dry_run)?
        } else {
            write_tool_config(tool, port, dry_run)?
        };

        match &result.action {
            WriteAction::Written => println!(
                "  {} {:<20} → {}",
                style("✓").green().bold(),
                result.tool,
                style(result.path.display()).dim()
            ),
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
        println!(
            "  All connected to {}",
            style(format!("http://localhost:{}/mcp/sse", port)).cyan()
        );
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
}
