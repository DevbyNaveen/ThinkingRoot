//! `atomic_facts` insert + read helpers.
//!
//! Bridges the in-memory [`AtomicFact`] (LLM-extracted SVO proposition,
//! `thinkingroot-core`) to the CozoDB `atomic_facts` table. Mirrors the
//! `witness_inserts.rs` idiom: 500-row CozoDB batches, one
//! `?[…] <- $rows :put atomic_facts {…}` script per batch.
//!
//! Ids MUST carry the `af:` namespace prefix (set by
//! `AtomicFact::derive_id`) so retrieval fusion never confuses a fact with
//! a claim id — `insert_atomic_facts_batch` rejects any row that doesn't.

use std::collections::BTreeMap;

use cozo::{DataValue, Num};
use thinkingroot_core::types::AtomicFact;
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;
use crate::rows::RawChunkRow;

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
fn ds(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}
fn du(v: &DataValue) -> u64 {
    match v {
        DataValue::Num(Num::Int(i)) => *i as u64,
        DataValue::Num(Num::Float(f)) => *f as u64,
        _ => 0,
    }
}
fn df(v: &DataValue) -> f64 {
    match v {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

const COLS: &str = "id, source_id, chunk_id, subject, predicate, object, statement, \
    confidence, extraction_model, workspace_id, sensitivity, byte_start, byte_end, \
    content_blake3, valid_from, valid_until, created_at";

/// Non-PK columns (everything after `id`) for the `:put … {id => …}` clause.
const NONPK_COLS: &str = "source_id, chunk_id, subject, predicate, object, statement, \
    confidence, extraction_model, workspace_id, sensitivity, byte_start, byte_end, \
    content_blake3, valid_from, valid_until, created_at";

fn row_to_fact(row: &[DataValue]) -> Option<AtomicFact> {
    if row.len() < 17 {
        return None;
    }
    Some(AtomicFact {
        id: ds(&row[0]),
        source_id: ds(&row[1]),
        chunk_id: ds(&row[2]),
        subject: ds(&row[3]),
        predicate: ds(&row[4]),
        object: ds(&row[5]),
        statement: ds(&row[6]),
        confidence: df(&row[7]) as f32,
        extraction_model: ds(&row[8]),
        workspace_id: ds(&row[9]),
        sensitivity: ds(&row[10]),
        byte_start: du(&row[11]),
        byte_end: du(&row[12]),
        content_blake3: ds(&row[13]),
        valid_from: df(&row[14]),
        valid_until: df(&row[15]),
        created_at: df(&row[16]),
    })
}

impl GraphStore {
    /// Batch-insert atomic facts (500/CozoDB call). Rejects any id missing
    /// the `af:` prefix — the namespace guard that keeps facts out of the
    /// claim id-space in retrieval fusion.
    pub fn insert_atomic_facts_batch(&self, facts: &[AtomicFact]) -> Result<()> {
        for fact in facts {
            if !fact.id.starts_with("af:") {
                return Err(Error::GraphStorage(format!(
                    "atomic fact id `{}` missing required `af:` prefix",
                    fact.id
                )));
            }
        }
        for chunk in facts.chunks(CHUNK) {
            let mut payload: Vec<DataValue> = Vec::with_capacity(chunk.len());
            for fa in chunk {
                payload.push(DataValue::List(vec![
                    s(&fa.id),
                    s(&fa.source_id),
                    s(&fa.chunk_id),
                    s(&fa.subject),
                    s(&fa.predicate),
                    s(&fa.object),
                    s(&fa.statement),
                    f(fa.confidence as f64),
                    s(&fa.extraction_model),
                    s(&fa.workspace_id),
                    s(&fa.sensitivity),
                    i(fa.byte_start as i64),
                    i(fa.byte_end as i64),
                    s(&fa.content_blake3),
                    f(fa.valid_from),
                    f(fa.valid_until),
                    f(fa.created_at),
                ]));
            }
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(payload));
            let script = format!("?[{COLS}] <- $rows\n:put atomic_facts {{ id => {NONPK_COLS} }}");
            self.query(&script, params)
                .map_err(|e| Error::GraphStorage(format!("insert_atomic_facts_batch: {e}")))?;
        }
        Ok(())
    }

    /// All atomic facts for one source (both live and tombstoned).
    pub fn get_atomic_facts_for_source(&self, source_id: &str) -> Result<Vec<AtomicFact>> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));
        let script = format!("?[{COLS}] := *atomic_facts{{{COLS}}}, source_id == $sid");
        let res = self.query(&script, params)?;
        Ok(res.rows.iter().filter_map(|r| row_to_fact(r)).collect())
    }

    /// All facts for a source INCLUDING tombstoned ones, newest first — the
    /// version timeline (live current value + its superseded history).
    pub fn get_fact_history_for_source(&self, source_id: &str) -> Result<Vec<AtomicFact>> {
        let mut facts = self.get_atomic_facts_for_source(source_id)?;
        facts.sort_by(|a, b| {
            b.is_live()
                .cmp(&a.is_live())
                .then(b.created_at.partial_cmp(&a.created_at).unwrap_or(std::cmp::Ordering::Equal))
        });
        Ok(facts)
    }

    /// One atomic fact by id (for retrieval hydration + citation byte-anchor).
    pub fn get_atomic_fact_by_id(&self, id: &str) -> Result<Option<AtomicFact>> {
        let mut params = BTreeMap::new();
        params.insert("fid".into(), DataValue::Str(id.into()));
        let script = format!("?[{COLS}] := *atomic_facts{{{COLS}}}, id == $fid");
        let res = self.query(&script, params)?;
        Ok(res.rows.first().and_then(|r| row_to_fact(r)))
    }

    /// Live atomic facts whose subject OR object matches `name`
    /// (case-insensitive) — the cross-document entity profile.
    pub fn get_atomic_facts_mentioning(&self, name: &str) -> Result<Vec<AtomicFact>> {
        let needle = name.to_lowercase();
        Ok(self
            .get_all_atomic_facts()?
            .into_iter()
            .filter(|f| {
                f.is_live()
                    && (f.subject.to_lowercase() == needle || f.object.to_lowercase() == needle)
            })
            .collect())
    }

    /// Every atomic fact in the workspace (both live and tombstoned).
    pub fn get_all_atomic_facts(&self) -> Result<Vec<AtomicFact>> {
        let script = format!("?[{COLS}] := *atomic_facts{{{COLS}}}");
        let res = self.query_read(&script)?;
        Ok(res.rows.iter().filter_map(|r| row_to_fact(r)).collect())
    }

    /// Bi-temporal supersession: tombstone (set `valid_until = now`) every
    /// LIVE fact of `source_id` whose id is NOT in `keep_ids` — i.e. facts a
    /// re-extraction no longer confirms. Tombstoned facts are kept (never
    /// deleted) so the console can render the version timeline. Returns the
    /// number tombstoned.
    pub fn supersede_facts_not_in(
        &self,
        source_id: &str,
        keep_ids: &[String],
        now: f64,
    ) -> Result<usize> {
        use std::collections::HashSet;
        let keep: HashSet<&str> = keep_ids.iter().map(|s| s.as_str()).collect();
        let updated: Vec<AtomicFact> = self
            .get_atomic_facts_for_source(source_id)?
            .into_iter()
            .filter(|f| f.is_live() && !keep.contains(f.id.as_str()))
            .map(|mut f| {
                f.valid_until = now;
                f
            })
            .collect();
        let n = updated.len();
        if n > 0 {
            self.insert_atomic_facts_batch(&updated)?;
        }
        Ok(n)
    }

    // ─── atomic_extract_queue — the async post-compile work list ───────────

    /// Enqueue (or re-enqueue, resetting to pending) sources for async LLM
    /// atomic-fact extraction. Called by the compile pipeline after Phase 6.7.
    pub fn enqueue_atomic_extract(&self, source_ids: &[String], now: f64) -> Result<()> {
        if source_ids.is_empty() {
            return Ok(());
        }
        let payload: Vec<DataValue> = source_ids
            .iter()
            .map(|sid| DataValue::List(vec![s(sid), s("pending"), f(now), i(0)]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("rows".into(), DataValue::List(payload));
        self.query(
            "?[source_id, status, enqueued_at, attempts] <- $rows\n\
             :put atomic_extract_queue { source_id => status, enqueued_at, attempts }",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("enqueue_atomic_extract: {e}")))?;
        Ok(())
    }

    /// Up to `limit` pending sources as `(source_id, attempts)`.
    pub fn pending_atomic_extract(&self, limit: usize) -> Result<Vec<(String, i64)>> {
        let script = format!(
            "?[source_id, attempts] := *atomic_extract_queue{{source_id, status, attempts}}, \
             status == 'pending' :limit {limit}"
        );
        let res = self.query_read(&script)?;
        Ok(res
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 2 {
                    return None;
                }
                Some((ds(&r[0]), du(&r[1]) as i64))
            })
            .collect())
    }

    /// Remove a source from the queue (success, or gave up after retries).
    pub fn complete_atomic_extract(&self, source_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));
        self.query(
            "?[source_id] := *atomic_extract_queue{source_id}, source_id == $sid\n\
             :rm atomic_extract_queue {source_id}",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("complete_atomic_extract: {e}")))?;
        Ok(())
    }

    /// Bump the retry counter for a source that failed this tick (stays pending).
    pub fn bump_atomic_extract_attempt(&self, source_id: &str, now: f64, attempts: i64) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));
        params.insert("now".into(), DataValue::Num(Num::Float(now)));
        params.insert("att".into(), DataValue::Num(Num::Int(attempts + 1)));
        self.query(
            "?[source_id, status, enqueued_at, attempts] <- [[$sid, 'pending', $now, $att]]\n\
             :put atomic_extract_queue { source_id => status, enqueued_at, attempts }",
            params,
        )
        .map_err(|e| Error::GraphStorage(format!("bump_atomic_extract_attempt: {e}")))?;
        Ok(())
    }

    /// All verbatim chunks for one source, ordered by chunk_index (the spine's
    /// doc→chunk children + the extractor's input).
    pub fn get_raw_chunks_for_source(&self, source_id: &str) -> Result<Vec<RawChunkRow>> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));
        let res = self.query(
            "?[id, source_id, chunk_index, chunk_type, content, byte_start, byte_end, content_blake3, created_at] := \
             *raw_chunks{id, source_id, chunk_index, chunk_type, content, byte_start, byte_end, content_blake3, created_at}, \
             source_id == $sid",
            params,
        )?;
        let mut out: Vec<RawChunkRow> = res
            .rows
            .iter()
            .filter_map(|r| {
                if r.len() < 9 {
                    return None;
                }
                Some(RawChunkRow {
                    id: ds(&r[0]),
                    source_id: ds(&r[1]),
                    chunk_index: du(&r[2]) as u32,
                    chunk_type: ds(&r[3]),
                    content: ds(&r[4]),
                    byte_start: du(&r[5]),
                    byte_end: du(&r[6]),
                    content_blake3: ds(&r[7]),
                    created_at: df(&r[8]),
                })
            })
            .collect();
        out.sort_by_key(|c| c.chunk_index);
        Ok(out)
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

    fn fact(source: &str, predicate: &str, start: u64) -> AtomicFact {
        AtomicFact {
            id: AtomicFact::derive_id(source, start, start + 10, predicate),
            source_id: source.into(),
            chunk_id: "ch".into(),
            subject: "S".into(),
            predicate: predicate.into(),
            object: "O".into(),
            statement: "S verb O".into(),
            confidence: 0.8,
            extraction_model: "m".into(),
            workspace_id: "ws".into(),
            sensitivity: "Public".into(),
            byte_start: start,
            byte_end: start + 10,
            content_blake3: "h".into(),
            valid_from: 1.0,
            valid_until: -1.0,
            created_at: 1.0,
        }
    }

    #[test]
    fn insert_and_read_atomic_facts_by_source() {
        let (_d, store) = store();
        let facts = vec![fact("srcA", "teaches", 0), fact("srcA", "owns", 20), fact("srcB", "uses", 0)];
        store.insert_atomic_facts_batch(&facts).unwrap();

        let a = store.get_atomic_facts_for_source("srcA").unwrap();
        assert_eq!(a.len(), 2);
        assert!(a.iter().all(|f| f.id.starts_with("af:")));
        let all = store.get_all_atomic_facts().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn rejects_id_without_af_prefix() {
        let (_d, store) = store();
        let mut bad = fact("srcA", "teaches", 0);
        bad.id = "claim:123".into();
        assert!(store.insert_atomic_facts_batch(&[bad]).is_err());
    }

    #[test]
    fn supersede_facts_not_in_tombstones_dropped_facts() {
        let (_d, store) = store();
        let f1 = fact("srcA", "teaches", 0);
        let f2 = fact("srcA", "owns", 20);
        store.insert_atomic_facts_batch(&[f1.clone(), f2.clone()]).unwrap();

        // Re-extraction confirms only f1 → f2 must be tombstoned (not deleted).
        let n = store
            .supersede_facts_not_in("srcA", &[f1.id.clone()], 99.0)
            .unwrap();
        assert_eq!(n, 1);

        let all = store.get_atomic_facts_for_source("srcA").unwrap();
        assert_eq!(all.len(), 2, "tombstoned fact is kept for the version timeline");
        let live: Vec<_> = all.iter().filter(|f| f.is_live()).collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, f1.id);
        let dead: Vec<_> = all.iter().filter(|f| !f.is_live()).collect();
        assert_eq!(dead[0].valid_until, 99.0);
    }

    #[test]
    fn extract_queue_enqueue_drain_complete() {
        let (_d, store) = store();
        store
            .enqueue_atomic_extract(&["s1".into(), "s2".into()], 1.0)
            .unwrap();
        let pending = store.pending_atomic_extract(10).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|(_, att)| *att == 0));

        store.complete_atomic_extract("s1").unwrap();
        let after = store.pending_atomic_extract(10).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].0, "s2");

        store.bump_atomic_extract_attempt("s2", 2.0, 0).unwrap();
        let bumped = store.pending_atomic_extract(10).unwrap();
        assert_eq!(bumped[0].1, 1, "attempt counter incremented");
    }

    #[test]
    fn raw_chunks_read_ordered_by_index() {
        let (_d, store) = store();
        // Insert via the per-source rebuild path.
        let mut rows = crate::graph::PerSourceRows::default();
        for k in [2u32, 0, 1] {
            rows.raw_chunks.push(crate::rows::RawChunkRow {
                id: format!("rc-{k}"),
                source_id: "src".into(),
                chunk_index: k,
                chunk_type: "text".into(),
                content: format!("c{k}"),
                byte_start: (k * 10) as u64,
                byte_end: (k * 10 + 4) as u64,
                content_blake3: "h".into(),
                created_at: 0.0,
            });
        }
        store
            .transactional_rebuild_sources(&[("src".to_string(), rows)])
            .unwrap();
        let chunks = store.get_raw_chunks_for_source("src").unwrap();
        let idxs: Vec<u32> = chunks.iter().map(|c| c.chunk_index).collect();
        assert_eq!(idxs, vec![0, 1, 2], "chunks returned in index order");
    }
}
