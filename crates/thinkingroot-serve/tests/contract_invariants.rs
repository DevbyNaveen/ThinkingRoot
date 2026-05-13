//! Compile Completeness Contract — CI gates (Test 12.1–12.5).
//!
//! `docs/2026-05-02-compile-completeness-contract.md` §12 enumerates
//! five gates that every PR touching the compile pipeline must satisfy.
//! They run end-to-end over the canonical fixture at
//! `tests/fixtures/contract_canonical/` (six files exercising every
//! Phase 6.7 emitter):
//!
//! - **12.1** — byte-coverage invariant (zero orphan bytes).
//! - **12.2** — every structural row carries the I-2 byte triple.
//! - **12.3** — per-row BLAKE3 verifies against byte_store source.
//! - **12.4** — covered by per-emitter unit tests under
//!   `crates/thinkingroot-serve/src/structural_persist/*.rs`.
//! - **12.5** — migration round-trip preserves claims.
//!
//! The harness bypasses the LLM tier by running structural extraction
//! only. Chunks the router classifies as Tier::Llm produce no claim
//! and Phase 6.7 emits a `chunks_residual` row covering their bytes —
//! I-3 still holds.

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::{
    Claim, ClaimType, ContentHash, Source, SourceSpan, TrustLevel, WorkspaceId,
};
use thinkingroot_extract::ExtractionOutput;
use thinkingroot_extract::router::{Tier, classify};
use thinkingroot_extract::structural::extract_structural;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::{FileSystemSourceStore, SourceByteStore};
use thinkingroot_serve::structural_persist::phase_6_7_structural_persist;

fn fixture_path() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest)
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("contract_canonical")
}

fn map_claim_type(s: &str) -> ClaimType {
    match s {
        "api_signature" => ClaimType::ApiSignature,
        "definition" => ClaimType::Definition,
        "dependency" => ClaimType::Dependency,
        // No `Doc` variant in ClaimType — fall back to Fact.
        _ => ClaimType::Fact,
    }
}

/// Common pipeline driver. Parses the fixture, persists sources +
/// bytes, runs structural-tier extraction, then Phase 6.7. Returns
/// the open graph + byte_store + tempdir (the dir is owned to keep
/// the workspace alive for the test's lifetime).
fn run_structural_pipeline(fixture: &Path) -> (TempDir, GraphStore, FileSystemSourceStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (graph, byte_store) = run_structural_pipeline_into(dir.path(), fixture);
    (dir, graph, byte_store)
}

fn run_structural_pipeline_into(
    data_dir: &Path,
    fixture: &Path,
) -> (GraphStore, FileSystemSourceStore) {
    let parser_config = thinkingroot_core::config::ParserConfig::default();
    let docs = thinkingroot_parse::parse_directory(fixture, &parser_config)
        .expect("parse_directory");
    assert!(!docs.is_empty(), "fixture should produce ≥1 DocumentIR");

    let graph = GraphStore::init(data_dir).expect("graph init");
    let byte_store = FileSystemSourceStore::new(data_dir).expect("byte_store init");

    // Phase 6: insert sources + persist bytes.
    for doc in &docs {
        let path = doc.uri.strip_prefix("file://").unwrap_or(&doc.uri);
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue, // git-pseudo-source URIs aren't on disk; skip
        };
        let content_hash = ContentHash::from_bytes(&bytes);
        let byte_size = bytes.len() as u64;
        let source = Source::new(doc.uri.clone(), doc.source_type)
            .with_id(doc.source_id)
            .with_hash(content_hash.clone())
            .with_trust(TrustLevel::Trusted)
            .with_size(byte_size);
        graph.insert_source(&source).expect("insert_source");
        byte_store
            .put(doc.source_id, &content_hash, &bytes)
            .expect("byte_store.put");
    }

    // Structural-tier extraction → ExtractionOutput. We bypass the
    // linker (it requires an entity-graph round-trip the test
    // doesn't need) and instead build the Claims directly so Phase 6.7
    // can stamp `content_blake3` onto them. The claims aren't actually
    // inserted into CozoDB by this test — that means code_signatures /
    // function_calls / doc_tags rows will have empty `claim_id` strings,
    // which is fine for Phase 9 (it audits byte ranges, not foreign
    // keys).
    let workspace_id = WorkspaceId::new();
    let mut extraction = ExtractionOutput::default();
    extraction.sources_processed = docs.len();
    for doc in &docs {
        for chunk in &doc.chunks {
            if classify(chunk) != Tier::Structural {
                continue;
            }
            let result = extract_structural(chunk, &doc.uri);
            for ext_claim in result.claims {
                let claim_type = map_claim_type(&ext_claim.claim_type);
                let mut claim = Claim::new(
                    &ext_claim.statement,
                    claim_type,
                    doc.source_id,
                    workspace_id,
                )
                .with_confidence(ext_claim.confidence)
                .with_extraction_tier(ext_claim.extraction_tier);
                if ext_claim.byte_end > ext_claim.byte_start {
                    claim = claim.with_span(SourceSpan::bytes(
                        ext_claim.byte_start,
                        ext_claim.byte_end,
                    ));
                }
                if let Some(sym) = ext_claim.symbol {
                    if !sym.is_empty() {
                        claim = claim.with_symbol(sym);
                    }
                }
                extraction.claims.push(claim);
            }
        }
    }

    // Persist the claims so Phase 6.7's row_blake3 stamping has rows
    // to update via the schema's content_blake3 column.
    graph
        .insert_claims_batch(&extraction.claims)
        .expect("insert_claims_batch");

    let doc_refs: Vec<&DocumentIR> = docs.iter().collect();
    let _ = phase_6_7_structural_persist(&doc_refs, &mut extraction, &graph, &byte_store)
        .expect("phase_6_7_structural_persist");

    (graph, byte_store)
}

fn dv_to_string(v: &cozo::DataValue) -> String {
    use cozo::{DataValue, Num};
    match v {
        DataValue::Str(s) => s.to_string(),
        DataValue::Num(Num::Int(i)) => i.to_string(),
        DataValue::Num(Num::Float(f)) => f.to_string(),
        DataValue::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

fn dv_to_u64(v: &cozo::DataValue) -> u64 {
    use cozo::{DataValue, Num};
    match v {
        DataValue::Num(Num::Int(i)) => (*i).max(0) as u64,
        DataValue::Num(Num::Float(f)) => f.max(0.0) as u64,
        _ => 0,
    }
}

/// Pull (col1, col2, col3) string-tuples from a 3-column read query.
fn query3(graph: &GraphStore, script: &str) -> Vec<(String, String, String)> {
    use std::collections::BTreeMap;
    let result = graph
        .query(script, BTreeMap::new())
        .unwrap_or_else(|e| panic!("query failed: {e}\nscript: {script}"));
    result
        .rows
        .iter()
        .map(|r| {
            let col = |i: usize| -> String {
                r.get(i).map(dv_to_string).unwrap_or_default()
            };
            (col(0), col(1), col(2))
        })
        .collect()
}

fn count_rows(graph: &GraphStore, script: &str) -> usize {
    use std::collections::BTreeMap;
    let result = graph
        .query(script, BTreeMap::new())
        .unwrap_or_else(|e| panic!("query failed: {e}\nscript: {script}"));
    result.rows.len()
}

// ─── Test 12.1 — byte-coverage invariant ────────────────────────────────

#[test]
fn test_12_1_byte_coverage_invariant_holds() {
    let (_dir, graph, _bs) = run_structural_pipeline(&fixture_path());
    let orphans = graph.query_orphan_bytes().expect("query_orphan_bytes");
    if !orphans.is_empty() {
        eprintln!("orphan byte ranges (first 10):");
        for (sid, s, e) in orphans.iter().take(10) {
            eprintln!("  source={sid} bytes [{s}..{e})  ({} bytes)", e - s);
        }
        let unique_sources: std::collections::HashSet<_> =
            orphans.iter().map(|(sid, _, _)| sid.to_string()).collect();
        panic!(
            "I-3 byte-coverage breach: {} orphan ranges across {} sources",
            orphans.len(),
            unique_sources.len()
        );
    }
}

// ─── Test 12.2 — byte-anchoring per table ───────────────────────────────

#[test]
fn test_12_2_every_row_byte_anchored() {
    let (_dir, graph, _bs) = run_structural_pipeline(&fixture_path());

    // (table, anchor_column). source_references uses `from_source_id`.
    let anchor_tables: &[(&str, &str)] = &[
        ("function_calls", "source_id"),
        ("doc_tags", "source_id"),
        ("code_links", "source_id"),
        ("code_signatures", "source_id"),
        ("config_tree", "source_id"),
        ("data_rows", "source_id"),
        ("git_commits", "source_id"),
        ("headings", "source_id"),
        ("chunks_residual", "source_id"),
        ("quantities", "source_id"),
        ("source_annotations", "source_id"),
        ("source_references", "from_source_id"),
        ("code_markers", "source_id"),
        ("test_annotations", "source_id"),
        ("git_blame", "source_id"),
        ("code_metrics", "source_id"),
    ];

    for (table, anchor) in anchor_tables {
        // Empty-anchor rows violate I-2.
        let q = format!(
            "?[a, bs, be] := *{table}{{{anchor}: a, byte_start: bs, byte_end: be}}, a = ''"
        );
        let bad_anchor = query3(&graph, &q);
        assert!(
            bad_anchor.is_empty(),
            "{table}: {} rows have empty {anchor} (I-2 violated)",
            bad_anchor.len()
        );

        // byte_end < byte_start violates I-2 (zero-length spans are
        // legitimate for trailing-newline-norm and binary PDF
        // placeholders, so we only flag inverted ranges).
        let q2 = format!(
            "?[a, bs, be] := *{table}{{{anchor}: a, byte_start: bs, byte_end: be}}, be < bs"
        );
        let bad_range = query3(&graph, &q2);
        assert!(
            bad_range.is_empty(),
            "{table}: {} rows have byte_end < byte_start (I-2 violated)",
            bad_range.len()
        );
    }
}

// ─── Test 12.3 — per-row BLAKE3 round-trip ──────────────────────────────

#[test]
fn test_12_3_blake3_round_trips() {
    use thinkingroot_graph::row_blake3;

    let (_dir, graph, byte_store) = run_structural_pipeline(&fixture_path());

    // Build source_id → bytes lookup once.
    let sources = query3(
        &graph,
        "?[id, content_hash, byte_size] := *sources{id, content_hash, byte_size}",
    );
    let mut bytes_by_source: std::collections::HashMap<String, Vec<u8>> =
        std::collections::HashMap::new();
    for (id, hash, _) in sources {
        let ch = ContentHash(hash);
        if let Ok(Some(bs)) = byte_store.get(&ch) {
            bytes_by_source.insert(id, bs.bytes);
        }
    }
    assert!(
        !bytes_by_source.is_empty(),
        "fixture should populate ≥1 source byte_store entry"
    );

    // Spot-check headings (always emitted for the README) +
    // code_signatures (auth.rs FunctionDefs).
    for table in ["headings", "code_signatures", "chunks_residual", "data_rows"] {
        let q = format!(
            "?[a, range, h] := *{table}{{source_id: a, byte_start: bs, byte_end: be, content_blake3: h}}, range = bs"
        );
        // Cozo's `:=` doesn't let us export bs directly with rename,
        // so use a 4-col query and ignore the spare. Better: re-query.
        let _ = q;

        let q2 = format!(
            "?[a, bs, be, h] := *{table}{{source_id: a, byte_start: bs, byte_end: be, content_blake3: h}}"
        );
        use std::collections::BTreeMap;
        let result = graph
            .query(&q2, BTreeMap::new())
            .unwrap_or_else(|e| panic!("query failed: {e}\nscript: {q2}"));

        for row in &result.rows {
            if row.len() < 4 {
                continue;
            }
            let sid = dv_to_string(&row[0]);
            let bs = dv_to_u64(&row[1]);
            let be = dv_to_u64(&row[2]);
            let stored = dv_to_string(&row[3]);
            let Some(bytes) = bytes_by_source.get(&sid) else {
                continue;
            };
            let recomputed = row_blake3(bytes, bs, be);
            assert_eq!(
                stored, recomputed,
                "{table}: row at {sid}[{bs}..{be}) BLAKE3 mismatch (I-4 violated)"
            );
        }
    }
}

// ─── Test 12.5 — migration round-trip ───────────────────────────────────

#[test]
fn test_12_5_migration_round_trip() {
    use thinkingroot_serve::backfill::backfill_structural;

    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();

    // Populate the workspace fully.
    let _ = run_structural_pipeline_into(&data_dir, &fixture_path());

    // Snapshot pre-migration claim count.
    let graph = GraphStore::init(&data_dir).expect("graph open");
    let pre_count = count_rows(&graph, "?[id] := *claims{id}");
    assert!(pre_count > 0, "fixture must produce ≥1 claim");

    // Reset workspace_meta to simulate an unmigrated v1 workspace.
    graph.set_workspace_meta("compile_schema_version", "1").unwrap();
    drop(graph);

    // Run the migration. backfill_structural detects the version
    // mismatch (or rather any non-"2") and proceeds.
    let report = backfill_structural(&data_dir).expect("backfill_structural");
    assert_eq!(report.schema_version_after, "2");

    // Verify counts.
    let graph2 = GraphStore::init(&data_dir).expect("graph re-open");
    let post_count = count_rows(&graph2, "?[id] := *claims{id}");
    assert_eq!(
        post_count, pre_count,
        "migration must not lose claims (round-trip safety)"
    );

    let v = graph2
        .get_workspace_meta("compile_schema_version")
        .unwrap()
        .unwrap_or_default();
    assert_eq!(v, "2");
}
