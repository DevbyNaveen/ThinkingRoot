use std::path::PathBuf;

use anyhow::Context as _;
use console::style;
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};

pub fn run_workspace_add(
    path: PathBuf,
    name: Option<String>,
    port: Option<u16>,
) -> anyhow::Result<()> {
    let abs_path = std::fs::canonicalize(&path)
        .with_context(|| format!("path not found: {}", path.display()))?;

    let mut registry = WorkspaceRegistry::load()?;

    let ws_name = name.unwrap_or_else(|| {
        abs_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string())
    });

    let ws_port = port.unwrap_or_else(|| registry.next_available_port());

    registry.add(WorkspaceEntry {
        name: ws_name.clone(),
        path: abs_path.clone(),
        port: ws_port,
    });
    registry.save()?;

    println!();
    println!(
        "  {} workspace \"{}\"",
        style("✓ Registered").green().bold(),
        style(&ws_name).white().bold()
    );
    println!("    Path:  {}", abs_path.display());
    println!("    Port:  {}", ws_port);
    println!(
        "\n  Run {} to compile it.",
        style(format!("root compile {}", abs_path.display())).cyan()
    );
    Ok(())
}

pub fn run_workspace_list() -> anyhow::Result<()> {
    let registry = WorkspaceRegistry::load()?;

    if registry.workspaces.is_empty() {
        println!();
        println!("  No workspaces registered.");
        println!(
            "  Run {} to add one.",
            style("root workspace add <path>").cyan()
        );
        return Ok(());
    }

    println!();
    println!(
        "  {:<20} {:<45} {:<6} {}",
        style("Name").bold(),
        style("Path").bold(),
        style("Port").bold(),
        style("Status").bold()
    );
    println!("  {}", style("─".repeat(80)).dim());

    for ws in &registry.workspaces {
        let data_dir = ws.path.join(".thinkingroot");
        let status = if data_dir.join("graph.db").exists() {
            style("compiled ✓").green().to_string()
        } else {
            style("not compiled").yellow().to_string()
        };
        println!(
            "  {:<20} {:<45} {:<6} {}",
            ws.name,
            ws.path.display(),
            ws.port,
            status
        );
    }
    println!();
    Ok(())
}

pub fn run_workspace_remove(name: &str) -> anyhow::Result<()> {
    let mut registry = WorkspaceRegistry::load()?;

    if !registry.remove(name) {
        anyhow::bail!(
            "workspace \"{}\" not found. Run `root workspace list` to see registered workspaces.",
            name
        );
    }

    registry.save()?;
    println!(
        "  {} workspace \"{}\"",
        style("✓ Removed").green().bold(),
        name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::WorkspaceRegistry;

    #[test]
    fn add_workspace_increments_port_automatically() {
        let mut reg = WorkspaceRegistry::default();
        let port = reg.next_available_port();
        assert_eq!(port, 3000);
        reg.add(WorkspaceEntry {
            name: "first".to_string(),
            path: PathBuf::from("/first"),
            port,
        });
        let port2 = reg.next_available_port();
        assert_eq!(port2, 3001);
    }

    #[test]
    fn remove_nonexistent_workspace_returns_false() {
        let mut reg = WorkspaceRegistry::default();
        assert!(!reg.remove("ghost"));
    }
}
