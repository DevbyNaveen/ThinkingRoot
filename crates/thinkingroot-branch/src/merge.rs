// crates/thinkingroot-branch/src/merge.rs
use std::path::Path;
use thinkingroot_core::error::Error;
use thinkingroot_core::{KnowledgeDiff, MergedBy, Result};
use thinkingroot_graph::graph::GraphStore;
use crate::branch::BranchRegistry;
use crate::snapshot::resolve_data_dir;

/// Execute a merge of `branch_name` into main.
///
/// 1. Verify diff.merge_allowed (abort with MergeBlocked if not).
/// 2. Open main graph.
/// 3. Insert new claims from diff into main.
/// 4. Link each new claim to matching entities in main (by canonical name lookup).
/// 5. Auto-resolved: supersede the losing claim in main.
/// 6. Insert new entities into main.
/// 7. Rebuild entity relations in main for consistency.
/// 8. Mark branch as merged in registry.
pub async fn execute_merge(
    root_path: &Path,
    branch_name: &str,
    diff: &KnowledgeDiff,
    merged_by: MergedBy,
    _propagate_deletions: bool,
) -> Result<()> {
    if !diff.merge_allowed {
        return Err(Error::MergeBlocked(diff.blocking_reasons.join("; ")));
    }

    let main_data_dir = resolve_data_dir(root_path, None);
    let main_graph = GraphStore::init(&main_data_dir.join("graph"))?;

    // Insert new claims
    for diff_claim in &diff.new_claims {
        let c = &diff_claim.claim;
        main_graph.insert_claim(c)?;

        // Link to entities by canonical name
        for entity_name in &diff_claim.entity_context {
            if let Some(entity_id) = main_graph.find_entity_id_by_name(entity_name)? {
                main_graph.link_claim_to_entity(&c.id.to_string(), &entity_id)?;
            }
        }
    }

    // Auto-resolved: supersede the loser in main
    for resolution in &diff.auto_resolved {
        if resolution.winner == resolution.branch_claim_id {
            // Branch won — supersede main claim
            main_graph.supersede_claim(
                &resolution.main_claim_id,
                &resolution.branch_claim_id,
            )?;
        }
        // If main won — branch claim is simply not inserted
    }

    // Insert new entities
    for diff_entity in &diff.new_entities {
        main_graph.insert_entity(&diff_entity.entity)?;
    }

    // Rebuild entity relations for consistency
    main_graph.rebuild_entity_relations()?;

    // Mark branch as merged in registry
    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = BranchRegistry::load_or_create(&refs_dir)?;
    registry.mark_merged(branch_name, merged_by)?;

    Ok(())
}
