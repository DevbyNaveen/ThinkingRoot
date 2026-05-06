//! `root engram {materialize, list, probe, expire}` — CLI bindings for
//! the Active Engram Protocol (AEP) lifecycle.
//!
//! AEP routes require an `X-TR-Session-Id` header; the CLI mints
//! `cli-<random>` per invocation, or threads through `--session <id>`
//! when the user wants the same session id across multiple commands
//! (typical pattern: `materialize` + repeated `probe` + final `expire`).
//!
//! Pointer format is `0xXXXX` (16-bit hex), matching `EngramPointer`
//! at the engine layer.

use std::path::Path;

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

/// Mint a fresh session id when the user did not pass `--session`.
/// Format mirrors the Python/TS SDKs (`py-<hex>` / `ts-<hex>`) so log
/// lines from a multi-surface debug session are visually distinct.
fn mint_session() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let raw = format!("cli|{pid}|{nanos}");
    let h = blake3::hash(raw.as_bytes()).to_hex();
    format!("cli-{}", &h.as_str()[..16])
}

fn resolve_session(arg: Option<String>) -> String {
    arg.filter(|s| !s.trim().is_empty())
        .unwrap_or_else(mint_session)
}

/// `root engram materialize <topic> [--seed <id>]... [--scope <s>] [--session <id>]`
pub async fn run_materialize(
    conn: &EngineConnection,
    root: &Path,
    topic: &str,
    seed_entity_ids: Vec<String>,
    scope: Option<String>,
    session: Option<String>,
) -> anyhow::Result<()> {
    let ws = cortex_remote::ensure_mounted_remote(conn, root).await?;
    let session = resolve_session(session);

    let mut body = serde_json::json!({
        "topic": topic,
    });
    if !seed_entity_ids.is_empty() {
        body["seed_entity_ids"] = serde_json::json!(seed_entity_ids);
    }
    if let Some(s) = scope {
        body["scope"] = serde_json::json!(s);
    }

    let path = format!("/api/v1/ws/{ws}/engrams");
    let data = cortex_remote::post_json_with_session(conn, &path, &session, &body)
        .await
        .with_context(|| format!("materialize engram for topic '{topic}'"))?;
    let pointer = data
        .get("pointer")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    println!(
        "  {} engram {} (session {})",
        style("✓").green().bold(),
        style(pointer).cyan().bold(),
        style(&session).dim()
    );
    println!();
    println!("{}", serde_json::to_string_pretty(&data.get("summary").cloned().unwrap_or(serde_json::Value::Null))?);
    println!();
    Ok(())
}

/// `root engram list [--session <id>]`
pub async fn run_list(
    conn: &EngineConnection,
    root: &Path,
    session: Option<String>,
) -> anyhow::Result<()> {
    let ws = cortex_remote::ensure_mounted_remote(conn, root).await?;
    let session = resolve_session(session);
    let path = format!("/api/v1/ws/{ws}/engrams");
    let data = cortex_remote::get_json_with_session(conn, &path, &session).await?;
    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}

/// `root engram probe <pointer> <question> [--clearance ...] [--probe-kind <k>]
/// [--score-with-hybrid] [--session <id>]`
#[allow(clippy::too_many_arguments)]
pub async fn run_probe(
    conn: &EngineConnection,
    root: &Path,
    pointer: &str,
    question: &str,
    clearance: Vec<String>,
    probe_kind: Option<String>,
    score_with_hybrid: bool,
    session: Option<String>,
) -> anyhow::Result<()> {
    let ws = cortex_remote::ensure_mounted_remote(conn, root).await?;
    let session = resolve_session(session);
    let mut body = serde_json::json!({ "question": question });
    if !clearance.is_empty() {
        body["clearance"] = serde_json::json!(clearance);
    }
    if let Some(k) = probe_kind {
        body["probe_kind"] = serde_json::json!(k);
    }
    if score_with_hybrid {
        body["score_with_hybrid"] = serde_json::json!(true);
    }
    let path = format!("/api/v1/ws/{ws}/engrams/{pointer}/probe");
    let data = cortex_remote::post_json_with_session(conn, &path, &session, &body)
        .await
        .with_context(|| format!("probe engram {pointer}"))?;
    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}

/// `root engram expire <pointer> [--session <id>]`
pub async fn run_expire(
    conn: &EngineConnection,
    root: &Path,
    pointer: &str,
    session: Option<String>,
) -> anyhow::Result<()> {
    let ws = cortex_remote::ensure_mounted_remote(conn, root).await?;
    let session = resolve_session(session);
    let path = format!("/api/v1/ws/{ws}/engrams/{pointer}");
    let data = cortex_remote::delete_json_with_session(conn, &path, &session).await?;
    let expired = data.get("expired").and_then(|v| v.as_bool()).unwrap_or(false);
    if expired {
        println!(
            "  {} engram {} expired",
            style("✓").green().bold(),
            style(pointer).cyan().bold()
        );
    } else {
        println!(
            "  {} engram {} not found",
            style("·").yellow().bold(),
            style(pointer).cyan().bold()
        );
    }
    Ok(())
}
