//! ARTMIP — "Recall is Rewrite" (L3, **the MIND**): the plasticity substrate +
//! the first laws (Hebbian potentiation + lazy Ebbinghaus decay).
//!
//! Spec: `docs/superpowers/specs/2026-06-27-thinkingroot-protocol-recall-is-rewrite.md`
//! (cloud repo). This is the part where recall stops being read-only and becomes
//! a *reconsolidating write*: the synapses (`spine_edge`) between co-recalled
//! memories strengthen (Hebb 1949), and unused ones fade (Ebbinghaus 1885).
//!
//! **Honesty / safety (load-bearing):**
//! - Every consumer is **flag-default-OFF and eval-gated** — the spec has a
//!   *falsifier* (if the recall-is-rewrite curve is flat, the claim is wrong), so
//!   these laws ship dark until measured. This module is the *mechanism*; it does
//!   not prove the mechanism helps.
//! - **Nothing is destroyed.** Plasticity lives in the `spine_edge` SIDECAR — drop
//!   the relation and the brain is the static index again. Decay is lazy (a read
//!   function of `t − last_access`); the actual pruning happens in `dream`, and it
//!   only fades the *active synapse*, never the provenance/bi-temporal rows.
//! - **Bounded against runaway** ([`H_MAX`] per-edge cap — a cheap homeostatic
//!   stand-in for §4.4's full scaling), so adversarial repeated queries can't
//!   blow weights up.
//!
//! The pure math (potentiate / decay / retrievability / stability) is unit-tested;
//! the graph I/O wraps it.

use std::collections::{BTreeMap, HashMap, HashSet};

use cozo::{DataValue, Num};

use crate::graph::GraphStore;
use crate::Result;

/// Per-edge ceiling on the plastic Hebbian component `h` — a cheap anti-runaway
/// bound (the simpler stand-in for §4.4 homeostatic scaling). Keeps synapses
/// bounded under adversarial repeated recall.
pub const H_MAX: f64 = 4.0;

/// Hebbian potentiation (Hebb 1949 + Turrigiano bound): a co-activated pair
/// strengthens by `eta · a_i · a_j`, clamped to `[0, H_MAX]`. Pure.
pub fn potentiate(h: f64, a_i: f64, a_j: f64, eta: f64) -> f64 {
    (h + eta * a_i.max(0.0) * a_j.max(0.0)).clamp(0.0, H_MAX)
}

/// Lazy Ebbinghaus decay of the plastic component: `h · exp(−λ·dt)` where `dt` is
/// seconds since last access. Pure; evaluated at read (no background sweep).
pub fn decayed_h(h: f64, dt_secs: f64, lambda: f64) -> f64 {
    if dt_secs <= 0.0 || lambda <= 0.0 {
        return h.max(0.0);
    }
    (h * (-lambda * dt_secs).exp()).max(0.0)
}

/// Node retrievability (spec §4.4b): `exp(−(t − t_i)/S)`, gated by stability `S`
/// (higher stability = slower forgetting). Pure.
pub fn retrievability(dt_secs: f64, stability: f64) -> f64 {
    let s = stability.max(1e-6);
    if dt_secs <= 0.0 {
        return 1.0;
    }
    (-dt_secs / s).exp()
}

/// Stability bump on successful recall (spacing effect, §4.4): `S · (1 + κ·a)`,
/// floored at 1.0 (used items become *harder* to forget). Pure.
pub fn bump_stability(stability: f64, a: f64, kappa: f64) -> f64 {
    (stability * (1.0 + kappa * a.max(0.0))).max(1.0)
}

impl GraphStore {
    /// Read one plastic edge's `(w_struct, h, last_access, stability)`, or `None`.
    fn read_spine_edge(&self, from: &str, to: &str) -> Result<Option<(f64, f64, f64, f64)>> {
        let mut params = BTreeMap::new();
        params.insert("f".into(), DataValue::Str(from.into()));
        params.insert("t".into(), DataValue::Str(to.into()));
        let res = self.query(
            "?[w_struct, h, last_access, stability] := \
             *spine_edge{from_id: $f, to_id: $t, w_struct, h, last_access, stability}",
            params,
        )?;
        Ok(res.rows.first().map(|r| {
            let g = |i: usize| match r.get(i) {
                Some(DataValue::Num(Num::Float(x))) => *x,
                Some(DataValue::Num(Num::Int(x))) => *x as f64,
                _ => 0.0,
            };
            (g(0), g(1), g(2), g(3))
        }))
    }

    fn put_spine_edge(
        &self,
        from: &str,
        to: &str,
        w_struct: f64,
        h: f64,
        last_access: f64,
        stability: f64,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("f".into(), DataValue::Str(from.into()));
        params.insert("t".into(), DataValue::Str(to.into()));
        params.insert("w".into(), DataValue::Num(Num::Float(w_struct)));
        params.insert("h".into(), DataValue::Num(Num::Float(h)));
        params.insert("la".into(), DataValue::Num(Num::Float(last_access)));
        params.insert("s".into(), DataValue::Num(Num::Float(stability)));
        self.query(
            "?[from_id, to_id, w_struct, h, last_access, stability] <- [[$f, $t, $w, $h, $la, $s]] \
             :put spine_edge {from_id, to_id => w_struct, h, last_access, stability}",
            params,
        )?;
        Ok(())
    }

    /// Set the **structural** prior `w_struct` for an edge WITHOUT clobbering its
    /// learned plastic state (`h`/`stability` preserved if the edge exists). Called
    /// at compile from existing entity/chunk/co-citation edges. Idempotent.
    pub fn upsert_spine_structural(&self, edges: &[(String, String, f64)], now: f64) -> Result<usize> {
        let mut n = 0;
        for (from, to, w) in edges {
            let (h, la, s) = match self.read_spine_edge(from, to)? {
                Some((_w, h, la, s)) => (h, la, s), // preserve learned plasticity
                None => (0.0, now, 1.0),
            };
            self.put_spine_edge(from, to, *w, h, la, s)?;
            n += 1;
        }
        Ok(n)
    }

    /// **The Hebbian write (the rewrite, §4.4).** For each co-recalled pair
    /// `(from, to, activation_product)`, strengthen the synapse by
    /// `eta · product` (bounded by [`H_MAX`]) and stamp `last_access = now`. The
    /// edge is created (w_struct=0) if it doesn't exist. Read-modify-write per
    /// pair so `h` accumulates correctly; caller bounds the pair count (cited
    /// subset of a recall). Returns the number of synapses written.
    pub fn bump_hebbian(&self, pairs: &[(String, String, f64)], eta: f64, now: f64) -> Result<usize> {
        let mut n = 0;
        for (from, to, product) in pairs {
            if from == to || from.is_empty() || to.is_empty() {
                continue;
            }
            let (w_struct, h, _la, s) = self.read_spine_edge(from, to)?.unwrap_or((0.0, 0.0, now, 1.0));
            // `product` already carries a_i·a_j; eta scales it.
            let h_new = potentiate(h, *product, 1.0, eta);
            self.put_spine_edge(from, to, w_struct, h_new, now, s)?;
            n += 1;
        }
        Ok(n)
    }

    /// The effective plastic weight of an edge at time `now`:
    /// `w_struct + decayed_h(h, now − last_access, lambda)`. Returns `0.0` for a
    /// non-existent edge. This is what spreading activation reads (§4.2).
    pub fn plastic_weight(&self, from: &str, to: &str, now: f64, lambda: f64) -> Result<f64> {
        match self.read_spine_edge(from, to)? {
            Some((w_struct, h, last_access, _s)) => {
                Ok(w_struct + decayed_h(h, (now - last_access).max(0.0), lambda))
            }
            None => Ok(0.0),
        }
    }

    /// All `spine_edge`s touching `node` (either direction) as
    /// `(neighbor_id, w_struct, h, last_access)` — the raw plastic adjacency.
    fn spine_edges_touching(&self, node: &str) -> Result<Vec<(String, f64, f64, f64)>> {
        let getf = |r: &[DataValue], i: usize| match r.get(i) {
            Some(DataValue::Num(Num::Float(x))) => *x,
            Some(DataValue::Num(Num::Int(x))) => *x as f64,
            _ => 0.0,
        };
        let gets = |r: &[DataValue], i: usize| -> Option<String> {
            r.get(i).and_then(|v| v.get_str()).map(|s| s.to_string())
        };
        let mut out = Vec::new();
        let mut p = BTreeMap::new();
        p.insert("n".to_string(), DataValue::Str(node.into()));
        let fwd = self.query(
            "?[to_id, w_struct, h, last_access] := \
             *spine_edge{from_id: $n, to_id, w_struct, h, last_access}",
            p.clone(),
        )?;
        for r in &fwd.rows {
            if let Some(nbr) = gets(r, 0) {
                out.push((nbr, getf(r, 1), getf(r, 2), getf(r, 3)));
            }
        }
        let bwd = self.query(
            "?[from_id, w_struct, h, last_access] := \
             *spine_edge{to_id: $n, from_id, w_struct, h, last_access}",
            p,
        )?;
        for r in &bwd.rows {
            if let Some(nbr) = gets(r, 0) {
                out.push((nbr, getf(r, 1), getf(r, 2), getf(r, 3)));
            }
        }
        Ok(out)
    }

    /// **Dream-time pruning (§4.7) — honest forgetting.** Remove plastic synapses
    /// whose *effective* weight (`w_struct + decayed_h`) has faded below `epsilon`.
    /// Only the active SYNAPSE is dropped — the facts, provenance, and bi-temporal
    /// rows are untouched, so a forgotten association is recoverable. Structural
    /// edges (`w_struct ≥ epsilon`) never prune. Idempotent. Returns the count
    /// pruned. Called from `dream`/maintenance; cheap (one scan of the sidecar).
    pub fn prune_spine_edges(&self, now: f64, lambda: f64, epsilon: f64) -> Result<usize> {
        let getf = |r: &[DataValue], i: usize| match r.get(i) {
            Some(DataValue::Num(Num::Float(x))) => *x,
            Some(DataValue::Num(Num::Int(x))) => *x as f64,
            _ => 0.0,
        };
        let gets = |r: &[DataValue], i: usize| -> Option<String> {
            r.get(i).and_then(|v| v.get_str()).map(|s| s.to_string())
        };
        let res = self.query(
            "?[from_id, to_id, w_struct, h, last_access] := \
             *spine_edge{from_id, to_id, w_struct, h, last_access}",
            BTreeMap::new(),
        )?;
        let mut doomed: Vec<(String, String)> = Vec::new();
        for r in &res.rows {
            let (Some(from), Some(to)) = (gets(r, 0), gets(r, 1)) else {
                continue;
            };
            let w = getf(r, 2) + decayed_h(getf(r, 3), (now - getf(r, 4)).max(0.0), lambda);
            if w < epsilon {
                doomed.push((from, to));
            }
        }
        for (from, to) in &doomed {
            let mut p = BTreeMap::new();
            p.insert("f".to_string(), DataValue::Str(from.clone().into()));
            p.insert("t".to_string(), DataValue::Str(to.clone().into()));
            self.query(
                "?[from_id, to_id] <- [[$f, $t]] :rm spine_edge {from_id, to_id}",
                p,
            )?;
        }
        Ok(doomed.len())
    }

    /// **The recall-is-rewrite READ (§4.3).** Plastic neighbors of `seeds` via
    /// `spine_edge` (both directions), each with its decayed effective weight
    /// `w_struct + decayed_h`. Entities you've **co-recalled before** (strong `h`)
    /// surface even when no *structural* relation connects them — the Hebbian
    /// writes finally affect retrieval, closing the loop. Above `min_w`, seeds
    /// excluded, best weight per neighbor.
    pub fn plastic_neighbors(
        &self,
        seeds: &[String],
        now: f64,
        lambda: f64,
        min_w: f64,
    ) -> Result<Vec<(String, f64)>> {
        let seed_set: HashSet<&str> = seeds.iter().map(|s| s.as_str()).collect();
        let mut out: HashMap<String, f64> = HashMap::new();
        for s in seeds {
            for (nbr, w_struct, h, la) in self.spine_edges_touching(s)? {
                if seed_set.contains(nbr.as_str()) {
                    continue;
                }
                let w = w_struct + decayed_h(h, (now - la).max(0.0), lambda);
                if w >= min_w {
                    let e = out.entry(nbr).or_insert(0.0);
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

    #[test]
    fn potentiation_accumulates_and_is_bounded() {
        // Repeated co-activation strengthens, then saturates at H_MAX.
        let mut h = 0.0;
        for _ in 0..1000 {
            h = potentiate(h, 1.0, 1.0, 0.1);
        }
        assert!((h - H_MAX).abs() < 1e-9, "h must saturate at H_MAX, got {h}");
        // Zero activation never strengthens.
        assert_eq!(potentiate(1.0, 0.0, 1.0, 0.5), 1.0);
    }

    #[test]
    fn decay_fades_unused_edges() {
        let h0 = 2.0;
        // No time passed → unchanged.
        assert_eq!(decayed_h(h0, 0.0, 0.01), h0);
        // After time, strictly smaller and positive.
        let later = decayed_h(h0, 1000.0, 0.001);
        assert!(later < h0 && later > 0.0);
        // Far future → approaches 0.
        assert!(decayed_h(h0, 1_000_000.0, 0.001) < 1e-3);
    }

    #[test]
    fn retrievability_and_stability() {
        // Fresh → fully retrievable.
        assert!((retrievability(0.0, 10.0) - 1.0).abs() < 1e-9);
        // Higher stability ⇒ more retrievable at the same age.
        assert!(retrievability(100.0, 100.0) > retrievability(100.0, 10.0));
        // Recall makes a node harder to forget (stability grows, floored at 1).
        assert!(bump_stability(1.0, 1.0, 0.5) > 1.0);
        assert_eq!(bump_stability(1.0, 0.0, 0.5), 1.0);
    }

    #[test]
    fn recall_is_rewrite_loop_end_to_end() {
        use cozo::DbInstance;
        let db = DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().unwrap();

        let now = 100.0;
        // A recall co-activates A and B → the synapse is WRITTEN (recall is rewrite).
        store.bump_hebbian(&[("A".into(), "B".into(), 1.0)], 0.5, now).unwrap();
        let w1 = store.plastic_weight("A", "B", now, 0.001).unwrap();
        assert!(w1 > 0.0, "recall strengthened the synapse");

        // A second co-recall strengthens it further (accumulation / use sharpens).
        store.bump_hebbian(&[("A".into(), "B".into(), 1.0)], 0.5, now).unwrap();
        let w2 = store.plastic_weight("A", "B", now, 0.001).unwrap();
        assert!(w2 > w1, "repeated recall sharpens the path");

        // Left unused for a long time → the synapse fades (Ebbinghaus, lazy at read).
        let w_later = store.plastic_weight("A", "B", now + 1_000_000.0, 0.001).unwrap();
        assert!(w_later < w2, "unused synapse decays");

        // A never-recalled pair carries no plastic weight.
        assert_eq!(store.plastic_weight("X", "Y", now, 0.001).unwrap(), 0.0);

        // Structural prior is preserved across Hebbian writes (compile sets it,
        // recall must not clobber it).
        store.upsert_spine_structural(&[("A".into(), "B".into(), 0.3)], now).unwrap();
        store.bump_hebbian(&[("A".into(), "B".into(), 1.0)], 0.5, now).unwrap();
        let w3 = store.plastic_weight("A", "B", now, 0.001).unwrap();
        assert!(w3 >= 0.3, "structural prior survives the Hebbian write");
    }

    #[test]
    fn prune_drops_faded_plastic_keeps_structural() {
        use cozo::DbInstance;
        let db = DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().unwrap();
        let now = 100.0;
        // A weak purely-plastic synapse (will decay below epsilon)…
        store.bump_hebbian(&[("A".into(), "B".into(), 0.01)], 0.1, now).unwrap();
        // …and a strong STRUCTURAL edge (compile-set prior).
        store.upsert_spine_structural(&[("C".into(), "D".into(), 0.5)], now).unwrap();

        // Far in the future the plastic synapse has faded → pruned; structural stays.
        let pruned = store.prune_spine_edges(now + 10_000_000.0, 0.001, 0.05).unwrap();
        assert_eq!(pruned, 1, "the faded plastic synapse is pruned");
        assert_eq!(
            store.plastic_weight("A", "B", now, 0.001).unwrap(),
            0.0,
            "forgotten synapse is gone from the hot index"
        );
        assert!(
            store.plastic_weight("C", "D", now, 0.001).unwrap() >= 0.5,
            "structural edge survives forgetting"
        );
    }
}
