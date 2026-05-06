//! Stream C — `root proposal {open,review,list,close}` subcommands.
//!
//! Knowledge Proposals (T0.4) gate `MergePolicy::RequiresProposal`
//! merges. Until this CLI subcommand shipped, proposals were reachable
//! only via REST or MCP — meaning `root` users running a strict-merge
//! workflow had no way to propose / approve from the terminal.
//!
//! Every operation routes through the cortex-resolved daemon. The
//! REST routes exist (rest.rs:2218..2398); these are CLI bindings only.

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

/// `root proposal open <branch> [--target main] [--description <text>]`
pub async fn run_open(
    conn: &EngineConnection,
    branch: &str,
    target: &str,
    description: Option<String>,
    min_reviewers: Option<u8>,
) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "target": target,
        "description": description,
        "min_reviewers": min_reviewers,
    });
    let path = format!("/api/v1/branches/{branch}/proposals");
    let data = cortex_remote::post_json(conn, &path, &body)
        .await
        .context("open proposal")?;
    let id = data
        .get("proposal")
        .and_then(|p| p.get("id"))
        .and_then(|i| i.as_str())
        .unwrap_or("?");
    println!(
        "  {} Proposal {} opened on branch {}",
        style("✓").green().bold(),
        style(id).cyan().bold(),
        style(branch).white().bold(),
    );
    Ok(())
}

/// `root proposal list [--branch <name>]`
pub async fn run_list(
    conn: &EngineConnection,
    branch: Option<&str>,
) -> anyhow::Result<()> {
    let path = match branch {
        Some(b) => format!("/api/v1/branches/{b}/proposals"),
        None => "/api/v1/proposals".to_string(),
    };
    let data = cortex_remote::get_json(conn, &path).await?;
    let proposals = data
        .get("proposals")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if proposals.is_empty() {
        println!("  {}", style("No proposals.").dim());
        return Ok(());
    }
    println!();
    for p in proposals {
        let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let source = p
            .get("source_branch")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let target = p
            .get("target_branch")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let status = p
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!(
            "  {} {} {}  {} → {}  ({})",
            style("•").cyan(),
            style(id).white().bold(),
            style(status).yellow(),
            style(source).cyan(),
            style(target).cyan(),
            style(format_chrono_or_blank(&p, "opened_at")).dim(),
        );
    }
    println!();
    Ok(())
}

/// `root proposal review <id> --approve|--request-changes|--comment [--note <text>]`
pub async fn run_review(
    conn: &EngineConnection,
    id: &str,
    decision: &str,
    note: Option<String>,
) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "decision": decision,
        "note": note,
    });
    let path = format!("/api/v1/proposals/{id}/reviews");
    cortex_remote::post_json(conn, &path, &body)
        .await
        .with_context(|| format!("review proposal {id}"))?;
    println!(
        "  {} Review recorded ({}) on {}",
        style("✓").green().bold(),
        style(decision).cyan(),
        style(id).white().bold(),
    );
    Ok(())
}

/// `root proposal close <id>`
pub async fn run_close(conn: &EngineConnection, id: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/proposals/{id}/close");
    cortex_remote::post_json(conn, &path, &serde_json::json!({})).await?;
    println!(
        "  {} Proposal {} closed",
        style("✓").green().bold(),
        style(id).white().bold()
    );
    Ok(())
}

fn format_chrono_or_blank(p: &serde_json::Value, key: &str) -> String {
    p.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}
