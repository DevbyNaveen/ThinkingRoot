// crates/thinkingroot-branch/src/merge.rs
use crate::branch::BranchRegistry;
use crate::lock::MergeLock;
use crate::recovery::{self, MergeIntent};
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

    // Carry branch-authored Root Functions into the target (deploys a new
    // version there; append-only, so target history is preserved).
    for diff_function in &diff.new_functions {
        target_graph.put_function(
            &diff_function.name,
            &diff_function.body,
            &diff_function.language,
        )?;
    }

    if propagate_deletions {
        use std::collections::HashSet;
        let source_uris: HashSet<String> = source_graph
            .get_all_sources()?
            .into_iter()
            .map(|(_, uri, _, _)| uri)
            .collect();
        let target_sources = target_graph.get_all_sources()?;
        for (_id, uri, source_type, _content_hash) in target_sources {
            let is_file_source = matches!(
                source_type.as_str(),
                "File" | "Document" | "Markdown" | "Code"
            );
            if is_file_source && !source_uris.contains(&uri) {
                let mut candidate_claims = Vec::new();
                let mut candidate_entities = HashSet::new();

                // Graph-query failures here are NOT non-fatal: a silent
                // empty list would skip the vector-index purge below, leaving
                // dangling embeddings for claims/entities we are about to
                // delete from the graph — the same silent-corruption mode
                // documented in the comment block ~40 lines below.  Propagate.
                for (sid, _, _) in target_graph.find_sources_by_uri(&uri)? {
                    candidate_claims.extend(target_graph.get_claim_ids_for_source(&sid)?);
                    candidate_entities.extend(target_graph.get_entity_ids_for_source(&sid)?);
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
                        // Existence-check failure must propagate: a silent
                        // skip leaves a dangling vector entry for an entity
                        // we are about to consider orphaned.
                        if target_graph.get_entity_by_id(&eid)?.is_none() {
                            vec_ids.push(format!("entity:{eid}"));
                        }
                    }

                    if !vec_ids.is_empty() {
                        // Vector-index errors during deletion propagation are NOT
                        // non-fatal: a merge that succeeds with stale embeddings
                        // silently corrupts hybrid retrieval and AEP probes for the
                        // affected claim ids.  The pre-merge snapshot at the top of
                        // this function is the recovery anchor — surface the failure
                        // so the caller can `root branch rollback` rather than ship a
                        // corrupt index.
                        let mut target_vec =
                            thinkingroot_graph::vector::VectorStore::init(&target_data_dir)
                                .await
                                .map_err(|e| {
                                    Error::VectorStorage(format!(
                                        "merge: failed to open target vector store for purge \
                                         after propagating deletion of '{uri}' from branch \
                                         '{source_branch_name}': {e} (run \
                                         `root branch rollback {source_branch_name}` to restore \
                                         pre-merge state)"
                                    ))
                                })?;
                        let id_refs: Vec<&str> = vec_ids.iter().map(|s| s.as_str()).collect();
                        target_vec.remove_by_ids(&id_refs);
                        target_vec.save().map_err(|e| {
                            Error::VectorStorage(format!(
                                "merge: failed to persist vector purge after deleting source \
                                 '{uri}' from target during merge of '{source_branch_name}': \
                                 {e} (run `root branch rollback {source_branch_name}` to \
                                 restore pre-merge state)"
                            ))
                        })?;
                    }
                }
            }
        }
    }

    target_graph.rebuild_entity_relations()?;

    // Vector reconciliation: any failure here means the target's vector
    // index is missing the branch's embeddings, which would cause hybrid
    // retrieval and AEP probes to silently miss those claims.  Promote
    // every failure to a hard error so the caller can roll back to the
    // pre-merge snapshot instead of shipping a corrupt index.  Skipped
    // entirely when the source has no `vectors.bin` (fresh branches).
    if source_data_dir.join("vectors.bin").exists() {
        let source_vec = thinkingroot_graph::vector::VectorStore::init(&source_data_dir)
            .await
            .map_err(|e| {
                Error::VectorStorage(format!(
                    "merge: failed to open source vector store for branch \
                     '{source_branch_name}': {e} (run `root branch rollback \
                     {source_branch_name}` to restore pre-merge state)"
                ))
            })?;
        let mut target_vec = thinkingroot_graph::vector::VectorStore::init(&target_data_dir)
            .await
            .map_err(|e| {
                Error::VectorStorage(format!(
                    "merge: failed to open target vector store while reconciling \
                     embeddings from branch '{source_branch_name}': {e} (run \
                     `root branch rollback {source_branch_name}` to restore pre-merge \
                     state)"
                ))
            })?;
        let items = source_vec.all_items();
        let item_count = items.len();
        if item_count > 0 {
            let n = target_vec.upsert_raw_batch(items).map_err(|e| {
                Error::VectorStorage(format!(
                    "merge: failed to upsert {item_count} branch embeddings into \
                     target during merge of '{source_branch_name}': {e} (run \
                     `root branch rollback {source_branch_name}` to restore pre-merge \
                     state)"
                ))
            })?;
            target_vec.save().map_err(|e| {
                Error::VectorStorage(format!(
                    "merge: failed to persist target vector store after upserting {n} \
                     branch embeddings during merge of '{source_branch_name}': {e} \
                     (run `root branch rollback {source_branch_name}` to restore \
                     pre-merge state)"
                ))
            })?;
            tracing::info!(
                "merge: reconciled {n} branch vector embeddings into '{}'",
                target_branch.unwrap_or("main")
            );
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
///
/// T0.6 gates layered on top of the existing health-score gate:
///
/// - **`MergePolicy::Ephemeral` source** — short-circuits to abandon.
///   Ephemeral branches never merge by definition; the registry is
///   updated to `Abandoned` and the disk path is left for `gc_branches`
///   to reclaim. Merge would be a contract violation, not a config
///   slip — `Error::MergeBlocked` carries the reason so callers can
///   surface it instead of silently swallowing the merge.
/// - **`MergePolicy::RequiresProposal` source** (T0.4) — the source
///   branch's policy demands an approved Knowledge Proposal before
///   merge.  We look up `find_approved_proposal(source, target)`; if
///   none exists, raw merges are rejected with a message pointing the
///   caller at `open_proposal`.  When an approved proposal is found
///   it is captured here and `mark_proposal_merged` is called after
///   the apply succeeds, keeping the proposal status honest with the
///   branch registry.
pub async fn execute_merge_into(
    root_path: &Path,
    source_branch_name: &str,
    target_branch: Option<&str>,
    diff: &KnowledgeDiff,
    merged_by: MergedBy,
    propagate_deletions: bool,
) -> Result<()> {
    execute_merge_into_with_options(
        root_path,
        source_branch_name,
        target_branch,
        diff,
        merged_by,
        propagate_deletions,
        false,
    )
    .await
}

/// Force-aware variant of [`execute_merge_into`].
///
/// `force=true` bypasses the T2.2 protected-branches gate (and matches
/// the existing `force=true` semantics on `compute_diff` for the
/// health-score gate).  Tag immutability is enforced separately at
/// `engine::ensure_branch_permission` and is NOT bypassed by `force`.
#[allow(clippy::too_many_arguments)]
pub async fn execute_merge_into_with_options(
    root_path: &Path,
    source_branch_name: &str,
    target_branch: Option<&str>,
    diff: &KnowledgeDiff,
    merged_by: MergedBy,
    propagate_deletions: bool,
    force: bool,
) -> Result<()> {
    execute_merge_into_cancellable(
        root_path,
        source_branch_name,
        target_branch,
        diff,
        merged_by,
        propagate_deletions,
        force,
        None,
    )
    .await
}

/// T1.5 — cancellable merge.
///
/// Same contract as [`execute_merge_into_with_options`], plus an
/// optional `cancel: CancellationToken` that is checked at every
/// phase boundary that has not yet committed durable target-graph
/// state:
///
/// 1. before the protected-branches gate,
/// 2. before the merge-policy gate,
/// 3. before writing the in-flight intent,
/// 4. before `apply_branch_diff` (the first mutation step), and
/// 5. between `apply_branch_diff` and `mark_merged_into` (the only
///    point where the intent file is still in place — recovery will
///    roll back from the pre-merge snapshot if we exit here).
///
/// Once `mark_merged_into` is reached the merge is durable; we
/// neither check nor honour the token after that point so a late
/// cancel cannot leave the registry inconsistent with the target
/// graph.
#[allow(clippy::too_many_arguments)]
pub async fn execute_merge_into_cancellable(
    root_path: &Path,
    source_branch_name: &str,
    target_branch: Option<&str>,
    diff: &KnowledgeDiff,
    merged_by: MergedBy,
    propagate_deletions: bool,
    force: bool,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<()> {
    macro_rules! check_cancel {
        () => {
            if let Some(t) = &cancel {
                if t.is_cancelled() {
                    return Err(Error::Cancelled);
                }
            }
        };
    }

    check_cancel!();

    if !diff.merge_allowed {
        return Err(Error::MergeBlocked(diff.blocking_reasons.join("; ")));
    }

    // T2.2 — protected-branches gate (default protects "main").  Run
    // BEFORE the merge-policy gate so the error is descriptive: a
    // protected target rejects the merge regardless of source policy.
    if !force {
        use thinkingroot_core::config::Config;
        if let Ok(config) = Config::load_merged(root_path) {
            let resolved_target = target_branch.unwrap_or("main");
            if config.merge.is_protected(resolved_target) {
                return Err(Error::MergeBlocked(format!(
                    "target branch '{resolved_target}' is protected by `merge.protected_branches` — \
                     pass force=true (and accept responsibility) or remove it from the config to merge"
                )));
            }
        }
    }

    check_cancel!();

    // T0.6 — read merge_policy off the source branch and gate.
    let refs_dir = root_path.join(".thinkingroot-refs");
    let registry_for_policy = BranchRegistry::load_or_create(&refs_dir)?;
    let mut authorising_proposal_id: Option<String> = None;
    if let Some(branch_ref) = registry_for_policy.get(source_branch_name) {
        if branch_ref.merge_policy.is_ephemeral() {
            // Ephemeral never merges — abandon and return without
            // touching the target graph. Mirrors the T0.6 "Stream
            // Sandbox Ephemeral default → discard on session end"
            // contract from `branch-system-improvements.md`.
            drop(registry_for_policy);
            let mut registry = BranchRegistry::load_or_create(&refs_dir)?;
            registry.abandon_branch(source_branch_name)?;
            return Err(Error::MergeBlocked(format!(
                "branch '{source_branch_name}' has MergePolicy::Ephemeral — abandoned instead of merged"
            )));
        }
        if branch_ref.merge_policy.requires_proposal() {
            // T0.4 — consult the proposal layer.  Approved proposal
            // for this exact (source, target) pair authorises the
            // merge; otherwise the gate rejects with a helpful
            // pointer to `open_proposal` so the caller can drive the
            // review flow.
            match thinkingroot_pr::find_approved_proposal(
                &refs_dir,
                source_branch_name,
                target_branch,
            )? {
                Some(proposal) => {
                    authorising_proposal_id = Some(proposal.id);
                }
                None => {
                    return Err(Error::MergeBlocked(format!(
                        "branch '{source_branch_name}' has MergePolicy::RequiresProposal — \
                         open a Knowledge Proposal via `thinkingroot_pr::open_proposal` and \
                         collect approvals before merging (no approved proposal found for \
                         source='{source_branch_name}', target={:?})",
                        target_branch
                    )));
                }
            }
        }
    }
    drop(registry_for_policy);

    check_cancel!();

    // T2.7 — record the in-flight intent BEFORE any graph mutation so
    // a crash mid-`apply_branch_diff` (or a hard process kill, or a
    // panic during vector reconciliation) leaves a recoverable record.
    // The intent is cleared only on the success path, after `mark_merged`
    // has updated the registry.  See `crate::recovery` for the recovery
    // pass that runs at workspace startup.
    let intent = MergeIntent {
        source_branch: source_branch_name.to_string(),
        target_branch: target_branch.map(|s| s.to_string()),
        started_at: chrono::Utc::now(),
        snapshot_subject: source_branch_name.to_string(),
        snapshot_prefix: "pre-merge".to_string(),
    };
    std::fs::create_dir_all(&refs_dir)?;
    recovery::write_merge_intent(&refs_dir, &intent)?;

    // `?` propagates apply failures with the intent file still in place —
    // the next `recover_orphan_merges` call will roll the target back from
    // the pre-merge snapshot.  The early-return is the recovery anchor.
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

    // Late-window cancel check — `apply_branch_diff` has finished but
    // the registry hasn't been updated yet.  Cancelling here leaves the
    // intent file in place; recovery will roll the target back from the
    // pre-merge snapshot on next workspace open.  Past this point we
    // do NOT honour the token: the registry write must complete to
    // keep on-disk state consistent with the post-apply target graph.
    check_cancel!();

    // Mark branch as merged in registry, then clear the intent.  Order
    // matters: if we clear the intent first and crash before
    // `mark_merged`, the registry would still show the branch as Active
    // and a re-run of the same merge would be allowed (idempotent in
    // theory, but expensive — best avoided).
    let mut registry = BranchRegistry::load_or_create(&refs_dir)?;
    registry.mark_merged_into(
        source_branch_name,
        merged_by,
        authorising_proposal_id.clone(),
    )?;

    // T0.4 — when a Knowledge Proposal authorised this merge, flip its
    // status to Merged so list_proposals reflects truth and a future
    // gate lookup doesn't see the same proposal as still Approved.
    // A failed mark_proposal_merged is logged but does not unwind the
    // merge — the registry already says merged, so the proposal
    // status drift is recoverable manually whereas unwinding the
    // merge here is not.
    if let Some(proposal_id) = &authorising_proposal_id {
        if let Err(e) = thinkingroot_pr::mark_proposal_merged(&refs_dir, proposal_id) {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "knowledge-pr: merge succeeded but mark_proposal_merged failed — \
                 proposal status will show stale `Approved` until manually fixed"
            );
        }
    }

    recovery::clear_merge_intent(&refs_dir, source_branch_name, intent.started_at)?;

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

    // T2.7 — same intent lifecycle as `execute_merge_into`.  Subject is
    // the rebase target (the branch being updated); prefix is "pre-rebase"
    // so recovery can disambiguate from merge snapshots.
    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let intent = MergeIntent {
        source_branch: parent_branch_name.to_string(),
        target_branch: Some(branch_name.to_string()),
        started_at: chrono::Utc::now(),
        snapshot_subject: branch_name.to_string(),
        snapshot_prefix: "pre-rebase".to_string(),
    };
    recovery::write_merge_intent(&refs_dir, &intent)?;

    // `?` propagates apply failures with the intent file in place —
    // recovery on next startup will restore the rebase target.
    apply_branch_diff(
        root_path,
        parent_branch_name,
        Some(branch_name),
        diff,
        false,
        "pre-rebase",
        branch_name,
    )
    .await?;

    recovery::clear_merge_intent(&refs_dir, parent_branch_name, intent.started_at)?;
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
