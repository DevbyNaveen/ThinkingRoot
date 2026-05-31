//! Capsule cache — the storage half of `compile_capsule`.
//!
//! A *capsule* is the compiled, witness-grounded context the engine
//! feeds the LLM instead of dumping raw state. The composition lives in
//! `thinkingroot-serve` (it embeds serve-layer types like the workspace
//! brief); this crate stores the result as an opaque `capsule_json`
//! blob plus its **provenance set** — every claim/witness/entity the
//! compile read.
//!
//! Two relations back this (declared in `graph.rs::create_schema`):
//!   * `capsule_cache {key => capsule_json, …}` — the warm payload.
//!   * `capsule_deps {key, object_id => object_kind}` — one row per
//!     fact the capsule depends on.
//!
//! Correctness over hit-rate: [`capsule_key`] hashes the **full query
//! text** (not just its class), so a cache hit can never return another
//! query's grounding. `query_class` is stored only as metadata for
//! analytics + the M4 prefetch predictor. Invalidation is *causal*:
//! [`GraphStore::invalidate_capsules_for`] evicts exactly the capsules
//! whose provenance set intersects a changed fact — the witness mesh as
//! a cache-dependency DAG.

use std::collections::BTreeMap;

use cozo::{DataValue, Num, ScriptMutability};
use serde::{Deserialize, Serialize};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// One cached capsule row. `capsule_json` is opaque to this crate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapsuleCacheRow {
    /// BLAKE3 hex of (prompt@version, branch, vars, query) — see [`capsule_key`].
    pub key: String,
    /// The serialized `CompiledCapsule` (serve-layer struct).
    pub capsule_json: String,
    pub prompt_name: String,
    pub prompt_version: i64,
    /// Branch the capsule was compiled against (`""` ⇒ main).
    pub branch: String,
    /// Coarse query class — metadata only (analytics + prefetch), never the key.
    pub query_class: String,
    /// Rough token count of the compiled payload, for "fraction of raw context" proofs.
    pub token_estimate: i64,
    pub created_at: f64,
}

/// Deterministic cache key: BLAKE3 hex over the *full* compile inputs.
///
/// Includes the verbatim `query` so two different questions can never
/// collide onto the same cached grounding. `vars` are canonicalised via
/// the `BTreeMap` ordering so key bytes are stable across call sites.
pub fn capsule_key(
    prompt_name: &str,
    prompt_version: i64,
    branch: Option<&str>,
    query: &str,
    vars: &BTreeMap<String, String>,
) -> String {
    let mut canon = String::new();
    canon.push_str("p\x1f");
    canon.push_str(prompt_name);
    canon.push('@');
    canon.push_str(&prompt_version.to_string());
    canon.push_str("\x1eb\x1f");
    canon.push_str(branch.unwrap_or(""));
    canon.push_str("\x1eq\x1f");
    canon.push_str(query);
    canon.push_str("\x1ev\x1f");
    for (k, v) in vars {
        canon.push_str(k);
        canon.push('=');
        canon.push_str(v);
        canon.push('\x1d');
    }
    blake3::hash(canon.as_bytes()).to_hex().to_string()
}

/// Coarse, deterministic class of a natural-language query — metadata
/// for analytics and the M4 prefetch predictor. NOT part of the cache
/// key (that would risk returning the wrong grounding). Normalises
/// case + whitespace and keys on the leading interrogative + length
/// bucket, which is stable enough to predict "the next question looks
/// like this one" without conflating distinct questions.
pub fn classify_query(query: &str) -> String {
    let norm = query.trim().to_ascii_lowercase();
    let first = norm.split_whitespace().next().unwrap_or("");
    let lead = match first {
        "who" | "what" | "when" | "where" | "why" | "how" | "which" | "whose" => first,
        "is" | "are" | "do" | "does" | "did" | "can" | "could" | "should" | "will" => "yesno",
        _ => "stmt",
    };
    let bucket = match norm.len() {
        0..=40 => "s",
        41..=160 => "m",
        _ => "l",
    };
    format!("{lead}:{bucket}")
}

fn dv_str(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => other.to_string(),
    }
}

fn dv_i64(v: &DataValue) -> i64 {
    match v {
        DataValue::Num(Num::Int(i)) => *i,
        DataValue::Num(Num::Float(f)) => *f as i64,
        _ => 0,
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
    /// Read a cached capsule, or `None` on a miss.
    pub fn capsule_cache_get(&self, key: &str) -> Result<Option<CapsuleCacheRow>> {
        let mut params = BTreeMap::new();
        params.insert("k".into(), DataValue::Str(key.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[key, capsule_json, prompt_name, prompt_version, branch, query_class, token_estimate, created_at] := \
                 *capsule_cache{key, capsule_json, prompt_name, prompt_version, branch, query_class, token_estimate, created_at}, \
                 key = $k",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("capsule_cache_get: {e}")))?;
        Ok(rows.rows.first().map(|r| CapsuleCacheRow {
            key: dv_str(&r[0]),
            capsule_json: dv_str(&r[1]),
            prompt_name: dv_str(&r[2]),
            prompt_version: dv_i64(&r[3]),
            branch: dv_str(&r[4]),
            query_class: dv_str(&r[5]),
            token_estimate: dv_i64(&r[6]),
            created_at: dv_f64(&r[7]),
        }))
    }

    /// Store a compiled capsule plus its provenance set. `deps` is the
    /// list of `(object_id, object_kind)` the compile read; each becomes
    /// one `capsule_deps` row so a later fact change can evict this key.
    pub fn capsule_cache_put(&self, row: &CapsuleCacheRow, deps: &[(String, String)]) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("key".into(), DataValue::Str(row.key.clone().into()));
        params.insert("capsule_json".into(), DataValue::Str(row.capsule_json.clone().into()));
        params.insert("prompt_name".into(), DataValue::Str(row.prompt_name.clone().into()));
        params.insert("prompt_version".into(), DataValue::Num(Num::Int(row.prompt_version)));
        params.insert("branch".into(), DataValue::Str(row.branch.clone().into()));
        params.insert("query_class".into(), DataValue::Str(row.query_class.clone().into()));
        params.insert("token_estimate".into(), DataValue::Num(Num::Int(row.token_estimate)));
        params.insert("created_at".into(), DataValue::Num(Num::Float(row.created_at)));
        self.query(
            r#"?[key, capsule_json, prompt_name, prompt_version, branch, query_class, token_estimate, created_at] <- [[
                $key, $capsule_json, $prompt_name, $prompt_version, $branch, $query_class, $token_estimate, $created_at
            ]]
            :put capsule_cache {key => capsule_json, prompt_name, prompt_version, branch, query_class, token_estimate, created_at}"#,
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
                :put capsule_deps {key, object_id => object_kind}"#,
                dep_params,
            )?;
        }
        Ok(())
    }

    /// Causal invalidation: evict every cached capsule whose provenance
    /// set intersects any of `object_ids` (a contribute's accepted claim
    /// ids, a rewritten witness, etc.). Returns the number of capsules
    /// evicted. The witness mesh is the dependency DAG that makes this
    /// precise — only capsules that actually read a changed fact die.
    pub fn invalidate_capsules_for(&self, object_ids: &[String]) -> Result<usize> {
        if object_ids.is_empty() {
            return Ok(0);
        }
        // 1. Collect the distinct cache keys whose deps name any changed object.
        let mut keys = std::collections::BTreeSet::new();
        for oid in object_ids {
            let mut params = BTreeMap::new();
            params.insert("oid".into(), DataValue::Str(oid.clone().into()));
            let rows = self
                .raw_db()
                .run_script(
                    "?[key] := *capsule_deps{object_id: $oid, key}",
                    params,
                    ScriptMutability::Immutable,
                )
                .map_err(|e| Error::GraphStorage(format!("invalidate_capsules lookup: {e}")))?;
            for r in &rows.rows {
                keys.insert(dv_str(&r[0]));
            }
        }
        // 2. Delete each cache row and all of its dep rows.
        for key in &keys {
            let mut p1 = BTreeMap::new();
            p1.insert("k".into(), DataValue::Str(key.clone().into()));
            self.raw_db()
                .run_script(
                    "?[key] := *capsule_cache{key}, key = $k\n:rm capsule_cache {key}",
                    p1,
                    ScriptMutability::Mutable,
                )
                .map_err(|e| Error::GraphStorage(format!("invalidate_capsules rm cache: {e}")))?;
            let mut p2 = BTreeMap::new();
            p2.insert("k".into(), DataValue::Str(key.clone().into()));
            self.raw_db()
                .run_script(
                    "?[key, object_id] := *capsule_deps{key, object_id}, key = $k\n:rm capsule_deps {key, object_id}",
                    p2,
                    ScriptMutability::Mutable,
                )
                .map_err(|e| Error::GraphStorage(format!("invalidate_capsules rm deps: {e}")))?;
        }
        Ok(keys.len())
    }

    #[doc(hidden)]
    /// Test-only count of cached capsules (used by the cache unit tests).
    pub fn capsule_cache_len(&self) -> Result<usize> {
        let rows = self
            .raw_db()
            .run_script(
                "?[key] := *capsule_cache{key}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("capsule_cache_len: {e}")))?;
        Ok(rows.rows.len())
    }

    /// Evict every capsule compiled against a branch — used on session
    /// teardown so a merged/abandoned branch leaves no stale capsules.
    pub fn invalidate_capsules_on_branch(&self, branch: &str) -> Result<usize> {
        let mut params = BTreeMap::new();
        params.insert("b".into(), DataValue::Str(branch.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[key] := *capsule_cache{key, branch: $b}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("invalidate_capsules_on_branch lookup: {e}")))?;
        let keys: Vec<String> = rows.rows.iter().map(|r| dv_str(&r[0])).collect();
        for key in &keys {
            let mut p1 = BTreeMap::new();
            p1.insert("k".into(), DataValue::Str(key.clone().into()));
            self.raw_db()
                .run_script(
                    "?[key] := *capsule_cache{key}, key = $k\n:rm capsule_cache {key}",
                    p1,
                    ScriptMutability::Mutable,
                )
                .map_err(|e| Error::GraphStorage(format!("invalidate_capsules_on_branch rm cache: {e}")))?;
            let mut p2 = BTreeMap::new();
            p2.insert("k".into(), DataValue::Str(key.clone().into()));
            self.raw_db()
                .run_script(
                    "?[key, object_id] := *capsule_deps{key, object_id}, key = $k\n:rm capsule_deps {key, object_id}",
                    p2,
                    ScriptMutability::Mutable,
                )
                .map_err(|e| Error::GraphStorage(format!("invalidate_capsules_on_branch rm deps: {e}")))?;
        }
        Ok(keys.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> GraphStore {
        let db = cozo::DbInstance::new("mem", "", "").unwrap();
        let s = GraphStore::from_db_for_testing(db);
        s.init_for_testing().unwrap();
        s
    }

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn row(key: &str, branch: &str) -> CapsuleCacheRow {
        CapsuleCacheRow {
            key: key.to_string(),
            capsule_json: format!("{{\"k\":\"{key}\"}}"),
            prompt_name: "sys".into(),
            prompt_version: 1,
            branch: branch.to_string(),
            query_class: "what:s".into(),
            token_estimate: 42,
            created_at: 1.0,
        }
    }

    #[test]
    fn capsule_key_is_deterministic_and_query_sensitive() {
        let v = vars(&[("a", "1"), ("b", "2")]);
        let k1 = capsule_key("sys", 3, Some("main"), "who is naveen", &v);
        let k2 = capsule_key("sys", 3, Some("main"), "who is naveen", &v);
        assert_eq!(k1, k2, "same inputs must hash identically");
        // A different query MUST yield a different key — otherwise a cache
        // hit could return another question's grounding.
        let k3 = capsule_key("sys", 3, Some("main"), "where is naveen", &v);
        assert_ne!(k1, k3);
        // Branch + version + vars all participate.
        assert_ne!(k1, capsule_key("sys", 3, Some("dev"), "who is naveen", &v));
        assert_ne!(k1, capsule_key("sys", 4, Some("main"), "who is naveen", &v));
        assert_ne!(k1, capsule_key("sys", 3, Some("main"), "who is naveen", &vars(&[("a", "9")])));
    }

    #[test]
    fn classify_query_buckets_lead_and_length() {
        assert_eq!(classify_query("Who is X?"), "who:s");
        assert_eq!(classify_query("Is it raining today in the city?"), "yesno:s");
        // Short statement (≤40 chars) → "s"; long one (41..=160) → "m".
        assert_eq!(classify_query("Explain X"), "stmt:s");
        assert_eq!(
            classify_query(
                "Summarize the complete architecture and the rationale behind every subsystem"
            ),
            "stmt:m"
        );
    }

    #[test]
    fn cache_put_get_round_trips() {
        let s = store();
        let r = row("k1", "");
        s.capsule_cache_put(&r, &[("claim-a".into(), "claim".into())]).unwrap();
        let got = s.capsule_cache_get("k1").unwrap().expect("cache hit");
        assert_eq!(got.key, "k1");
        assert_eq!(got.capsule_json, r.capsule_json);
        assert_eq!(got.token_estimate, 42);
        assert!(s.capsule_cache_get("missing").unwrap().is_none());
    }

    #[test]
    fn invalidate_for_object_evicts_only_dependents() {
        let s = store();
        // k1 depends on claim-a; k2 depends on claim-b.
        s.capsule_cache_put(&row("k1", ""), &[("claim-a".into(), "claim".into())]).unwrap();
        s.capsule_cache_put(&row("k2", ""), &[("claim-b".into(), "claim".into())]).unwrap();
        assert_eq!(s.capsule_cache_len().unwrap(), 2);

        let evicted = s.invalidate_capsules_for(&["claim-a".to_string()]).unwrap();
        assert_eq!(evicted, 1, "only the capsule depending on claim-a dies");
        assert!(s.capsule_cache_get("k1").unwrap().is_none());
        assert!(s.capsule_cache_get("k2").unwrap().is_some());

        // Invalidating an unknown object evicts nothing.
        assert_eq!(s.invalidate_capsules_for(&["claim-z".to_string()]).unwrap(), 0);
    }

    #[test]
    fn invalidate_on_branch_scopes_correctly() {
        let s = store();
        s.capsule_cache_put(&row("m1", ""), &[("c1".into(), "claim".into())]).unwrap();
        s.capsule_cache_put(&row("b1", "stream/x"), &[("c2".into(), "claim".into())]).unwrap();
        s.capsule_cache_put(&row("b2", "stream/x"), &[("c3".into(), "claim".into())]).unwrap();

        let evicted = s.invalidate_capsules_on_branch("stream/x").unwrap();
        assert_eq!(evicted, 2);
        assert!(s.capsule_cache_get("m1").unwrap().is_some(), "main capsule survives");
        assert!(s.capsule_cache_get("b1").unwrap().is_none());
        assert!(s.capsule_cache_get("b2").unwrap().is_none());

        // The deps of evicted capsules are gone too (no orphan rows).
        assert_eq!(s.invalidate_capsules_for(&["c2".to_string()]).unwrap(), 0);
    }
}
