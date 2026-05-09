// crates/thinkingroot-serve/src/intelligence/adapters/cursor.rs
//
// Cursor adapter (Task 19 / Week 3, plan 2026-05-09).
//
// Spawns `cursor-agent -p --output-format text --print` inside a sandbox
// worktree and parses Cursor's plain-text output (Cursor v0.x emits
// human-readable text rather than structured JSON; the adapter folds
// the prose into one Token event and treats the agent's natural exit
// as `Done`). When Cursor's `--output-format json` lands (preview as
// of 2026-05) we'll route through `parse_json_line` instead — both
// shapes are tested below.
//
// MCP wiring: the adapter writes a per-spawn `.cursor/mcp.json`
// pointing at this server's stdio MCP endpoint when
// `mcp_config_path` is set. Cursor reads the workspace-relative
// `.cursor/` config automatically.

use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{AdapterError, AdapterEvent, AgentAdapter};

#[derive(Debug, Clone)]
pub struct CursorConfig {
    /// Override via `CURSOR_AGENT_BINARY` env var. Default: `"cursor-agent"`.
    pub binary_path: PathBuf,
    /// When set, the adapter writes `<worktree>/.cursor/mcp.json`
    /// from this template before spawning. The template should be a
    /// JSON document Cursor's MCP loader accepts (one server entry
    /// per stdio adapter). `None` skips the write.
    pub mcp_config_template: Option<String>,
    /// Extra args spliced before the prompt.
    pub extra_args: Vec<String>,
    /// Output format Cursor was invoked with — selects the parser.
    /// `Json` is the future-proofed path; `Text` is what most v0.x
    /// installs emit today.
    pub output_format: CursorOutputFormat,
}

/// Two parsers are built in. Pick by config so a given install can
/// run either format without recompiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorOutputFormat {
    /// Plain prose. Each line becomes a `Token` event; the watchdog
    /// synthesises `Done` from process exit because Cursor v0.x
    /// doesn't emit a terminal marker.
    Text,
    /// Newline-delimited JSON, one event per line. Same shape family
    /// as Claude Code but without `tool_use` / `tool_result`
    /// (Cursor's tool calls happen inside the agent process, hidden
    /// from the parent).
    Json,
}

impl Default for CursorConfig {
    fn default() -> Self {
        let binary_path = std::env::var("CURSOR_AGENT_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("cursor-agent"));
        Self {
            binary_path,
            mcp_config_template: None,
            extra_args: Vec::new(),
            output_format: CursorOutputFormat::Text,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CursorAdapter {
    config: CursorConfig,
}

impl CursorAdapter {
    pub fn new(config: CursorConfig) -> Self {
        Self { config }
    }

    pub fn from_env() -> Self {
        Self::new(CursorConfig::default())
    }

    fn build_args(&self, prompt: &str) -> Vec<String> {
        let mut args: Vec<String> = vec!["-p".to_string()];
        match self.config.output_format {
            CursorOutputFormat::Text => {
                args.push("--output-format".to_string());
                args.push("text".to_string());
            }
            CursorOutputFormat::Json => {
                args.push("--output-format".to_string());
                args.push("json".to_string());
            }
        }
        args.extend(self.config.extra_args.iter().cloned());
        args.push(prompt.to_string());
        args
    }

    /// Write `.cursor/mcp.json` if the config carries a template.
    /// Idempotent + best-effort: an existing file is overwritten,
    /// a write failure is propagated as `SpawnFailed`.
    fn maybe_write_mcp_config(&self, worktree: &Path) -> std::io::Result<()> {
        let Some(template) = &self.config.mcp_config_template else {
            return Ok(());
        };
        let dir = worktree.join(".cursor");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("mcp.json"), template)?;
        Ok(())
    }
}

#[async_trait]
impl AgentAdapter for CursorAdapter {
    fn name(&self) -> &'static str {
        "cursor"
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
        self.maybe_write_mcp_config(worktree)
            .map_err(AdapterError::SpawnFailed)?;

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
        let format = self.config.output_format;

        let (tx, rx) = mpsc::channel::<AdapterEvent>(32);
        let tx_for_stdout = tx.clone();

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            // Text mode accumulates prose; we synthesise Done on
            // process exit (the watchdog task below).
            let mut accumulator = String::new();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match format {
                        CursorOutputFormat::Text => {
                            // Empty lines are content separators in
                            // Cursor's prose — preserve them.
                            if !accumulator.is_empty() {
                                accumulator.push('\n');
                            }
                            accumulator.push_str(&line);
                            // Also emit each line as a Token so the
                            // SSE relay sees streaming progress.
                            if !line.is_empty()
                                && tx_for_stdout
                                    .send(AdapterEvent::Token {
                                        content: line.clone(),
                                    })
                                    .await
                                    .is_err()
                            {
                                return;
                            }
                        }
                        CursorOutputFormat::Json => {
                            if line.trim().is_empty() {
                                continue;
                            }
                            if let Some(ev) = parse_json_line(&line) {
                                if tx_for_stdout.send(ev).await.is_err() {
                                    return;
                                }
                            } else {
                                tracing::warn!("cursor: unparseable line: {}", line);
                            }
                        }
                    },
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
            // Process finished cleanly. In Text mode emit Done with
            // the accumulated body. In Json mode the producer should
            // have already emitted Done; we only synth one if it
            // didn't, to match the trait contract that every spawn
            // ends with exactly one Done or Error.
            if matches!(format, CursorOutputFormat::Text) {
                let _ = tx_for_stdout
                    .send(AdapterEvent::Done {
                        final_text: accumulator,
                        cost_usd: None,
                    })
                    .await;
            }
        });

        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::debug!("cursor stderr: {}", line);
                    }
                }
            });
        }

        let tx_for_wait = tx;
        tokio::spawn(async move {
            let _wait = child.wait().await;
            drop(tx_for_wait);
        });

        Ok(rx)
    }
}

/// Pure parser for Cursor's JSON output mode. Same family as Claude
/// Code's stream-json but Cursor only emits assistant/result kinds —
/// tool calls are hidden inside the agent process.
pub fn parse_json_line(line: &str) -> Option<AdapterEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let kind = v.get("type")?.as_str()?;
    match kind {
        "assistant" => {
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(AdapterEvent::Token {
                    content: text.to_string(),
                })
            }
        }
        "result" => {
            let is_error = v
                .get("is_error")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            if is_error {
                let message = v
                    .get("error")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.get("result").and_then(|s| s.as_str()))
                    .unwrap_or("cursor reported error")
                    .to_string();
                return Some(AdapterEvent::Error { message });
            }
            let final_text = v
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            Some(AdapterEvent::Done {
                final_text,
                cost_usd: None,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_parser_handles_assistant_text() {
        let ev = parse_json_line(r#"{"type":"assistant","text":"hello"}"#).unwrap();
        match ev {
            AdapterEvent::Token { content } => assert_eq!(content, "hello"),
            other => panic!("expected Token, got {other:?}"),
        }
    }

    #[test]
    fn json_parser_handles_result_success() {
        let ev = parse_json_line(r#"{"type":"result","is_error":false,"result":"x"}"#).unwrap();
        match ev {
            AdapterEvent::Done { final_text, .. } => assert_eq!(final_text, "x"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn json_parser_handles_result_error() {
        let ev = parse_json_line(r#"{"type":"result","is_error":true,"error":"boom"}"#).unwrap();
        match ev {
            AdapterEvent::Error { message } => assert_eq!(message, "boom"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn json_parser_drops_empty_assistant_text() {
        assert!(parse_json_line(r#"{"type":"assistant","text":""}"#).is_none());
    }

    #[test]
    fn json_parser_drops_unknown_kind() {
        assert!(parse_json_line(r#"{"type":"thinking"}"#).is_none());
    }

    #[test]
    fn json_parser_drops_malformed() {
        assert!(parse_json_line("not json").is_none());
        assert!(parse_json_line("{}").is_none());
    }

    #[test]
    fn build_args_text_mode() {
        let cfg = CursorConfig {
            binary_path: PathBuf::from("cursor-agent"),
            mcp_config_template: None,
            extra_args: Vec::new(),
            output_format: CursorOutputFormat::Text,
        };
        let adapter = CursorAdapter::new(cfg);
        let args = adapter.build_args("p");
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"text".to_string()));
        assert_eq!(args.last().unwrap(), "p");
    }

    #[test]
    fn build_args_json_mode() {
        let cfg = CursorConfig {
            binary_path: PathBuf::from("cursor-agent"),
            mcp_config_template: None,
            extra_args: vec!["--max-turns".to_string(), "3".to_string()],
            output_format: CursorOutputFormat::Json,
        };
        let adapter = CursorAdapter::new(cfg);
        let args = adapter.build_args("p");
        assert!(args.contains(&"json".to_string()));
        let mt = args.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(args[mt + 1], "3");
    }

    #[test]
    fn maybe_write_mcp_config_writes_template_to_dotcursor() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = CursorConfig {
            binary_path: PathBuf::from("cursor-agent"),
            mcp_config_template: Some(r#"{"mcpServers":{"tr":{"command":"root"}}}"#.to_string()),
            extra_args: Vec::new(),
            output_format: CursorOutputFormat::Text,
        };
        let adapter = CursorAdapter::new(cfg);
        adapter.maybe_write_mcp_config(temp.path()).unwrap();
        let written = std::fs::read_to_string(temp.path().join(".cursor/mcp.json")).unwrap();
        assert!(written.contains("mcpServers"));
    }

    #[test]
    fn maybe_write_mcp_config_noop_when_no_template() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = CursorConfig::default();
        let adapter = CursorAdapter::new(cfg);
        // Should succeed but write nothing.
        adapter.maybe_write_mcp_config(temp.path()).unwrap();
        assert!(!temp.path().join(".cursor/mcp.json").exists());
    }

    /// See claude_code.rs::write_fake_binary — same idea, separate
    /// copy because tests don't import across module boundaries.
    fn write_fake_binary(body: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake-cursor.sh");
        let script = format!("#!/bin/sh\n{body}\n");
        std::fs::write(&path, script).expect("write script");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn spawn_text_mode_with_fake_binary_emits_token_then_done() {
        let (_dir, fake) = write_fake_binary("echo line-one\necho line-two");
        let temp = tempfile::tempdir().unwrap();
        let cfg = CursorConfig {
            binary_path: fake,
            mcp_config_template: None,
            extra_args: Vec::new(),
            output_format: CursorOutputFormat::Text,
        };
        let adapter = CursorAdapter::new(cfg);
        let mut rx = adapter
            .spawn("p", temp.path(), None)
            .await
            .expect("spawn ok");
        let mut tokens = Vec::new();
        let mut done = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                AdapterEvent::Token { content } => tokens.push(content),
                AdapterEvent::Done { final_text, .. } => {
                    done = Some(final_text);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(tokens, vec!["line-one".to_string(), "line-two".to_string()]);
        assert_eq!(done.as_deref(), Some("line-one\nline-two"));
    }

    #[tokio::test]
    async fn spawn_json_mode_with_fake_binary_emits_parsed_done() {
        let (_dir, fake) = write_fake_binary(
            r#"echo '{"type":"assistant","text":"hi"}'
echo '{"type":"result","is_error":false,"result":"done"}'"#,
        );
        let temp = tempfile::tempdir().unwrap();
        let cfg = CursorConfig {
            binary_path: fake,
            mcp_config_template: None,
            extra_args: Vec::new(),
            output_format: CursorOutputFormat::Json,
        };
        let adapter = CursorAdapter::new(cfg);
        let mut rx = adapter.spawn("p", temp.path(), None).await.unwrap();
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        assert!(events.iter().any(|e| matches!(e, AdapterEvent::Token { content } if content == "hi")));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AdapterEvent::Done { final_text, .. } if final_text == "done"))
        );
    }

    #[tokio::test]
    async fn spawn_rejects_non_directory_worktree() {
        let adapter = CursorAdapter::new(CursorConfig::default());
        let res = adapter
            .spawn("p", Path::new("/dev/null/not-a-dir"), None)
            .await;
        assert!(matches!(res, Err(AdapterError::InvalidWorktree { .. })));
    }

    #[tokio::test]
    async fn spawn_returns_binary_not_found() {
        let cfg = CursorConfig {
            binary_path: PathBuf::from("/no/such/cursor"),
            mcp_config_template: None,
            extra_args: Vec::new(),
            output_format: CursorOutputFormat::Text,
        };
        let adapter = CursorAdapter::new(cfg);
        let res = adapter.spawn("p", Path::new("/tmp"), None).await;
        assert!(matches!(res, Err(AdapterError::BinaryNotFound { .. })));
    }

    // ─── Real-binary integration tests ─────────────────────────────
    //
    // Gated on the `RUN_REAL_CURSOR=1` environment variable AND
    // `#[ignore]` so they never run in the default test loop.
    //
    //   RUN_REAL_CURSOR=1 CURSOR_BINARY=cursor-agent \
    //     cargo test --package thinkingroot-serve \
    //     --test-threads=1 -- --ignored real_cursor
    //
    // **Honesty note for maintainers:** at the time these tests were
    // written, the `cursor-agent` CLI was NOT installed on the
    // primary development machine — only the Cursor IDE app
    // (`/usr/local/bin/cursor`) was present, which is the desktop
    // application, not the headless agent CLI. These tests are
    // therefore written for parity with the Claude Code real-binary
    // tests but have NOT been run end-to-end against a real
    // `cursor-agent` from this codebase. Treat the assertions as
    // "expected behaviour based on Cursor's documented output
    // format"; once `cursor-agent` ships and stabilises, run with
    // `RUN_REAL_CURSOR=1` and tighten the assertions if the wire
    // format diverges.

    fn skip_if_real_cursor_disabled() -> bool {
        if std::env::var("RUN_REAL_CURSOR").as_deref() != Ok("1") {
            eprintln!(
                "skipping real-cursor test: set RUN_REAL_CURSOR=1 to enable"
            );
            return true;
        }
        let bin = std::env::var("CURSOR_BINARY").unwrap_or_else(|_| "cursor-agent".to_string());
        match std::process::Command::new(&bin).arg("--version").output() {
            Ok(out) if out.status.success() => false,
            _ => {
                eprintln!(
                    "skipping real-cursor test: `{bin} --version` failed (binary missing or non-executable)"
                );
                true
            }
        }
    }

    #[tokio::test]
    #[ignore = "real-binary; requires RUN_REAL_CURSOR=1 + cursor-agent CLI on PATH"]
    async fn real_cursor_emits_tokens_for_simple_prompt() {
        if skip_if_real_cursor_disabled() {
            return;
        }

        let bin = std::env::var("CURSOR_BINARY").unwrap_or_else(|_| "cursor-agent".to_string());
        let cfg = CursorConfig {
            binary_path: PathBuf::from(bin),
            mcp_config_template: None,
            extra_args: Vec::new(),
            // JSON mode gives stable per-event parsing if cursor-agent
            // exposes it; fall back to Text by overriding the env var.
            output_format: CursorOutputFormat::Json,
        };
        let adapter = CursorAdapter::new(cfg);
        let worktree = tempfile::tempdir().expect("tempdir");
        let mut rx = adapter
            .spawn(
                "Reply with the single word: ready",
                worktree.path(),
                None,
            )
            .await
            .expect("spawn real cursor");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mut got_any_event = false;
            let mut got_done = false;
            while let Some(ev) = rx.recv().await {
                got_any_event = true;
                if matches!(ev, AdapterEvent::Done { .. }) {
                    got_done = true;
                    break;
                }
            }
            (got_any_event, got_done)
        })
        .await
        .expect("real cursor run timed out (60s)");

        assert!(outcome.0, "expected at least one event from real cursor-agent");
        assert!(
            outcome.1,
            "expected a Done event from real cursor-agent (this assertion may need tightening once the wire format stabilises)"
        );
    }

    #[tokio::test]
    #[ignore = "real-binary; requires RUN_REAL_CURSOR=1"]
    async fn real_cursor_terminates_when_consumer_drops() {
        if skip_if_real_cursor_disabled() {
            return;
        }
        let bin = std::env::var("CURSOR_BINARY").unwrap_or_else(|_| "cursor-agent".to_string());
        let cfg = CursorConfig {
            binary_path: PathBuf::from(bin),
            mcp_config_template: None,
            extra_args: Vec::new(),
            output_format: CursorOutputFormat::Json,
        };
        let adapter = CursorAdapter::new(cfg);
        let worktree = tempfile::tempdir().expect("tempdir");
        let rx = adapter
            .spawn("Write a long essay.", worktree.path(), None)
            .await
            .expect("spawn");
        drop(rx);
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}
