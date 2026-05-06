//! Stream C — `root branch {events,stats,lineage,rebase,rollback}`
//! subcommands. Each routes through the cortex-resolved daemon's
//! existing REST surface (rest.rs:1567..2059, 2795..2868). Pure CLI
//! bindings — no new daemon work.

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

/// `root branch events <name>` — print the audit log entries for a branch.
pub async fn run_events(conn: &EngineConnection, branch: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/branches/{branch}/events");
    let data = cortex_remote::get_json(conn, &path)
        .await
        .with_context(|| format!("fetch events for {branch}"))?;
    let events = data
        .get("events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if events.is_empty() {
        println!(
            "  {} {}",
            style("No events recorded for").dim(),
            style(branch).cyan()
        );
        return Ok(());
    }
    println!(
        "\n  {} {}",
        style("Events for branch:").white().bold(),
        style(branch).cyan().bold()
    );
    for e in events {
        let kind = e
            .as_object()
            .and_then(|o| o.keys().next().cloned())
            .unwrap_or_else(|| "Unknown".to_string());
        let at = e
            .get(&kind)
            .and_then(|v| v.get("at"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!(
            "    {} {}  {}",
            style("•").cyan(),
            style(kind).white().bold(),
            style(at).dim()
        );
    }
    println!();
    Ok(())
}

/// `root branch stats <name>` — claim/entity/source counts for a branch.
pub async fn run_stats(conn: &EngineConnection, branch: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/branches/{branch}/stats");
    let data = cortex_remote::get_json(conn, &path)
        .await
        .with_context(|| format!("fetch stats for {branch}"))?;
    let claims = data.get("claim_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let entities = data
        .get("entity_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let sources = data
        .get("source_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let events = data.get("event_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    println!(
        "\n  {} {} ({})\n    claims:   {}\n    entities: {}\n    sources:  {}\n    events:   {}\n",
        style("Branch").white().bold(),
        style(branch).cyan().bold(),
        style(status).yellow(),
        claims,
        entities,
        sources,
        events
    );
    Ok(())
}

/// `root branch lineage` — fork/merge DAG across all branches.
pub async fn run_lineage(conn: &EngineConnection) -> anyhow::Result<()> {
    let data = cortex_remote::get_json(conn, "/api/v1/branches/lineage").await?;
    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}

/// `root branch rebase <name>` — sync branch with parent (apply parent-only claims).
pub async fn run_rebase(conn: &EngineConnection, branch: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/branches/{branch}/rebase");
    cortex_remote::post_json(conn, &path, &serde_json::json!({}))
        .await
        .with_context(|| format!("rebase {branch}"))?;
    println!(
        "  {} Branch {} rebased onto its parent",
        style("✓").green().bold(),
        style(branch).cyan().bold()
    );
    Ok(())
}

/// `root branch rollback <name>` — restore main from pre-merge snapshot.
pub async fn run_rollback(conn: &EngineConnection, branch: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/branches/{branch}/rollback");
    cortex_remote::post_json(conn, &path, &serde_json::json!({}))
        .await
        .with_context(|| format!("rollback merge of {branch}"))?;
    println!(
        "  {} Merge of {} rolled back",
        style("✓").green().bold(),
        style(branch).cyan().bold()
    );
    Ok(())
}
