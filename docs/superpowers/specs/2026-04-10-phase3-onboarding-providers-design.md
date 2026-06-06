# Phase 3 — Onboarding, Provider Expansion & Multi-Tool Connection

**Date:** 2026-04-10  
**Status:** Approved  
**Scope:** `thinkingroot-core`, `thinkingroot-extract`, `thinkingroot-cli`

---

## Problem Statement

ThinkingRoot has a working 6-stage pipeline and a solid REST + MCP serve layer, but there is no path from "just installed the binary" to "agents are connected and working." A student, researcher, or developer must:

1. Manually understand TOML config syntax to set an LLM provider
2. Know where Claude Desktop, Cursor, or other tools store their MCP configs
3. Hand-edit JSON files with the correct schema (which differs per tool)
4. Remember to re-run `root serve --path ...` every time with the right paths

This phase closes all four gaps with composable commands that work together for first-time users and independently for power users.

---

## Goals

- `root setup` — guided first-run wizard: provider → workspace → connect tools → compile
- `root connect` — standalone MCP config writer for 7 AI tools across 3 OS platforms
- `root workspace add/list/remove` — global registry so `root serve` needs no arguments
- OpenRouter, Together AI, Perplexity, LiteLLM, and `custom` provider support
- Git-style config hierarchy: global `~/.config/thinkingroot/` + per-workspace `.thinkingroot/`
- `root serve --install-service` — generate OS-native service files (launchd, systemd, Windows Service)

---

## Non-Goals

- TypeScript SDK (Phase 3 separate)
- VS Code extension (Phase 3 separate)
- GitHub Action (Phase 3 separate)
- Agent write-back to the graph (Phase 4)
- Cloud sync of workspaces (Phase 4)

---

## Architecture

### Config Hierarchy

Follows the git model: global base + per-workspace overrides + CLI flag wins.

```
~/.config/thinkingroot/
├── config.toml        ← global: LLM provider, API keys, serve defaults
└── workspaces.toml    ← global: registered workspace registry

<project>/.thinkingroot/
├── config.toml        ← per-workspace: model overrides, workspace name
├── graph.db           ← CozoDB (unchanged)
└── artifacts/         ← compiled output (unchanged)
```

**Merge order (highest wins):**
```
CLI flags  >  per-workspace config  >  global config  >  compiled defaults
```

New function in `thinkingroot-core`: `Config::load_merged(workspace_path: &Path) -> Result<Config>`

Reads global config first, overlays per-workspace TOML, returns final merged struct. All existing callers of `Config::load()` migrate to `Config::load_merged()`.

### Global Config (`~/.config/thinkingroot/config.toml`)

```toml
[llm]
default_provider = "openrouter"
extraction_model = "anthropic/claude-3-haiku"

[llm.providers.openrouter]
api_key_env = "OPENROUTER_API_KEY"

[llm.providers.openai]
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com"

[llm.providers.together]
api_key_env = "TOGETHER_API_KEY"

[llm.providers.perplexity]
api_key_env = "PERPLEXITY_API_KEY"

[llm.providers.litellm]
api_key_env = ""               # optional for local deployments
base_url = "http://localhost:4000"

[llm.providers.custom]
api_key_env = "CUSTOM_LLM_API_KEY"
base_url = "https://your-endpoint.com/v1"

[serve]
default_port = 3000
default_host = "127.0.0.1"
```

### Workspace Registry (`~/.config/thinkingroot/workspaces.toml`)

```toml
[[workspace]]
name = "notes"
path = "/Users/naveen/notes"
port = 3000

[[workspace]]
name = "work"
path = "/Users/naveen/work/project"
port = 3001
```

Kept in a separate file from `config.toml` — workspace list changes frequently (adds/removes), LLM config changes rarely. Mixing them creates noisy diffs for users who version-control their dotfiles.

### Per-Workspace Config (`.thinkingroot/config.toml`)

Only overrides needed — empty file is valid. All non-overridden values fall through to global.

```toml
[workspace]
name = "notes"

[llm]
extraction_model = "meta-llama/llama-3.1-8b-instruct"  # cheaper model for this workspace
```

---

## LLM Provider Additions

### New Providers

All four missing providers are OpenAI-compatible. Zero new network code required — all reuse the existing `OpenAiProvider` struct with a different `base_url`.

| Provider | Base URL | Auth env var |
|---|---|---|
| OpenRouter | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| Together AI | `https://api.together.xyz/v1` | `TOGETHER_API_KEY` |
| Perplexity | `https://api.perplexity.ai` | `PERPLEXITY_API_KEY` |
| LiteLLM | configurable (default `http://localhost:4000`) | optional |
| Custom | user-specified `base_url` | `CUSTOM_LLM_API_KEY` |

### Code Changes

**`thinkingroot-core/src/config.rs` — add to `ProvidersConfig`:**
```rust
pub openrouter: Option<ProviderConfig>,
pub together:   Option<ProviderConfig>,
pub perplexity: Option<ProviderConfig>,
pub litellm:    Option<ProviderConfig>,
pub custom:     Option<ProviderConfig>,
```

`ProviderConfig` already has `api_key_env: Option<String>` and `base_url: Option<String>` — no struct changes.

**`thinkingroot-extract/src/llm.rs` — new match arms (~20 lines):**

Extract a shared `resolve_key(cfg: Option<&ProviderConfig>, default_env: &str) -> Result<String>` helper to DRY up all arms (existing 6 + new 5). Then add:

```rust
"openrouter" => Provider::OpenAi(OpenAiProvider::new(
    &resolve_key(config.providers.openrouter.as_ref(), "OPENROUTER_API_KEY")?,
    model, "https://openrouter.ai/api/v1")),

"together" => Provider::OpenAi(OpenAiProvider::new(
    &resolve_key(config.providers.together.as_ref(), "TOGETHER_API_KEY")?,
    model, "https://api.together.xyz/v1")),

"perplexity" => Provider::OpenAi(OpenAiProvider::new(
    &resolve_key(config.providers.perplexity.as_ref(), "PERPLEXITY_API_KEY")?,
    model, "https://api.perplexity.ai")),

"litellm" => {
    let key = resolve_key_optional(config.providers.litellm.as_ref());
    let url = resolve_base_url(config.providers.litellm.as_ref(), "http://localhost:4000");
    Provider::OpenAi(OpenAiProvider::new(&key, model, &url))
},

"custom" => {
    let key = resolve_key(config.providers.custom.as_ref(), "CUSTOM_LLM_API_KEY")?;
    let url = resolve_base_url_required(config.providers.custom.as_ref(), "custom")?;
    Provider::OpenAi(OpenAiProvider::new(&key, model, &url))
},
```

Update the error message for unknown providers to list all 11 options.

### API Key Validation

Before saving any config, validate with a real (cheap) API call:

```rust
async fn validate_key(provider: &str, key: &str, model: &str) -> Result<()> {
    let client = LlmClient::new_direct(provider, key, model).await?;
    client.chat("You are a test.", "Reply with: ok").await?;
    Ok(())
}
```

Fail fast with a clear message: `"Key validation failed: 401 Unauthorized. Check your OPENROUTER_API_KEY."` Do not write config until validation passes.

---

## `root setup` Wizard

### New dependency

Add to `crates/thinkingroot-cli/Cargo.toml`:
```toml
dialoguer = "0.11"
indicatif = "0.17"   # already present for progress bars
```

### Implementation

New file: `crates/thinkingroot-cli/src/setup.rs`  
Entry: `pub async fn run_setup() -> anyhow::Result<()>`  
Called from `main.rs` `Commands::Setup` arm.

### Flow (5 steps)

**Step 1 — Global config location**
- Show resolved path `~/.config/thinkingroot/`
- If already configured: show current state, offer update menu (see Idempotency)
- Confirm with `[Y/n]`

**Step 2 — LLM provider**
- `dialoguer::Select` with all 11 providers, OpenRouter highlighted as recommended
- Password input (hidden) for API key
- Spinner while validating key against real API
- `dialoguer::Select` for model (top 3 recommendations per provider + "enter manually")

**Step 3 — First workspace**
- `dialoguer::Input` for path (default: current directory)
- `dialoguer::Input` for name (default: directory basename)
- `dialoguer::Input` for port (default: 3000; auto-increments to next unused port by checking existing entries in `workspaces.toml` — does not probe TCP sockets)
- Registers to `~/.config/thinkingroot/workspaces.toml`
- Creates `.thinkingroot/config.toml` in the workspace

**Step 4 — Connect AI tools**
- Scan all 7 tool config paths (macOS/Windows/Linux per platform)
- Show detected vs. not-found with checkmarks
- `dialoguer::Confirm` to connect detected tools
- Calls `mcp_config::write_tool_config()` for each confirmed tool

**Step 5 — Compile**
- `dialoguer::Select`: compile now / skip
- If compile now: show `indicatif` progress bar, print result summary

**Final screen:** Print summary table + exact next commands.

### Idempotency

Re-running `root setup` on an existing installation shows:
```
  ThinkingRoot is already configured.
  Provider: openrouter / anthropic/claude-3-haiku
  Workspaces: notes, work (2 total)

  What would you like to update?
  > Change LLM provider
    Add a workspace
    Connect more AI tools
    Reconfigure from scratch
    Cancel
```

Never silently overwrites existing config.

---

## MCP Config Writer (`root connect`)

### Implementation

New file: `crates/thinkingroot-cli/src/mcp_config.rs`  
Structs: `ToolDef`, `ToolDetector`, `ConfigFormat`, `WriteResult`  
Entry: `pub fn connect_all_tools(port: u16, dry_run: bool) -> Vec<WriteResult>`

### Tool Definitions (all paths verified from official documentation)

| Tool | macOS config path | Windows config path | Linux config path | Key |
|---|---|---|---|---|
| Claude Desktop | `~/Library/Application Support/Claude/claude_desktop_config.json` | `%APPDATA%\Claude\claude_desktop_config.json` | not supported | `mcpServers` |
| Cursor | `~/.cursor/mcp.json` | `%USERPROFILE%\.cursor\mcp.json` | `~/.cursor/mcp.json` | `mcpServers` |
| VS Code | `~/Library/Application Support/Code/User/mcp.json` | `%APPDATA%\Code\User\mcp.json` | `~/.config/Code/User/mcp.json` | `servers` |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | `%USERPROFILE%\.codeium\windsurf\mcp_config.json` | `~/.codeium/windsurf/mcp_config.json` | `mcpServers` |
| Zed | `~/.config/zed/settings.json` | `%APPDATA%\Zed\settings.json` | `~/.config/zed/settings.json` | `context_servers` |
| Cline | `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json` | `%APPDATA%\Code\User\globalStorage\saoudrizwan.claude-dev\settings\cline_mcp_settings.json` | `~/.config/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json` | `mcpServers` |
| Continue.dev | `~/.continue/mcpServers/thinkingroot.json` | `%USERPROFILE%\.continue\mcpServers\thinkingroot.json` | `~/.continue/mcpServers/thinkingroot.json` | individual file |

### Config Formats Written

```jsonc
// McpServers (Claude Desktop, Cursor, Windsurf, Cline)
{ "mcpServers": { "thinkingroot": { "url": "http://localhost:3000/mcp/sse" } } }

// Servers (VS Code) — different key + explicit type field
{ "servers": { "thinkingroot": { "type": "sse", "url": "http://localhost:3000/mcp/sse" } } }

// ContextServers (Zed) — different key
{ "context_servers": { "thinkingroot": { "url": "http://localhost:3000/mcp/sse" } } }

// Continue.dev — standalone file at ~/.continue/mcpServers/thinkingroot.json
{ "mcpServers": { "thinkingroot": { "url": "http://localhost:3000/mcp/sse" } } }
```

### Merge Strategy

1. Read existing file as `serde_json::Value` (empty object if file does not exist)
2. Insert `existing[servers_key]["thinkingroot"] = our_entry`
3. All other keys in the file are untouched
4. Write back as `serde_json::to_string_pretty`
5. Create parent directories with `fs::create_dir_all` if needed

This is safe to run multiple times — re-running updates our entry, never touches others.

### CLI Interface

```bash
root connect                     # detect + write all found tools
root connect --tool claude       # specific tool by name
root connect --tool cursor
root connect --port 3001         # non-default port
root connect --dry-run           # print what would change, write nothing
root connect --remove            # remove thinkingroot entry from all tool configs
```

**Detection:** A tool is "detected" when its config file's parent directory exists on disk (not just the file — tools may not have any MCP config yet).

---

## Workspace Registry (`root workspace`)

### CLI Interface

```bash
root workspace add ./path                         # register with auto-detected name + port
root workspace add ./path --name=work --port=3001 # explicit name + port
root workspace list                               # table: name, path, port, compile status
root workspace remove work                        # unregister by name
```

### `root serve` Integration

With no flags, `root serve` reads `~/.config/thinkingroot/workspaces.toml` and mounts all registered workspaces. Existing flag-based invocation continues to work:

```bash
root serve                      # mount all from registry (new behaviour); if registry empty, print "No workspaces registered. Run `root setup` or `root workspace add <path>`." and exit 1
root serve --name=notes         # mount single workspace by registry name
root serve --path=./custom      # explicit path, bypasses registry (unchanged)
root serve --install-service    # generate + install OS service file
```

---

## Daemon / Service Mode

Do not implement Unix fork or Windows service registration in Rust. Generate platform-appropriate service files and print install instructions.

### `root serve --install-service`

**macOS** → writes `~/Library/LaunchAgents/dev.thinkingroot.plist`, prints `launchctl load` command  
**Linux** → writes `~/.config/systemd/user/thinkingroot.service`, prints `systemctl --user enable` command  
**Windows** → writes `%USERPROFILE%\thinkingroot-service.ps1` (PowerShell wrapper), prints `sc.exe create` command  

Logs always written to `~/.config/thinkingroot/serve.log`.

---

## New CLI Surface Summary

```
root setup                      NEW — first-run wizard
root connect                    NEW — write MCP configs to all detected AI tools
root connect --tool <name>      NEW — specific tool
root connect --dry-run          NEW — preview without writing
root connect --remove           NEW — remove from all tool configs
root workspace add <path>       NEW — register workspace
root workspace list             NEW — show registry
root workspace remove <name>    NEW — unregister
root serve                      CHANGED — reads registry when no --path given
root serve --install-service    NEW — generate OS service file
```

All existing commands (`compile`, `health`, `query`, `graph`, `serve --path`) are unchanged.

---

## File Changes Summary

| File | Change |
|---|---|
| `crates/thinkingroot-core/src/config.rs` | Add 5 provider fields to `ProvidersConfig`; add `ServeConfig`; add `Config::load_merged()`; add `GlobalConfig` and `WorkspaceRegistry` structs |
| `crates/thinkingroot-extract/src/llm.rs` | Extract `resolve_key()` helper; add 5 new match arms |
| `crates/thinkingroot-cli/src/main.rs` | Add `Setup`, `Connect`, `Workspace` to `Commands` enum |
| `crates/thinkingroot-cli/src/setup.rs` | NEW — interactive wizard |
| `crates/thinkingroot-cli/src/mcp_config.rs` | NEW — tool detection + config writer |
| `crates/thinkingroot-cli/src/workspace.rs` | NEW — registry read/write, workspace commands |
| `crates/thinkingroot-cli/Cargo.toml` | Add `dialoguer = "0.11"`, `dirs = "5"` |

---

## Testing

- **Unit:** `mcp_config.rs` — merge logic with fixture JSON files for each tool format; verify other keys are preserved
- **Unit:** `config.rs` — `load_merged()` priority order (CLI > workspace > global > default)
- **Unit:** `llm.rs` — all 11 provider arms resolve correct base URLs
- **Integration:** `root connect --dry-run` on a temp directory with mock config files
- **Integration:** `root workspace add/list/remove` round-trip
- **Manual:** Full `root setup` flow on macOS + Windows (CI matrix)

---

## Risks

| Risk | Mitigation |
|---|---|
| Tool updates change config paths | Each path is in one place (`mcp_config.rs`); easy to update |
| User has existing MCP config with other servers | Merge strategy (section above) preserves all existing keys |
| API key validation adds latency to setup | Show spinner; skip validation with `--skip-validation` flag for CI |
| Windows path resolution differs | Use `dirs` crate for cross-platform home/config dirs |
| Continue.dev directory may not exist | `create_dir_all` before writing |
