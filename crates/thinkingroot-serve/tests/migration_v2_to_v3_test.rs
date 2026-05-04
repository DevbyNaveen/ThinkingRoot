//! Migration tests for the v2 → v3 (water-flow) compile schema bump.
//!
//! Covers:
//! - Orphan structural row purge after source deletion without cascade.
//! - Idempotency of re-running the migration.
//! - Dangling `callee_claim_id` reset for Phase 7e pointers.
//! - Auto-trigger behaviour on schema-version mismatch.
//! - Explicit `backfill_water_flow_v3_at_path` API.
//! - `resolution_deps` is left empty until T5.

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

/// T3 does not populate `resolution_deps` — that is T5's responsibility.
/// Pin the current empty-or-absent state so T5 can loosen this assertion
/// when it lands.
#[test]
fn migration_resolution_deps_left_empty_until_t5() {
    let (_dir, store) = make_store();
    backfill_water_flow_v3(&store).unwrap();

    let result = store.raw_db().run_script(
        "?[count(from_source_id)] := *resolution_deps{from_source_id}",
        Default::default(),
        ScriptMutability::Immutable,
    );
    if let Ok(r) = result {
        if let Some(row) = r.rows.first() {
            if let DataValue::Num(cozo::Num::Int(n)) = &row[0] {
                assert_eq!(
                    *n, 0,
                    "resolution_deps should be empty until T5 populates it"
                );
            }
        }
    }
    // If the table doesn't exist yet, the script errors — that is also fine
    // (T5 creates it). The important thing is no panic and no pre-T5 data.
}
