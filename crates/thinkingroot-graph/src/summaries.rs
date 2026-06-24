//! E4 — hierarchical summary nodes (altitude: function → file → repo).
//!
//! A bottom-up pass that distills the compiled code graph into a small ladder
//! of summary nodes, so an agent can read the repo at altitude (the one-line
//! repo summary, then drill into a file, then a symbol) instead of paying for
//! every claim. Stored in the derived `summary_nodes` relation (NOT a
//! byte-coverage structural table), rebuilt wholesale per compile.
//!
//! This module builds **deterministic template** summaries with no model in
//! the loop — fully reproducible and the honest fallback when no LLM is
//! configured. LLM-authored 1-sentence summaries (and embedding them for
//! altitude-then-drill retrieval) are a serve-layer enrichment that overrides
//! `summary` text on top of this substrate; until then the deterministic text
//! is what ships (never a fabricated summary).
//!
//! Empty graph → zero summary nodes (honesty rule).

use std::collections::BTreeMap;

use cozo::{DataValue, Num};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// Altitude bands. Higher altitude = coarser.
pub const ALTITUDE_FUNCTION: &str = "function";
pub const ALTITUDE_FILE: &str = "file";
pub const ALTITUDE_REPO: &str = "repo";
/// North-star: one summary node per imported document (`target_id =
/// source_id`). Powers the Sources page "Summary" tab. Distinct from the
/// code-oriented function/file/repo ladder above.
pub const ALTITUDE_DOCUMENT: &str = "document";

/// One hierarchical summary node.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SummaryNode {
    pub id: String,
    pub altitude: String,
    /// What this node summarizes: a claim id (function), a file path (file),
    /// or `"repo"` (repo root).
    pub target_id: String,
    pub summary: String,
    /// Child summary-node ids (JSON array string).
    pub child_ids_json: String,
    pub source_uri: String,
    pub created_at: f64,
}

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

impl GraphStore {
    /// Build the deterministic summary ladder for the workspace, replacing any
    /// prior summary nodes. Returns the number of summary nodes written.
    /// `now` is the timestamp stamped on every node (caller supplies it so the
    /// build stays pure/replayable). Empty graph → 0 nodes.
    pub fn build_summaries(&self, now: f64) -> Result<usize> {
        self.clear_summary_nodes()?;

        let entities = self.list_code_entities()?;
        if entities.is_empty() {
            return Ok(0);
        }

        let mut nodes: Vec<SummaryNode> = Vec::new();

        // ── function altitude: one node per symbol-bearing claim ──
        // Group function node ids by file as we go (for the file altitude).
        let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut file_symbols: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for e in &entities {
            let id = format!("sum:function:{}", e.claim_id);
            let file = if e.source_path.is_empty() {
                "<unknown>".to_string()
            } else {
                e.source_path.clone()
            };
            nodes.push(SummaryNode {
                id: id.clone(),
                altitude: ALTITUDE_FUNCTION.to_string(),
                target_id: e.claim_id.clone(),
                summary: format!("`{}` — defined in {}", e.symbol, file),
                child_ids_json: "[]".to_string(),
                source_uri: file.clone(),
                created_at: now,
            });
            by_file.entry(file.clone()).or_default().push(id);
            file_symbols.entry(file).or_default().push(e.symbol.clone());
        }

        // ── file altitude: one node per file, children = its functions ──
        let mut file_node_ids: Vec<String> = Vec::new();
        for (file, child_ids) in &by_file {
            let id = format!("sum:file:{file}");
            let syms = file_symbols.get(file).cloned().unwrap_or_default();
            let preview: Vec<String> = syms.iter().take(5).cloned().collect();
            let more = if syms.len() > preview.len() {
                format!(", +{} more", syms.len() - preview.len())
            } else {
                String::new()
            };
            nodes.push(SummaryNode {
                id: id.clone(),
                altitude: ALTITUDE_FILE.to_string(),
                target_id: file.clone(),
                summary: format!(
                    "{file}: {} symbol(s) — {}{}",
                    syms.len(),
                    preview.join(", "),
                    more
                ),
                child_ids_json: serde_json::to_string(child_ids).unwrap_or_else(|_| "[]".into()),
                source_uri: file.clone(),
                created_at: now,
            });
            file_node_ids.push(id);
        }

        // ── repo altitude: single root, children = files ──
        nodes.push(SummaryNode {
            id: "sum:repo:root".to_string(),
            altitude: ALTITUDE_REPO.to_string(),
            target_id: "repo".to_string(),
            summary: format!(
                "Repository: {} file(s), {} symbol(s)",
                by_file.len(),
                entities.len()
            ),
            child_ids_json: serde_json::to_string(&file_node_ids).unwrap_or_else(|_| "[]".into()),
            source_uri: String::new(),
            created_at: now,
        });

        let count = nodes.len();
        self.insert_summary_nodes(&nodes)?;
        Ok(count)
    }

    /// Remove all summary nodes (wholesale rebuild precedes every build).
    pub fn clear_summary_nodes(&self) -> Result<()> {
        self.query(
            "?[id] := *summary_nodes{id} :rm summary_nodes {id}",
            Default::default(),
        )
        .map_err(|e| Error::GraphStorage(format!("clear_summary_nodes: {e}")))?;
        Ok(())
    }

    /// Batch-insert summary nodes.
    fn insert_summary_nodes(&self, nodes: &[SummaryNode]) -> Result<()> {
        for chunk in nodes.chunks(500) {
            let payload: Vec<DataValue> = chunk
                .iter()
                .map(|n| {
                    DataValue::List(vec![
                        DataValue::Str(n.id.clone().into()),
                        DataValue::Str(n.altitude.clone().into()),
                        DataValue::Str(n.target_id.clone().into()),
                        DataValue::Str(n.summary.clone().into()),
                        DataValue::Str(n.child_ids_json.clone().into()),
                        DataValue::Str(n.source_uri.clone().into()),
                        DataValue::Num(Num::Float(n.created_at)),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".to_string(), DataValue::List(payload));
            self.query(
                "?[id, altitude, target_id, summary, child_ids_json, source_uri, created_at] <- $rows \
                 :put summary_nodes {id => altitude, target_id, summary, child_ids_json, source_uri, created_at}",
                params,
            )
            .map_err(|e| Error::GraphStorage(format!("insert_summary_nodes: {e}")))?;
        }
        Ok(())
    }

    /// Upsert the per-document summary node for one source (wholesale-replace
    /// the prior `document`-altitude node for that `source_id`). `child_ids` are
    /// the fact/chunk ids the summary covers.
    pub fn upsert_document_summary(
        &self,
        source_id: &str,
        source_uri: &str,
        summary: &str,
        child_ids: &[String],
        now: f64,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("sid".to_string(), DataValue::Str(source_id.into()));
        self.query(
            "?[id] := *summary_nodes{id, altitude, target_id}, altitude == 'document', \
             target_id == $sid\n:rm summary_nodes {id}",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("upsert_document_summary :rm: {e}")))?;

        let node = SummaryNode {
            id: format!("doc-summary:{source_id}"),
            altitude: ALTITUDE_DOCUMENT.to_string(),
            target_id: source_id.to_string(),
            summary: summary.to_string(),
            child_ids_json: serde_json::to_string(child_ids).unwrap_or_else(|_| "[]".into()),
            source_uri: source_uri.to_string(),
            created_at: now,
        };
        self.insert_summary_nodes(&[node])
    }

    /// Build a DETERMINISTIC per-document summary from the source's live atomic
    /// facts (the always-available fallback; an LLM-authored summary can be
    /// supplied via [`Self::upsert_document_summary`] instead). Returns `false`
    /// when the source has no facts yet (nothing to summarize). `now` keeps the
    /// build pure/replayable.
    pub fn build_document_summary(
        &self,
        source_id: &str,
        source_uri: &str,
        now: f64,
    ) -> Result<bool> {
        let facts: Vec<_> = self
            .get_atomic_facts_for_source(source_id)?
            .into_iter()
            .filter(|f| f.is_live())
            .collect();
        if facts.is_empty() {
            return Ok(false);
        }
        let chunk_count = self.get_raw_chunks_for_source(source_id)?.len();
        let preview: Vec<&str> = facts.iter().take(5).map(|f| f.statement.as_str()).collect();
        let summary = format!(
            "{} fact{} across {} section{}. {}",
            facts.len(),
            if facts.len() == 1 { "" } else { "s" },
            chunk_count,
            if chunk_count == 1 { "" } else { "s" },
            preview.join(" · ")
        );
        let child_ids: Vec<String> = facts.iter().map(|f| f.id.clone()).collect();
        self.upsert_document_summary(source_id, source_uri, &summary, &child_ids, now)?;
        Ok(true)
    }

    /// Read summary nodes, optionally filtered to one altitude. Ordered by
    /// (altitude, id) for determinism.
    pub fn get_summary_nodes(&self, altitude: Option<&str>) -> Result<Vec<SummaryNode>> {
        let rows = self
            .query(
                "?[id, altitude, target_id, summary, child_ids_json, source_uri, created_at] := \
                 *summary_nodes{id, altitude, target_id, summary, child_ids_json, source_uri, created_at}",
                Default::default(),
            )
            .map_err(|e| Error::GraphStorage(format!("get_summary_nodes: {e}")))?;
        let mut out: Vec<SummaryNode> = Vec::new();
        for row in &rows.rows {
            if row.len() < 7 {
                continue;
            }
            let alt = dv_str(&row[1]);
            if let Some(want) = altitude {
                if alt != want {
                    continue;
                }
            }
            out.push(SummaryNode {
                id: dv_str(&row[0]),
                altitude: alt,
                target_id: dv_str(&row[2]),
                summary: dv_str(&row[3]),
                child_ids_json: dv_str(&row[4]),
                source_uri: dv_str(&row[5]),
                created_at: dv_f64(&row[6]),
            });
        }
        out.sort_by(|a, b| a.altitude.cmp(&b.altitude).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> GraphStore {
        GraphStore::init(&tempdir().unwrap().into_path()).unwrap()
    }

    fn put_claim(s: &GraphStore, id: &str, symbol: &str, source_path: &str) {
        let row = DataValue::List(vec![
            DataValue::Str(id.into()),
            DataValue::Str(format!("def {symbol}").into()),
            DataValue::Str("function_def".into()),
            DataValue::Str("src1".into()),
            DataValue::Str(symbol.into()),
            DataValue::Str(source_path.into()),
        ]);
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(vec![row]));
        s.query(
            "?[id, statement, claim_type, source_id, symbol, source_path] <- $rows \
             :put claims {id => statement, claim_type, source_id, symbol, source_path}",
            params,
        )
        .unwrap();
    }

    #[test]
    fn document_summary_built_from_facts_and_upserts() {
        use thinkingroot_core::types::AtomicFact;
        let s = store();
        // No facts yet → nothing to summarize.
        assert!(!s.build_document_summary("srcD", "doc.md", 1.0).unwrap());

        let f = |pred: &str, start: u64, stmt: &str| AtomicFact {
            id: AtomicFact::derive_id("srcD", start, start + 5, pred),
            source_id: "srcD".into(),
            chunk_id: "c".into(),
            subject: "S".into(),
            predicate: pred.into(),
            object: "O".into(),
            statement: stmt.into(),
            confidence: 0.9,
            extraction_model: "m".into(),
            workspace_id: "ws".into(),
            sensitivity: "Public".into(),
            byte_start: start,
            byte_end: start + 5,
            content_blake3: "h".into(),
            valid_from: 1.0,
            valid_until: -1.0,
            created_at: 1.0,
        };
        s.insert_atomic_facts_batch(&[f("a", 0, "Alpha fact"), f("b", 10, "Beta fact")])
            .unwrap();

        assert!(s.build_document_summary("srcD", "doc.md", 1.0).unwrap());
        let docs = s.get_summary_nodes(Some(ALTITUDE_DOCUMENT)).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].target_id, "srcD");
        assert!(docs[0].summary.contains("2 facts"));
        assert!(docs[0].summary.contains("Alpha fact"));
        let children: Vec<String> = serde_json::from_str(&docs[0].child_ids_json).unwrap();
        assert_eq!(children.len(), 2);

        // Idempotent: a second build replaces (not duplicates) the node.
        s.build_document_summary("srcD", "doc.md", 2.0).unwrap();
        assert_eq!(s.get_summary_nodes(Some(ALTITUDE_DOCUMENT)).unwrap().len(), 1);
    }

    #[test]
    fn bottom_up_builds_all_altitudes() {
        let s = store();
        put_claim(&s, "a", "fn_a", "a.rs");
        put_claim(&s, "b", "fn_b", "a.rs");
        put_claim(&s, "c", "fn_c", "b.rs");

        let n = s.build_summaries(0.0).unwrap();
        // 3 function + 2 file + 1 repo = 6.
        assert_eq!(n, 6);
        assert_eq!(s.get_summary_nodes(Some(ALTITUDE_FUNCTION)).unwrap().len(), 3);
        assert_eq!(s.get_summary_nodes(Some(ALTITUDE_FILE)).unwrap().len(), 2);
        let repo = s.get_summary_nodes(Some(ALTITUDE_REPO)).unwrap();
        assert_eq!(repo.len(), 1);
        assert!(repo[0].summary.contains("2 file(s)"));
        assert!(repo[0].summary.contains("3 symbol(s)"));
        // Repo node's children are the two file nodes.
        let children: Vec<String> = serde_json::from_str(&repo[0].child_ids_json).unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn rebuild_replaces_prior_nodes() {
        let s = store();
        put_claim(&s, "a", "fn_a", "a.rs");
        s.build_summaries(0.0).unwrap();
        assert_eq!(s.get_summary_nodes(None).unwrap().len(), 1 + 1 + 1);
        // Rebuild after adding a symbol → no stale duplicates.
        put_claim(&s, "b", "fn_b", "a.rs");
        s.build_summaries(1.0).unwrap();
        assert_eq!(s.get_summary_nodes(Some(ALTITUDE_FUNCTION)).unwrap().len(), 2);
        assert_eq!(s.get_summary_nodes(Some(ALTITUDE_REPO)).unwrap().len(), 1);
    }

    #[test]
    fn empty_graph_yields_no_summaries() {
        let s = store();
        assert_eq!(s.build_summaries(0.0).unwrap(), 0);
        assert!(s.get_summary_nodes(None).unwrap().is_empty());
    }
}
