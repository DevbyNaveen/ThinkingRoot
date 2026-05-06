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

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use console::style;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use thinkingroot_core::cortex::EngineConnection;

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
pub async fn get_json(
    conn: &EngineConnection,
    path: &str,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let resp = client(UNARY_TIMEOUT)?
        .get(&url)
        .send()
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
    let resp = client(UNARY_TIMEOUT)?
        .post(&url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    decode_envelope(resp).await
}

/// DELETE a daemon endpoint. Returns the `data` field value.
pub async fn delete_json(
    conn: &EngineConnection,
    path: &str,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}{path}", base_url(conn)?);
    let resp = client(UNARY_TIMEOUT)?
        .delete(&url)
        .send()
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
    let resp = client(UNARY_TIMEOUT)?
        .post(&url)
        .header("X-TR-Session-Id", session_id)
        .json(body)
        .send()
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
    let resp = client(UNARY_TIMEOUT)?
        .get(&url)
        .header("X-TR-Session-Id", session_id)
        .send()
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
    let resp = client(UNARY_TIMEOUT)?
        .delete(&url)
        .header("X-TR-Session-Id", session_id)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    decode_envelope(resp).await
}

async fn decode_envelope(resp: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
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
/// Progress events are streamed back as JSON-encoded
/// `ProgressEvent`s; the CLI prints a one-line summary per phase
/// (matching the in-process `progress::run_compile_progress` UX
/// closely enough that scripts grepping the output see no
/// difference).
pub async fn run_compile_remote(
    conn: &EngineConnection,
    path: &Path,
    branch: Option<&str>,
    no_rooting: bool,
    json: bool,
) -> anyhow::Result<()> {
    let url = format!("{}/api/v1/ws/_/compile/stream", base_url(conn)?);
    let body = serde_json::json!({
        "root_path": path.display().to_string(),
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
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "daemon rejected compile request: {} — {}",
            status,
            extract_error_message(&body)
        );
    }

    let mut stream = resp.bytes_stream().eventsource();
    let mut last_phase = String::new();
    let mut final_summary: Option<serde_json::Value> = None;
    let mut captured_summary: Option<thinkingroot_core::IncrementalSummary> = None;

    let consume = async {
        while let Some(event) = stream.next().await {
            let event = event.context("SSE stream error")?;
            // Each `data:` line in the SSE wire format carries one
            // serialised ProgressEvent; the wire shape is
            // tag-on-`kind`, snake_case, per the v3 invariants in
            // CLAUDE.md.
            let payload: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, raw = %event.data, "unparsable progress event");
                    continue;
                }
            };
            let kind = payload
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            match kind {
                "phase_started" => {
                    let phase = payload
                        .get("phase")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    if phase != last_phase {
                        println!("  {} {}", style("→").cyan(), style(&phase).white().bold());
                        last_phase = phase;
                    }
                }
                "incremental_done" => {
                    if let Some(summary_value) = payload.get("summary") {
                        match serde_json::from_value::<thinkingroot_core::IncrementalSummary>(
                            summary_value.clone(),
                        ) {
                            Ok(summary) => captured_summary = Some(summary),
                            Err(e) => tracing::warn!(
                                error = %e,
                                raw = %summary_value,
                                "failed to deserialize IncrementalSummary from incremental_done event — daemon/CLI version mismatch?"
                            ),
                        }
                    }
                }
                "completed" | "result" | "done" => {
                    final_summary = Some(payload);
                }
                "error" | "failed" => {
                    let msg = payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("daemon emitted error event with no message");
                    anyhow::bail!("daemon compile failed: {msg}");
                }
                "cancelled" => {
                    anyhow::bail!("daemon compile cancelled");
                }
                _ => { /* unknown — preserve forward-compat by ignoring */ }
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            // Drop the request future so the body stream tears
            // down. The daemon's DropGuard sees the disconnect and
            // cancels the pipeline.
            anyhow::bail!("compile cancelled by user (Ctrl-C)");
        }
        result = consume => {
            result?;
        }
    }

    if let Some(summary) = final_summary {
        let files = summary.get("files_parsed").and_then(|v| v.as_u64()).unwrap_or(0);
        let claims = summary.get("claims_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let entities = summary.get("entities_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let relations = summary.get("relations_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let health = summary.get("health_score").and_then(|v| v.as_u64()).unwrap_or(0);
        println!();
        println!(
            "  {} compiled {} files",
            style("ThinkingRoot").green().bold(),
            style(files).white().bold()
        );
        println!(
            "  {} {}%",
            style("Knowledge Health:").white().bold(),
            style(health).green().bold()
        );
        println!(
            "  {} {} claims  {} entities  {} relations",
            style("  ├──").dim(),
            style(claims).cyan(),
            style(entities).cyan(),
            style(relations).cyan()
        );
        println!();
    }

    if let Some(summary) = captured_summary {
        if json {
            println!("{}", serde_json::to_string(&summary)?);
        } else {
            crate::summary_printer::print(&summary, false);
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
    let body = resp.text().await.unwrap_or_default();
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
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "daemon search failed ({status}): {}",
            extract_error_message(&body)
        );
    }

    let payload: serde_json::Value =
        serde_json::from_str(&body).context("unparsable search response")?;
    let results = payload
        .get("data")
        .or(Some(&payload))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

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
        let text = resp.text().await.unwrap_or_default();
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
    let body = resp.text().await.unwrap_or_default();
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
        let body = resp.text().await.unwrap_or_default();
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
            let body = r.text().await.unwrap_or_default();
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
    let body = resp.text().await.unwrap_or_default();
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

    let config = Config::load_merged(&root).unwrap_or_default();
    let files_on_disk =
        thinkingroot_parse::walker::walk(&root, &config.parsers).unwrap_or_default();

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
        let txt = resp.text().await.unwrap_or_default();
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
}
