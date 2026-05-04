//! T7 — Transactional per-source rebuild.
//!
//! Closes invariant **I-W4** of the water-flow incremental compile spec
//! (`docs/superpowers/specs/2026-05-04-incremental-compile-water-flow-design.md`):
//! per-source rebuild — cascade-then-emit for one source across the 16
//! structural tables — must execute as ONE atomic CozoDB transaction so
//! AEP/Hybrid concurrent readers can never observe a torn intermediate.
//!
//! ## Strategy: `multi_transaction` (Path A — script fusion via Cozo's tx API)
//!
//! The probe test in
//! `crates/thinkingroot-serve/tests/incremental_concurrency_test.rs::
//! cozo_multi_transaction_rolls_back_on_failure` confirmed two things:
//!
//! 1. A single `db.run_script` call with multiple `;`-separated statements
//!    **does not parse** when each statement starts its own `?[…] := …`
//!    rule — Cozo's grammar treats consecutive `?` rules as a single rule
//!    with multiple bodies, and rejects the script with "Rule ? has
//!    multiple definitions with conflicting heads".
//! 2. `db.multi_transaction(write=true)` opens a transaction handle that
//!    accepts an arbitrary sequence of `run_script` calls and commits
//!    them atomically on `commit()`.  When any one call fails the
//!    transaction can be `abort()`ed and earlier writes are rolled back.
//!
//! Therefore [`GraphStore::transactional_rebuild_source`] uses
//! `multi_transaction(true)`: it issues 16 `:rm` cascade calls (from
//! [`thinkingroot_core::structural_registry::pk_rm_script_for_table`])
//! followed by one `?[…] <- $rows :put <table> {…}` call per table
//! with new rows.  Row payloads flow through the `$rows` parameter as
//! `DataValue::List<DataValue::List<…>>` — the same shape used by the
//! existing per-table batch helpers in `structural_inserts.rs`, so
//! column ordering and primary-key partitioning carry over verbatim.
//! Any error inside the tx triggers `abort()`; success ends in
//! `commit()`.
//!
//! ## Why not 17 separate top-level `run_script` calls
//!
//! Cozo's atomicity guarantee on `run_script` is per-call.  Issuing one
//! `:rm` + N `:put` calls outside a transaction would let a concurrent
//! reader observe e.g. a cascaded `function_calls` table (rows removed)
//! but a still-old `headings` table (rows not yet replaced).  That's
//! the snapshot-consistency breach I-W4 forbids.  `multi_transaction`
//! folds the same N calls into one atomic boundary.
//!
//! ## Empty `PerSourceRows` is intentional
//!
//! Even when every field is empty, the cascade still fires.  Reason: a
//! source whose new emit set is empty for all 16 tables means "this
//! recompile produced no structural rows for that source"; rows from a
//! prior compile are stale by definition and must be cleared.  Phase
//! 6.7's caller relies on this — see `flush_buckets` in
//! `crates/thinkingroot-serve/src/structural_persist.rs`.

use std::collections::BTreeMap;

use cozo::{DataValue, MultiTransaction, Num};
use thinkingroot_core::structural_registry::{pk_rm_script_for_table, STRUCTURAL_TABLES};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;
use crate::rows::{
    CodeLink, CodeMarker, CodeMetric, CodeSignature, ConfigTreeNode, DataRowRow, DocTagRow,
    FunctionCall, GitBlameRow, GitCommit, HeadingRow, QuantityRow, ResidualChunk,
    SourceAnnotation, SourceReference, TestAnnotation,
};

/// New structural rows for one source, grouped per table.  Used by
/// [`GraphStore::transactional_rebuild_source`] to batch a per-source
/// rebuild.  Empty vecs mean "no new rows for this table" — the cascade
/// still fires (the source may have been removed entirely or no longer
/// emits rows for that table).
#[derive(Debug, Default, Clone)]
pub struct PerSourceRows {
    pub function_calls: Vec<FunctionCall>,
    pub headings: Vec<HeadingRow>,
    pub doc_tags: Vec<DocTagRow>,
    pub code_links: Vec<CodeLink>,
    pub code_signatures: Vec<CodeSignature>,
    pub config_tree: Vec<ConfigTreeNode>,
    pub data_rows: Vec<DataRowRow>,
    pub chunks_residual: Vec<ResidualChunk>,
    pub quantities: Vec<QuantityRow>,
    pub source_annotations: Vec<SourceAnnotation>,
    pub source_references: Vec<SourceReference>,
    pub code_markers: Vec<CodeMarker>,
    pub test_annotations: Vec<TestAnnotation>,
    pub git_blame: Vec<GitBlameRow>,
    pub git_commits: Vec<GitCommit>,
    pub code_metrics: Vec<CodeMetric>,
}

impl PerSourceRows {
    /// True iff every per-table vec is empty.  The transactional rebuild
    /// path still cascades for empty rebuilds (see module-level docs);
    /// callers can use this to skip dispatching no-op rebuilds for
    /// sources they never touched in this compile.
    pub fn is_empty(&self) -> bool {
        self.function_calls.is_empty()
            && self.headings.is_empty()
            && self.doc_tags.is_empty()
            && self.code_links.is_empty()
            && self.code_signatures.is_empty()
            && self.config_tree.is_empty()
            && self.data_rows.is_empty()
            && self.chunks_residual.is_empty()
            && self.quantities.is_empty()
            && self.source_annotations.is_empty()
            && self.source_references.is_empty()
            && self.code_markers.is_empty()
            && self.test_annotations.is_empty()
            && self.git_blame.is_empty()
            && self.git_commits.is_empty()
            && self.code_metrics.is_empty()
    }

    /// Run one `:put <table> {…}` script inside `tx` for every table
    /// whose vec is non-empty.  Tables with no new rows are skipped
    /// (the cascade `:rm` already handled their pre-state).  The caller
    /// is responsible for opening the transaction, running the cascade
    /// `:rm`s, and either `commit()`ing or `abort()`ing afterwards.
    pub fn append_put_scripts(&self, tx: &MultiTransaction) -> Result<()> {
        if !self.function_calls.is_empty() {
            run_put(tx, function_calls_put_spec(&self.function_calls))?;
        }
        if !self.headings.is_empty() {
            run_put(tx, headings_put_spec(&self.headings))?;
        }
        if !self.doc_tags.is_empty() {
            run_put(tx, doc_tags_put_spec(&self.doc_tags))?;
        }
        if !self.code_links.is_empty() {
            run_put(tx, code_links_put_spec(&self.code_links))?;
        }
        if !self.code_signatures.is_empty() {
            run_put(tx, code_signatures_put_spec(&self.code_signatures))?;
        }
        if !self.config_tree.is_empty() {
            run_put(tx, config_tree_put_spec(&self.config_tree))?;
        }
        if !self.data_rows.is_empty() {
            run_put(tx, data_rows_put_spec(&self.data_rows))?;
        }
        if !self.chunks_residual.is_empty() {
            run_put(tx, chunks_residual_put_spec(&self.chunks_residual))?;
        }
        if !self.quantities.is_empty() {
            run_put(tx, quantities_put_spec(&self.quantities))?;
        }
        if !self.source_annotations.is_empty() {
            run_put(tx, source_annotations_put_spec(&self.source_annotations))?;
        }
        if !self.source_references.is_empty() {
            run_put(tx, source_references_put_spec(&self.source_references))?;
        }
        if !self.code_markers.is_empty() {
            run_put(tx, code_markers_put_spec(&self.code_markers))?;
        }
        if !self.test_annotations.is_empty() {
            run_put(tx, test_annotations_put_spec(&self.test_annotations))?;
        }
        if !self.git_blame.is_empty() {
            run_put(tx, git_blame_put_spec(&self.git_blame))?;
        }
        if !self.git_commits.is_empty() {
            run_put(tx, git_commits_put_spec(&self.git_commits))?;
        }
        if !self.code_metrics.is_empty() {
            run_put(tx, code_metrics_put_spec(&self.code_metrics))?;
        }
        Ok(())
    }
}

/// One per-table `:put` payload — script text plus its parameter map.
struct PutSpec {
    script: String,
    params: BTreeMap<String, DataValue>,
}

/// Run a [`PutSpec`] inside an open multi-transaction.  Errors are
/// surfaced as [`Error::GraphStorage`] so the caller can abort the tx
/// and propagate.
fn run_put(tx: &MultiTransaction, spec: PutSpec) -> Result<()> {
    tx.run_script(&spec.script, spec.params)
        .map_err(|e| Error::GraphStorage(format!("transactional rebuild :put failed: {e}")))?;
    Ok(())
}

impl GraphStore {
    /// Per-source rebuild — atomic across the 16 structural tables.
    ///
    /// Opens a `multi_transaction(write=true)`, issues 16 `:rm` cascade
    /// scripts (one per structural table) scoped to `source_id`, runs
    /// one `:put` script per table that has new rows, and `commit()`s.
    /// On any error inside the tx the transaction is `abort()`ed so
    /// none of the partial writes survive.  Concurrent AEP/Hybrid
    /// readers therefore see either the pre-state or the post-state
    /// of every table touched, never a torn intermediate.  Closes
    /// invariant I-W4.
    ///
    /// The probe test
    /// `crates/thinkingroot-serve/tests/incremental_concurrency_test.rs::
    /// cozo_multi_transaction_rolls_back_on_failure` pinned the
    /// rollback semantics of `multi_transaction` + `abort()` for both
    /// happy- and sad-path failures.
    pub fn transactional_rebuild_source(
        &self,
        source_id: &str,
        new_rows: &PerSourceRows,
    ) -> Result<()> {
        let tx = self.raw_db().multi_transaction(true);

        let result = (|| -> Result<()> {
            // 1. Cascade: 16 :rm scripts scoped by $sid.  Order matches
            //    STRUCTURAL_TABLES; each script uses the table's
            //    primary-key projection (handles composite-key tables
            //    transparently).
            for spec in STRUCTURAL_TABLES {
                let script = pk_rm_script_for_table(spec.name, spec.source_id_column);
                let mut params: BTreeMap<String, DataValue> = BTreeMap::new();
                params.insert("sid".into(), DataValue::Str(source_id.into()));
                tx.run_script(&script, params).map_err(|e| {
                    Error::GraphStorage(format!(
                        "transactional rebuild for source {source_id}: cascade :rm on {} failed: {e}",
                        spec.name,
                    ))
                })?;
            }

            // 2. Per-table inserts (only for tables with new rows).
            new_rows.append_put_scripts(&tx).map_err(|e| match e {
                Error::GraphStorage(msg) => Error::GraphStorage(format!(
                    "transactional rebuild for source {source_id}: {msg}"
                )),
                other => other,
            })?;

            Ok(())
        })();

        match result {
            Ok(()) => {
                tx.commit().map_err(|e| {
                    Error::GraphStorage(format!(
                        "transactional rebuild for source {source_id}: commit failed: {e}"
                    ))
                })?;
                Ok(())
            }
            Err(e) => {
                // Best-effort abort.  If abort itself fails, prefer
                // surfacing the original error to the caller — that's
                // the more diagnostic of the two.
                let _ = tx.abort();
                Err(e)
            }
        }
    }
}

// ─── Per-table put-block emitters ─────────────────────────────────────
//
// Each helper mirrors the `:put` shape of the corresponding
// `insert_<table>_batch` method in `structural_inserts.rs` exactly:
// rows are bundled as `DataValue::List` of `DataValue::List`s, bound to
// the parameter `$rows`, and inserted with the same `?[…] <- $rows
// :put <table> {pk => non_pk}` script.  This is the same shape Cozo's
// existing batch-insert code path uses, so the round-trip semantics
// (column ordering, primary-key partition, type coercion) are
// guaranteed to match.

fn s(v: impl Into<String>) -> DataValue {
    DataValue::Str(v.into().into())
}

fn i(v: i64) -> DataValue {
    DataValue::Num(Num::Int(v))
}

fn f(v: f64) -> DataValue {
    DataValue::Num(Num::Float(v))
}

fn b(v: bool) -> DataValue {
    DataValue::Bool(v)
}

/// Build a [`PutSpec`] from a static script template and the per-row
/// `DataValue::List` payload.  Centralises the `params.insert("rows",
/// DataValue::List(payload))` glue so each per-table helper is just
/// "the cell projection."
fn put_spec(script: &'static str, payload: Vec<DataValue>) -> PutSpec {
    let mut params = BTreeMap::new();
    params.insert("rows".into(), DataValue::List(payload));
    PutSpec {
        script: script.to_string(),
        params,
    }
}

fn function_calls_put_spec(rows: &[FunctionCall]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3] <- $rows \
         :put function_calls {id => caller_claim_id, callee_name, callee_claim_id, source_id, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn headings_put_spec(rows: &[HeadingRow]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, source_id, level, text, parent_heading_id, byte_start, byte_end, content_blake3] <- $rows \
         :put headings {id => source_id, level, text, parent_heading_id, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn doc_tags_put_spec(rows: &[DocTagRow]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, claim_id, kind, target, description, source_id, byte_start, byte_end, content_blake3] <- $rows \
         :put doc_tags {id => claim_id, kind, target, description, source_id, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn code_links_put_spec(rows: &[CodeLink]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3] <- $rows \
         :put code_links {id => source_id, chunk_id, url, link_text, is_internal, target_source_id, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn code_signatures_put_spec(rows: &[CodeSignature]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[claim_id, parameters_json, return_type, visibility, trait_name, parent_scope, field_types_json, source_id, byte_start, byte_end, content_blake3] <- $rows \
         :put code_signatures {claim_id => parameters_json, return_type, visibility, trait_name, parent_scope, field_types_json, source_id, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn config_tree_put_spec(rows: &[ConfigTreeNode]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[source_id, dotted_path, value, value_type, byte_start, byte_end, content_blake3] <- $rows \
         :put config_tree {source_id, dotted_path => value, value_type, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn data_rows_put_spec(rows: &[DataRowRow]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, source_id, row_index, columns_json, byte_start, byte_end, content_blake3] <- $rows \
         :put data_rows {id => source_id, row_index, columns_json, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn chunks_residual_put_spec(rows: &[ResidualChunk]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, source_id, chunk_type, content, metadata_json, byte_start, byte_end, content_blake3] <- $rows \
         :put chunks_residual {id => source_id, chunk_type, content, metadata_json, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn quantities_put_spec(rows: &[QuantityRow]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, claim_id, metric_name, value, unit, qualifier, is_live, captured_at, source_id, byte_start, byte_end, content_blake3] <- $rows \
         :put quantities {id => claim_id, metric_name, value, unit, qualifier, is_live, captured_at, source_id, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn source_annotations_put_spec(rows: &[SourceAnnotation]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, source_id, kind, value, byte_start, byte_end, content_blake3] <- $rows \
         :put source_annotations {id => source_id, kind, value, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn source_references_put_spec(rows: &[SourceReference]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, from_source_id, to_source_id, reference_kind, fragment, byte_start, byte_end, content_blake3] <- $rows \
         :put source_references {id => from_source_id, to_source_id, reference_kind, fragment, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn code_markers_put_spec(rows: &[CodeMarker]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, source_id, kind, text, in_claim_id, byte_start, byte_end, content_blake3] <- $rows \
         :put code_markers {id => source_id, kind, text, in_claim_id, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn test_annotations_put_spec(rows: &[TestAnnotation]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, source_id, claim_id, framework, annotation_kind, name, byte_start, byte_end, content_blake3] <- $rows \
         :put test_annotations {id => source_id, claim_id, framework, annotation_kind, name, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn git_blame_put_spec(rows: &[GitBlameRow]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[source_id, line_start, line_end, commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3] <- $rows \
         :put git_blame {source_id, line_start, line_end => commit_sha, author, author_email, blamed_at, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn git_commits_put_spec(rows: &[GitCommit]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[source_id, commit_sha, commit_author, commit_email, commit_timestamp, changed_files_json, message, parent_sha, byte_start, byte_end, content_blake3] <- $rows \
         :put git_commits {source_id, commit_sha => commit_author, commit_email, commit_timestamp, changed_files_json, message, parent_sha, byte_start, byte_end, content_blake3}",
        payload,
    )
}

fn code_metrics_put_spec(rows: &[CodeMetric]) -> PutSpec {
    let payload: Vec<DataValue> = rows
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
    put_spec(
        "?[id, source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method, byte_start, byte_end, content_blake3] <- $rows \
         :put code_metrics {id => source_id, scope, scope_claim_id, loc, cyclomatic, fan_in, fan_out, complexity_method, byte_start, byte_end, content_blake3}",
        payload,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_store() -> GraphStore {
        let dir = tempdir().unwrap();
        let path = dir.into_path();
        GraphStore::init(&path).unwrap()
    }

    #[test]
    fn empty_per_source_rows_is_empty() {
        let rows = PerSourceRows::default();
        assert!(rows.is_empty());
    }

    #[test]
    fn non_empty_per_source_rows_is_not_empty() {
        let mut rows = PerSourceRows::default();
        rows.function_calls.push(FunctionCall {
            id: "fc-1".into(),
            caller_claim_id: String::new(),
            callee_name: "x".into(),
            callee_claim_id: String::new(),
            source_id: "src".into(),
            byte_start: 0,
            byte_end: 1,
            content_blake3: "b".into(),
        });
        assert!(!rows.is_empty());
    }

    #[test]
    fn empty_rebuild_just_cascades() {
        let store = make_store();
        // Cascade against a brand-new source_id with nothing to put — the
        // call must still succeed (no rows to clean, but no error).
        let rows = PerSourceRows::default();
        store
            .transactional_rebuild_source("nonexistent-source", &rows)
            .unwrap();
    }

    #[test]
    fn rebuild_inserts_new_rows() {
        let store = make_store();
        let mut rows = PerSourceRows::default();
        rows.function_calls.push(FunctionCall {
            id: "fc-rb-1".into(),
            caller_claim_id: "c".into(),
            callee_name: "n".into(),
            callee_claim_id: String::new(),
            source_id: "src-rb-1".into(),
            byte_start: 0,
            byte_end: 5,
            content_blake3: "b".into(),
        });
        rows.headings.push(HeadingRow {
            id: "h-rb-1".into(),
            source_id: "src-rb-1".into(),
            level: 1,
            text: "Heading".into(),
            parent_heading_id: String::new(),
            byte_start: 5,
            byte_end: 15,
            content_blake3: "b2".into(),
        });
        store
            .transactional_rebuild_source("src-rb-1", &rows)
            .unwrap();

        // Verify both inserts landed.
        let probe_fc = store
            .query_read("?[id] := *function_calls{id, source_id: 'src-rb-1'}")
            .unwrap();
        assert_eq!(probe_fc.rows.len(), 1);
        let probe_h = store
            .query_read("?[id] := *headings{id, source_id: 'src-rb-1'}")
            .unwrap();
        assert_eq!(probe_h.rows.len(), 1);
    }

    #[test]
    fn rebuild_clears_then_inserts() {
        let store = make_store();
        // First compile: 3 function_calls.
        let mut rows = PerSourceRows::default();
        for k in 0..3 {
            rows.function_calls.push(FunctionCall {
                id: format!("fc-old-{k}"),
                caller_claim_id: "c".into(),
                callee_name: format!("old-{k}"),
                callee_claim_id: String::new(),
                source_id: "src-rb-2".into(),
                byte_start: k * 10,
                byte_end: k * 10 + 5,
                content_blake3: format!("b-old-{k}"),
            });
        }
        store
            .transactional_rebuild_source("src-rb-2", &rows)
            .unwrap();
        let probe = store
            .query_read("?[id] := *function_calls{id, source_id: 'src-rb-2'}")
            .unwrap();
        assert_eq!(probe.rows.len(), 3);

        // Second compile: 1 function_call.  Cascade clears the 3 old rows;
        // put inserts the 1 new row.
        let mut rows = PerSourceRows::default();
        rows.function_calls.push(FunctionCall {
            id: "fc-new-0".into(),
            caller_claim_id: "c".into(),
            callee_name: "new-0".into(),
            callee_claim_id: String::new(),
            source_id: "src-rb-2".into(),
            byte_start: 100,
            byte_end: 105,
            content_blake3: "b-new-0".into(),
        });
        store
            .transactional_rebuild_source("src-rb-2", &rows)
            .unwrap();
        let probe = store
            .query_read("?[id] := *function_calls{id, source_id: 'src-rb-2'}")
            .unwrap();
        assert_eq!(probe.rows.len(), 1);
    }

    #[test]
    fn rebuild_only_touches_target_source() {
        let store = make_store();
        // Insert rows for source A.
        let mut rows_a = PerSourceRows::default();
        rows_a.function_calls.push(FunctionCall {
            id: "fc-a".into(),
            caller_claim_id: "c".into(),
            callee_name: "a".into(),
            callee_claim_id: String::new(),
            source_id: "src-a".into(),
            byte_start: 0,
            byte_end: 5,
            content_blake3: "b-a".into(),
        });
        store.transactional_rebuild_source("src-a", &rows_a).unwrap();

        // Now rebuild source B — must not affect A.
        let mut rows_b = PerSourceRows::default();
        rows_b.function_calls.push(FunctionCall {
            id: "fc-b".into(),
            caller_claim_id: "c".into(),
            callee_name: "b".into(),
            callee_claim_id: String::new(),
            source_id: "src-b".into(),
            byte_start: 0,
            byte_end: 5,
            content_blake3: "b-b".into(),
        });
        store.transactional_rebuild_source("src-b", &rows_b).unwrap();

        let probe_a = store
            .query_read("?[id] := *function_calls{id, source_id: 'src-a'}")
            .unwrap();
        assert_eq!(probe_a.rows.len(), 1, "source A's row must survive rebuild of B");

        let probe_b = store
            .query_read("?[id] := *function_calls{id, source_id: 'src-b'}")
            .unwrap();
        assert_eq!(probe_b.rows.len(), 1);
    }
}
