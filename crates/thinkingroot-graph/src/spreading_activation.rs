//! Slice 10 — deterministic spreading-activation BFS over the
//! workspace's `entity_relations` table.
//!
//! Cognitive-science basis: Collins & Loftus 1975 (spreading
//! activation theory of semantic memory) and ACT-R (Anderson
//! 1983–2004). The algorithm is pure graph traversal — no learning,
//! no weight training, no fabrication. Given a seed entity it walks
//! up to `max_hops` of `entity_relations` edges, accumulating each
//! reached entity's intensity as `decay ^ hop_distance`.
//!
//! # Why a separate module
//!
//! The `graph.rs` file is already 6k+ lines and the BFS is a small
//! self-contained read query — keeping it here means callers reach
//! `thinkingroot_graph::spreading_activation::spread` via a clean
//! top-level path.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::graph::GraphStore;
use crate::{Error, Result};
use cozo::{DataValue, ScriptMutability};
use serde::{Deserialize, Serialize};

/// One step in a spreading-activation cascade. `hop_distance == 0`
/// indicates the seed itself; `intensity == decay ^ hop_distance`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CascadeRipple {
    /// Reached entity id.
    pub entity_id: String,
    /// Number of `entity_relations` hops between the seed and this
    /// entity. 0 for the seed itself.
    pub hop_distance: u8,
    /// Decay-discounted activation intensity in `[0.0, 1.0]`.
    pub intensity: f64,
}

/// Default decay factor — each hop halves the intensity (ACT-R-style).
pub const DEFAULT_DECAY: f64 = 0.5;
/// Default maximum hops. 3 is the classic ACT-R bound for "this is
/// still semantically related" before the chain dilutes into noise.
pub const DEFAULT_MAX_HOPS: u8 = 3;
/// Defensive cap on the number of entities returned. Covers the
/// pathological-hub case (one entity connected to thousands) so the
/// caller never has to trim a 50k-row response.
pub const MAX_FRONTIER: usize = 1024;

/// Compute the spreading-activation cascade rooted at `seed`.
///
/// Always includes the seed itself with `hop_distance == 0` and
/// `intensity == 1.0`.  The query reads `entity_relations` directly
/// — that table aggregates per-source edges into one global edge per
/// `(from_id, to_id, relation_type)`, so a hop is independent of how
/// many sources mentioned the relation.
pub fn spread(
    store: &GraphStore,
    seed: &str,
    max_hops: u8,
    decay: f64,
) -> Result<Vec<CascadeRipple>> {
    if seed.is_empty() {
        return Err(Error::Config(
            "spreading_activation: seed entity id must not be empty".to_string(),
        ));
    }
    let max_hops = max_hops.min(8); // hard ceiling; deeper than 8 is noise
    let decay = if decay.is_finite() && decay >= 0.0 && decay <= 1.0 {
        decay
    } else {
        DEFAULT_DECAY
    };

    let mut visited: HashMap<String, u8> = HashMap::new();
    visited.insert(seed.to_string(), 0);
    let mut frontier: VecDeque<String> = VecDeque::new();
    frontier.push_back(seed.to_string());

    for hop in 1..=max_hops {
        if frontier.is_empty() {
            break;
        }
        let mut next: HashSet<String> = HashSet::new();
        // Walk one BFS layer.  The cozo query is parameterised on
        // `from_id` so we issue at most |frontier| queries per hop —
        // bounded above by `MAX_FRONTIER`.  In practice frontier
        // sizes for typical workspaces stay well below this.
        for entity in frontier.drain(..) {
            if visited.len() >= MAX_FRONTIER {
                break;
            }
            for neighbour in neighbours_of(store, &entity)? {
                if !visited.contains_key(&neighbour) {
                    visited.insert(neighbour.clone(), hop);
                    next.insert(neighbour);
                    if visited.len() >= MAX_FRONTIER {
                        break;
                    }
                }
            }
        }
        frontier.extend(next);
    }

    let mut ripples: Vec<CascadeRipple> = visited
        .into_iter()
        .map(|(entity_id, hop_distance)| CascadeRipple {
            entity_id,
            hop_distance,
            intensity: decay.powi(hop_distance as i32),
        })
        .collect();
    // Stable order: hop ascending, then entity id lexicographically.
    ripples.sort_by(|a, b| {
        a.hop_distance
            .cmp(&b.hop_distance)
            .then_with(|| a.entity_id.cmp(&b.entity_id))
    });
    Ok(ripples)
}

fn neighbours_of(store: &GraphStore, entity: &str) -> Result<Vec<String>> {
    // Both directions — the relation is undirected for activation
    // purposes (Collins & Loftus treat the network as symmetric).
    // Cozo Datalog doesn't support OR-style heads in a single script,
    // so we run two queries and union the results.  Same pattern used
    // by `graph::get_relations_from_entity`.
    let mut params = std::collections::BTreeMap::new();
    params.insert("eid".to_string(), DataValue::from(entity));
    let raw = store.raw_db();
    let outgoing = raw
        .run_script(
            "?[other] := *entity_relations{from_id: $eid, to_id: other}",
            params.clone(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("spread/outgoing: {e}")))?;
    let incoming = raw
        .run_script(
            "?[other] := *entity_relations{from_id: other, to_id: $eid}",
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| Error::GraphStorage(format!("spread/incoming: {e}")))?;

    let mut out: HashSet<String> = HashSet::new();
    for row in outgoing.rows.into_iter().chain(incoming.rows.into_iter()) {
        if let Some(name) = row
            .into_iter()
            .next()
            .and_then(|v| v.get_str().map(|s| s.to_string()))
        {
            out.insert(name);
        }
    }
    Ok(out.into_iter().collect())
}

/// Multi-session / HippoRAG-style recall expansion (the multi-hop step the
/// default `/ask` retriever lacks). From a set of vector-retrieved SEED claims,
/// flow activation out through the entity graph and pull back the claims of the
/// activated entities — the connected memories a pure vector search misses
/// because they share no surface terms with the query.
///
/// Pipeline: seed claims → their entities (`get_entity_ids_for_claim`) →
/// [`spread`] over `entity_relations` (decaying with hop distance) → the claims
/// of each activated entity (`get_claims_for_entity`). Returns
/// `(claim_id, statement, claim_type, activation_weight)` for NEW claims only
/// (seeds excluded), strongest-activation first. Everything is bounded
/// (`seed_entity_cap`, `out_claim_cap`, [`spread`]'s own `MAX_FRONTIER`) so cost
/// stays predictable on hub-heavy graphs. Read-only.
#[allow(clippy::too_many_arguments)]
/// `plastic = Some((now, lambda))` turns on the **recall-is-rewrite READ** (L3):
/// in addition to structural `entity_relations` spread, the seeds' *plastic*
/// neighbours (`spine_edge`, Hebbian-strengthened by past co-recall) are
/// activated — so paths you've used before surface. `None` = pure structural
/// spread (the default).
#[allow(clippy::too_many_arguments)]
pub fn expand_claims_from_seeds(
    store: &GraphStore,
    seed_claim_ids: &[String],
    max_hops: u8,
    decay: f64,
    seed_entity_cap: usize,
    out_claim_cap: usize,
    plastic: Option<(f64, f64)>,
) -> Result<Vec<(String, String, String, f32)>> {
    let seed_set: HashSet<&str> = seed_claim_ids.iter().map(|s| s.as_str()).collect();

    // 1. Seed entities = the distinct entities mentioned by the seed claims.
    let mut seed_entities: Vec<String> = Vec::new();
    let mut seen_ent: HashSet<String> = HashSet::new();
    for cid in seed_claim_ids {
        if seed_entities.len() >= seed_entity_cap {
            break;
        }
        for eid in store.get_entity_ids_for_claim(cid).unwrap_or_default() {
            if seen_ent.insert(eid.clone()) {
                seed_entities.push(eid);
                if seed_entities.len() >= seed_entity_cap {
                    break;
                }
            }
        }
    }
    if seed_entities.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Spread from each seed; keep the MAX intensity that reached each entity.
    let mut activation: HashMap<String, f32> = HashMap::new();
    for seed in &seed_entities {
        for ripple in spread(store, seed, max_hops, decay)? {
            if ripple.hop_distance == 0 {
                continue; // the seed entity itself — its claims are already seeds
            }
            let e = activation.entry(ripple.entity_id).or_insert(0.0);
            *e = e.max(ripple.intensity as f32);
        }
    }

    // 2b. Recall-is-rewrite READ: also activate the seeds' PLASTIC neighbours
    // (Hebbian synapses from past co-recall), so used paths surface even with no
    // structural relation. This is the read side of the L3 loop.
    if let Some((now, lambda)) = plastic {
        for (nbr, w) in store.plastic_neighbors(&seed_entities, now, lambda, 0.05)? {
            let e = activation.entry(nbr).or_insert(0.0);
            *e = e.max(w as f32);
        }
        // §4.4b forgetting affects RECALL: scale each activated node by its
        // retrievability ρ = exp(−Δt/S), so a faded (long-unused) memory
        // contributes less. Unseen nodes are ρ=1 (neutral). Closes the read loop:
        // decay → low ρ → lower rank.
        for (eid, a) in activation.iter_mut() {
            let rho = store.node_retrievability(eid, now).unwrap_or(1.0) as f32;
            *a *= rho;
        }
    }

    // 3. Materialize claims for activated entities (strongest first), weighting
    //    each claim by the activation that reached its entity; exclude seeds.
    let mut entities: Vec<(String, f32)> = activation.into_iter().collect();
    entities.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: HashMap<String, (String, String, f32)> = HashMap::new();
    'outer: for (eid, intensity) in entities {
        for (cid, statement, ctype) in store.get_claims_for_entity(&eid).unwrap_or_default() {
            if seed_set.contains(cid.as_str()) {
                continue;
            }
            let entry = out.entry(cid).or_insert((statement, ctype, 0.0));
            entry.2 = entry.2.max(intensity);
            if out.len() >= out_claim_cap {
                break 'outer;
            }
        }
    }

    let mut result: Vec<(String, String, String, f32)> = out
        .into_iter()
        .map(|(cid, (st, ct, w))| (cid, st, ct, w))
        .collect();
    result.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cozo::{DataValue, DbInstance, ScriptMutability};

    /// Build an in-memory store with a triangle A→B→C plus an isolated D.
    fn fixture() -> GraphStore {
        let db = DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().unwrap();
        for (a, b) in &[("A", "B"), ("B", "C"), ("A", "D")] {
            insert_edge(&store, a, b);
        }
        store
    }

    fn insert_edge(store: &GraphStore, from: &str, to: &str) {
        let mut params = std::collections::BTreeMap::new();
        params.insert("fid".to_string(), DataValue::from(from));
        params.insert("tid".to_string(), DataValue::from(to));
        params.insert("rtype".to_string(), DataValue::from("rel"));
        params.insert("str".to_string(), DataValue::from(1.0_f64));
        store
            .raw_db()
            .run_script(
                r#"?[from_id, to_id, relation_type, strength] <- [[$fid, $tid, $rtype, $str]]
                   :put entity_relations { from_id, to_id, relation_type => strength }"#,
                params,
                ScriptMutability::Mutable,
            )
            .unwrap();
    }

    #[test]
    fn bfs_returns_seed_at_intensity_one() {
        let store = fixture();
        let ripples = spread(&store, "A", 0, 0.5).unwrap();
        assert_eq!(ripples.len(), 1);
        assert_eq!(ripples[0].entity_id, "A");
        assert_eq!(ripples[0].hop_distance, 0);
        assert!((ripples[0].intensity - 1.0).abs() < 1e-9);
    }

    #[test]
    fn bfs_reaches_two_hops_and_decays() {
        let store = fixture();
        let ripples = spread(&store, "A", 3, 0.5).unwrap();
        // A (hop 0), B + D (hop 1), C (hop 2)
        let by_id: std::collections::HashMap<&str, &CascadeRipple> =
            ripples.iter().map(|r| (r.entity_id.as_str(), r)).collect();
        assert_eq!(by_id["A"].hop_distance, 0);
        assert_eq!(by_id["B"].hop_distance, 1);
        assert_eq!(by_id["D"].hop_distance, 1);
        assert_eq!(by_id["C"].hop_distance, 2);
        assert!((by_id["B"].intensity - 0.5).abs() < 1e-9);
        assert!((by_id["C"].intensity - 0.25).abs() < 1e-9);
    }

    #[test]
    fn bfs_handles_cycle_without_double_counting() {
        let store = fixture();
        // Add a cycle C → A so a walker could loop.
        insert_edge(&store, "C", "A");
        let ripples = spread(&store, "A", 4, 0.5).unwrap();
        // Each entity appears at most once.
        let ids: std::collections::HashSet<&str> =
            ripples.iter().map(|r| r.entity_id.as_str()).collect();
        assert_eq!(ids.len(), ripples.len(), "no duplicates");
    }

    #[test]
    fn bfs_caps_max_hops_to_eight() {
        let store = fixture();
        // Even when we ask for 50 hops the algorithm bounds itself.
        let ripples = spread(&store, "A", 50, 0.5).unwrap();
        let max = ripples.iter().map(|r| r.hop_distance).max().unwrap();
        assert!(max <= 8);
    }

    #[test]
    fn bfs_seed_must_not_be_empty() {
        let store = fixture();
        assert!(spread(&store, "", 3, 0.5).is_err());
    }

    #[test]
    fn bfs_invalid_decay_falls_back_to_default() {
        let store = fixture();
        let ripples = spread(&store, "A", 1, f64::NAN).unwrap();
        let b = ripples.iter().find(|r| r.entity_id == "B").unwrap();
        // Default decay is 0.5; hop 1 → intensity 0.5.
        assert!((b.intensity - 0.5).abs() < 1e-9);
    }

    fn insert_claim(store: &GraphStore, id: &str, source: &str) {
        let mut p = std::collections::BTreeMap::new();
        p.insert("id".to_string(), DataValue::from(id));
        p.insert("st".to_string(), DataValue::from("a statement"));
        p.insert("ct".to_string(), DataValue::from("fact"));
        p.insert("src".to_string(), DataValue::from(source));
        store
            .raw_db()
            .run_script(
                r#"?[id, statement, claim_type, source_id] <- [[$id, $st, $ct, $src]]
                   :put claims {id => statement, claim_type, source_id}"#,
                p,
                ScriptMutability::Mutable,
            )
            .unwrap();
    }

    fn link_claim_entity(store: &GraphStore, claim: &str, entity: &str) {
        let mut p = std::collections::BTreeMap::new();
        p.insert("cid".to_string(), DataValue::from(claim));
        p.insert("eid".to_string(), DataValue::from(entity));
        store
            .raw_db()
            .run_script(
                r#"?[claim_id, entity_id] <- [[$cid, $eid]]
                   :put claim_entity_edges {claim_id, entity_id}"#,
                p,
                ScriptMutability::Mutable,
            )
            .unwrap();
    }

    #[test]
    fn expand_pulls_connected_claims_via_multi_hop() {
        let store = fixture(); // entity edges: A→B, B→C, A→D
        insert_claim(&store, "cA", "srcX");
        insert_claim(&store, "cC", "srcX");
        link_claim_entity(&store, "cA", "A");
        link_claim_entity(&store, "cC", "C");

        // Seed = cA (mentions A). Activation flows A→B→C and pulls C's claim —
        // a memory pure vector search on cA's text would never reach.
        let out =
            expand_claims_from_seeds(&store, &["cA".to_string()], 3, 0.5, 16, 16, None).unwrap();
        let ids: Vec<&str> = out.iter().map(|(c, ..)| c.as_str()).collect();
        assert!(ids.contains(&"cC"), "multi-hop reaches C's claim");
        assert!(!ids.contains(&"cA"), "seed claim is excluded from expansion");
    }

    #[test]
    fn expand_empty_when_no_seed_entities() {
        let store = fixture();
        // cZ has no entity links → nothing to spread from.
        insert_claim(&store, "cZ", "srcX");
        let out =
            expand_claims_from_seeds(&store, &["cZ".to_string()], 3, 0.5, 16, 16, None).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn plastic_edge_surfaces_claim_closing_the_loop() {
        // The recall-is-rewrite loop: a Hebbian synapse (no structural relation)
        // pulls a connected claim on the NEXT recall.
        let store = fixture(); // structural edges: A→B, B→C, A→D (no Z)
        insert_claim(&store, "cA", "srcX");
        insert_claim(&store, "cZ", "srcX");
        link_claim_entity(&store, "cA", "A");
        link_claim_entity(&store, "cZ", "Z"); // Z is NOT in entity_relations

        // Without plasticity, a seed on A can never reach Z's claim.
        let cold =
            expand_claims_from_seeds(&store, &["cA".to_string()], 3, 0.5, 16, 16, None).unwrap();
        assert!(!cold.iter().any(|(c, ..)| c == "cZ"), "no structural path A→Z");

        // A past recall co-activated A and Z → Hebbian synapse written.
        store.bump_hebbian(&[("A".into(), "Z".into(), 1.0)], 0.5, 100.0).unwrap();

        // Now the SAME seed reaches Z's claim via the plastic edge (loop closed).
        let warm = expand_claims_from_seeds(
            &store,
            &["cA".to_string()],
            3,
            0.5,
            16,
            16,
            Some((100.0, 1e-6)),
        )
        .unwrap();
        assert!(
            warm.iter().any(|(c, ..)| c == "cZ"),
            "plastic synapse from past co-recall surfaces cZ"
        );
    }
}
