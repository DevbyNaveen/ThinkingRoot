//! `root branch-template {list,get,upsert,delete,apply}` — CLI bindings
//! for the T3.7 branch-template registry.
//!
//! REST surface: `/api/v1/branch-templates` (collection) +
//! `/api/v1/branch-templates/{name}` (item). `apply` creates a new
//! branch with the named template, mirroring `POST /branches { template }`.

use anyhow::Context as _;
use console::style;
use thinkingroot_core::cortex::EngineConnection;

use crate::cortex_remote;

/// `root branch-template list`
pub async fn run_list(conn: &EngineConnection) -> anyhow::Result<()> {
    let data = cortex_remote::get_json(conn, "/api/v1/branch-templates")
        .await
        .context("list branch templates")?;
    let templates = data
        .get("templates")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if templates.is_empty() {
        println!("  {}", style("No branch templates registered.").dim());
        return Ok(());
    }
    println!();
    for t in templates {
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let desc = t
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("(no description)");
        println!(
            "  {} {}",
            style("•").cyan(),
            style(name).white().bold(),
        );
        println!("    {}", style(desc).dim());
    }
    println!();
    Ok(())
}

/// `root branch-template get <name>`
pub async fn run_get(conn: &EngineConnection, name: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/branch-templates/{name}");
    let data = cortex_remote::get_json(conn, &path)
        .await
        .with_context(|| format!("get template {name}"))?;
    let template = data.get("template").cloned().unwrap_or(serde_json::Value::Null);
    println!("{}", serde_json::to_string_pretty(&template)?);
    Ok(())
}

/// `root branch-template delete <name>`
pub async fn run_delete(conn: &EngineConnection, name: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/branch-templates/{name}");
    cortex_remote::delete_json(conn, &path)
        .await
        .with_context(|| format!("delete template {name}"))?;
    println!(
        "  {} template {} deleted",
        style("✓").green().bold(),
        style(name).white().bold()
    );
    Ok(())
}

/// `root branch-template upsert <json-file>` — read full BranchTemplate
/// JSON from disk and POST to the collection. Authoring TOML by hand
/// is more ergonomic via the on-disk file at
/// `<root>/.thinkingroot-refs/branch_templates.toml`; this command is
/// for scripted updates against a remote daemon where the file isn't
/// reachable.
pub async fn run_upsert(
    conn: &EngineConnection,
    file: &std::path::Path,
) -> anyhow::Result<()> {
    let body_str = std::fs::read_to_string(file)
        .with_context(|| format!("read {}", file.display()))?;
    let body: serde_json::Value = serde_json::from_str(&body_str)
        .with_context(|| format!("parse {} as BranchTemplate JSON", file.display()))?;
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("template JSON missing required 'name' field"))?
        .to_string();
    cortex_remote::post_json(conn, "/api/v1/branch-templates", &body)
        .await
        .context("upsert template")?;
    println!(
        "  {} template {} upserted",
        style("✓").green().bold(),
        style(&name).white().bold()
    );
    Ok(())
}

/// `root branch-template apply <template> --to <branch> [--description <text>]`
/// — materialise a new branch from the named template.
pub async fn run_apply(
    conn: &EngineConnection,
    template: &str,
    branch: &str,
    description: Option<String>,
) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "name": branch,
        "template": template,
        "description": description,
    });
    cortex_remote::post_json(conn, "/api/v1/branches", &body)
        .await
        .with_context(|| format!("apply template {template} as branch {branch}"))?;
    println!(
        "  {} branch {} created from template {}",
        style("✓").green().bold(),
        style(branch).cyan().bold(),
        style(template).white().bold()
    );
    Ok(())
}
