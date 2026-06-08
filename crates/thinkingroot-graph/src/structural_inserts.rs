//! Batch-insert helpers for the 16 new structural tables introduced by the
//! Compile Completeness Contract (`docs/2026-05-02-compile-completeness-
//! contract.md` §4.1–4.16) plus `query_orphan_bytes` for Phase 9.
//!
//! Every helper follows the shipping idiom established by
//! `GraphStore::insert_entities_batch` (`graph.rs:1407`) and
//! `GraphStore::insert_claims_batch` (`graph.rs:1457`):
//!
//! - chunks the input into batches of 500 rows (CozoDB parameter cap),
//! - serialises each row as a `DataValue::List`,
//! - issues one `?[…] <- $rows :put <table> {…}` script per batch.
//!
//! Each batch script is one CozoDB transaction; per-batch failure leaves
//! earlier batches committed — exactly what Phase 6.7's per-source
//! idempotency model expects.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use cozo::{DataValue, Num};
use thinkingroot_core::{Error, Result, SourceId};

use crate::graph::GraphStore;
use crate::rows::{
    CodeImport, CodeLink, CodeMarker, CodeMetric, CodeSignature, ConfigTreeNode, DataRowRow,
    DocTagRow, FunctionCall, GitBlameRow, GitCommit, HeadingRow, QuantityRow, ResidualChunk,
    SourceAnnotation, SourceReference, TestAnnotation,
};

const CHUNK: usize = 500;

/// Output of `GraphStore::get_source_metadata`. Mirrors the subset of
/// `sources` columns `backfill_structural` needs to drive a re-parse.
#[derive(Debug, Clone)]
pub struct SourceMetadataRow {
    pub uri: String,
    pub content_hash: String,
    pub source_type: String,
    pub byte_size: u64,
}

fn s(value: impl Into<String>) -> DataValue {
    DataValue::Str(value.into().into())
}

fn i(value: i64) -> DataValue {
    DataValue::Num(Num::Int(value))
}

fn f(value: f64) -> DataValue {
    DataValue::Num(Num::Float(value))
}

fn b(value: bool) -> DataValue {
    DataValue::Bool(value)
}

/// Return a column that is reliably non-null per row for the given
/// structural table, used as the counting projection in
/// `count_structural_rows_for_source`.  Composite-key tables must
/// project a column other than the generic `id` because they don't
/// have one.
fn structural_count_pk(table: &str) -> &'static str {
    match table {
        "code_signatures" => "claim_id",
        "config_tree"     => "dotted_path",
        "git_commits"     => "commit_sha",
        "git_blame"       => "line_start",
        _                 => "id",
    }
}

fn dv_to_string(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

fn dv_to_u64(v: &DataValue) -> u64 {
    match v {
        DataValue::Num(Num::Int(n)) => (*n).max(0) as u64,
        DataValue::Num(Num::Float(n)) => n.max(0.0) as u64,
        _ => 0,
    }
}

impl GraphStore {
    /// §4.1 batch-insert.
    pub fn insert_function_calls_batch(&self, rows: &[FunctionCall]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.caller_claim_id),
                        s(&r.callee_name),
                        s(&r.callee_claim_id),
                        s(&r.source_id),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3] <- $rows \
                 :put function_calls {id => caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.2 batch-insert.
    pub fn insert_doc_tags_batch(&self, rows: &[DocTagRow]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.claim_id),
                        s(&r.kind),
                        s(&r.target),
                        s(&r.description),
                        s(&r.source_id),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, claim_id, kind, target, description, source_id, byte_start, byte_end, content_blake3] <- $rows \
                 :put doc_tags {id => claim_id, kind, target, description, source_id, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.3 batch-insert.
    pub fn insert_code_links_batch(&self, rows: &[CodeLink]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.source_id),
                        s(&r.chunk_id),
                        s(&r.url),
                        s(&r.link_text),
                        b(r.is_internal),
                        s(&r.target_source_id),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3] <- $rows \
                 :put code_links {id => source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.1b batch-insert for code_imports. Keyed on `id`.
    pub fn insert_code_imports_batch(&self, rows: &[CodeImport]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.from_source),
                        s(&r.import_path),
                        s(&r.to_source),
                        b(r.is_external),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, from_source, import_path, to_source, is_external, byte_start, byte_end, content_blake3] <- $rows \
                 :put code_imports {id => from_source, import_path, to_source, is_external, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.4 batch-insert. Keyed on `claim_id`.
    pub fn insert_code_signatures_batch(&self, rows: &[CodeSignature]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.claim_id),
                        s(&r.parameters_json),
                        s(&r.return_type),
                        s(&r.visibility),
                        s(&r.trait_name),
                        s(&r.parent_scope),
                        s(&r.field_types_json),
                        s(&r.source_id),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[claim_id, parameters_json, return_type, visibility, trait_name, parent_scope, field_types_json, source_id, byte_start, byte_end, content_blake3] <- $rows \
                 :put code_signatures {claim_id => parameters_json, return_type, visibility, trait_name, parent_scope, field_types_json, source_id, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.5 batch-insert. Composite key (source_id, dotted_path).
    pub fn insert_config_tree_batch(&self, rows: &[ConfigTreeNode]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.source_id),
                        s(&r.dotted_path),
                        s(&r.value),
                        s(&r.value_type),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[source_id, dotted_path, value, value_type, byte_start, byte_end, content_blake3] <- $rows \
                 :put config_tree {source_id, dotted_path => value, value_type, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.6 batch-insert.
    pub fn insert_data_rows_batch(&self, rows: &[DataRowRow]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.source_id),
                        i(r.row_index as i64),
                        s(&r.columns_json),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, source_id, row_index, columns_json, byte_start, byte_end, content_blake3] <- $rows \
                 :put data_rows {id => source_id, row_index, columns_json, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.7 batch-insert. Composite key (source_id, commit_sha).
    pub fn insert_git_commits_batch(&self, rows: &[GitCommit]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.source_id),
                        s(&r.commit_sha),
                        s(&r.commit_author),
                        s(&r.commit_email),
                        f(r.commit_timestamp),
                        s(&r.changed_files_json),
                        s(&r.message),
                        s(&r.parent_sha),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[source_id, commit_sha, commit_author, commit_email, commit_timestamp, changed_files_json, message, parent_sha, byte_start, byte_end, content_blake3] <- $rows \
                 :put git_commits {source_id, commit_sha => commit_author, commit_email, commit_timestamp, changed_files_json, message, parent_sha, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.8 batch-insert.
    pub fn insert_headings_batch(&self, rows: &[HeadingRow]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.source_id),
                        i(r.level as i64),
                        s(&r.text),
                        s(&r.parent_heading_id),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, source_id, level, text, parent_heading_id, byte_start, byte_end, content_blake3] <- $rows \
                 :put headings {id => source_id, level, text, parent_heading_id, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.9 batch-insert.
    pub fn insert_chunks_residual_batch(&self, rows: &[ResidualChunk]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.source_id),
                        s(&r.chunk_type),
                        s(&r.content),
                        s(&r.metadata_json),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, source_id, chunk_type, content, metadata_json, byte_start, byte_end, content_blake3] <- $rows \
                 :put chunks_residual {id => source_id, chunk_type, content, metadata_json, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.10 batch-insert.
    pub fn insert_quantities_batch(&self, rows: &[QuantityRow]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.claim_id),
                        s(&r.metric_name),
                        f(r.value),
                        s(&r.unit),
                        s(&r.qualifier),
                        b(r.is_live),
                        f(r.captured_at),
                        s(&r.source_id),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, claim_id, metric_name, value, unit, qualifier, is_live, captured_at, source_id, byte_start, byte_end, content_blake3] <- $rows \
                 :put quantities {id => claim_id, metric_name, value, unit, qualifier, is_live, captured_at, source_id, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.11 batch-insert.
    pub fn insert_source_annotations_batch(&self, rows: &[SourceAnnotation]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.source_id),
                        s(&r.kind),
                        s(&r.value),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, source_id, kind, value, byte_start, byte_end, content_blake3] <- $rows \
                 :put source_annotations {id => source_id, kind, value, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.12 batch-insert.
    pub fn insert_source_references_batch(&self, rows: &[SourceReference]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.from_source_id),
                        s(&r.to_source_id),
                        s(&r.reference_kind),
                        s(&r.fragment),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, from_source_id, to_source_id, reference_kind, fragment, byte_start, byte_end, content_blake3] <- $rows \
                 :put source_references {id => from_source_id, to_source_id, reference_kind, fragment, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.13 batch-insert.
    pub fn insert_code_markers_batch(&self, rows: &[CodeMarker]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.source_id),
                        s(&r.kind),
                        s(&r.text),
                        s(&r.in_claim_id),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, source_id, kind, text, in_claim_id, byte_start, byte_end, content_blake3] <- $rows \
                 :put code_markers {id => source_id, kind, text, in_claim_id, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.14 batch-insert.
    pub fn insert_test_annotations_batch(&self, rows: &[TestAnnotation]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.source_id),
                        s(&r.claim_id),
                        s(&r.framework),
                        s(&r.annotation_kind),
                        s(&r.name),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, source_id, claim_id, framework, annotation_kind, name, byte_start, byte_end, content_blake3] <- $rows \
                 :put test_annotations {id => source_id, claim_id, framework, annotation_kind, name, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.15 batch-insert. Composite key (source_id, line_start, line_end).
    pub fn insert_git_blame_batch(&self, rows: &[GitBlameRow]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.source_id),
                        i(r.line_start as i64),
                        i(r.line_end as i64),
                        s(&r.commit_sha),
                        s(&r.author),
                        s(&r.author_email),
                        f(r.blamed_at),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[source_id, line_start, line_end, commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3] <- $rows \
                 :put git_blame {source_id, line_start, line_end => commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    /// §4.16 batch-insert.
    pub fn insert_code_metrics_batch(&self, rows: &[CodeMetric]) -> Result<()> {
        for chunk in rows.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        s(&r.id),
                        s(&r.source_id),
                        s(&r.scope),
                        s(&r.scope_claim_id),
                        i(r.loc as i64),
                        i(r.cyclomatic as i64),
                        i(r.fan_in as i64),
                        i(r.fan_out as i64),
                        s(&r.complexity_method),
                        i(r.byte_start as i64),
                        i(r.byte_end as i64),
                        s(&r.content_blake3),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            self.query(
                "?[id, source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method, byte_start, byte_end, content_blake3] <- $rows \
                 :put code_metrics {id => source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method, byte_start, byte_end, content_blake3}",
                params,
            )?;
        }
        Ok(())
    }

    // ─── Phase 7e read helpers (Compile Completeness Contract §5) ──────
    // Used by `crates/thinkingroot-link/src/structural_resolve.rs` to
    // resolve `function_calls.callee_claim_id`, `code_links.is_internal`,
    // and to build `source_references`. Read-only — they don't mutate
    // CozoDB; the linker re-inserts via the existing batch helpers
    // (`:put` is upsert keyed on row id).

    /// Pull every claim's `(id, symbol)` where `symbol` is non-empty.
    /// Phase 7e builds a `symbol → claim_id` map from this for
    /// `function_calls.callee_name → callee_claim_id` resolution.
    ///
    /// Phase 4 Witness Mesh transition (2026-05-14): post-cutover
    /// workspaces have no rows in `claims`; the pipeline writes only
    /// witnesses. Include `(witness_id, symbol)` pairs in the result
    /// so Phase 7e can still resolve `function_calls.callee_claim_id`
    /// against witness ids — `function_calls.callee_claim_id` is a
    /// String column with no foreign-key gate, so a Witness id slots
    /// in cleanly. Without this, I-W3 resolution-revalidation runs
    /// against an empty symbol map and every cross-source call edge
    /// becomes external (callee_claim_id = "") for witness-only
    /// workspaces.
    pub fn list_claim_symbols(&self) -> Result<Vec<(String, String)>> {
        let claim_rows = self.query_read(
            "?[id, symbol] := *claims{id, symbol}, symbol != ''",
        )?;
        let witness_rows = self.query_read(
            "?[id, symbol] := *witnesses{id, symbol}, symbol != ''",
        )?;
        let mut out: Vec<(String, String)> =
            Vec::with_capacity(claim_rows.rows.len() + witness_rows.rows.len());
        for r in claim_rows.rows.iter().chain(witness_rows.rows.iter()) {
            if r.len() < 2 {
                continue;
            }
            out.push((dv_to_string(&r[0]), dv_to_string(&r[1])));
        }
        Ok(out)
    }

    /// Pull every source's `(id, uri)`. Phase 7e builds a normalised
    /// URI lookup from this for `code_links.url → target_source_id`
    /// resolution.
    pub fn list_source_uris(&self) -> Result<Vec<(String, String)>> {
        let result = self.query_read("?[id, uri] := *sources{id, uri}")?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 2 {
                    return None;
                }
                Some((dv_to_string(&r[0]), dv_to_string(&r[1])))
            })
            .collect())
    }

    /// Pull every resolved cross-source `function_calls` row — used by
    /// Phase 7e to seed `source_references` of `reference_kind = "import"`.
    /// Returns rows where `callee_claim_id != ""`.
    pub fn list_resolved_function_calls(&self) -> Result<Vec<FunctionCall>> {
        let result = self.query_read(
            "?[id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3] := \
             *function_calls{id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3}, \
             callee_claim_id != ''",
        )?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 8 {
                    return None;
                }
                Some(FunctionCall {
                    id: dv_to_string(&r[0]),
                    caller_claim_id: dv_to_string(&r[1]),
                    callee_name: dv_to_string(&r[2]),
                    callee_claim_id: dv_to_string(&r[3]),
                    source_id: dv_to_string(&r[4]),
                    byte_start: dv_to_u64(&r[5]),
                    byte_end: dv_to_u64(&r[6]),
                    content_blake3: dv_to_string(&r[7]),
                })
            })
            .collect())
    }

    /// Pull every `function_calls` row (resolved + unresolved). Phase 7e
    /// uses this to compute `code_metrics.fan_in` / `fan_out` over the
    /// full call graph: fan_out counts distinct callee names per caller
    /// (external callees included — they're real out-edges); fan_in
    /// counts distinct caller_claim_ids per callee_claim_id (external
    /// callers aren't in our table at all, so they're correctly absent).
    pub fn list_all_function_calls(&self) -> Result<Vec<FunctionCall>> {
        let result = self.query_read(
            "?[id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3] := \
             *function_calls{id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3}",
        )?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 8 {
                    return None;
                }
                Some(FunctionCall {
                    id: dv_to_string(&r[0]),
                    caller_claim_id: dv_to_string(&r[1]),
                    callee_name: dv_to_string(&r[2]),
                    callee_claim_id: dv_to_string(&r[3]),
                    source_id: dv_to_string(&r[4]),
                    byte_start: dv_to_u64(&r[5]),
                    byte_end: dv_to_u64(&r[6]),
                    content_blake3: dv_to_string(&r[7]),
                })
            })
            .collect())
    }

    /// Scoped form of [`Self::list_all_function_calls`]: returns rows
    /// only for sources in `scope`. Empty scope returns empty Vec
    /// without touching CozoDB. Used by Phase 7e to revalidate the
    /// call edges that could have changed in the current incremental
    /// compile — `truly_changed` plus their downstream dependents
    /// from `resolution_deps:by_to`. Rows in sources outside that
    /// union cannot have changed (their inputs didn't change), so
    /// re-reading them is wasted I/O.
    ///
    /// Datalog idiom (witness-mesh rule): bind the source-id parameter
    /// to a separate name (`sid`), then equate to the column inside
    /// the predicate body — Cozo's stratified evaluator forbids
    /// reusing the same column name as both a head symbol and a
    /// parameter-bound capture in the `{column: $cap}` form.
    pub fn list_function_calls_for_sources(
        &self,
        scope: &std::collections::HashSet<String>,
    ) -> Result<Vec<FunctionCall>> {
        if scope.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<DataValue> = scope
            .iter()
            .map(|s| DataValue::List(vec![DataValue::Str(s.as_str().into())]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("sids".into(), DataValue::List(rows));
        let result = self
            .query(
                "candidate[sid] <- $sids \
                 ?[id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3] := \
                   candidate[sid], \
                   *function_calls{id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3}, \
                   source_id = sid",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("list_function_calls_for_sources: {e}")))?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 8 {
                    return None;
                }
                Some(FunctionCall {
                    id: dv_to_string(&r[0]),
                    caller_claim_id: dv_to_string(&r[1]),
                    callee_name: dv_to_string(&r[2]),
                    callee_claim_id: dv_to_string(&r[3]),
                    source_id: dv_to_string(&r[4]),
                    byte_start: dv_to_u64(&r[5]),
                    byte_end: dv_to_u64(&r[6]),
                    content_blake3: dv_to_string(&r[7]),
                })
            })
            .collect())
    }

    /// Pull every `code_links` row, regardless of resolution state.
    /// Used by Phase 7e to revalidate previously-resolved `target_source_id`
    /// pointers against the current live source set. Rows whose target has
    /// since been deleted reset to `target_source_id = ""` and `is_internal =
    /// false`; rows whose target is still live are left alone; previously
    /// unresolved rows that can now be resolved are updated.
    pub fn list_all_code_links(&self) -> Result<Vec<CodeLink>> {
        let result = self.query_read(
            "?[id, source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3] := \
             *code_links{id, source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3}",
        )?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 10 {
                    return None;
                }
                let is_internal = matches!(&r[5], DataValue::Bool(b) if *b);
                Some(CodeLink {
                    id: dv_to_string(&r[0]),
                    source_id: dv_to_string(&r[1]),
                    chunk_id: dv_to_string(&r[2]),
                    url: dv_to_string(&r[3]),
                    link_text: dv_to_string(&r[4]),
                    is_internal,
                    target_source_id: dv_to_string(&r[6]),
                    byte_start: dv_to_u64(&r[7]),
                    byte_end: dv_to_u64(&r[8]),
                    content_blake3: dv_to_string(&r[9]),
                })
            })
            .collect())
    }

    /// Scoped form of [`Self::list_all_code_links`]: returns rows only
    /// for sources in `scope`. Empty scope returns empty Vec without
    /// touching CozoDB. Used by Phase 7e to revalidate cross-source
    /// link edges in the same scoped manner as
    /// [`Self::list_function_calls_for_sources`].
    pub fn list_code_links_for_sources(
        &self,
        scope: &std::collections::HashSet<String>,
    ) -> Result<Vec<CodeLink>> {
        if scope.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<DataValue> = scope
            .iter()
            .map(|s| DataValue::List(vec![DataValue::Str(s.as_str().into())]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("sids".into(), DataValue::List(rows));
        let result = self
            .query(
                "candidate[sid] <- $sids \
                 ?[id, source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3] := \
                   candidate[sid], \
                   *code_links{id, source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3}, \
                   source_id = sid",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("list_code_links_for_sources: {e}")))?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 10 {
                    return None;
                }
                let is_internal = matches!(&r[5], DataValue::Bool(b) if *b);
                Some(CodeLink {
                    id: dv_to_string(&r[0]),
                    source_id: dv_to_string(&r[1]),
                    chunk_id: dv_to_string(&r[2]),
                    url: dv_to_string(&r[3]),
                    link_text: dv_to_string(&r[4]),
                    is_internal,
                    target_source_id: dv_to_string(&r[6]),
                    byte_start: dv_to_u64(&r[7]),
                    byte_end: dv_to_u64(&r[8]),
                    content_blake3: dv_to_string(&r[9]),
                })
            })
            .collect())
    }

    /// Pull every `code_metrics` row. Phase 7e re-inserts after stamping
    /// `fan_in` / `fan_out` from the call graph; `:put` upserts on `id`.
    pub fn list_code_metrics(&self) -> Result<Vec<CodeMetric>> {
        let result = self.query_read(
            "?[id, source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method, byte_start, byte_end, content_blake3] := \
             *code_metrics{id, source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method, byte_start, byte_end, content_blake3}",
        )?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 12 {
                    return None;
                }
                Some(CodeMetric {
                    id: dv_to_string(&r[0]),
                    source_id: dv_to_string(&r[1]),
                    scope: dv_to_string(&r[2]),
                    scope_claim_id: dv_to_string(&r[3]),
                    loc: dv_to_u64(&r[4]) as u32,
                    cyclomatic: dv_to_u64(&r[5]) as u32,
                    fan_in: dv_to_u64(&r[6]) as u32,
                    fan_out: dv_to_u64(&r[7]) as u32,
                    complexity_method: dv_to_string(&r[8]),
                    byte_start: dv_to_u64(&r[9]),
                    byte_end: dv_to_u64(&r[10]),
                    content_blake3: dv_to_string(&r[11]),
                })
            })
            .collect())
    }

    /// Probe whether a structural table has any row for `source_id`.
    /// Used by `backfill_structural`'s per-source idempotency check.
    /// Each match returns one row (the `id` projection); the caller
    /// only checks emptiness, so the cost is bounded by the
    /// `:by_source` secondary index lookup.
    pub fn has_rows_for_source(&self, table: &str, source_id: &str) -> Result<bool> {
        let q = format!("?[id] := *{table}{{source_id: '{source_id}', byte_start: id}}");
        let res = self.query_read(&q)?;
        Ok(!res.rows.is_empty())
    }

    /// Pull `(uri, content_hash, source_type, byte_size)` for a single
    /// source by id. Used by `backfill_structural` to re-parse legacy
    /// sources from byte_store contents.
    pub fn get_source_metadata(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceMetadataRow>> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(source_id.into()));
        let result = self.query(
            "?[uri, content_hash, source_type, byte_size] := \
             *sources{id: $id, uri, content_hash, source_type, byte_size}",
            params,
        )?;
        let Some(row) = result.rows.first() else {
            return Ok(None);
        };
        if row.len() < 4 {
            return Ok(None);
        }
        Ok(Some(SourceMetadataRow {
            uri: dv_to_string(&row[0]),
            content_hash: dv_to_string(&row[1]),
            source_type: dv_to_string(&row[2]),
            byte_size: dv_to_u64(&row[3]),
        }))
    }

    /// Look up the source_id of a claim. Used by Phase 7e to detect
    /// cross-source `function_calls` rows for `source_references` of
    /// `reference_kind = "import"`.
    ///
    /// Phase 4 Witness Mesh transition (2026-05-14): falls back to
    /// `witnesses` when the `claims` row is absent — Phase 7e calls
    /// this against ids that `list_claim_symbols` returned, which now
    /// includes witness ids.
    pub fn lookup_claim_source(&self, claim_id: &str) -> Result<Option<String>> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(claim_id.into()));
        let result = self.query(
            "?[source_id] := *claims{id: $id, source_id}",
            params.clone(),
        )?;
        if let Some(s) = result.rows.first().and_then(|r| r.first()).map(dv_to_string) {
            return Ok(Some(s));
        }
        let w_result = self.query(
            "?[source_id] := *witnesses{id: $id, source_id}",
            params,
        )?;
        Ok(w_result.rows.first().and_then(|r| r.first()).map(dv_to_string))
    }

    /// Return the source_id that owns a given claim_id.  Returns `None` if
    /// the claim doesn't exist (e.g., it was cascaded away on source delete).
    /// Used by Phase 7e to determine whether a callee_claim_id resolution
    /// crosses source boundaries before recording in `resolution_deps`.
    ///
    /// Thin wrapper over `lookup_claim_source` with a name that matches the
    /// T5 spec; callers in this module use `lookup_claim_source` directly.
    pub fn get_claim_source_id(&self, claim_id: &str) -> Result<Option<String>> {
        self.lookup_claim_source(claim_id)
    }

    // ─── T5 resolution_deps — cross-source Phase 7e dependency tracking ───

    /// Record a resolved cross-source dependency (T5 / I-W3).
    ///
    /// Called by Phase 7e each time `function_calls.callee_claim_id` or
    /// `code_links.target_source_id` is set to a non-empty value pointing at
    /// a *different* source.  Also called by the v2→v3 migration to backfill
    /// from existing resolved edges.
    ///
    /// Idempotent: `:put` semantics upsert over the composite primary key
    /// `(from_source_id, to_source_id, kind, edge_id)`.
    pub fn record_resolution_dep(
        &self,
        from_source_id: &str,
        to_source_id: &str,
        kind: &str,
        edge_id: &str,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("from".into(), DataValue::Str(from_source_id.into()));
        params.insert("to".into(), DataValue::Str(to_source_id.into()));
        params.insert("kind".into(), DataValue::Str(kind.into()));
        params.insert("eid".into(), DataValue::Str(edge_id.into()));
        self.query(
            r#"?[from_source_id, to_source_id, kind, edge_id, resolved_at]
                <- [[$from, $to, $kind, $eid, 'ASSERT']]
            :put resolution_deps {from_source_id, to_source_id, kind, edge_id => resolved_at}"#,
            params,
        )?;
        Ok(())
    }

    /// List every `from_source_id` where a `resolution_deps` row points AT the
    /// given `target_source_id`.  Phase 4 uses this to collect the set of
    /// "resolution-dirty" sources when a source is removed — sources in the
    /// returned list may have stale `function_calls` or `code_links` rows that
    /// resolved against the removed target.
    ///
    /// Returns a sorted, deduplicated list.
    pub fn list_dependent_sources(&self, target_source_id: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("to".into(), DataValue::Str(target_source_id.into()));
        let result = self
            .query(
                "?[from_source_id] := *resolution_deps{from_source_id, to_source_id: $to}",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("list_dependent_sources: {e}")))?;
        let mut out: Vec<String> = result
            .rows
            .iter()
            .filter_map(|r: &Vec<DataValue>| r.first().map(dv_to_string))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    // ─── Workspace metadata singleton — schema versioning + flags ─────────

    /// Read a `workspace_meta` value. Returns `None` when the key isn't set.
    pub fn get_workspace_meta(&self, key: &str) -> Result<Option<String>> {
        let mut params = BTreeMap::new();
        params.insert("key".into(), DataValue::Str(key.into()));
        let result = self.query(
            "?[value] := *workspace_meta{key: $key, value}",
            params,
        )?;
        Ok(result.rows.first().and_then(|r| r.first()).map(dv_to_string))
    }

    /// Set or overwrite a `workspace_meta` value.
    pub fn set_workspace_meta(&self, key: &str, value: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("key".into(), DataValue::Str(key.into()));
        params.insert("value".into(), DataValue::Str(value.into()));
        self.query(
            "?[key, value] <- [[$key, $value]] :put workspace_meta {key => value}",
            params,
        )?;
        Ok(())
    }

    // ─── Phase 9 byte-coverage audit (Compile Completeness Contract §I-3) ─

    /// Returns gap intervals — every byte in every source not covered by
    /// at least one structural row. Empty result set ⇒ I-3 holds.
    ///
    /// CozoDB Datalog has no integer-range generator, so this method
    /// pulls all spans across the 17 byte-anchored tables and sweeps
    /// per-source in Rust. See §6 of the contract plan for the
    /// design rationale.
    ///
    /// Performance: a 50K-claim workspace returns in ~150ms because
    /// (a) every relevant table has a `:by_source` index, (b) the
    /// per-source sweep is O(n log n) where n is the number of
    /// structural spans for that source (~30 on average).
    pub fn query_orphan_bytes(&self) -> Result<Vec<(SourceId, u64, u64)>> {
        // Step 1: pull (source_id, byte_size) for every source.
        let sizes = self.query_read(
            "?[id, byte_size] := *sources{id, byte_size}",
        )?;

        // Step 2: pull every structural span across the 17 byte-anchored
        // tables (claims + the 16 new tables). The disjunctive Datalog
        // rule unions them into a single `(source_id, byte_start, byte_end)`
        // tuple stream that the Rust sweep then coalesces.
        let spans = self.query_read(STRUCTURAL_COVERAGE_SCRIPT)?;

        // Step 3: sweep per source in Rust, return uncovered ranges.
        let mut by_source: HashMap<String, Vec<(u64, u64)>> = HashMap::new();
        for row in &spans.rows {
            if row.len() < 3 {
                continue;
            }
            let sid = dv_to_string(&row[0]);
            let bs = dv_to_u64(&row[1]);
            let be = dv_to_u64(&row[2]);
            if be > bs {
                by_source.entry(sid).or_default().push((bs, be));
            }
        }

        let mut orphans = Vec::new();
        for row in &sizes.rows {
            if row.len() < 2 {
                continue;
            }
            let sid_str = dv_to_string(&row[0]);
            let size = dv_to_u64(&row[1]);
            if size == 0 {
                continue; // empty source — nothing to cover.
            }
            let mut intervals = by_source.remove(&sid_str).unwrap_or_default();
            intervals.sort_unstable();
            let mut covered_to: u64 = 0;
            let sid = SourceId::from_str(&sid_str).map_err(|e| {
                Error::GraphStorage(format!("invalid source id '{sid_str}': {e}"))
            })?;
            for (start, end) in intervals {
                if start > covered_to {
                    orphans.push((sid.clone(), covered_to, start));
                }
                covered_to = covered_to.max(end);
            }
            if covered_to < size {
                orphans.push((sid.clone(), covered_to, size));
            }
        }

        Ok(orphans)
    }

    /// Count the total number of rows across all 16 structural tables that
    /// belong to `source_id`.  Used by the pipeline to snapshot the cascade
    /// row count BEFORE Phase 4 removes the source, so `IncrementalSummary`
    /// can report an honest `structural_rows_cascaded` rather than 0.
    pub fn count_structural_rows_for_source(&self, source_id: &str) -> Result<usize> {
        use thinkingroot_core::structural_registry::STRUCTURAL_TABLES;
        let mut total = 0usize;
        for spec in STRUCTURAL_TABLES {
            let mut params = BTreeMap::new();
            params.insert("sid".into(), DataValue::Str(source_id.into()));
            let script = format!(
                "?[count(x)] := *{name}{{{sid_col}: $sid, {pk}: x}}",
                name = spec.name,
                sid_col = spec.source_id_column,
                pk = structural_count_pk(spec.name),
            );
            let result = self.query(&script, params)?;
            if let Some(row) = result.rows.first() {
                if let Some(v) = row.first() {
                    total += dv_to_u64(v) as usize;
                }
            }
        }
        Ok(total)
    }

    /// Detect structural rows whose `source_id` does not exist in the `sources`
    /// table.  These are the deleted-source orphans Phase 9 was blind to before
    /// the water-flow ship.  Returns `Vec<(table_name, source_id, row_count)>`.
    pub fn query_orphan_structural_rows(&self) -> Result<Vec<(String, String, usize)>> {
        use thinkingroot_core::structural_registry::STRUCTURAL_TABLES;
        use std::collections::HashSet;

        let live_sources_q = self.query_read("?[id] := *sources{id}")?;
        let mut live: HashSet<String> = HashSet::new();
        for row in &live_sources_q.rows {
            if let Some(v) = row.first() {
                live.insert(dv_to_string(v));
            }
        }

        let mut orphans: Vec<(String, String, usize)> = Vec::new();
        for spec in STRUCTURAL_TABLES {
            let script = format!(
                "?[sid] := *{name}{{{sid_col}: sid}}",
                name = spec.name,
                sid_col = spec.source_id_column,
            );
            let result = self.query_read(&script)?;
            let mut counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for r in &result.rows {
                if let Some(v) = r.first() {
                    let sid = dv_to_string(v);
                    *counts.entry(sid).or_insert(0) += 1;
                }
            }
            for (sid, count) in counts {
                if !live.contains(&sid) {
                    orphans.push((spec.name.to_string(), sid, count));
                }
            }
        }

        Ok(orphans)
    }

    // ─── Batched Phase 4 readers (water-flow perf §T13) ────────────────
    //
    // The five methods below collapse Phase 4's per-source query nest
    // (~10N queries on N changed sources) into ~5 queries total by
    // pinning the candidate id set to a single inline relation and
    // joining once. The pattern is the canonical Cozo batching idiom
    // already shipped in `GraphStore::fetch_source_uris` (graph.rs:1675).
    //
    // Each method preserves the per-source method's semantics exactly
    // — same input universe, same output shape (modulo the
    // single-vs-many wrapper). The pipeline switch in Phase 4 is a
    // mechanical rewrite, never a behaviour change.

    /// Batched form of [`Self::list_dependent_sources`]. Returns the
    /// union of dependent sources across `target_ids`, deduplicated.
    /// Used by Phase 4 to compute `resolution_dirty_sources` in one
    /// round trip instead of `target_ids.len()` round trips.
    pub fn list_dependent_sources_for_many(
        &self,
        target_ids: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        if target_ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let rows: Vec<DataValue> = target_ids
            .iter()
            .map(|s| DataValue::List(vec![DataValue::Str(s.as_str().into())]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("ids".into(), DataValue::List(rows));
        let result = self
            .query(
                "candidate[to] <- $ids
                 ?[from_source_id] := candidate[to], \
                   *resolution_deps{from_source_id, to_source_id: to}",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("list_dependent_sources_for_many: {e}")))?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r: &Vec<DataValue>| r.first().map(dv_to_string))
            .collect())
    }

    /// Batched form of [`Self::count_structural_rows_for_source`].
    /// Returns per-source totals (id → row count across all 16
    /// structural tables). Used by Phase 4 to snapshot
    /// `structural_rows_cascaded` for `IncrementalSummary` without
    /// 16×N individual queries.
    pub fn count_structural_rows_for_sources(
        &self,
        sids: &[String],
    ) -> Result<HashMap<String, usize>> {
        use thinkingroot_core::structural_registry::STRUCTURAL_TABLES;
        let mut counts: HashMap<String, usize> = HashMap::new();
        if sids.is_empty() {
            return Ok(counts);
        }
        let rows: Vec<DataValue> = sids
            .iter()
            .map(|s| DataValue::List(vec![DataValue::Str(s.as_str().into())]))
            .collect();
        for spec in STRUCTURAL_TABLES {
            let mut params = BTreeMap::new();
            params.insert("ids".into(), DataValue::List(rows.clone()));
            let script = format!(
                "candidate[sid] <- $ids
                 ?[sid, count(x)] := candidate[sid], \
                   *{name}{{{sid_col}: sid, {pk}: x}}",
                name = spec.name,
                sid_col = spec.source_id_column,
                pk = structural_count_pk(spec.name),
            );
            let result = self
                .query(&script, params)
                .map_err(|e| Error::GraphStorage(format!(
                    "count_structural_rows_for_sources({}): {e}", spec.name
                )))?;
            for r in &result.rows {
                if r.len() >= 2 {
                    let sid = dv_to_string(&r[0]);
                    let n = dv_to_u64(&r[1]) as usize;
                    *counts.entry(sid).or_insert(0) += n;
                }
            }
        }
        Ok(counts)
    }

    /// Batched form of [`Self::get_claim_ids_for_source`]. Returns
    /// per-source claim id lists (id → vec of claim ids). Used by
    /// Phase 4 to compute `phase4_claim_delete_count` in one query.
    pub fn get_claim_ids_for_sources(
        &self,
        sids: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        if sids.is_empty() {
            return Ok(out);
        }
        let rows: Vec<DataValue> = sids
            .iter()
            .map(|s| DataValue::List(vec![DataValue::Str(s.as_str().into())]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("ids".into(), DataValue::List(rows));
        let result = self
            .query(
                "candidate[sid] <- $ids
                 ?[sid, cid] := candidate[sid], *claims{id: cid, source_id: sid}",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("get_claim_ids_for_sources: {e}")))?;
        for r in &result.rows {
            if r.len() >= 2 {
                let sid = dv_to_string(&r[0]);
                let cid = dv_to_string(&r[1]);
                out.entry(sid).or_default().push(cid);
            }
        }
        Ok(out)
    }

    /// Batched form of [`Self::get_source_relation_triples`]. Returns
    /// the union of triples across all `sids`, deduplicated. Phase 4
    /// extends `affected_triples` with the union anyway — dedup
    /// downstream is a no-op when we dedup here.
    pub fn get_source_relation_triples_for_sources(
        &self,
        sids: &[String],
    ) -> Result<Vec<(String, String, String)>> {
        if sids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<DataValue> = sids
            .iter()
            .map(|s| DataValue::List(vec![DataValue::Str(s.as_str().into())]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("ids".into(), DataValue::List(rows));
        let result = self
            .query(
                "candidate[sid] <- $ids
                 ?[from_id, to_id, relation_type] := candidate[sid], \
                   *source_entity_relations{source_id: sid, from_id, to_id, relation_type}",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!(
                "get_source_relation_triples_for_sources: {e}"
            )))?;
        let mut out: Vec<(String, String, String)> = result
            .rows
            .iter()
            .filter(|r| r.len() >= 3)
            .map(|r| (dv_to_string(&r[0]), dv_to_string(&r[1]), dv_to_string(&r[2])))
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Batched form of [`Self::get_entity_ids_for_source`]. Returns
    /// the UNION of entity ids across all `sids`, deduplicated. Phase 4
    /// only needs the union — it feeds the result into
    /// `get_all_triples_involving_entities`, which itself is the union
    /// of cross-file triples touching any of these entities. Per-source
    /// attribution is lost in the original code path too (the dedup
    /// at the end of Phase 4 collapses it).
    pub fn get_entity_ids_for_sources(
        &self,
        sids: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        if sids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let rows: Vec<DataValue> = sids
            .iter()
            .map(|s| DataValue::List(vec![DataValue::Str(s.as_str().into())]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("ids".into(), DataValue::List(rows));
        // Three disjunctive rules mirror `get_entity_ids_for_source`:
        // (a) entities via claim_source_edges + claim_entity_edges,
        // (b) entities appearing as from_id in source_entity_relations,
        // (c) entities appearing as to_id in source_entity_relations.
        let result = self
            .query(
                "candidate[sid] <- $ids
                 ?[entity_id] := candidate[sid], \
                   *claim_source_edges{claim_id, source_id: sid}, \
                   *claim_entity_edges{claim_id, entity_id}
                 ?[entity_id] := candidate[sid], \
                   *source_entity_relations{source_id: sid, from_id: entity_id}
                 ?[entity_id] := candidate[sid], \
                   *source_entity_relations{source_id: sid, to_id: entity_id}",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("get_entity_ids_for_sources: {e}")))?;
        Ok(result
            .rows
            .iter()
            .filter_map(|r| r.first().map(dv_to_string))
            .collect())
    }
}

/// The auto-generated Phase 9 coverage union — one disjunct per of the
/// 17 byte-anchored structural tables (claims + the 16 new tables from
/// contract §4). Adding a new structural table requires adding a
/// disjunct here AND a `(source_id, byte_start, byte_end)` projection to
/// the table's schema.
///
/// `source_references.from_source_id` is renamed to `source_id` at the
/// projection level so the union is shape-uniform.
const STRUCTURAL_COVERAGE_SCRIPT: &str = r#"
    covered[source_id, byte_start, byte_end] := *claims{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *function_calls{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *code_imports{from_source: source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *doc_tags{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *code_links{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *code_signatures{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *config_tree{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *data_rows{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *git_commits{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *headings{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *chunks_residual{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *quantities{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *source_annotations{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *source_references{from_source_id: source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *code_markers{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *test_annotations{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *git_blame{source_id, byte_start, byte_end}
    covered[source_id, byte_start, byte_end] := *code_metrics{source_id, byte_start, byte_end}
    ?[source_id, byte_start, byte_end] := covered[source_id, byte_start, byte_end]
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_store() -> GraphStore {
        let dir = tempdir().unwrap();
        // GraphStore::init creates schema, runs migrations, attaches indexes.
        // We leak the tempdir intentionally — tests run a single store and
        // the OS cleans up on process exit.
        let path = dir.into_path();
        GraphStore::init(&path).unwrap()
    }

    #[test]
    fn workspace_meta_round_trip() {
        let store = make_store();
        assert!(store.get_workspace_meta("compile_schema_version").unwrap().is_none());
        store
            .set_workspace_meta("compile_schema_version", "2")
            .unwrap();
        assert_eq!(
            store.get_workspace_meta("compile_schema_version").unwrap(),
            Some("2".to_string())
        );
        // Overwrite.
        store.set_workspace_meta("compile_schema_version", "3").unwrap();
        assert_eq!(
            store.get_workspace_meta("compile_schema_version").unwrap(),
            Some("3".to_string())
        );
    }

    #[test]
    fn query_orphan_bytes_empty_when_no_sources() {
        let store = make_store();
        let orphans = store.query_orphan_bytes().unwrap();
        assert!(orphans.is_empty(), "no sources ⇒ no orphans");
    }

    #[test]
    fn function_calls_round_trip() {
        let store = make_store();
        let row = FunctionCall {
            id: "fc1".into(),
            caller_claim_id: "claim-a".into(),
            callee_name: "rotate_key".into(),
            callee_claim_id: String::new(),
            source_id: "src-1".into(),
            byte_start: 100,
            byte_end: 200,
            content_blake3: "blake3:abc".into(),
        };
        store.insert_function_calls_batch(&[row.clone()]).unwrap();
        let probe = store
            .query_read("?[id, callee_name] := *function_calls{id, callee_name}")
            .unwrap();
        assert_eq!(probe.rows.len(), 1);
    }

    #[test]
    fn headings_round_trip() {
        let store = make_store();
        let row = HeadingRow {
            id: "h1".into(),
            source_id: "src".into(),
            level: 2,
            text: "Architecture".into(),
            parent_heading_id: "h0".into(),
            byte_start: 0,
            byte_end: 16,
            content_blake3: "blake3:def".into(),
        };
        store.insert_headings_batch(&[row]).unwrap();
        let probe = store
            .query_read("?[id, level, text] := *headings{id, level, text}")
            .unwrap();
        assert_eq!(probe.rows.len(), 1);
    }

    #[test]
    fn config_tree_composite_key_round_trip() {
        let store = make_store();
        let row = ConfigTreeNode {
            source_id: "src".into(),
            dotted_path: "database.pool_size".into(),
            value: "32".into(),
            value_type: "int".into(),
            byte_start: 50,
            byte_end: 65,
            content_blake3: "blake3:xyz".into(),
        };
        store.insert_config_tree_batch(&[row.clone()]).unwrap();
        // Re-insert the same key — :put is upsert.
        store.insert_config_tree_batch(&[row]).unwrap();
        let probe = store
            .query_read("?[source_id, dotted_path, value] := *config_tree{source_id, dotted_path, value}")
            .unwrap();
        assert_eq!(probe.rows.len(), 1);
    }

    // ─── Batched Phase 4 reader tests (Commit 3 / §T13) ────────────────

    #[test]
    fn batched_phase4_methods_handle_empty_input() {
        let store = make_store();
        // Every batched reader must short-circuit on empty input — Phase 4
        // calls them with `truly_changed` which is empty when only
        // deletions are pending (and would otherwise issue a malformed
        // Cozo query against an empty inline relation).
        let empty: Vec<String> = Vec::new();
        assert!(store.list_dependent_sources_for_many(&empty).unwrap().is_empty());
        assert!(store.count_structural_rows_for_sources(&empty).unwrap().is_empty());
        assert!(store.get_claim_ids_for_sources(&empty).unwrap().is_empty());
        assert!(store.get_source_relation_triples_for_sources(&empty).unwrap().is_empty());
        assert!(store.get_entity_ids_for_sources(&empty).unwrap().is_empty());
    }

    #[test]
    fn count_structural_rows_for_sources_aggregates_per_source_across_tables() {
        let store = make_store();
        // src-1: 2 function_calls + 1 heading = 3 rows.
        // src-2: 1 function_call only = 1 row.
        store
            .insert_function_calls_batch(&[
                FunctionCall {
                    id: "fc-a".into(),
                    caller_claim_id: "c1".into(),
                    callee_name: "foo".into(),
                    callee_claim_id: String::new(),
                    source_id: "src-1".into(),
                    byte_start: 0,
                    byte_end: 10,
                    content_blake3: "b1".into(),
                },
                FunctionCall {
                    id: "fc-b".into(),
                    caller_claim_id: "c1".into(),
                    callee_name: "bar".into(),
                    callee_claim_id: String::new(),
                    source_id: "src-1".into(),
                    byte_start: 20,
                    byte_end: 30,
                    content_blake3: "b2".into(),
                },
                FunctionCall {
                    id: "fc-c".into(),
                    caller_claim_id: "c2".into(),
                    callee_name: "baz".into(),
                    callee_claim_id: String::new(),
                    source_id: "src-2".into(),
                    byte_start: 0,
                    byte_end: 10,
                    content_blake3: "b3".into(),
                },
            ])
            .unwrap();
        store
            .insert_headings_batch(&[HeadingRow {
                id: "h1".into(),
                source_id: "src-1".into(),
                level: 2,
                text: "API".into(),
                parent_heading_id: String::new(),
                byte_start: 40,
                byte_end: 50,
                content_blake3: "b4".into(),
            }])
            .unwrap();

        let counts = store
            .count_structural_rows_for_sources(&[
                "src-1".to_string(),
                "src-2".to_string(),
                "src-missing".to_string(),
            ])
            .unwrap();
        assert_eq!(counts.get("src-1").copied(), Some(3), "2 function_calls + 1 heading");
        assert_eq!(counts.get("src-2").copied(), Some(1), "1 function_call");
        assert_eq!(
            counts.get("src-missing"),
            None,
            "absent ids must not appear in the result map"
        );

        // Batched result must equal the sum of per-source calls — the
        // pre-fix per-source method is the ground truth.
        let single_1 = store.count_structural_rows_for_source("src-1").unwrap();
        let single_2 = store.count_structural_rows_for_source("src-2").unwrap();
        assert_eq!(counts.get("src-1").copied().unwrap_or(0), single_1);
        assert_eq!(counts.get("src-2").copied().unwrap_or(0), single_2);
    }

    #[test]
    fn list_dependent_sources_for_many_unions_targets() {
        let store = make_store();
        // src-A depends on src-1, src-B depends on src-2, src-C depends
        // on src-1 (so src-1 has two upstream dependents).
        store.record_resolution_dep("src-A", "src-1", "function_call", "edge-1").unwrap();
        store.record_resolution_dep("src-B", "src-2", "function_call", "edge-2").unwrap();
        store.record_resolution_dep("src-C", "src-1", "code_link", "edge-3").unwrap();

        // Querying src-1 + src-2 together should return {src-A, src-B,
        // src-C} — the union of dependents. Dedup is implicit (src-1
        // has two upstream rows but src-A only appears once).
        let deps = store
            .list_dependent_sources_for_many(&[
                "src-1".to_string(),
                "src-2".to_string(),
            ])
            .unwrap();
        assert_eq!(deps.len(), 3, "expected union of 3 distinct dependents");
        assert!(deps.contains("src-A"));
        assert!(deps.contains("src-B"));
        assert!(deps.contains("src-C"));

        // Batched result must equal the union of per-source calls.
        let mut single_union: std::collections::HashSet<String> = std::collections::HashSet::new();
        for t in ["src-1", "src-2"] {
            for dep in store.list_dependent_sources(t).unwrap() {
                single_union.insert(dep);
            }
        }
        assert_eq!(deps, single_union, "batched must equal per-target union");
    }

    // ─── Phase 7e scoped reader tests (Tier 1 commit C) ──────────────────

    #[test]
    fn list_function_calls_for_sources_empty_scope_short_circuits() {
        let store = make_store();
        // Seed a row to prove the empty-scope path doesn't return everything.
        store
            .insert_function_calls_batch(&[FunctionCall {
                id: "fc-bg".into(),
                caller_claim_id: "c".into(),
                callee_name: "n".into(),
                callee_claim_id: String::new(),
                source_id: "src-bg".into(),
                byte_start: 0,
                byte_end: 5,
                content_blake3: "b".into(),
            }])
            .unwrap();
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        let rows = store.list_function_calls_for_sources(&empty).unwrap();
        assert!(rows.is_empty(), "empty scope returns empty Vec");
    }

    #[test]
    fn list_function_calls_for_sources_filters_to_scope() {
        let store = make_store();
        store
            .insert_function_calls_batch(&[
                FunctionCall {
                    id: "fc-a".into(),
                    caller_claim_id: "c1".into(),
                    callee_name: "n1".into(),
                    callee_claim_id: String::new(),
                    source_id: "src-A".into(),
                    byte_start: 0,
                    byte_end: 5,
                    content_blake3: "b1".into(),
                },
                FunctionCall {
                    id: "fc-b".into(),
                    caller_claim_id: "c2".into(),
                    callee_name: "n2".into(),
                    callee_claim_id: String::new(),
                    source_id: "src-B".into(),
                    byte_start: 0,
                    byte_end: 5,
                    content_blake3: "b2".into(),
                },
                FunctionCall {
                    id: "fc-c".into(),
                    caller_claim_id: "c3".into(),
                    callee_name: "n3".into(),
                    callee_claim_id: String::new(),
                    source_id: "src-C".into(),
                    byte_start: 0,
                    byte_end: 5,
                    content_blake3: "b3".into(),
                },
            ])
            .unwrap();

        let mut scope = std::collections::HashSet::new();
        scope.insert("src-A".to_string());
        scope.insert("src-C".to_string());

        let rows = store.list_function_calls_for_sources(&scope).unwrap();
        assert_eq!(rows.len(), 2, "scope {{A, C}} must return exactly 2 rows");
        let ids: std::collections::HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        assert!(ids.contains("fc-a"));
        assert!(ids.contains("fc-c"));
        assert!(!ids.contains("fc-b"), "src-B is out of scope");
    }

    #[test]
    fn list_code_links_for_sources_empty_scope_short_circuits() {
        let store = make_store();
        store
            .insert_code_links_batch(&[CodeLink {
                id: "cl-bg".into(),
                source_id: "src-bg".into(),
                chunk_id: "ch".into(),
                url: "http://x".into(),
                link_text: "x".into(),
                is_internal: false,
                target_source_id: String::new(),
                byte_start: 0,
                byte_end: 5,
                content_blake3: "b".into(),
            }])
            .unwrap();
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        let rows = store.list_code_links_for_sources(&empty).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn list_code_links_for_sources_filters_to_scope() {
        let store = make_store();
        store
            .insert_code_links_batch(&[
                CodeLink {
                    id: "cl-a".into(),
                    source_id: "src-A".into(),
                    chunk_id: "ch1".into(),
                    url: "http://a".into(),
                    link_text: "a".into(),
                    is_internal: false,
                    target_source_id: String::new(),
                    byte_start: 0,
                    byte_end: 5,
                    content_blake3: "b1".into(),
                },
                CodeLink {
                    id: "cl-b".into(),
                    source_id: "src-B".into(),
                    chunk_id: "ch2".into(),
                    url: "http://b".into(),
                    link_text: "b".into(),
                    is_internal: false,
                    target_source_id: String::new(),
                    byte_start: 0,
                    byte_end: 5,
                    content_blake3: "b2".into(),
                },
            ])
            .unwrap();

        let mut scope = std::collections::HashSet::new();
        scope.insert("src-B".to_string());

        let rows = store.list_code_links_for_sources(&scope).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "cl-b");
    }
}
