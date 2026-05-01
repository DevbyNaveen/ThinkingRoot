// crates/thinkingroot-branch/src/merge.rs
use crate::branch::BranchRegistry;
use crate::lock::MergeLock;
use crate::snapshot::{resolve_data_dir, slugify};
use std::path::Path;
use thinkingroot_core::error::Error;
use thinkingroot_core::{KnowledgeDiff, MergedBy, Result};
use thinkingroot_graph::graph::GraphStore;

fn snapshot_target_db(
    target_data_dir: &std::path::Path,
    snapshot_prefix: &str,
    snapshot_subject: &str,
) -> Result<()> {
    let db_path = target_data_dir.join("graph").join("graph.db");
    if db_path.exists() {
        let ts = chrono::Utc::now().timestamp();
        let slug = slugify(snapshot_subject);
        let backup_path = target_data_dir
            .join("graph")
            .join(format!("graph.db.{snapshot_prefix}-{slug}-{ts}"));
        std::fs::copy(&db_path, &backup_path)?;
        tracing::debug!("snapshot written to {}", backup_path.display());
    }
    Ok(())
}

async fn apply_branch_diff(
    root_path: &Path,
    source_branch_name: &str,
    target_branch: Option<&str>,
    diff: &KnowledgeDiff,
    propagate_deletions: bool,
    snapshot_prefix: &str,
    snapshot_subject: &str,
) -> Result<()> {
    if source_branch_name == target_branch.unwrap_or("main") {
        return Err(Error::MergeBlocked(
            "source and target branches must be different".to_string(),
        ));
    }

    let _merge_lock = MergeLock::acquire(root_path)?;
    let target_data_dir = resolve_data_dir(root_path, target_branch);
    snapshot_target_db(&target_data_dir, snapshot_prefix, snapshot_subject)?;
    let target_graph = GraphStore::init(&target_data_dir.join("graph"))?;
    let source_data_dir = resolve_data_dir(root_path, Some(source_branch_name));
    let source_graph = GraphStore::init(&source_data_dir.join("graph"))?;

    // Copy source records for all new claims from the source graph.
    let mut copied_source_ids = std::collections::HashSet::new();
    for diff_claim in &diff.new_claims {
        let source_id = diff_claim.claim.source.to_string();
        if copied_source_ids.contains(&source_id) {
            continue;
        }
        match source_graph.get_source_by_id(&source_id) {
            Ok(Some(source)) => {
                if target_graph.find_sources_by_uri(&source.uri)?.is_empty() {
                    tracing::debug!(
                        "merge: copying source '{}' from branch '{}' into '{}'",
                        source.uri,
                        source_branch_name,
                        target_branch.unwrap_or("main")
                    );
                    target_graph.insert_source(&source)?;
                }
                copied_source_ids.insert(source_id);
            }
            Ok(None) => {
                tracing::warn!(
                    "merge: source '{}' not found in source graph — claim will be orphaned",
                    source_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "merge: failed to look up source '{}' in source graph: {}",
                    source_id,
                    e
                );
            }
        }
    }

    // Insert new claims
    for diff_claim in &diff.new_claims {
        let c = &diff_claim.claim;
        target_graph.insert_claim(c)?;
        target_graph.link_claim_to_source(&c.id.to_string(), &c.source.to_string())?;

        for entity_name in &diff_claim.entity_context {
            if let Some(entity_id) = target_graph.find_entity_id_by_name(entity_name)? {
                target_graph.link_claim_to_entity(&c.id.to_string(), &entity_id)?;
            }
        }
    }

    // Auto-resolved: supersede the loser in target
    for resolution in &diff.auto_resolved {
        if resolution.winner == resolution.branch_claim_id {
            target_graph.supersede_claim(&resolution.main_claim_id, &resolution.branch_claim_id)?;
        }
    }

    for diff_entity in &diff.new_entities {
        target_graph.insert_entity(&diff_entity.entity)?;
    }

    for diff_relation in &diff.new_relations {
        let from_id = target_graph.find_entity_id_by_name(&diff_relation.from_name)?;
        let to_id = target_graph.find_entity_id_by_name(&diff_relation.to_name)?;
        if let (Some(from), Some(to)) = (from_id, to_id) {
            target_graph.link_entities(
                &from,
                &to,
                &diff_relation.relation_type,
                diff_relation.strength,
            )?;
        }
    }

    if propagate_deletions {
        use std::collections::HashSet;
        let source_uris: HashSet<String> = source_graph
            .get_all_sources()?
            .into_iter()
            .map(|(_, uri, _)| uri)
            .collect();
        let target_sources = target_graph.get_all_sources()?;
        for (_id, uri, source_type) in target_sources {
            let is_file_source = matches!(
                source_type.as_str(),
                "File" | "Document" | "Markdown" | "Code"
            );
            if is_file_source && !source_uris.contains(&uri) {
                let mut candidate_claims = Vec::new();
                let mut candidate_entities = HashSet::new();

                for (sid, _, _) in target_graph.find_sources_by_uri(&uri).unwrap_or_default() {
                    candidate_claims.extend(
                        target_graph
                            .get_claim_ids_for_source(&sid)
                            .unwrap_or_default(),
                    );
                    candidate_entities.extend(
                        target_graph
                            .get_entity_ids_for_source(&sid)
                            .unwrap_or_default(),
                    );
                }

                let removed = target_graph.remove_source_by_uri(&uri)?;
                if removed > 0 {
                    tracing::info!(
                        "merge(propagate-deletions): removed source '{}' (deleted on branch '{}')",
                        uri,
                        source_branch_name
                    );

                    let mut vec_ids: Vec<String> = Vec::new();
                    for cid in candidate_claims {
                        vec_ids.push(format!("claim:{cid}"));
                    }
                    for eid in candidate_entities {
                        match target_graph.get_entity_by_id(&eid) {
                            Ok(None) => vec_ids.push(format!("entity:{eid}")),
                            Ok(Some(_)) => {}
                            Err(e) => {
                                tracing::warn!(
                                    "merge: failed to check existence of candidate entity '{}' (non-fatal): {}",
                                    eid,
                                    e
                                );
                            }
                        }
                    }

                    if !vec_ids.is_empty() {
                        if let Ok(mut target_vec) =
                            thinkingroot_graph::vector::VectorStore::init(&target_data_dir).await
                        {
                            let id_refs: Vec<&str> = vec_ids.iter().map(|s| s.as_str()).collect();
                            target_vec.remove_by_ids(&id_refs);
                            if let Err(e) = target_vec.save() {
                                tracing::warn!("vector purge save failed (non-fatal): {e}");
                            }
                        }
                    }
                }
            }
        }
    }

    target_graph.rebuild_entity_relations()?;

    if source_data_dir.join("vectors.bin").exists() {
        match (
            thinkingroot_graph::vector::VectorStore::init(&source_data_dir).await,
            thinkingroot_graph::vector::VectorStore::init(&target_data_dir).await,
        ) {
            (Ok(source_vec), Ok(mut target_vec)) => {
                let items = source_vec.all_items();
                if !items.is_empty() {
                    match target_vec.upsert_raw_batch(items) {
                        Ok(n) => {
                            if let Err(e) = target_vec.save() {
                                tracing::warn!("merge vector save failed (non-fatal): {e}");
                            } else {
                                tracing::info!(
                                    "merge: reconciled {n} branch vector embeddings into '{}'",
                                    target_branch.unwrap_or("main")
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!("merge vector reconciliation failed (non-fatal): {e}");
                        }
                    }
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!("merge vector store init failed (non-fatal): {e}");
            }
        }
    }

    Ok(())
}

/// Execute a merge of `branch_name` into main.
pub async fn execute_merge(
    root_path: &Path,
    branch_name: &str,
    diff: &KnowledgeDiff,
    merged_by: MergedBy,
    propagate_deletions: bool,
) -> Result<()> {
    execute_merge_into(
        root_path,
        branch_name,
        None,
        diff,
        merged_by,
        propagate_deletions,
    )
    .await
}

/// Execute a merge of `source_branch_name` into an explicit target branch.
pub async fn execute_merge_into(
    root_path: &Path,
    source_branch_name: &str,
    target_branch: Option<&str>,
    diff: &KnowledgeDiff,
    merged_by: MergedBy,
    propagate_deletions: bool,
) -> Result<()> {
    if !diff.merge_allowed {
        return Err(Error::MergeBlocked(diff.blocking_reasons.join("; ")));
    }
    apply_branch_diff(
        root_path,
        source_branch_name,
        target_branch,
        diff,
        propagate_deletions,
        "pre-merge",
        source_branch_name,
    )
    .await?;

    // Mark branch as merged in registry
    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = BranchRegistry::load_or_create(&refs_dir)?;
    registry.mark_merged(source_branch_name, merged_by)?;

    Ok(())
}

/// Rebase `branch_name` with changes from `parent_branch_name`.
pub async fn execute_rebase(
    root_path: &Path,
    branch_name: &str,
    parent_branch_name: &str,
    diff: &KnowledgeDiff,
) -> Result<()> {
    if !diff.merge_allowed {
        return Err(Error::MergeBlocked(diff.blocking_reasons.join("; ")));
    }
    apply_branch_diff(
        root_path,
        parent_branch_name,
        Some(branch_name),
        diff,
        false,
        "pre-rebase",
        branch_name,
    )
    .await
}

/// Roll back a merge by restoring the pre-merge snapshot of graph.db.
///
/// Finds the most recent `graph.db.pre-merge-{slug}-*` backup created when
/// `branch_name` was merged, and copies it back over the current `graph.db`.
///
/// Returns `Err` if no backup is found for the given branch.
pub fn rollback_merge(root_path: &Path, branch_name: &str) -> Result<()> {
    let main_data_dir = resolve_data_dir(root_path, None);
    let graph_dir = main_data_dir.join("graph");
    let slug = slugify(branch_name);
    let prefix = format!("graph.db.pre-merge-{slug}-");

    // Find all matching backups and pick the most recent (highest timestamp).
    let mut candidates: Vec<std::path::PathBuf> = std::fs::read_dir(&graph_dir)?
        .filter_map(|entry| entry.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect();

    if candidates.is_empty() {
        return Err(Error::MergeBlocked(format!(
            "no pre-merge backup found for branch '{}' — cannot roll back",
            branch_name
        )));
    }

    // Sort lexicographically; since the suffix is a Unix timestamp, this gives
    // chronological order and the last element is the most recent backup.
    candidates.sort();
    let backup = candidates.last().expect("non-empty after filter");

    let db_path = graph_dir.join("graph.db");
    std::fs::copy(backup, &db_path)?;
    tracing::info!(
        "rolled back main graph to pre-merge snapshot {}",
        backup.display()
    );
    Ok(())
}
