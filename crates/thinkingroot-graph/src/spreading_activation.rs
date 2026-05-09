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
}
