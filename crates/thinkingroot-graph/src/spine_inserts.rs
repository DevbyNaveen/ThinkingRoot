//! `spine_edges` construction — the denormalised mother-node hierarchy
//! (`document → chunk → fact → entity`) that retrieval graph-expansion walks.
//!
//! Edges are rebuilt wholesale per-source (the source's facts/chunks are the
//! single source of truth), so a re-extraction never leaves orphan edges.
//! Entity resolution is best-effort: a fact's subject/object that matches a
//! known entity name produces a `fact_mentions_entity` edge; unresolved names
//! dangle (v1 — no speculative entity creation here).

use std::collections::BTreeMap;

use cozo::{DataValue, Num};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;
use crate::rows::SpineEdge;

fn s(value: impl Into<String>) -> DataValue {
    DataValue::Str(value.into().into())
}
fn f(value: f64) -> DataValue {
    DataValue::Num(Num::Float(value))
}

impl GraphStore {
    /// Rebuild the mother-node spine edges for one source from its current
    /// `raw_chunks` + live `atomic_facts` + resolved entities. Returns the
    /// number of edges written.
    pub fn rebuild_spine_for_source(&self, source_id: &str) -> Result<usize> {
        let chunks = self.get_raw_chunks_for_source(source_id)?;
        let facts = self.get_atomic_facts_for_source(source_id)?;
        // Entity name (lowercased) → id, for fact_mentions_entity resolution.
        let entities = self.get_all_entities()?; // (id, name, type)
        let name_to_id: BTreeMap<String, String> = entities
            .into_iter()
            .map(|(id, name, _)| (name.to_lowercase(), id))
            .collect();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let mut edges: Vec<SpineEdge> = Vec::new();
        // doc → chunk
        for c in &chunks {
            edges.push(SpineEdge {
                from_id: source_id.to_string(),
                to_id: c.id.clone(),
                edge_kind: "doc_has_chunk".into(),
                source_id: source_id.to_string(),
                confidence: 1.0,
                created_at: now,
            });
        }
        // chunk → fact, fact → entity (live facts only)
        for fa in facts.iter().filter(|f| f.is_live()) {
            if !fa.chunk_id.is_empty() {
                edges.push(SpineEdge {
                    from_id: fa.chunk_id.clone(),
                    to_id: fa.id.clone(),
                    edge_kind: "chunk_has_fact".into(),
                    source_id: source_id.to_string(),
                    confidence: fa.confidence as f64,
                    created_at: now,
                });
            }
            for name in [&fa.subject, &fa.object] {
                if let Some(eid) = name_to_id.get(&name.to_lowercase()) {
                    edges.push(SpineEdge {
                        from_id: fa.id.clone(),
                        to_id: eid.clone(),
                        edge_kind: "fact_mentions_entity".into(),
                        source_id: source_id.to_string(),
                        confidence: fa.confidence as f64,
                        created_at: now,
                    });
                }
            }
        }

        self.write_spine_edges_for_source(source_id, &edges)?;
        Ok(edges.len())
    }

    /// Wholesale replace one source's spine edges (scoped `:rm` then batch put).
    pub fn write_spine_edges_for_source(&self, source_id: &str, edges: &[SpineEdge]) -> Result<()> {
        // Clear prior edges for this source.
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));
        self.query(
            "?[from_id, to_id, edge_kind] := *spine_edges{from_id, to_id, edge_kind, source_id}, \
             source_id == $sid\n:rm spine_edges {from_id, to_id, edge_kind}",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("write_spine_edges :rm: {e}")))?;

        if edges.is_empty() {
            return Ok(());
        }
        let payload: Vec<DataValue> = edges
            .iter()
            .map(|e| {
                DataValue::List(vec![
                    s(&e.from_id),
                    s(&e.to_id),
                    s(&e.edge_kind),
                    s(&e.source_id),
                    f(e.confidence),
                    f(e.created_at),
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
        .map_err(|e| Error::GraphStorage(format!("write_spine_edges :put: {e}")))?;
        Ok(())
    }

    /// Spine neighbours of a node by edge kind (one hop). Used by retrieval
    /// graph-expansion to walk the mother-node hierarchy.
    pub fn spine_neighbors(&self, from_id: &str, edge_kind: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("fid".into(), DataValue::Str(from_id.into()));
        params.insert("kind".into(), DataValue::Str(edge_kind.into()));
        let res = self.query(
            "?[to_id] := *spine_edges{from_id, to_id, edge_kind}, from_id == $fid, edge_kind == $kind",
            params,
        )?;
        Ok(res
            .rows
            .iter()
            .filter_map(|r| r.first().map(|v| match v {
                DataValue::Str(s) => s.to_string(),
                other => format!("{other:?}"),
            }))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::AtomicFact;
    use thinkingroot_core::Entity;

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::init(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn spine_links_doc_chunk_fact_and_entity() {
        let (_d, store) = store();

        // One chunk for source "src".
        let mut rows = crate::graph::PerSourceRows::default();
        rows.raw_chunks.push(crate::rows::RawChunkRow {
            id: "chunk1".into(),
            source_id: "src".into(),
            chunk_index: 0,
            chunk_type: "text".into(),
            content: "Yuriy teaches the course".into(),
            byte_start: 0,
            byte_end: 24,
            content_blake3: "h".into(),
            created_at: 0.0,
        });
        store
            .transactional_rebuild_sources(&[("src".to_string(), rows)])
            .unwrap();

        // A known entity "Yuriy".
        let ent = Entity::new("Yuriy", thinkingroot_core::types::EntityType::Person);
        store.insert_entity(&ent).unwrap();

        // One fact whose subject is the entity, in chunk1.
        let mut fact = AtomicFact {
            id: AtomicFact::derive_id("src", 0, 24, "teaches"),
            source_id: "src".into(),
            chunk_id: "chunk1".into(),
            subject: "Yuriy".into(),
            predicate: "teaches".into(),
            object: "course".into(),
            statement: "Yuriy teaches the course".into(),
            confidence: 0.9,
            extraction_model: "m".into(),
            workspace_id: "ws".into(),
            sensitivity: "Public".into(),
            byte_start: 0,
            byte_end: 24,
            content_blake3: "h".into(),
            valid_from: 1.0,
            valid_until: -1.0,
            created_at: 1.0,
        };
        store.insert_atomic_facts_batch(&[fact.clone()]).unwrap();

        let n = store.rebuild_spine_for_source("src").unwrap();
        // 1 doc_has_chunk + 1 chunk_has_fact + 1 fact_mentions_entity (Yuriy).
        assert_eq!(n, 3, "expected doc→chunk, chunk→fact, fact→entity");

        let chunks = store.spine_neighbors("src", "doc_has_chunk").unwrap();
        assert_eq!(chunks, vec!["chunk1".to_string()]);
        let facts = store.spine_neighbors("chunk1", "chunk_has_fact").unwrap();
        assert_eq!(facts, vec![fact.id.clone()]);

        // A tombstoned fact must drop out of the spine on rebuild.
        fact.valid_until = 5.0;
        store.insert_atomic_facts_batch(&[fact.clone()]).unwrap();
        let n2 = store.rebuild_spine_for_source("src").unwrap();
        assert_eq!(n2, 1, "only doc→chunk remains once the fact is tombstoned");
    }
}
