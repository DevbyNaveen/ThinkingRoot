//! JIT capability-acquisition MCP tools.
//!
//! These are the mechanisms an agent uses to *acquire* a capability it
//! lacks, rather than to query existing knowledge. Today:
//!
//! - `mcp_server_install` — register an external MCP server into the
//!   workspace's `.thinkingroot/mcp-servers.toml`, remount the live
//!   `ExternalMcpRegistry`, and fire `notifications/tools/list_changed`
//!   so connected clients re-fetch the catalogue. This is rung 4 of the
//!   acquisition ladder (see `intelligence/jit.rs`).
//!
//! `skill_define` (rung 3) lands in the same module — it writes a new
//! workspace skill so a subsequent `use_skill` finds it.
//!
//! Both follow the Phase E.6 `mcp::tool_trait` registry pattern
//! (`operator_tools.rs` is the reference): implement `McpToolHandler`,
//! register in [`register_all`], and `tools::handle_call`'s
//! fall-through arm dispatches by name. No edits to the giant
//! `handle_call` match block.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::mcp::external_registry::{
    self, McpServersConfig, ServerEntry, TransportKind,
};
use crate::mcp::tool_trait::{McpToolContext, McpToolError, McpToolHandler, register_tool};

/// One installed connector's status for the Console: its configured
/// name + transport plus how many tools it currently exposes live (0 if
/// it failed to start / hasn't been remounted).
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerInfo {
    pub name: String,
    pub transport: String,
    pub tool_count: usize,
}

/// Path to a workspace's external-MCP config file.
fn servers_config_path(workspace_root: &Path) -> std::path::PathBuf {
    workspace_root
        .join(".thinkingroot")
        .join("mcp-servers.toml")
}

/// Read, upsert (by `name`), and atomically rewrite
/// `<workspace_root>/.thinkingroot/mcp-servers.toml`. Returns the total
/// number of configured servers after the upsert.
///
/// Pure file I/O — no transport spawn here — so it is unit-testable
/// without a live engine. The caller remounts the registry separately.
pub fn upsert_server_entry(
    workspace_root: &Path,
    entry: ServerEntry,
) -> Result<usize, String> {
    let path = servers_config_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let mut config: McpServersConfig = if path.exists() {
        let bytes = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        toml::from_str(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?
    } else {
        McpServersConfig::default()
    };

    // Upsert by name — re-installing an existing server updates it
    // rather than producing a duplicate `[[server]]` block.
    if let Some(slot) = config.server.iter_mut().find(|s| s.name == entry.name) {
        *slot = entry;
    } else {
        config.server.push(entry);
    }
    let count = config.server.len();

    let body = toml::to_string_pretty(&config)
        .map_err(|e| format!("serialize mcp-servers.toml: {e}"))?;
    // Atomic rename so a concurrent reader never sees a torn file.
    let tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap())
        .map_err(|e| format!("tempfile: {e}"))?;
    std::fs::write(tmp.path(), body.as_bytes()).map_err(|e| format!("write tmp: {e}"))?;
    tmp.persist(&path).map_err(|e| format!("persist {}: {e}", path.display()))?;

    Ok(count)
}

/// Read the configured servers from `mcp-servers.toml` (source of truth
/// for what's installed, independent of whether each started). Empty
/// when the file is absent.
pub fn list_configured_servers(workspace_root: &Path) -> Result<Vec<ServerEntry>, String> {
    let path = servers_config_path(workspace_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let config: McpServersConfig =
        toml::from_str(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(config.server)
}

/// Remove a server by name and rewrite the config. Returns `true` if it
/// existed. The caller remounts the registry afterwards.
pub fn remove_server_entry(workspace_root: &Path, name: &str) -> Result<bool, String> {
    let path = servers_config_path(workspace_root);
    if !path.exists() {
        return Ok(false);
    }
    let bytes = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut config: McpServersConfig =
        toml::from_str(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let before = config.server.len();
    config.server.retain(|s| s.name != name);
    let removed = config.server.len() != before;
    if removed {
        let body = toml::to_string_pretty(&config)
            .map_err(|e| format!("serialize mcp-servers.toml: {e}"))?;
        let tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap())
            .map_err(|e| format!("tempfile: {e}"))?;
        std::fs::write(tmp.path(), body.as_bytes()).map_err(|e| format!("write tmp: {e}"))?;
        tmp.persist(&path).map_err(|e| format!("persist {}: {e}", path.display()))?;
    }
    Ok(removed)
}

/// Validate the transport-specific required fields before we touch
/// disk. Returns the parsed `ServerEntry` or a user-facing reason the
/// model can act on.
pub fn parse_and_validate(args: &Value) -> Result<ServerEntry, String> {
    let entry: ServerEntry = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid server spec: {e}"))?;
    if entry.name.trim().is_empty() {
        return Err("`name` must be a non-empty server identifier".into());
    }
    match entry.transport {
        TransportKind::Stdio => {
            if entry.command.as_deref().unwrap_or("").trim().is_empty() {
                return Err("stdio transport requires a non-empty `command`".into());
            }
        }
        TransportKind::Http => {
            if entry.endpoint.as_deref().unwrap_or("").trim().is_empty() {
                return Err("http transport requires a non-empty `endpoint`".into());
            }
        }
    }
    Ok(entry)
}

// ── mcp_server_install ───────────────────────────────────────────────────────

struct McpServerInstall;

#[async_trait]
impl McpToolHandler for McpServerInstall {
    fn name(&self) -> &'static str {
        "mcp_server_install"
    }

    fn description(&self) -> &'static str {
        "Install (or update) an external MCP server for this workspace and remount it live. \
         Use when the user — or your own gap analysis — needs a tool that isn't in the current \
         catalogue (e.g. a GitHub, filesystem, or database MCP server). The server is persisted \
         to .thinkingroot/mcp-servers.toml, started immediately, and a tools/list_changed \
         notification is broadcast so its tools become callable without a reconnect. \
         Re-installing the same `name` updates it in place."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name":        { "type": "string", "description": "Unique server id, e.g. 'github'. Re-using a name updates that server." },
                "transport":   { "type": "string", "enum": ["stdio", "http"], "description": "How the server is reached." },
                "command":     { "type": "string", "description": "stdio only: executable, e.g. 'npx'." },
                "args":        { "type": "array", "items": { "type": "string" }, "description": "stdio only: command arguments." },
                "env":         { "type": "object", "additionalProperties": { "type": "string" }, "description": "stdio only: extra env vars. Values may use ${VAR} refs resolved from the process env at load." },
                "cwd":         { "type": "string", "description": "stdio only: working directory." },
                "endpoint":    { "type": "string", "description": "http only: server URL." },
                "timeout_secs":{ "type": "integer", "description": "Optional per-request timeout in seconds." },
                "auth":        { "type": "object", "description": "http only: { kind: 'bearer'|'api_key', token: '...' }. token may be a ${VAR} ref." }
            },
            "required": ["name", "transport"]
        })
    }

    fn is_write(&self) -> bool {
        // Installs a process + mutates workspace config + reaches the
        // network — unambiguously write-class, so it routes through the
        // approval gate like every other mutating tool.
        true
    }

    async fn handle(
        &self,
        args: Value,
        ctx: &McpToolContext<'_>,
    ) -> Result<Value, McpToolError> {
        let entry = parse_and_validate(&args).map_err(McpToolError::InvalidArgs)?;
        let name = entry.name.clone();

        // Egress enforcement: an HTTP MCP server reaches out to its
        // endpoint, so its host must clear the project's outbound
        // allowlist. stdio servers are local processes (no egress).
        if entry.transport == TransportKind::Http {
            if let Some(endpoint) = entry.endpoint.as_deref() {
                let host = url::Url::parse(endpoint)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_default();
                if !crate::egress::host_allowed_from_env(&host) {
                    return Err(McpToolError::Refused(format!(
                        "endpoint host '{host}' is not in this project's outbound allowlist \
                         (TR_OUTBOUND_ALLOWLIST) — add it via the cloud Console's egress settings"
                    )));
                }
            }
        }

        let workspace_root = ctx
            .engine
            .workspace_root_path(ctx.workspace)
            .ok_or_else(|| {
                McpToolError::Refused(format!(
                    "workspace '{}' is not mounted — cannot resolve its config path",
                    ctx.workspace
                ))
            })?;

        let count = upsert_server_entry(&workspace_root, entry)
            .map_err(McpToolError::Refused)?;

        // Remount the global registry from the freshly-written config.
        // A bad single entry is logged + skipped inside the registry
        // builder, so this only errors on an unreadable/corrupt file.
        external_registry::load_global_from_workspace_config(&workspace_root)
            .await
            .map_err(|e| McpToolError::Refused(format!("remount failed: {e}")))?;

        let notified = crate::mcp::sse::notify_tools_list_changed().await;

        Ok(json!({
            "installed": name,
            "server_count": count,
            "tools_list_changed_notified": notified,
        }))
    }
}

// ── skill_define ─────────────────────────────────────────────────────────────

/// Validate a skill slug → safe filename. Allows lowercase letters,
/// digits, `-`, `_`. Rejects anything else (path traversal, spaces) so
/// the write target stays inside the skills dir.
fn validate_slug(name: &str) -> Result<String, String> {
    let slug = name.trim();
    if slug.is_empty() {
        return Err("skill name must be non-empty".into());
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(format!(
            "skill name `{slug}` must be kebab-case [a-z0-9-_] (no spaces, slashes, or uppercase)"
        ));
    }
    Ok(slug.to_string())
}

/// Write a skill file to `<workspace_root>/.thinkingroot/skills/<slug>.md`
/// with the 2-key frontmatter `SkillRegistry::parse_skill` expects.
/// Returns the written path. Pure-ish (only file I/O) so it's testable
/// without an engine.
pub fn write_skill_file(
    workspace_root: &Path,
    slug: &str,
    description: &str,
    body: &str,
) -> Result<std::path::PathBuf, String> {
    if description.trim().is_empty() {
        return Err("skill description must be non-empty".into());
    }
    if body.trim().is_empty() {
        return Err("skill body must be non-empty".into());
    }
    let dir = workspace_root.join(".thinkingroot").join("skills");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{slug}.md"));
    // Frontmatter is single-line per field (the parser is a 2-key
    // hand-roll), so collapse any newlines in description.
    let one_line_desc = description.replace('\n', " ");
    let contents = format!("---\nname: {slug}\ndescription: {one_line_desc}\n---\n\n{body}\n");
    let tmp = tempfile::NamedTempFile::new_in(&dir).map_err(|e| format!("tempfile: {e}"))?;
    std::fs::write(tmp.path(), contents.as_bytes()).map_err(|e| format!("write tmp: {e}"))?;
    tmp.persist(&path).map_err(|e| format!("persist {}: {e}", path.display()))?;
    Ok(path)
}

struct SkillDefine;

#[async_trait]
impl McpToolHandler for SkillDefine {
    fn name(&self) -> &'static str {
        "skill_define"
    }

    fn description(&self) -> &'static str {
        "Author a new reusable skill (a markdown playbook) for this workspace. Use when you've \
         worked out a repeatable procedure and want it available to future sessions via \
         `use_skill`. Writes .thinkingroot/skills/<name>.md with name+description frontmatter and \
         your body. The skill becomes loadable on the next agent run. This is rung 3 of the JIT \
         acquisition ladder (define a skill rather than re-derive a procedure each time)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name":        { "type": "string", "description": "kebab-case skill id, e.g. 'refactor-rust'. Becomes the filename." },
                "description": { "type": "string", "description": "One line: WHEN to use this skill (the agent matches on this)." },
                "body":        { "type": "string", "description": "Markdown playbook: the steps to follow." }
            },
            "required": ["name", "description", "body"]
        })
    }

    fn is_write(&self) -> bool {
        true
    }

    async fn handle(
        &self,
        args: Value,
        ctx: &McpToolContext<'_>,
    ) -> Result<Value, McpToolError> {
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");

        let slug = validate_slug(name).map_err(McpToolError::InvalidArgs)?;

        let workspace_root = ctx
            .engine
            .workspace_root_path(ctx.workspace)
            .ok_or_else(|| {
                McpToolError::Refused(format!("workspace '{}' is not mounted", ctx.workspace))
            })?;

        let path = write_skill_file(&workspace_root, &slug, description, body)
            .map_err(McpToolError::Refused)?;

        Ok(json!({
            "defined": slug,
            "path": path.display().to_string(),
            "note": "available via use_skill on the next agent run (skill registry reloads from disk)",
        }))
    }
}

/// Register every acquisition tool into the global `tool_trait`
/// registry. Idempotent (duplicate names overwrite). Call sites mirror
/// `operator_tools::register_all`: `rest::new_with_root` (SSE/HTTP) and
/// `mcp::stdio::run` (stdio transport).
pub fn register_all() {
    register_tool(Arc::new(McpServerInstall));
    register_tool(Arc::new(SkillDefine));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_creates_then_updates_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let stdio = ServerEntry {
            name: "github".into(),
            transport: TransportKind::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "gh-mcp".into()],
            env: Default::default(),
            cwd: None,
            endpoint: None,
            timeout_secs: None,
            auth: None,
        };
        let n = upsert_server_entry(root, stdio).unwrap();
        assert_eq!(n, 1);

        // Same name → update, not duplicate.
        let updated = ServerEntry {
            name: "github".into(),
            transport: TransportKind::Stdio,
            command: Some("node".into()),
            args: vec!["gh.js".into()],
            env: Default::default(),
            cwd: None,
            endpoint: None,
            timeout_secs: None,
            auth: None,
        };
        let n = upsert_server_entry(root, updated).unwrap();
        assert_eq!(n, 1, "re-installing the same name must not duplicate");

        // Different name → appended.
        let http = ServerEntry {
            name: "search".into(),
            transport: TransportKind::Http,
            command: None,
            args: vec![],
            env: Default::default(),
            cwd: None,
            endpoint: Some("https://example.com/mcp".into()),
            timeout_secs: Some(30),
            auth: None,
        };
        let n = upsert_server_entry(root, http).unwrap();
        assert_eq!(n, 2);

        // Round-trips through the same parser the registry uses.
        let bytes = std::fs::read_to_string(servers_config_path(root)).unwrap();
        let cfg: McpServersConfig = toml::from_str(&bytes).unwrap();
        assert_eq!(cfg.server.len(), 2);
        let gh = cfg.server.iter().find(|s| s.name == "github").unwrap();
        assert_eq!(gh.command.as_deref(), Some("node"));
    }

    #[test]
    fn skill_slug_validation_and_write() {
        assert!(validate_slug("refactor-rust").is_ok());
        assert!(validate_slug("my_skill_2").is_ok());
        assert!(validate_slug("bad/slug").is_err());
        assert!(validate_slug("Bad Slug").is_err());
        assert!(validate_slug("").is_err());

        let tmp = tempfile::tempdir().unwrap();
        let path =
            write_skill_file(tmp.path(), "greet", "Use when greeting", "Say hi politely.").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // Parses back through the real SkillRegistry parser.
        let skill =
            crate::intelligence::skills::parse_skill(path.clone(), &raw).expect("valid skill file");
        assert_eq!(skill.name, "greet");
        assert_eq!(skill.description, "Use when greeting");
        // Empty body / description rejected.
        assert!(write_skill_file(tmp.path(), "x", "", "body").is_err());
        assert!(write_skill_file(tmp.path(), "x", "desc", "  ").is_err());
    }

    #[test]
    fn list_and_remove_configured_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        upsert_server_entry(
            root,
            ServerEntry {
                name: "github".into(),
                transport: TransportKind::Stdio,
                command: Some("npx".into()),
                args: vec![],
                env: Default::default(),
                cwd: None,
                endpoint: None,
                timeout_secs: None,
                auth: None,
            },
        )
        .unwrap();
        let listed = list_configured_servers(root).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "github");
        assert!(remove_server_entry(root, "github").unwrap());
        assert!(!remove_server_entry(root, "github").unwrap());
        assert_eq!(list_configured_servers(root).unwrap().len(), 0);
    }

    #[test]
    fn validate_rejects_missing_transport_fields() {
        // stdio without command.
        let bad = json!({ "name": "x", "transport": "stdio" });
        assert!(parse_and_validate(&bad).is_err());
        // http without endpoint.
        let bad = json!({ "name": "x", "transport": "http" });
        assert!(parse_and_validate(&bad).is_err());
        // empty name.
        let bad = json!({ "name": "  ", "transport": "stdio", "command": "npx" });
        assert!(parse_and_validate(&bad).is_err());
        // valid stdio.
        let ok = json!({ "name": "fs", "transport": "stdio", "command": "npx", "args": ["fs-mcp"] });
        assert!(parse_and_validate(&ok).is_ok());
    }
}
