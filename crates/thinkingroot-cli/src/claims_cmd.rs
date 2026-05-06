//! `root claims` — query the compiled claim graph from the CLI.
//!
//! Modes:
//!   - `root claims [--type T] [--entity E] [--min-confidence F] [--limit N] [--offset N]`
//!     → GET `/api/v1/ws/{ws}/claims?...`
//!   - `root claims --as-of <ISO-8601>` (T2.4 bitemporal as-of)
//!     → GET `/api/v1/ws/{ws}/claims/as-of?as_of=...&branch=...`
//!   - `root claims --rooted` lists trust-rooted claims via the dedicated route.
//!
//! `--branch` selects a non-main branch when provided.

use std::path::Path;

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

/// Aggregated entry-point — main dispatches after parsing flags.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    conn: &EngineConnection,
    root: &Path,
    as_of: Option<&str>,
    rooted: bool,
    branch: Option<&str>,
    claim_type: Option<&str>,
    entity: Option<&str>,
    min_confidence: Option<f64>,
    limit: Option<u32>,
    offset: Option<u32>,
    json: bool,
) -> anyhow::Result<()> {
    let ws = cortex_remote::ensure_mounted_remote(conn, root).await?;

    let path = if let Some(ts) = as_of {
        let mut p = format!(
            "/api/v1/ws/{ws}/claims/as-of?as_of={}",
            cortex_remote::urlencoding(ts)
        );
        if let Some(b) = branch {
            p.push_str(&format!("&branch={}", cortex_remote::urlencoding(b)));
        }
        p
    } else if rooted {
        format!("/api/v1/ws/{ws}/claims/rooted")
    } else {
        let mut p = format!("/api/v1/ws/{ws}/claims");
        let mut sep = '?';
        let mut push = |p: &mut String, k: &str, v: String| {
            p.push(sep);
            p.push_str(&format!("{k}={v}"));
            sep = '&';
        };
        // We can't capture a mutable `sep` and a closure that mutates it
        // in the same scope cleanly, so build the query string inline.
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(t) = claim_type {
            q.push(("type".into(), cortex_remote::urlencoding(t)));
        }
        if let Some(e) = entity {
            q.push(("entity".into(), cortex_remote::urlencoding(e)));
        }
        if let Some(c) = min_confidence {
            q.push(("min_confidence".into(), c.to_string()));
        }
        if let Some(l) = limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(o) = offset {
            q.push(("offset".into(), o.to_string()));
        }
        for (k, v) in q {
            push(&mut p, &k, v);
        }
        let _ = sep;
        p
    };

    let data = cortex_remote::get_json(conn, &path)
        .await
        .context("list claims")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    // Both `claims/as-of` (object with `claims`) and base `claims`
    // (array) are supported — normalize.
    let claims_arr = data
        .get("claims")
        .and_then(|v| v.as_array())
        .cloned()
        .or_else(|| data.as_array().cloned())
        .unwrap_or_default();

    println!();
    println!(
        "  {} {} ({} claim{})",
        style("Workspace").white().bold(),
        style(&ws).cyan().bold(),
        claims_arr.len(),
        if claims_arr.len() == 1 { "" } else { "s" }
    );
    if let Some(ts) = as_of {
        println!("  {} {}", style("As-of").white().bold(), style(ts).yellow());
    }
    if let Some(b) = branch {
        println!("  {} {}", style("Branch").white().bold(), style(b).cyan());
    }
    if claims_arr.is_empty() {
        println!("\n  {}", style("No claims matched.").dim());
        return Ok(());
    }
    println!();
    for c in claims_arr.iter().take(50) {
        let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let statement = c.get("statement").and_then(|v| v.as_str()).unwrap_or("?");
        let conf = c.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let kind = c.get("claim_type").and_then(|v| v.as_str()).unwrap_or("?");
        println!(
            "  {} {} {}  {}",
            style("•").cyan(),
            style(format!("[{kind}]")).yellow(),
            style(format!("conf={conf:.2}")).dim(),
            style(id).dim()
        );
        println!("    {}", style(statement).white());
    }
    if claims_arr.len() > 50 {
        println!(
            "\n  {} (+{} more — pass --json for full list)",
            style("…").dim(),
            claims_arr.len() - 50
        );
    }
    println!();
    Ok(())
}
