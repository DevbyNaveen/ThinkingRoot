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
use thinkingroot_core::types::{ContentHash, SourceType};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::rows::{
    CodeLink, CodeMarker, CodeMetric, CodeSignature, ConfigTreeNode, DataRowRow, DocTagRow,
    FunctionCall, HeadingRow, QuantityRow, ResidualChunk, SourceAnnotation, TestAnnotation,
};

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
fn all_16_tables_have_cascade_entry() {
    use thinkingroot_core::STRUCTURAL_TABLES;
    assert_eq!(
        STRUCTURAL_TABLES.len(),
        16,
        "registry must list exactly 16 tables"
    );
    for spec in STRUCTURAL_TABLES {
        assert!(!spec.name.is_empty());
        assert!(!spec.source_id_column.is_empty());
    }
}
