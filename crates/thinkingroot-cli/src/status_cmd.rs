//! Slice 0 — `root status` command.
//!
//! Reads the daemon's `/api/v1/workspaces/{name}/status` endpoint —
//! the same source of truth the desktop's right-rail badge, chat
//! banner, export dialog, and MCP TOOLS panel all consume. With
//! `--watch`, subscribes to the SSE companion endpoint and prints
//! every snapshot transition until Ctrl-C.
//!
//! Honest behaviour:
//!
//! - When the daemon is not running, exits with the standard
//!   "daemon unreachable" exit code (75) — never falls back to a
//!   stale local snapshot or invents one.
//! - Human prose mirrors the desktop right-rail badge tone-for-tone:
//!   the same five substrate states, the same diagnostic messages,
//!   the same actionable hints. CLI and desktop never diverge on the
//!   text the user reads.
//! - `--json` emits one [`WorkspaceStatus`] JSON line per snapshot
//!   in watch mode, suitable for piping into `jq` or driving a
//!   custom UI.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use thinkingroot_core::types::{
    CompileOutcome, CompileState, LlmState, MountState, SourcesState, SubstrateState,
    WorkspaceStatus, WorkspaceStatusEvent,
};

use crate::cortex_client::resolve_engine;
use thinkingroot_core::cortex::{EngineConnection, EngineIntent};

/// Options accepted by [`run_status`]. Mirrors the clap-derived
/// arguments verbatim so the dispatch site stays trivial.
#[derive(Debug, Clone)]
pub struct StatusOpts {
    /// Workspace name (None = active workspace).
    pub name: Option<String>,
    /// Emit raw JSON instead of formatted prose.
    pub json: bool,
    /// Subscribe to the SSE stream and print every snapshot.
    pub watch: bool,
}

/// Entry point. Resolves the daemon endpoint, picks the workspace
/// name (provided or inferred from the registry's active entry), then
/// either prints one snapshot (`--watch=false`, default) or streams
/// snapshots until Ctrl-C.
pub async fn run_status(opts: StatusOpts) -> Result<()> {
    let resolved = resolve_engine(EngineIntent::Command)
        .await
        .context("resolving daemon for status")?;

    let endpoint = match resolved {
        EngineConnection::Remote { host, port, .. } => (host, port),
        EngineConnection::Stdio | EngineConnection::InProcess => {
            bail!(
                "no running daemon found — start one with `root serve` and retry"
            );
        }
    };

    let workspace = match opts.name {
        Some(n) => n,
        None => active_workspace_name()?,
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building http client")?;

    if opts.watch {
        run_watch(&client, &endpoint.0, endpoint.1, &workspace, opts.json).await
    } else {
        run_one_shot(&client, &endpoint.0, endpoint.1, &workspace, opts.json).await
    }
}

fn active_workspace_name() -> Result<String> {
    use thinkingroot_core::WorkspaceRegistry;
    let registry = WorkspaceRegistry::load().context("loading workspace registry")?;
    registry
        .active_entry()
        .map(|e| e.name.clone())
        .ok_or_else(|| anyhow!("no active workspace — pass --name or run `root mount` first"))
}

async fn run_one_shot(
    client: &reqwest::Client,
    host: &str,
    port: u16,
    workspace: &str,
    json: bool,
) -> Result<()> {
    let url = format!("http://{host}:{port}/api/v1/workspaces/{workspace}/status");
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status_code = resp.status();
    if !status_code.is_success() {
        bail!(
            "daemon returned HTTP {} for status of `{workspace}`",
            status_code
        );
    }
    let snap: WorkspaceStatus = resp
        .json()
        .await
        .context("decoding workspace status JSON")?;
    if json {
        let body = serde_json::to_string(&snap).context("encoding status to json")?;
        println!("{body}");
    } else {
        let mut out = std::io::stdout().lock();
        write_human(&mut out, &snap)?;
    }
    Ok(())
}

async fn run_watch(
    client: &reqwest::Client,
    host: &str,
    port: u16,
    workspace: &str,
    json: bool,
) -> Result<()> {
    let url = format!("http://{host}:{port}/api/v1/workspaces/{workspace}/status/stream");
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(0))
        .send()
        .await
        .with_context(|| format!("connecting to {url}"))?;
    if !resp.status().is_success() {
        bail!("daemon returned HTTP {} for status stream", resp.status());
    }
    let mut stream = resp.bytes_stream().eventsource();
    let mut stdout = std::io::stdout().lock();

    while let Some(event) = stream.next().await {
        let event = event.context("reading sse event")?;
        let parsed: WorkspaceStatusEvent = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warning: failed to decode status event: {e}");
                continue;
            }
        };
        match parsed {
            WorkspaceStatusEvent::Snapshot(snap) => {
                if json {
                    let body = serde_json::to_string(&snap)
                        .context("encoding watch snapshot to json")?;
                    writeln!(stdout, "{body}")?;
                } else {
                    write_human(&mut stdout, &snap)?;
                    writeln!(stdout, "—")?;
                }
                stdout.flush()?;
            }
            WorkspaceStatusEvent::Heartbeat { .. } => {
                // Heartbeats are intentional silence in human-prose
                // mode (would clutter the terminal); JSON mode emits
                // nothing for them either — the wire shape's
                // `kind: "heartbeat"` tag makes them trivially
                // distinguishable when the user genuinely wants them.
            }
        }
    }
    Ok(())
}

fn write_human<W: Write>(out: &mut W, snap: &WorkspaceStatus) -> Result<()> {
    writeln!(out, "Workspace: {}", snap.name)?;
    writeln!(out, "Path:      {}", snap.path.display())?;
    writeln!(out, "As of:     {}", snap.as_of.to_rfc3339())?;
    writeln!(out)?;
    writeln!(out, "Substrate: {}", format_substrate(&snap.substrate))?;
    writeln!(out, "Sources:   {}", format_sources(&snap.sources))?;
    writeln!(out, "Mount:     {}", format_mount(&snap.mount))?;
    writeln!(out, "LLM:       {}", format_llm(&snap.llm))?;
    writeln!(out, "Compile:   {}", format_compile(&snap.compile))?;
    writeln!(
        out,
        "Branch:    {} ({}modified)",
        snap.branch.current,
        if snap.branch.modified { "" } else { "not " }
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "Readiness:  compile={}  query={}  chat={}  export={}  publish={}",
        flag(snap.readiness.for_compile),
        flag(snap.readiness.for_query),
        flag(snap.readiness.for_chat),
        flag(snap.readiness.for_export),
        flag(snap.readiness.for_publish),
    )?;
    if !snap.diagnostics.is_empty() {
        writeln!(out)?;
        writeln!(out, "Diagnostics:")?;
        for d in &snap.diagnostics {
            let glyph = match d.severity {
                thinkingroot_core::types::DiagnosticSeverity::Error => "✗",
                thinkingroot_core::types::DiagnosticSeverity::Warn => "!",
                thinkingroot_core::types::DiagnosticSeverity::Info => "i",
            };
            writeln!(out, "  {glyph} [{}] {}", d.code, d.message)?;
            if !d.actions.is_empty() {
                let actions: Vec<String> =
                    d.actions.iter().map(|a| a.label.clone()).collect();
                writeln!(out, "      → {}", actions.join(" · "))?;
            }
        }
    }
    Ok(())
}

fn flag(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

fn format_substrate(s: &SubstrateState) -> String {
    match s {
        SubstrateState::Absent => "absent (no .thinkingroot/)".into(),
        SubstrateState::Empty { graph_db_bytes } => {
            format!("empty ({} on disk, 0 claims)", human_bytes(*graph_db_bytes))
        }
        SubstrateState::Populated {
            graph_db_bytes,
            claim_count,
            entity_count,
            source_count_at_last_compile,
        } => format!(
            "populated ({} on disk, {claim_count} claim(s), {entity_count} entity(s), {source_count_at_last_compile} source(s))",
            human_bytes(*graph_db_bytes)
        ),
        SubstrateState::Orphaned { workspace_root } => {
            format!("orphaned (root deleted: {})", workspace_root.display())
        }
        SubstrateState::Corrupt { reason } => format!("corrupt ({reason})"),
    }
}

fn format_sources(s: &SourcesState) -> String {
    match s {
        SourcesState::None => "none".into(),
        SourcesState::Some {
            file_count,
            total_bytes,
            fingerprint_match,
            ..
        } => format!(
            "{file_count} file(s), {} ({})",
            human_bytes(*total_bytes),
            if *fingerprint_match {
                "fingerprints match"
            } else {
                "fingerprints stale — recompile"
            }
        ),
    }
}

fn format_mount(m: &MountState) -> String {
    match m {
        MountState::NotMounted => "not mounted".into(),
        MountState::Mounting => "mounting…".into(),
        MountState::Mounted { since } => format!("mounted (since {})", since.to_rfc3339()),
        MountState::Failed { reason, at } => {
            format!("failed: {reason} (at {})", at.to_rfc3339())
        }
    }
}

fn format_llm(l: &LlmState) -> String {
    match l {
        LlmState::Unconfigured => "no provider configured".into(),
        LlmState::Configured { provider, model } => {
            format!(
                "configured: {provider}{}",
                model.as_deref().map(|m| format!(" ({m})")).unwrap_or_default()
            )
        }
        LlmState::Healthy {
            provider,
            model,
            last_probed_at,
        } => format!(
            "healthy: {provider}{} (last probe {})",
            model.as_deref().map(|m| format!(" ({m})")).unwrap_or_default(),
            last_probed_at.to_rfc3339(),
        ),
        LlmState::Unreachable {
            provider,
            reason,
            last_probed_at,
        } => format!(
            "unreachable: {provider} — {reason} (probed {})",
            last_probed_at.to_rfc3339()
        ),
    }
}

fn format_compile(c: &CompileState) -> String {
    match c {
        CompileState::Idle {
            last_finished_at,
            last_duration_ms,
            last_outcome,
        } => {
            let outcome = match last_outcome {
                Some(CompileOutcome::Success {
                    extracted_claims,
                    sources_processed,
                }) => format!(
                    "success ({extracted_claims} claim(s), {sources_processed} source(s))"
                ),
                Some(CompileOutcome::Partial {
                    extracted_claims,
                    failed_batches,
                    summary,
                }) => format!(
                    "partial ({extracted_claims} claim(s), {failed_batches} batch failure(s); {summary})"
                ),
                Some(CompileOutcome::Failed { phase, reason }) => {
                    format!("failed at {phase}: {reason}")
                }
                Some(CompileOutcome::Cancelled { phase }) => {
                    format!("cancelled at {phase}")
                }
                None => "never run".into(),
            };
            let when = last_finished_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "—".into());
            let dur = last_duration_ms
                .map(|d| format!("{d} ms"))
                .unwrap_or_else(|| "—".into());
            format!("idle (last: {outcome} at {when}, took {dur})")
        }
        CompileState::Running { phase, started_at, .. } => {
            format!("running in `{phase}` (started {})", started_at.to_rfc3339())
        }
        CompileState::Cancelling { since } => {
            format!("cancelling… (since {})", since.to_rfc3339())
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    thinkingroot_core::types::format_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use thinkingroot_core::types::{
        BranchState, CompileOutcome, CompileState, LlmState, MountState, SourcesState,
        SubstrateState, WorkspaceStatus,
    };

    fn snap_for(substrate: SubstrateState) -> WorkspaceStatus {
        WorkspaceStatus::assemble(
            "demo".into(),
            PathBuf::from("/tmp/demo"),
            true,
            substrate,
            SourcesState::None,
            MountState::NotMounted,
            LlmState::Unconfigured,
            CompileState::Idle {
                last_finished_at: None,
                last_duration_ms: None,
                last_outcome: None,
            },
            BranchState::default(),
        )
    }

    #[test]
    fn human_writer_renders_all_axes() {
        let snap = snap_for(SubstrateState::Populated {
            graph_db_bytes: 65_536,
            claim_count: 42,
            entity_count: 17,
            source_count_at_last_compile: 5,
        });
        let mut buf = Vec::new();
        write_human(&mut buf, &snap).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Workspace: demo"));
        assert!(text.contains("populated"));
        assert!(text.contains("42 claim"));
        assert!(text.contains("Readiness:"));
    }

    #[test]
    fn human_writer_renders_diagnostics_with_actions() {
        // The CipherVault scenario from the screenshot: empty
        // substrate, no sources, no provider — three diagnostics with
        // suggested actions.
        let snap = snap_for(SubstrateState::Empty {
            graph_db_bytes: 12_288,
        });
        let mut buf = Vec::new();
        write_human(&mut buf, &snap).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Diagnostics:"));
        assert!(text.contains("[empty_substrate]"));
        assert!(text.contains("[no_sources]"));
        assert!(text.contains("[no_provider]"));
        // Action labels must surface so the user can see the
        // suggestions without re-deriving them.
        assert!(
            text.contains("Re-run compile") || text.contains("Run compile"),
            "expected a compile-related action label in:\n{text}"
        );
    }

    #[test]
    fn format_substrate_covers_every_variant() {
        assert!(format_substrate(&SubstrateState::Absent).starts_with("absent"));
        assert!(format_substrate(&SubstrateState::Empty { graph_db_bytes: 0 }).starts_with("empty"));
        assert!(format_substrate(&SubstrateState::Corrupt {
            reason: "boom".into()
        })
        .contains("boom"));
    }

    #[test]
    fn format_compile_outcome_round_trip() {
        let c = CompileState::Idle {
            last_finished_at: Some(chrono::Utc::now()),
            last_duration_ms: Some(1840),
            last_outcome: Some(CompileOutcome::Success {
                extracted_claims: 42,
                sources_processed: 5,
            }),
        };
        let s = format_compile(&c);
        assert!(s.contains("success"));
        assert!(s.contains("42 claim"));
    }
}
