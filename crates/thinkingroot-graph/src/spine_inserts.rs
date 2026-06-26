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

    /// Like [`Self::ensure_entity`] but with an explicit, authoritative
    /// [`EntityType`] (from the EDC typing pass) instead of the mechanical
    /// `guess_entity_type` heuristic.
    fn ensure_entity_typed(
        &self,
        cache: &mut BTreeMap<String, String>,
        name: &str,
        entity_type: EntityType,
        created: &mut usize,
    ) -> Result<String> {
        let key = name.to_lowercase();
        if let Some(id) = cache.get(&key) {
            return Ok(id.clone());
        }
        let entity = Entity::new(name, entity_type);
        let id = entity.id.to_string();
        self.insert_entity(&entity)?;
        cache.insert(key, id.clone());
        *created += 1;
        Ok(id)
    }

    /// EDC-typed promotion (the write-boundary clean-extraction path). Identical
    /// in shape to [`Self::promote_fact_entities_and_relations`] but driven by an
    /// authoritative typing map (keyed by the RAW fact subject/object string,
    /// lowercased) produced off-lock by the LLM typing/verify stage:
    ///   * `keep == false` ⇒ the candidate is a literal/value/date/count → it is
    ///     NOT promoted to a node (kills over-extraction).
    ///   * the supplied [`EntityType`] is used verbatim (kills mis-typing).
    ///   * a relation is written ONLY when BOTH endpoints survived as real
    ///     entities → **resolve-before-relate**, so dangling edges become
    ///     structurally impossible.
    /// Candidates absent from the map fall back to the mechanical heuristic
    /// (graceful — e.g. the LLM was unavailable), so no knowledge is lost.
    pub fn promote_fact_entities_and_relations_typed(
        &self,
        source_id: &str,
        typing: &BTreeMap<String, (String, EntityType, bool)>,
    ) -> Result<usize> {
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

        // Resolve one fact endpoint to a (canonical, type) pair, or `None` to
        // skip it (literal, junk, or LLM-rejected). The map key is the RAW fact
        // string (lowercased) so it aligns with how `serve` harvested candidates.
        let resolve = |raw: &str| -> Option<(String, EntityType)> {
            match typing.get(&raw.trim().to_lowercase()) {
                Some((_, _, false)) => None, // explicitly rejected (literal/value)
                Some((canonical, ty, true)) => {
                    // Still apply the junk-name floor to the canonical form.
                    clean_entity_name(canonical).map(|c| (c, *ty))
                }
                // Unknown to the map → mechanical fallback (no regression).
                None => clean_entity_name(raw).map(|c| {
                    let ty = guess_entity_type(&c);
                    (c, ty)
                }),
            }
        };

        let mut created = 0usize;
        let mut triples: Vec<(String, String, String)> = Vec::new();
        // Every entity confirmed by THIS extraction — used to refresh its
        // attestation (the bi-temporal liveness signal for #3).
        let mut attested: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for f in &facts {
            let (Some((subj, s_ty)), Some((obj, o_ty))) = (resolve(&f.subject), resolve(&f.object))
            else {
                continue; // resolve-before-relate: at least one endpoint dropped
            };
            let sid = self.ensure_entity_typed(&mut name_to_id, &subj, s_ty, &mut created)?;
            let oid = self.ensure_entity_typed(&mut name_to_id, &obj, o_ty, &mut created)?;
            attested.insert(sid.clone());
            attested.insert(oid.clone());
            if sid == oid {
                continue;
            }
            triples.push((sid, oid, normalize_predicate(&f.predicate)));
        }
        for (sid, oid, rel) in &triples {
            self.link_entities(sid, oid, rel, 0.8)?;
        }
        // Refresh attestation for every confirmed entity (additive, zero-risk).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let ids: Vec<String> = attested.into_iter().collect();
        self.attest_entities(&ids, now)?;
        Ok(created)
    }

    /// #3 — refresh the attestation of confirmed entities (sidecar bi-temporal
    /// record). `retired_at = -1.0` (live), `last_attested_at = now`. Idempotent
    /// `:put`. Called from typed promotion AFTER the spine rebuild, so a
    /// re-confirmed entity always carries a fresh `last_attested_at` — that
    /// recency is the liveness signal the retirement sweep keys on.
    pub fn attest_entities(&self, entity_ids: &[String], now: f64) -> Result<()> {
        if entity_ids.is_empty() {
            return Ok(());
        }
        let payload: Vec<DataValue> = entity_ids
            .iter()
            .map(|id| DataValue::List(vec![s(id), DataValue::Bool(true), f(now), f(-1.0)]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("rows".into(), DataValue::List(payload));
        self.query(
            "?[entity_id, attested, last_attested_at, retired_at] <- $rows\n\
             :put entity_attestation { entity_id => attested, last_attested_at, retired_at }",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("attest_entities: {e}")))?;
        Ok(())
    }

    /// #3 — retire attested entities that no longer have ANY live
    /// `fact_mentions_entity` spine edge (bi-temporal: set `retired_at = now`,
    /// KEEP the node + its history; NEVER hard-delete).
    ///
    /// Why the spine signal is globally sound: `rebuild_spine_for_source`
    /// replaces only ONE source's edges, so an entity mentioned by any source
    /// keeps an edge; an untouched source keeps its edges (its entities are
    /// never wrongly retired). Only a recompile that DROPS an entity removes
    /// that source's edge to it — and if no other source mentions it, it
    /// becomes unmentioned and is retired here. Requires the drain to rebuild
    /// the spine AFTER promotion (so freshly-promoted entities have edges).
    ///
    /// Gated by the caller behind `TR_ENTITY_RETIRE` (default OFF) until
    /// validated on real data. Returns the number of entities newly retired.
    pub fn retire_unmentioned_entities(&self, now: f64) -> Result<usize> {
        let pending = self.query_read(
            "?[entity_id] := *entity_attestation{entity_id, retired_at}, retired_at < 0.0, \
             not *spine_edges{to_id: entity_id, edge_kind: 'fact_mentions_entity'}",
        )?;
        let n = pending.rows.len();
        if n == 0 {
            return Ok(0);
        }
        let mut params = BTreeMap::new();
        params.insert("now".into(), f(now));
        self.query(
            "?[entity_id, attested, last_attested_at, retired_at] := \
             *entity_attestation{entity_id, last_attested_at, retired_at: old_r}, \
             old_r < 0.0, \
             not *spine_edges{to_id: entity_id, edge_kind: 'fact_mentions_entity'}, \
             attested = false, retired_at = $now\n\
             :put entity_attestation { entity_id => attested, last_attested_at, retired_at }",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("retire_unmentioned_entities: {e}")))?;
        Ok(n)
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

    /// Wipe ALL derived knowledge (entities, relations, claims, witnesses,
    /// atomic facts, chunks, spine, concepts, summaries, extract queue) — keeps
    /// the `sources` rows so a recompile re-discovers + rebuilds everything
    /// cleanly. Used by the workspace reset (clears accumulated junk so the new
    /// clean-extraction code starts from a blank graph). Best-effort per table.
    pub fn clear_all_knowledge(&self) -> Result<()> {
        // (table, PK projection) — one `:rm` per table.
        let specs: &[(&str, &str)] = &[
            ("entities", "id"),
            ("entity_relations", "from_id, to_id, relation_type"),
            ("source_entity_relations", "source_id, from_id, to_id, relation_type"),
            ("claims", "id"),
            ("claim_entity_edges", "claim_id, entity_id"),
            ("atomic_facts", "id"),
            ("raw_chunks", "id"),
            ("spine_edges", "from_id, to_id, edge_kind"),
            ("concept_nodes", "id"),
            ("atomic_extract_queue", "source_id"),
            ("summary_nodes", "id"),
            ("witnesses", "id"),
            ("chunks_residual", "id"),
            ("function_calls", "id"),
            ("headings", "id"),
            ("doc_tags", "id"),
            ("code_links", "id"),
            ("data_rows", "id"),
        ];
        for (table, pk) in specs {
            let script = format!("?[{pk}] := *{table}{{{pk}}}\n:rm {table} {{{pk}}}");
            // Tolerate missing tables / empty relations — best-effort wipe.
            let _ = self.query(&script, BTreeMap::new());
        }
        Ok(())
    }

    /// Mother→entity edges for the Neural Graph: distinct `(source_id, entity_id)`
    /// pairs derived from the `fact_mentions_entity` spine (a document's facts
    /// mention these entities). Lets the graph draw each DOCUMENT as a mother
    /// node connected to the entities it produced.
    pub fn get_mother_entity_edges(&self) -> Result<Vec<(String, String)>> {
        let res = self.query_read(
            "?[source_id, to_id] := *spine_edges{source_id, to_id, edge_kind}, \
             edge_kind == 'fact_mentions_entity'",
        )?;
        let mut out: Vec<(String, String)> = res
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 2 {
                    return None;
                }
                let ds = |v: &cozo::DataValue| match v {
                    cozo::DataValue::Str(s) => s.to_string(),
                    other => format!("{other:?}"),
                };
                Some((ds(&r[0]), ds(&r[1])))
            })
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
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

/// Comparison / constraint / value phrases that are NOT entities (common in
/// schema/spec docs). A fact subject/object containing one is dropped.
const NON_ENTITY_MARKERS: &[&str] = &[
    "more than", "less than", "greater than", "at least", "at most", "must be",
    "equal to", "not null", "default ", "between ", "ranges from", " per ",
    "cannot be", "should be", "no more", "up to", "minimum", "maximum",
];

/// Normalise a fact subject/object into a clean entity name, or `None` to skip
/// non-entity junk (values, numbers, constraint clauses, mangled fragments).
/// This is the gate that keeps the graph legible — SOTA memory graphs show
/// named entities, NOT every value or clause.
fn clean_entity_name(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    for art in ["the ", "a ", "an ", "The ", "A ", "An "] {
        if let Some(rest) = s.strip_prefix(art) {
            s = rest.trim();
            break;
        }
    }
    let s = s
        .trim_matches(|c: char| c == '.' || c == ',' || c == '"' || c == ':' || c == ';')
        .trim();
    let words: Vec<&str> = s.split_whitespace().collect();
    if s.len() < 2 || s.len() > 48 || words.is_empty() || words.len() > 5 {
        return None;
    }
    // Must contain a letter; must NOT start with a digit (values like "20 …").
    if !s.chars().any(|c| c.is_alphabetic()) {
        return None;
    }
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    // Reject if any token is a bare number or single char ("Camera Nr N 3 0",
    // "1,2,…,12") — schema fragments, not entity names.
    if words
        .iter()
        .any(|w| w.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.') || w.chars().count() == 1)
    {
        return None;
    }
    // Reject constraint / comparison / value phrases.
    let lc = s.to_lowercase();
    if NON_ENTITY_MARKERS.iter().any(|m| lc.contains(m)) {
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

/// Schema/structural words that a real PERSON name never contains — used to
/// stop columns like "Camera Nr" / "Teacher Id" being mislabelled Person.
const SCHEMA_WORDS: &[&str] = &[
    "nr", "id", "no", "code", "date", "number", "type", "name", "field", "key",
    "table", "column", "pk", "fk", "view", "class", "group", "groups", "rubric",
    "department", "departments", "service", "services", "book", "books",
];

/// Best‑effort entity type from the name (drives the graph SHAPE). Defaults to
/// Concept (the neutral ring) — honest when we can't tell. Person is inferred
/// ONLY for a clean two-word Title-Case alphabetic name that contains no schema
/// word (so "Lena Park" → Person, but "Camera Nr" / "Class Nr" → Concept).
fn guess_entity_type(name: &str) -> EntityType {
    let lc = name.to_lowercase();
    let words: Vec<&str> = name.split_whitespace().collect();
    if lc == "team" || lc.ends_with(" team") || lc.starts_with("team ") {
        return EntityType::Team;
    }
    if lc.ends_with(" inc")
        || lc.ends_with(" llc")
        || lc.ends_with(" corp")
        || lc.contains("robotics")
        || lc.contains("consortium")
        || lc.contains("dynamics")
        || lc.contains(" systems")
    {
        return EntityType::Organization;
    }
    let two_title_alpha = words.len() == 2
        && words.iter().all(|w| {
            w.chars().next().is_some_and(|c| c.is_uppercase()) && w.chars().all(|c| c.is_alphabetic())
        });
    let has_schema_word = words
        .iter()
        .any(|w| SCHEMA_WORDS.contains(&w.to_lowercase().as_str()));
    if two_title_alpha && !has_schema_word {
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
        assert_eq!(clean_entity_name("a"), None);
        assert_eq!(clean_entity_name("the engineering team of the central robotics division"), None);
        // Non-entity junk that polluted the graph:
        assert_eq!(clean_entity_name("more than 0"), None);
        assert_eq!(clean_entity_name("20 patients per day"), None); // starts with digit
        assert_eq!(clean_entity_name("Camera Nr N 3 0"), None); // digit-heavy fragment
        assert_eq!(clean_entity_name("must be unique"), None);
    }

    #[test]
    fn entity_type_does_not_mislabel_columns_as_person() {
        assert_eq!(guess_entity_type("Lena Park"), EntityType::Person);
        assert_eq!(guess_entity_type("Mateo Silva"), EntityType::Person);
        assert_eq!(guess_entity_type("Camera Nr"), EntityType::Concept); // was wrongly Person
        assert_eq!(guess_entity_type("Class Nr"), EntityType::Concept);
        assert_eq!(guess_entity_type("Teacher Id"), EntityType::Concept);
        assert_eq!(guess_entity_type("Nova Dynamics"), EntityType::Organization);
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
    fn typed_promote_uses_types_drops_literals_resolves_before_relate() {
        let (_d, store) = store();
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
                f("leads", "Lena Park", "Orion Labs", 0),
                f("employs", "Orion Labs", "50 people", 20), // object is a literal
            ])
            .unwrap();

        // EDC decisions (keyed by RAW fact string, lowercased).
        let mut typing: BTreeMap<String, (String, EntityType, bool)> = BTreeMap::new();
        typing.insert("lena park".into(), ("Lena Park".into(), EntityType::Person, true));
        typing.insert(
            "orion labs".into(),
            ("Orion Labs".into(), EntityType::Organization, true),
        );
        // The literal is rejected → must NOT become a node.
        typing.insert("50 people".into(), ("50 people".into(), EntityType::Concept, false));

        let created = store
            .promote_fact_entities_and_relations_typed("src", &typing)
            .unwrap();
        assert_eq!(created, 2, "literal endpoint dropped → only 2 real entities");

        let ents = store.get_all_entities().unwrap();
        let orion = ents.iter().find(|(_, n, _)| n == "Orion Labs").unwrap();
        assert_eq!(
            EntityType::from_any(&orion.2),
            Some(EntityType::Organization),
            "EDC type used verbatim — company is org, not person"
        );
        assert!(
            !ents.iter().any(|(_, n, _)| n == "50 people"),
            "literal must never be promoted to a node"
        );

        // resolve-before-relate: employs(Orion Labs → 50 people) is dropped
        // because one endpoint vanished; only leads(Lena Park → Orion Labs) stays.
        let rels = store.get_all_relations().unwrap();
        assert_eq!(rels.len(), 1, "only the both-endpoints-kept relation survives");
    }

    #[test]
    fn retire_unmentioned_keeps_mentioned_entities() {
        let (_d, store) = store();
        let mentioned = Entity::new("Fresh Co", EntityType::Organization);
        let gone = Entity::new("Stale Co", EntityType::Organization);
        store.insert_entity(&mentioned).unwrap();
        store.insert_entity(&gone).unwrap();
        store
            .attest_entities(&[mentioned.id.to_string(), gone.id.to_string()], 1.0)
            .unwrap();

        // Only `mentioned` has a live fact_mentions_entity spine edge; `gone`
        // dropped out of its source (no edge) → it should be retired.
        store
            .write_spine_edges_for_source(
                "src",
                &[SpineEdge {
                    from_id: "fact1".into(),
                    to_id: mentioned.id.to_string(),
                    edge_kind: "fact_mentions_entity".into(),
                    source_id: "src".into(),
                    confidence: 0.9,
                    created_at: 1.0,
                }],
            )
            .unwrap();

        let retired = store.retire_unmentioned_entities(100.0).unwrap();
        assert_eq!(retired, 1, "only the unmentioned entity is retired");

        // Idempotent: already-retired entity is not retired again.
        assert_eq!(store.retire_unmentioned_entities(100.0).unwrap(), 0);

        // Both nodes KEPT — retirement never hard-deletes (history preserved).
        let names: Vec<String> = store
            .get_all_entities()
            .unwrap()
            .into_iter()
            .map(|(_, n, _)| n)
            .collect();
        assert!(names.iter().any(|n| n == "Stale Co"));
        assert!(names.iter().any(|n| n == "Fresh Co"));
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
