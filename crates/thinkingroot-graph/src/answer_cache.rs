//! Provenance-aware answer cache (final-plan §5 input #4).
//!
//! Caches a synthesized `/ask` answer keyed by (branch, query) together with
//! its **provenance set** — the grounding claim ids the answer was built from.
//! The unique property (no other memory product ships it): invalidation is
//! *causal*. When a claim is superseded / contradicted / removed,
//! [`GraphStore::invalidate_answers_for`] evicts exactly the cached answers
//! whose provenance intersects that change — so a cache hit can serve a stale
//! answer only within the TTL window, never an answer built on a fact that has
//! since changed.
//!
//! Two relations back it (declared in `graph.rs::create_schema`), mirroring the
//! capsule cache:
//!   * `answer_cache {key => answer_json, query, branch, created_at}`
//!   * `answer_cache_deps {key, object_id => object_kind}`
//!
//! HONEST LIMIT: provenance catches *changes to claims the answer used*, not
//! the arrival of a NEW relevant claim the cached answer never saw. That is the
//! classic cache-staleness gap; the TTL (serve-layer) bounds it, and the
//! feature ships behind a default-off flag until eval measures the hit rate.

use std::collections::BTreeMap;

use cozo::{DataValue, Num, ScriptMutability};
use serde::{Deserialize, Serialize};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// One cached answer row. `answer_json` is opaque to this crate (the serve
/// layer serializes its own answer struct into it).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnswerCacheRow {
    /// BLAKE3 hex of (branch, query) — see [`answer_cache_key`].
    pub key: String,
    pub answer_json: String,
    pub query: String,
    /// Branch the answer was produced against (`""` ⇒ main).
    pub branch: String,
    pub created_at: f64,
}

/// Deterministic cache key: BLAKE3 hex over (branch, full query text). The
/// verbatim query is included so two distinct questions can never collide onto
/// the same cached answer.
pub fn answer_cache_key(branch: Option<&str>, query: &str) -> String {
    let mut canon = String::new();
    canon.push_str("b\x1f");
    canon.push_str(branch.unwrap_or(""));
    canon.push_str("\x1eq\x1f");
    canon.push_str(query);
    blake3::hash(canon.as_bytes()).to_hex().to_string()
}

fn dv_str(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => other.to_string(),
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
    /// Read a cached answer, or `None` on a miss. TTL is the caller's concern
    /// (the row carries `created_at`).
    pub fn answer_cache_get(&self, key: &str) -> Result<Option<AnswerCacheRow>> {
        let mut params = BTreeMap::new();
        params.insert("k".into(), DataValue::Str(key.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[key, answer_json, query, branch, created_at] := \
                 *answer_cache{key, answer_json, query, branch, created_at}, key = $k",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("answer_cache_get: {e}")))?;
        Ok(rows.rows.first().map(|r| AnswerCacheRow {
            key: dv_str(&r[0]),
            answer_json: dv_str(&r[1]),
            query: dv_str(&r[2]),
            branch: dv_str(&r[3]),
            created_at: dv_f64(&r[4]),
        }))
    }

    /// Store an answer plus its provenance set (`(object_id, object_kind)` per
    /// grounding claim). Overwrites any prior row for the key.
    pub fn answer_cache_put(&self, row: &AnswerCacheRow, deps: &[(String, String)]) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("key".into(), DataValue::Str(row.key.clone().into()));
        params.insert("answer_json".into(), DataValue::Str(row.answer_json.clone().into()));
        params.insert("query".into(), DataValue::Str(row.query.clone().into()));
        params.insert("branch".into(), DataValue::Str(row.branch.clone().into()));
        params.insert("created_at".into(), DataValue::Num(Num::Float(row.created_at)));
        self.query(
            r#"?[key, answer_json, query, branch, created_at] <- [[
                $key, $answer_json, $query, $branch, $created_at
            ]]
            :put answer_cache {key => answer_json, query, branch, created_at}"#,
            params,
        )?;

        if !deps.is_empty() {
            let dep_rows: Vec<DataValue> = deps
                .iter()
                .map(|(oid, kind)| {
                    DataValue::List(vec![
                        DataValue::Str(row.key.clone().into()),
                        DataValue::Str(oid.clone().into()),
                        DataValue::Str(kind.clone().into()),
                    ])
                })
                .collect();
            let mut dep_params = BTreeMap::new();
            dep_params.insert("rows".into(), DataValue::List(dep_rows));
            self.query(
                r#"?[key, object_id, object_kind] <- $rows
                :put answer_cache_deps {key, object_id => object_kind}"#,
                dep_params,
            )?;
        }
        Ok(())
    }

    /// Causal invalidation: evict every cached answer whose provenance set
    /// intersects any of `object_ids`. Returns the number of answers evicted.
    pub fn invalidate_answers_for(&self, object_ids: &[String]) -> Result<usize> {
        if object_ids.is_empty() {
            return Ok(0);
        }
        let mut keys = std::collections::BTreeSet::new();
        for oid in object_ids {
            let mut params = BTreeMap::new();
            params.insert("oid".into(), DataValue::Str(oid.clone().into()));
            let rows = self
                .raw_db()
                .run_script(
                    "?[key] := *answer_cache_deps{object_id: $oid, key}",
                    params,
                    ScriptMutability::Immutable,
                )
                .map_err(|e| Error::GraphStorage(format!("invalidate_answers lookup: {e}")))?;
            for r in &rows.rows {
                keys.insert(dv_str(&r[0]));
            }
        }
        for key in &keys {
            let mut p1 = BTreeMap::new();
            p1.insert("k".into(), DataValue::Str(key.clone().into()));
            self.raw_db()
                .run_script(
                    "?[key] := *answer_cache{key}, key = $k\n:rm answer_cache {key}",
                    p1,
                    ScriptMutability::Mutable,
                )
                .map_err(|e| Error::GraphStorage(format!("invalidate_answers rm cache: {e}")))?;
            let mut p2 = BTreeMap::new();
            p2.insert("k".into(), DataValue::Str(key.clone().into()));
            self.raw_db()
                .run_script(
                    "?[key, object_id] := *answer_cache_deps{key, object_id}, key = $k\n:rm answer_cache_deps {key, object_id}",
                    p2,
                    ScriptMutability::Mutable,
                )
                .map_err(|e| Error::GraphStorage(format!("invalidate_answers rm deps: {e}")))?;
        }
        Ok(keys.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> GraphStore {
        let db = cozo::DbInstance::new("mem", "", "").unwrap();
        let s = GraphStore::from_db_for_testing(db);
        s.init_for_testing().unwrap();
        s
    }

    #[test]
    fn key_is_query_and_branch_specific() {
        assert_ne!(answer_cache_key(None, "q1"), answer_cache_key(None, "q2"));
        assert_ne!(
            answer_cache_key(Some("main"), "q"),
            answer_cache_key(Some("topic/x"), "q")
        );
        // Deterministic.
        assert_eq!(answer_cache_key(None, "same"), answer_cache_key(None, "same"));
    }

    #[test]
    fn put_get_roundtrips_and_invalidation_is_causal() {
        let g = mem();
        let row = AnswerCacheRow {
            key: answer_cache_key(None, "what db?"),
            answer_json: "{\"answer\":\"Postgres\"}".into(),
            query: "what db?".into(),
            branch: String::new(),
            created_at: 100.0,
        };
        g.answer_cache_put(&row, &[("c1".into(), "claim".into()), ("c2".into(), "claim".into())])
            .unwrap();

        // Hit.
        let got = g.answer_cache_get(&row.key).unwrap().expect("hit");
        assert_eq!(got.answer_json, row.answer_json);

        // A change to an UNRELATED claim does not evict.
        assert_eq!(g.invalidate_answers_for(&["other".into()]).unwrap(), 0);
        assert!(g.answer_cache_get(&row.key).unwrap().is_some());

        // A change to a dep claim evicts exactly this answer + its deps.
        assert_eq!(g.invalidate_answers_for(&["c2".into()]).unwrap(), 1);
        assert!(g.answer_cache_get(&row.key).unwrap().is_none());
        // Re-invalidating is a clean no-op.
        assert_eq!(g.invalidate_answers_for(&["c1".into()]).unwrap(), 0);
    }

    #[test]
    fn empty_object_ids_is_noop() {
        let g = mem();
        assert_eq!(g.invalidate_answers_for(&[]).unwrap(), 0);
    }
}
