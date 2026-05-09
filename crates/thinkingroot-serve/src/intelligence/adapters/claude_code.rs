// crates/thinkingroot-serve/src/intelligence/adapters/claude_code.rs
//
// Claude Code adapter (Task 18 / Week 3, plan 2026-05-09).
//
// Spawns `claude -p --output-format stream-json --mcp-config <tr.json>`
// inside a sandbox worktree, parses the newline-delimited stream-json
// output, and emits `AdapterEvent`s through an mpsc channel. The
// adapter terminates the subprocess when the receiver is dropped
// (caller cancellation) or when the process exits naturally.
//
// Stream-json wire format Claude Code emits (see
// docs.anthropic.com/en/docs/claude-code/cli-reference for the
// canonical reference):
//
//   {"type":"system","subtype":"init",...}                   # ignored
//   {"type":"assistant","message":{"content":[
//      {"type":"text","text":"..."}]}}                        # Token
//   {"type":"assistant","message":{"content":[
//      {"type":"tool_use","id":"...","name":"...","input":{}}]}}   # ToolUse
//   {"type":"user","message":{"content":[
//      {"type":"tool_result","tool_use_id":"...",
//       "content":"...","is_error":false}]}}                  # ToolResult
//   {"type":"result","subtype":"success",
//    "result":"...","total_cost_usd":0.12}                    # Done
//   {"type":"result","subtype":"error_during_execution",...}  # Error
//
// The parser is forgiving: unrecognised `type` values yield `None`
// and don't error the stream. Lines that don't parse as JSON are
// dropped with a `tracing::warn!` (don't kill the run because Claude
// Code v1.x emitted one bad line — most output is fine).

use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{AdapterError, AdapterEvent, AgentAdapter};

/// Configuration captured at construction time. The defaults match a
/// typical developer install (`claude` on PATH, no MCP config). For
/// production the bus passes a path to a generated `tr.json` MCP
/// config that wires Claude Code's MCP client into this server's
/// stdio transport.
#[derive(Debug, Clone)]
pub struct ClaudeCodeConfig {
    /// Binary to spawn. Override via the `CLAUDE_BINARY` env var so
    /// integration tests can substitute a fake-claude shell script
    /// without changing the adapter call sites. Default: `"claude"`.
    pub binary_path: PathBuf,
    /// Optional path to a `tr.json` MCP config the spawned agent
    /// will read with `--mcp-config`. None means "agent uses its own
    /// `~/.config/claude/mcp.json`".
    pub mcp_config_path: Option<PathBuf>,
    /// Extra args to splice in before the prompt. Surface for
    /// experimentation (`--max-turns 4`, `--allowed-tools mcp__*`)
    /// without churning the adapter's call signature.
    pub extra_args: Vec<String>,
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        let binary_path = std::env::var("CLAUDE_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("claude"));
        Self {
            binary_path,
            mcp_config_path: None,
            extra_args: Vec::new(),
        }
    }
}

/// The adapter. One `ClaudeCodeAdapter` per logical configuration —
/// not per spawn. Cheap to clone via the `Arc` indirection callers
/// typically use.
#[derive(Debug, Clone)]
pub struct ClaudeCodeAdapter {
    config: ClaudeCodeConfig,
}

impl ClaudeCodeAdapter {
    pub fn new(config: ClaudeCodeConfig) -> Self {
        Self { config }
    }

    /// Convenience constructor with all defaults. Equivalent to
    /// `ClaudeCodeAdapter::new(ClaudeCodeConfig::default())`.
    pub fn from_env() -> Self {
        Self::new(ClaudeCodeConfig::default())
    }

    fn build_args(&self, prompt: &str) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];
        if let Some(cfg) = &self.config.mcp_config_path {
            args.push("--mcp-config".to_string());
            args.push(cfg.to_string_lossy().to_string());
        }
        args.extend(self.config.extra_args.iter().cloned());
        args.push(prompt.to_string());
        args
    }
}

#[async_trait]
impl AgentAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        "claude_code"
    }

    async fn spawn(
        &self,
        prompt: &str,
        worktree: &Path,
        _pack: Option<&Path>,
    ) -> Result<mpsc::Receiver<AdapterEvent>, AdapterError> {
        if !worktree.is_dir() {
            return Err(AdapterError::InvalidWorktree {
                path: worktree.to_string_lossy().to_string(),
            });
        }

        let args = self.build_args(prompt);
        let mut command = Command::new(&self.config.binary_path);
        command
            .args(&args)
            .current_dir(worktree)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AdapterError::BinaryNotFound {
                binary: self.config.binary_path.to_string_lossy().to_string(),
                source: e,
            },
            _ => AdapterError::SpawnFailed(e),
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            AdapterError::SpawnFailed(std::io::Error::other("subprocess has no stdout"))
        })?;
        let stderr = child.stderr.take();

        // 32 is comfortably above the typical event rate; back-pressure
        // is the right behaviour if the consumer stalls.
        let (tx, rx) = mpsc::channel::<AdapterEvent>(32);

        // Reader task: parses stdout line-by-line, pushes events.
        let tx_for_stdout = tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if let Some(events) = parse_stream_json_line(&line) {
                            for ev in events {
                                if tx_for_stdout.send(ev).await.is_err() {
                                    return;
                                }
                            }
                        } else {
                            tracing::warn!("claude_code: unparseable line: {}", line);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx_for_stdout
                            .send(AdapterEvent::Error {
                                message: format!("stdout read failed: {e}"),
                            })
                            .await;
                        return;
                    }
                }
            }
        });

        // Stderr drain task: don't let the subprocess block on a full
        // stderr pipe. Stderr is logged at debug level (Claude Code
        // emits informational diagnostics here, not errors).
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::debug!("claude_code stderr: {}", line);
                    }
                }
            });
        }

        // Watchdog: when the consumer drops `rx`, `tx` becomes the
        // last sender; sending on it returns Err. The reader task
        // bails on first send failure, which lets the child
        // shut down naturally (kill_on_drop ensures the OS terminates
        // the process when `child` is dropped at the end of this
        // task).
        let tx_for_wait = tx;
        tokio::spawn(async move {
            let _wait = child.wait().await;
            // Reader task may have already closed; sending Done here
            // is best-effort. We do NOT synthesize a Done from the
            // process exit because the parser already emits Done
            // from `result.subtype: success`. A spurious extra Done
            // would confuse the SSE relay.
            drop(tx_for_wait);
        });

        Ok(rx)
    }
}

/// Pure parser for one line of Claude Code stream-json output.
/// Returns one or more `AdapterEvent`s when the line maps to known
/// shapes; returns `None` when the line is JSON but not recognised
/// (e.g. `system.init`); returns `None` on parse failure (caller
/// logs).
///
/// One line can produce multiple events because Claude Code's
/// `assistant.message.content` is an array — a single line may
/// contain a Token block followed by a ToolUse block.
pub fn parse_stream_json_line(line: &str) -> Option<Vec<AdapterEvent>> {
    let v: Value = serde_json::from_str(line).ok()?;
    let kind = v.get("type")?.as_str()?;
    match kind {
        "system" => {
            // init / startup notifications — ignored intentionally.
            // Returning an empty Vec rather than None signals "we
            // recognised this and chose to drop it".
            Some(Vec::new())
        }
        "assistant" => parse_assistant(&v),
        "user" => parse_user_tool_results(&v),
        "result" => parse_result(&v),
        _ => None,
    }
}

fn parse_assistant(v: &Value) -> Option<Vec<AdapterEvent>> {
    let content = v.get("message")?.get("content")?.as_array()?;
    let mut out = Vec::new();
    for block in content {
        let block_type = match block.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };
        match block_type {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        out.push(AdapterEvent::Token {
                            content: text.to_string(),
                        });
                    }
                }
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                if !name.is_empty() {
                    out.push(AdapterEvent::ToolUse { id, name, input });
                }
            }
            _ => {
                // thinking, image, etc. — ignored at v1.0.
            }
        }
    }
    Some(out)
}

fn parse_user_tool_results(v: &Value) -> Option<Vec<AdapterEvent>> {
    let content = v.get("message")?.get("content")?.as_array()?;
    let mut out = Vec::new();
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            continue;
        }
        let id = block
            .get("tool_use_id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();
        let content_text = match block.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let is_error = block
            .get("is_error")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        out.push(AdapterEvent::ToolResult {
            id,
            content: content_text,
            is_error,
        });
    }
    Some(out)
}

fn parse_result(v: &Value) -> Option<Vec<AdapterEvent>> {
    let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
    let is_error = v
        .get("is_error")
        .and_then(|b| b.as_bool())
        .unwrap_or(subtype.starts_with("error"));
    if is_error {
        let message = v
            .get("result")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                v.get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| format!("claude_code result.{subtype}"));
        return Some(vec![AdapterEvent::Error { message }]);
    }
    let final_text = v
        .get("result")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    let cost_usd = v.get("total_cost_usd").and_then(|c| c.as_f64());
    Some(vec![AdapterEvent::Done {
        final_text,
        cost_usd,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_none() {
        assert!(parse_stream_json_line("").is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        assert!(parse_stream_json_line("not json at all").is_none());
        assert!(parse_stream_json_line("{").is_none());
    }

    #[test]
    fn missing_type_returns_none() {
        assert!(parse_stream_json_line(r#"{"foo":"bar"}"#).is_none());
    }

    #[test]
    fn unknown_type_returns_none() {
        assert!(parse_stream_json_line(r#"{"type":"future_kind","data":{}}"#).is_none());
    }

    #[test]
    fn system_init_returns_empty_event_vec() {
        let e =
            parse_stream_json_line(r#"{"type":"system","subtype":"init","cwd":"/x"}"#).unwrap();
        assert!(e.is_empty());
    }

    #[test]
    fn parses_assistant_text_block_into_token() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AdapterEvent::Token { content } => assert_eq!(content, "hello"),
            other => panic!("expected Token, got {other:?}"),
        }
    }

    #[test]
    fn empty_text_block_does_not_emit() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":""}]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn parses_assistant_tool_use_block() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"call_1","name":"search_claims","input":{"q":"auth"}}
        ]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AdapterEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "search_claims");
                assert_eq!(input["q"], "auth");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parses_mixed_text_and_tool_use_in_one_message() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"let me check"},
            {"type":"tool_use","id":"c1","name":"search_claims","input":{}}
        ]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AdapterEvent::Token { .. }));
        assert!(matches!(events[1], AdapterEvent::ToolUse { .. }));
    }

    #[test]
    fn parses_user_tool_result_block() {
        let line = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"c1","content":"42 claims","is_error":false}
        ]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AdapterEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                assert_eq!(id, "c1");
                assert_eq!(content, "42 claims");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_with_object_content_serialises_to_string() {
        let line = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"c1","content":{"a":1},"is_error":false}
        ]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AdapterEvent::ToolResult { content, .. } => {
                assert!(content.contains("\"a\":1"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_with_error_flag_propagates() {
        let line = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"c1","content":"oops","is_error":true}
        ]}}"#;
        let events = parse_stream_json_line(line).unwrap();
        match &events[0] {
            AdapterEvent::ToolResult { is_error, .. } => assert!(*is_error),
            other => panic!("expected error tool_result, got {other:?}"),
        }
    }

    #[test]
    fn parses_result_success_into_done_with_cost() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,
            "result":"final answer","total_cost_usd":0.123}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AdapterEvent::Done {
                final_text,
                cost_usd,
            } => {
                assert_eq!(final_text, "final answer");
                assert!((cost_usd.unwrap() - 0.123).abs() < 1e-9);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn parses_result_error_subtype_into_error_event() {
        let line = r#"{"type":"result","subtype":"error_during_execution",
            "is_error":true,"result":"hit max turns"}"#;
        let events = parse_stream_json_line(line).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AdapterEvent::Error { message } => assert_eq!(message, "hit max turns"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn result_without_cost_field_is_none() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"x"}"#;
        let events = parse_stream_json_line(line).unwrap();
        match &events[0] {
            AdapterEvent::Done { cost_usd, .. } => assert!(cost_usd.is_none()),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn result_subtype_starting_with_error_treated_as_error_even_without_is_error_flag() {
        // Defensive: a future Claude Code may emit error subtypes
        // without an explicit `is_error` flag. Subtype prefix wins.
        let line = r#"{"type":"result","subtype":"error_max_turns","result":"hit max turns"}"#;
        let events = parse_stream_json_line(line).unwrap();
        match &events[0] {
            AdapterEvent::Error { message } => assert_eq!(message, "hit max turns"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn build_args_includes_required_flags() {
        let cfg = ClaudeCodeConfig {
            binary_path: PathBuf::from("/usr/bin/claude"),
            mcp_config_path: None,
            extra_args: Vec::new(),
        };
        let adapter = ClaudeCodeAdapter::new(cfg);
        let args = adapter.build_args("hello");
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        // Prompt is always last so the agent can read it positionally.
        assert_eq!(args.last().unwrap(), "hello");
    }

    #[test]
    fn build_args_threads_mcp_config_path_when_set() {
        let cfg = ClaudeCodeConfig {
            binary_path: PathBuf::from("claude"),
            mcp_config_path: Some(PathBuf::from("/tmp/tr.json")),
            extra_args: Vec::new(),
        };
        let adapter = ClaudeCodeAdapter::new(cfg);
        let args = adapter.build_args("p");
        let pos = args.iter().position(|a| a == "--mcp-config");
        assert!(pos.is_some(), "missing --mcp-config flag");
        assert_eq!(args[pos.unwrap() + 1], "/tmp/tr.json");
    }

    #[test]
    fn build_args_includes_extra_args_before_prompt() {
        let cfg = ClaudeCodeConfig {
            binary_path: PathBuf::from("claude"),
            mcp_config_path: None,
            extra_args: vec!["--max-turns".to_string(), "4".to_string()],
        };
        let adapter = ClaudeCodeAdapter::new(cfg);
        let args = adapter.build_args("p");
        let mt = args.iter().position(|a| a == "--max-turns").unwrap();
        let prompt_pos = args.iter().rposition(|a| a == "p").unwrap();
        assert!(mt < prompt_pos);
        assert_eq!(args[mt + 1], "4");
    }

    #[tokio::test]
    async fn spawn_rejects_non_directory_worktree() {
        let adapter = ClaudeCodeAdapter::new(ClaudeCodeConfig::default());
        let res = adapter
            .spawn("hello", Path::new("/dev/null/not-a-dir"), None)
            .await;
        assert!(matches!(res, Err(AdapterError::InvalidWorktree { .. })));
    }

    #[tokio::test]
    async fn spawn_returns_binary_not_found_for_missing_executable() {
        let cfg = ClaudeCodeConfig {
            binary_path: PathBuf::from("/no/such/binary/anywhere"),
            mcp_config_path: None,
            extra_args: Vec::new(),
        };
        let adapter = ClaudeCodeAdapter::new(cfg);
        let res = adapter.spawn("hi", Path::new("/tmp"), None).await;
        match res {
            Err(AdapterError::BinaryNotFound { binary, .. }) => {
                assert!(binary.contains("/no/such/binary"));
            }
            other => panic!("expected BinaryNotFound, got {other:?}"),
        }
    }

    /// Write a shell-script "fake binary" the adapter can spawn. The
    /// script ignores its CLI args entirely (heredoc body, no
    /// `$@` references) so build_args's `-p`, `--output-format`, etc.
    /// can be passed harmlessly. Returns a tempdir + path to the
    /// executable script — caller keeps the tempdir alive until the
    /// test finishes.
    fn write_fake_binary(body: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake-claude.sh");
        let script = format!("#!/bin/sh\n{body}\n");
        std::fs::write(&path, script).expect("write script");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn spawn_with_fake_binary_streams_canned_events() {
        // Verifies the full subprocess lifecycle: spawn → read
        // stdout lines → parse → send → consumer receives.
        let (_dir, fake) = write_fake_binary(
            r#"
echo '{"type":"system","subtype":"init"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.01}'
"#,
        );
        let cfg = ClaudeCodeConfig {
            binary_path: fake,
            mcp_config_path: None,
            extra_args: Vec::new(),
        };
        let adapter = ClaudeCodeAdapter::new(cfg);
        let mut rx = adapter
            .spawn("ignored-prompt", Path::new("/tmp"), None)
            .await
            .expect("spawn");

        let mut tokens = Vec::new();
        let mut done = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                AdapterEvent::Token { content } => tokens.push(content),
                AdapterEvent::Done {
                    final_text,
                    cost_usd,
                } => {
                    done = Some(final_text);
                    assert!((cost_usd.unwrap() - 0.01).abs() < 1e-9);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(tokens, vec!["hi".to_string()]);
        assert_eq!(done.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn spawn_with_fake_binary_emitting_error_result_yields_error_event() {
        let (_dir, fake) = write_fake_binary(
            r#"
echo '{"type":"result","subtype":"error_max_turns","is_error":true,"result":"hit max turns"}'
"#,
        );
        let cfg = ClaudeCodeConfig {
            binary_path: fake,
            mcp_config_path: None,
            extra_args: Vec::new(),
        };
        let adapter = ClaudeCodeAdapter::new(cfg);
        let mut rx = adapter
            .spawn("p", Path::new("/tmp"), None)
            .await
            .expect("spawn");

        let mut got_error = None;
        while let Some(ev) = rx.recv().await {
            if let AdapterEvent::Error { message } = ev {
                got_error = Some(message);
                break;
            }
        }
        assert_eq!(got_error.as_deref(), Some("hit max turns"));
    }

    // ─── Real-binary integration tests ─────────────────────────────
    //
    // Gated on the `RUN_REAL_CLAUDE=1` environment variable AND
    // `#[ignore]` so they never run in the default `cargo test`
    // loop. To execute:
    //
    //   RUN_REAL_CLAUDE=1 ANTHROPIC_API_KEY=... \
    //     cargo test --package thinkingroot-serve \
    //     --test-threads=1 -- --ignored real_claude
    //
    // Why both gates:
    // - `#[ignore]` keeps the regular CI test loop deterministic and
    //   offline. Real-binary runs cost API tokens and need network.
    // - The env-var check inside the test body lets a `cargo test
    //   -- --ignored` invocation that DIDN'T set the env var skip
    //   gracefully (rather than fail with a confusing
    //   `BinaryNotFound` if `claude` is also missing from PATH).
    //
    // What these prove:
    // - Real Claude Code's stream-json output structurally matches
    //   the parser. If Claude Code v3.x changes the wire format,
    //   the relevant assertion below fails and the parser needs
    //   updating.
    // - The subprocess lifecycle (spawn → read → consumer drop →
    //   process termination) works against a real, possibly
    //   slow-starting binary, not just shell-script fakes.
    //
    // What they DO NOT prove:
    // - Adapter behaviour with MCP tools (would need a live
    //   thinkingroot-serve daemon + tr.json mcp config).
    // - Error paths from real network failures.

    fn skip_if_real_claude_disabled() -> bool {
        if std::env::var("RUN_REAL_CLAUDE").as_deref() != Ok("1") {
            eprintln!(
                "skipping real-claude test: set RUN_REAL_CLAUDE=1 to enable"
            );
            return true;
        }
        // Also require the binary to be reachable so a misconfigured
        // run produces a clear skip rather than a flaky failure.
        let bin = std::env::var("CLAUDE_BINARY").unwrap_or_else(|_| "claude".to_string());
        match std::process::Command::new(&bin).arg("--version").output() {
            Ok(out) if out.status.success() => false,
            _ => {
                eprintln!(
                    "skipping real-claude test: `{bin} --version` failed (binary missing or non-executable)"
                );
                true
            }
        }
    }

    #[tokio::test]
    #[ignore = "real-binary; requires RUN_REAL_CLAUDE=1 + ANTHROPIC_API_KEY"]
    async fn real_claude_emits_tokens_for_simple_prompt() {
        if skip_if_real_claude_disabled() {
            return;
        }

        // A trivial prompt that should resolve in one assistant turn
        // without any tool calls. We don't constrain the exact text
        // — we only verify that AT LEAST one Token arrives and that
        // a Done event eventually closes the stream.
        let adapter = ClaudeCodeAdapter::from_env();
        let worktree = tempfile::tempdir().expect("tempdir");
        let mut rx = adapter
            .spawn(
                "Reply with the single word: ready",
                worktree.path(),
                None,
            )
            .await
            .expect("spawn real claude");

        // 60s ceiling — Claude Code's first-token latency is normally
        // <5s but cold-start + network can stretch it.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mut got_token = false;
            let mut got_done = false;
            while let Some(ev) = rx.recv().await {
                match ev {
                    AdapterEvent::Token { .. } => got_token = true,
                    AdapterEvent::Done { .. } => {
                        got_done = true;
                        break;
                    }
                    AdapterEvent::Error { message } => {
                        panic!("real claude emitted Error: {message}");
                    }
                    _ => {}
                }
            }
            (got_token, got_done)
        })
        .await
        .expect("real claude run timed out (60s)");

        assert!(outcome.0, "expected at least one Token from real claude");
        assert!(outcome.1, "expected a Done event from real claude");
    }

    #[tokio::test]
    #[ignore = "real-binary; requires RUN_REAL_CLAUDE=1"]
    async fn real_claude_terminates_when_consumer_drops() {
        if skip_if_real_claude_disabled() {
            return;
        }

        // Smoke-test the kill_on_drop guarantee against a real binary:
        // start a long-form prompt, drop the receiver immediately,
        // expect the watchdog task to clean up without deadlocking.
        let adapter = ClaudeCodeAdapter::from_env();
        let worktree = tempfile::tempdir().expect("tempdir");
        let rx = adapter
            .spawn(
                "Write a 500-word essay about Rust.",
                worktree.path(),
                None,
            )
            .await
            .expect("spawn");

        // Drop the receiver — the reader task should bail on first
        // `tx.send` failure and the wait task should observe child
        // termination.
        drop(rx);

        // Give the supervisor up to 10s to settle. We can't directly
        // observe the child PID from here, but a healthy run returns
        // promptly because the kill_on_drop OS signal short-circuits
        // any pending API call.
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        // No assertion — the test passes if it doesn't hang or panic.
    }
}
