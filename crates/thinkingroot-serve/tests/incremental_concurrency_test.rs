//! Snapshot-consistency tests for water-flow incremental compile (T7).
//! The first test probes Cozo's multi-statement atomicity to choose the
//! correct rebuild strategy.  The remaining tests assert on the chosen
//! strategy: a `multi_transaction` opens, the cascade :rm + per-table
//! :put run inside it, and the whole thing commits or aborts as one
//! atomic boundary.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use cozo::{DataValue, ScriptMutability};
use tempfile::TempDir;
use thinkingroot_graph::graph::{GraphStore, PerSourceRows};
use thinkingroot_graph::rows::FunctionCall;

fn make_store() -> (TempDir, GraphStore) {
    let dir = tempfile::tempdir().unwrap();
    // GraphStore::init creates the schema and runs migrations on the
    // SQLite-backed Cozo instance under `dir.path()/graph.db`.
    let store = GraphStore::init(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn cozo_multi_transaction_rolls_back_on_failure() {
    // Probe: does Cozo's `multi_transaction` API correctly roll back
    // earlier writes when a later `run_script` call fails (followed by
    // `abort()`)?  If yes, T7 can build per-source rebuild on top of
    // `multi_transaction`: cascade :rm + per-table :put as separate
    // run_script calls, abort on first error.
    //
    // Note: a single `db.run_script` with `;`-separated statements
    // _does not_ work for our case — the Cozo grammar parses
    // consecutive `?[…] := …; ?[…] := …` as one rule with conflicting
    // heads ("Rule ? has multiple definitions with conflicting heads").
    // The `multi_transaction` API is the supported way to atomically
    // bundle multiple writes.
    let (_dir, store) = make_store();

    let pre = store
        .raw_db()
        .run_script(
            "?[count(id)] := *function_calls{id}",
            Default::default(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    let pre_count = match &pre.rows[0][0] {
        DataValue::Num(cozo::Num::Int(n)) => *n,
        _ => -1,
    };
    assert_eq!(pre_count, 0);

    // Open a multi-statement write transaction.
    let tx = store.raw_db().multi_transaction(true);

    // Statement 1: insert one row — should succeed inside the tx.
    let mut p1: BTreeMap<String, DataValue> = BTreeMap::new();
    p1.insert("id".into(), DataValue::Str("probe-1".into()));
    let r1 = tx.run_script(
        r#"?[id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3]
            <- [[$id, '', 'probe', '', '', 0, 0, '']]
        :put function_calls {id => caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3}"#,
        p1,
    );
    assert!(r1.is_ok(), "first statement must succeed inside the tx");

    // Statement 2: deliberately invalid — references a table that does
    // not exist.  Should fail.
    let mut p2: BTreeMap<String, DataValue> = BTreeMap::new();
    p2.insert("id".into(), DataValue::Str("probe-bad".into()));
    let r2 = tx.run_script(
        r#"?[bad_column] <- [[$id]] :put nonexistent_table {bad_column}"#,
        p2,
    );
    assert!(r2.is_err(), "second statement (nonexistent table) must fail");

    // Abort the multi-transaction — the row inserted by statement 1
    // must NOT survive.
    tx.abort().unwrap();

    let post = store
        .raw_db()
        .run_script(
            "?[count(id)] := *function_calls{id}",
            Default::default(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    let post_count = match &post.rows[0][0] {
        DataValue::Num(cozo::Num::Int(n)) => *n,
        _ => -1,
    };
    eprintln!("[T7 probe] post_count={post_count} (0 = atomic, 1 = NOT atomic)");
    assert_eq!(
        post_count, 0,
        "Cozo multi_transaction must roll back uncommitted writes on abort",
    );
}

/// Read `function_calls` count for `source_id` via raw Cozo.  Used by
/// the concurrent-reader test below — emits one read per spin without
/// holding any Rust-side state across iterations.
fn count_fc_for_source(store: &GraphStore, source_id: &str) -> i64 {
    let mut params: BTreeMap<String, DataValue> = BTreeMap::new();
    params.insert("sid".into(), DataValue::Str(source_id.into()));
    let result = store
        .raw_db()
        .run_script(
            "?[count(id)] := *function_calls{id, source_id: $sid}",
            params,
            ScriptMutability::Immutable,
        )
        .expect("read query");
    if let Some(row) = result.rows.first()
        && let DataValue::Num(cozo::Num::Int(n)) = &row[0]
    {
        return *n;
    }
    -1
}

#[test]
fn concurrent_reader_during_rebuild_sees_consistent_state() {
    // Snapshot consistency assertion: while one thread is doing a
    // transactional_rebuild_source, another thread spins reading the
    // function_calls count for that source.  Every observation must
    // be either the pre-state count (100) or the post-state count
    // (50) — never any intermediate value, because the cascade-then-put
    // is one atomic multi_transaction.
    let (_dir, store_owned) = make_store();
    let store = Arc::new(store_owned);

    let source_id = "concur-source-1".to_string();

    // Seed 100 function_calls for source.
    let mut initial = Vec::with_capacity(100);
    for k in 0..100 {
        initial.push(FunctionCall {
            id: format!("fc-init-{k}"),
            caller_claim_id: "c".to_string(),
            callee_name: format!("n-{k}"),
            callee_claim_id: String::new(),
            source_id: source_id.clone(),
            byte_start: (k as u64) * 10,
            byte_end: (k as u64) * 10 + 5,
            content_blake3: format!("b-{k}"),
        });
    }
    store.insert_function_calls_batch(&initial).unwrap();
    assert_eq!(count_fc_for_source(&store, &source_id), 100);

    // Reader thread: spin 200 iterations reading the count.
    let reader_store = Arc::clone(&store);
    let reader_sid = source_id.clone();
    let reader = thread::spawn(move || {
        let mut observations: Vec<i64> = Vec::with_capacity(200);
        for _ in 0..200 {
            observations.push(count_fc_for_source(&reader_store, &reader_sid));
            std::thread::yield_now();
        }
        observations
    });

    // Writer: rebuild with 50 new function_calls.
    let mut new_rows = PerSourceRows::default();
    for k in 0..50 {
        new_rows.function_calls.push(FunctionCall {
            id: format!("fc-new-{k}"),
            caller_claim_id: "c".to_string(),
            callee_name: format!("nn-{k}"),
            callee_claim_id: String::new(),
            source_id: source_id.clone(),
            byte_start: (k as u64) * 20,
            byte_end: (k as u64) * 20 + 5,
            content_blake3: format!("nb-{k}"),
        });
    }
    store
        .transactional_rebuild_source(&source_id, &new_rows)
        .unwrap();

    let observations = reader.join().expect("reader thread joined");
    for o in &observations {
        assert!(
            *o == 100 || *o == 50,
            "observed torn count: {o} (allowed: 100 (pre) or 50 (post))",
        );
    }
    // Sanity: post-rebuild count is 50.
    assert_eq!(count_fc_for_source(&store, &source_id), 50);
}

#[test]
fn transactional_rebuild_partial_failure_rolls_back_completely() {
    // If the rebuild encounters a constraint violation mid-transaction,
    // NO rows from the partial write should remain — the cascade :rm
    // got rolled back alongside the failed :put.  Concretely: pre-state
    // had 1 row, the new rows contain a duplicate primary key in the
    // same put-block (which we expect Cozo to reject because :put
    // requires unique keys per row in one batch).  After failure, the
    // pre-state row must still be there.
    let (_dir, store) = make_store();
    let source_id = "rb-fail-source".to_string();

    let initial = vec![FunctionCall {
        id: "fc-pre".to_string(),
        caller_claim_id: "c".to_string(),
        callee_name: "pre".to_string(),
        callee_claim_id: String::new(),
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 5,
        content_blake3: "b-pre".to_string(),
    }];
    store.insert_function_calls_batch(&initial).unwrap();
    assert_eq!(count_fc_for_source(&store, &source_id), 1);

    // Build a PerSourceRows that should fail :put — duplicate id "fc-dup"
    // in the same batch.  Cozo treats the entry rule as a set, so the
    // duplicate may collapse to one row in some grammar paths; failing
    // that, the test still pins "no torn intermediate".  The contract
    // we assert is: either rebuild succeeded with NO duplicate row left
    // OR rebuild failed and the pre-state survives.
    let mut rows = PerSourceRows::default();
    rows.function_calls.push(FunctionCall {
        id: "fc-dup".to_string(),
        caller_claim_id: "c".to_string(),
        callee_name: "a".to_string(),
        callee_claim_id: String::new(),
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 5,
        content_blake3: "b1".to_string(),
    });
    rows.function_calls.push(FunctionCall {
        id: "fc-dup".to_string(), // duplicate id
        caller_claim_id: "c".to_string(),
        callee_name: "b".to_string(),
        callee_claim_id: String::new(),
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 5,
        content_blake3: "b2".to_string(),
    });

    let result = store.transactional_rebuild_source(&source_id, &rows);
    let count = count_fc_for_source(&store, &source_id);

    if result.is_err() {
        // Rebuild failed → cascade rolled back, pre-state intact.  Post
        // count must be 1 (the pre-existing row).  A count of 0 here
        // would mean the cascade :rm committed but the :put was rolled
        // back — exactly the torn intermediate I-W4 forbids.
        assert_eq!(
            count, 1,
            "rebuild failed → pre-state must survive (rollback); got count={count}",
        );
    } else {
        // Rebuild succeeded → Cozo collapsed the duplicate keys.  No
        // torn row should remain; the count is exactly the unique-key
        // count of the put block (which is 1 because both rows shared
        // an id).
        assert_eq!(
            count, 1,
            "rebuild succeeded → final count is 1 (deduplicated); got count={count}",
        );
    }
}

#[test]
fn rebuild_with_unrelated_pre_state_rows_for_other_source_unchanged() {
    // I-W4 is per-source: rebuilding source A must not touch source B's
    // rows.  Cascade is scoped by `source_id = $sid`, so other sources'
    // rows must survive untouched.
    let (_dir, store) = make_store();

    let initial_a = vec![FunctionCall {
        id: "fc-a".to_string(),
        caller_claim_id: "c".to_string(),
        callee_name: "a".to_string(),
        callee_claim_id: String::new(),
        source_id: "src-a".to_string(),
        byte_start: 0,
        byte_end: 5,
        content_blake3: "b-a".to_string(),
    }];
    let initial_b = vec![FunctionCall {
        id: "fc-b".to_string(),
        caller_claim_id: "c".to_string(),
        callee_name: "b".to_string(),
        callee_claim_id: String::new(),
        source_id: "src-b".to_string(),
        byte_start: 0,
        byte_end: 5,
        content_blake3: "b-b".to_string(),
    }];
    store.insert_function_calls_batch(&initial_a).unwrap();
    store.insert_function_calls_batch(&initial_b).unwrap();

    // Rebuild source A with a brand-new function_call.
    let mut rows = PerSourceRows::default();
    rows.function_calls.push(FunctionCall {
        id: "fc-a-new".to_string(),
        caller_claim_id: "c".to_string(),
        callee_name: "a-new".to_string(),
        callee_claim_id: String::new(),
        source_id: "src-a".to_string(),
        byte_start: 10,
        byte_end: 15,
        content_blake3: "b-a-new".to_string(),
    });
    store.transactional_rebuild_source("src-a", &rows).unwrap();

    assert_eq!(count_fc_for_source(&store, "src-a"), 1);
    assert_eq!(count_fc_for_source(&store, "src-b"), 1);
}
