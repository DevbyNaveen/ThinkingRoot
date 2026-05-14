//! Integration tests for the Witness Mesh bridge's lossless statement
//! materialisation (Phase 5 of the Witness Mesh cutover, 2026-05-14).
//!
//! Pre-Phase 5: `GraphStore::get_all_claims_with_sources` synthesised
//! `[witness_type] symbol @byte_start..byte_end` text — useful for
//! debugging but useless for chat citations or the Brain UI. Phase 5
//! switches to reading the actual source bytes from the workspace's
//! `FileSystemSourceStore` and decoding them as UTF-8 so consumers
//! see real source content.
//!
//! These tests pin the materialisation contract end-to-end against a
//! real on-disk graph + byte store. `from_db_for_testing` is covered
//! separately as the explicit fallback path (no byte store attached
//! → synthesised text).

use std::sync::Arc;

use chrono::Utc;
use thinkingroot_core::types::{
    Confidence, ContentHash, Sensitivity, Source, SourceId, SourceType, TrustLevel, Witness,
    WitnessInput, WitnessSpan, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::{FileSystemSourceStore, SourceByteStore};

/// Helper: build a `Witness` whose canonical anchor is `(file_blake3,
/// byte_start, byte_end)` and that resolves to `source_id` in the
/// `sources` table. `content_blake3` is set to a real BLAKE3 over the
/// supplied bytes so a future verifier round-trip can find it.
fn make_witness(
    rule: &str,
    witness_type: &str,
    file_blake3: &str,
    source_id: SourceId,
    workspace: WorkspaceId,
    byte_start: u64,
    byte_end: u64,
    expected_bytes: &[u8],
    symbol: Option<&str>,
) -> Witness {
    let span = WitnessSpan {
        file_blake3: file_blake3.into(),
        start: byte_start,
        end: byte_end,
    };
    let input = WitnessInput::ByteRef {
        file_blake3: file_blake3.into(),
        start: byte_start,
        end: byte_end,
    };
    let content_blake3 = blake3::hash(expected_bytes).to_hex().to_string();
    let mut w = Witness::new(
        rule,
        witness_type,
        vec![input],
        vec![span],
        source_id,
        workspace,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        Utc::now(),
    );
    if let Some(sym) = symbol {
        w.symbol = Some(sym.into());
    }
    w
}

/// Helper: pre-populate the byte store with `bytes` so the
/// materialisation lookup succeeds.
fn put_bytes(data_dir: &std::path::Path, source_id: SourceId, bytes: &[u8]) -> ContentHash {
    let store = FileSystemSourceStore::new(data_dir).unwrap();
    let hash = ContentHash::from_bytes(bytes);
    store.put(source_id, &hash, bytes).unwrap();
    hash
}

#[test]
fn bridge_materialises_real_source_bytes_when_byte_store_attached() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path();
    let store = GraphStore::init(data_dir).unwrap();

    let source_id = SourceId::new();
    let workspace = WorkspaceId::new();
    let body = b"fn answer() -> i32 { 42 }\n";
    let hash = put_bytes(data_dir, source_id.clone(), body);

    let source = Source::new(
        "file:///fixture/answer.rs".into(),
        SourceType::File,
    )
    .with_id(source_id.clone())
    .with_hash(hash)
    .with_trust(TrustLevel::Trusted)
    .with_size(body.len() as u64);
    store.insert_source(&source).unwrap();

    // Function declaration span: "fn answer() -> i32 { 42 }" = bytes 0..25
    let witness = make_witness(
        "tree-sitter::function-decl@v1",
        "declares::function",
        "f-blake3", // file_blake3 not used by the bridge today; sources.content_hash is the lookup key
        source_id,
        workspace,
        0,
        25,
        &body[..25],
        Some("answer"),
    );
    store.insert_witnesses_batch(&[witness]).unwrap();

    let rows = store.get_all_claims_with_sources().unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one witness-derived row");
    let (_id, statement, _wtype, _conf, _uri, _ev) = &rows[0];
    assert_eq!(
        statement, "fn answer() -> i32 { 42 }",
        "statement must be lossless source bytes, not synthesised metadata"
    );
}

#[test]
fn materialize_statement_returns_none_without_byte_store() {
    // `from_db_for_testing` instances attach no byte store — the
    // bridge falls back to synthesised text. This is the contract
    // every existing in-memory test relies on (we did not break them
    // when we added the byte-store field).
    let db = cozo::DbInstance::new("mem", "", "").unwrap();
    let store = GraphStore::from_db_for_testing(db);
    store.init_for_testing().unwrap();
    assert!(
        store
            .materialize_statement("does-not-matter", 0, 100)
            .unwrap()
            .is_none(),
        "no byte store → materialize_statement must return None, never panic, never fabricate"
    );
}

#[test]
fn bridge_falls_back_to_synthesised_text_when_bytes_missing() {
    // Set up a real graph at a tempdir BUT do not put any bytes in the
    // byte store. The bridge must still return a row — with the
    // synthesised `[witness_type] symbol @start..end` form — so the
    // UI never sees an empty statement.
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path();
    let store = GraphStore::init(data_dir).unwrap();

    let source_id = SourceId::new();
    let workspace = WorkspaceId::new();
    // We need the source row to exist with a content_hash for the
    // byte-store lookup to even be attempted. Use a hash that doesn't
    // correspond to any stored bytes — the byte store returns None,
    // and the bridge falls back.
    let stale_hash = ContentHash::from_bytes(b"these-bytes-are-not-stored");
    let source = Source::new(
        "file:///fixture/missing.rs".into(),
        SourceType::File,
    )
    .with_id(source_id.clone())
    .with_hash(stale_hash)
    .with_trust(TrustLevel::Unknown)
    .with_size(0);
    store.insert_source(&source).unwrap();

    let witness = make_witness(
        "tree-sitter::function-decl@v1",
        "declares::function",
        "f-blake3",
        source_id,
        workspace,
        0,
        25,
        b"fn missing() -> i32 { 0 }",
        Some("missing"),
    );
    store.insert_witnesses_batch(&[witness]).unwrap();

    let rows = store.get_all_claims_with_sources().unwrap();
    assert_eq!(rows.len(), 1);
    let (_id, statement, _wtype, _conf, _uri, _ev) = &rows[0];
    assert!(
        statement.starts_with("[declares::function] missing @"),
        "byte store miss must surface the synthesised fallback, not fabricate text — got {statement:?}"
    );
}

#[test]
fn materialize_statement_handles_oversized_end_offset() {
    // The `SourceByteStore::get_range` contract clamps an out-of-bounds
    // `end` to the file end. We want the bridge to surface this
    // clamped slice (not return None), so a witness whose `byte_end`
    // is slightly past EOF (e.g. an off-by-one in a future extractor)
    // still produces lossless text instead of falling back.
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path();
    let store = GraphStore::init(data_dir).unwrap();

    let source_id = SourceId::new();
    let workspace = WorkspaceId::new();
    let body = b"hello";
    let hash = put_bytes(data_dir, source_id.clone(), body);

    let source = Source::new(
        "file:///fixture/short.txt".into(),
        SourceType::File,
    )
    .with_id(source_id.clone())
    .with_hash(hash)
    .with_size(body.len() as u64);
    store.insert_source(&source).unwrap();

    // byte_end is past EOF — clamped to body.len() by the byte store.
    let witness = make_witness(
        "tree-sitter::function-decl@v1",
        "declares::function",
        "f-blake3",
        source_id.clone(),
        workspace,
        0,
        9999,
        body,
        None,
    );
    store.insert_witnesses_batch(&[witness]).unwrap();

    let materialised = store
        .materialize_statement(&source_id.to_string(), 0, 9999)
        .unwrap();
    assert_eq!(
        materialised.as_deref(),
        Some("hello"),
        "out-of-bounds end must clamp to EOF and return the available slice"
    );
}

#[test]
fn byte_store_accessor_is_present_after_init() {
    // Pin the `byte_store()` accessor contract: production `init` always
    // attaches a store. Down-stream consumers (engine layer) rely on
    // `Some(_)` here when wiring branch diffs / proposal reviews.
    let tmp = tempfile::tempdir().unwrap();
    let store = GraphStore::init(tmp.path()).unwrap();
    let bs: Option<Arc<dyn SourceByteStore>> = store.byte_store();
    assert!(bs.is_some(), "GraphStore::init must attach a FileSystemSourceStore");
}
