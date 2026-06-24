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
use thinkingroot_core::types::EntityType;
use thinkingroot_core::Entity;

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

    /// Promote a source's live atomic facts into first-class graph ENTITY NODES
    /// + entity↔entity RELATIONS, so the LLM‑extracted knowledge appears in the
    /// Neural Graph (and can be clustered into concepts). Subject/object strings
    /// are lightly normalised (drop leading articles, skip junk/over‑long
    /// phrases); the predicate becomes the relation type. Returns the number of
    /// NEW entity nodes created. Idempotent (re‑run reuses existing entities).
    pub fn promote_fact_entities_and_relations(&self, source_id: &str) -> Result<usize> {
        let facts: Vec<_> = self
            .get_atomic_facts_for_source(source_id)?
            .into_iter()
            .filter(|f| f.is_live())
            .collect();
        if facts.is_empty() {
            return Ok(0);
        }
        let mut name_to_id: BTreeMap<String, String> = self
            .get_all_entities()?
            .into_iter()
            .map(|(id, name, _)| (name.to_lowercase(), id))
            .collect();

        let mut created = 0usize;
        let mut triples: Vec<(String, String, String)> = Vec::new();
        for f in &facts {
            let (Some(subj), Some(obj)) = (clean_entity_name(&f.subject), clean_entity_name(&f.object))
            else {
                continue;
            };
            let sid = self.ensure_entity(&mut name_to_id, &subj, &mut created)?;
            let oid = self.ensure_entity(&mut name_to_id, &obj, &mut created)?;
            if sid == oid {
                continue;
            }
            let rel = normalize_predicate(&f.predicate);
            triples.push((sid, oid, rel));
        }
        // Direct insert with a fixed confidence — these relations are evidenced
        // by the fact itself, not by per-source noisy-OR aggregation (which
        // `update_entity_relations_for_triples` assumes and which fact-promoted
        // entities lack).
        for (sid, oid, rel) in &triples {
            self.link_entities(sid, oid, rel, 0.8)?;
        }
        Ok(created)
    }

    fn ensure_entity(
        &self,
        cache: &mut BTreeMap<String, String>,
        name: &str,
        created: &mut usize,
    ) -> Result<String> {
        let key = name.to_lowercase();
        if let Some(id) = cache.get(&key) {
            return Ok(id.clone());
        }
        let entity = Entity::new(name, guess_entity_type(name));
        let id = entity.id.to_string();
        self.insert_entity(&entity)?;
        cache.insert(key, id.clone());
        *created += 1;
        Ok(id)
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

/// Normalise a fact subject/object into a clean entity name, or `None` to skip
/// junk. Drops a leading article, trims, and rejects empties / over‑long
/// phrases / pure stop‑words so the graph stays legible.
fn clean_entity_name(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    for art in ["the ", "a ", "an ", "The ", "A ", "An "] {
        if let Some(rest) = s.strip_prefix(art) {
            s = rest.trim();
            break;
        }
    }
    let s = s.trim_matches(|c: char| c == '.' || c == ',' || c == '"').trim();
    let words = s.split_whitespace().count();
    if s.len() < 2 || s.len() > 60 || words == 0 || words > 6 {
        return None;
    }
    Some(s.to_string())
}

/// Short relation label from a predicate (snake‑case, bounded).
fn normalize_predicate(pred: &str) -> String {
    let p = pred.trim().to_lowercase().replace([' ', '-'], "_");
    let p: String = p.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
    if p.is_empty() {
        "related".to_string()
    } else {
        p.chars().take(40).collect()
    }
}

/// Best‑effort entity type from the name (drives the graph SHAPE). Defaults to
/// Concept (the neutral ring) — honest when we can't tell.
fn guess_entity_type(name: &str) -> EntityType {
    let lc = name.to_lowercase();
    let words: Vec<&str> = name.split_whitespace().collect();
    let looks_proper = words.len() <= 3 && words.iter().all(|w| w.chars().next().is_some_and(|c| c.is_uppercase()));
    if lc.contains("team") {
        EntityType::Team
    } else if lc.ends_with(" inc") || lc.contains("robotics") || lc.contains("consortium") || lc.contains("corp") {
        EntityType::Organization
    } else if looks_proper && words.len() == 2 {
        // Two capitalised words → likely a person name.
        EntityType::Person
    } else {
        EntityType::Concept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::AtomicFact;

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::init(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn clean_entity_name_filters_junk() {
        assert_eq!(clean_entity_name("Acme Robotics").as_deref(), Some("Acme Robotics"));
        assert_eq!(clean_entity_name("the database course").as_deref(), Some("database course"));
        assert_eq!(clean_entity_name("a"), None); // too short after article
        assert_eq!(clean_entity_name("the engineering team of the central robotics division here"), None); // >6 words
    }

    #[test]
    fn promote_creates_entities_and_relations() {
        let (_d, store) = store();
        let before = store.get_all_entities().unwrap().len();

        let f = |pred: &str, subj: &str, obj: &str, start: u64| AtomicFact {
            id: AtomicFact::derive_id("src", start, start + 5, pred),
            source_id: "src".into(),
            chunk_id: "c".into(),
            subject: subj.into(),
            predicate: pred.into(),
            object: obj.into(),
            statement: format!("{subj} {pred} {obj}"),
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
        store
            .insert_atomic_facts_batch(&[
                f("founded_by", "Acme Robotics", "Dana Reyes", 0),
                f("designed", "Mateo Silva", "Atlas OS", 20),
            ])
            .unwrap();

        let created = store.promote_fact_entities_and_relations("src").unwrap();
        assert!(created >= 4, "expected ≥4 new entities, got {created}");
        let after = store.get_all_entities().unwrap().len();
        assert!(
            after >= before + 4,
            "entities should grow: before={before} after={after} created={created}"
        );
        let rels = store.get_all_relations().unwrap();
        assert!(!rels.is_empty(), "entity relations must be created from facts");

        // Idempotent: re-run reuses entities, creates none new.
        let again = store.promote_fact_entities_and_relations("src").unwrap();
        assert_eq!(again, 0, "re-promotion must not duplicate entities");
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
