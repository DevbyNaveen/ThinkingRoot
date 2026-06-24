//! Typed row structs for the 16 new structural tables introduced by the
//! Compile Completeness Contract (`docs/2026-05-02-compile-completeness-
//! contract.md` §4.1–4.16).
//!
//! Each struct mirrors the corresponding CozoDB schema. They exist so
//! Phase 6.7 emitters (`crates/thinkingroot-extract/src/structural_emitters/`)
//! and the batch-insert helpers in `graph.rs` can pass typed payloads
//! across the crate boundary instead of marshalling stringly-typed tuples.
//!
//! Every struct carries the I-2 byte-anchoring triple
//! `(source_id, byte_start, byte_end)` and the I-4 per-row tamper-evidence
//! field `content_blake3`. The contract's auto-generated Phase 9 audit
//! query (graph::query_orphan_bytes) projects through these fields.

use serde::{Deserialize, Serialize};

/// §4.1 function_calls — code call graph row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub id: String,
    pub caller_claim_id: String,
    pub callee_name: String,
    /// Empty until Phase 7e linker resolves it against workspace FunctionDefs.
    pub callee_claim_id: String,
    pub source_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.2 doc_tags — structured documentation annotation row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocTagRow {
    pub id: String,
    pub claim_id: String,
    /// "param" | "returns" | "throws" | "deprecated" | "see" | <literal>.
    pub kind: String,
    pub target: String,
    pub description: String,
    pub source_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.3 code_links — hyperlink in prose / comment row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeLink {
    pub id: String,
    pub source_id: String,
    pub chunk_id: String,
    pub url: String,
    pub link_text: String,
    /// Resolved at Phase 7e (false until linker matches against sources).
    pub is_internal: bool,
    pub target_source_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.3b code_imports — one row per import/use statement. `to_source` +
/// `is_external` are resolved lazily (suffix-match against `sources.uri`) at
/// traversal time; the persisted row carries the raw `import_path` string and
/// its byte anchor. `from_source` is the owning (importing) source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeImport {
    pub id: String,
    pub from_source: String,
    pub import_path: String,
    /// Resolved at traversal time (empty until a suffix-match resolves it).
    pub to_source: String,
    /// True when the import path resolves outside the workspace (best-effort
    /// heuristic at emit; refined at traversal).
    pub is_external: bool,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.4 code_signatures — function/type shape row. Keyed on `claim_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSignature {
    pub claim_id: String,
    pub parameters_json: String,
    pub return_type: String,
    pub visibility: String,
    pub trait_name: String,
    pub parent_scope: String,
    pub field_types_json: String,
    pub source_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.5 config_tree — TOML/YAML/JSON leaf row.
/// Keyed on `(source_id, dotted_path)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigTreeNode {
    pub source_id: String,
    pub dotted_path: String,
    pub value: String,
    /// "string" | "int" | "bool" | "float" | "array" | "table" | "null".
    pub value_type: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.6 data_rows — CSV/TSV/markdown-table row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataRowRow {
    pub id: String,
    pub source_id: String,
    pub row_index: u32,
    /// JSON object of header → cell.
    pub columns_json: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.7 git_commits — commit-level metadata row.
/// Keyed on `(source_id, commit_sha)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommit {
    pub source_id: String,
    pub commit_sha: String,
    pub commit_author: String,
    pub commit_email: String,
    pub commit_timestamp: f64,
    /// JSON array of paths.
    pub changed_files_json: String,
    pub message: String,
    pub parent_sha: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.8 headings — markdown heading hierarchy row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadingRow {
    pub id: String,
    pub source_id: String,
    pub level: u8,
    pub text: String,
    pub parent_heading_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.9 chunks_residual — fall-through row for chunks that produced 0 claims
/// AND 0 typed structural rows. The catch-all that makes I-3 (byte coverage)
/// tractable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidualChunk {
    pub id: String,
    pub source_id: String,
    pub chunk_type: String,
    pub content: String,
    pub metadata_json: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.10 quantities — numeric value extracted from a claim or its chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuantityRow {
    pub id: String,
    pub claim_id: String,
    pub metric_name: String,
    pub value: f64,
    pub unit: String,
    pub qualifier: String,
    pub is_live: bool,
    pub captured_at: f64,
    pub source_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.11 source_annotations — file-level pragma row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceAnnotation {
    pub id: String,
    pub source_id: String,
    /// "license" | "copyright" | "encoding" | "shebang" | "mode"
    /// | "trailing_newline_norm".
    pub kind: String,
    pub value: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.12 source_references — cross-doc citation row. Built at Phase 7e.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceReference {
    pub id: String,
    pub from_source_id: String,
    pub to_source_id: String,
    /// "link" | "import" | "include" | "cite".
    pub reference_kind: String,
    pub fragment: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.13 code_markers — TODO/FIXME/HACK/SAFETY/NOTE/XXX/BUG/PERF row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeMarker {
    pub id: String,
    pub source_id: String,
    pub kind: String,
    pub text: String,
    pub in_claim_id: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.14 test_annotations — test-marker awareness row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestAnnotation {
    pub id: String,
    pub source_id: String,
    pub claim_id: String,
    /// "rust_test" | "junit" | "jest" | "pytest".
    pub framework: String,
    /// "test" | "ignore" | "should_panic" | "describe" | "it" | "fixture" | "skip".
    pub annotation_kind: String,
    pub name: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.15 git_blame — per-line-range author attribution row.
/// Keyed on `(source_id, line_start, line_end)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBlameRow {
    pub source_id: String,
    pub line_start: u32,
    pub line_end: u32,
    pub commit_sha: String,
    pub author: String,
    pub author_email: String,
    pub blamed_at: f64,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

/// §4.16 code_metrics — per-file / per-function complexity row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeMetric {
    pub id: String,
    pub source_id: String,
    /// "file" | "function" | "type".
    pub scope: String,
    pub scope_claim_id: String,
    pub loc: u32,
    pub cyclomatic: u32,
    /// fan_in / fan_out resolved at Phase 7e.
    pub fan_in: u32,
    pub fan_out: u32,
    /// "mccabe" for the 5 supported languages, "unsupported" for others
    /// (per Compile Completeness Contract §15 Q3).
    pub complexity_method: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
}

// ─── North-star compile rebuild (2026-06-24) — mother-node spine rows ───

/// `raw_chunks` — verbatim 1:1 track of every parsed chunk. The
/// "nothing is lost" layer and the spine's chunk node. Distinct from
/// [`ResidualChunk`] (a gap-filler that only fires on uncovered chunks);
/// `raw_chunks` stores ALL chunks and is rebuilt wholesale per-source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawChunkRow {
    pub id: String,
    pub source_id: String,
    pub chunk_index: u32,
    pub chunk_type: String,
    pub content: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
    pub created_at: f64,
}

/// `concept_nodes` — community summary the Stitcher grows from
/// `detect_communities()`. Inserted `quarantined`; promoted to `active`
/// only after a verify pass confirms evidenced co-occurrence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConceptNode {
    pub id: String,
    pub label: String,
    /// JSON array of member entity ids.
    pub member_entity_ids_json: String,
    pub origin: String,
    /// `quarantined` | `active` | `rejected`.
    pub status: String,
    pub confidence: f64,
    /// JSON array of the evidencing shared-claim ids (never empty for a
    /// legitimately-grown concept).
    pub evidence_json: String,
    pub provenance: String,
    pub created_at: f64,
}

/// `spine_edges` — one denormalised row per mother-node hierarchy edge so
/// retrieval graph-expansion is a single indexed join per hop. `edge_kind`
/// ∈ {`doc_has_chunk`, `chunk_has_fact`, `fact_mentions_entity`,
/// `entity_in_concept`}.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpineEdge {
    pub from_id: String,
    pub to_id: String,
    pub edge_kind: String,
    pub source_id: String,
    pub confidence: f64,
    pub created_at: f64,
}
