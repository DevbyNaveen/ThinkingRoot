//! ARTMIP §4.10 — **Episodic Trajectory Chaining** (the MIND): recall reasoning
//! PATHS, not just facts.
//!
//! As nodes are activated in sequence, directed `temporal_flow` edges accumulate
//! "what tends to follow what". A later recall is then PRIMED toward the nodes
//! that historically followed its seeds — the spotlight of attention flows along
//! the paths that resolved similar tasks before (preventing loops, enabling
//! path-based replay; Teyler & Rudy 2007).
//!
//! **Bounded version (this slice):** each recall contributes its relevance-ordered
//! activation sequence — consecutive `(a→b)` pairs bump `temporal_flow`. The
//! richer cross-session form (group by `session_id`, weight by the real Δt between
//! thoughts, `w = exp(−Δt)`) is the future upgrade; the substrate (`temporal_flow`)
//! is the same. Flag-off, eval-gated, reversible (sidecar). Pure-ish graph I/O.

use std::collections::{BTreeMap, HashMap};

use cozo::{DataValue, Num};

use crate::graph::GraphStore;
use crate::Result;

impl GraphStore {
    fn read_temporal_flow(&self, from: &str, to: &str) -> Result<Option<(f64, i64)>> {
        let mut p = BTreeMap::new();
        p.insert("a".to_string(), DataValue::Str(from.into()));
        p.insert("b".to_string(), DataValue::Str(to.into()));
        let res = self.query(
            "?[weight, count] := *temporal_flow{from_id: $a, to_id: $b, weight, count}",
            p,
        )?;
        Ok(res.rows.first().map(|r| {
            let w = match r.first() {
                Some(DataValue::Num(Num::Float(x))) => *x,
                Some(DataValue::Num(Num::Int(x))) => *x as f64,
                _ => 0.0,
            };
            let c = match r.get(1) {
                Some(DataValue::Num(Num::Int(x))) => *x,
                Some(DataValue::Num(Num::Float(x))) => *x as i64,
                _ => 0,
            };
            (w, c)
        }))
    }

    /// Record a trajectory: for each consecutive `(a→b)` in `seq`, accumulate the
    /// `temporal_flow` edge (`weight += incr`, `count += 1`, `last_at = now`).
    /// Self-loops/empties skipped. Read-modify-write per pair (caller bounds the
    /// sequence length). Returns edges written.
    pub fn record_trajectory(&self, seq: &[String], incr: f64, now: f64) -> Result<usize> {
        let mut n = 0;
        for w in seq.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if a == b || a.is_empty() || b.is_empty() {
                continue;
            }
            let (weight, count) = self.read_temporal_flow(a, b)?.unwrap_or((0.0, 0));
            let mut p = BTreeMap::new();
            p.insert("a".to_string(), DataValue::Str(a.clone().into()));
            p.insert("b".to_string(), DataValue::Str(b.clone().into()));
            p.insert("w".to_string(), DataValue::Num(Num::Float(weight + incr)));
            p.insert("c".to_string(), DataValue::Num(Num::Int(count + 1)));
            p.insert("la".to_string(), DataValue::Num(Num::Float(now)));
            self.query(
                "?[from_id, to_id, weight, count, last_at] <- [[$a, $b, $w, $c, $la]] \
                 :put temporal_flow {from_id, to_id => weight, count, last_at}",
                p,
            )?;
            n += 1;
        }
        Ok(n)
    }

    /// Nodes that historically FOLLOWED any of `from_nodes` (via `temporal_flow`),
    /// with their accumulated weight, above `min_w` — the priming signal for
    /// trajectory-biased spreading activation. Best weight per follower.
    pub fn trajectory_followers(&self, from_nodes: &[String], min_w: f64) -> Result<Vec<(String, f64)>> {
        let mut out: HashMap<String, f64> = HashMap::new();
        for node in from_nodes {
            let mut p = BTreeMap::new();
            p.insert("n".to_string(), DataValue::Str(node.clone().into()));
            let res = self.query(
                "?[to_id, weight] := *temporal_flow{from_id: $n, to_id, weight}",
                p,
            )?;
            for r in &res.rows {
                let Some(to) = r.first().and_then(|v| v.get_str()).map(|s| s.to_string()) else {
                    continue;
                };
                let w = match r.get(1) {
                    Some(DataValue::Num(Num::Float(x))) => *x,
                    Some(DataValue::Num(Num::Int(x))) => *x as f64,
                    _ => 0.0,
                };
                if w >= min_w {
                    let e = out.entry(to).or_insert(0.0);
                    *e = e.max(w);
                }
            }
        }
        Ok(out.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cozo::DbInstance;

    fn store() -> GraphStore {
        let db = DbInstance::new("mem", "", "").unwrap();
        let s = GraphStore::from_db_for_testing(db);
        s.init_for_testing().unwrap();
        s
    }

    #[test]
    fn trajectory_records_and_primes_followers() {
        let s = store();
        let now = 100.0;
        // Two recalls walked the path A → B → C.
        s.record_trajectory(&["A".into(), "B".into(), "C".into()], 1.0, now).unwrap();
        s.record_trajectory(&["A".into(), "B".into(), "C".into()], 1.0, now).unwrap();

        // B historically follows A → primed when A is a seed.
        let after_a = s.trajectory_followers(&["A".into()], 0.5).unwrap();
        assert!(after_a.iter().any(|(n, w)| n == "B" && *w >= 2.0), "B follows A, weight accumulated");

        // C follows B; A follows nothing recorded as a follower of C.
        let after_b = s.trajectory_followers(&["B".into()], 0.5).unwrap();
        assert!(after_b.iter().any(|(n, _)| n == "C"));
        assert!(s.trajectory_followers(&["C".into()], 0.5).unwrap().is_empty(), "nothing follows C yet");
    }

    #[test]
    fn trajectory_skips_self_and_empty() {
        let s = store();
        // A→A self-loop and an empty id are skipped; only A→B records.
        let n = s.record_trajectory(&["A".into(), "A".into(), "".into(), "B".into()], 1.0, 1.0).unwrap();
        // pairs: (A,A) skip, (A,"") skip, ("",B) skip → 0 recorded.
        assert_eq!(n, 0);
    }
}
