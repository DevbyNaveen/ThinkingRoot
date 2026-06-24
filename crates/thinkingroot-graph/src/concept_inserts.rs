//! `concept_nodes` insert/read + concept-membership spine edges.
//!
//! Concepts are community summaries the Stitcher grows from
//! `detect_communities()`. They are inserted `quarantined` and promoted to
//! `active` only once a verify pass confirms evidenced co-occurrence
//! (anti-hallucination gate 2). Membership edges (`entity_in_concept`) live in
//! `spine_edges` so retrieval graph-expansion can walk concept ↔ entity.

use std::collections::BTreeMap;

use cozo::{DataValue, Num};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;
use crate::rows::ConceptNode;

fn s(value: impl Into<String>) -> DataValue {
    DataValue::Str(value.into().into())
}
fn f(value: f64) -> DataValue {
    DataValue::Num(Num::Float(value))
}
fn ds(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}
fn df(v: &DataValue) -> f64 {
    match v {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

const CCOLS: &str = "id, label, member_entity_ids_json, origin, status, confidence, \
    evidence_json, provenance, created_at";
const CNONPK: &str = "label, member_entity_ids_json, origin, status, confidence, \
    evidence_json, provenance, created_at";

impl GraphStore {
    /// Stable concept id for a member set (sorted, deduped) — `concept:<blake3>`.
    pub fn concept_id_for(members: &[String]) -> String {
        let mut sorted: Vec<&str> = members.iter().map(|s| s.as_str()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut hasher = blake3::Hasher::new();
        for m in sorted {
            hasher.update(m.as_bytes());
            hasher.update(b"\0");
        }
        format!("concept:{}", hasher.finalize().to_hex())
    }

    /// Upsert a concept node (PK = id, so re-grow is idempotent).
    pub fn upsert_concept_node(&self, node: &ConceptNode) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert(
            "rows".into(),
            DataValue::List(vec![DataValue::List(vec![
                s(&node.id),
                s(&node.label),
                s(&node.member_entity_ids_json),
                s(&node.origin),
                s(&node.status),
                f(node.confidence),
                s(&node.evidence_json),
                s(&node.provenance),
                f(node.created_at),
            ])]),
        );
        self.query(
            &format!("?[{CCOLS}] <- $rows\n:put concept_nodes {{ id => {CNONPK} }}"),
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("upsert_concept_node: {e}")))?;
        Ok(())
    }

    /// Set a concept's status (`quarantined` | `active` | `rejected`).
    pub fn set_concept_status(&self, id: &str, status: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.into()));
        params.insert("st".into(), DataValue::Str(status.into()));
        self.query(
            "?[id, label, member_entity_ids_json, origin, status, confidence, evidence_json, provenance, created_at] := \
             *concept_nodes{id, label, member_entity_ids_json, origin, confidence, evidence_json, provenance, created_at}, \
             id == $id, status = $st\n\
             :put concept_nodes {id => label, member_entity_ids_json, origin, status, confidence, evidence_json, provenance, created_at}",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("set_concept_status: {e}")))?;
        Ok(())
    }

    /// List concept nodes, optionally filtered by status.
    pub fn list_concept_nodes(&self, status: Option<&str>) -> Result<Vec<ConceptNode>> {
        let res = match status {
            Some(st) => {
                let mut params = BTreeMap::new();
                params.insert("st".into(), DataValue::Str(st.into()));
                self.query(
                    &format!("?[{CCOLS}] := *concept_nodes{{{CCOLS}}}, status == $st"),
                    params,
                )?
            }
            None => self.query_read(&format!("?[{CCOLS}] := *concept_nodes{{{CCOLS}}}"))?,
        };
        Ok(res
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 9 {
                    return None;
                }
                Some(ConceptNode {
                    id: ds(&r[0]),
                    label: ds(&r[1]),
                    member_entity_ids_json: ds(&r[2]),
                    origin: ds(&r[3]),
                    status: ds(&r[4]),
                    confidence: df(&r[5]),
                    evidence_json: ds(&r[6]),
                    provenance: ds(&r[7]),
                    created_at: df(&r[8]),
                })
            })
            .collect())
    }

    /// Member entity ids of a concept.
    pub fn get_concept_members(&self, id: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.into()));
        let res = self.query(
            "?[member_entity_ids_json] := *concept_nodes{id, member_entity_ids_json}, id == $id",
            params,
        )?;
        let Some(row) = res.rows.first() else {
            return Ok(Vec::new());
        };
        Ok(serde_json::from_str(&ds(&row[0])).unwrap_or_default())
    }

    /// Remove every concept node (wholesale re-grow each Stitcher pass).
    pub fn clear_concepts(&self) -> Result<()> {
        self.query("?[id] := *concept_nodes{id}\n:rm concept_nodes {id}", BTreeMap::new())
            .map_err(|e| Error::GraphStorage(format!("clear_concepts: {e}")))?;
        // Also clear stale concept-membership spine edges.
        self.query(
            "?[from_id, to_id, edge_kind] := *spine_edges{from_id, to_id, edge_kind}, \
             edge_kind == 'entity_in_concept'\n:rm spine_edges {from_id, to_id, edge_kind}",
            BTreeMap::new(),
        )
        .map_err(|e| Error::GraphStorage(format!("clear concept membership edges: {e}")))?;
        Ok(())
    }

    /// Write `entity_in_concept` membership edges (concept → member entity) for
    /// an ACTIVE concept into `spine_edges`.
    pub fn write_concept_membership(&self, concept_id: &str, member_ids: &[String], now: f64) -> Result<()> {
        if member_ids.is_empty() {
            return Ok(());
        }
        let payload: Vec<DataValue> = member_ids
            .iter()
            .map(|eid| {
                DataValue::List(vec![
                    s(concept_id),
                    s(eid),
                    s("entity_in_concept"),
                    s(""),
                    f(1.0),
                    f(now),
                ])
            })
            .collect();
        let mut params = BTreeMap::new();
        params.insert("rows".into(), DataValue::List(payload));
        self.query(
            "?[from_id, to_id, edge_kind, source_id, confidence, created_at] <- $rows\n\
             :put spine_edges { from_id, to_id, edge_kind => source_id, confidence, created_at }",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("write_concept_membership: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::init(dir.path()).unwrap();
        (dir, store)
    }

    fn concept(members: &[&str], status: &str, evidence: &[&str]) -> ConceptNode {
        let m: Vec<String> = members.iter().map(|s| s.to_string()).collect();
        ConceptNode {
            id: GraphStore::concept_id_for(&m),
            label: members.join(" + "),
            member_entity_ids_json: serde_json::to_string(&m).unwrap(),
            origin: "stitch".into(),
            status: status.into(),
            confidence: 0.7,
            evidence_json: serde_json::to_string(evidence).unwrap(),
            provenance: "stitch://concept/x".into(),
            created_at: 1.0,
        }
    }

    #[test]
    fn concept_upsert_status_members_and_filter() {
        let (_d, store) = store();
        let c = concept(&["e1", "e2", "e3"], "quarantined", &["claim1"]);
        store.upsert_concept_node(&c).unwrap();

        assert_eq!(store.list_concept_nodes(Some("quarantined")).unwrap().len(), 1);
        assert_eq!(store.list_concept_nodes(Some("active")).unwrap().len(), 0);

        store.set_concept_status(&c.id, "active").unwrap();
        assert_eq!(store.list_concept_nodes(Some("active")).unwrap().len(), 1);
        let members = store.get_concept_members(&c.id).unwrap();
        assert_eq!(members.len(), 3);

        // Idempotent id: same members → same id → no duplicate.
        store.upsert_concept_node(&concept(&["e3", "e1", "e2"], "active", &["claim1"])).unwrap();
        assert_eq!(store.list_concept_nodes(None).unwrap().len(), 1);
    }

    #[test]
    fn membership_edges_written_and_cleared() {
        let (_d, store) = store();
        let c = concept(&["e1", "e2"], "active", &["c1"]);
        store.upsert_concept_node(&c).unwrap();
        store
            .write_concept_membership(&c.id, &["e1".into(), "e2".into()], 1.0)
            .unwrap();
        let nbrs = store.spine_neighbors(&c.id, "entity_in_concept").unwrap();
        assert_eq!(nbrs.len(), 2);

        store.clear_concepts().unwrap();
        assert_eq!(store.list_concept_nodes(None).unwrap().len(), 0);
        assert_eq!(store.spine_neighbors(&c.id, "entity_in_concept").unwrap().len(), 0);
    }
}
