//! Mechanical citation gate — the anti-hallucination invariant.
//!
//! ThinkingRoot's honesty rule ("no fake data; never claim grounding you
//! don't have") is enforced here as code, not hope. A grounded answer cites
//! claims with inline `[claim:<id>]` markers (and, on the witness-rendering
//! paths, `[[witness:<64-hex>]]`). The model can only legitimately cite a
//! claim it was actually *shown* — the retrieved grounding set. So the gate
//! is precise and simple:
//!
//!   verified  ⇔  the cited id ∈ the set of retrieved claim ids
//!
//! Any cited id NOT in the grounding set is a **fabricated provenance** and
//! is stripped. If the model emitted markers but *none* verify, the answer is
//! refused outright (we replace it with an honest "not enough verified
//! information" rather than ship a confidently-wrong, falsely-cited reply).
//!
//! This id-membership check is the precise specialisation, for an id-keyed
//! marker substrate, of the byte-span interval-overlap technique from
//! *Citation-Grounded Code Comprehension* (Dec 2025): the retrieved unit
//! here *is* the witness, so "does the citation overlap a retrieved span"
//! reduces to "was this witness in the retrieved set" — strictly stronger
//! (exact) and cheaper. Verified citations are then *enriched* with the
//! witness byte anchor (`source_uri` + `[start,end)` + `content_blake3`) so
//! the UI can highlight + tamper-verify the exact source bytes.
//!
//! The gate is **inert when no markers are present** — the Memory /
//! LongMemEval persona emits no `[claim:]` prefixes by contract, so its
//! answers pass through untouched (preserving the 91.2% wire behaviour).

use std::collections::HashSet;

use crate::engine::{ClaimSearchHit, QueryEngine};

/// A verified citation, byte-anchored to its source for UI highlight +
/// tamper verification. `byte_start == byte_end == 0` with an empty
/// `content_blake3` means the id verified by membership but no witness
/// byte-span resolved (source-granular citation) — still honest, just
/// coarser.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Citation {
    pub claim_id: String,
    pub source_uri: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
    pub symbol: Option<String>,
    /// 1.0 = byte-anchored + membership-verified; lower when only
    /// source-granular (member but no resolvable witness span).
    pub confidence: f32,
}

/// The outcome of running the gate over one answer.
#[derive(Debug, Clone, serde::Serialize, Default, PartialEq)]
pub struct CitationOutcome {
    /// Verified citations, in first-occurrence order, de-duplicated.
    pub citations: Vec<Citation>,
    /// Cited ids that were NOT in the grounding set — fabricated
    /// provenance, stripped from the answer's trust surface.
    pub stripped: Vec<String>,
    /// Fraction of emitted markers that verified, in [0,1]. 0 when no
    /// markers were emitted.
    pub answer_confidence: f32,
    /// True when markers were emitted but none verified — the answer
    /// should be refused (replaced with an honest non-answer).
    pub refused: bool,
}

/// Parse every `[claim:<id>]` marker. Tolerant: an id is any non-empty run
/// of characters up to the closing `]` (ULID, hex, or otherwise). Order is
/// preserved; duplicates collapse to first occurrence. Pure, allocation-light.
pub fn parse_claim_markers(text: &str) -> Vec<String> {
    const PREFIX: &str = "[claim:";
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let after = &rest[at + PREFIX.len()..];
        if let Some(close) = after.find(']') {
            let id = after[..close].trim();
            if !id.is_empty() && seen.insert(id.to_string()) {
                out.push(id.to_string());
            }
            rest = &after[close + 1..];
        } else {
            break; // dangling marker (truncated stream) — stop, never loop
        }
    }
    out
}

/// Every distinct cited id in `answer`, across both marker grammars
/// (`[claim:<id>]` and `[[witness:<64-hex>]]`), first-occurrence order.
pub fn parse_all_markers(answer: &str) -> Vec<String> {
    let mut out = parse_claim_markers(answer);
    let mut seen: HashSet<String> = out.iter().cloned().collect();
    for wid in super::citation_markers::extract_witness_citations(answer) {
        let hex = wid.to_hex();
        if seen.insert(hex.clone()) {
            out.push(hex);
        }
    }
    out
}

/// The pure, engine-free core of the gate. Classifies the answer's markers
/// against the grounding set and builds source-granular citations (no byte
/// anchor — that enrichment needs the graph; see [`verify_citations`]).
/// Fully unit-testable without an engine.
pub fn verify_citations_sync(answer: &str, grounding: &[ClaimSearchHit]) -> CitationOutcome {
    let cited = parse_all_markers(answer);
    if cited.is_empty() {
        // No markers → gate inert (Memory/LongMemEval persona path).
        return CitationOutcome::default();
    }

    let allowed: HashSet<&str> = grounding.iter().map(|h| h.id.as_str()).collect();

    let mut citations = Vec::new();
    let mut stripped = Vec::new();
    for id in cited {
        if let Some(hit) = grounding.iter().find(|h| h.id == id) {
            citations.push(Citation {
                claim_id: id,
                source_uri: hit.source_uri.clone(),
                byte_start: 0,
                byte_end: 0,
                content_blake3: String::new(),
                symbol: None,
                // Source-granular until enriched with a witness byte span.
                confidence: 0.75,
            });
        } else {
            debug_assert!(!allowed.contains(id.as_str()));
            stripped.push(id);
        }
    }

    finalize(citations, stripped)
}

/// Full gate: runs the sync core, then enriches each verified citation with
/// its witness byte anchor from the graph (`source_uri`, `[start,end)`,
/// `content_blake3`, `symbol`). Use on the one-shot `/ask` path where the
/// engine handle is still live. The streaming path uses
/// [`verify_citations_sync`] (engine already released).
pub async fn verify_citations(
    engine: &QueryEngine,
    ws: &str,
    answer: &str,
    grounding: &[ClaimSearchHit],
) -> CitationOutcome {
    let mut outcome = verify_citations_sync(answer, grounding);
    for cit in &mut outcome.citations {
        // Best-effort byte-span enrichment; absence keeps the
        // source-granular citation (never fabricate a span).
        if let Ok(spans) = engine.get_witnesses_for_claim(ws, &cit.claim_id).await
            && let Some(sp) = spans.into_iter().next()
        {
            cit.source_uri = sp.source_uri;
            cit.byte_start = sp.byte_start;
            cit.byte_end = sp.byte_end;
            cit.content_blake3 = sp.content_blake3;
            cit.symbol = sp.symbol;
            cit.confidence = 1.0;
        }
    }
    // Recompute answer_confidence after enrichment (confidence may rise).
    let stripped = std::mem::take(&mut outcome.stripped);
    let citations = std::mem::take(&mut outcome.citations);
    finalize(citations, stripped)
}

/// Optional stricter abstention bar (§3 #6 "verified or silent"). When
/// `TR_VERIFY_MIN_CONFIDENCE` is a float in (0,1], abstain whenever the
/// verified fraction is below it — not only when EVERY citation is fabricated.
/// Unset / out-of-range → `None` → the default all-fabricated rule, so the
/// LongMemEval wire contract is unchanged.
fn verify_min_confidence() -> Option<f32> {
    std::env::var("TR_VERIFY_MIN_CONFIDENCE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|m| *m > 0.0 && *m <= 1.0)
}

/// Pure abstention decision — extracted so the policy is unit-testable without
/// touching the environment. With `min_confidence = None` the rule is the
/// original "refuse only when markers were emitted but none verified". With a
/// threshold, refuse whenever the verified fraction is below it.
fn decide_refused(
    total: usize,
    verified_count: usize,
    answer_confidence: f32,
    min_confidence: Option<f32>,
) -> bool {
    if total == 0 {
        return false; // no markers → inert gate, never a refusal
    }
    match min_confidence {
        Some(min) => answer_confidence < min,
        None => verified_count == 0,
    }
}

/// Compute `answer_confidence` + `refused` from the verified/stripped split.
fn finalize(citations: Vec<Citation>, stripped: Vec<String>) -> CitationOutcome {
    let total = citations.len() + stripped.len();
    let answer_confidence = if total == 0 {
        0.0
    } else {
        // Mean per-citation confidence weighted by the verified fraction:
        // a half-fabricated answer scores low even if its real citations
        // are byte-anchored.
        let verified_sum: f32 = citations.iter().map(|c| c.confidence).sum();
        verified_sum / total as f32
    };
    let refused = decide_refused(
        total,
        citations.len(),
        answer_confidence,
        verify_min_confidence(),
    );
    CitationOutcome {
        citations,
        stripped,
        answer_confidence,
        refused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, uri: &str) -> ClaimSearchHit {
        ClaimSearchHit {
            id: id.to_string(),
            statement: format!("statement for {id}"),
            claim_type: "fact".to_string(),
            confidence: 0.9,
            source_uri: uri.to_string(),
            relevance: 0.8,
        }
    }

    #[test]
    fn decide_refused_default_rule_only_when_all_fabricated() {
        // No min threshold: refuse iff markers emitted but none verified.
        assert!(decide_refused(3, 0, 0.0, None), "all fabricated → refuse");
        assert!(!decide_refused(3, 1, 0.33, None), "one verified → keep");
        assert!(!decide_refused(0, 0, 0.0, None), "no markers → inert");
    }

    #[test]
    fn decide_refused_threshold_abstains_below_bar() {
        // §3 #6 strict bar at 0.5: a half-fabricated answer abstains even
        // though one citation verified.
        assert!(decide_refused(4, 2, 0.5 - 0.01, Some(0.5)), "below bar → refuse");
        assert!(!decide_refused(4, 4, 1.0, Some(0.5)), "fully verified → keep");
        assert!(!decide_refused(4, 3, 0.75, Some(0.5)), "above bar → keep");
        // Threshold never refuses a markerless (inert) answer.
        assert!(!decide_refused(0, 0, 0.0, Some(0.9)));
    }

    #[test]
    fn parse_claim_markers_extracts_dedups_preserves_order() {
        let t = "A [claim:c1] B [claim:c2] C [claim:c1] D";
        assert_eq!(parse_claim_markers(t), vec!["c1", "c2"]);
    }

    #[test]
    fn parse_claim_markers_handles_truncated_marker() {
        assert!(parse_claim_markers("dangling [claim:c1").is_empty());
        assert_eq!(parse_claim_markers("[claim:c1] [claim:c2"), vec!["c1"]);
    }

    #[test]
    fn no_markers_is_inert_gate() {
        // Memory-persona answer: no [claim:] prefixes → gate does nothing.
        let g = vec![hit("c1", "a.rs")];
        let out = verify_citations_sync("The answer with no markers.", &g);
        assert_eq!(out, CitationOutcome::default());
        assert!(!out.refused);
    }

    #[test]
    fn strips_hallucinated_citation_keeps_real_one() {
        let g = vec![hit("c1", "a.rs")];
        let out = verify_citations_sync("Foo [claim:c1] bar [claim:c2].", &g);
        assert_eq!(out.citations.len(), 1);
        assert_eq!(out.citations[0].claim_id, "c1");
        assert_eq!(out.citations[0].source_uri, "a.rs");
        assert_eq!(out.stripped, vec!["c2"]);
        assert!(!out.refused);
        // 1 of 2 markers verified.
        assert!((out.answer_confidence - 0.75 / 2.0).abs() < 1e-6);
    }

    #[test]
    fn refuses_when_all_citations_fabricated() {
        let g = vec![hit("c1", "a.rs")];
        let out = verify_citations_sync("Wrong [claim:zzz] and [claim:yyy].", &g);
        assert!(out.citations.is_empty());
        assert_eq!(out.stripped, vec!["zzz", "yyy"]);
        assert!(out.refused, "all-fabricated answer must be refused");
        assert_eq!(out.answer_confidence, 0.0);
    }

    #[test]
    fn all_real_citations_are_not_refused() {
        let g = vec![hit("c1", "a.rs"), hit("c2", "b.rs")];
        let out = verify_citations_sync("[claim:c1] then [claim:c2].", &g);
        assert_eq!(out.citations.len(), 2);
        assert!(out.stripped.is_empty());
        assert!(!out.refused);
        assert!((out.answer_confidence - 0.75).abs() < 1e-6);
    }
}
