//! Clean-room reimplementation. Inspired by openhuman/mcp_client/registry.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.5 (2026-05-17) — `root mcp add/list/remove` CLI surface.
//!
//! Reads + writes the per-workspace `.thinkingroot/mcp-servers.toml`
//! file. No network, no engine, no subprocess spawn — purely
//! config-file manipulation. The actual MCP servers are spawned by
//! the daemon on next mount (or on next compile, whichever first
//! triggers `external_registry::load_global_from_workspace_config`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use thinkingroot_serve::mcp::external_registry::{
    AuthEntry, McpServersConfig, ServerEntry, TransportKind,
};

/// `root mcp add <name> -- <command> [args...]`
pub fn add(name: &str, workspace: &Path, command_and_args: &[String]) -> Result<()> {
    if name.is_empty() {
        bail!("server name must not be empty");
    }
    if command_and_args.is_empty() {
        bail!(
            "no command provided. Usage:\n  root mcp add <name> -- <command> [args...]"
        );
    }
    let mut config = load_or_empty(workspace)?;
    if config.server.iter().any(|s| s.name == name) {
        bail!(
            "server '{name}' is already registered in {}. \
             Remove it first via `root mcp remove {name}`.",
            config_path(workspace).display()
        );
    }
    let command = command_and_args[0].clone();
    let args: Vec<String> = command_and_args[1..].to_vec();
    config.server.push(ServerEntry {
        name: name.to_string(),
        transport: TransportKind::Stdio,
        command: Some(command),
        args,
        env: Default::default(),
        cwd: None,
        endpoint: None,
        timeout_secs: None,
        auth: None,
        oauth_provider: None,
    });
    write(workspace, &config)?;
    println!(
        "✓ Registered MCP server '{name}' in {}\n  Tools appear as `{name}::<tool>` in `tools/list`.\n  Restart the daemon (or remount the workspace) to load the new server.",
        config_path(workspace).display()
    );
    Ok(())
}

pub fn list(workspace: &Path) -> Result<()> {
    let config = load_or_empty(workspace)?;
    if config.server.is_empty() {
        println!(
            "No external MCP servers registered in this workspace.\n  Config file: {}",
            config_path(workspace).display()
        );
        return Ok(());
    }
    println!(
        "External MCP servers registered in {}:",
        config_path(workspace).display()
    );
    for s in &config.server {
        let desc = match s.transport {
            TransportKind::Stdio => format!(
                "stdio: {} {}",
                s.command.as_deref().unwrap_or("(no command)"),
                s.args.join(" ")
            ),
            TransportKind::Http => format!(
                "http: {}",
                s.endpoint.as_deref().unwrap_or("(no endpoint)")
            ),
        };
        println!("  • {} → {desc}", s.name);
        if let Some(a) = &s.auth {
            let kind = match a.kind {
                thinkingroot_serve::mcp::external_registry::AuthKind::Bearer => "bearer",
                thinkingroot_serve::mcp::external_registry::AuthKind::ApiKey => "x-api-key",
            };
            // Don't leak the token. Show kind + redacted form.
            let redacted = redact_token(&a.token);
            println!("    auth: {kind} {redacted}");
        }
    }
    let _ = AuthEntry::default_redaction_marker();
    Ok(())
}

pub fn remove(name: &str, workspace: &Path) -> Result<()> {
    let mut config = load_or_empty(workspace)?;
    let before = config.server.len();
    config.server.retain(|s| s.name != name);
    if config.server.len() == before {
        bail!("server '{name}' not found in {}", config_path(workspace).display());
    }
    write(workspace, &config)?;
    println!(
        "✓ Removed MCP server '{name}' from {}",
        config_path(workspace).display()
    );
    Ok(())
}

fn config_path(workspace: &Path) -> PathBuf {
    workspace.join(".thinkingroot").join("mcp-servers.toml")
}

fn load_or_empty(workspace: &Path) -> Result<McpServersConfig> {
    let path = config_path(workspace);
    if !path.exists() {
        return Ok(McpServersConfig::default());
    }
    let bytes = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn write(workspace: &Path, config: &McpServersConfig) -> Result<()> {
    let path = config_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(config)
        .with_context(|| "serialise mcp-servers.toml")?;
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Show the first 4 chars + ellipsis + last 2 — never the full
/// token. Bearer / API keys are sensitive; the CLI's `list` should
/// confirm the entry exists without printing the secret in `ps`/
/// shell history.
fn redact_token(token: &str) -> String {
    if token.starts_with("${") && token.ends_with('}') {
        // Env-ref form is safe to print verbatim — it doesn't carry
        // the secret.
        return token.to_string();
    }
    if token.len() <= 6 {
        return "***".to_string();
    }
    format!("{}…{}", &token[..4], &token[token.len() - 2..])
}

// Trait-cooperator: provide a "default redaction marker" hook on
// AuthEntry so the `list` formatter has a stable reference. Today
// it returns the empty string — kept for future extensibility
// without changing call sites.
trait AuthEntryExt {
    fn default_redaction_marker() -> &'static str {
        ""
    }
}
impl AuthEntryExt for AuthEntry {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn add_persists_a_stdio_server() {
        let tmp = tempdir().unwrap();
        add(
            "fs",
            tmp.path(),
            &[
                "npx".to_string(),
                "-y".to_string(),
                "fs-mcp".to_string(),
                "/tmp".to_string(),
            ],
        )
        .unwrap();
        let config = load_or_empty(tmp.path()).unwrap();
        assert_eq!(config.server.len(), 1);
        assert_eq!(config.server[0].name, "fs");
        assert_eq!(config.server[0].command.as_deref(), Some("npx"));
        assert_eq!(config.server[0].args, vec!["-y", "fs-mcp", "/tmp"]);
    }

    #[test]
    fn add_rejects_duplicate_names() {
        let tmp = tempdir().unwrap();
        add("dup", tmp.path(), &["cat".into()]).unwrap();
        let err = add("dup", tmp.path(), &["cat".into()]).unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn remove_drops_named_entry() {
        let tmp = tempdir().unwrap();
        add("fs", tmp.path(), &["cat".into()]).unwrap();
        remove("fs", tmp.path()).unwrap();
        let config = load_or_empty(tmp.path()).unwrap();
        assert!(config.server.is_empty());
    }

    #[test]
    fn remove_unknown_name_fails() {
        let tmp = tempdir().unwrap();
        let err = remove("ghost", tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn list_on_empty_workspace_is_friendly() {
        let tmp = tempdir().unwrap();
        // Should not panic / fail.
        list(tmp.path()).unwrap();
    }

    #[test]
    fn redact_token_hides_long_secrets() {
        assert_eq!(redact_token("supersecret123"), "supe…23");
    }

    #[test]
    fn redact_token_marks_short_secrets() {
        assert_eq!(redact_token("xyz"), "***");
    }

    #[test]
    fn redact_token_passes_env_refs_verbatim() {
        assert_eq!(redact_token("${MY_TOKEN}"), "${MY_TOKEN}");
    }
}
