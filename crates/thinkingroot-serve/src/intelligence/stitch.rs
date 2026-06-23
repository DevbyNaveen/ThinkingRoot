//! The Stitcher — graph-tending creature (pure, testable core).
//!
//! A per-brain background agent that patrols the cognition graph and tends its
//! relational STRUCTURE (vs `dream`, which generates new claim CONTENT):
//!   * weave  — form/strengthen `entity_relations` edges ingest never made,
//!   * resolve — merge duplicate entities (destructive → proposal, never auto),
//!   * grow   — detect communities and grow higher-order concept nodes.
//!
//! Algorithm (Approach A): a DETERMINISTIC candidate phase (this module +
//! graph reads) proposes pairs grounded in real structural evidence, then the
//! workspace LLM only ADJUDICATES the type + confidence. The LLM is never
//! given the freedom to invent a pair — that separation is the hallucination
//! firewall. Every accepted edge carries a Connection contract (evidence +
//! confidence + provenance `stitch://`).
//!
//! This file is the pure text/graph-algorithm core (ontology, candidate
//! ranking, prompt + parse, community detection). The loop (lock → fork
//! quarantine branch → adjudicate → settle) lives on `QueryEngine`.

/// The open relation vocabulary the Stitcher may assign. Extends the engine's
/// fixed structural enum with epistemic/causal/associative relations the
/// memory-graph literature shows matter. The LLM must pick from this set;
/// anything else normalizes to the weak `related-to`.
pub const STITCH_RELATION_TYPES: &[&str] = &[
    "supports",
    "contradicts",
    "elaborates",
    "refines",
    "exemplifies",
    "generalizes",
    "causes",
    "precedes",
    "enables",
    "co-occurs-with",
    "analogous-to",
    "related-to",
    "depends-on",
    "part-of",
    "uses",
];

/// Canonicalize a raw LLM-emitted relation type to the ontology, else fall back
/// to `related-to`. Tolerant of spaces/underscores and case.
pub fn normalize_relation_type(raw: &str) -> String {
    let r = raw
        .trim()
        .to_lowercase()
        .replace(|c: char| c == ' ' || c == '_', "-");
    if STITCH_RELATION_TYPES.contains(&r.as_str()) {
        r
    } else {
        "related-to".to_string()
    }
}

/// A candidate connection surfaced by the deterministic phase. Grounded in real
/// structural evidence — the LLM only adjudicates whether/how the pair relates.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub from_id: String,
    pub to_id: String,
    pub from_name: String,
    pub to_name: String,
    /// Structural-signal strength (co-citation weight, co-occurrence count, …);
    /// used only for ranking the candidate list.
    pub signal: f64,
    /// Human-readable context shown to the LLM (e.g. a shared claim statement).
    pub context: String,
    /// Claim ids that justify the pair — the non-empty evidence carried into the
    /// Connection contract. The LLM cannot invent these.
    pub evidence: Vec<String>,
}

/// An adjudicated, contract-complete connection ready to weave.
#[derive(Debug, Clone)]
pub struct Connection {
    pub from_id: String,
    pub to_id: String,
    pub relation_type: String,
    pub confidence: f64,
    pub evidence: Vec<String>,
}

/// Dedup candidates by unordered (from,to) pair (keeping the highest signal and
/// merging evidence), drop self/empty pairs, rank by signal desc, cap to `cap`.
/// Deterministic (stable tie-break by id) → unit-testable with no model.
pub fn rank_and_cap(cands: Vec<Candidate>, cap: usize) -> Vec<Candidate> {
    use std::collections::HashMap;
    let mut best: HashMap<(String, String), Candidate> = HashMap::new();
    for c in cands {
        if c.from_id == c.to_id || c.from_id.is_empty() || c.to_id.is_empty() {
            continue;
        }
        let key = if c.from_id <= c.to_id {
            (c.from_id.clone(), c.to_id.clone())
        } else {
            (c.to_id.clone(), c.from_id.clone())
        };
        match best.get_mut(&key) {
            Some(e) => {
                if c.signal > e.signal {
                    e.signal = c.signal;
                }
                for ev in c.evidence {
                    if !e.evidence.contains(&ev) {
                        e.evidence.push(ev);
                    }
                }
                if e.context.is_empty() {
                    e.context = c.context;
                }
            }
            None => {
                best.insert(key, c);
            }
        }
    }
    let mut out: Vec<Candidate> = best.into_values().collect();
    out.sort_by(|a, b| {
        b.signal
            .partial_cmp(&a.signal)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.from_id.cmp(&b.from_id))
            .then_with(|| a.to_id.cmp(&b.to_id))
    });
    out.truncate(cap);
    out
}

/// System prompt for adjudication. Constrains output to the ontology + the given
/// indices (the firewall) and a strict one-line format.
pub const STITCH_SYSTEM: &str = "You are the relational weaver of a memory graph. You are given \
candidate pairs of entities that a structural signal suggests MIGHT be related, with the context \
connecting them. For each pair that is GENUINELY related, output ONE line with the pair's index, a \
single relationship type, and a confidence between 0 and 1. Choose the type ONLY from this set: \
supports, contradicts, elaborates, refines, exemplifies, generalizes, causes, precedes, enables, \
co-occurs-with, analogous-to, related-to, depends-on, part-of, uses. NEVER introduce a pair that is \
not listed. If a pair is not meaningfully related, omit it. Output format, one per line: \
`<index> <type> <confidence>` (e.g. `3 supports 0.82`). No preamble, no numbering, no markdown.";

/// Build the adjudication prompt from the candidate list.
pub fn build_stitch_prompt(cands: &[Candidate]) -> String {
    let mut p = String::from("Candidate entity pairs:\n");
    for (i, c) in cands.iter().enumerate() {
        p.push_str(&format!("[{i}] \"{}\" — \"{}\"", c.from_name, c.to_name));
        let ctx = c.context.trim();
        if !ctx.is_empty() {
            p.push_str("  context: ");
            p.push_str(ctx);
        }
        p.push('\n');
    }
    p.push_str("\nRelationships (one per line, `<index> <type> <confidence>`):");
    p
}

/// Parse the LLM output into Connections. Each line maps to a candidate BY
/// INDEX — an index out of range is dropped (the firewall: the LLM cannot weave
/// a pair we did not propose). Tolerant of bullets/brackets/punctuation.
pub fn parse_stitch_connections(text: &str, cands: &[Candidate]) -> Vec<Connection> {
    let mut out = Vec::new();
    for line in text.lines() {
        let s = line
            .trim()
            .trim_start_matches(|c: char| c == '-' || c == '*' || c == '•')
            .trim();
        if s.is_empty() {
            continue;
        }
        let cleaned = s.replace(
            |c: char| matches!(c, '[' | ']' | ':' | '(' | ')' | ','),
            " ",
        );
        let mut it = cleaned.split_whitespace();
        let Some(idx_tok) = it.next() else { continue };
        let Ok(idx) = idx_tok.parse::<usize>() else { continue };
        let Some(cand) = cands.get(idx) else { continue }; // firewall: unknown index dropped
        let Some(type_tok) = it.next() else { continue };
        if type_tok.eq_ignore_ascii_case("none") {
            continue; // explicit non-relationship
        }
        let rtype = normalize_relation_type(type_tok);
        let conf = it
            .filter_map(|t| t.parse::<f64>().ok())
            .last()
            .unwrap_or(0.6)
            .clamp(0.0, 1.0);
        out.push(Connection {
            from_id: cand.from_id.clone(),
            to_id: cand.to_id.clone(),
            relation_type: rtype,
            confidence: conf,
            evidence: cand.evidence.clone(),
        });
    }
    out
}

/// GraphRAG community detection via synchronous label propagation over an
/// undirected edge list. Deterministic: nodes processed in sorted order, ties
/// broken toward the smallest label. Returns communities of size ≥ `min_size`
/// (singletons dropped) — each becomes a grown concept node.
pub fn detect_communities(
    edges: &[(String, String)],
    rounds: usize,
    min_size: usize,
) -> Vec<Vec<String>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (a, b) in edges {
        if a == b || a.is_empty() || b.is_empty() {
            continue;
        }
        adj.entry(a.clone()).or_default().insert(b.clone());
        adj.entry(b.clone()).or_default().insert(a.clone());
    }
    let mut label: BTreeMap<String, String> =
        adj.keys().map(|k| (k.clone(), k.clone())).collect();
    for _ in 0..rounds.max(1) {
        let mut changed = false;
        for (node, neighbors) in &adj {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for nb in neighbors {
                if let Some(l) = label.get(nb) {
                    *counts.entry(l.clone()).or_insert(0) += 1;
                }
            }
            // most frequent neighbour label; tie → smallest label.
            if let Some(best) = counts
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(k, _)| k.clone())
            {
                if label.get(node) != Some(&best) {
                    label.insert(node.clone(), best);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (node, l) in &label {
        groups.entry(l.clone()).or_default().push(node.clone());
    }
    groups
        .into_values()
        .filter(|g| g.len() >= min_size.max(2))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(from: &str, to: &str, sig: f64, ev: &[&str]) -> Candidate {
        Candidate {
            from_id: from.into(),
            to_id: to.into(),
            from_name: from.into(),
            to_name: to.into(),
            signal: sig,
            context: String::new(),
            evidence: ev.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn normalize_maps_and_falls_back() {
        assert_eq!(normalize_relation_type("Supports"), "supports");
        assert_eq!(normalize_relation_type("co occurs with"), "co-occurs-with");
        assert_eq!(normalize_relation_type("frobnicates"), "related-to");
    }

    #[test]
    fn rank_dedups_unordered_pairs_and_caps() {
        let got = rank_and_cap(
            vec![
                cand("a", "b", 0.5, &["c1"]),
                cand("b", "a", 0.9, &["c2"]), // same unordered pair, higher signal
                cand("a", "a", 1.0, &[]),     // self-pair dropped
                cand("c", "d", 0.2, &[]),
            ],
            10,
        );
        assert_eq!(got.len(), 2, "self-pair dropped, a-b deduped");
        // highest-signal pair first; evidence merged.
        assert_eq!(got[0].signal, 0.9);
        assert!(got[0].evidence.contains(&"c1".to_string()));
        assert!(got[0].evidence.contains(&"c2".to_string()));
    }

    #[test]
    fn parse_is_firewalled_to_known_indices() {
        let cands = vec![cand("e0a", "e0b", 1.0, &["c1"]), cand("e1a", "e1b", 1.0, &["c2"])];
        // line 0 valid; line 9 out of range (dropped); "none" skipped; bullets ok.
        let text = "- [0] supports 0.82\n9 causes 0.9\n1: none\n* 1 part_of 0.7";
        let got = parse_stitch_connections(text, &cands);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].relation_type, "supports");
        assert_eq!(got[0].from_id, "e0a");
        assert_eq!(got[0].evidence, vec!["c1".to_string()]);
        assert_eq!(got[1].relation_type, "part-of");
        assert!((got[0].confidence - 0.82).abs() < 1e-9);
    }

    #[test]
    fn communities_separate_two_clusters() {
        let edges = vec![
            ("a".into(), "b".into()),
            ("b".into(), "c".into()),
            ("a".into(), "c".into()),
            ("x".into(), "y".into()),
            ("y".into(), "z".into()),
            ("x".into(), "z".into()),
        ];
        let mut comms = detect_communities(&edges, 10, 2);
        comms.sort_by_key(|g| g.first().cloned().unwrap_or_default());
        assert_eq!(comms.len(), 2);
        assert_eq!(comms[0], vec!["a", "b", "c"]);
        assert_eq!(comms[1], vec!["x", "y", "z"]);
    }

    #[test]
    fn prompt_lists_indexed_pairs() {
        let p = build_stitch_prompt(&[cand("Auth", "Billing", 1.0, &["c1"])]);
        assert!(p.contains("[0] \"Auth\" — \"Billing\""));
        assert!(p.trim_end().ends_with("<confidence>`):"));
    }
}
