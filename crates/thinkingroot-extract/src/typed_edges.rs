//! Typed-edge derivation — SOTA Lever 2 mechanical extractors.
//!
//! Reads a `&[Witness]` slice (the same `filtered_extraction.witnesses`
//! that Phase 6.45 of the pipeline persists) and derives
//! `(from, to, edge_type, evidence, confidence)` tuples for the
//! `witness_typed_edges` Cozo table.
//!
//! Each extractor is purely mechanical — no LLM, no network — and emits
//! at most O(n²) edges over the input window. Empirical n is small (a
//! single source typically contributes <1000 witnesses), so the quadratic
//! pair scans stay cheap. The aggregator
//! [`derive_all_typed_edges`] runs every extractor and returns the
//! deduplicated union; pipeline callers pass the result to
//! [`thinkingroot_graph::graph::GraphStore::insert_witness_typed_edges_batch`].
//!
//! Why this lives in `thinkingroot-extract` and not `thinkingroot-graph`:
//! the extractor crate already owns the rule-catalog source of truth and
//! the `collect_witnesses_from_documents` entry point. Putting derivation
//! beside catalog enforcement keeps "rule name → output edge_type" in a
//! single audit surface.
//!
//! Catalog version 1.4.0 (2026-05-15) adds the 4 typed-edge rules:
//!   - `edge::markdown-supersedes@v1`  (deferred — needs heading-path infra)
//!   - `edge::quantity-contradicts@v1` (deferred — needs value extraction)
//!   - `edge::heading-related@v1`      (implemented v1)
//!   - `edge::temporal-next@v1`        (implemented v1)
//!
//! `Supersedes` and `Contradicts` emitters are stubbed out and return
//! empty vecs. They will land as a follow-up once heading_path + quantity
//! value extraction are wired through to the Witness payload (today both
//! signals exist only in the CCC structural tables, not on the Witness
//! row itself).

use std::collections::BTreeMap;

use thinkingroot_core::types::{Witness, WitnessId};

/// A single typed edge ready for `insert_witness_typed_edges_batch`.
/// Tuple shape matches that function's parameter exactly.
pub type DerivedTypedEdge = (WitnessId, WitnessId, &'static str, Option<WitnessId>, f32);

/// Edge type names — must match the alphabet enforced at the storage
/// layer (`witness_inserts::VALID_TYPES`). Reusing string literals
/// here avoids a cross-crate dependency on a shared enum; the storage
/// layer rejects misspellings loudly.
const EDGE_RELATED: &str = "Related";
const EDGE_TEMPORAL_NEXT: &str = "TemporalNext";
#[allow(dead_code)]
const EDGE_SUPERSEDES: &str = "Supersedes";
#[allow(dead_code)]
const EDGE_CONTRADICTS: &str = "Contradicts";

/// Rule names — must match the catalog entries in `rule_catalog.rs`.
const RULE_HEADING: &str = "markdown::heading@v1";
const RULE_GIT_COMMIT: &str = "git::commit@v1";
const EVIDENCE_RULE_HEADING_RELATED: &str = "edge::heading-related@v1";
const EVIDENCE_RULE_TEMPORAL_NEXT: &str = "edge::temporal-next@v1";

/// Confidence everything in this module emits. Pinned to the catalog
/// default for the edge rules (0.95). A future per-rule override can
/// raise/lower via the descriptor.
const DEFAULT_EDGE_CONFIDENCE: f32 = 0.95;

/// Run every active edge derivation pass over `witnesses` and return
/// the union. Each pass is idempotent; running the aggregator twice
/// against the same input vec produces edges that the storage layer's
/// `:put witness_typed_edges` will dedupe by primary key.
///
/// `pipeline_started_at` is reserved for future edge rules that need a
/// canonical now-value (matches Witness.created_at semantics). Today's
/// active extractors don't use it but it's threaded through so the
/// signature is stable across future ships.
pub fn derive_all_typed_edges(witnesses: &[Witness]) -> Vec<DerivedTypedEdge> {
    let mut out: Vec<DerivedTypedEdge> = Vec::new();
    out.extend(derive_heading_related(witnesses));
    out.extend(derive_temporal_next(witnesses));
    // Future extractors:
    //   out.extend(derive_markdown_supersedes(witnesses));
    //   out.extend(derive_quantity_contradicts(witnesses));
    //
    // Deduplicate by (from, to, type) so multiple passes producing the
    // same edge collapse — the storage layer's PK enforces uniqueness
    // anyway but pre-filtering here cuts batch I/O.
    out.sort_by(|a, b| {
        a.0.to_hex()
            .cmp(&b.0.to_hex())
            .then_with(|| a.1.to_hex().cmp(&b.1.to_hex()))
            .then_with(|| a.2.cmp(b.2))
    });
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
    out
}

/// `edge::heading-related@v1` — emits a single `Related` edge per pair
/// of markdown heading witnesses that share the same exact heading
/// text. Edge direction is deterministic: alphabetically-earlier id is
/// the `from`. The directed-but-symmetric storage shape matches the
/// `list_witness_contradictions` retrieval idiom.
///
/// Heading text equality is a strict byte comparison; case-insensitive
/// or stemming-tolerant matching would require a normalisation pass
/// that we're deliberately keeping out of v1 (silent over-matching is
/// worse than honest under-matching for substrate trust).
///
/// Empty `symbol` headings are skipped — they carry no signal for
/// cross-source linking.
pub fn derive_heading_related(witnesses: &[Witness]) -> Vec<DerivedTypedEdge> {
    let mut by_symbol: BTreeMap<&str, Vec<&Witness>> = BTreeMap::new();
    for w in witnesses {
        if w.rule != RULE_HEADING {
            continue;
        }
        let Some(sym) = w.symbol.as_deref() else {
            continue;
        };
        if sym.is_empty() {
            continue;
        }
        by_symbol.entry(sym).or_default().push(w);
    }

    let mut out: Vec<DerivedTypedEdge> = Vec::new();
    for group in by_symbol.values() {
        if group.len() < 2 {
            continue;
        }
        // Sort by id hex so edge direction is deterministic across
        // runs — same input vec → same emitted edges, byte-for-byte.
        let mut sorted: Vec<&&Witness> = group.iter().collect();
        sorted.sort_by_key(|w| w.id.to_hex());
        for i in 0..sorted.len() {
            for j in (i + 1)..sorted.len() {
                let from = sorted[i].id.clone();
                let to = sorted[j].id.clone();
                // Self-loop guard — content-addressed ids make this
                // theoretically impossible for distinct witnesses but
                // a future caller-bug that passes duplicates would
                // surface here.
                if from == to {
                    continue;
                }
                out.push((
                    from,
                    to,
                    EDGE_RELATED,
                    Some(synthesise_evidence_id(EVIDENCE_RULE_HEADING_RELATED)),
                    DEFAULT_EDGE_CONFIDENCE,
                ));
            }
        }
    }
    out
}

/// `edge::temporal-next@v1` — chains `git::commit@v1` witnesses into a
/// `TemporalNext` linked list ordered by `created_at`. The chain is
/// per-source (each source's commits chain independently) because two
/// commits to different files have no temporal-meaningful relationship
/// in the retrieval substrate.
///
/// Ties on `created_at` break by id hex for determinism. Single-commit
/// sources emit zero edges (no `next` to link to).
pub fn derive_temporal_next(witnesses: &[Witness]) -> Vec<DerivedTypedEdge> {
    let mut by_source: BTreeMap<String, Vec<&Witness>> = BTreeMap::new();
    for w in witnesses {
        if w.rule != RULE_GIT_COMMIT {
            continue;
        }
        by_source
            .entry(w.source.to_string())
            .or_default()
            .push(w);
    }

    let mut out: Vec<DerivedTypedEdge> = Vec::new();
    for commits in by_source.values_mut() {
        if commits.len() < 2 {
            continue;
        }
        commits.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.to_hex().cmp(&b.id.to_hex()))
        });
        for win in commits.windows(2) {
            let from = win[0].id.clone();
            let to = win[1].id.clone();
            if from == to {
                continue;
            }
            out.push((
                from,
                to,
                EDGE_TEMPORAL_NEXT,
                Some(synthesise_evidence_id(EVIDENCE_RULE_TEMPORAL_NEXT)),
                DEFAULT_EDGE_CONFIDENCE,
            ));
        }
    }
    out
}

/// Construct a synthetic evidence-witness id from a rule name.
///
/// The witness_typed_edges schema requires an `evidence_witness_id`
/// per edge. For mechanically-derived edges the evidence is the *rule*
/// itself, not a separate Witness row — we mint a deterministic id
/// keyed only on the rule name so every edge from the same rule
/// shares the same evidence id, and the storage layer's
/// content-addressed dedup collapses the rows correctly.
///
/// A future "evidence is a real Witness in the table" model would
/// thread the rule-output Witness's id through here; the column is
/// already shaped to carry it. For v1 the synthetic id keeps the
/// schema invariant (`evidence_witness_id` non-null) without forcing
/// us to emit a separate row per derived edge.
fn synthesise_evidence_id(rule_name: &str) -> WitnessId {
    let hash = blake3::hash(rule_name.as_bytes());
    WitnessId(hash.as_bytes().to_owned().try_into().expect("32-byte hash"))
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use thinkingroot_core::types::{
        Confidence, Sensitivity, SourceId, WitnessInput, WitnessSpan, WorkspaceId,
    };

    fn witness(rule: &str, symbol: Option<&str>, source: SourceId, secs: i64) -> Witness {
        let when = Utc.timestamp_opt(secs, 0).unwrap();
        // WitnessId is content-derived from (rule, spans). To give each
        // test fixture a distinct id, derive the span byte range from
        // `secs` so different secs values produce different ids even
        // with the same rule.
        let start = (secs as u64).wrapping_mul(101);
        let end = start + 50;
        let mut w = Witness::new(
            rule.to_string(),
            "documents::heading".to_string(),
            vec![WitnessInput::ByteRef {
                file_blake3: "f".into(),
                start,
                end,
            }],
            vec![WitnessSpan {
                file_blake3: "f".into(),
                start,
                end,
            }],
            source,
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            blake3::hash(format!("bytes-{rule}-{secs}").as_bytes())
                .to_hex()
                .to_string(),
            when,
        );
        w.symbol = symbol.map(|s| s.to_string());
        w.created_at = when;
        w.valid_from = when;
        w
    }

    #[test]
    fn heading_related_emits_edge_between_same_heading_text() {
        let s = SourceId::new();
        let a = witness(RULE_HEADING, Some("Architecture"), s, 1000);
        let b = witness(RULE_HEADING, Some("Architecture"), s, 2000);
        let c = witness(RULE_HEADING, Some("Different Heading"), s, 3000);
        let edges = derive_heading_related(&[a, b, c]);
        assert_eq!(edges.len(), 1, "exactly one Related edge for the pair");
        assert_eq!(edges[0].2, EDGE_RELATED);
        assert!(
            edges[0].3.is_some(),
            "evidence id must be set for mechanical edges"
        );
    }

    #[test]
    fn heading_related_skips_empty_symbol() {
        let s = SourceId::new();
        let a = witness(RULE_HEADING, None, s, 1000);
        let b = witness(RULE_HEADING, Some(""), s, 2000);
        let c = witness(RULE_HEADING, Some(""), s, 3000);
        let edges = derive_heading_related(&[a, b, c]);
        assert!(
            edges.is_empty(),
            "empty/missing symbol must not produce edges"
        );
    }

    #[test]
    fn heading_related_emits_n_choose_2_for_n_matches() {
        let s = SourceId::new();
        let a = witness(RULE_HEADING, Some("Intro"), s, 1000);
        let b = witness(RULE_HEADING, Some("Intro"), s, 2000);
        let c = witness(RULE_HEADING, Some("Intro"), s, 3000);
        let edges = derive_heading_related(&[a, b, c]);
        // C(3,2) = 3 pairs.
        assert_eq!(edges.len(), 3);
    }

    #[test]
    fn heading_related_ignores_non_heading_rules() {
        let s = SourceId::new();
        let a = witness("markdown::paragraph@v1", Some("Same Text"), s, 1000);
        let b = witness("markdown::paragraph@v1", Some("Same Text"), s, 2000);
        let edges = derive_heading_related(&[a, b]);
        assert!(edges.is_empty(), "only markdown::heading@v1 matches");
    }

    #[test]
    fn heading_related_direction_is_deterministic() {
        let s = SourceId::new();
        let a = witness(RULE_HEADING, Some("Stable"), s, 1000);
        let b = witness(RULE_HEADING, Some("Stable"), s, 2000);
        let edges_a = derive_heading_related(&[a.clone(), b.clone()]);
        let edges_b = derive_heading_related(&[b, a]);
        assert_eq!(edges_a, edges_b, "input order must not affect output");
    }

    #[test]
    fn temporal_next_chains_commits_by_creation_time() {
        let s = SourceId::new();
        let c1 = witness(RULE_GIT_COMMIT, None, s, 1_000_000);
        let c2 = witness(RULE_GIT_COMMIT, None, s, 1_000_100);
        let c3 = witness(RULE_GIT_COMMIT, None, s, 1_000_200);
        let edges = derive_temporal_next(&[c1.clone(), c3.clone(), c2.clone()]);
        // Three commits chain into two edges: c1→c2, c2→c3.
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].0, c1.id);
        assert_eq!(edges[0].1, c2.id);
        assert_eq!(edges[1].0, c2.id);
        assert_eq!(edges[1].1, c3.id);
        assert!(edges.iter().all(|e| e.2 == EDGE_TEMPORAL_NEXT));
    }

    #[test]
    fn temporal_next_chains_per_source_independently() {
        let s1 = SourceId::new();
        let s2 = SourceId::new();
        let a1 = witness(RULE_GIT_COMMIT, None, s1, 1000);
        let a2 = witness(RULE_GIT_COMMIT, None, s1, 2000);
        let b1 = witness(RULE_GIT_COMMIT, None, s2, 1500);
        let b2 = witness(RULE_GIT_COMMIT, None, s2, 2500);
        let edges = derive_temporal_next(&[a1, a2, b1, b2]);
        // 2 sources × (n-1) edges each = 2 edges total.
        assert_eq!(edges.len(), 2, "no cross-source temporal chain");
    }

    #[test]
    fn temporal_next_skips_single_commit_sources() {
        let s = SourceId::new();
        let only = witness(RULE_GIT_COMMIT, None, s, 1000);
        let edges = derive_temporal_next(&[only]);
        assert!(edges.is_empty(), "1 commit = 0 edges");
    }

    #[test]
    fn temporal_next_ignores_non_commit_rules() {
        let s = SourceId::new();
        let a = witness(RULE_HEADING, Some("X"), s, 1000);
        let b = witness(RULE_HEADING, Some("Y"), s, 2000);
        let edges = derive_temporal_next(&[a, b]);
        assert!(edges.is_empty(), "only git::commit@v1 chains");
    }

    #[test]
    fn temporal_next_ties_break_by_id() {
        let s = SourceId::new();
        // Same created_at — deterministic order must still produce
        // exactly one chain. We construct two distinct Witnesses
        // (different spans → different ids) but stamp identical
        // `created_at` post-hoc so the secondary sort key
        // (`id.to_hex()`) takes over.
        let mut a = witness(RULE_GIT_COMMIT, None, s, 1000);
        let mut b = witness(RULE_GIT_COMMIT, None, s, 2000);
        let same = a.created_at;
        b.created_at = same;
        b.valid_from = same;
        let edges = derive_temporal_next(&[a, b]);
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn aggregator_dedupes_and_returns_union() {
        let s = SourceId::new();
        let h1 = witness(RULE_HEADING, Some("Architecture"), s, 1000);
        let h2 = witness(RULE_HEADING, Some("Architecture"), s, 2000);
        let c1 = witness(RULE_GIT_COMMIT, None, s, 1500);
        let c2 = witness(RULE_GIT_COMMIT, None, s, 2500);
        let edges = derive_all_typed_edges(&[h1, h2, c1, c2]);
        // 1 Related (heading pair) + 1 TemporalNext (commit pair) = 2.
        assert_eq!(edges.len(), 2);
        let types: std::collections::BTreeSet<&str> = edges.iter().map(|e| e.2).collect();
        assert!(types.contains(EDGE_RELATED));
        assert!(types.contains(EDGE_TEMPORAL_NEXT));
    }

    #[test]
    fn synthesise_evidence_id_is_deterministic() {
        let a = synthesise_evidence_id(EVIDENCE_RULE_HEADING_RELATED);
        let b = synthesise_evidence_id(EVIDENCE_RULE_HEADING_RELATED);
        assert_eq!(a, b, "same rule name → same evidence id");
        let c = synthesise_evidence_id(EVIDENCE_RULE_TEMPORAL_NEXT);
        assert_ne!(a, c, "different rule names → different evidence ids");
    }

    #[test]
    fn empty_input_produces_empty_output() {
        let edges = derive_all_typed_edges(&[]);
        assert!(edges.is_empty());
    }
}
