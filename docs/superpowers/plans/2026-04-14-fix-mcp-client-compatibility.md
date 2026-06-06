# Fix MCP Client Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix MCP protocol breakage across Cursor, Windsurf, Codex, and Antigravity/Gemini CLI by correcting protocol version negotiation, missing transport-type hints, missing credential env passthrough, and adding Gemini CLI config support.

**Architecture:** Two files touch everything. `mod.rs` owns the handshake and capabilities; `mcp_config.rs` owns tool detection, config file writing, and the `apply_entry` logic that sets per-tool config format. All five issues collapse into targeted edits on those two files.

**Tech Stack:** Rust, Axum, `serde_json`, `toml`, `dirs` crate, MCP JSON-RPC 2.0 protocol (versions `2024-11-05` and `2025-03-26`).

---

## File Map

| File | Change |
|------|--------|
| `crates/thinkingroot-serve/src/mcp/mod.rs` | Protocol version negotiation + `prompts` capability |
| `crates/thinkingroot-cli/src/mcp_config.rs` | `"type":"sse"` in entries, `GeminiCli` format variant, Codex env passthrough, Gemini CLI tool detection |

---

### Task 1: Protocol version negotiation in `mod.rs`

**Problem:** `server_info()` hard-codes `"2024-11-05"`. Cursor 0.44+, Windsurf, Codex, and Antigravity send `initialize` with `"protocolVersion":"2025-03-26"` and reject (or degrade) when the server echoes back an older version.

**Fix:** Echo back the client's requested version if supported; otherwise use the latest supported. Also add `"prompts":{}` to capabilities (some Cursor builds check for it).

**Files:**
- Modify: `crates/thinkingroot-serve/src/mcp/mod.rs`

- [ ] **Step 1: Write the failing test**

Add a test module at the bottom of `crates/thinkingroot-serve/src/mcp/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_echoes_supported_version() {
        let info = server_info(Some("2025-03-26"));
        assert_eq!(info["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn server_info_falls_back_to_latest_for_unknown_version() {
        let info = server_info(Some("2099-01-01"));
        assert_eq!(info["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn server_info_uses_latest_when_no_version_requested() {
        let info = server_info(None);
        assert_eq!(info["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn server_info_accepts_legacy_version() {
        let info = server_info(Some("2024-11-05"));
        assert_eq!(info["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn server_info_includes_prompts_capability() {
        let info = server_info(None);
        assert!(info["capabilities"]["prompts"].is_object());
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cd /Users/naveen/Desktop/thinkingroot
cargo test -p thinkingroot-serve mcp::tests --no-default-features 2>&1 | tail -20
```

Expected: compile error — `server_info` takes 0 args, tests call it with 1.

- [ ] **Step 3: Implement version negotiation**

Replace the entire `server_info` function and add the constant in `crates/thinkingroot-serve/src/mcp/mod.rs`:

```rust
const SUPPORTED_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];

pub fn server_info(requested_version: Option<&str>) -> Value {
    // Echo back the client's version if we support it; otherwise use our latest.
    let version = requested_version
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(SUPPORTED_VERSIONS[0]);
    serde_json::json!({
        "protocolVersion": version,
        "serverInfo": { "name": "thinkingroot", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": {
            "resources": { "listChanged": false },
            "tools": {},
            "prompts": {}
        }
    })
}
```

- [ ] **Step 4: Update `dispatch()` to extract and forward the client version**

In `dispatch()`, replace:
```rust
"initialize" => JsonRpcResponse::success(id, server_info()),
```
with:
```rust
"initialize" => {
    let requested = request.params.get("protocolVersion").and_then(|v| v.as_str());
    JsonRpcResponse::success(id, server_info(requested))
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p thinkingroot-serve mcp::tests --no-default-features 2>&1 | tail -20
```

Expected: all 5 tests pass.

- [ ] **Step 6: Full workspace type-check**

```bash
cargo check --workspace --no-default-features 2>&1 | tail -20
```

Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-serve/src/mcp/mod.rs
git commit -m "fix(mcp): negotiate protocol version with client, add prompts capability"
```

---

### Task 2: Add `"type":"sse"` to McpServers config entries

**Problem:** `apply_entry()` emits `{"url":"..."}` for Cursor, Windsurf, Claude Desktop, Antigravity, Cline, Continue.dev. Cursor 0.44+ and Windsurf require the explicit `"type":"sse"` field; without it some versions default to stdio transport and try to exec the URL string as a process.

**Files:**
- Modify: `crates/thinkingroot-cli/src/mcp_config.rs`

- [ ] **Step 1: Write the failing test**

Add to the existing `tests` module inside `mcp_config.rs`:

```rust
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
```

- [ ] **Step 2: Run test to confirm it fails**

```bash
cargo test -p thinkingroot-cli mcp_config::tests::mcp_servers_entry_includes_type_sse --no-default-features 2>&1 | tail -10
```

Expected: FAIL — the `"type"` key is missing.

- [ ] **Step 3: Add `"type":"sse"` to the default arm of `apply_entry`**

In `apply_entry`, find and replace:

Old:
```rust
        _ => json!({
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
```

New:
```rust
        _ => json!({
            "type": "sse",
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
```

- [ ] **Step 4: Run all `mcp_config` tests**

```bash
cargo test -p thinkingroot-cli mcp_config --no-default-features 2>&1 | tail -20
```

Expected: all existing tests still pass + new test passes.

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-cli/src/mcp_config.rs
git commit -m "fix(connect): add type:sse to McpServers config entries for Cursor/Windsurf"
```

---

### Task 3: Add Gemini CLI support (new `GeminiCli` format variant)

**Problem:** Google's Gemini CLI (open-source, `~/.gemini/settings.json`) uses `httpUrl` instead of `url` in its `mcpServers` entries. The current code neither detects the tool nor writes the right key. The existing Antigravity entry only covered an older internal path.

**Files:**
- Modify: `crates/thinkingroot-cli/src/mcp_config.rs`

- [ ] **Step 1: Write the failing test**

Add to the tests module:

```rust
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
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test -p thinkingroot-cli mcp_config::tests::gemini_cli --no-default-features 2>&1 | tail -10
```

Expected: compile error — `GeminiCli` variant doesn't exist yet.

- [ ] **Step 3: Add `GeminiCli` variant to `ConfigFormat`**

Find:
```rust
pub enum ConfigFormat {
    McpServers,
    Servers,
    ContextServers,
    ContinueDev,
    ClaudeCode,
    CodexToml,
}
```

Replace with:
```rust
pub enum ConfigFormat {
    McpServers,
    Servers,
    ContextServers,
    ContinueDev,
    ClaudeCode,
    CodexToml,
    /// Gemini CLI (~/.gemini/settings.json): mcpServers key, httpUrl instead of url
    GeminiCli,
}
```

- [ ] **Step 4: Handle `GeminiCli` in `apply_entry`**

In `apply_entry`, update the `servers_key` match:

```rust
    let servers_key = match format {
        ConfigFormat::McpServers | ConfigFormat::ContinueDev => "mcpServers",
        ConfigFormat::Servers => "servers",
        ConfigFormat::ContextServers => "context_servers",
        ConfigFormat::GeminiCli => "mcpServers",
        ConfigFormat::ClaudeCode | ConfigFormat::CodexToml => return,
    };
```

And update the `entry` match — add `GeminiCli` **before** the default arm:

```rust
    let entry = match format {
        ConfigFormat::Servers => json!({
            "type": "sse",
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
        ConfigFormat::GeminiCli => json!({
            "httpUrl": format!("http://localhost:{}/mcp/sse", port)
        }),
        _ => json!({
            "type": "sse",
            "url": format!("http://localhost:{}/mcp/sse", port)
        }),
    };
```

- [ ] **Step 5: Handle `GeminiCli` in `remove_entry`**

In `remove_entry`, update the `servers_key` match:

```rust
    let servers_key = match format {
        ConfigFormat::McpServers | ConfigFormat::ContinueDev => "mcpServers",
        ConfigFormat::Servers => "servers",
        ConfigFormat::ContextServers => "context_servers",
        ConfigFormat::GeminiCli => "mcpServers",
        ConfigFormat::ClaudeCode | ConfigFormat::CodexToml => return,
    };
```

- [ ] **Step 6: Add Gemini CLI to `tool_defs()`**

In `tool_defs()`, add this entry after the Antigravity entry:

```rust
        (
            "Gemini CLI",
            Box::new(|| dirs::home_dir().map(|d| d.join(".gemini").join("settings.json"))),
            ConfigFormat::GeminiCli,
        ),
```

- [ ] **Step 7: Update the "no tools detected" message in `run_connect`**

Find:
```rust
        println!(
            "  Supported: Claude Desktop, Claude Code, Cursor, VS Code, Windsurf, Zed, Cline, Continue.dev, Antigravity, Codex"
        );
```

Replace with:
```rust
        println!(
            "  Supported: Claude Desktop, Claude Code, Cursor, VS Code, Windsurf, Zed, Cline, Continue.dev, Antigravity, Gemini CLI, Codex"
        );
```

- [ ] **Step 8: Run all mcp_config tests**

```bash
cargo test -p thinkingroot-cli mcp_config --no-default-features 2>&1 | tail -20
```

Expected: all tests pass including the two new Gemini CLI tests.

- [ ] **Step 9: Full workspace type-check**

```bash
cargo check --workspace --no-default-features 2>&1 | tail -20
```

Expected: no errors.

- [ ] **Step 10: Commit**

```bash
git add crates/thinkingroot-cli/src/mcp_config.rs
git commit -m "feat(connect): add Gemini CLI support with httpUrl MCP config format"
```

---

### Task 4: Codex env var passthrough

**Problem:** The Codex TOML entry has `command` and `args` but no `env` table. When Codex is launched as a GUI (Electron) app, it doesn't inherit the user's shell environment, so the spawned `root serve --mcp-stdio` subprocess has no LLM provider credentials. Any tool call that triggers LLM operations (`compile`, `contribute`) silently fails.

**Fix:** At `root connect` time, snapshot the currently-set credential env vars into the TOML `env` table so the subprocess always has them regardless of how Codex was launched.

**Files:**
- Modify: `crates/thinkingroot-cli/src/mcp_config.rs`

- [ ] **Step 1: Write the failing test**

Add to the tests module:

```rust
#[test]
fn codex_toml_captures_env_vars_when_set() {
    // Arrange: set a fake credential in the test process env
    std::env::set_var("OPENAI_API_KEY", "sk-test-value");

    let input = r#"model = "gpt-4o""#;
    let mut doc: toml::Value = input.parse().unwrap();
    apply_codex_entry(&mut doc, "/usr/local/bin/root", "/workspace");

    let mcp = doc["mcp_servers"]["thinkingroot"].as_table().unwrap();
    assert_eq!(
        mcp["env"]["OPENAI_API_KEY"].as_str().unwrap(),
        "sk-test-value"
    );

    // Clean up
    std::env::remove_var("OPENAI_API_KEY");
}

#[test]
fn codex_toml_omits_env_table_when_no_credentials_set() {
    // Make sure none of the tracked vars are set for this test
    const VARS: &[&str] = &[
        "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN",
        "AWS_PROFILE", "AWS_DEFAULT_REGION", "AWS_REGION",
        "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GROQ_API_KEY", "DEEPSEEK_API_KEY",
    ];
    for v in VARS { std::env::remove_var(v); }

    let mut doc: toml::Value = toml::Value::Table(toml::map::Map::new());
    apply_codex_entry(&mut doc, "/usr/local/bin/root", "/workspace");

    let mcp = doc["mcp_servers"]["thinkingroot"].as_table().unwrap();
    assert!(!mcp.contains_key("env"), "env table should be absent when no credentials are set");
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test -p thinkingroot-cli "mcp_config::tests::codex_toml_captures_env" --no-default-features 2>&1 | tail -10
cargo test -p thinkingroot-cli "mcp_config::tests::codex_toml_omits_env" --no-default-features 2>&1 | tail -10
```

Expected: both FAIL — `env` key never written.

- [ ] **Step 3: Add env var passthrough to `apply_codex_entry`**

In `apply_codex_entry`, add this block after the `args` insert and before `mcp_servers.insert(...)`:

```rust
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
```

- [ ] **Step 4: Run all mcp_config tests**

```bash
cargo test -p thinkingroot-cli mcp_config --no-default-features 2>&1 | tail -20
```

Expected: all tests pass. The existing `codex_toml_inserts_mcp_server_entry` test still passes because it doesn't set any credential env vars and doesn't assert that `env` is absent.

- [ ] **Step 5: Full workspace type-check and test run**

```bash
cargo check --workspace --no-default-features 2>&1 | tail -10
cargo test --workspace --no-default-features 2>&1 | tail -20
```

Expected: no errors, all tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-cli/src/mcp_config.rs
git commit -m "fix(connect): forward credential env vars to Codex stdio subprocess"
```

---

## Self-Review Checklist

**Spec coverage:**
- [x] Protocol version `2024-11-05` → `2025-03-26` negotiation — Task 1
- [x] Missing `"type":"sse"` for Cursor/Windsurf — Task 2
- [x] Gemini CLI detection + `httpUrl` format — Task 3
- [x] Codex env passthrough — Task 4
- [x] `"prompts":{}` capability — Task 1 Step 3

**Placeholder scan:** None found — all steps include complete code.

**Type consistency:** `ConfigFormat::GeminiCli` introduced in Task 3 Step 3 and used in Steps 4, 5, 6 of the same task. `server_info(Option<&str>)` defined in Task 1 Step 3 and called in Step 4. No mismatches.
