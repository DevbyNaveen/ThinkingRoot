// crates/thinkingroot-branch/src/merge.rs
use crate::branch::BranchRegistry;
use crate::lock::MergeLock;
use crate::snapshot::{resolve_data_dir, slugify};
use std::path::Path;
use thinkingroot_core::error::Error;
use thinkingroot_core::{KnowledgeDiff, MergedBy, Result};
use thinkingroot_graph::graph::GraphStore;

/// Execute a merge of `branch_name` into main.
///
/// 1. Verify diff.merge_allowed (abort with MergeBlocked if not).
/// 2. Snapshot main's graph.db to graph.db.pre-merge-{slug}-{ts} before any mutation.
/// 3. Open main graph.
/// 4. Insert new claims from diff into main.
/// 5. Link each new claim to matching entities in main (by canonical name lookup).
/// 6. Auto-resolved: supersede the losing claim in main.
/// 7. Insert new entities into main.
/// 8. Link new relations (by canonical name lookup).
/// 9. Rebuild entity relations in main for consistency.
/// 10. Mark branch as merged in registry.
pub async fn execute_merge(
    root_path: &Path,
    branch_name: &str,
    diff: &KnowledgeDiff,
    merged_by: MergedBy,
    propagate_deletions: bool,
) -> Result<()> {
    if !diff.merge_allowed {
        return Err(Error::MergeBlocked(diff.blocking_reasons.join("; ")));
    }

    // Acquire advisory merge lock — prevents concurrent merges from racing on graph.db.
    let _merge_lock = MergeLock::acquire(root_path)?;

    let main_data_dir = resolve_data_dir(root_path, None);

    // Pre-merge snapshot — copy graph.db before any mutation.
    // This enables `root merge --rollback <branch>` to restore to this point.
    let db_path = main_data_dir.join("graph").join("graph.db");
    if db_path.exists() {
        let ts = chrono::Utc::now().timestamp();
        let slug = slugify(branch_name);
        let backup_path = main_data_dir
            .join("graph")
            .join(format!("graph.db.pre-merge-{slug}-{ts}"));
        std::fs::copy(&db_path, &backup_path)?;
        tracing::debug!("pre-merge snapshot written to {}", backup_path.display());
    }

    let main_graph = GraphStore::init(&main_data_dir.join("graph"))?;

    // Copy source records for all new claims from the branch graph.
    // Claims carry a source_id foreign key — without the corresponding source row
    // in main, health checks will report them as orphaned claims.
    let branch_data_dir = resolve_data_dir(root_path, Some(branch_name));
    let branch_graph = GraphStore::init(&branch_data_dir.join("graph"))?;

    let mut copied_source_ids = std::collections::HashSet::new();
    for diff_claim in &diff.new_claims {
        let source_id = diff_claim.claim.source.to_string();
        if copied_source_ids.contains(&source_id) {
            continue;
        }
        match branch_graph.get_source_by_id(&source_id) {
            Ok(Some(source)) => {
                // Only insert if not already present in main (idempotent).
                if main_graph.find_sources_by_uri(&source.uri)?.is_empty() {
                    tracing::debug!("merge: copying source '{}' from branch to main", source.uri);
                    main_graph.insert_source(&source)?;
                }
                copied_source_ids.insert(source_id);
            }
            Ok(None) => {
                tracing::warn!(
                    "merge: source '{}' not found in branch graph — claim will be orphaned",
                    source_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "merge: failed to look up source '{}' in branch graph: {}",
                    source_id,
                    e
                );
            }
        }
    }

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
            main_graph.supersede_claim(&resolution.main_claim_id, &resolution.branch_claim_id)?;
        }
        // If main won — branch claim is simply not inserted
    }

    // Insert new entities
    for diff_entity in &diff.new_entities {
        main_graph.insert_entity(&diff_entity.entity)?;
    }

    // Link new relations — look up entity IDs by canonical name and call link_entities.
    for diff_relation in &diff.new_relations {
        let from_id = main_graph.find_entity_id_by_name(&diff_relation.from_name)?;
        let to_id = main_graph.find_entity_id_by_name(&diff_relation.to_name)?;
        if let (Some(from), Some(to)) = (from_id, to_id) {
            main_graph.link_entities(
                &from,
                &to,
                &diff_relation.relation_type,
                diff_relation.strength,
            )?;
        }
    }

    // Propagate deletions: sources present in main but absent in branch were
    // deleted on the branch — remove them (and all derived claims) from main.
    if propagate_deletions {
        use std::collections::HashSet;
        let branch_uris: HashSet<String> = branch_graph
            .get_all_sources()?
            .into_iter()
            .map(|(_, uri, _)| uri)
            .collect();
        let main_sources = main_graph.get_all_sources()?;
        for (_id, uri, source_type) in main_sources {
            // Only propagate deletions for file-based sources; skip Git/URL sources
            // that the branch may simply never have compiled.
            let is_file_source = matches!(
                source_type.as_str(),
                "File" | "Document" | "Markdown" | "Code"
            );
            if is_file_source && !branch_uris.contains(&uri) {
                // Collect candidate IDs *before* removal.
                let mut candidate_claims = Vec::new();
                let mut candidate_entities = HashSet::new();

                // Note: we use `unwrap_or_default()` here to treat "source not found" as no-op during deletion propagation.
                for (sid, _, _) in main_graph.find_sources_by_uri(&uri).unwrap_or_default() {
                    candidate_claims.extend(
                        main_graph
                            .get_claim_ids_for_source(&sid)
                            .unwrap_or_default(),
                    );
                    candidate_entities.extend(
                        main_graph
                            .get_entity_ids_for_source(&sid)
                            .unwrap_or_default(),
                    );
                }

                let removed = main_graph.remove_source_by_uri(&uri)?;
                if removed > 0 {
                    tracing::info!(
                        "merge(propagate-deletions): removed source '{}' (deleted on branch '{}')",
                        uri,
                        branch_name
                    );

                    // Identify which IDs should actually be purged from the vector index.
                    // Claims are always removed when their source is removed.
                    // Entities are only purged if they were actually orphaned and removed from the graph store.
                    let mut vec_ids: Vec<String> = Vec::new();
                    for cid in candidate_claims {
                        vec_ids.push(format!("claim:{cid}"));
                    }
                    for eid in candidate_entities {
                        match main_graph.get_entity_by_id(&eid) {
                            Ok(None) => {
                                // Entity truly removed from graph — candidate for vector purge.
                                vec_ids.push(format!("entity:{eid}"));
                            }
                            Ok(Some(_)) => {
                                // Entity still exists (supported by other sources).
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "merge: failed to check existence of candidate entity '{}' (non-fatal): {}",
                                    eid,
                                    e
                                );
                            }
                        }
                    }

                    // Purge stale embeddings from the main vector index.
                    if !vec_ids.is_empty() {
                        let main_data_dir = resolve_data_dir(root_path, None);
                        if let Ok(mut main_vec) =
                            thinkingroot_graph::vector::VectorStore::init(&main_data_dir).await
                        {
                            let id_refs: Vec<&str> = vec_ids.iter().map(|s| s.as_str()).collect();
                            main_vec.remove_by_ids(&id_refs);
                            if let Err(e) = main_vec.save() {
                                tracing::warn!("vector purge save failed (non-fatal): {e}");
                            }
                        }
                    }
                }
            }
        }
    }

    // Rebuild entity relations for consistency
    main_graph.rebuild_entity_relations()?;

    // Reconcile vector indexes: copy branch embeddings into main so that
    // contributed claims written to the branch become searchable in main
    // after the merge without requiring a full recompile.
    let branch_data_dir = resolve_data_dir(root_path, Some(branch_name));
    let main_data_dir = resolve_data_dir(root_path, None);
    if branch_data_dir.join("vectors.bin").exists() {
        match (
            thinkingroot_graph::vector::VectorStore::init(&branch_data_dir).await,
            thinkingroot_graph::vector::VectorStore::init(&main_data_dir).await,
        ) {
            (Ok(branch_vec), Ok(mut main_vec)) => {
                let items = branch_vec.all_items();
                if !items.is_empty() {
                    match main_vec.upsert_raw_batch(items) {
                        Ok(n) => {
                            if let Err(e) = main_vec.save() {
                                tracing::warn!("merge vector save failed (non-fatal): {e}");
                            } else {
                                tracing::info!(
                                    "merge: reconciled {n} branch vector embeddings into main"
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

    // Mark branch as merged in registry
    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = BranchRegistry::load_or_create(&refs_dir)?;
    registry.mark_merged(branch_name, merged_by)?;

    Ok(())
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
