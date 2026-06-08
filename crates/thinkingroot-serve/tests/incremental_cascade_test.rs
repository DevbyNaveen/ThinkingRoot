//! End-to-end cascade tests for the Compile Completeness Contract's 16
//! structural tables.  Pre-water-flow, `remove_source_by_id` only
//! cleaned up the original (claims, entities, edges, temporal,
//! contradictions, trial verdicts, certificates, derivation_edges,
//! events, source) set — leaving function_calls / headings / doc_tags /
//! ... rows orphaned at deleted source_ids.  These tests pin the fix.

use std::collections::BTreeMap;

use cozo::{DataValue, ScriptMutability};
use tempfile::tempdir;
use thinkingroot_core::Source;
use thinkingroot_core::types::{ClaimType, ContentHash, SourceType, WorkspaceId};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::rows::{
    CodeLink, CodeMarker, CodeMetric, CodeSignature, ConfigTreeNode, DataRowRow, DocTagRow,
    FunctionCall, HeadingRow, QuantityRow, ResidualChunk, SourceAnnotation, TestAnnotation,
};
use thinkingroot_link::structural_resolve;

fn make_store() -> GraphStore {
    let dir = tempdir().unwrap();
    let path = dir.into_path();
    GraphStore::init(&path).unwrap()
}

fn count_rows(store: &GraphStore, script: &str, source_id: &str) -> usize {
    let mut params = BTreeMap::new();
    params.insert("sid".into(), DataValue::Str(source_id.into()));
    let result = store
        .raw_db()
        .run_script(script, params, ScriptMutability::Immutable)
        .unwrap();
    if let Some(row) = result.rows.first() {
        if let DataValue::Num(cozo::Num::Int(n)) = &row[0] {
            return *n as usize;
        }
    }
    0
}

fn count_function_calls_for_source(store: &GraphStore, source_id: &str) -> usize {
    count_rows(
        store,
        "?[count(id)] := *function_calls{id, source_id: $sid}",
        source_id,
    )
}

fn fresh_source(store: &GraphStore, uri: &str) -> String {
    let source =
        Source::new(uri.into(), SourceType::File).with_hash(ContentHash(format!("hash-{uri}")));
    let source_id = source.id.to_string();
    store.insert_source(&source).unwrap();
    source_id
}

#[test]
fn file_delete_cascades_function_calls() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://foo.rs");

    let row = FunctionCall {
        id: "fc-1".to_string(),
        caller_claim_id: "caller-1".to_string(),
        callee_name: "foo".to_string(),
        callee_claim_id: String::new(),
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-1".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    assert_eq!(count_function_calls_for_source(&store, &source_id), 1);

    store.remove_source_by_uri("test://foo.rs").unwrap();

    assert_eq!(
        count_function_calls_for_source(&store, &source_id),
        0,
        "function_calls cascade missing — orphan rows survived source delete"
    );
}

#[test]
fn file_delete_cascades_headings() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://heading.md");

    let row = HeadingRow {
        id: "h-1".to_string(),
        source_id: source_id.clone(),
        level: 1,
        text: "Intro".to_string(),
        parent_heading_id: String::new(),
        byte_start: 0,
        byte_end: 8,
        content_blake3: "blake-h".to_string(),
    };
    store.insert_headings_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *headings{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://heading.md").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *headings{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "headings cascade missing"
    );
}

#[test]
fn file_delete_cascades_doc_tags() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://doc.rs");

    let row = DocTagRow {
        id: "dt-1".to_string(),
        claim_id: "claim-1".to_string(),
        kind: "param".to_string(),
        target: "n".to_string(),
        description: "input number".to_string(),
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-dt".to_string(),
    };
    store.insert_doc_tags_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *doc_tags{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://doc.rs").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *doc_tags{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "doc_tags cascade missing"
    );
}

#[test]
fn file_delete_cascades_code_links() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://link.rs");

    let row = CodeLink {
        id: "cl-1".to_string(),
        source_id: source_id.clone(),
        chunk_id: "chunk-1".to_string(),
        url: "https://example.com".to_string(),
        link_text: "example".to_string(),
        is_internal: false,
        target_source_id: String::new(),
        byte_start: 0,
        byte_end: 19,
        content_blake3: "blake-cl".to_string(),
    };
    store.insert_code_links_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *code_links{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://link.rs").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *code_links{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "code_links cascade missing"
    );
}

#[test]
fn file_delete_cascades_code_signatures() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://sig.rs");

    let row = CodeSignature {
        claim_id: "claim-sig-1".to_string(),
        parameters_json: "[]".to_string(),
        return_type: "u32".to_string(),
        visibility: "pub".to_string(),
        trait_name: String::new(),
        parent_scope: String::new(),
        field_types_json: "[]".to_string(),
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 24,
        content_blake3: "blake-sig".to_string(),
    };
    store.insert_code_signatures_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(claim_id)] := *code_signatures{claim_id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://sig.rs").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(claim_id)] := *code_signatures{claim_id, source_id: $sid}",
            &source_id,
        ),
        0,
        "code_signatures cascade missing"
    );
}

#[test]
fn file_delete_cascades_config_tree() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://config.toml");

    let row = ConfigTreeNode {
        source_id: source_id.clone(),
        dotted_path: "package.name".to_string(),
        value: "demo".to_string(),
        value_type: "string".to_string(),
        byte_start: 0,
        byte_end: 12,
        content_blake3: "blake-cfg".to_string(),
    };
    store.insert_config_tree_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(dotted_path)] := *config_tree{source_id: $sid, dotted_path}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://config.toml").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(dotted_path)] := *config_tree{source_id: $sid, dotted_path}",
            &source_id,
        ),
        0,
        "config_tree cascade missing"
    );
}

#[test]
fn file_delete_cascades_data_rows() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://data.csv");

    let row = DataRowRow {
        id: "dr-1".to_string(),
        source_id: source_id.clone(),
        row_index: 0,
        columns_json: r#"{"name":"alice"}"#.to_string(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-dr".to_string(),
    };
    store.insert_data_rows_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *data_rows{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://data.csv").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *data_rows{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "data_rows cascade missing"
    );
}

#[test]
fn file_delete_cascades_chunks_residual() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://residual.txt");

    let row = ResidualChunk {
        id: "cr-1".to_string(),
        source_id: source_id.clone(),
        chunk_type: "byte_gap".to_string(),
        content: "trailing whitespace".to_string(),
        metadata_json: "{}".to_string(),
        byte_start: 0,
        byte_end: 18,
        content_blake3: "blake-cr".to_string(),
    };
    store.insert_chunks_residual_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *chunks_residual{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://residual.txt").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *chunks_residual{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "chunks_residual cascade missing"
    );
}

#[test]
fn file_delete_cascades_quantities() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://qty.md");

    let row = QuantityRow {
        id: "q-1".to_string(),
        claim_id: "claim-q".to_string(),
        metric_name: "p99".to_string(),
        value: 120.0,
        unit: "ms".to_string(),
        qualifier: String::new(),
        is_live: false,
        captured_at: 0.0,
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 8,
        content_blake3: "blake-q".to_string(),
    };
    store.insert_quantities_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *quantities{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://qty.md").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *quantities{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "quantities cascade missing"
    );
}

#[test]
fn file_delete_cascades_source_annotations() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://license.rs");

    let row = SourceAnnotation {
        id: "sa-1".to_string(),
        source_id: source_id.clone(),
        kind: "license".to_string(),
        value: "MIT".to_string(),
        byte_start: 0,
        byte_end: 3,
        content_blake3: "blake-sa".to_string(),
    };
    store.insert_source_annotations_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *source_annotations{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://license.rs").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *source_annotations{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "source_annotations cascade missing"
    );
}

#[test]
fn file_delete_cascades_code_markers() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://todo.rs");

    let row = CodeMarker {
        id: "cm-1".to_string(),
        source_id: source_id.clone(),
        kind: "TODO".to_string(),
        text: "implement me".to_string(),
        in_claim_id: String::new(),
        byte_start: 0,
        byte_end: 12,
        content_blake3: "blake-cm".to_string(),
    };
    store.insert_code_markers_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *code_markers{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://todo.rs").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *code_markers{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "code_markers cascade missing"
    );
}

#[test]
fn file_delete_cascades_test_annotations() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://tests.rs");

    let row = TestAnnotation {
        id: "ta-1".to_string(),
        source_id: source_id.clone(),
        claim_id: "claim-ta".to_string(),
        framework: "rust_test".to_string(),
        annotation_kind: "test".to_string(),
        name: "test_thing".to_string(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-ta".to_string(),
    };
    store.insert_test_annotations_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *test_annotations{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://tests.rs").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *test_annotations{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "test_annotations cascade missing"
    );
}

#[test]
fn file_delete_cascades_code_metrics() {
    let store = make_store();
    let source_id = fresh_source(&store, "test://metrics.rs");

    let row = CodeMetric {
        id: "metric-1".to_string(),
        source_id: source_id.clone(),
        scope: "file".to_string(),
        scope_claim_id: String::new(),
        loc: 10,
        cyclomatic: 1,
        fan_in: 0,
        fan_out: 0,
        complexity_method: "mccabe".to_string(),
        byte_start: 0,
        byte_end: 32,
        content_blake3: "blake-metric".to_string(),
    };
    store.insert_code_metrics_batch(&[row]).unwrap();
    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *code_metrics{id, source_id: $sid}",
            &source_id,
        ),
        1
    );

    store.remove_source_by_uri("test://metrics.rs").unwrap();

    assert_eq!(
        count_rows(
            &store,
            "?[count(id)] := *code_metrics{id, source_id: $sid}",
            &source_id,
        ),
        0,
        "code_metrics cascade missing"
    );
}

#[test]
fn all_17_tables_have_cascade_entry() {
    use thinkingroot_core::STRUCTURAL_TABLES;
    assert_eq!(
        STRUCTURAL_TABLES.len(),
        17,
        "registry must list exactly 17 tables"
    );
    for spec in STRUCTURAL_TABLES {
        assert!(!spec.name.is_empty());
        assert!(!spec.source_id_column.is_empty());
    }
}

#[test]
fn phase_9_detects_orphan_source_rows() {
    let store = make_store();

    let row = FunctionCall {
        id: "fc-orphan".to_string(),
        caller_claim_id: "caller-orphan".to_string(),
        callee_name: "ghost".to_string(),
        callee_claim_id: String::new(),
        source_id: "ghost-source-id".to_string(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-ghost".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    let orphans = store.query_orphan_structural_rows().unwrap();
    assert!(
        !orphans.is_empty(),
        "expected at least one orphan structural row, got none"
    );
    assert!(
        orphans.iter().any(|(table, sid, _)| table == "function_calls" && sid == "ghost-source-id"),
        "expected (function_calls, ghost-source-id) orphan, got: {orphans:?}"
    );
}

#[test]
fn phase_9_passes_after_clean_cascade() {
    let store = make_store();
    let source = Source::new("test://clean.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-clean".into()));
    let source_id = source.id.to_string();
    store.insert_source(&source).unwrap();

    let row = FunctionCall {
        id: "fc-clean".to_string(),
        caller_claim_id: "caller-clean".to_string(),
        callee_name: "bar".to_string(),
        callee_claim_id: String::new(),
        source_id: source_id.clone(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-clean".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    store.remove_source_by_uri("test://clean.rs").unwrap();

    let orphans = store.query_orphan_structural_rows().unwrap();
    assert!(orphans.is_empty(), "expected no orphans after clean cascade, got: {orphans:?}");
}

// ── T4: Phase 7e re-resolution tests ─────────────────────────────────────────

/// Pre-T4, `resolve` only re-resolved rows where `callee_claim_id = ""`.
/// A row already resolved to a claim that was subsequently deleted would
/// retain the dangling claim id forever.  Post-T4 every row is revalidated
/// each compile: dangling ids reset to `""` (external) or re-resolve to a
/// newly-live target.
#[test]
fn function_deleted_callsite_dangling_callee_id_re_resolves_to_empty() {
    let store = make_store();

    // Source A defines fn `target`; source B has a function_calls row
    // already resolved to A's claim.
    let src_a = Source::new("test://a.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-a".into()));
    store.insert_source(&src_a).unwrap();

    let src_b = Source::new("test://b.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-b".into()));
    let src_b_id = src_b.id.to_string();
    store.insert_source(&src_b).unwrap();

    // Insert a claim in A with symbol = "target" so Phase 7e can find it.
    let claim_a = thinkingroot_core::Claim::new(
        "fn target() { ... }",
        ClaimType::Fact,
        src_a.id,
        WorkspaceId::new(),
    )
    .with_symbol("target");
    let claim_a_id = claim_a.id.to_string();
    store.insert_claim(&claim_a).unwrap();

    // function_calls row in B, already resolved (post-Phase 7e) to A's claim.
    let row = FunctionCall {
        id: "fc-b-calls-a".to_string(),
        caller_claim_id: "caller-in-b".to_string(),
        callee_name: "target".to_string(),
        callee_claim_id: claim_a_id.clone(), // previously resolved
        source_id: src_b_id.clone(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-b".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    // Delete source A — simulating its source changing in a later compile.
    // The cascade removes A's source row and its claim rows.
    store.remove_source_by_uri("test://a.rs").unwrap();

    // Re-run Phase 7e.  Post-T4 it revalidates every row (not just the
    // ones with callee_claim_id = ""), so the dangling pointer must reset.
    structural_resolve::resolve(&store).unwrap();

    let mut params = BTreeMap::new();
    params.insert("id".into(), DataValue::Str("fc-b-calls-a".into()));
    let result = store
        .raw_db()
        .run_script(
            "?[callee_claim_id] := *function_calls{id: $id, callee_claim_id}",
            params,
            ScriptMutability::Immutable,
        )
        .unwrap();
    let resolved = match &result.rows[0][0] {
        DataValue::Str(s) => s.to_string(),
        _ => String::new(),
    };
    assert_eq!(
        resolved, "",
        "dangling callee_claim_id should be reset to external (\"\") after target claim deleted"
    );
}

/// When a `code_links` row has `target_source_id` pointing at a source that
/// was subsequently removed, Phase 7e must reset both `target_source_id` and
/// `is_internal` to their unresolved defaults.
#[test]
fn code_link_target_source_deleted_re_resolves() {
    let store = make_store();

    let src_x = Source::new("test://x.md".into(), SourceType::File)
        .with_hash(ContentHash("hash-x".into()));
    let src_x_id = src_x.id.to_string();
    store.insert_source(&src_x).unwrap();

    let src_y = Source::new("test://y.md".into(), SourceType::File)
        .with_hash(ContentHash("hash-y".into()));
    let src_y_id = src_y.id.to_string();
    store.insert_source(&src_y).unwrap();

    // Code link already resolved: x → y.
    let link = CodeLink {
        id: "cl-1".to_string(),
        source_id: src_x_id.clone(),
        chunk_id: String::new(),
        url: "test://y.md".to_string(),
        link_text: "see y".to_string(),
        is_internal: true,
        target_source_id: src_y_id.clone(),
        byte_start: 0,
        byte_end: 8,
        content_blake3: "blake-link".to_string(),
    };
    store.insert_code_links_batch(&[link]).unwrap();

    // Delete source Y.
    store.remove_source_by_uri("test://y.md").unwrap();

    // Re-run Phase 7e.
    structural_resolve::resolve(&store).unwrap();

    let mut params = BTreeMap::new();
    params.insert("id".into(), DataValue::Str("cl-1".into()));
    let result = store
        .raw_db()
        .run_script(
            "?[target_source_id, is_internal] := *code_links{id: $id, target_source_id, is_internal}",
            params,
            ScriptMutability::Immutable,
        )
        .unwrap();
    let target = match &result.rows[0][0] {
        DataValue::Str(s) => s.to_string(),
        _ => String::new(),
    };
    let internal = matches!(&result.rows[0][1], DataValue::Bool(true));
    assert_eq!(
        target, "",
        "target_source_id must be cleared when target source no longer exists"
    );
    assert!(
        !internal,
        "is_internal must be false when target source no longer exists"
    );
}

/// Phase 7e revalidation: when a callee function is renamed in the structural
/// table (its claim's `symbol` changes from "old_name" to something else or
/// the claim is removed entirely), `structural_resolve::resolve` must clear
/// any `function_calls` row that was previously resolved to that claim.
///
/// Scenario:
///   - Source A defines a claim with symbol "old_name".
///   - Source B has a `function_calls` row for `callee_name = "old_name"`,
///     already resolved to A's claim id.
///   - A's claim is removed (simulating a rename / deletion in a later
///     compile, with no new "old_name" claim replacing it).
///   - After Phase 7e re-runs, the `callee_claim_id` on B's row must be
///     cleared to "" because the live claim set no longer contains the target.
#[test]
fn function_renamed_in_place_old_row_replaced() {
    let store = make_store();

    // Source A: home of the function whose symbol will disappear.
    let src_a = Source::new("test://renamed-a.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-ra".into()));
    store.insert_source(&src_a).unwrap();

    // Source B: the caller.
    let src_b = Source::new("test://renamed-b.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-rb".into()));
    let src_b_id = src_b.id.to_string();
    store.insert_source(&src_b).unwrap();

    // Insert a claim in A with symbol "old_name" so Phase 7e can resolve it.
    let claim_a = thinkingroot_core::Claim::new(
        "fn old_name() { ... }",
        ClaimType::Fact,
        src_a.id,
        WorkspaceId::new(),
    )
    .with_symbol("old_name");
    let claim_a_id = claim_a.id.to_string();
    store.insert_claim(&claim_a).unwrap();

    // B's function_calls row is already resolved to A's claim (post-Phase 7e
    // from a previous compile).
    let call = FunctionCall {
        id: "fc-rename-stable".to_string(),
        caller_claim_id: "caller-in-b".to_string(),
        callee_name: "old_name".to_string(),
        callee_claim_id: claim_a_id.clone(), // resolved to A's claim
        source_id: src_b_id.clone(),
        byte_start: 100,
        byte_end: 200,
        content_blake3: "blake-rename".to_string(),
    };
    store.insert_function_calls_batch(&[call]).unwrap();

    // Simulate the rename: remove A's source (which cascades its claim row).
    // No new "old_name" claim exists — the symbol is simply gone from the
    // live workspace.
    store.remove_source_by_uri("test://renamed-a.rs").unwrap();

    // Phase 7e revalidates every row.  The previously-resolved
    // callee_claim_id no longer exists in the live claim set, so it must be
    // reset to "" (external / unresolved).
    let stats = structural_resolve::resolve(&store).unwrap();
    assert!(
        stats.calls_updated >= 1,
        "resolve should have updated the dangling call row"
    );

    let mut params = BTreeMap::new();
    params.insert("id".into(), DataValue::Str("fc-rename-stable".into()));
    let result = store
        .raw_db()
        .run_script(
            "?[callee_claim_id] := *function_calls{id: $id, callee_claim_id}",
            params,
            ScriptMutability::Immutable,
        )
        .unwrap();
    let resolved = match &result.rows[0][0] {
        DataValue::Str(s) => s.to_string(),
        _ => String::new(),
    };
    assert_eq!(
        resolved, "",
        "callee_claim_id must be cleared to \"\" after callee symbol is renamed/removed"
    );
}

// ── T5: resolution_deps table tests ──────────────────────────────────────────

/// Phase 7e must record a `resolution_deps` row whenever a `function_calls`
/// row is resolved to a callee claim that lives in a different source.
/// After resolution, querying `resolution_deps` for `(from=B, to=A)` must
/// return at least one row.
#[test]
fn resolution_deps_records_cross_source_dependencies() {
    let store = make_store();

    // Source A: defines the callee.
    let src_a = Source::new("test://da.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-da".into()));
    let src_a_id = src_a.id.to_string();
    store.insert_source(&src_a).unwrap();

    // Source B: contains the caller.
    let src_b = Source::new("test://db.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-db".into()));
    let src_b_id = src_b.id.to_string();
    store.insert_source(&src_b).unwrap();

    // Claim in A with symbol "dep_target".
    let claim_a = thinkingroot_core::Claim::new(
        "fn dep_target() {}",
        ClaimType::Fact,
        src_a.id,
        WorkspaceId::new(),
    )
    .with_symbol("dep_target");
    store.insert_claim(&claim_a).unwrap();

    // function_calls row in B, unresolved.
    let row = FunctionCall {
        id: "fc-dep".to_string(),
        caller_claim_id: "caller-dep".to_string(),
        callee_name: "dep_target".to_string(),
        callee_claim_id: String::new(),
        source_id: src_b_id.clone(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-dep".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    // Phase 7e must resolve the call and record the cross-source dep.
    structural_resolve::resolve(&store).unwrap();

    let result = store
        .raw_db()
        .run_script(
            "?[from_source_id, to_source_id, kind] := *resolution_deps{from_source_id, to_source_id, kind}",
            Default::default(),
            ScriptMutability::Immutable,
        )
        .unwrap();
    assert!(
        result.rows.iter().any(|r| {
            let from = match &r[0] {
                DataValue::Str(s) => s.to_string(),
                _ => String::new(),
            };
            let to = match &r[1] {
                DataValue::Str(s) => s.to_string(),
                _ => String::new(),
            };
            from == src_b_id && to == src_a_id
        }),
        "expected resolution_deps row (from=B, to=A); got: {:?}",
        result.rows
    );
}

/// When a source is deleted, `resolution_deps` rows pointing AT it (to_source_id)
/// must be cascaded away so `list_dependent_sources` no longer returns phantom deps.
#[test]
fn resolution_deps_cleans_up_on_source_deletion() {
    let store = make_store();

    // Insert the target source so the row is valid.
    let src = Source::new("test://r-dep.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-r-dep".into()));
    let src_id = src.id.to_string();
    store.insert_source(&src).unwrap();

    // Manually record a dep pointing AT src (simulating a prior Phase 7e run).
    store
        .record_resolution_dep("other-source", &src_id, "function_call", "edge-1")
        .unwrap();

    // Verify the dep exists.
    let deps_before = store.list_dependent_sources(&src_id).unwrap();
    assert!(
        deps_before.contains(&"other-source".to_string()),
        "dep should be present before deletion"
    );

    // Delete the target source.
    store.remove_source_by_uri("test://r-dep.rs").unwrap();

    // The cascade must have removed the resolution_deps row.
    let deps_after = store.list_dependent_sources(&src_id).unwrap();
    assert!(
        deps_after.is_empty(),
        "resolution_deps rows pointing at deleted source should be cascaded; got: {deps_after:?}"
    );
}

// ── T6: function-moved cross-source re-resolution scenario ───────────────────

/// End-to-end scenario: a function moves from source A to source C; source B
/// had a `function_calls` row resolved to A.  When A is removed and C appears
/// with the same symbol, Phase 7e (T4 revalidation) must re-point B's row to C.
/// This test also pins that `resolution_deps` correctly tracks the B → A dep
/// before the move, so Phase 4's dirty-source collection (T6 pipeline hook)
/// would correctly identify B as needing re-extraction.
#[test]
fn function_moved_file_a_to_file_b_cross_source_resolution_re_resolves() {
    let store = make_store();

    // Source A: original home of fn `moved`.
    let src_a = Source::new("test://moved-a.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-ma".into()));
    let src_a_id = src_a.id.to_string();
    store.insert_source(&src_a).unwrap();

    // Source B: caller of `moved`.
    let src_b = Source::new("test://moved-b.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-mb".into()));
    let src_b_id = src_b.id.to_string();
    store.insert_source(&src_b).unwrap();

    // Claim in A with symbol "moved".
    let claim_a = thinkingroot_core::Claim::new(
        "fn moved() {}",
        ClaimType::Fact,
        src_a.id,
        WorkspaceId::new(),
    )
    .with_symbol("moved");
    store.insert_claim(&claim_a).unwrap();

    // B's function_calls row for `moved`, initially unresolved.
    let row = FunctionCall {
        id: "fc-mv".to_string(),
        caller_claim_id: "caller-mv".to_string(),
        callee_name: "moved".to_string(),
        callee_claim_id: String::new(),
        source_id: src_b_id.clone(),
        byte_start: 0,
        byte_end: 16,
        content_blake3: "blake-mv".to_string(),
    };
    store.insert_function_calls_batch(&[row]).unwrap();

    // Phase 7e resolves B's call to A's claim and records B → A dep.
    structural_resolve::resolve(&store).unwrap();

    // Confirm resolution_deps recorded the B → A dependency.
    let dependents_before = store.list_dependent_sources(&src_a_id).unwrap();
    assert!(
        dependents_before.contains(&src_b_id),
        "expected B to depend on A after first Phase 7e; got: {dependents_before:?}"
    );

    // Insert source C — new home for fn `moved`.
    let src_c = Source::new("test://moved-c.rs".into(), SourceType::File)
        .with_hash(ContentHash("hash-mc".into()));
    store.insert_source(&src_c).unwrap();
    let claim_c = thinkingroot_core::Claim::new(
        "fn moved() {}",
        ClaimType::Fact,
        src_c.id,
        WorkspaceId::new(),
    )
    .with_symbol("moved");
    let claim_c_id = claim_c.id.to_string();
    store.insert_claim(&claim_c).unwrap();

    // Remove A (simulating the move). The cascade clears resolution_deps for A.
    store.remove_source_by_uri("test://moved-a.rs").unwrap();

    // Re-run Phase 7e — T4 revalidation must re-point B's row to C's claim.
    structural_resolve::resolve(&store).unwrap();

    let mut params = BTreeMap::new();
    params.insert("id".into(), DataValue::Str("fc-mv".into()));
    let result = store
        .raw_db()
        .run_script(
            "?[callee_claim_id] := *function_calls{id: $id, callee_claim_id}",
            params,
            ScriptMutability::Immutable,
        )
        .unwrap();
    let resolved = match &result.rows[0][0] {
        DataValue::Str(s) => s.to_string(),
        _ => String::new(),
    };
    assert_eq!(
        resolved, claim_c_id,
        "function_call should now resolve to C's claim after the move"
    );
}
