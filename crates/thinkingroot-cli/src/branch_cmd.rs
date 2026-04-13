// crates/thinkingroot-cli/src/branch_cmd.rs
//! CLI handlers for the Knowledge Version Control (KVC) subcommands:
//! branch, checkout, diff, merge, status, snapshot.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Context as _;
use console::style;
use thinkingroot_branch::{
    create_branch, delete_branch, diff::compute_diff, gc_branches, list_branches,
    merge::execute_merge, purge_branch, read_head_branch, rollback_merge,
    snapshot::resolve_data_dir, write_head_branch,
};
use thinkingroot_core::{Config, MergedBy, types::ContentHash};
use thinkingroot_graph::graph::GraphStore;

/// Handle `root branch [<name>] [--list] [--delete <name>] [--purge <name>] [--gc]`
///
/// With no arguments, defaults to `--list`.
pub async fn handle_branch(
    root: &Path,
    name: Option<&str>,
    list: bool,
    delete: Option<&str>,
    purge: Option<&str>,
    gc: bool,
    description: Option<String>,
) -> anyhow::Result<()> {
    // Default to list when no action is specified.
    let list = list || (name.is_none() && delete.is_none() && purge.is_none() && !gc);
    if list {
        let branches = list_branches(root)?;
        if branches.is_empty() {
            println!("  No branches — you are on main");
        } else {
            let head = read_head_branch(root).unwrap_or_else(|_| "main".to_string());
            for b in &branches {
                let marker = if b.name == head { "*" } else { " " };
                println!(
                    "  {} {} {}",
                    style(marker).green().bold(),
                    style(&b.name).white().bold(),
                    style(format!("(from: {})", b.parent)).dim()
                );
            }
        }
        return Ok(());
    }

    if let Some(to_delete) = delete {
        delete_branch(root, to_delete)
            .with_context(|| format!("branch '{}' not found", to_delete))?;
        println!(
            "  {} Branch '{}' abandoned (data dir kept — use --purge to remove)",
            style("✓").green(),
            to_delete
        );
        return Ok(());
    }

    if let Some(to_purge) = purge {
        purge_branch(root, to_purge).with_context(|| format!("branch '{}' not found", to_purge))?;
        println!(
            "  {} Branch '{}' purged (data directory removed)",
            style("✓").green(),
            to_purge
        );
        return Ok(());
    }

    if gc {
        let removed = gc_branches(root).context("garbage collection failed")?;
        if removed == 0 {
            println!("  No abandoned branches to collect");
        } else {
            println!(
                "  {} Collected {} abandoned branch data director{}",
                style("✓").green(),
                removed,
                if removed == 1 { "y" } else { "ies" }
            );
        }
        return Ok(());
    }

    if let Some(branch_name) = name {
        let parent = read_head_branch(root).unwrap_or_else(|_| "main".to_string());
        let branch = create_branch(root, branch_name, &parent, description)
            .await
            .with_context(|| format!("failed to create branch '{}'", branch_name))?;
        println!(
            "  {} Created branch '{}' from '{}'",
            style("✓").green(),
            style(&branch.name).cyan().bold(),
            style(&branch.parent).white()
        );
        println!("  Hint: root checkout {}", branch.name);
    }
    Ok(())
}

/// Handle `root merge <branch> --rollback`
pub fn handle_rollback(root: &Path, branch: &str) -> anyhow::Result<()> {
    rollback_merge(root, branch).with_context(|| format!("rollback of '{}' failed", branch))?;
    println!(
        "  {} Main rolled back to state before '{}' was merged",
        style("✓").green().bold(),
        style(branch).cyan().bold()
    );
    Ok(())
}

/// Handle `root checkout <name>`
pub async fn handle_checkout(root: &Path, name: &str) -> anyhow::Result<()> {
    // Allow checking out main without it being in the branch registry.
    if name != "main" {
        let branches = list_branches(root)?;
        if !branches.iter().any(|b| b.name == name) {
            anyhow::bail!(
                "branch '{}' not found. Run `root branch --list` to see branches.",
                name
            );
        }
    }
    write_head_branch(root, name)?;
    println!(
        "  {} Switched to branch '{}'",
        style("✓").green(),
        style(name).cyan().bold()
    );
    Ok(())
}

/// Handle `root diff <branch>`
pub async fn handle_diff(root: &Path, branch: &str) -> anyhow::Result<()> {
    let config = Config::load_merged(root)?;
    let mc = &config.merge;

    let main_data_dir = resolve_data_dir(root, None);
    let branch_data_dir = resolve_data_dir(root, Some(branch));

    if !branch_data_dir.exists() {
        anyhow::bail!(
            "branch '{}' not found at {}",
            branch,
            branch_data_dir.display()
        );
    }

    let main_graph =
        GraphStore::init(&main_data_dir.join("graph")).context("failed to open main graph")?;
    let branch_graph =
        GraphStore::init(&branch_data_dir.join("graph")).context("failed to open branch graph")?;

    let diff = compute_diff(
        &main_graph,
        &branch_graph,
        branch,
        mc.auto_resolve_threshold,
        mc.max_health_drop,
        mc.block_on_contradictions,
    )?;

    println!(
        "\n  {} {} → main",
        style("Knowledge PR:").white().bold(),
        style(branch).cyan().bold()
    );
    println!(
        "  Health:  before={}%  after={}%",
        style(format!("{:.1}", diff.health_before.overall * 100.0)).yellow(),
        style(format!("{:.1}", diff.health_after.overall * 100.0)).green()
    );
    println!();
    println!(
        "  {} New claims: {}",
        style("┌").dim(),
        style(diff.new_claims.len()).cyan().bold()
    );
    for dc in &diff.new_claims {
        println!(
            "  {} {} {}",
            style("│ +").green(),
            style(format!("[{:?}]", dc.claim.claim_type)).dim(),
            dc.claim.statement
        );
        if !dc.entity_context.is_empty() {
            println!(
                "  {}   entities: {}",
                style("│").dim(),
                dc.entity_context.join(", ")
            );
        }
    }
    println!(
        "  {} New entities: {}",
        style("│").dim(),
        style(diff.new_entities.len()).cyan().bold()
    );
    for de in &diff.new_entities {
        println!(
            "  {} {} ({:?})",
            style("│ +").green(),
            de.entity.canonical_name,
            de.entity.entity_type
        );
    }
    if !diff.new_relations.is_empty() {
        println!(
            "  {} New relations: {}",
            style("│").dim(),
            style(diff.new_relations.len()).cyan().bold()
        );
        for dr in &diff.new_relations {
            println!(
                "  {} {} --[{}]--> {} (strength={:.2})",
                style("│ +").green(),
                dr.from_name,
                dr.relation_type,
                dr.to_name,
                dr.strength,
            );
        }
    }
    if !diff.auto_resolved.is_empty() {
        println!(
            "  {} Auto-resolved: {}",
            style("│").dim(),
            style(diff.auto_resolved.len()).yellow().bold()
        );
        for r in &diff.auto_resolved {
            println!(
                "  {}   winner: {} (Δ={:.2})",
                style("│").dim(),
                r.winner,
                r.confidence_delta
            );
        }
    }
    if !diff.needs_review.is_empty() {
        println!(
            "  {} Contradictions needing review: {}",
            style("│").dim(),
            style(diff.needs_review.len()).red().bold()
        );
        for c in &diff.needs_review {
            println!("  {}   main:   {}", style("│").dim(), c.main_statement);
            println!("  {}   branch: {}", style("│").dim(), c.branch_statement);
        }
    }
    println!();
    if diff.merge_allowed {
        println!("  {} Merge allowed", style("✓").green().bold());
    } else {
        println!("  {} Merge blocked:", style("✗").red().bold());
        for reason in &diff.blocking_reasons {
            println!("    {} {}", style("-").red(), reason);
        }
    }
    println!();
    Ok(())
}

/// Handle `root merge <branch> [--force] [--propagate-deletions] [--resolve N=keep-main|keep-branch]`
pub async fn handle_merge(
    root: &Path,
    branch: &str,
    force: bool,
    propagate_deletions: bool,
    resolutions: &[String],
) -> anyhow::Result<()> {
    let config = Config::load_merged(root)?;
    let mc = &config.merge;

    let main_data_dir = resolve_data_dir(root, None);
    let branch_data_dir = resolve_data_dir(root, Some(branch));

    if !branch_data_dir.exists() {
        anyhow::bail!("branch '{}' not found", branch);
    }

    let main_graph =
        GraphStore::init(&main_data_dir.join("graph")).context("failed to open main graph")?;
    let branch_graph =
        GraphStore::init(&branch_data_dir.join("graph")).context("failed to open branch graph")?;

    let mut diff = compute_diff(
        &main_graph,
        &branch_graph,
        branch,
        mc.auto_resolve_threshold,
        mc.max_health_drop,
        mc.block_on_contradictions,
    )?;

    if force {
        diff.merge_allowed = true;
        diff.blocking_reasons.clear();
    }

    // Apply manual contradiction resolutions specified via --resolve <n>=keep-main|keep-branch.
    // The index corresponds to the 0-based position in the `needs_review` list as printed by `root diff`.
    if !resolutions.is_empty() {
        use thinkingroot_core::{AutoResolution, DiffClaim, DiffStatus};
        // Collect resolutions, validating format first to fail fast before mutating diff.
        let parsed: Vec<(usize, &str)> = resolutions
            .iter()
            .map(|spec| {
                let (idx_s, resolution) = spec.split_once('=').ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid --resolve format '{}': expected <index>=keep-main|keep-branch",
                        spec
                    )
                })?;
                let idx: usize = idx_s.trim().parse().map_err(|_| {
                    anyhow::anyhow!(
                        "--resolve index must be a non-negative integer, got '{}'",
                        idx_s
                    )
                })?;
                if idx >= diff.needs_review.len() {
                    anyhow::bail!(
                        "--resolve index {} out of range: only {} contradiction(s) need review",
                        idx,
                        diff.needs_review.len()
                    );
                }
                if resolution.trim() != "keep-main" && resolution.trim() != "keep-branch" {
                    anyhow::bail!(
                        "invalid --resolve value '{}': expected keep-main or keep-branch",
                        resolution
                    );
                }
                Ok((idx, resolution.trim()))
            })
            .collect::<anyhow::Result<_>>()?;

        // Track which needs_review indices to remove after applying all resolutions.
        let mut remove_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (idx, resolution) in &parsed {
            let pair = &diff.needs_review[*idx];
            match *resolution {
                "keep-main" => {
                    // Branch claim is simply not inserted — remove from review queue.
                    remove_ids.insert(pair.branch_claim_id.clone());
                }
                "keep-branch" => {
                    // Reconstruct the full Claim from the branch graph and promote to new_claims.
                    let branch_claim_id = pair.branch_claim_id.clone();
                    let main_claim_id = pair.main_claim_id.clone();
                    match branch_graph.get_claim_by_id(&branch_claim_id)? {
                        Some(claim) => {
                            let entity_map = branch_graph
                                .get_entity_names_for_claims(&[branch_claim_id.as_str()])?;
                            let entity_context = entity_map
                                .get(branch_claim_id.as_str())
                                .cloned()
                                .unwrap_or_default();
                            diff.new_claims.push(DiffClaim {
                                claim,
                                entity_context,
                                diff_status: DiffStatus::Added,
                            });
                            diff.auto_resolved.push(AutoResolution {
                                main_claim_id: main_claim_id.clone(),
                                branch_claim_id: branch_claim_id.clone(),
                                winner: branch_claim_id.clone(),
                                confidence_delta: 0.0,
                                reason: "Manual resolution: keep-branch".to_string(),
                            });
                        }
                        None => {
                            anyhow::bail!(
                                "branch claim '{}' not found in branch graph — cannot apply --resolve {}=keep-branch",
                                branch_claim_id,
                                idx
                            );
                        }
                    }
                    remove_ids.insert(pair.branch_claim_id.clone());
                }
                _ => unreachable!("validated above"),
            }
        }

        // Remove resolved items from needs_review.
        diff.needs_review
            .retain(|p| !remove_ids.contains(&p.branch_claim_id));

        // Re-evaluate merge_allowed now that some contradictions may have been resolved.
        // Drop any blocking reason that referenced contradictions if needs_review is now empty.
        if diff.needs_review.is_empty() {
            diff.blocking_reasons
                .retain(|r| !r.contains("contradiction"));
        }
        if !force {
            diff.merge_allowed = diff.blocking_reasons.is_empty();
        }
    }

    execute_merge(
        root,
        branch,
        &diff,
        MergedBy::Human {
            user: "cli".to_string(),
        },
        propagate_deletions,
    )
    .await
    .with_context(|| format!("merge of '{}' failed", branch))?;

    println!(
        "  {} Merged '{}' into main",
        style("✓").green().bold(),
        style(branch).cyan().bold()
    );
    println!("    {} new claims", diff.new_claims.len());
    println!("    {} new entities", diff.new_entities.len());
    println!(
        "    {} auto-resolved contradictions",
        diff.auto_resolved.len()
    );
    if !diff.needs_review.is_empty() {
        println!(
            "    {} contradiction(s) remain unresolved (use --resolve to address)",
            diff.needs_review.len()
        );
    }
    Ok(())
}

/// Handle `root status`
pub async fn handle_status(root: &Path) -> anyhow::Result<()> {
    // Canonicalize so that path comparisons against stored URIs are consistent.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

    let head = read_head_branch(&root).unwrap_or_else(|_| "main".to_string());

    println!(
        "\n  {} {}",
        style("On branch:").white().bold(),
        style(&head).cyan().bold()
    );

    // Resolve the graph.db for the active branch (main lives under .thinkingroot/,
    // feature branches under .thinkingroot-refs/branches/<name>/).
    let data_dir = if head == "main" {
        resolve_data_dir(&root, None)
    } else {
        resolve_data_dir(&root, Some(&head))
    };
    let graph_path = data_dir.join("graph");

    if !graph_path.exists() {
        println!(
            "  {}\n",
            style("Graph not compiled yet — run `root compile .` first").dim()
        );
        return Ok(());
    }

    let graph = GraphStore::init(&graph_path).context("failed to open graph")?;

    // Use parser config for gitignore / exclude-pattern rules so the file list
    // here matches exactly what the compiler would process.
    let config = Config::load_merged(&root).unwrap_or_default();

    let files_on_disk =
        thinkingroot_parse::walker::walk(&root, &config.parsers).unwrap_or_default();

    // (uri → stored_content_hash) from the graph.
    let graph_sources: HashMap<String, String> = graph
        .get_sources_with_hashes()
        .context("failed to read sources from graph")?
        .into_iter()
        .collect();

    let mut modified: Vec<String> = Vec::new();
    let mut untracked: Vec<String> = Vec::new();

    for file_path in &files_on_disk {
        let uri = file_path.to_string_lossy().to_string();
        match graph_sources.get(&uri) {
            Some(stored_hash) => {
                // Known file — check whether content has changed.
                match std::fs::read(file_path) {
                    Ok(bytes) => {
                        if ContentHash::from_bytes(&bytes).0 != *stored_hash {
                            modified.push(uri);
                        }
                    }
                    Err(_) => modified.push(uri), // unreadable → treat as modified
                }
            }
            None => untracked.push(uri),
        }
    }

    // Deleted: graph sources whose URI no longer exists on disk.
    let disk_uris: HashSet<String> = files_on_disk
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let mut deleted: Vec<String> = graph_sources
        .keys()
        .filter(|uri| !disk_uris.contains(*uri))
        .cloned()
        .collect();

    modified.sort();
    untracked.sort();
    deleted.sort();

    println!();
    println!("  {}", style("Sync Status").white().bold());
    println!("  {}", style("──────────────────────────────").dim());

    if modified.is_empty() && untracked.is_empty() && deleted.is_empty() {
        println!("  {} Graph is up to date", style("✓").green().bold());
    } else {
        for uri in &modified {
            println!(
                "  {}  {}",
                style("modified ").yellow().bold(),
                strip_root(&root, uri)
            );
        }
        for uri in &untracked {
            println!("  {}  {}", style("untracked").dim(), strip_root(&root, uri));
        }
        for uri in &deleted {
            println!(
                "  {}  {}",
                style("deleted  ").red().bold(),
                strip_root(&root, uri)
            );
        }

        let total = modified.len() + untracked.len() + deleted.len();
        println!();
        println!(
            "  {} {} file{} out of sync — run {} to update",
            style("!").yellow(),
            style(total).white().bold(),
            if total == 1 { "" } else { "s" },
            style("root compile").cyan()
        );
    }

    println!();
    Ok(())
}

/// Strip the workspace root prefix from a URI for compact display.
fn strip_root(root: &Path, uri: &str) -> String {
    let prefix = root.to_string_lossy();
    uri.strip_prefix(prefix.as_ref())
        .map(|s| s.trim_start_matches('/').to_string())
        .unwrap_or_else(|| uri.to_string())
}

/// Handle `root snapshot <name>`
pub async fn handle_snapshot(root: &Path, name: &str) -> anyhow::Result<()> {
    use thinkingroot_branch::branch::BranchRegistry;
    use thinkingroot_branch::snapshot::create_branch_layout;

    let head = read_head_branch(root).unwrap_or_else(|_| "main".to_string());
    let parent_data_dir = resolve_data_dir(root, Some(&head));
    let snapshot_data_dir = resolve_data_dir(root, Some(name));

    create_branch_layout(&parent_data_dir, &snapshot_data_dir)
        .with_context(|| format!("failed to create snapshot layout for '{}'", name))?;

    let refs_dir = root.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = BranchRegistry::load_or_create(&refs_dir)?;
    registry.create_branch(name, &head, Some(format!("Snapshot of {}", head)))?;

    println!(
        "  {} Snapshot '{}' created from '{}'",
        style("✓").green().bold(),
        style(name).cyan().bold(),
        style(&head).white()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn handle_status_on_new_workspace() {
        let dir = TempDir::new().unwrap();
        // Should not panic on a workspace with no branches yet.
        let result = handle_status(dir.path()).await;
        assert!(
            result.is_ok(),
            "status on new workspace should succeed: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn handle_branch_list_on_empty() {
        let dir = TempDir::new().unwrap();
        let result = handle_branch(dir.path(), None, true, None, None, false, None).await;
        assert!(
            result.is_ok(),
            "branch --list on empty workspace should succeed"
        );
    }
}
