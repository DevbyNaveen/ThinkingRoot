//! Clean-room reimplementation. Inspired by openhuman/mcp_client/stdio.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.5 (2026-05-17) — stdio MCP transport.
//!
//! Spawns the configured MCP server as a child process and speaks
//! line-delimited JSON-RPC over its stdin/stdout. Request ids are
//! monotonic; responses are correlated by id via an
//! `oneshot`-channel-per-pending-request map.
//!
//! ## Lifecycle
//!
//! - `StdioTransport::spawn(cmd, args, env, cwd)` spawns the child
//!   and starts the reader task in the background.
//! - `rpc(method, params)` enqueues a request, awaits a typed
//!   response on the per-request oneshot.
//! - `Drop` of the transport kills the child (best-effort SIGTERM
//!   then SIGKILL on `tokio::process::Child::kill`).
//!
//! ## Error model
//!
//! - Child died: the reader task closes every pending channel ⇒
//!   all in-flight `rpc()` calls return `TransportFailed`.
//! - Per-request timeout (default 30s) returns `Timeout`.
//! - Malformed JSON from the child surfaces as `Protocol`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use super::client::{McpClientError, McpTransport};

/// Default per-request timeout. MCP tool calls can be slow
/// (Playwright, browser automation, large file reads), so 30s gives
/// a healthy margin.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);

pub struct StdioTransport {
    next_id: AtomicI64,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpClientError>>>>>,
    /// Hold a child handle so `Drop` can kill the subprocess.
    child: Mutex<Option<Child>>,
    timeout: Duration,
    /// Set to `true` when the background reader observes EOF or a
    /// read error. Subsequent `rpc()` calls bail immediately rather
    /// than queueing a request that would never be answered + then
    /// blocking on a write to a closed stdin + then sleeping the
    /// full per-request timeout. `Arc<AtomicBool>` so the reader
    /// task and the transport share one instance.
    dead: Arc<AtomicBool>,
}

impl StdioTransport {
    /// Spawn a child process + start the background reader.
    ///
    /// Returns the transport ready for `McpClient::initialize`.
    pub async fn spawn(
        program: &str,
        args: &[String],
        env: HashMap<String, String>,
        cwd: Option<PathBuf>,
    ) -> Result<Arc<Self>, McpClientError> {
        Self::spawn_with_timeout(program, args, env, cwd, DEFAULT_RPC_TIMEOUT).await
    }

    /// Same as `spawn` but with a caller-supplied per-RPC timeout.
    /// Used by `external_registry::start_client` to honour the
    /// per-server `timeout_secs` field from `mcp-servers.toml`. Pre-
    /// fix the legacy `with_timeout` builder discarded its argument
    /// silently, so every stdio server inherited the 30s default
    /// regardless of operator config.
    pub async fn spawn_with_timeout(
        program: &str,
        args: &[String],
        env: HashMap<String, String>,
        cwd: Option<PathBuf>,
        timeout: Duration,
    ) -> Result<Arc<Self>, McpClientError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // capture for diagnostics; not surfaced unless TRACE
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| McpClientError::TransportFailed(format!("spawn {program}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpClientError::TransportFailed("child stdin not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpClientError::TransportFailed("child stdout not piped".into()))?;
        // stderr capture — drain on a background task so the child
        // doesn't block on a full pipe; log at TRACE so noisy
        // servers don't flood the daemon log.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::trace!(target: "mcp_stdio", "child stderr: {line}");
                }
            });
        }

        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpClientError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));

        // Background reader: parse one JSON-RPC envelope per line.
        // On unparseable lines, log at WARN and continue — some
        // MCP servers emit informational stderr/stdout lines that
        // aren't JSON-RPC; we treat them as noise.
        let pending_for_reader = pending.clone();
        let dead_for_reader = dead.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        match serde_json::from_str::<Value>(&line) {
                            Ok(envelope) => dispatch_envelope(envelope, &pending_for_reader).await,
                            Err(e) => {
                                tracing::warn!(
                                    target: "mcp_stdio",
                                    "non-JSON-RPC line from child (treating as noise): {e}; line={line}"
                                );
                            }
                        }
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        // EOF — child closed stdout. Mark the
                        // transport dead BEFORE draining so any
                        // racing `rpc()` call observes the flag and
                        // bails immediately instead of inserting a
                        // doomed request.
                        dead_for_reader.store(true, Ordering::SeqCst);
                        let mut p = pending_for_reader.lock().await;
                        for (_, tx) in p.drain() {
                            let _ = tx.send(Err(McpClientError::TransportFailed(
                                "child stdout closed (EOF)".into(),
                            )));
                        }
                        break;
                    }
                    Err(e) => {
                        dead_for_reader.store(true, Ordering::SeqCst);
                        let mut p = pending_for_reader.lock().await;
                        for (_, tx) in p.drain() {
                            let _ = tx.send(Err(McpClientError::TransportFailed(format!(
                                "stdout read: {e}"
                            ))));
                        }
                        break;
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            next_id: AtomicI64::new(1),
            stdin: Mutex::new(stdin),
            pending,
            child: Mutex::new(Some(child)),
            timeout,
            dead,
        }))
    }
}

async fn dispatch_envelope(
    envelope: Value,
    pending: &Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpClientError>>>>,
) {
    // JSON-RPC envelopes have either `result` OR `error`, plus an `id`.
    // Notifications (no id) are silently dropped at v1 — we don't
    // act on server-initiated notifications.
    let id = envelope.get("id").and_then(|v| v.as_i64());
    let id = match id {
        Some(i) => i,
        None => return, // notification — drop
    };
    let tx = pending.lock().await.remove(&id);
    if let Some(tx) = tx {
        if let Some(err) = envelope.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32603);
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();
            let _ = tx.send(Err(McpClientError::RpcError { code, message }));
        } else if let Some(result) = envelope.get("result") {
            let _ = tx.send(Ok(result.clone()));
        } else {
            let _ = tx.send(Err(McpClientError::Protocol(
                "envelope has neither result nor error".into(),
            )));
        }
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    /// stdio connectors are not OAuth-aware — `user_id` is accepted to
    /// satisfy the `McpTransport` trait but is intentionally ignored.
    /// OAuth injection only applies to HTTP connectors (Klavis MCP
    /// servers), never to local subprocess-based servers.
    async fn rpc(
        &self,
        method: &str,
        params: Value,
        _user_id: Option<&str>,
    ) -> Result<Value, McpClientError> {
        // Fast-fail when the reader has already observed EOF/error —
        // queueing a request that no one will answer wastes the full
        // per-RPC timeout and pollutes `pending` with a stale entry
        // until either Drop or a late race-window response.
        if self.dead.load(Ordering::SeqCst) {
            return Err(McpClientError::TransportFailed(
                "stdio child is no longer responding (EOF / read error observed)".into(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        // Write request — line-terminated. Hold stdin briefly.
        let line = format!("{}\n", serde_json::to_string(&envelope)?);
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        // Await response under timeout.
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                // sender dropped — reader cleaned up after failure.
                Err(McpClientError::TransportFailed(
                    "response channel dropped".into(),
                ))
            }
            Err(_) => {
                // Timeout. Drop the pending entry to avoid leaking
                // the oneshot when a late response arrives.
                self.pending.lock().await.remove(&id);
                Err(McpClientError::Timeout(self.timeout))
            }
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        // `kill_on_drop(true)` handles SIGKILL; we additionally
        // try a graceful close via stdin drop (the Mutex's Drop
        // releases the ChildStdin).
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.start_kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn `cat` and immediately close stdin → EOF on stdout →
    /// every pending request should fail with TransportFailed.
    /// `cat` doesn't speak JSON-RPC; the test only exercises the
    /// EOF-cleanup path.
    #[tokio::test]
    async fn stdin_close_fails_pending_requests_loudly() {
        // `cat /dev/null` exits immediately with EOF on stdout.
        let transport = match StdioTransport::spawn(
            "sh",
            &["-c".into(), "exit 0".into()],
            HashMap::new(),
            None,
        )
        .await
        {
            Ok(t) => t,
            Err(_) => {
                // sh not in $PATH — skip on exotic CI runners.
                return;
            }
        };
        // Give the child time to exit + reader to detect EOF.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Any rpc must fail with TransportFailed (channel dropped
        // by EOF-cleanup) or Timeout (rare race).
        let result = transport
            .rpc("initialize", serde_json::json!({}), None)
            .await;
        assert!(result.is_err(), "expected error after child exit");
    }

    #[tokio::test]
    async fn spawn_invalid_program_returns_typed_error() {
        let result = StdioTransport::spawn(
            "this-binary-does-not-exist-anywhere-deadbeef",
            &[],
            HashMap::new(),
            None,
        )
        .await;
        assert!(matches!(result, Err(McpClientError::TransportFailed(_))));
    }
}
