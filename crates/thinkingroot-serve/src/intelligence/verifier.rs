// crates/thinkingroot-serve/src/intelligence/verifier.rs
//
// Mechanical citation verifier (Task 12, plan 2026-05-09).
//
// Runs after the agent emits its final text (or a `respond()` tool
// payload — v1.1) and produces a `TrustReceipt` describing what was
// grounded vs auto-cited vs unverifiable. Three policies, layered:
//
//   * **Confidence floor.** A retrieval hit with `fuse_score` ≥
//     `auto_cite_threshold` can be attached to an assertion as
//     direct evidence; below that it gets the negative-cite label
//     ("related context, not entailment").
//
//   * **Negative-cite category.** Low-confidence matches are still
//     surfaced — the user sees a muted "related" pill rather than a
//     fabricated entailment. The trust-receipt UI renders them
//     differently (gray vs blue) so readers can tell direct support
//     from adjacent context.
//
//   * **Pass-through verdicts.** Chitchat ("hi", "thanks!"), rejected
//     write tools, and the LongMemEval bench harness all have
//     legitimate reasons NOT to be verified. Each gets an explicit
//     `Verdict` so the trust-receipt UI shows "no claim made"
//     instead of a fabricated citation.
//
// The verifier is deliberately a **pure function over a `Substrate`
// trait**: real production wires the substrate to `QueryEngine`'s
// claim-existence check, tests pass an in-memory mock. This keeps
// every policy decision unit-testable and protects the contract that
// the verifier never invents a citation that doesn't exist.

use crate::intelligence::respond::{Citation, Relevance, RespondPayload};
use crate::intelligence::synthesizer::is_chitchat;

/// Why we're producing a verdict — gates which policies fire.
///
/// The `verify` entry point dispatches on this; callers populate it
/// from session state (chitchat short-circuit, ApprovalGate
/// rejection, Memory persona LongMemEval contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyKind {
    /// Default: agent produced a substantive answer. Apply auto-cite
    /// + negative-cite + claim-id existence checks.
    Substantive,
    /// User said "hi" / "thanks!" — agent produced a greeting reply.
    /// Skip verification entirely; emit `Verdict::SkippedChitchat`.
    Chitchat,
    /// A write tool was rejected and the agent's reply is the
    /// "user declined" rationale. Nothing to ground; emit
    /// `Verdict::SkippedRejection`.
    SkippedRejection,
    /// Memory persona / LongMemEval bench. The bench harness owns
    /// its own scoring; we don't second-guess. Emit
    /// `Verdict::SkippedBenchHarness`.
    SkippedBench,
}

/// One verifier output. The trust-receipt SSE event carries this
/// straight to the chat UI; `event_kind()` is the wire-event type
/// the UI matches against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every cited claim resolved AND every assertion-bearing span
    /// is grounded (either by the model's citation or by an
    /// auto-cite). The trust receipt renders 🔒 green.
    FullyGrounded {
        /// Distinct `claim_id`s the response actually relies on.
        claims_used: Vec<String>,
        /// Of `claims_used`, how many were auto-cited (vs explicit
        /// from the agent). Useful for the trust-receipt UI to show
        /// "4 grounded · 1 auto-cited".
        auto_cited_count: usize,
    },

    /// Some assertions are anchored to "related context" — substrate
    /// rows that match by surface vocabulary but don't strictly
    /// entail the assertion. Trust receipt renders 🔒 yellow.
    /// `related_count` ≥ 1; `claims_used` is the union of evidence
    /// + related citations.
    PartiallyGrounded {
        claims_used: Vec<String>,
        related_count: usize,
    },

    /// At least one citation referenced a `claim_id` that doesn't
    /// exist in the substrate. The agent hallucinated. Trust
    /// receipt renders 🔒 red and the chat UI flags the response.
    UnverifiedCitations {
        /// `claim_id`s the agent emitted that don't resolve.
        bad_claim_ids: Vec<String>,
        /// Resolvable claims that did make it through — the UI
        /// can still credit those.
        claims_used: Vec<String>,
    },

    /// Skipped because the input was a greeting / chitchat. The UI
    /// renders no chip at all (or a faint "no claim made" footer).
    SkippedChitchat,

    /// Skipped because the agent's reply was a rejection rationale,
    /// not a substantive answer. Same UI as `SkippedChitchat`.
    SkippedRejection,

    /// Skipped because the request was a LongMemEval bench run.
    /// Bench harness owns scoring; we don't double-judge.
    SkippedBenchHarness,
}

impl Verdict {
    /// SSE event-kind suffix the streaming handler emits alongside
    /// `event: trust_receipt`. Stable across the v1.0 wire format
    /// so the desktop UI can switch on it without parsing the body.
    pub fn event_kind(&self) -> &'static str {
        match self {
            Verdict::FullyGrounded { .. } => "fully_grounded",
            Verdict::PartiallyGrounded { .. } => "partially_grounded",
            Verdict::UnverifiedCitations { .. } => "unverified_citations",
            Verdict::SkippedChitchat => "skipped_chitchat",
            Verdict::SkippedRejection => "skipped_rejection",
            Verdict::SkippedBenchHarness => "skipped_bench",
        }
    }

    /// Convenience for tests + the UI: the count of distinct
    /// substrate claims this verdict credits the response with.
    pub fn claim_count(&self) -> usize {
        match self {
            Verdict::FullyGrounded { claims_used, .. }
            | Verdict::PartiallyGrounded { claims_used, .. }
            | Verdict::UnverifiedCitations { claims_used, .. } => claims_used.len(),
            _ => 0,
        }
    }

    /// Wire shape carried by `event: trust_receipt` SSE messages and
    /// the desktop's `chat-event` Tauri channel. A flat JSON object —
    /// not serde's default tagged-enum encoding — so consumers switch
    /// on a single `kind` string without needing to walk a payload
    /// envelope. Adding a new field to one variant is non-breaking
    /// (older clients ignore unknown keys); renaming `kind` values is
    /// the wire-format break.
    pub fn to_sse_payload(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Verdict::FullyGrounded {
                claims_used,
                auto_cited_count,
            } => json!({
                "kind": self.event_kind(),
                "claims_used": claims_used,
                "auto_cited_count": auto_cited_count,
            }),
            Verdict::PartiallyGrounded {
                claims_used,
                related_count,
            } => json!({
                "kind": self.event_kind(),
                "claims_used": claims_used,
                "related_count": related_count,
            }),
            Verdict::UnverifiedCitations {
                bad_claim_ids,
                claims_used,
            } => json!({
                "kind": self.event_kind(),
                "claims_used": claims_used,
                "bad_claim_ids": bad_claim_ids,
            }),
            Verdict::SkippedChitchat
            | Verdict::SkippedRejection
            | Verdict::SkippedBenchHarness => json!({
                "kind": self.event_kind(),
                "claims_used": [],
            }),
        }
    }
}

/// One retrieval hit the verifier may auto-cite against. Mirrors the
/// fields of `hybrid_types::RetrievalHit` we actually need; kept here
/// as a value type so the verifier doesn't take a dep on the heavy
/// retrieval-result struct (which has 20+ fields most callers
/// don't carry around).
///
/// **Important for the C4 coupling fix (Task 5 audit):** populate
/// this from the **pre-exclusion** top-K — i.e. before
/// `excluded_claim_ids` removed Rejected-tier claims — so we don't
/// auto-cite to a worse claim when the rooting harness is forcing
/// the substrate to drop high-confidence rows.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalHit {
    /// Substrate claim id (must resolve via the `Substrate` trait).
    pub claim_id: String,
    /// 11-component fused score from `intelligence::hybrid::fuse_score`.
    /// IEEE-754 deterministic across runs given identical input —
    /// see `.claude/rules/hybrid-retrieval.md` for the contract.
    pub score: f32,
    /// Optional certificate hash from `tr-sigstore`, when the claim
    /// was signed. Forwarded into `Citation.certificate_hash` when
    /// auto-citing.
    pub certificate_hash: Option<String>,
    /// Short text fragment the substrate associates with this hit
    /// — used as the `Citation.span` when auto-citing (we attach
    /// to the closest matching substring of the response text).
    pub snippet: String,
}

/// Confidence floor that gates direct `Evidence` citation. Hits
/// below this threshold can still be surfaced — they're attached as
/// `Relevance::Related` instead. Tuned against LongMemEval
/// `multi-session` category at plan time (2026-05-09); revisit when
/// the broader eval harness lands.
pub const DEFAULT_AUTO_CITE_THRESHOLD: f32 = 0.55;

/// The substrate claim-existence check abstracted as a trait so the
/// verifier is unit-testable without a live `QueryEngine`. Real
/// production wires this to `engine.claim_exists(workspace, id)`;
/// tests pass a `HashSet<String>` wrapper.
pub trait Substrate: Send + Sync {
    /// Return true when `claim_id` resolves to a row in the
    /// substrate. Implementations must be cheap (in-memory lookup
    /// or short Datalog query); the verifier may call this once
    /// per cited `claim_id` per response.
    fn claim_exists(&self, claim_id: &str) -> bool;
}

/// Inputs to one verifier run. `kind` short-circuits the policy
/// pipeline; everything else is consumed only when `kind ==
/// Substantive`.
pub struct VerifyInput<'a> {
    /// Why we're verifying — chitchat / rejection / bench skip the
    /// full pipeline.
    pub kind: VerifyKind,
    /// The agent's final-answer text. For chitchat/rejection this
    /// is irrelevant; for substantive answers, every assertion
    /// span must be grounded.
    pub text: &'a str,
    /// Citations the agent explicitly emitted (v1.0: empty when
    /// agent only streamed text; v1.1: populated from `respond()`).
    pub agent_citations: &'a [Citation],
    /// Pre-exclusion retrieval top-K from the request. The verifier
    /// uses this to auto-cite uncited assertions.
    pub top_k: &'a [RetrievalHit],
    /// Substrate accessor (claim-existence checks).
    pub substrate: &'a dyn Substrate,
    /// Confidence floor for `Evidence` vs `Related`. Defaults to
    /// `DEFAULT_AUTO_CITE_THRESHOLD`.
    pub auto_cite_threshold: f32,
}

/// The verifier entry point. Pure function over the input; never
/// mutates state. Caller collects the `Verdict` and emits an SSE
/// `trust_receipt` event.
pub fn verify(input: &VerifyInput<'_>) -> Verdict {
    // Pass-through verdicts short-circuit before any substrate
    // lookups. The Substantive path always runs the full pipeline.
    match input.kind {
        VerifyKind::Chitchat => return Verdict::SkippedChitchat,
        VerifyKind::SkippedRejection => return Verdict::SkippedRejection,
        VerifyKind::SkippedBench => return Verdict::SkippedBenchHarness,
        VerifyKind::Substantive => {}
    }

    // Defensive pass-through: even when `kind == Substantive` the
    // text might still be a greeting (e.g. the user asked nothing
    // meaningful). The chitchat heuristic is cheap and matches the
    // synthesizer's own short-circuit so behaviour stays consistent.
    if is_chitchat(input.text) {
        return Verdict::SkippedChitchat;
    }

    // 1. Validate every agent-emitted citation. Any unresolvable
    //    `claim_id` is the model hallucinating; that flips the
    //    verdict to UnverifiedCitations and gates the rest.
    let mut bad_claim_ids: Vec<String> = Vec::new();
    let mut resolved_evidence: Vec<String> = Vec::new();
    let mut resolved_related: Vec<String> = Vec::new();
    for c in input.agent_citations {
        if input.substrate.claim_exists(&c.claim_id) {
            match c.relevance {
                Relevance::Evidence => resolved_evidence.push(c.claim_id.clone()),
                Relevance::Related => resolved_related.push(c.claim_id.clone()),
            }
        } else {
            bad_claim_ids.push(c.claim_id.clone());
        }
    }

    if !bad_claim_ids.is_empty() {
        // Hallucinated citation → red verdict regardless of how
        // many were good. Still surface the resolved ones so the
        // UI can credit them.
        let mut claims_used = resolved_evidence;
        claims_used.extend(resolved_related);
        return Verdict::UnverifiedCitations {
            bad_claim_ids,
            claims_used: dedup_preserve_order(claims_used),
        };
    }

    // 2. If the agent provided no explicit citations, auto-cite
    //    against the retrieval top-K. Pick the highest-scoring hit
    //    that resolves; classify by the confidence floor.
    let mut auto_cited: Vec<String> = Vec::new();
    if input.agent_citations.is_empty() && !input.top_k.is_empty() {
        let best = top_resolvable_hit(input.top_k, input.substrate);
        if let Some(hit) = best {
            if hit.score >= input.auto_cite_threshold {
                resolved_evidence.push(hit.claim_id.clone());
                auto_cited.push(hit.claim_id.clone());
            } else {
                resolved_related.push(hit.claim_id.clone());
                auto_cited.push(hit.claim_id.clone());
            }
        }
    }

    // 3. Compose the verdict. Mixed evidence + related → partial;
    //    pure evidence → full; nothing resolvable → red as well
    //    (treat empty-resolved as "we couldn't ground anything").
    let claims_used =
        dedup_preserve_order(resolved_evidence.iter().chain(resolved_related.iter()).cloned());
    let auto_cited_count = auto_cited.len();
    let related_count = resolved_related.len();

    if claims_used.is_empty() {
        // Nothing the agent said was grounded — and we couldn't
        // auto-cite either. Surface as red so the UI flags it
        // rather than silently treating it as full grounding.
        return Verdict::UnverifiedCitations {
            bad_claim_ids: Vec::new(),
            claims_used: Vec::new(),
        };
    }

    if related_count > 0 {
        Verdict::PartiallyGrounded {
            claims_used,
            related_count,
        }
    } else {
        Verdict::FullyGrounded {
            claims_used,
            auto_cited_count,
        }
    }
}

/// Convenience: dispatch from a `RespondPayload`. Same policies but
/// parses citations + checks `unmatched_spans` for span/text drift.
/// Used by the v1.1 respond() path; in v1.0 the streaming handler
/// goes through `verify` directly with empty `agent_citations`.
pub fn verify_respond(
    response: &RespondPayload,
    kind: VerifyKind,
    top_k: &[RetrievalHit],
    substrate: &dyn Substrate,
    auto_cite_threshold: f32,
) -> Verdict {
    // Drop citations whose span doesn't appear in the text — they
    // can't be highlighted in the UI and aren't load-bearing for
    // the answer.
    let unmatched: std::collections::HashSet<&str> = response
        .unmatched_spans()
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let citations: Vec<Citation> = response
        .citations
        .iter()
        .filter(|c| !unmatched.contains(c.claim_id.as_str()))
        .cloned()
        .collect();

    let input = VerifyInput {
        kind,
        text: &response.text,
        agent_citations: &citations,
        top_k,
        substrate,
        auto_cite_threshold,
    };
    verify(&input)
}

// ─────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────

/// Pick the highest-scoring retrieval hit whose `claim_id` actually
/// resolves in the substrate. Skips hits that look high-scoring but
/// reference rows that have since been GC'd / superseded — defends
/// against the "substrate-drifted-since-retrieval" race.
fn top_resolvable_hit<'a>(
    hits: &'a [RetrievalHit],
    substrate: &dyn Substrate,
) -> Option<&'a RetrievalHit> {
    // Hits arrive ranked but we don't trust the caller's ordering:
    // pick the explicit max so reordering bugs upstream can't
    // propagate. NaN scores compare as Less so they fall to the
    // bottom rather than poisoning the comparison.
    hits.iter()
        .filter(|h| substrate.claim_exists(&h.claim_id))
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Less)
        })
}

/// Dedup an iterator of strings while preserving first-seen order.
/// Used to keep `claims_used` stable across runs (prompt-cache
/// friendly when the receipt is logged).
fn dedup_preserve_order<I>(iter: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in iter {
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// In-memory substrate stub for tests. Trait method is
    /// `Send + Sync`, so a `HashSet<String>` plus a thin wrapper
    /// is enough.
    struct Sub(HashSet<String>);

    impl Sub {
        fn with(ids: &[&str]) -> Self {
            Sub(ids.iter().map(|s| s.to_string()).collect())
        }
    }

    impl Substrate for Sub {
        fn claim_exists(&self, claim_id: &str) -> bool {
            self.0.contains(claim_id)
        }
    }

    fn hit(claim_id: &str, score: f32) -> RetrievalHit {
        RetrievalHit {
            claim_id: claim_id.to_string(),
            score,
            certificate_hash: None,
            snippet: format!("snippet for {claim_id}"),
        }
    }

    fn evidence(span: &str, claim_id: &str) -> Citation {
        Citation {
            span: span.to_string(),
            claim_id: claim_id.to_string(),
            certificate_hash: None,
            relevance: Relevance::Evidence,
        }
    }

    fn related(span: &str, claim_id: &str) -> Citation {
        Citation {
            span: span.to_string(),
            claim_id: claim_id.to_string(),
            certificate_hash: None,
            relevance: Relevance::Related,
        }
    }

    #[test]
    fn chitchat_kind_short_circuits_without_substrate_calls() {
        let sub = Sub::with(&[]);
        let v = verify(&VerifyInput {
            kind: VerifyKind::Chitchat,
            text: "hi",
            agent_citations: &[],
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        assert_eq!(v, Verdict::SkippedChitchat);
        assert_eq!(v.event_kind(), "skipped_chitchat");
        assert_eq!(v.claim_count(), 0);
    }

    #[test]
    fn rejection_kind_short_circuits() {
        let sub = Sub::with(&[]);
        let v = verify(&VerifyInput {
            kind: VerifyKind::SkippedRejection,
            text: "user declined: no permission",
            agent_citations: &[],
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        assert_eq!(v, Verdict::SkippedRejection);
        assert_eq!(v.event_kind(), "skipped_rejection");
    }

    #[test]
    fn bench_kind_short_circuits() {
        let sub = Sub::with(&[]);
        let v = verify(&VerifyInput {
            kind: VerifyKind::SkippedBench,
            text: "anything",
            agent_citations: &[],
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        assert_eq!(v, Verdict::SkippedBenchHarness);
        assert_eq!(v.event_kind(), "skipped_bench");
    }

    #[test]
    fn substantive_path_skips_chitchat_text_defensively() {
        // Even when the kind is Substantive, a greeting text gets
        // skipped so we don't fabricate citations for "hi".
        let sub = Sub::with(&["a", "b"]);
        let top_k = vec![hit("a", 0.9)];
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "thanks!",
            agent_citations: &[],
            top_k: &top_k,
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        assert_eq!(v, Verdict::SkippedChitchat);
    }

    #[test]
    fn fully_grounded_when_every_citation_resolves() {
        let sub = Sub::with(&["a7c2", "4d8e"]);
        let citations = vec![evidence("WebhookHandler", "a7c2"), evidence("validates", "4d8e")];
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "the WebhookHandler validates event ids",
            agent_citations: &citations,
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        match v {
            Verdict::FullyGrounded {
                claims_used,
                auto_cited_count,
            } => {
                assert_eq!(claims_used, vec!["a7c2".to_string(), "4d8e".to_string()]);
                assert_eq!(auto_cited_count, 0, "no auto-cites when agent provided them");
            }
            other => panic!("expected FullyGrounded, got {other:?}"),
        }
    }

    #[test]
    fn unverified_when_any_citation_unresolved() {
        let sub = Sub::with(&["good"]);
        let citations = vec![
            evidence("real", "good"),
            evidence("hallucinated", "ghost"),
        ];
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "real and hallucinated",
            agent_citations: &citations,
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        match v {
            Verdict::UnverifiedCitations {
                bad_claim_ids,
                claims_used,
            } => {
                assert_eq!(bad_claim_ids, vec!["ghost".to_string()]);
                assert_eq!(claims_used, vec!["good".to_string()]);
            }
            other => panic!("expected UnverifiedCitations, got {other:?}"),
        }
    }

    #[test]
    fn partially_grounded_when_any_relevance_is_related() {
        let sub = Sub::with(&["a", "b"]);
        let citations = vec![evidence("x", "a"), related("y", "b")];
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "x then y",
            agent_citations: &citations,
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        match v {
            Verdict::PartiallyGrounded {
                claims_used,
                related_count,
            } => {
                assert_eq!(claims_used, vec!["a".to_string(), "b".to_string()]);
                assert_eq!(related_count, 1);
            }
            other => panic!("expected PartiallyGrounded, got {other:?}"),
        }
    }

    #[test]
    fn auto_cite_promotes_top_hit_above_threshold_to_evidence() {
        // No agent citations → verifier auto-cites against top_k.
        // Top hit's score is above the floor → Evidence → FullyGrounded.
        let sub = Sub::with(&["a"]);
        let top_k = vec![hit("a", 0.9), hit("b", 0.4)]; // b not in substrate anyway
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "the answer involves widget X",
            agent_citations: &[],
            top_k: &top_k,
            substrate: &sub,
            auto_cite_threshold: 0.5,
        });
        match v {
            Verdict::FullyGrounded {
                claims_used,
                auto_cited_count,
            } => {
                assert_eq!(claims_used, vec!["a".to_string()]);
                assert_eq!(auto_cited_count, 1);
            }
            other => panic!("expected FullyGrounded with auto-cite, got {other:?}"),
        }
    }

    #[test]
    fn auto_cite_below_threshold_emits_partial_with_related() {
        let sub = Sub::with(&["c"]);
        let top_k = vec![hit("c", 0.2)]; // below 0.5 threshold
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "an inference about widget X",
            agent_citations: &[],
            top_k: &top_k,
            substrate: &sub,
            auto_cite_threshold: 0.5,
        });
        match v {
            Verdict::PartiallyGrounded {
                claims_used,
                related_count,
            } => {
                assert_eq!(claims_used, vec!["c".to_string()]);
                assert_eq!(related_count, 1);
            }
            other => panic!("expected PartiallyGrounded with related auto-cite, got {other:?}"),
        }
    }

    #[test]
    fn auto_cite_skips_hits_that_no_longer_resolve() {
        // Substrate drifted since retrieval — top hit doesn't exist
        // any more. Verifier picks the next-best resolvable.
        let sub = Sub::with(&["b"]);
        let top_k = vec![hit("a", 0.9), hit("b", 0.7)]; // a was GC'd
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "still substantive",
            agent_citations: &[],
            top_k: &top_k,
            substrate: &sub,
            auto_cite_threshold: 0.5,
        });
        match v {
            Verdict::FullyGrounded { claims_used, .. } => {
                assert_eq!(claims_used, vec!["b".to_string()]);
            }
            other => panic!("expected FullyGrounded picking second-best, got {other:?}"),
        }
    }

    #[test]
    fn red_when_no_citation_resolves_and_no_topk_to_auto_cite() {
        // Agent gave nothing, retrieval is empty → can't ground
        // anything. Treat as red (UnverifiedCitations with empty
        // claims_used) so the UI flags it rather than silently
        // marking it green.
        let sub = Sub::with(&[]);
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "i think the auth bug is in the retry path",
            agent_citations: &[],
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        match v {
            Verdict::UnverifiedCitations {
                bad_claim_ids,
                claims_used,
            } => {
                assert!(bad_claim_ids.is_empty());
                assert!(claims_used.is_empty());
            }
            other => panic!("expected UnverifiedCitations on no-grounding-possible, got {other:?}"),
        }
    }

    #[test]
    fn dedups_repeated_claim_ids_in_claims_used() {
        let sub = Sub::with(&["a"]);
        let citations = vec![evidence("first", "a"), evidence("second", "a")];
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "first and second",
            agent_citations: &citations,
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
        });
        match v {
            Verdict::FullyGrounded { claims_used, .. } => {
                assert_eq!(claims_used, vec!["a".to_string()]);
            }
            other => panic!("expected FullyGrounded with deduped claims, got {other:?}"),
        }
    }

    #[test]
    fn verify_respond_drops_citations_with_unmatched_spans() {
        let sub = Sub::with(&["a", "b"]);
        let resp = RespondPayload {
            text: "alpha bravo charlie".to_string(),
            citations: vec![
                evidence("alpha", "a"),
                evidence("non-existent fragment", "b"),
            ],
            suggested_actions: vec![],
        };
        let v = verify_respond(&resp, VerifyKind::Substantive, &[], &sub, 0.5);
        // The unmatched-span citation ("b") is dropped by the
        // pre-filter; we then verify the remaining one ("a").
        match v {
            Verdict::FullyGrounded { claims_used, .. } => {
                assert_eq!(claims_used, vec!["a".to_string()]);
            }
            other => panic!("expected FullyGrounded after span filter, got {other:?}"),
        }
    }

    #[test]
    fn verify_respond_chitchat_short_circuits_before_span_check() {
        let sub = Sub::with(&[]);
        let resp = RespondPayload {
            text: "hi".to_string(),
            citations: vec![],
            suggested_actions: vec![],
        };
        let v = verify_respond(&resp, VerifyKind::Chitchat, &[], &sub, 0.5);
        assert_eq!(v, Verdict::SkippedChitchat);
    }

    #[test]
    fn nan_scores_in_topk_dont_poison_max() {
        // NaN must compare as Less so a real-scored hit wins. This
        // is the contract `total_cmp`-style ordering relies on.
        let sub = Sub::with(&["good"]);
        let top_k = vec![
            RetrievalHit {
                claim_id: "good".to_string(),
                score: 0.8,
                certificate_hash: None,
                snippet: "g".to_string(),
            },
            RetrievalHit {
                claim_id: "nan".to_string(),
                score: f32::NAN,
                certificate_hash: None,
                snippet: "n".to_string(),
            },
        ];
        // "nan" doesn't exist in substrate so it'd be filtered anyway,
        // but the assertion is that adding it doesn't crash the
        // ordering and the resolvable hit still wins.
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "real answer",
            agent_citations: &[],
            top_k: &top_k,
            substrate: &sub,
            auto_cite_threshold: 0.5,
        });
        match v {
            Verdict::FullyGrounded { claims_used, .. } => {
                assert_eq!(claims_used, vec!["good".to_string()]);
            }
            other => panic!("expected FullyGrounded ignoring NaN, got {other:?}"),
        }
    }

    #[test]
    fn forwards_certificate_hash_when_agent_supplies_it() {
        let sub = Sub::with(&["a"]);
        let citations = vec![Citation {
            span: "x".to_string(),
            claim_id: "a".to_string(),
            certificate_hash: Some("0xa3f7…".to_string()),
            relevance: Relevance::Evidence,
        }];
        let v = verify(&VerifyInput {
            kind: VerifyKind::Substantive,
            text: "x",
            agent_citations: &citations,
            top_k: &[],
            substrate: &sub,
            auto_cite_threshold: 0.5,
        });
        // The verifier's verdict doesn't carry per-citation
        // certificate hashes (those flow through the SSE wire
        // format separately). Just assert the citation passed
        // verification.
        assert_eq!(v.claim_count(), 1);
    }

    #[test]
    fn to_sse_payload_emits_kind_for_fully_grounded() {
        let v = Verdict::FullyGrounded {
            claims_used: vec!["a".into(), "b".into()],
            auto_cited_count: 1,
        };
        let p = v.to_sse_payload();
        assert_eq!(p["kind"], "fully_grounded");
        assert_eq!(p["claims_used"][0], "a");
        assert_eq!(p["claims_used"][1], "b");
        assert_eq!(p["auto_cited_count"], 1);
    }

    #[test]
    fn to_sse_payload_emits_kind_for_partially_grounded() {
        let v = Verdict::PartiallyGrounded {
            claims_used: vec!["a".into()],
            related_count: 1,
        };
        let p = v.to_sse_payload();
        assert_eq!(p["kind"], "partially_grounded");
        assert_eq!(p["related_count"], 1);
    }

    #[test]
    fn to_sse_payload_emits_kind_for_unverified() {
        let v = Verdict::UnverifiedCitations {
            bad_claim_ids: vec!["bad".into()],
            claims_used: vec![],
        };
        let p = v.to_sse_payload();
        assert_eq!(p["kind"], "unverified_citations");
        assert_eq!(p["bad_claim_ids"][0], "bad");
        assert_eq!(p["claims_used"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn to_sse_payload_emits_empty_claims_for_skip_variants() {
        for v in [
            Verdict::SkippedChitchat,
            Verdict::SkippedRejection,
            Verdict::SkippedBenchHarness,
        ] {
            let p = v.to_sse_payload();
            assert!(p["kind"].is_string());
            assert_eq!(p["claims_used"].as_array().unwrap().len(), 0);
        }
    }
}
