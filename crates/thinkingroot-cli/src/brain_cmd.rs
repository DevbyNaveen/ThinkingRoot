//! `root brief` and `root investigate` — CLI parity with the MCP
//! brain probes.  Both route through the daemon's REST surface
//! (`POST /api/v1/ws/{ws}/brain/{brief,investigate}`) — the
//! engine remains the sole owner of `graph.db` per Cortex Protocol.
//!
//! `focus` is intentionally not exposed: it mutates per-session
//! `SessionContext.focus_entity` which has no meaning for a one-shot
//! CLI invocation. The MCP path keeps it for LLM session continuity.

use std::path::Path;

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

/// `root brief [--branch <name>] [--json]`
pub async fn run_brief(
    conn: &EngineConnection,
    root: &Path,
    branch: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let ws = cortex_remote::ensure_mounted_remote(conn, root).await?;
    let body = serde_json::json!({ "branch": branch });
    let path = format!("/api/v1/ws/{ws}/brain/brief");
    let data = cortex_remote::post_json(conn, &path, &body)
        .await
        .context("brief")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let workspace = data
        .get("workspace")
        .and_then(|v| v.as_str())
        .unwrap_or(&ws);
    let entity_count = data
        .get("entity_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let claim_count = data.get("claim_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let source_count = data
        .get("source_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let contradiction_count = data
        .get("contradiction_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    println!();
    println!(
        "  {} {} {}",
        style("Workspace").white().bold(),
        style(workspace).cyan().bold(),
        if let Some(b) = branch {
            style(format!("(branch {b})")).dim().to_string()
        } else {
            style("(main)").dim().to_string()
        },
    );
    println!(
        "  {}  sources: {}   entities: {}   claims: {}   contradictions: {}",
        style("Counts").white(),
        style(source_count).cyan().bold(),
        style(entity_count).cyan().bold(),
        style(claim_count).cyan().bold(),
        if contradiction_count > 0 {
            style(contradiction_count).yellow().bold()
        } else {
            style(contradiction_count).green().bold()
        },
    );

    if let Some(top) = data.get("top_entities").and_then(|v| v.as_array())
        && !top.is_empty()
    {
        println!("\n  {}", style("Top entities").white().bold());
        for ent in top.iter().take(10) {
            let name = ent.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let kind = ent
                .get("entity_type")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let claims = ent.get("claim_count").and_then(|v| v.as_u64()).unwrap_or(0);
            println!(
                "    {} {}  {}  {}",
                style("•").cyan(),
                style(name).white().bold(),
                style(format!("[{kind}]")).dim(),
                style(format!("{claims} claims")).dim(),
            );
        }
    }

    if let Some(decisions) = data.get("recent_decisions").and_then(|v| v.as_array())
        && !decisions.is_empty()
    {
        println!("\n  {}", style("Recent decisions").white().bold());
        for d in decisions.iter().take(5) {
            let stmt = d
                .get(0)
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let conf = d.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            println!(
                "    {} {}  {}",
                style("•").cyan(),
                style(stmt).white(),
                style(format!("conf={conf:.2}")).dim(),
            );
        }
    }
    println!();
    Ok(())
}

/// `root investigate <entity> [--branch <name>] [--json]`
pub async fn run_investigate(
    conn: &EngineConnection,
    root: &Path,
    entity: &str,
    branch: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let ws = cortex_remote::ensure_mounted_remote(conn, root).await?;
    let body = serde_json::json!({ "entity": entity, "branch": branch });
    let path = format!("/api/v1/ws/{ws}/brain/investigate");
    let data = cortex_remote::post_json(conn, &path, &body)
        .await
        .with_context(|| format!("investigate {entity}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let name = data.get("name").and_then(|v| v.as_str()).unwrap_or(entity);
    let kind = data
        .get("entity_type")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let desc = data.get("description").and_then(|v| v.as_str()).unwrap_or("");

    println!();
    println!(
        "  {} {}  {}",
        style("Entity").white().bold(),
        style(name).cyan().bold(),
        style(format!("[{kind}]")).dim(),
    );
    if !desc.is_empty() {
        println!("    {}", style(desc).white());
    }

    if let Some(out) = data.get("outgoing_relations").and_then(|v| v.as_array())
        && !out.is_empty()
    {
        println!("\n  {}", style("Outgoing relations").white().bold());
        for r in out {
            let target = r.get(0).and_then(|v| v.as_str()).unwrap_or("?");
            let rel = r.get(1).and_then(|v| v.as_str()).unwrap_or("?");
            let strength = r.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
            println!(
                "    {} {} [{}] {}",
                style("→").cyan(),
                style(target).white(),
                style(rel).yellow(),
                style(format!("{strength:.2}")).dim(),
            );
        }
    }

    if let Some(inc) = data.get("incoming_relations").and_then(|v| v.as_array())
        && !inc.is_empty()
    {
        println!("\n  {}", style("Incoming relations").white().bold());
        for r in inc {
            let source = r.get(0).and_then(|v| v.as_str()).unwrap_or("?");
            let rel = r.get(1).and_then(|v| v.as_str()).unwrap_or("?");
            let strength = r.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
            println!(
                "    {} {} [{}] {}",
                style("←").cyan(),
                style(source).white(),
                style(rel).yellow(),
                style(format!("{strength:.2}")).dim(),
            );
        }
    }

    let claim_count = data
        .get("claims")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let contradiction_count = data
        .get("contradictions")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    println!(
        "\n  {} claims: {}   contradictions: {}",
        style("Summary").white().bold(),
        style(claim_count).cyan().bold(),
        if contradiction_count > 0 {
            style(contradiction_count).yellow().bold()
        } else {
            style(contradiction_count).green().bold()
        },
    );
    println!();
    Ok(())
}
