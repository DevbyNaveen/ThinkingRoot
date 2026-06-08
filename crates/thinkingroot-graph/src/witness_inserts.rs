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

/// A citation's resolved byte anchor — the witness span(s) a claim/witness
/// id derives from, joined to its source URI. Returned by
/// [`GraphStore::get_witnesses_for_claim`] so the citation gate can hand the
/// UI a byte-precise, source-anchored pointer (`file` + `[start,end)` +
/// `content_blake3` for tamper-evident verification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCitationSpan {
    pub source_id: String,
    pub source_uri: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
    pub symbol: Option<String>,
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

    /// Batch-insert typed edges into the witness graph (SOTA Lever 2).
    ///
    /// Each entry is `(from, to, edge_type, evidence_witness_id, confidence)`.
    /// `edge_type` must be one of the four canonical strings — the
    /// graph layer rejects unknown variants at write time so a downstream
    /// query can never observe a typed-edge whose `edge_type` isn't in
    /// the alphabet (`Supersedes`/`Contradicts`/`Related`/`TemporalNext`).
    ///
    /// `evidence_witness_id` carries the Witness whose extraction emitted
    /// this edge. Pass `""` only for hand-asserted edges (agent write
    /// path); mechanical-extractor edges always have an evidence witness.
    /// `confidence` clamps to `[0.0, 1.0]`.
    ///
    /// Returns the number of edges actually inserted (after edge-type
    /// validation). Caller can compare against `edges.len()` to detect
    /// silent drops; we never silently swallow malformed edges.
    pub fn insert_witness_typed_edges_batch(
        &self,
        edges: &[(WitnessId, WitnessId, &str, Option<WitnessId>, f32)],
    ) -> Result<usize> {
        const VALID_TYPES: &[&str] =
            &["Supersedes", "Contradicts", "Related", "TemporalNext"];

        let valid: Vec<&(WitnessId, WitnessId, &str, Option<WitnessId>, f32)> = edges
            .iter()
            .filter(|(_, _, t, _, _)| VALID_TYPES.contains(t))
            .collect();

        if valid.len() != edges.len() {
            // Honest report — rather than dropping silently, log the
            // count + first sample so the caller can fix their typing.
            let sample: Vec<&str> = edges
                .iter()
                .filter(|(_, _, t, _, _)| !VALID_TYPES.contains(t))
                .take(3)
                .map(|(_, _, t, _, _)| *t)
                .collect();
            tracing::warn!(
                target: "witness_typed_edges",
                dropped = edges.len() - valid.len(),
                sample = ?sample,
                "dropped typed edges with unknown edge_type — must be one of: \
                 Supersedes/Contradicts/Related/TemporalNext"
            );
        }

        let now_secs = chrono::Utc::now().timestamp() as f64;

        for chunk in valid.chunks(CHUNK) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|(from, to, edge_type, evidence, conf)| {
                    DataValue::List(vec![
                        s(from.to_hex()),
                        s(to.to_hex()),
                        s(edge_type.to_string()),
                        s(evidence
                            .as_ref()
                            .map(|w| w.to_hex())
                            .unwrap_or_default()),
                        f(conf.clamp(0.0, 1.0) as f64),
                        f(now_secs),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            let script = "
                ?[from_witness_id, to_witness_id, edge_type, evidence_witness_id, confidence, created_at] <- $rows
                :put witness_typed_edges {
                    from_witness_id, to_witness_id, edge_type =>
                    evidence_witness_id, confidence, created_at
                }
            ";
            self.query(script, params).map_err(|e| {
                Error::GraphStorage(format!("insert_witness_typed_edges_batch: {e}"))
            })?;
        }
        Ok(valid.len())
    }

    /// Insert a single typed edge — convenience wrapper around the batch
    /// helper for tests and one-off agent contribute paths. The batch
    /// path is the right one for compile-time edge emission.
    pub fn insert_witness_typed_edge(
        &self,
        from: &WitnessId,
        to: &WitnessId,
        edge_type: &str,
        evidence: Option<&WitnessId>,
        confidence: f32,
    ) -> Result<bool> {
        let edges = vec![(
            from.clone(),
            to.clone(),
            edge_type,
            evidence.cloned(),
            confidence,
        )];
        let inserted = self.insert_witness_typed_edges_batch(&edges)?;
        Ok(inserted == 1)
    }

    /// Count typed-edge rows of a given type. `None` counts all edges.
    pub fn count_witness_typed_edges(&self, edge_type: Option<&str>) -> Result<u64> {
        let result = match edge_type {
            None => self
                .query_read("?[count(from_witness_id)] := *witness_typed_edges{from_witness_id}")
                .map_err(|e| Error::GraphStorage(format!("count_witness_typed_edges: {e}")))?,
            Some(t) => {
                let mut params = BTreeMap::new();
                params.insert("kind".into(), s(t.to_string()));
                self.query(
                    "?[count(from_witness_id)] := *witness_typed_edges{from_witness_id, edge_type}, edge_type = $kind",
                    params,
                )
                .map_err(|e| Error::GraphStorage(format!("count_witness_typed_edges: {e}")))?
            }
        };
        if let Some(row) = result.rows.first() {
            if let Some(DataValue::Num(Num::Int(n))) = row.first() {
                return Ok((*n).max(0) as u64);
            }
        }
        Ok(0)
    }

    /// List the Witnesses that supersede `witness_id` (the `from` of an
    /// edge typed `Supersedes` pointing to `witness_id`). Walks one hop;
    /// callers wanting the full chain compose with `walk_typed_edges`.
    pub fn list_witness_supersedes(&self, witness_id: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("wid".into(), s(witness_id.to_string()));
        let result = self
            .query(
                "?[from_id] := *witness_typed_edges{from_witness_id: from_id, to_witness_id, edge_type}, \
                 to_witness_id = $wid, edge_type = 'Supersedes'",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("list_witness_supersedes: {e}")))?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| row.first().and_then(|dv| match dv {
                DataValue::Str(s) => Some(s.to_string()),
                _ => None,
            }))
            .collect())
    }

    /// List the Witnesses contradicting `witness_id` (edges typed
    /// `Contradicts` in either direction — contradiction is symmetric
    /// for retrieval purposes but stored as a directed edge for honest
    /// audit of "who flagged the contradiction first").
    pub fn list_witness_contradictions(&self, witness_id: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("wid".into(), s(witness_id.to_string()));
        let result = self
            .query(
                "?[other_id] := *witness_typed_edges{from_witness_id, to_witness_id, edge_type}, \
                 edge_type = 'Contradicts', \
                 ((from_witness_id = $wid, other_id = to_witness_id) or \
                  (to_witness_id = $wid, other_id = from_witness_id))",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("list_witness_contradictions: {e}")))?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| row.first().and_then(|dv| match dv {
                DataValue::Str(s) => Some(s.to_string()),
                _ => None,
            }))
            .collect())
    }

    /// Walk typed edges from `start_id`, returning every reachable
    /// Witness id. Uses Cozo's recursive Datalog with a cycle guard
    /// (`mid != $start`) matching the AEP `Q_SUPERSESSION_CHAIN` pattern
    /// (`aep_queries.rs:115`). `max_hops` is reserved for a future
    /// bounded-depth primitive once Cozo exposes one; for now Cozo's
    /// fixed-point evaluator terminates on the cycle guard alone.
    ///
    /// `edge_types` filters to specific edge kinds; empty slice = all.
    pub fn walk_witness_typed_edges(
        &self,
        start_id: &str,
        edge_types: &[&str],
        max_hops: usize,
    ) -> Result<Vec<String>> {
        if max_hops == 0 {
            return Ok(Vec::new());
        }
        let _ = max_hops.min(8); // pinned for forward-compat with a depth primitive

        let mut params = BTreeMap::new();
        params.insert("start".into(), s(start_id.to_string()));

        // Build the optional `edge_type` filter inline. Cozo's Datalog
        // rejects an `IN` expression with an empty list, so we emit the
        // entire predicate clause only when we have at least one type.
        let type_clause = if edge_types.is_empty() {
            String::new()
        } else {
            let quoted: Vec<String> = edge_types
                .iter()
                .map(|t| format!("'{}'", t.replace('\'', "")))
                .collect();
            format!(", et in [{}]", quoted.join(", "))
        };

        // Recursive walk. Two rule heads with the same name = disjunction.
        // Pattern mirrors `Q_SUPERSESSION_CHAIN` in aep_queries.rs:115.
        // `mid != $start` is the cycle guard for finite termination.
        // The final `?[id]` filter excludes the start node — graph
        // walkers conventionally return the *reachable* set, not the
        // input. A cycle test confirms `$start` doesn't sneak back in
        // via a→b→…→$start paths.
        let script = format!(
            r#"
            reachable[id] := *witness_typed_edges{{from_witness_id: from_id, to_witness_id: id, edge_type: et}},
                             from_id = $start{type_clause}
            reachable[id] := reachable[mid], mid != $start,
                             *witness_typed_edges{{from_witness_id: mid, to_witness_id: id, edge_type: et}}{type_clause}
            ?[id] := reachable[id], id != $start
            "#
        );

        let result = self
            .query(&script, params)
            .map_err(|e| Error::GraphStorage(format!("walk_witness_typed_edges: {e}")))?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| row.first().and_then(|dv| match dv {
                DataValue::Str(s) => Some(s.to_string()),
                _ => None,
            }))
            .collect())
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

    /// Resolve a "claim id" (which IS a witness id in the Witness-Mesh
    /// substrate) to its byte-anchored source span(s), joined to the
    /// source URI. Used by the citation gate to turn a verified
    /// `[claim:<id>]` marker into a byte-precise, source-anchored
    /// pointer the UI can highlight + verify against `content_blake3`.
    ///
    /// Returns an empty Vec when the id is unknown — honesty rule:
    /// absence is an empty list, never a fabricated span.
    pub fn get_witnesses_for_claim(&self, claim_id: &str) -> Result<Vec<ResolvedCitationSpan>> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), s(claim_id.to_string()));
        // Bind the witness id to a capture var (`wid`) then constrain it,
        // matching `get_witness`'s `$param`-on-column avoidance. The
        // `source_id` variable is shared across the `witnesses` and
        // `sources` atoms, so Datalog unifies them — a natural join that
        // yields the source URI alongside the denormalised byte anchor.
        let result = self
            .query(
                "?[source_id, uri, byte_start, byte_end, content_blake3, symbol] := \
                  *witnesses{id: wid, source_id, byte_start, byte_end, content_blake3, symbol}, \
                  wid = $cid, \
                  *sources{id: source_id, uri}",
                params,
            )
            .map_err(|e| {
                Error::GraphStorage(format!("get_witnesses_for_claim({claim_id}): {e}"))
            })?;

        fn dv_str(v: &DataValue) -> String {
            match v {
                DataValue::Str(s) => s.to_string(),
                _ => String::new(),
            }
        }
        fn dv_u64(v: &DataValue) -> u64 {
            match v {
                DataValue::Num(Num::Int(i)) => (*i).max(0) as u64,
                DataValue::Num(Num::Float(f)) => f.max(0.0) as u64,
                _ => 0,
            }
        }

        let mut out = Vec::with_capacity(result.rows.len());
        for row in &result.rows {
            if row.len() < 6 {
                continue;
            }
            let symbol = match &row[5] {
                DataValue::Str(s) if !s.is_empty() => Some(s.to_string()),
                _ => None,
            };
            out.push(ResolvedCitationSpan {
                source_id: dv_str(&row[0]),
                source_uri: dv_str(&row[1]),
                byte_start: dv_u64(&row[2]),
                byte_end: dv_u64(&row[3]),
                content_blake3: dv_str(&row[4]),
                symbol,
            });
        }
        Ok(out)
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
    // spans[0]. Assert agreement before returning so a torn write or
    // a migration bug surfaces as a typed error rather than silently
    // producing a row whose in-memory shape disagrees with the
    // indexed columns that Datalog queries join on. Empty `spans` is
    // already rejected by `WitnessMesh::assemble` upstream; this is
    // defence-in-depth for the persistence boundary.
    let denorm_start = dv_u64(&row[11]);
    let denorm_end = dv_u64(&row[12]);
    if let Some(primary) = spans.first() {
        if primary.start != denorm_start || primary.end != denorm_end {
            return Err(Error::GraphStorage(format!(
                "witness {id_hex}: denormalised byte range \
                 ({denorm_start}..{denorm_end}) disagrees with spans[0] \
                 ({}..{}); row corrupt or stale",
                primary.start, primary.end
            )));
        }
    } else {
        return Err(Error::GraphStorage(format!(
            "witness {id_hex}: spans_json decoded to empty array; \
             every Witness must carry at least one span"
        )));
    }

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

    // ─── SOTA Lever 2: witness_typed_edges substrate tests ─────────────

    #[test]
    fn typed_edge_round_trips_through_table() {
        let store = fresh_store();
        let a = function_witness("file_z", 0, 5);
        let b = function_witness("file_z", 5, 10);
        store
            .insert_witnesses_batch(&[a.clone(), b.clone()])
            .unwrap();
        let ok = store
            .insert_witness_typed_edge(&a.id, &b.id, "Supersedes", None, 0.95)
            .expect("insert edge");
        assert!(ok);
        assert_eq!(store.count_witness_typed_edges(None).unwrap(), 1);
        assert_eq!(
            store
                .count_witness_typed_edges(Some("Supersedes"))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .count_witness_typed_edges(Some("Contradicts"))
                .unwrap(),
            0
        );
    }

    #[test]
    fn unknown_edge_type_is_dropped_loudly_not_silently() {
        let store = fresh_store();
        let a = function_witness("file_y", 0, 5);
        let b = function_witness("file_y", 5, 10);
        store
            .insert_witnesses_batch(&[a.clone(), b.clone()])
            .unwrap();
        let inserted = store
            .insert_witness_typed_edges_batch(&[(
                a.id.clone(),
                b.id.clone(),
                "Bogus",
                None,
                0.5,
            )])
            .expect("call ok");
        assert_eq!(inserted, 0, "unknown edge_type must drop");
        assert_eq!(store.count_witness_typed_edges(None).unwrap(), 0);
    }

    #[test]
    fn confidence_clamps_to_unit_interval() {
        let store = fresh_store();
        let a = function_witness("file_x", 0, 5);
        let b = function_witness("file_x", 5, 10);
        let c = function_witness("file_x", 10, 15);
        store
            .insert_witnesses_batch(&[a.clone(), b.clone(), c.clone()])
            .unwrap();
        // Over-1.0 and below-0.0 both get clamped at write time.
        store
            .insert_witness_typed_edge(&a.id, &b.id, "Related", None, 5.0)
            .unwrap();
        store
            .insert_witness_typed_edge(&a.id, &c.id, "Related", None, -2.0)
            .unwrap();
        assert_eq!(store.count_witness_typed_edges(None).unwrap(), 2);
    }

    #[test]
    fn list_supersedes_returns_predecessors() {
        let store = fresh_store();
        // Three versions of the same logical claim; v1 ← v2 ← v3.
        let v1 = function_witness("paper.md", 0, 10);
        let v2 = function_witness("paper.md", 10, 20);
        let v3 = function_witness("paper.md", 20, 30);
        store
            .insert_witnesses_batch(&[v1.clone(), v2.clone(), v3.clone()])
            .unwrap();
        store
            .insert_witness_typed_edge(&v2.id, &v1.id, "Supersedes", None, 0.99)
            .unwrap();
        store
            .insert_witness_typed_edge(&v3.id, &v2.id, "Supersedes", None, 0.99)
            .unwrap();
        let supers_of_v1 = store.list_witness_supersedes(&v1.id.to_hex()).unwrap();
        assert_eq!(supers_of_v1.len(), 1);
        assert_eq!(supers_of_v1[0], v2.id.to_hex());
    }

    #[test]
    fn contradictions_are_symmetric_for_retrieval() {
        let store = fresh_store();
        let a = function_witness("a.md", 0, 5);
        let b = function_witness("a.md", 5, 10);
        store
            .insert_witnesses_batch(&[a.clone(), b.clone()])
            .unwrap();
        // Insert directed (a -> b); both endpoints should surface as
        // contradiction partners.
        store
            .insert_witness_typed_edge(&a.id, &b.id, "Contradicts", None, 0.9)
            .unwrap();
        let from_a = store.list_witness_contradictions(&a.id.to_hex()).unwrap();
        let from_b = store.list_witness_contradictions(&b.id.to_hex()).unwrap();
        assert_eq!(from_a, vec![b.id.to_hex()]);
        assert_eq!(from_b, vec![a.id.to_hex()]);
    }

    #[test]
    fn walk_typed_edges_traverses_chain_and_terminates_on_cycle() {
        let store = fresh_store();
        // a -> b -> c -> a (cycle); walk from a should NOT loop forever.
        let a = function_witness("c.md", 0, 5);
        let b = function_witness("c.md", 5, 10);
        let c = function_witness("c.md", 10, 15);
        store
            .insert_witnesses_batch(&[a.clone(), b.clone(), c.clone()])
            .unwrap();
        store
            .insert_witness_typed_edge(&a.id, &b.id, "Related", None, 0.8)
            .unwrap();
        store
            .insert_witness_typed_edge(&b.id, &c.id, "Related", None, 0.8)
            .unwrap();
        store
            .insert_witness_typed_edge(&c.id, &a.id, "Related", None, 0.8)
            .unwrap();
        // Walk; the recursive rule's `mid != $start` guard keeps the
        // fixed-point finite. Expected reachable set: {b, c}.
        let reachable = store
            .walk_witness_typed_edges(&a.id.to_hex(), &["Related"], 5)
            .unwrap();
        assert!(reachable.contains(&b.id.to_hex()));
        assert!(reachable.contains(&c.id.to_hex()));
        assert!(
            !reachable.contains(&a.id.to_hex()),
            "cycle guard must keep $start out of result"
        );
    }

    #[test]
    fn walk_typed_edges_respects_edge_type_filter() {
        let store = fresh_store();
        let a = function_witness("d.md", 0, 5);
        let b = function_witness("d.md", 5, 10);
        let c = function_witness("d.md", 10, 15);
        store
            .insert_witnesses_batch(&[a.clone(), b.clone(), c.clone()])
            .unwrap();
        store
            .insert_witness_typed_edge(&a.id, &b.id, "Related", None, 0.8)
            .unwrap();
        store
            .insert_witness_typed_edge(&a.id, &c.id, "TemporalNext", None, 0.8)
            .unwrap();
        // Filter to Related only: must not return c.
        let related_only = store
            .walk_witness_typed_edges(&a.id.to_hex(), &["Related"], 3)
            .unwrap();
        assert_eq!(related_only.len(), 1);
        assert_eq!(related_only[0], b.id.to_hex());
        // Filter to TemporalNext only: must not return b.
        let temporal_only = store
            .walk_witness_typed_edges(&a.id.to_hex(), &["TemporalNext"], 3)
            .unwrap();
        assert_eq!(temporal_only.len(), 1);
        assert_eq!(temporal_only[0], c.id.to_hex());
    }

    #[test]
    fn walk_typed_edges_empty_edge_types_walks_all_kinds() {
        let store = fresh_store();
        let a = function_witness("e.md", 0, 5);
        let b = function_witness("e.md", 5, 10);
        let c = function_witness("e.md", 10, 15);
        store
            .insert_witnesses_batch(&[a.clone(), b.clone(), c.clone()])
            .unwrap();
        store
            .insert_witness_typed_edge(&a.id, &b.id, "Related", None, 0.8)
            .unwrap();
        store
            .insert_witness_typed_edge(&a.id, &c.id, "Contradicts", None, 0.8)
            .unwrap();
        let all = store
            .walk_witness_typed_edges(&a.id.to_hex(), &[], 3)
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn empty_typed_edge_batch_is_no_op() {
        let store = fresh_store();
        let inserted = store.insert_witness_typed_edges_batch(&[]).unwrap();
        assert_eq!(inserted, 0);
        assert_eq!(store.count_witness_typed_edges(None).unwrap(), 0);
    }
}
