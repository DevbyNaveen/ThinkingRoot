// crates/thinkingroot-branch/src/lib.rs
pub mod branch;
pub mod diff;
pub mod lock;
pub mod merge;
pub mod recovery;
pub mod snapshot;
pub mod templates;

use std::path::Path;
use thinkingroot_core::{
    BranchKind, BranchPermissions, BranchRef, Config, MergePolicy, MergedBy, RedactionPolicy,
    Result,
};
use thinkingroot_graph::graph::GraphStore;

/// Create a new knowledge branch from a parent branch (default: main).
///
/// - Copies `{parent_data_dir}/graph/graph.db` to the new branch dir
/// - Symlinks `models/` and `cache/` from parent (avoids duplicating ~300MB)
/// - Registers the branch in `.thinkingroot-refs/branches.toml`
/// - Branch data dir lives at `.thinkingroot/branches/{slug}/`
pub async fn create_branch(
    root_path: &Path,
    name: &str,
    parent: &str,
    description: Option<String>,
) -> Result<BranchRef> {
    create_branch_with_owner(
        root_path,
        name,
        parent,
        description,
        None,
        BranchPermissions::default(),
    )
    .await
}

/// Create a new branch with an explicit owner and permissions.
///
/// `kind` defaults to [`BranchKind::Feature`] and `merge_policy`
/// defaults to [`MergePolicy::Manual`]. For non-default kinds (e.g.
/// `Stream` branches created by `mcp/mod.rs::ensure_session_branch`)
/// use [`create_branch_full`].
pub async fn create_branch_with_owner(
    root_path: &Path,
    name: &str,
    parent: &str,
    description: Option<String>,
    owner: Option<String>,
    permissions: BranchPermissions,
) -> Result<BranchRef> {
    create_branch_full(
        root_path,
        name,
        parent,
        description,
        owner,
        permissions,
        BranchKind::default(),
        MergePolicy::default(),
        None,
    )
    .await
}

/// Create a new branch with the full T0.6 attribute set
/// (kind + merge policy) plus the T2.6 redaction policy AND the T0.5
/// `parent_commit_hash` LCA pointer.
///
/// Sets `BranchRef::parent_commit_hash` to the BLAKE3 of the parent's
/// `graph.db` at fork time.  Combined with the immutable
/// `graph.db.parent-at-fork` snapshot (saved by
/// [`snapshot::create_branch_layout`]), this gives `compute_three_way_diff`
/// the LCA it needs to surface real conflicts where two-way merge
/// would silently last-writer-win.
#[allow(clippy::too_many_arguments)]
pub async fn create_branch_full(
    root_path: &Path,
    name: &str,
    parent: &str,
    description: Option<String>,
    owner: Option<String>,
    permissions: BranchPermissions,
    kind: BranchKind,
    merge_policy: MergePolicy,
    redaction: Option<RedactionPolicy>,
) -> Result<BranchRef> {
    let parent_data_dir = snapshot::resolve_data_dir(root_path, Some(parent));
    let branch_data_dir = snapshot::resolve_data_dir(root_path, Some(name));

    snapshot::create_branch_layout(&parent_data_dir, &branch_data_dir)?;

    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    let mut branch_ref = registry.create_branch_full(
        name,
        parent,
        description,
        owner,
        permissions,
        kind,
        merge_policy,
        redaction,
    )?;

    // T0.5 — record the LCA pointer.  We hash the parent's graph.db
    // (which is what we just copied to graph.db.parent-at-fork) so a
    // future merge can verify the snapshot still represents the same
    // bytes.  A missing parent graph.db (fresh workspace, parent was
    // empty) is recorded as None — the three-way merge gate falls
    // back to two-way for those.
    let parent_db = parent_data_dir.join("graph").join("graph.db");
    if parent_db.exists() {
        let bytes = std::fs::read(&parent_db)
            .map_err(|e| thinkingroot_core::Error::io_path(&parent_db, e))?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        registry.set_parent_commit_hash(name, hash.clone())?;
        branch_ref.parent_commit_hash = Some(hash);
    }

    Ok(branch_ref)
}

/// Create an immutable tag pointing at a target commit (T2.5).
///
/// Tags are first-class branches with [`BranchKind::Tag`] so the
/// existing immutability gate at
/// `thinkingroot-serve::engine::ensure_branch_permission` rejects any
/// write/merge/rebase/delete attempt against them — even by the
/// owner.  The `target` is typically a BLAKE3 commit hash
/// (matches the format `parent_commit_hash` uses), but any opaque
/// 1..=128-char identifier is accepted; the engine never tries to
/// resolve it back to substrate state, so tag semantics stay decoupled
/// from any future commit log.
///
/// Returns `Error::BranchAlreadyExists` if a branch / tag with the
/// same name already exists.
pub fn create_tag(
    root_path: &Path,
    name: &str,
    ref_name: &str,
    target: &str,
    owner: Option<String>,
    description: Option<String>,
) -> Result<BranchRef> {
    if name.trim().is_empty() {
        return Err(thinkingroot_core::Error::Config(
            "tag name must not be empty".into(),
        ));
    }
    if ref_name.trim().is_empty() {
        return Err(thinkingroot_core::Error::Config(
            "tag ref_name must not be empty".into(),
        ));
    }
    if target.is_empty() || target.len() > 128 {
        return Err(thinkingroot_core::Error::Config(
            "tag target must be 1..=128 chars".into(),
        ));
    }
    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    // Tags fork conceptually from `main` (so the lineage DAG draws an
    // edge `main → tag`) but they share no substrate with main —
    // `create_branch_full` here is registry-only; we do NOT call
    // `snapshot::create_branch_layout` because Tags are name pins, not
    // substrate forks.
    registry.create_branch_full(
        name,
        "main",
        description,
        owner,
        BranchPermissions::default(),
        BranchKind::Tag {
            ref_name: ref_name.to_string(),
            target: target.to_string(),
        },
        // Tags never merge — Manual is the safe default; the immutability
        // gate hard-rejects merge attempts regardless of policy, so this
        // is just for the registry payload.
        MergePolicy::Manual,
        None,
    )
}

/// List every active [`BranchKind::Tag`] in the workspace.  Order is
/// the underlying `branches.toml` order (insertion-time on most
/// workspaces).
pub fn list_tags(root_path: &Path) -> Result<Vec<BranchRef>> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    if !refs_dir.exists() {
        return Ok(vec![]);
    }
    let registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    Ok(registry
        .list_active()
        .into_iter()
        .filter(|b| matches!(b.kind, BranchKind::Tag { .. }))
        .cloned()
        .collect())
}

/// Set or clear the T2.3 TTL on an existing branch.
///
/// `Some(secs)` opts the branch into auto-abandon by the maintenance
/// pass after `secs` seconds since `created_at`.  `None` clears the
/// TTL.  Tag branches refuse this — they're immutable.
pub fn set_branch_max_age_secs(
    root_path: &Path,
    name: &str,
    max_age_secs: Option<u64>,
) -> Result<BranchRef> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    if let Some(b) = registry.get(name)
        && matches!(b.kind, BranchKind::Tag { .. })
    {
        return Err(thinkingroot_core::Error::PermissionDenied {
            actor: "system".into(),
            action: format!("set TTL on tag '{name}' (tags are immutable)"),
        });
    }
    registry.set_max_age_secs(name, max_age_secs)
}

/// Update the redaction policy on an existing branch and persist.
pub fn set_branch_redaction(
    root_path: &Path,
    name: &str,
    policy: Option<RedactionPolicy>,
) -> Result<BranchRef> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    registry.set_redaction(name, policy)
}

/// List all active branches for a workspace.
pub fn list_branches(root_path: &Path) -> Result<Vec<BranchRef>> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    if !refs_dir.exists() {
        return Ok(vec![]);
    }
    let registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    Ok(registry.list_active().into_iter().cloned().collect())
}

/// Read the active HEAD branch name. Returns "main" if no HEAD exists.
pub fn read_head_branch(root_path: &Path) -> Result<String> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    branch::read_head(&refs_dir)
}

/// Write the active HEAD branch name.
pub fn write_head_branch(root_path: &Path, branch_name: &str) -> Result<()> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    branch::write_head(&refs_dir, branch_name)
}

/// Soft-delete a branch (mark as Abandoned, data dir kept).
pub fn delete_branch(root_path: &Path, name: &str) -> Result<()> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    registry.abandon_branch(name)
}

/// Hard-delete a branch: mark as Abandoned AND remove its `.thinkingroot/branches/{slug}/` directory.
///
/// Use `delete_branch` for soft delete (keeps data dir). Use this when you want
/// to reclaim disk space and are sure you no longer need the branch data.
pub fn purge_branch(root_path: &Path, name: &str) -> Result<()> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    registry.abandon_branch(name)?;
    let data_dir = snapshot::resolve_data_dir(root_path, Some(name));
    if data_dir.exists() {
        std::fs::remove_dir_all(&data_dir)?;
    }
    Ok(())
}

/// Garbage-collect: purge all branches currently in Abandoned state.
///
/// Removes their data directories and leaves only the registry tombstone entries
/// so history is preserved.
pub fn gc_branches(root_path: &Path) -> Result<usize> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    if !refs_dir.exists() {
        return Ok(0);
    }
    let registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    let abandoned: Vec<String> = registry
        .list_abandoned()
        .into_iter()
        .map(|b| b.name.clone())
        .collect();
    let count = abandoned.len();
    for name in &abandoned {
        let data_dir = snapshot::resolve_data_dir(root_path, Some(name));
        if data_dir.exists() {
            std::fs::remove_dir_all(&data_dir)?;
        }
    }
    Ok(count)
}

/// Roll back a previously executed merge by restoring the pre-merge graph snapshot.
pub fn rollback_merge(root_path: &Path, branch_name: &str) -> Result<()> {
    merge::rollback_merge(root_path, branch_name)
}

/// Merge `source_branch` into `target_branch`.
///
/// When the source branch has a T0.5 parent-at-fork snapshot on disk
/// (the immutable LCA copy created by `create_branch_layout`), the
/// merge dispatches to [`diff::compute_three_way_diff`] which surfaces
/// real conflicts where two-way diff would silently last-writer-win.
/// Branches predating T0.5 fall back to the historical
/// [`diff::compute_diff_into`] path — preserves the existing
/// behaviour for every workspace that has not yet recreated its
/// branches.
pub async fn merge_into(
    root_path: &Path,
    source_branch: &str,
    target_branch: &str,
    merged_by: MergedBy,
    force: bool,
    propagate_deletions: bool,
) -> Result<thinkingroot_core::KnowledgeDiff> {
    merge_into_cancellable(
        root_path,
        source_branch,
        target_branch,
        merged_by,
        force,
        propagate_deletions,
        None,
    )
    .await
}

/// T1.5 — cancellable variant of [`merge_into`].
///
/// Computes the diff first (non-mutating), then runs the merge under
/// the supplied [`CancellationToken`].  See
/// [`merge::execute_merge_into_cancellable`] for the exact phase
/// boundaries at which the token is honoured — once the registry
/// write begins the token is intentionally ignored so on-disk state
/// stays consistent.
///
/// Pass `None` to opt out (matches [`merge_into`] semantics).
#[allow(clippy::too_many_arguments)]
pub async fn merge_into_cancellable(
    root_path: &Path,
    source_branch: &str,
    target_branch: &str,
    merged_by: MergedBy,
    force: bool,
    propagate_deletions: bool,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<thinkingroot_core::KnowledgeDiff> {
    let config = Config::load_merged(root_path)?;
    let merge_cfg = &config.merge;
    let source_data_dir = snapshot::resolve_data_dir(root_path, Some(source_branch));
    let target_data_dir = snapshot::resolve_data_dir(root_path, Some(target_branch));
    let target_graph = GraphStore::init(&target_data_dir.join("graph"))?;
    let source_graph = GraphStore::init(&source_data_dir.join("graph"))?;

    // T0.5 — dispatch to three-way diff when the parent-at-fork
    // snapshot is present.  `parent_at_fork_dir` returns
    // `<source>/graph/parent-at-fork`; that directory contains a
    // graph.db copy of the LCA at fork time.  Existence check skips
    // the dispatch cleanly for legacy branches.
    let lca_dir = snapshot::parent_at_fork_dir(&source_data_dir);
    let mut diff = if lca_dir.join("graph.db").exists() {
        let base_graph = GraphStore::init(&lca_dir)?;
        diff::compute_three_way_diff(
            &base_graph,
            &target_graph,
            &source_graph,
            source_branch,
            Some(target_branch),
            merge_cfg.auto_resolve_threshold,
            merge_cfg.max_health_drop,
            merge_cfg.block_on_contradictions,
        )?
    } else {
        // T1.1 — async wrapper opens both branches' vector stores and
        // runs the embedding-cosine contradiction pass on top of the
        // two-way diff.  Falls back to a vector-free diff inside the
        // wrapper when either side is missing `vectors.bin`.
        diff::compute_diff_into_with_vector_dirs(
            &target_graph,
            &source_graph,
            &target_data_dir,
            &source_data_dir,
            source_branch,
            Some(target_branch),
            merge_cfg.auto_resolve_threshold,
            merge_cfg.max_health_drop,
            merge_cfg.block_on_contradictions,
        )
        .await?
    };
    if force {
        diff.merge_allowed = true;
        diff.blocking_reasons.clear();
    }
    merge::execute_merge_into_cancellable(
        root_path,
        source_branch,
        Some(target_branch),
        &diff,
        merged_by,
        propagate_deletions,
        false,
        cancel,
    )
    .await?;
    Ok(diff)
}

/// T1.5 — compute the merge diff WITHOUT executing the merge.
///
/// Returns the same `KnowledgeDiff` that [`merge_into`] would have
/// fed to `execute_merge_into`, but never touches the target graph,
/// the registry, or the intent file.  Used by the dry-run merge REST
/// path so callers can preview the diff (claim/entity counts,
/// auto-resolved set, needs-review conflicts, health gates) before
/// committing.
///
/// Honour-`force`: same semantics as [`merge_into`] — when `force`
/// is true the gate flags are flipped on the returned diff so the
/// caller sees what the merge would have allowed if forced.
pub async fn dry_run_merge_into(
    root_path: &Path,
    source_branch: &str,
    target_branch: &str,
    force: bool,
) -> Result<thinkingroot_core::KnowledgeDiff> {
    let config = Config::load_merged(root_path)?;
    let merge_cfg = &config.merge;
    let source_data_dir = snapshot::resolve_data_dir(root_path, Some(source_branch));
    let target_data_dir = snapshot::resolve_data_dir(root_path, Some(target_branch));
    let target_graph = GraphStore::init(&target_data_dir.join("graph"))?;
    let source_graph = GraphStore::init(&source_data_dir.join("graph"))?;

    let lca_dir = snapshot::parent_at_fork_dir(&source_data_dir);
    let mut diff = if lca_dir.join("graph.db").exists() {
        let base_graph = GraphStore::init(&lca_dir)?;
        diff::compute_three_way_diff(
            &base_graph,
            &target_graph,
            &source_graph,
            source_branch,
            Some(target_branch),
            merge_cfg.auto_resolve_threshold,
            merge_cfg.max_health_drop,
            merge_cfg.block_on_contradictions,
        )?
    } else {
        diff::compute_diff_into_with_vector_dirs(
            &target_graph,
            &source_graph,
            &target_data_dir,
            &source_data_dir,
            source_branch,
            Some(target_branch),
            merge_cfg.auto_resolve_threshold,
            merge_cfg.max_health_drop,
            merge_cfg.block_on_contradictions,
        )
        .await?
    };
    if force {
        diff.merge_allowed = true;
        diff.blocking_reasons.clear();
    }
    Ok(diff)
}

/// Rebase `name` by pulling new claims from its parent branch into the branch.
pub async fn rebase_branch(
    root_path: &Path,
    name: &str,
) -> Result<thinkingroot_core::KnowledgeDiff> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    let registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    let branch_ref = registry
        .get(name)
        .ok_or_else(|| thinkingroot_core::Error::BranchNotFound(name.to_string()))?;

    let config = Config::load_merged(root_path)?;
    let merge_cfg = &config.merge;
    let branch_data_dir = snapshot::resolve_data_dir(root_path, Some(name));
    let parent_data_dir = snapshot::resolve_data_dir(root_path, Some(&branch_ref.parent));
    let branch_graph = GraphStore::init(&branch_data_dir.join("graph"))?;
    let parent_graph = GraphStore::init(&parent_data_dir.join("graph"))?;
    let diff = diff::compute_rebase_diff(
        &branch_graph,
        &parent_graph,
        name,
        &branch_ref.parent,
        merge_cfg.auto_resolve_threshold,
        merge_cfg.max_health_drop,
        merge_cfg.block_on_contradictions,
    )?;
    merge::execute_rebase(root_path, name, &branch_ref.parent, &diff).await?;
    Ok(diff)
}

/// Migrate legacy branch directories from the old sibling layout to the new nested layout.
///
/// Old: `{root}/.thinkingroot-{slug}/`
/// New: `{root}/.thinkingroot/branches/{slug}/`
///
/// Safe to call on every startup — idempotent, skips already-migrated branches.
/// Returns the number of directories migrated.
pub fn migrate_legacy_layout(root_path: &Path) -> Result<usize> {
    snapshot::migrate_legacy_layout(root_path)
}

/// Recover any merges interrupted by a crash before they could update
/// the registry.  Reads `<root>/.thinkingroot-refs/merges_in_flight.toml`
/// and rolls each in-flight target back from its pre-merge snapshot.
///
/// Idempotent + safe to call from every startup path; on a clean
/// workspace it stats one file and returns.  See [`recovery`] for the
/// underlying state machine and [`recovery::RecoveryReport`] for the
/// caller-visible outcome.
pub fn recover_orphan_merges(root_path: &Path) -> Result<recovery::RecoveryReport> {
    recovery::recover_orphan_merges(root_path)
}
