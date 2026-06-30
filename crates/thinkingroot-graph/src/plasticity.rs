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

    /// Build/refresh the **structural** spine priors (`w_struct`, §4.2) from
    /// `entity_relations` — the shared-entity / co-citation structure set at
    /// compile. Aggregated by MAX strength per `(from,to)` pair, capped. Preserves
    /// learned plastic `h` (delegates to `upsert_spine_structural`), so a fresh
    /// brain carries structural priors *and* keeps everything it has learned.
    /// Called from `dream` (sleep consolidation). Returns edges written.
    pub fn populate_spine_from_relations(&self, now: f64, cap: usize) -> Result<usize> {
        let res = self.query_read(
            "?[from_id, to_id, strength] := *entity_relations{from_id, to_id, strength}",
        )?;
        let mut best: HashMap<(String, String), f64> = HashMap::new();
        for r in &res.rows {
            let (Some(from), Some(to)) = (
                r.first().and_then(|v| v.get_str()).map(|s| s.to_string()),
                r.get(1).and_then(|v| v.get_str()).map(|s| s.to_string()),
            ) else {
                continue;
            };
            if from == to {
                continue;
            }
            let s = match r.get(2) {
                Some(DataValue::Num(Num::Float(x))) => *x,
                Some(DataValue::Num(Num::Int(x))) => *x as f64,
                _ => 1.0,
            };
            let e = best.entry((from, to)).or_insert(0.0);
            if s > *e {
                *e = s;
            }
        }
        let mut edges: Vec<(String, String, f64)> =
            best.into_iter().map(|((f, t), s)| (f, t, s)).collect();
        edges.truncate(cap);
        self.upsert_spine_structural(&edges, now)
    }

    fn list_concept_ids(&self) -> Result<Vec<String>> {
        let res = self.query_read("?[id] := *concept_nodes{id}")?;
        Ok(res
            .rows
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.get_str()).map(|s| s.to_string()))
            .collect())
    }

    /// A member is LIVE unless its `entity_attestation` says `retired_at >= 0`.
    /// No attestation row → live (default).
    fn member_is_live(&self, entity_id: &str) -> Result<bool> {
        let mut p = BTreeMap::new();
        p.insert("e".to_string(), DataValue::Str(entity_id.into()));
        let res = self.query("?[retired_at] := *entity_attestation{entity_id: $e, retired_at}", p)?;
        Ok(match res.rows.first().and_then(|r| r.first()) {
            Some(DataValue::Num(Num::Float(x))) => *x < 0.0,
            Some(DataValue::Num(Num::Int(x))) => *x < 0,
            _ => true,
        })
    }

    fn set_concept_stale(&self, concept_id: &str, stale: bool, live_fraction: f64, now: f64) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("c".to_string(), DataValue::Str(concept_id.into()));
        p.insert("s".to_string(), DataValue::Bool(stale));
        p.insert("lf".to_string(), DataValue::Num(Num::Float(live_fraction)));
        p.insert("ca".to_string(), DataValue::Num(Num::Float(now)));
        self.query(
            "?[concept_id, stale, live_fraction, checked_at] <- [[$c, $s, $lf, $ca]] \
             :put concept_stale {concept_id => stale, live_fraction, checked_at}",
            p,
        )?;
        Ok(())
    }

    /// **Tethered plasticity (§4.5) — the truth leash.** Re-ground every concept
    /// against its tether: a concept is STALE when the live fraction of its member
    /// facts/entities falls below `quorum` (its ground truth has been retracted).
    /// Marks (never deletes — re-derived next dream). Returns `(checked, stale)`.
    pub fn recompute_concept_tethers(&self, quorum: f64, now: f64) -> Result<(usize, usize)> {
        let concept_ids = self.list_concept_ids()?;
        let mut stale_count = 0;
        for cid in &concept_ids {
            let members = self.get_concept_members(cid)?;
            if members.is_empty() {
                continue;
            }
            let live = members.iter().filter(|m| self.member_is_live(m).unwrap_or(true)).count();
            let frac = live as f64 / members.len() as f64;
            let is_stale = frac < quorum;
            self.set_concept_stale(cid, is_stale, frac, now)?;
            if is_stale {
                stale_count += 1;
            }
        }
        Ok((concept_ids.len(), stale_count))
    }

    /// Whether a concept is currently flagged stale (read-side gate — a stale
    /// generalization should be down-ranked/withheld until re-derived).
    pub fn is_concept_stale(&self, concept_id: &str) -> Result<bool> {
        let mut p = BTreeMap::new();
        p.insert("c".to_string(), DataValue::Str(concept_id.into()));
        let res = self.query("?[stale] := *concept_stale{concept_id: $c, stale}", p)?;
        Ok(res
            .rows
            .first()
            .and_then(|r| r.first())
            .map(|v| matches!(v, DataValue::Bool(true)))
            .unwrap_or(false))
    }

    /// `(stability, last_access)` for a node, or `None` if it's never been recalled.
    fn read_node_state(&self, node_id: &str) -> Result<Option<(f64, f64)>> {
        let mut p = BTreeMap::new();
        p.insert("n".to_string(), DataValue::Str(node_id.into()));
        let res =
            self.query("?[stability, last_access] := *node_state{node_id: $n, stability, last_access}", p)?;
        let getf = |r: &[DataValue], i: usize| match r.get(i) {
            Some(DataValue::Num(Num::Float(x))) => *x,
            Some(DataValue::Num(Num::Int(x))) => *x as f64,
            _ => 0.0,
        };
        Ok(res.rows.first().map(|r| (getf(r, 0).max(1e-6), getf(r, 1))))
    }

    /// **Stability bump on recall (§4.4 — the spacing effect).** Each recalled
    /// node `(id, activation)` becomes more durable: `S ← S·(1+κ·a)` (floored at
    /// 1), and `last_access ← now`. Used memories become HARDER to forget.
    pub fn bump_node_recall(&self, nodes: &[(String, f64)], kappa: f64, now: f64) -> Result<usize> {
        let mut n = 0;
        for (node, a) in nodes {
            if node.is_empty() {
                continue;
            }
            let s = self.read_node_state(node)?.map(|(s, _)| s).unwrap_or(1.0);
            let s_new = bump_stability(s, *a, kappa);
            let mut p = BTreeMap::new();
            p.insert("n".to_string(), DataValue::Str(node.clone().into()));
            p.insert("s".to_string(), DataValue::Num(Num::Float(s_new)));
            p.insert("la".to_string(), DataValue::Num(Num::Float(now)));
            self.query(
                "?[node_id, stability, last_access] <- [[$n, $s, $la]] \
                 :put node_state {node_id => stability, last_access}",
                p,
            )?;
            n += 1;
        }
        Ok(n)
    }

    /// **Node retrievability (§4.4b).** `ρ = exp(−(now − last_access)/stability)`
    /// in `(0,1]`. A node never recalled returns `1.0` (neutral — not penalized;
    /// it just hasn't been *used* yet). The forgetting signal for ranking.
    pub fn node_retrievability(&self, node_id: &str, now: f64) -> Result<f64> {
        match self.read_node_state(node_id)? {
            Some((stability, last_access)) => {
                Ok(retrievability((now - last_access).max(0.0), stability))
            }
            None => Ok(1.0),
        }
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

    #[test]
    fn structural_population_preserves_learned_plasticity() {
        use cozo::{DbInstance, ScriptMutability};
        let db = DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().unwrap();
        let now = 100.0;

        // Learn a plastic synapse A-B first (h > 0, no structural prior yet).
        store.bump_hebbian(&[("A".into(), "B".into(), 1.0)], 0.5, now).unwrap();
        let h_only = store.plastic_weight("A", "B", now, 0.0).unwrap();

        // A structural entity relation A-B exists in the graph.
        let mut p = std::collections::BTreeMap::new();
        p.insert("f".to_string(), DataValue::from("A"));
        p.insert("t".to_string(), DataValue::from("B"));
        store
            .raw_db()
            .run_script(
                r#"?[from_id, to_id, relation_type, strength] <- [[$f, $t, "rel", 0.4]]
                   :put entity_relations {from_id, to_id, relation_type => strength}"#,
                p,
                ScriptMutability::Mutable,
            )
            .unwrap();

        // Sleep populates the structural prior → w_struct added, learned h kept.
        assert_eq!(store.populate_spine_from_relations(now, 1000).unwrap(), 1);
        let w_full = store.plastic_weight("A", "B", now, 0.0).unwrap();
        assert!(w_full >= 0.4, "structural prior present");
        assert!(w_full > h_only, "structural prior added ON TOP of preserved h");
    }

    #[test]
    fn tethered_plasticity_marks_stale_when_ground_truth_dies() {
        use cozo::{DbInstance, ScriptMutability};
        let db = DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().unwrap();
        let now = 100.0;

        // A concept g1 grown from 3 members (the tether is `member_entity_ids_json`,
        // which is what `get_concept_members` reads).
        let mut cp = BTreeMap::new();
        cp.insert("id".to_string(), DataValue::from("g1"));
        cp.insert("mj".to_string(), DataValue::from(r#"["e1","e2","e3"]"#));
        store
            .raw_db()
            .run_script(
                r#"?[id, member_entity_ids_json] <- [[$id, $mj]]
                   :put concept_nodes {id => member_entity_ids_json}"#,
                cp,
                ScriptMutability::Mutable,
            )
            .unwrap();

        // e1 stays live; e2 and e3 are RETIRED (their ground truth was retracted).
        store.attest_entities(&["e1".into()], now).unwrap();
        for e in ["e2", "e3"] {
            let mut p = BTreeMap::new();
            p.insert("e".to_string(), DataValue::from(e));
            store
                .raw_db()
                .run_script(
                    r#"?[entity_id, attested, last_attested_at, retired_at] <- [[$e, true, 100.0, 200.0]]
                       :put entity_attestation {entity_id => attested, last_attested_at, retired_at}"#,
                    p,
                    ScriptMutability::Mutable,
                )
                .unwrap();
        }

        // Only 1/3 members live < 0.5 quorum → the concept has drifted → stale.
        let (checked, stale) = store.recompute_concept_tethers(0.5, now).unwrap();
        assert_eq!(checked, 1);
        assert_eq!(stale, 1);
        assert!(
            store.is_concept_stale("g1").unwrap(),
            "concept is stale once a quorum of its ground truth is retracted"
        );
    }

    #[test]
    fn node_stability_grows_with_recall_retrievability_decays() {
        use cozo::DbInstance;
        let db = DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().unwrap();
        let now = 100.0;

        // An unseen node is neutral (not penalized): ρ = 1.0.
        assert!((store.node_retrievability("N", now).unwrap() - 1.0).abs() < 1e-9);

        // Recall it once → stability bumps, last_access = now.
        store.bump_node_recall(&[("N".into(), 1.0)], 0.5, now).unwrap();
        assert!(store.node_retrievability("N", now).unwrap() > 0.99, "fresh recall stays retrievable");
        let after_1 = store.node_retrievability("N", now + 1.0).unwrap();
        assert!(after_1 < 1.0 && after_1 > 0.0, "retrievability decays with time");

        // Recall it AGAIN → stability grows → more retrievable at the same age.
        store.bump_node_recall(&[("N".into(), 1.0)], 0.5, now).unwrap();
        let after_2 = store.node_retrievability("N", now + 1.0).unwrap();
        assert!(after_2 > after_1, "more recalls ⇒ harder to forget (spacing effect)");
    }
}
