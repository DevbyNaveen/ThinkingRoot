//! Migration tests for the v2 → v3 (water-flow) compile schema bump.
//!
//! Covers:
//! - Orphan structural row purge after source deletion without cascade.
//! - Idempotency of re-running the migration.
//! - Dangling `callee_claim_id` reset for Phase 7e pointers.
//! - Auto-trigger behaviour on schema-version mismatch.
//! - Explicit `backfill_water_flow_v3_at_path` API.
//! - `resolution_deps` backfill from current resolved edges (T5).

use std::collections::BTreeMap;

use cozo::{DataValue, ScriptMutability};
use tempfile::tempdir;
use thinkingroot_core::types::{ContentHash, SourceType};
use thinkingroot_core::Source;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::rows::FunctionCall;
use thinkingroot_serve::backfill::backfill_water_flow_v3;

fn make_store() -> (tempfile::TempDir, GraphStore) {
    let dir = tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let store = GraphStore::init(&path).unwrap();
    (dir, store)
}

/// Insert a `function_calls` row pointing at a source_id that does NOT exist
/// in the `sources` table, then verify the migration purges it and bumps the
/// schema version.
#[test]
fn migration_purges_orphan_structural_rows() {
    let (_dir, store) = make_store();

    let row = FunctionCall {
        id: "fc-orphan-mig".to_string(),
        caller_claim_id: "caller-orphan-mig".to_string(),
        callee_name: "ghost_fn".to_string(),
        callee_claim_id: String::new(),
        source_id: "ghost-source-mig".to_string(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-orphan-mig".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    let pre = store.query_orphan_structural_rows().unwrap();
    assert_eq!(
        pre.len(),
        1,
        "expected 1 orphan pre-migration, got {}",
        pre.len()
    );

    backfill_water_flow_v3(&store).unwrap();

    let post = store.query_orphan_structural_rows().unwrap();
    assert!(
        post.is_empty(),
        "expected no orphans post-migration, got {post:?}"
    );

    let v = store.get_workspace_meta("compile_schema_version").unwrap();
    assert_eq!(v.as_deref(), Some("3"));
}

/// Running `backfill_water_flow_v3` twice on the same store must be a no-op
/// on the second call and must leave `compile_schema_version = "3"`.
#[test]
fn migration_is_idempotent_on_re_run() {
    let (_dir, store) = make_store();
    backfill_water_flow_v3(&store).unwrap();
    backfill_water_flow_v3(&store).unwrap();
    let v = store.get_workspace_meta("compile_schema_version").unwrap();
    assert_eq!(v.as_deref(), Some("3"));
}

/// A `function_calls` row whose `callee_claim_id` points to a claim that no
/// longer exists should have its `callee_claim_id` reset to `""` (external)
/// by the migration.
#[test]
fn migration_re_resets_dangling_callee_claim_ids() {
    let (_dir, store) = make_store();

    // Insert a real source so the function_call row is NOT an orphan
    // (it should survive the orphan purge and only be modified by step 2).
    let source = Source::new("test://has-fn.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-fn".into()));
    let source_id = source.id.to_string();
    store.insert_source(&source).unwrap();

    let row = FunctionCall {
        id: "fc-dangling".to_string(),
        caller_claim_id: "caller-dangling".to_string(),
        callee_name: "missing_fn".to_string(),
        // Points at a claim_id that was never inserted — dangling pointer.
        callee_claim_id: "deleted-claim-id".to_string(),
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-d".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    backfill_water_flow_v3(&store).unwrap();

    let mut params = BTreeMap::new();
    params.insert("id".into(), DataValue::Str("fc-dangling".into()));
    let result = store
        .raw_db()
        .run_script(
            "?[callee_claim_id] := *function_calls{id: $id, callee_claim_id}",
            params,
            ScriptMutability::Immutable,
        )
        .unwrap();
    let resolved = match result.rows.first().and_then(|r| r.first()) {
        Some(DataValue::Str(s)) => s.to_string(),
        _ => String::new(),
    };
    assert_eq!(
        resolved, "",
        "dangling callee_claim_id should be reset to empty string (external)"
    );
}

/// When `compile_schema_version` is set to `"2"` (pre-water-flow), the
/// migration must still bump it to `"3"`.
#[test]
fn migration_auto_triggers_on_compile_schema_version_mismatch() {
    let dir = tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let store = GraphStore::init(&path).unwrap();
    store
        .set_workspace_meta("compile_schema_version", "2")
        .unwrap();
    backfill_water_flow_v3(&store).unwrap();
    let v = store.get_workspace_meta("compile_schema_version").unwrap();
    assert_eq!(v.as_deref(), Some("3"));
}

/// `backfill_water_flow_v3_at_path` opens the store, migrates, and closes it.
/// A freshly-opened store afterwards must report `compile_schema_version = "3"`.
#[test]
fn explicit_root_migrate_runs_same_logic() {
    let dir = tempdir().unwrap();
    let path = dir.path().to_path_buf();
    {
        // Create the workspace (runs schema migrations but leaves version unset).
        let _store = GraphStore::init(&path).unwrap();
    }
    thinkingroot_serve::backfill::backfill_water_flow_v3_at_path(&path).unwrap();
    let store2 = GraphStore::init(&path).unwrap();
    let v = store2
        .get_workspace_meta("compile_schema_version")
        .unwrap();
    assert_eq!(v.as_deref(), Some("3"));
}

/// T5: the migration must backfill `resolution_deps` from existing resolved
/// function_calls so that Phase 4's dirty-source collection works on the first
/// incremental compile after migration, without requiring a full re-compile.
#[test]
fn migration_builds_resolution_deps_from_current_resolved_edges() {
    let (_dir, store) = make_store();

    // Source A: home of the callee.
    let src_a = thinkingroot_core::Source::new(
        "test://ma.rs".into(),
        thinkingroot_core::types::SourceType::File,
    )
    .with_hash(thinkingroot_core::types::ContentHash("hash-ma-mig".into()));
    let src_a_id = src_a.id.to_string();
    store.insert_source(&src_a).unwrap();

    // Source B: caller.
    let src_b = thinkingroot_core::Source::new(
        "test://mb.rs".into(),
        thinkingroot_core::types::SourceType::File,
    )
    .with_hash(thinkingroot_core::types::ContentHash("hash-mb-mig".into()));
    let src_b_id = src_b.id.to_string();
    store.insert_source(&src_b).unwrap();

    // Claim in A.
    let claim_a = thinkingroot_core::Claim::new(
        "fn t() {}",
        thinkingroot_core::types::ClaimType::Fact,
        src_a.id,
        thinkingroot_core::types::WorkspaceId::new(),
    );
    let claim_a_id = claim_a.id.to_string();
    store.insert_claim(&claim_a).unwrap();

    // A function_calls row in B already resolved to A's claim.
    let row = thinkingroot_graph::rows::FunctionCall {
        id: "fc-mig-1".to_string(),
        caller_claim_id: "caller-mig".to_string(),
        callee_name: "t".to_string(),
        callee_claim_id: claim_a_id,
        source_id: src_b_id.clone(),
        byte_start: 0,
        byte_end: 8,
        content_blake3: "blake-mig".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    // Run the migration — step 3 must build B → A in resolution_deps.
    backfill_water_flow_v3(&store).unwrap();

    let deps = store.list_dependent_sources(&src_a_id).unwrap();
    assert!(
        deps.contains(&src_b_id),
        "migration should have built B → A dep in resolution_deps; got: {deps:?}"
    );
}
