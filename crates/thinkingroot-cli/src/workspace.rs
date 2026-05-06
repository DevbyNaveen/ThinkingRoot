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

/// Stream G — `root workspace scan [--root <path>...]`.
///
/// Walks the supplied roots (or sensible defaults: `~/Desktop`,
/// `~/Documents`, `~/code`, `~/dev`, `~/projects`, `~/src`,
/// `~/workspace`) up to depth 4 looking for `.thinkingroot/`
/// markers. Newly-discovered workspaces are added to the shared
/// `WorkspaceRegistry`; existing entries are preserved.
///
/// Skips: `node_modules`, `target`, `.git`, `.venv`, `venv`,
/// `__pycache__`, `.next`, `dist`, `build`, `.cache`, `.idea`,
/// `.vscode`, `Library`, `Pictures`, `Music`, `Movies`,
/// `Applications`.
pub fn run_workspace_scan(roots: Vec<std::path::PathBuf>) -> anyhow::Result<()> {
    use console::style;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use thinkingroot_core::WorkspaceEntry;

    const MAX_DEPTH: usize = 4;
    const PRUNED: &[&str] = &[
        "node_modules",
        "target",
        ".git",
        ".venv",
        "venv",
        "__pycache__",
        ".next",
        "dist",
        "build",
        ".cache",
        ".idea",
        ".vscode",
        "Library",
        "Pictures",
        "Music",
        "Movies",
        "Applications",
    ];

    fn expand_tilde(s: &str) -> Option<PathBuf> {
        if let Some(rest) = s.strip_prefix("~/") {
            let home = std::env::var("HOME").ok()?;
            Some(PathBuf::from(home).join(rest))
        } else if s == "~" {
            std::env::var("HOME").ok().map(PathBuf::from)
        } else {
            Some(PathBuf::from(s))
        }
    }

    fn default_roots() -> Vec<PathBuf> {
        let Ok(home) = std::env::var("HOME") else {
            return Vec::new();
        };
        let home = PathBuf::from(home);
        [
            "Desktop",
            "Documents",
            "code",
            "dev",
            "projects",
            "src",
            "workspace",
        ]
        .iter()
        .map(|sub| home.join(sub))
        .filter(|p| p.exists())
        .collect()
    }

    fn walk(dir: &std::path::Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > MAX_DEPTH {
            return;
        }
        let marker = dir.join(".thinkingroot");
        if marker.is_dir() {
            out.push(dir.to_path_buf());
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') && name != ".thinkingroot" {
                continue;
            }
            if PRUNED.contains(&name) {
                continue;
            }
            walk(&path, depth + 1, out);
        }
    }

    let resolved_roots: Vec<PathBuf> = if roots.is_empty() {
        if let Ok(from_env) = std::env::var("TR_SCAN_ROOTS") {
            let parsed: Vec<PathBuf> = from_env
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(expand_tilde)
                .collect();
            if parsed.is_empty() { default_roots() } else { parsed }
        } else {
            default_roots()
        }
    } else {
        roots
            .into_iter()
            .map(|p| expand_tilde(p.to_string_lossy().as_ref()).unwrap_or(p))
            .collect()
    };

    let mut discovered: Vec<PathBuf> = Vec::new();
    for root in &resolved_roots {
        if let Ok(canon) = std::fs::canonicalize(root) {
            walk(&canon, 0, &mut discovered);
        }
    }

    let mut registry = WorkspaceRegistry::load()?;
    let known: HashSet<PathBuf> = registry
        .workspaces
        .iter()
        .map(|w| w.path.clone())
        .collect();

    let mut new_count = 0usize;
    for path in &discovered {
        if known.contains(path) {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string());
        let port = registry.next_available_port();
        registry.add(WorkspaceEntry {
            name: name.clone(),
            path: path.clone(),
            port,
        });
        new_count += 1;
        println!(
            "  {} {} {}",
            style("+").green().bold(),
            style(&name).cyan().bold(),
            style(format!("({})", path.display())).dim()
        );
    }
    if new_count > 0 {
        registry.save()?;
    }
    println!(
        "\n  {} {} workspace(s); registered {} new\n",
        style("Discovered").white().bold(),
        discovered.len(),
        new_count
    );
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
