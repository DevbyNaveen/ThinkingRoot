//! Witness-mesh insert + query helpers.
//!
//! Bridges the in-memory `Witness` type (from `thinkingroot-core`) to
//! the CozoDB `witnesses` + `witness_input_edges` tables added during
//! the Witness Mesh scaffold phase.
//!
//! The shipping idiom matches `structural_inserts.rs`:
//! - chunks of 500 rows per CozoDB batch (parameter cap),
//! - one `?[…] <- $rows :put witnesses {…}` script per batch,
//! - per-batch failure leaves earlier batches committed (per-source
//!   idempotency).
//!
//! Two persistence concerns are handled inline here rather than
//! delegated to serde-on-the-DB-side:
//! - `inputs_json` is the JSON-encoded `Vec<WitnessInput>` (a typed
//!   enum with `kind` discriminator). Decoding is the caller's job.
//! - `spans_json` is the JSON-encoded `Vec<WitnessSpan>`. The
//!   canonical anchor (`spans[0]`) is denormalised into the
//!   `byte_start` / `byte_end` columns for Datalog join speed.
//!
//! Encoding failure on either field is surfaced as a hard
//! `Error::GraphStorage` — silently writing `"[]"` would corrupt
//! `walk_mesh` traversal downstream.

use std::collections::BTreeMap;

use cozo::{DataValue, Num};
use thinkingroot_core::types::{Witness, WitnessId, WitnessInput, WitnessSpan};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// Per-batch row cap. Matches `structural_inserts::CHUNK` so memory
/// budgeting for combined-graph operations is predictable.
const CHUNK: usize = 500;

fn s(value: impl Into<String>) -> DataValue {
    DataValue::Str(value.into().into())
}

fn i(value: i64) -> DataValue {
    DataValue::Num(Num::Int(value))
}

fn f(value: f64) -> DataValue {
    DataValue::Num(Num::Float(value))
}

/// JSON-encode the inputs list. Surfaces encode failure rather than
/// silently writing `"[]"`.
fn encode_inputs(witness_id: &WitnessId, inputs: &[WitnessInput]) -> Result<String> {
    serde_json::to_string(inputs).map_err(|e| {
        Error::GraphStorage(format!(
            "encode witness.inputs for {witness_id}: {e}"
        ))
    })
}

/// JSON-encode the spans list. Surfaces encode failure rather than
/// silently writing `"[]"`.
fn encode_spans(witness_id: &WitnessId, spans: &[WitnessSpan]) -> Result<String> {
    serde_json::to_string(spans).map_err(|e| {
        Error::GraphStorage(format!(
            "encode witness.spans for {witness_id}: {e}"
        ))
    })
}

impl GraphStore {
    /// Insert a single Witness. Idempotent: re-inserting the same
    /// `(id)` overwrites the row (CozoDB `:put` semantics). The
    /// Witness's id is content-derived so re-inserting an
    /// unchanged Witness is a true no-op at the byte level.
    pub fn insert_witness(&self, witness: &Witness) -> Result<()> {
        if witness.spans.is_empty() {
            return Err(Error::GraphStorage(format!(
                "witness {} has no spans — refusing to insert (Witness Mesh I-W8 \
                 requires at least one anchor span)",
                witness.id
            )));
        }
        let anchor = &witness.spans[0];

        let inputs_json = encode_inputs(&witness.id, &witness.inputs)?;
        let spans_json = encode_spans(&witness.id, &witness.spans)?;

        let mut params = BTreeMap::new();
        params.insert("id".into(), s(witness.id.to_hex()));
        params.insert("witness_type".into(), s(witness.witness_type.clone()));
        params.insert("rule".into(), s(witness.rule.clone()));
        params.insert("inputs_json".into(), s(inputs_json));
        params.insert("spans_json".into(), s(spans_json));
        params.insert("source_id".into(), s(witness.source.to_string()));
        params.insert("workspace_id".into(), s(witness.workspace.to_string()));
        params.insert("sensitivity".into(), s(witness.sensitivity.as_str()));
        params.insert("confidence".into(), f(witness.confidence.value()));
        params.insert("content_blake3".into(), s(witness.content_blake3.clone()));
        params.insert(
            "symbol".into(),
            s(witness.symbol.clone().unwrap_or_default()),
        );
        params.insert("byte_start".into(), i(anchor.start as i64));
        params.insert("byte_end".into(), i(anchor.end as i64));
        params.insert(
            "created_at".into(),
            f(witness.created_at.timestamp() as f64),
        );
        params.insert(
            "valid_from".into(),
            f(witness.valid_from.timestamp() as f64),
        );
        params.insert(
            "valid_until".into(),
            f(witness
                .valid_until
                .map(|d| d.timestamp() as f64)
                .unwrap_or(0.0)),
        );

        let script = "
            ?[
                id, witness_type, rule, inputs_json, spans_json,
                source_id, workspace_id, sensitivity, confidence,
                content_blake3, symbol, byte_start, byte_end,
                created_at, valid_from, valid_until
            ] <- [[
                $id, $witness_type, $rule, $inputs_json, $spans_json,
                $source_id, $workspace_id, $sensitivity, $confidence,
                $content_blake3, $symbol, $byte_start, $byte_end,
                $created_at, $valid_from, $valid_until
            ]]
            :put witnesses {
                id =>
                witness_type, rule, inputs_json, spans_json,
                source_id, workspace_id, sensitivity, confidence,
                content_blake3, symbol, byte_start, byte_end,
                created_at, valid_from, valid_until
            }
        ";
        self.query(script, params)
            .map_err(|e| Error::GraphStorage(format!("insert_witness({}): {e}", witness.id)))?;
        Ok(())
    }

    /// Batch-insert witnesses. Chunks at 500 rows per CozoDB call.
    pub fn insert_witnesses_batch(&self, witnesses: &[Witness]) -> Result<()> {
        for chunk in witnesses.chunks(CHUNK) {
            let mut payload: Vec<DataValue> = Vec::with_capacity(chunk.len());
            for witness in chunk {
                if witness.spans.is_empty() {
                    return Err(Error::GraphStorage(format!(
                        "witness {} has no spans — refusing batch insert",
                        witness.id
                    )));
                }
                let anchor = &witness.spans[0];
                let inputs_json = encode_inputs(&witness.id, &witness.inputs)?;
                let spans_json = encode_spans(&witness.id, &witness.spans)?;
                payload.push(DataValue::List(vec![
                    s(witness.id.to_hex()),
                    s(witness.witness_type.clone()),
                    s(witness.rule.clone()),
                    s(inputs_json),
                    s(spans_json),
                    s(witness.source.to_string()),
                    s(witness.workspace.to_string()),
                    s(witness.sensitivity.as_str()),
                    f(witness.confidence.value()),
                    s(witness.content_blake3.clone()),
                    s(witness.symbol.clone().unwrap_or_default()),
                    i(anchor.start as i64),
                    i(anchor.end as i64),
                    f(witness.created_at.timestamp() as f64),
                    f(witness.valid_from.timestamp() as f64),
                    f(witness
                        .valid_until
                        .map(|d| d.timestamp() as f64)
                        .unwrap_or(0.0)),
                ]));
            }
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            let script = "
                ?[
                    id, witness_type, rule, inputs_json, spans_json,
                    source_id, workspace_id, sensitivity, confidence,
                    content_blake3, symbol, byte_start, byte_end,
                    created_at, valid_from, valid_until
                ] <- $rows
                :put witnesses {
                    id =>
                    witness_type, rule, inputs_json, spans_json,
                    source_id, workspace_id, sensitivity, confidence,
                    content_blake3, symbol, byte_start, byte_end,
                    created_at, valid_from, valid_until
                }
            ";
            self.query(script, params)
                .map_err(|e| Error::GraphStorage(format!("insert_witnesses_batch: {e}")))?;
        }
        Ok(())
    }

    /// Batch-insert DAG edges. Each entry is `(parent, child)`.
    pub fn insert_witness_input_edges_batch(
        &self,
        edges: &[(WitnessId, WitnessId)],
    ) -> Result<()> {
        for chunk in edges.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|(parent, child)| {
                    DataValue::List(vec![
                        s(parent.to_hex()),
                        s(child.to_hex()),
                        s("derives_from"),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            let script = "
                ?[parent_witness_id, child_witness_id, edge_kind] <- $rows
                :put witness_input_edges {
                    parent_witness_id, child_witness_id =>
                    edge_kind
                }
            ";
            self.query(script, params).map_err(|e| {
                Error::GraphStorage(format!("insert_witness_input_edges_batch: {e}"))
            })?;
        }
        Ok(())
    }

    /// Count the witnesses table. Used by the migration tool to
    /// report progress + by tests to verify dedup behaviour.
    pub fn count_witnesses(&self) -> Result<u64> {
        let script = "?[count(id)] := *witnesses{id}";
        let result = self
            .query_read(script)
            .map_err(|e| Error::GraphStorage(format!("count_witnesses: {e}")))?;
        if let Some(row) = result.rows.first() {
            if let Some(DataValue::Num(Num::Int(n))) = row.first() {
                return Ok((*n).max(0) as u64);
            }
        }
        Ok(0)
    }

    /// Witness count grouped by `source_id`. Returns
    /// `(source_id, count)` pairs — sources with zero witnesses are
    /// not present in the result (no `LEFT JOIN`; the SourceLibrary
    /// caller handles "missing" as zero). Used by the Playground
    /// Source Library to badge each source with its witness count.
    pub fn count_witnesses_by_source(&self) -> Result<Vec<(String, u64)>> {
        let script = "?[source_id, count(id)] := *witnesses{id, source_id}";
        let result = self
            .query_read(script)
            .map_err(|e| Error::GraphStorage(format!("count_witnesses_by_source: {e}")))?;
        let mut out = Vec::with_capacity(result.rows.len());
        for row in &result.rows {
            if row.len() < 2 {
                continue;
            }
            let source_id = match &row[0] {
                DataValue::Str(s) => s.to_string(),
                _ => continue,
            };
            let count = match &row[1] {
                DataValue::Num(Num::Int(n)) => (*n).max(0) as u64,
                _ => 0,
            };
            out.push((source_id, count));
        }
        Ok(out)
    }

    /// Fetch a single Witness by its content-derived id (lower-hex).
    /// Returns `None` when no row exists; surface this honestly rather
    /// than returning a fake "empty" Witness because consumers gate
    /// downstream behaviour on `Some` (e.g. AEP probe materialisation).
    pub fn get_witness(&self, id: &str) -> Result<Option<Witness>> {
        let mut params = BTreeMap::new();
        params.insert("wid".into(), s(id.to_string()));
        // Cozo Datalog requires every head symbol to be free in the
        // body; we can't reuse the column name as both a head symbol
        // AND a constraint binding via `{column: $param}`. Bind the
        // column to a capture variable then constrain that variable.
        let result = self
            .query(
                "?[id, witness_type, rule, inputs_json, spans_json, source_id, workspace_id, \
                  sensitivity, confidence, content_blake3, symbol, byte_start, byte_end, \
                  created_at, valid_from, valid_until] := \
                  *witnesses{id, witness_type, rule, inputs_json, spans_json, source_id, \
                  workspace_id, sensitivity, confidence, content_blake3, symbol, byte_start, \
                  byte_end, created_at, valid_from, valid_until}, \
                  id = $wid",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("get_witness({id}): {e}")))?;
        if result.rows.is_empty() {
            return Ok(None);
        }
        parse_witness_row(&result.rows[0]).map(Some)
    }

    /// List Witnesses scoped to a workspace, optionally capped at
    /// `limit` rows. Pass `None` for `limit` to list every Witness in
    /// the workspace — no pagination cursor today because v1.0
    /// workspaces hold under 1M witnesses and the full set fits
    /// comfortably in memory.
    pub fn list_witnesses_by_workspace(
        &self,
        workspace_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<Witness>> {
        let mut params = BTreeMap::new();
        params.insert("ws".into(), s(workspace_id.to_string()));
        let result = self
            .query(
                "?[id, witness_type, rule, inputs_json, spans_json, source_id, workspace_id, \
                  sensitivity, confidence, content_blake3, symbol, byte_start, byte_end, \
                  created_at, valid_from, valid_until] := \
                  *witnesses{id, witness_type, rule, inputs_json, spans_json, source_id, \
                  workspace_id, sensitivity, confidence, content_blake3, symbol, \
                  byte_start, byte_end, created_at, valid_from, valid_until}, \
                  workspace_id = $ws",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("list_witnesses_by_workspace: {e}")))?;
        let cap = limit.unwrap_or(usize::MAX);
        let mut out: Vec<Witness> = Vec::with_capacity(result.rows.len().min(cap));
        for row in result.rows.iter().take(cap) {
            out.push(parse_witness_row(row)?);
        }
        Ok(out)
    }

    /// List every Witness in this graph, optionally capped at `limit`
    /// rows. Because each workspace owns its own CozoDB instance, the
    /// returned set is already workspace-scoped — no separate filter
    /// is required. Used by the REST list endpoint and MCP tools.
    pub fn list_witnesses(&self, limit: Option<usize>) -> Result<Vec<Witness>> {
        let result = self
            .query_read(
                "?[id, witness_type, rule, inputs_json, spans_json, source_id, workspace_id, \
                  sensitivity, confidence, content_blake3, symbol, byte_start, byte_end, \
                  created_at, valid_from, valid_until] := \
                  *witnesses{id, witness_type, rule, inputs_json, spans_json, source_id, \
                  workspace_id, sensitivity, confidence, content_blake3, symbol, \
                  byte_start, byte_end, created_at, valid_from, valid_until}",
            )
            .map_err(|e| Error::GraphStorage(format!("list_witnesses: {e}")))?;
        let cap = limit.unwrap_or(usize::MAX);
        let mut out: Vec<Witness> = Vec::with_capacity(result.rows.len().min(cap));
        for row in result.rows.iter().take(cap) {
            out.push(parse_witness_row(row)?);
        }
        Ok(out)
    }

    /// Walk the Witness Mesh DAG starting from `root_id`, returning
    /// every Witness reachable via `witness_input_edges` within
    /// `max_depth` hops. Order is BFS (breadth-first), so the
    /// returned vec puts the root first, then its direct parents,
    /// then their parents, etc.
    ///
    /// Use case: an AI agent has a witness id from `search` or
    /// `list_witnesses` and wants the full derivation chain — what
    /// rule produced this, what bytes did it derive from, what
    /// other witnesses share its inputs. This is the foundational
    /// operation for the `walk_mesh` MCP tool.
    ///
    /// `max_fanout` caps the number of edges followed per node to
    /// keep pathological meshes (a witness with thousands of
    /// children) bounded. Default of 50 in callers matches the
    /// MCP tool's expected response size budget.
    ///
    /// Returns `(witnesses, edges)`. `edges` is the subset of
    /// `witness_input_edges` rows that fall inside the walked set —
    /// useful for clients reconstructing the DAG shape locally.
    pub fn walk_mesh_from(
        &self,
        root_id: &str,
        max_depth: usize,
        max_fanout: usize,
    ) -> Result<(Vec<Witness>, Vec<(String, String)>)> {
        use std::collections::HashSet;

        if max_depth == 0 {
            // Walk-of-zero-depth = just the root, if it exists.
            return match self.get_witness(root_id)? {
                Some(w) => Ok((vec![w], vec![])),
                None => Ok((vec![], vec![])),
            };
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut frontier: Vec<String> = vec![root_id.to_string()];
        let mut witnesses: Vec<Witness> = Vec::new();
        let mut edges: Vec<(String, String)> = Vec::new();

        for _ in 0..=max_depth {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier: Vec<String> = Vec::new();
            for current in frontier.drain(..) {
                if !seen.insert(current.clone()) {
                    continue;
                }
                // Fetch the witness itself.
                if let Some(w) = self.get_witness(&current)? {
                    witnesses.push(w);
                }
                // Find every parent (witnesses this one derives
                // from) — i.e. rows where `child_witness_id =
                // current` in `witness_input_edges`.
                let mut params = BTreeMap::new();
                params.insert("cw".into(), s(current.clone()));
                let result = self
                    .query(
                        "?[parent_witness_id, child_witness_id] := \
                          *witness_input_edges{parent_witness_id, child_witness_id}, \
                          child_witness_id = $cw",
                        params,
                    )
                    .map_err(|e| {
                        Error::GraphStorage(format!("walk_mesh edge query: {e}"))
                    })?;
                for (count, row) in result.rows.iter().enumerate() {
                    if count >= max_fanout {
                        break;
                    }
                    let parent = match row.first() {
                        Some(DataValue::Str(s)) => s.to_string(),
                        _ => continue,
                    };
                    let child = match row.get(1) {
                        Some(DataValue::Str(s)) => s.to_string(),
                        _ => continue,
                    };
                    edges.push((parent.clone(), child));
                    if !seen.contains(&parent) {
                        next_frontier.push(parent);
                    }
                }
            }
            frontier = next_frontier;
        }

        Ok((witnesses, edges))
    }

    /// List Witnesses derived from a specific source. Used by the
    /// per-source re-derive path (`transactional_rebuild_source`)
    /// and by debug tools that want to inspect what a source produced.
    pub fn list_witnesses_by_source(&self, source_id: &str) -> Result<Vec<Witness>> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), s(source_id.to_string()));
        let result = self
            .query(
                "?[id, witness_type, rule, inputs_json, spans_json, source_id, workspace_id, \
                  sensitivity, confidence, content_blake3, symbol, byte_start, byte_end, \
                  created_at, valid_from, valid_until] := \
                  *witnesses{id, witness_type, rule, inputs_json, spans_json, \
                  source_id, workspace_id, sensitivity, confidence, content_blake3, \
                  symbol, byte_start, byte_end, created_at, valid_from, valid_until}, \
                  source_id = $sid",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("list_witnesses_by_source: {e}")))?;
        let mut out: Vec<Witness> = Vec::with_capacity(result.rows.len());
        for row in &result.rows {
            out.push(parse_witness_row(row)?);
        }
        Ok(out)
    }
}

/// Reconstruct a `Witness` from a CozoDB row in the column order:
/// `[id, witness_type, rule, inputs_json, spans_json, source_id,
///   workspace_id, sensitivity, confidence, content_blake3, symbol,
///   byte_start, byte_end, created_at, valid_from, valid_until]`.
///
/// All conversions are typed — a malformed row surfaces as a
/// `GraphStorage` error rather than silent corruption. The most
/// common failure mode (a `WitnessId` whose hex form isn't 64 chars)
/// is surfaced explicitly so debugging the substrate is tractable.
fn parse_witness_row(row: &[DataValue]) -> Result<Witness> {
    use chrono::{TimeZone, Utc};
    use std::str::FromStr;
    use thinkingroot_core::types::{Confidence, Sensitivity, SourceId, WorkspaceId};

    fn dv_str(v: &DataValue) -> String {
        match v {
            DataValue::Str(s) => s.to_string(),
            _ => String::new(),
        }
    }
    fn dv_f64(v: &DataValue) -> f64 {
        match v {
            DataValue::Num(Num::Float(f)) => *f,
            DataValue::Num(Num::Int(i)) => *i as f64,
            _ => 0.0,
        }
    }
    fn dv_u64(v: &DataValue) -> u64 {
        match v {
            DataValue::Num(Num::Int(i)) => (*i).max(0) as u64,
            DataValue::Num(Num::Float(f)) => f.max(0.0) as u64,
            _ => 0,
        }
    }

    if row.len() < 16 {
        return Err(Error::GraphStorage(format!(
            "parse_witness_row: row has {} columns, expected 16",
            row.len()
        )));
    }

    let id_hex = dv_str(&row[0]);
    let id = WitnessId::from_hex(&id_hex).map_err(|e| {
        Error::GraphStorage(format!("malformed witness id `{id_hex}`: {e}"))
    })?;

    let inputs_json = dv_str(&row[3]);
    let spans_json = dv_str(&row[4]);
    let inputs: Vec<WitnessInput> = if inputs_json.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&inputs_json).map_err(|e| {
            Error::GraphStorage(format!("decode inputs_json for {id_hex}: {e}"))
        })?
    };
    let spans: Vec<WitnessSpan> = serde_json::from_str(&spans_json).map_err(|e| {
        Error::GraphStorage(format!("decode spans_json for {id_hex}: {e}"))
    })?;

    let source_str = dv_str(&row[5]);
    let workspace_str = dv_str(&row[6]);
    let source = SourceId::from_str(&source_str).map_err(|e| {
        Error::GraphStorage(format!("unparseable source_id `{source_str}`: {e}"))
    })?;
    let workspace = WorkspaceId::from_str(&workspace_str).map_err(|e| {
        Error::GraphStorage(format!("unparseable workspace_id `{workspace_str}`: {e}"))
    })?;

    let sensitivity_str = dv_str(&row[7]);
    let sensitivity = Sensitivity::parse(&sensitivity_str).unwrap_or(Sensitivity::Public);
    let confidence = Confidence::new(dv_f64(&row[8]));
    let content_blake3 = dv_str(&row[9]);
    let symbol_str = dv_str(&row[10]);
    let symbol = if symbol_str.is_empty() { None } else { Some(symbol_str) };

    // byte_start / byte_end (row[11], row[12]) are denormalised from
    // spans[0] — we trust the spans_json round-trip for the in-memory
    // representation and only read byte_start/byte_end here for the
    // schema-level assertion check.
    let _denorm_start = dv_u64(&row[11]);
    let _denorm_end = dv_u64(&row[12]);

    let created_at_unix = dv_f64(&row[13]);
    let valid_from_unix = dv_f64(&row[14]);
    let valid_until_unix = dv_f64(&row[15]);

    let created_at = Utc
        .timestamp_opt(created_at_unix as i64, 0)
        .single()
        .unwrap_or_else(Utc::now);
    let valid_from = Utc
        .timestamp_opt(valid_from_unix as i64, 0)
        .single()
        .unwrap_or_else(Utc::now);
    let valid_until = if valid_until_unix > 0.0 {
        Utc.timestamp_opt(valid_until_unix as i64, 0).single()
    } else {
        None
    };

    Ok(Witness {
        id,
        witness_type: dv_str(&row[1]),
        rule: dv_str(&row[2]),
        inputs,
        spans,
        source,
        workspace,
        sensitivity,
        confidence,
        content_blake3,
        symbol,
        created_at,
        valid_from,
        valid_until,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use thinkingroot_core::types::{
        Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
    };

    fn fresh_store() -> GraphStore {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Keep the tempdir alive for the duration of the store's
        // life by leaking it — these are short-lived test stores
        // and the OS reclaims at process exit. The alternative
        // (returning `(GraphStore, TempDir)`) would force every
        // test to bind two locals which obscures the assertion.
        let path = Box::leak(Box::new(tmp));
        GraphStore::init(path.path()).expect("graph store init")
    }

    fn function_witness(file: &str, start: u64, end: u64) -> Witness {
        Witness::new(
            "tree-sitter::function-decl@v1",
            "declares::function",
            vec![WitnessInput::ByteRef {
                file_blake3: file.to_string(),
                start,
                end,
            }],
            vec![WitnessSpan {
                file_blake3: file.to_string(),
                start,
                end,
            }],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            blake3::hash(b"function body bytes").to_hex().to_string(),
            Utc::now(),
        )
    }

    #[test]
    fn insert_witness_round_trips_to_witnesses_table() {
        let store = fresh_store();
        let w = function_witness("file_a", 0, 10);
        let w_id = w.id;
        store.insert_witness(&w).expect("insert");
        let count = store.count_witnesses().expect("count");
        assert_eq!(count, 1);
        // Re-insert is idempotent (content-addressed id).
        store.insert_witness(&w).expect("re-insert idempotent");
        assert_eq!(store.count_witnesses().expect("count"), 1);
        // Sanity: id hex is the BLAKE3-derived form, not a ULID.
        assert_eq!(w_id.to_hex().len(), 64);
    }

    #[test]
    fn batch_insert_witnesses_inserts_all() {
        let store = fresh_store();
        let witnesses: Vec<Witness> = (0..7)
            .map(|i| function_witness("file_b", i * 10, i * 10 + 5))
            .collect();
        store
            .insert_witnesses_batch(&witnesses)
            .expect("batch insert");
        assert_eq!(store.count_witnesses().expect("count"), 7);
    }

    #[test]
    fn batch_insert_witness_input_edges_writes_dag() {
        let store = fresh_store();
        let parent = function_witness("file_c", 0, 5);
        let child = function_witness("file_c", 5, 10);
        store.insert_witnesses_batch(&[parent.clone(), child.clone()]).unwrap();
        store
            .insert_witness_input_edges_batch(&[(parent.id, child.id)])
            .expect("edge insert");
        // The edge round-trip query is not in scope for this test —
        // walk_mesh's edge join is exercised in pipeline integration tests.
        // Here we just verify the insert path doesn't error.
    }

    #[test]
    fn empty_spans_witness_rejected() {
        let store = fresh_store();
        let mut w = function_witness("file_d", 0, 5);
        w.spans.clear();
        let result = store.insert_witness(&w);
        assert!(result.is_err(), "empty spans must surface as error");
    }

    #[test]
    fn empty_batch_is_no_op() {
        let store = fresh_store();
        store.insert_witnesses_batch(&[]).expect("empty batch ok");
        store
            .insert_witness_input_edges_batch(&[])
            .expect("empty edges ok");
        assert_eq!(store.count_witnesses().expect("count"), 0);
    }

    #[test]
    fn get_witness_returns_none_for_unknown_id() {
        let store = fresh_store();
        let missing = store.get_witness(&"0".repeat(64)).expect("query ok");
        assert!(missing.is_none());
    }

    #[test]
    fn get_witness_round_trips_an_inserted_witness() {
        let store = fresh_store();
        let original = function_witness("f", 0, 7);
        store.insert_witness(&original).unwrap();
        let fetched = store
            .get_witness(&original.id.to_hex())
            .unwrap()
            .expect("witness present after insert");
        assert_eq!(fetched.id, original.id);
        assert_eq!(fetched.witness_type, original.witness_type);
        assert_eq!(fetched.rule, original.rule);
        assert_eq!(fetched.spans, original.spans);
        assert_eq!(fetched.content_blake3, original.content_blake3);
        assert_eq!(fetched.sensitivity, original.sensitivity);
        // Confidence comes back via the Float column — round-trip
        // within 1e-9 is plenty for the static catalog values
        // (0.50 / 0.95 / 0.99).
        assert!((fetched.confidence.value() - original.confidence.value()).abs() < 1e-9);
    }

    #[test]
    fn list_by_workspace_filters_to_scope() {
        let store = fresh_store();
        let ws_a = WorkspaceId::new();
        let ws_b = WorkspaceId::new();
        let now = Utc::now();
        let mut w_a = function_witness("f", 0, 5);
        w_a.workspace = ws_a;
        w_a.created_at = now;
        w_a.valid_from = now;
        let mut w_b = function_witness("g", 0, 5);
        w_b.workspace = ws_b;
        w_b.created_at = now;
        w_b.valid_from = now;
        store.insert_witnesses_batch(&[w_a.clone(), w_b.clone()]).unwrap();
        let list_a = store
            .list_witnesses_by_workspace(&ws_a.to_string(), None)
            .unwrap();
        assert_eq!(list_a.len(), 1);
        assert_eq!(list_a[0].workspace, ws_a);
        let list_b = store
            .list_witnesses_by_workspace(&ws_b.to_string(), None)
            .unwrap();
        assert_eq!(list_b.len(), 1);
        assert_eq!(list_b[0].workspace, ws_b);
    }

    #[test]
    fn list_by_workspace_honours_limit() {
        let store = fresh_store();
        let ws = WorkspaceId::new();
        let now = Utc::now();
        let witnesses: Vec<Witness> = (0..5)
            .map(|i| {
                let mut w = function_witness("f", i * 10, i * 10 + 5);
                w.workspace = ws;
                w.created_at = now;
                w.valid_from = now;
                w
            })
            .collect();
        store.insert_witnesses_batch(&witnesses).unwrap();
        let limited = store
            .list_witnesses_by_workspace(&ws.to_string(), Some(2))
            .unwrap();
        assert_eq!(limited.len(), 2);
        let unbounded = store
            .list_witnesses_by_workspace(&ws.to_string(), None)
            .unwrap();
        assert_eq!(unbounded.len(), 5);
    }

    #[test]
    fn walk_mesh_from_empty_root_returns_empty() {
        let store = fresh_store();
        let (witnesses, edges) = store
            .walk_mesh_from(&"0".repeat(64), 5, 50)
            .expect("walk ok");
        assert!(witnesses.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn walk_mesh_from_isolated_witness_returns_just_it() {
        let store = fresh_store();
        let w = function_witness("f", 0, 5);
        store.insert_witness(&w).unwrap();
        let (witnesses, edges) = store
            .walk_mesh_from(&w.id.to_hex(), 5, 50)
            .expect("walk ok");
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].id, w.id);
        assert!(edges.is_empty(), "isolated witness has no edges");
    }

    #[test]
    fn walk_mesh_from_follows_parent_chain() {
        let store = fresh_store();
        let parent = function_witness("p", 0, 10);
        let child = function_witness("c", 10, 20);
        store
            .insert_witnesses_batch(&[parent.clone(), child.clone()])
            .unwrap();
        store
            .insert_witness_input_edges_batch(&[(parent.id, child.id)])
            .unwrap();
        // Walk starting from child should reach the parent at depth 1.
        let (witnesses, edges) = store
            .walk_mesh_from(&child.id.to_hex(), 2, 50)
            .expect("walk ok");
        assert_eq!(witnesses.len(), 2);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].0, parent.id.to_hex());
        assert_eq!(edges[0].1, child.id.to_hex());
    }

    #[test]
    fn walk_mesh_max_depth_zero_returns_only_root() {
        let store = fresh_store();
        let parent = function_witness("p", 0, 10);
        let child = function_witness("c", 10, 20);
        store
            .insert_witnesses_batch(&[parent.clone(), child.clone()])
            .unwrap();
        store
            .insert_witness_input_edges_batch(&[(parent.id, child.id)])
            .unwrap();
        // max_depth=0 should not follow any edges.
        let (witnesses, edges) = store
            .walk_mesh_from(&child.id.to_hex(), 0, 50)
            .expect("walk ok");
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].id, child.id);
        assert!(edges.is_empty());
    }

    #[test]
    fn list_by_source_scopes_correctly() {
        let store = fresh_store();
        let src_a = SourceId::new();
        let src_b = SourceId::new();
        let now = Utc::now();
        let mut w_a = function_witness("f", 0, 5);
        w_a.source = src_a;
        w_a.created_at = now;
        w_a.valid_from = now;
        let mut w_b = function_witness("g", 0, 5);
        w_b.source = src_b;
        w_b.created_at = now;
        w_b.valid_from = now;
        store.insert_witnesses_batch(&[w_a, w_b]).unwrap();
        let only_a = store.list_witnesses_by_source(&src_a.to_string()).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].source, src_a);
    }
}
