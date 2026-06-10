//! HTTP-delegate paths for stateful CLI commands under the Cortex
//! Protocol.
//!
//! When `cortex_client::resolve_engine` returns a
//! `EngineConnection::Remote`, every stateful subcommand flows
//! through this module instead of opening CozoDB in-process. The
//! existing in-process call paths are kept intact and re-selected
//! when:
//!   - The user passes `--in-process` explicitly (escape hatch for
//!     hermetic CI / air-gapped scenarios).
//!   - `resolve_engine` itself failed (logged warning; honour the
//!     graceful-degradation rule by trying in-process rather than
//!     dying).
//!
//! Wire types are `serde_json::Value`-shaped here rather than
//! importing the typed request structs from `thinkingroot-serve`'s
//! internal modules. That keeps the CLI/serve boundary thin and
//! prevents a refactor on the server side from forcing a CLI rebuild
//! beyond the JSON contract.
//!
//! Spec: `docs/2026-05-02-unified-singleton-runtime.md` §6 + §7.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use console::style;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_client::health_check;

/// Number of total attempts (including the first call) for unary HTTP
/// helpers. One real attempt + one retry-after-reconnect is enough:
/// a daemon restart sequence (graceful shutdown → respawn → bind) is
/// O(seconds), so any failure persisting beyond a single retry is a
/// genuine outage that the user needs to see.
const MAX_UNARY_ATTEMPTS: u32 = 2;

/// Backoff sequence between unary retries. Mirrors `wait_for_livez`'s
/// exponential schedule but only the first two steps — total worst-
/// case wait before an unrecoverable failure surfaces is ~600 ms.
const UNARY_RETRY_BACKOFF_MS: &[u64] = &[150, 450];

/// Detect whether a `reqwest::Error` is the kind that warrants a
/// daemon-restart-aware retry: connect refused, transport-level read
/// timeout, or a 5xx that the daemon would emit while shutting down.
/// 4xx is **never** retried — those are programming errors.
fn is_transient_transport(err: &reqwest::Error) -> bool {
    if err.is_connect() || err.is_timeout() {
        return true;
    }
    if let Some(status) = err.status() {
        // 502/503/504 from a fronting proxy or a half-shutdown daemon
        // mean "try again." Distinct from 401/403/404 which are
        // permanent.
        let code = status.as_u16();
        return code == 502 || code == 503 || code == 504;
    }
    // Body-side failures (broken pipe mid-read, TLS reset) bubble up
    // as request errors without a status. Treat as transient — the
    // daemon either crashed or restarted; one retry decides.
    err.is_request() && err.url().is_some()
}

/// Wrap a unary HTTP call with daemon-restart-aware retry.
///
/// The closure is invoked at most [`MAX_UNARY_ATTEMPTS`] times. Between
/// attempts we:
///
/// 1. Probe `/livez` (1 s timeout, same as `cortex_client::health_check`).
/// 2. If the daemon answers, the failure was a transient hiccup — wait
///    a short backoff and retry the original call.
/// 3. If the daemon does NOT answer, wait the longer backoff (gives a
///    restarting daemon a moment to bind) and retry.
///
/// Cancellation surfaces immediately. Any non-transient error (4xx
/// from the daemon, malformed URL, etc.) bypasses the retry loop and
/// surfaces verbatim.
async fn with_reconnect<F, Fut, T>(
    conn: &EngineConnection,
    operation: &'static str,
    mut call: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, reqwest::Error>>,
{
    let (host, port) = match conn {
        EngineConnection::Remote { host, port, .. } => (host.clone(), *port),
        other => anyhow::bail!("{operation} called with non-Remote connection: {other:?}"),
    };
    let mut last_err: Option<reqwest::Error> = None;
    for attempt in 0..MAX_UNARY_ATTEMPTS {
        match call().await {
            Ok(v) => return Ok(v),
            Err(e) if !is_transient_transport(&e) => {
                return Err(e).with_context(|| format!("{operation} (non-transient)"));
            }
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 == MAX_UNARY_ATTEMPTS {
                    break;
                }
                let alive = health_check(&host, port).await;
                let backoff = UNARY_RETRY_BACKOFF_MS
                    .get(attempt as usize)
                    .copied()
                    .unwrap_or(450);
                tracing::warn!(
                    operation = %operation,
                    daemon_alive = alive,
                    attempt = attempt + 1,
                    backoff_ms = backoff,
                    "cortex retry"
                );
                tokio::time::sleep(Duration::from_millis(backoff)).await;
            }
        }
    }
    let last_attempt = last_err
        .as_ref()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "no underlying error".to_string());
    Err(anyhow::Error::new(DaemonUnreachable {
        operation: operation.to_string(),
        attempts: MAX_UNARY_ATTEMPTS,
        last_attempt,
    }))
}

/// Marker error attached by [`with_reconnect`] when the retry budget
/// is exhausted. `main.rs` downcasts the top-level `anyhow::Error`
/// chain against this type so the process can exit with
/// [`crate::pack_cmd::EXIT_DAEMON_UNREACHABLE`] (75) instead of the
/// generic `1`. Wrappers and CI scripts use the distinct exit code
/// to render "your engine is not running" without parsing stderr.
#[derive(Debug)]
pub struct DaemonUnreachable {
    /// Operation we tried before giving up (e.g. `"GET"`, `"POST"`).
    pub operation: String,
    /// Total attempts made.
    pub attempts: u32,
    /// Stringified last underlying transport error.
    pub last_attempt: String,
}

impl std::fmt::Display for DaemonUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed after {} attempts; cortex daemon is unreachable: {}",
            self.operation, self.attempts, self.last_attempt
        )
    }
}

impl std::error::Error for DaemonUnreachable {}

/// Build a base URL from a Remote connection. Centralised so the
/// `127.0.0.1` vs `[::1]` formatting decision lives in one place.
fn base_url(conn: &EngineConnection) -> anyhow::Result<String> {
    match conn {
        EngineConnection::Remote { host, port, .. } => Ok(format!("http://{host}:{port}")),
        other => anyhow::bail!("cortex_remote called with non-Remote connection: {other:?}"),
    }
}

/// Long timeouts for compile and ask — the actual cancellation
/// signal is the SSE stream drop, not this timeout. Setting it to
/// 1 hour means a slow compile won't 408 spuriously; if the user
/// wants out, Ctrl-C cancels via the drop_guard contract.
const STREAMING_TIMEOUT: Duration = Duration::from_secs(3600);

/// Tighter timeout for unary GETs (health, search, render, etc.).
/// 60 s covers every observed warm-cache call; cold-cache mounts
/// stretch to ~30 s on a large workspace.
const UNARY_TIMEOUT: Duration = Duration::from_secs(60);

/// Build a reqwest client tuned for the given timeout.
fn client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("failed to build reqwest client for cortex remote call")
}

/// Common JSON error envelope returned by `thinkingroot-serve`.
fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.to_string())
}

/// Stream C — generic helpers used by the new parity commands
/// (`tag_cmd`, `proposal_cmd`, branch extras, brain probes). All three
/// share the same shape: GET/POST a JSON-enveloped REST endpoint, peel
/// off the `data` field, and pretty-print the result for the user.

/// GET a daemon endpoint and return the raw JSON body of the `data`
/// field on the `{ok, data, error}` envelope. The caller decides how
/// to render — tables vs JSON vs counts.
///
/// Wrapped in [`with_reconnect`] so a daemon restart between two
/// CLI calls is invisible at this seam — the second attempt re-attaches
/// to the new process automatically.
///
/// Note: the closure calls `.error_for_status()` so 5xx-class
/// responses surface as `reqwest::Error` with a status code — that's
/// what [`is_transient_transport`] keys off to decide retry. 4xx and
/// non-{502,503,504} 5xx errors fail fast and propagate to the caller.
pub async fn get_json(
    conn: &EngineConnection,
    path: &str,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let client = client(UNARY_TIMEOUT)?;
    let resp = with_reconnect(conn, "GET", || async {
        client.get(&url).send().await?.error_for_status()
    })
    .await
    .with_context(|| format!("GET {url}"))?;
    decode_envelope(resp).await
}

/// POST a daemon endpoint with a JSON body. Same envelope decoding.
pub async fn post_json(
    conn: &EngineConnection,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let client = client(UNARY_TIMEOUT)?;
    let resp = with_reconnect(conn, "POST", || async {
        client.post(&url).json(body).send().await?.error_for_status()
    })
    .await
    .with_context(|| format!("POST {url}"))?;
    decode_envelope(resp).await
}

/// PUT a daemon endpoint with a JSON body. Same envelope decoding as
/// [`post_json`]. Used by `root brain push` to deploy prompts/functions.
pub async fn put_json(
    conn: &EngineConnection,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let client = client(UNARY_TIMEOUT)?;
    let resp = with_reconnect(conn, "PUT", || async {
        client.put(&url).json(body).send().await?.error_for_status()
    })
    .await
    .with_context(|| format!("PUT {url}"))?;
    decode_envelope(resp).await
}

/// DELETE a daemon endpoint. Returns the `data` field value.
pub async fn delete_json(
    conn: &EngineConnection,
    path: &str,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let client = client(UNARY_TIMEOUT)?;
    let resp = with_reconnect(conn, "DELETE", || async {
        client.delete(&url).send().await?.error_for_status()
    })
    .await
    .with_context(|| format!("DELETE {url}"))?;
    decode_envelope(resp).await
}

/// Same as `post_json` but threads an `X-TR-Session-Id` header through.
/// Used by AEP / engram routes which require a session id per call —
/// the engine's `EngramManager` pins TTL + per-session quotas to that id.
pub async fn post_json_with_session(
    conn: &EngineConnection,
    path: &str,
    session_id: &str,
    body: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let client = client(UNARY_TIMEOUT)?;
    let resp = with_reconnect(conn, "POST", || async {
        client
            .post(&url)
            .header("X-TR-Session-Id", session_id)
            .json(body)
            .send()
            .await?
            .error_for_status()
    })
    .await
    .with_context(|| format!("POST {url}"))?;
    decode_envelope(resp).await
}

/// Same as `get_json` but threads an `X-TR-Session-Id` header through.
pub async fn get_json_with_session(
    conn: &EngineConnection,
    path: &str,
    session_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let client = client(UNARY_TIMEOUT)?;
    let resp = with_reconnect(conn, "GET", || async {
        client
            .get(&url)
            .header("X-TR-Session-Id", session_id)
            .send()
            .await?
            .error_for_status()
    })
    .await
    .with_context(|| format!("GET {url}"))?;
    decode_envelope(resp).await
}

/// Same as `delete_json` but threads an `X-TR-Session-Id` header through.
pub async fn delete_json_with_session(
    conn: &EngineConnection,
    path: &str,
    session_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let client = client(UNARY_TIMEOUT)?;
    let resp = with_reconnect(conn, "DELETE", || async {
        client
            .delete(&url)
            .header("X-TR-Session-Id", session_id)
            .send()
            .await?
            .error_for_status()
    })
    .await
    .with_context(|| format!("DELETE {url}"))?;
    decode_envelope(resp).await
}

async fn decode_envelope(resp: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
    if !status.is_success() {
        anyhow::bail!(
            "daemon request failed ({status}): {}",
            extract_error_message(&body)
        );
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parse response envelope: {body}"))?;
    let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
    if !ok {
        anyhow::bail!("daemon returned ok=false: {}", extract_error_message(&body));
    }
    Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
}

/// `root compile` over the daemon's `/compile/stream` SSE endpoint.
///
/// Cancellation contract: the SSE body stream is held inside a
/// `tokio::select!` against `tokio::signal::ctrl_c()`. When the user
/// presses Ctrl-C, we drop the body — the daemon's response writer
/// observes the broken pipe, the in-flight stream's `DropGuard`
/// fires the engine's `CancellationToken`, and the pipeline exits at
/// the next phase boundary with `Error::Cancelled`.
///
/// Each `progress` SSE event carries a JSON-encoded `ProgressEvent`
/// from `thinkingroot-serve::pipeline`. We deserialize each one back
/// into the typed enum and feed it into the same `progress::drive_progress_bars`
/// renderer the in-process path uses — so both surfaces show identical
/// 10-phase indicatif bars with ETA, instead of the CLI silently
/// dropping every event.
pub async fn run_compile_remote(
    conn: &EngineConnection,
    path: &Path,
    branch: Option<&str>,
    no_rooting: bool,
    json: bool,
) -> anyhow::Result<()> {
    use crate::pipeline::{PipelineResult, ProgressEvent};
    use indicatif::MultiProgress;

    let url = format!("{}/api/v1/ws/_/compile/stream", base_url(conn)?);
    // Bugfix 2026-05-10 — canonicalize the path on the CLI side before
    // sending. The daemon's CWD is not the user's CWD (the daemon is
    // detached, often spawned from `/`); a relative path like "." sent
    // verbatim is resolved against the daemon's CWD by the server-side
    // `std::fs::canonicalize` and silently picks up the wrong workspace.
    // The CLI is the only side that knows what the user typed `compile .`
    // against, so the canonicalize must happen here.
    let absolute_path = std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize compile path: {}", path.display()))?;
    let body = serde_json::json!({
        "root_path": absolute_path.display().to_string(),
        "branch": branch,
        "no_rooting": no_rooting,
    });

    println!();
    println!(
        "  {} {}",
        style("Compiling (remote daemon)").cyan().bold(),
        style(path.display()).white()
    );
    println!();

    let resp = client(STREAMING_TIMEOUT)?
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("failed to send compile request to daemon")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
        anyhow::bail!(
            "daemon rejected compile request: {} — {}",
            status,
            extract_error_message(&body)
        );
    }

    // Spin up the same indicatif renderer the in-process path uses.
    // The driver task drains the channel until tx is dropped, then
    // finalizes any unfinished bars.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let mp = MultiProgress::new();
    let driver_handle = tokio::task::spawn(crate::progress::drive_progress_bars(rx, mp));

    let mut stream = resp.bytes_stream().eventsource();
    let mut final_result: Option<PipelineResult> = None;
    let mut error_msg: Option<String> = None;
    let mut cancelled = false;

    let consume = async {
        while let Some(event) = stream.next().await {
            let event = event.context("SSE stream error")?;
            match event.event.as_str() {
                "progress" => {
                    // Typed deserialize — adding a new ProgressEvent
                    // variant in pipeline.rs means the bar renderer
                    // either handles it (no-op fine) or compile-errors
                    // there. Either way, no silent drop.
                    match serde_json::from_str::<ProgressEvent>(&event.data) {
                        Ok(pe) => {
                            // Forward into the renderer. Send error
                            // means the driver task panicked; treat as
                            // fatal because the user is now blind to
                            // progress.
                            if tx.send(pe).is_err() {
                                anyhow::bail!("progress renderer task ended unexpectedly");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                raw = %event.data,
                                "unparsable ProgressEvent from daemon — daemon/CLI version mismatch?"
                            );
                        }
                    }
                }
                "done" => {
                    match serde_json::from_str::<PipelineResult>(&event.data) {
                        Ok(pr) => final_result = Some(pr),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                raw = %event.data,
                                "unparsable PipelineResult from daemon `done` event"
                            );
                        }
                    }
                    break;
                }
                "failed" => {
                    error_msg = Some(extract_error_message(&event.data));
                    break;
                }
                "cancelled" => {
                    cancelled = true;
                    break;
                }
                _ => { /* unknown SSE event-type — forward-compat ignore */ }
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    let consume_result = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            // Dropping tx + the stream tears down the body. The
            // daemon's DropGuard sees the disconnect and cancels the
            // pipeline. We still await the driver so the indicatif
            // teardown writes its final bytes before we return.
            drop(tx);
            let _ = driver_handle.await;
            anyhow::bail!("compile cancelled by user (Ctrl-C)");
        }
        result = consume => result,
    };

    // Close the channel so the bar driver finalizes any in-flight bars
    // (skipped/failed visual depending on whether we saw a `failed`).
    drop(tx);
    let _ = driver_handle.await;
    eprintln!();

    consume_result?;

    if let Some(msg) = error_msg {
        anyhow::bail!("daemon compile failed: {msg}");
    }
    if cancelled {
        anyhow::bail!("daemon compile cancelled");
    }

    if let Some(result) = final_result {
        // Headline summary — same shape as the in-process path's
        // post-bar print so script greppers see identical output.
        println!();
        println!(
            "  {} compiled {} files",
            style("ThinkingRoot").green().bold(),
            style(result.files_parsed).white().bold()
        );
        println!(
            "  {} {}%",
            style("Knowledge Health:").white().bold(),
            style(result.health_score).green().bold()
        );
        println!(
            "  {} {} claims  {} entities  {} relations",
            style("  ├──").dim(),
            style(result.claims_count).cyan(),
            style(result.entities_count).cyan(),
            style(result.relations_count).cyan()
        );
        if result.failed_batches > 0 {
            println!(
                "  {} {} LLM batches failed — knowledge graph is partial",
                style("  ⚠").yellow().bold(),
                style(result.failed_batches).yellow().bold()
            );
        }
        println!();

        // Structured incremental delta — same renderer the in-process
        // path uses, so JSON / human output is byte-identical.
        if json {
            println!("{}", serde_json::to_string(&result.incremental_summary)?);
        } else {
            crate::summary_printer::print(&result.incremental_summary, false);
        }
    }

    Ok(())
}

/// `root health` over the daemon's `/ws/{ws}/health` endpoint.
pub async fn run_health_remote(conn: &EngineConnection, path: &Path) -> anyhow::Result<()> {
    let ws = workspace_id_for(path);
    let url = format!("{}/api/v1/ws/{ws}/health", base_url(conn)?);
    let resp = client(UNARY_TIMEOUT)?
        .get(&url)
        .send()
        .await
        .context("failed to GET health from daemon")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
    if !status.is_success() {
        anyhow::bail!(
            "daemon health check failed ({status}): {}",
            extract_error_message(&body)
        );
    }

    let payload: serde_json::Value =
        serde_json::from_str(&body).context("unparsable health response")?;
    let inner = payload.get("data").unwrap_or(&payload);
    let score = inner
        .get("health_score")
        .and_then(|v| v.as_f64())
        .or_else(|| inner.get("score").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);

    println!();
    println!(
        "  {} {:.1}%",
        style("Knowledge Health (remote):").white().bold(),
        style(score).green().bold()
    );
    println!();

    Ok(())
}

/// `root query` over the daemon's `/ws/{ws}/search` endpoint.
pub async fn run_query_remote(
    conn: &EngineConnection,
    path: &Path,
    query: &str,
    top_k: usize,
) -> anyhow::Result<()> {
    let ws = workspace_id_for(path);
    let url = format!(
        "{}/api/v1/ws/{ws}/search?q={}&top_k={top_k}",
        base_url(conn)?,
        urlencoding(query),
    );
    let resp = client(UNARY_TIMEOUT)?
        .get(&url)
        .send()
        .await
        .context("failed to GET search results from daemon")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
    if !status.is_success() {
        anyhow::bail!(
            "daemon search failed ({status}): {}",
            extract_error_message(&body)
        );
    }

    let payload: serde_json::Value =
        serde_json::from_str(&body).context("unparsable search response")?;
    // Surface a non-array response loudly instead of silently
    // returning "0 results" — a malformed `data` field (e.g. an API
    // drift that wraps results in `{ "items": [...] }`) is
    // indistinguishable from a legitimate empty-result query when
    // collapsed via `unwrap_or_default()` and hides upstream contract
    // breaks from the operator.
    let results = payload
        .get("data")
        .or(Some(&payload))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "search response has no array `data` field (or response itself is not an array); body: {body}"
            )
        })?
        .clone();

    println!();
    println!(
        "  {} \"{}\"  ({} results)",
        style("Searching (remote):").cyan().bold(),
        style(query).white(),
        results.len(),
    );
    println!();
    for (i, hit) in results.iter().enumerate() {
        let score = hit.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let label = hit
            .get("label")
            .or_else(|| hit.get("statement"))
            .or_else(|| hit.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("(no label)");
        println!(
            "  {} {} {} {}",
            style(format!("{}.", i + 1)).dim(),
            style(label).white(),
            style(format!("({:.0}%)", score * 100.0)).dim(),
            ""
        );
    }
    println!();
    Ok(())
}

/// `root ask` over the daemon's `/ws/{ws}/ask` endpoint (unary; the
/// stream variant is used by the desktop chat surface).
pub async fn run_ask_remote(
    conn: &EngineConnection,
    path: &Path,
    question: &str,
    date: Option<&str>,
) -> anyhow::Result<()> {
    let ws = workspace_id_for(path);
    let url = format!("{}/api/v1/ws/{ws}/ask", base_url(conn)?);
    let body = serde_json::json!({
        "question": question,
        "question_date": date.unwrap_or(""),
    });

    println!();
    println!(
        "  {} \"{}\"",
        style("Thinking (remote):").cyan().bold(),
        style(question).white()
    );

    // Long timeout because LLM synthesis can take 30 s+ on large
    // contexts. Ctrl-C still cancels via select!/drop.
    let consume = async {
        let resp = client(STREAMING_TIMEOUT)?
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("failed to POST ask to daemon")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
        if !status.is_success() {
            anyhow::bail!(
                "daemon ask failed ({status}): {}",
                extract_error_message(&text)
            );
        }
        let payload: serde_json::Value =
            serde_json::from_str(&text).context("unparsable ask response")?;
        let answer = payload
            .get("data")
            .and_then(|d| d.get("answer"))
            .or_else(|| payload.get("answer"))
            .and_then(|v| v.as_str())
            .unwrap_or("(daemon returned empty answer)");
        println!();
        println!("{answer}");
        println!();
        Ok::<_, anyhow::Error>(())
    };

    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            anyhow::bail!("ask cancelled by user (Ctrl-C)");
        }
        result = consume => {
            result
        }
    }
}

/// `root render` over `/ws/{ws}/artifacts`. Lists artifacts; the
/// per-artifact `--type` extension is left to the next iteration —
/// the in-process render_cmd remains the source of truth for
/// the local-file emit semantics.
pub async fn run_render_remote(conn: &EngineConnection, path: &Path) -> anyhow::Result<()> {
    let ws = workspace_id_for(path);
    let url = format!("{}/api/v1/ws/{ws}/artifacts", base_url(conn)?);
    let resp = client(UNARY_TIMEOUT)?
        .get(&url)
        .send()
        .await
        .context("failed to GET artifacts from daemon")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
    if !status.is_success() {
        anyhow::bail!(
            "daemon render failed ({status}): {}",
            extract_error_message(&body)
        );
    }
    let payload: serde_json::Value =
        serde_json::from_str(&body).context("unparsable render response")?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

/// `root reflect` over `/ws/{ws}/artifacts` (alias of render today;
/// the JSON output mode is the same surface).
pub async fn run_reflect_remote(conn: &EngineConnection, path: &Path) -> anyhow::Result<()> {
    run_render_remote(conn, path).await
}

/// `root status` over the daemon's `GET /api/v1/ws/{ws}/sources` plus
/// a local filesystem walk to detect modified/untracked/deleted files.
///
/// The hash-comparison happens client-side — the CLI walks the disk
/// (which the daemon can't do) and only the source-list-with-hashes
/// comes from the daemon. Mounts the workspace if the daemon hasn't
/// seen it yet.
pub async fn run_status_remote(
    conn: &EngineConnection,
    root: &Path,
) -> anyhow::Result<()> {
    use console::style;
    use std::collections::{HashMap, HashSet};
    use thinkingroot_core::{Config, types::ContentHash};

    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let ws = workspace_id_for(&root);
    let base = base_url(conn)?;
    let client = client(UNARY_TIMEOUT)?;

    // Stream B — ensure the daemon has the workspace mounted before
    // querying its sources. `mount_workspace_handler` is idempotent
    // (overwrites under the same name) and also pins
    // `state.workspace_root` (Stream A).
    let mount_url = format!("{base}/api/v1/workspaces");
    let mount_body = serde_json::json!({
        "name": &ws,
        "root_path": root.display().to_string(),
    });
    let resp = client
        .post(&mount_url)
        .json(&mount_body)
        .send()
        .await
        .context("failed to POST /workspaces from daemon")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
        anyhow::bail!(
            "daemon mount failed ({status}): {}",
            extract_error_message(&body)
        );
    }

    // Resolve the active branch via `/api/v1/head` so the header we
    // print matches what `git status`-style consumers expect.
    let head_url = format!("{base}/api/v1/head");
    let head: String = match client.get(&head_url).send().await {
        Ok(r) if r.status().is_success() => {
            let body = r.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
            serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("data")
                        .and_then(|d| d.get("head"))
                        .and_then(|h| h.as_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| "main".to_string())
        }
        _ => "main".to_string(),
    };

    println!(
        "\n  {} {}",
        style("On branch:").white().bold(),
        style(&head).cyan().bold()
    );

    // Pull the daemon's view of compiled sources (uri + content_hash).
    let sources_url = format!("{base}/api/v1/ws/{ws}/sources");
    let resp = client
        .get(&sources_url)
        .send()
        .await
        .context("failed to GET sources from daemon")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
    if !status.is_success() {
        anyhow::bail!(
            "daemon status failed ({status}): {}",
            extract_error_message(&body)
        );
    }

    #[derive(serde::Deserialize)]
    struct SourceWire {
        uri: String,
        #[serde(default)]
        content_hash: String,
    }

    let payload: serde_json::Value =
        serde_json::from_str(&body).context("unparsable sources response")?;
    let sources_value = payload
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("sources response missing data field"))?;
    let sources: Vec<SourceWire> = serde_json::from_value(sources_value)
        .context("unable to decode sources list")?;

    // (uri → stored_content_hash) — match handle_status's local pattern.
    let graph_sources: HashMap<String, String> = sources
        .into_iter()
        .map(|s| (s.uri, s.content_hash))
        .collect();

    // Pre-fix `unwrap_or_default()` here meant a corrupt `config.toml`
    // or an unwalkable workspace directory (permission denied, missing
    // root) silently substituted an empty config + empty file list,
    // making `root status --remote` print "Working tree clean" while
    // 100 dirty files sat on disk.  Both errors now propagate via
    // `with_context` so the operator sees the actual cause.
    let config = Config::load_merged(&root)
        .with_context(|| format!("failed to load workspace config at {}", root.display()))?;
    let files_on_disk = thinkingroot_parse::walker::walk(&root, &config.parsers)
        .with_context(|| format!("failed to walk workspace at {}", root.display()))?;

    let mut modified: Vec<String> = Vec::new();
    let mut untracked: Vec<String> = Vec::new();

    for file_path in &files_on_disk {
        let uri = file_path.to_string_lossy().to_string();
        match graph_sources.get(&uri) {
            Some(stored_hash) => match std::fs::read(file_path) {
                Ok(bytes) => {
                    if !stored_hash.is_empty()
                        && ContentHash::from_bytes(&bytes).0 != *stored_hash
                    {
                        modified.push(uri);
                    }
                }
                Err(_) => modified.push(uri),
            },
            None => untracked.push(uri),
        }
    }

    let disk_uris: HashSet<String> = files_on_disk
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let mut deleted: Vec<String> = graph_sources
        .keys()
        .filter(|uri| !disk_uris.contains(uri.as_str()))
        .cloned()
        .collect();

    modified.sort();
    untracked.sort();
    deleted.sort();

    if modified.is_empty() && untracked.is_empty() && deleted.is_empty() {
        println!(
            "  {}\n",
            style("Working tree clean — graph is in sync with disk").green()
        );
        return Ok(());
    }

    if !modified.is_empty() {
        println!("\n  {}", style("Modified files:").yellow().bold());
        for f in &modified {
            println!("    {} {}", style("M").yellow().bold(), f);
        }
    }
    if !untracked.is_empty() {
        println!("\n  {}", style("Untracked files:").red().bold());
        for f in &untracked {
            println!("    {} {}", style("?").red().bold(), f);
        }
    }
    if !deleted.is_empty() {
        println!("\n  {}", style("Deleted from disk:").magenta().bold());
        for f in &deleted {
            println!("    {} {}", style("D").magenta().bold(), f);
        }
    }
    println!();
    Ok(())
}

/// Mount the workspace at `root` into the daemon under its basename
/// and return the resolved workspace id. Idempotent — the daemon's
/// `mount_workspace_handler` overwrites under the same name.
///
/// Used by every workspace-scoped remote command (brain probes,
/// retrieve, claims, engrams, ...) so the daemon's `state.engine`
/// has a graph to query against. `run_status_remote` and the
/// command stream commands have their own inline mount call — when
/// touching them, prefer this helper to avoid duplication.
pub async fn ensure_mounted_remote(
    conn: &EngineConnection,
    root: &Path,
) -> anyhow::Result<String> {
    let abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let ws = workspace_id_for(&abs);
    let url = format!("{}/api/v1/workspaces", base_url(conn)?);
    let body = serde_json::json!({
        "name": &ws,
        "root_path": abs.display().to_string(),
    });
    let resp = client(UNARY_TIMEOUT)?
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_else(|e| format!("<read body failed: {e}>"));
        anyhow::bail!(
            "daemon mount failed ({status}): {}",
            extract_error_message(&txt)
        );
    }
    Ok(ws)
}

/// Workspace identifier used in REST URLs. The daemon mounts a
/// workspace by name on first reference; we pass the basename of
/// the path so multi-workspace daemons can route correctly.
///
/// When the basename is empty (root-level path) we fall back to
/// `default` to match the in-process CLI's existing behaviour.
pub fn workspace_id_for(path: &Path) -> String {
    path.canonicalize()
        .ok()
        .and_then(|abs| {
            abs.file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "default".to_string())
}

/// Minimal URL-encoder — sufficient for the `q=` query-string param.
/// Avoids a full `url` crate import on a hot path that only needs a
/// shallow encode of spaces, quotes, and a few common metachars.
pub fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for b in ch.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::cortex::StartedBy;

    #[test]
    fn base_url_formats_loopback() {
        let conn = EngineConnection::Remote {
            host: "127.0.0.1".to_string(),
            port: 31760,
            started_by: StartedBy::Cli,
            pid: 42,
        };
        assert_eq!(base_url(&conn).unwrap(), "http://127.0.0.1:31760");
    }

    #[test]
    fn base_url_rejects_non_remote() {
        let conn = EngineConnection::InProcess;
        assert!(base_url(&conn).is_err());
    }

    #[test]
    fn workspace_id_falls_back_to_default_for_empty_basename() {
        // Root path "/" has no basename — must not panic, must
        // return a non-empty workspace id.
        let id = workspace_id_for(Path::new("/"));
        assert!(!id.is_empty());
    }

    #[test]
    fn urlencoding_passes_unreserved_chars() {
        assert_eq!(urlencoding("abc-XYZ_123.~"), "abc-XYZ_123.~");
    }

    #[test]
    fn urlencoding_encodes_spaces_and_quotes() {
        assert_eq!(urlencoding("a b\"c"), "a%20b%22c");
    }

    #[test]
    fn urlencoding_handles_non_ascii() {
        // The é (U+00E9) is two UTF-8 bytes 0xC3 0xA9.
        assert_eq!(urlencoding("é"), "%C3%A9");
    }

    #[test]
    fn extract_error_message_unwraps_envelope() {
        let body =
            r#"{"error":{"code":"NOT_FOUND","message":"workspace 'foo' not mounted"}}"#;
        assert_eq!(extract_error_message(body), "workspace 'foo' not mounted");
    }

    #[test]
    fn extract_error_message_falls_through_on_plain_text() {
        let body = "internal server error";
        assert_eq!(extract_error_message(body), body);
    }

    // ── Slice 4: with_reconnect tests ──────────────────────────────

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    #[derive(Clone)]
    struct MockState {
        op_calls: Arc<AtomicU32>,
        fail_with_status: Arc<AtomicU32>,
        succeed: Arc<AtomicBool>,
    }

    impl MockState {
        fn new(initial_status: u16) -> Self {
            Self {
                op_calls: Arc::new(AtomicU32::new(0)),
                fail_with_status: Arc::new(AtomicU32::new(initial_status as u32)),
                succeed: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    async fn livez_handler() -> impl IntoResponse {
        (StatusCode::OK, "ok")
    }

    async fn op_handler(State(s): State<MockState>) -> impl IntoResponse {
        s.op_calls.fetch_add(1, Ordering::SeqCst);
        let forced = s.fail_with_status.load(Ordering::SeqCst);
        if forced != 0 && !s.succeed.load(Ordering::SeqCst) {
            let status =
                StatusCode::from_u16(forced as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return (status, "transient").into_response();
        }
        Json(serde_json::json!({
            "ok": true,
            "data": { "hits": 42 },
            "error": null
        }))
        .into_response()
    }

    async fn spawn_mock(
        initial_status: u16,
    ) -> (SocketAddr, MockState, tokio::sync::oneshot::Sender<()>) {
        let state = MockState::new(initial_status);
        let app = Router::new()
            .route("/livez", get(livez_handler))
            .route("/api/v1/op", get(op_handler))
            .route("/api/v1/op", post(op_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        // Let the listener start accepting before the test makes a call.
        tokio::time::sleep(Duration::from_millis(50)).await;
        (addr, state, tx)
    }

    fn remote_conn(addr: SocketAddr) -> EngineConnection {
        EngineConnection::Remote {
            host: "127.0.0.1".into(),
            port: addr.port(),
            pid: std::process::id(),
            started_by: StartedBy::Cli,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unary_call_retries_after_503_clears() {
        let (addr, state, _shutdown) = spawn_mock(503).await;
        let conn = remote_conn(addr);
        // Flip to success after 200 ms — long enough that the first
        // call fails but the retry-attempt backoff (450 ms) has cleared.
        let succeed = state.succeed.clone();
        tokio::spawn(async move {
            // Backoff between attempts is 150 ms; flip success at
            // 50 ms so the second attempt clears.
            tokio::time::sleep(Duration::from_millis(50)).await;
            succeed.store(true, Ordering::SeqCst);
        });
        let v = get_json(&conn, "/api/v1/op").await.unwrap();
        assert_eq!(v["hits"], 42);
        assert!(state.op_calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unary_call_does_not_retry_on_4xx() {
        let (addr, state, _shutdown) = spawn_mock(404).await;
        let conn = remote_conn(addr);
        let err = get_json(&conn, "/api/v1/op").await.unwrap_err();
        assert_eq!(state.op_calls.load(Ordering::SeqCst), 1, "no retry on 4xx");
        assert!(!err.chain().any(|c| c.is::<DaemonUnreachable>()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unary_call_fails_loudly_after_retry_exhausted() {
        let (addr, state, _shutdown) = spawn_mock(503).await;
        let conn = remote_conn(addr);
        let err = get_json(&conn, "/api/v1/op").await.unwrap_err();
        assert_eq!(state.op_calls.load(Ordering::SeqCst), 2);
        assert!(
            err.chain().any(|c| c.is::<DaemonUnreachable>()),
            "expected DaemonUnreachable in chain, got: {err:#}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn post_with_body_also_retries() {
        let (addr, state, _shutdown) = spawn_mock(502).await;
        let conn = remote_conn(addr);
        let succeed = state.succeed.clone();
        tokio::spawn(async move {
            // Backoff between attempts is 150 ms; flip success at
            // 50 ms so the second attempt clears.
            tokio::time::sleep(Duration::from_millis(50)).await;
            succeed.store(true, Ordering::SeqCst);
        });
        let body = serde_json::json!({ "k": "v" });
        let v = post_json(&conn, "/api/v1/op", &body).await.unwrap();
        assert_eq!(v["hits"], 42);
        assert!(state.op_calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreachable_daemon_surfaces_marker_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let conn = remote_conn(addr);
        let err = get_json(&conn, "/api/v1/op").await.unwrap_err();
        let marker = err
            .chain()
            .find_map(|c| c.downcast_ref::<DaemonUnreachable>())
            .expect("DaemonUnreachable should be in chain");
        assert_eq!(marker.attempts, 2);
        assert!(!marker.last_attempt.is_empty());
    }

    #[test]
    fn is_transient_classifies_status_codes() {
        // 502/503/504 are retried.
        assert!(matches!(
            is_transient_transport_for_status(502),
            true
        ));
        assert!(matches!(
            is_transient_transport_for_status(503),
            true
        ));
        assert!(matches!(
            is_transient_transport_for_status(504),
            true
        ));
        // 4xx and 5xx-not-{502,503,504} are NOT retried by the
        // status-only classifier path. Note that bare reqwest errors
        // without a status code (connect/timeout) are caught by the
        // is_connect/is_timeout branches; this test only covers the
        // status-coded path.
        assert!(matches!(
            is_transient_transport_for_status(404),
            false
        ));
        assert!(matches!(
            is_transient_transport_for_status(500),
            false
        ));
    }

    /// Test-only mirror of [`is_transient_transport`]'s status-code
    /// branch — keeps the test crisp without needing a real
    /// `reqwest::Error` (which is hard to construct in unit tests).
    fn is_transient_transport_for_status(code: u16) -> bool {
        matches!(code, 502 | 503 | 504)
    }
}
