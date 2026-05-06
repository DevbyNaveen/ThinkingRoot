//! `root retrieve` — hybrid retrieval CLI parity.
//!
//! Routes through `POST /api/v1/ws/{ws}/search/hybrid`. The daemon's
//! `engine.hybrid_retrieve` does the 11-component score fusion + per-row
//! BLAKE3 verification. CLI output mirrors the JSON wire shape; with
//! `--json` we emit one-line stable JSON for piping, without it we
//! pretty-print the top hits.

use std::path::Path;

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

/// `root retrieve <query> [--top-k 50] [--branch <name>] [--profile compliance|default] [--json]`
pub async fn run_retrieve(
    conn: &EngineConnection,
    root: &Path,
    query: &str,
    top_k: usize,
    branch: Option<&str>,
    profile: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let ws = cortex_remote::ensure_mounted_remote(conn, root).await?;

    // Wire shape mirrors `engine::RetrievalRequest`. The daemon does
    // not require profile to be set (defaults to ScoringProfile::default).
    let mut body = serde_json::json!({
        "query": query,
        "top_k": top_k,
    });
    if let Some(b) = branch {
        body["branch"] = serde_json::json!(b);
    }
    if let Some(p) = profile {
        body["profile"] = serde_json::json!(p);
    }
    let path = format!("/api/v1/ws/{ws}/search/hybrid");
    let data = cortex_remote::post_json(conn, &path, &body)
        .await
        .context("hybrid retrieve")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let hits = data
        .get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let total_candidates = data
        .get("total_candidates")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    println!();
    println!(
        "  {} \"{}\"   ({} candidates → {} hits)",
        style("Hybrid retrieve:").white().bold(),
        style(query).cyan(),
        total_candidates,
        hits.len(),
    );
    if hits.is_empty() {
        println!("\n  {}", style("No hits.").dim());
        return Ok(());
    }
    println!();
    for (i, hit) in hits.iter().enumerate() {
        let claim_id = hit.get("claim_id").and_then(|v| v.as_str()).unwrap_or("?");
        let statement = hit
            .get("statement")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let score = hit.get("fused_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let tier = hit
            .get("admission_tier")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!(
            "  {:>2}. {}  {}  {}",
            i + 1,
            style(format!("{score:.3}")).cyan().bold(),
            style(format!("[{tier}]")).yellow(),
            style(claim_id).dim(),
        );
        println!("      {}", style(statement).white());
        if let Some(verified) = hit.get("provenance_verified").and_then(|v| v.as_bool()) {
            let mark = if verified {
                style("✓").green()
            } else {
                style("✗").red()
            };
            println!("      {} provenance", mark);
        }
    }
    println!();
    Ok(())
}
