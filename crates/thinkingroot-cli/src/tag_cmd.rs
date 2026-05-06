//! Stream C — `root tag {create,list,get}` subcommands.
//!
//! T2.5 immutable snapshot tags. The daemon ships REST routes
//! (rest.rs:1789..1849); these are CLI bindings only.

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

/// `root tag create <name> --branch <branch> [--message <text>]`
pub async fn run_create(
    conn: &EngineConnection,
    name: &str,
    branch: &str,
    message: Option<String>,
) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "name": name,
        "branch": branch,
        "message": message,
    });
    cortex_remote::post_json(conn, "/api/v1/tags", &body)
        .await
        .with_context(|| format!("create tag {name}"))?;
    println!(
        "  {} Tag {} created from branch {}",
        style("✓").green().bold(),
        style(name).cyan().bold(),
        style(branch).white().bold(),
    );
    Ok(())
}

/// `root tag list`
pub async fn run_list(conn: &EngineConnection) -> anyhow::Result<()> {
    let data = cortex_remote::get_json(conn, "/api/v1/tags").await?;
    let tags = data
        .get("tags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if tags.is_empty() {
        println!("  {}", style("No tags.").dim());
        return Ok(());
    }
    println!();
    for t in tags {
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let target = t
            .get("target_commit_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let created = t
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!(
            "  {} {} {}",
            style("•").cyan(),
            style(name).white().bold(),
            style(format!("→ {} ({})", &target[..target.len().min(12)], created)).dim(),
        );
    }
    println!();
    Ok(())
}

/// `root tag get <name>` — print full tag JSON for inspection.
pub async fn run_get(conn: &EngineConnection, name: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/tags/{name}");
    let data = cortex_remote::get_json(conn, &path).await?;
    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}
