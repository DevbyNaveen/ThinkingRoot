//! Hybrid Retrieval — public types shared across the 9-layer pipeline.
//!
//! Split out of `engine.rs` to keep that file focused on workspace lifecycle.
//! Re-exported by `engine.rs` so callers see them as `engine::RetrievalHit`,
//! mirroring how `engram.rs` owns RARP types.
//!
//! Spec: `docs/2026-05-02-hybrid-retrieval-spec.md` §3.2 (RetrievalHit), §4.1
//! (RetrievalRequest), §5 (ScoringProfile), §7 (ByteSpanBundle).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thinkingroot_core::types::{AdmissionTier, GroundingMethod, Sensitivity, TrustLevel};

use crate::engine::{
    CallEdge, ClaimSearchHit, CodeMarkerRef, CodeMetricRef, ContradictionRef, DocTagRef,
    EventTriple, GitBlameRef, KnownUnknown, QuantityRef, SourceByteSpan, TestAnnotationRef,
    TrialScores,
};

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Retrieval call input. Carries the natural-language query, optional typed
/// predicates, the caller's clearance set, the scoring profile, and a few
/// boolean toggles that gate which candidates are admitted.
///
/// `now` is reproducibility plumbing: `None` means "use `Utc::now()`"; tests
/// pin a fixed `DateTime<Utc>` to make recency math deterministic.
#[derive(Debug, Clone, Deserialize)]
pub struct RetrievalRequest {
    pub query_text: String,
    #[serde(default)]
    pub typed_predicates: Vec<TypedPredicate>,
    pub session_id: String,
    #[serde(default = "default_clearance")]
    pub clearance: Vec<Sensitivity>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default)]
    pub time_window: Option<(DateTime<Utc>, DateTime<Utc>)>,
    #[serde(default)]
    pub scoring_profile: ScoringProfile,
    #[serde(default)]
    pub require_certificate: bool,
    #[serde(default)]
    pub include_test_origin: bool,
    #[serde(default)]
    pub include_quarantined: bool,
    #[serde(default)]
    pub require_provenance_verified: bool,
    #[serde(default)]
    pub now: Option<DateTime<Utc>>,
    /// Restricts candidates to this claim-id set. Used by AEP composition
    /// (`Engram.cluster_claim_ids`) and by session-scoped legacy callers.
    /// `None` = unscoped.
    #[serde(default)]
    pub scoped_claim_ids: Option<Vec<String>>,
}

fn default_clearance() -> Vec<Sensitivity> {
    vec![Sensitivity::Public]
}

fn default_top_k() -> usize {
    50
}

// ---------------------------------------------------------------------------
// Typed predicates (13 variants — spec §4.1)
// ---------------------------------------------------------------------------

/// Structured filters for the Datalog side of the planner. Multiple
/// predicates combine with AND semantics; OR is intentionally not supported
/// in v1 (spec §17 Q1 — keeping routing rigid for predictable latency).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypedPredicate {
    EntityType { value: String },
    EntityName { value: String },
    ClaimType { value: String },
    SourceTrustAtLeast { value: TrustLevel },
    AuthoredBy { value: String },
    AuthoredAfter { value: DateTime<Utc> },
    InCallGraphOf { entity_name: String, depth: u8 },
    HasDocTag { tag_kind: String, target: Option<String> },
    HasMarker { kinds: Vec<String> },
    QuantityRange { metric: String, min: f64, max: f64 },
    InHeadingPath { path: Vec<String> },
    SupersedesClaim { claim_id: String },
    ReferencedBy { source_id: String },
}

// ---------------------------------------------------------------------------
// Hit
// ---------------------------------------------------------------------------

/// A single ranked retrieval hit. Carries the seed claim's identity plus the
/// full structural enrichment bundle joined across up to 33 substrate tables,
/// with byte-anchored provenance, score breakdown, and caveats.
#[derive(Debug, Clone, Serialize)]
pub struct RetrievalHit {
    // Anchor
    pub claim_id: String,
    pub statement: String,
    pub claim_type: String,

    // Provenance bundle
    pub byte_spans: ByteSpanBundle,
    pub source_blake3s: Vec<String>,
    pub source_authority: TrustLevel,
    pub source_uri: String,

    // Trust diagnostics
    pub admission_tier: AdmissionTier,
    pub trial_scores: Option<TrialScores>,
    pub certificate_hash: Option<String>,
    pub grounding_score: Option<f64>,
    pub grounding_method: Option<GroundingMethod>,

    // Temporal — Unix epoch seconds, matching `claim_temporal.valid_from/until`.
    pub valid_window: (Option<f64>, Option<f64>),
    pub superseded_by_chain: Vec<String>,

    // Lineage
    pub derivation_parents: Vec<String>,
    pub derivation_root: Option<String>,

    // Privacy
    pub sensitivity: Sensitivity,

    // Code-aware enrichment (None for non-code claims)
    pub code_signature: Option<CodeSignatureRef>,
    pub code_metrics: Option<CodeMetricRef>,
    pub callers: Vec<CallEdge>,
    pub callees: Vec<CallEdge>,
    pub doc_tags: Vec<DocTagRef>,
    pub markers: Vec<CodeMarkerRef>,
    pub test_origin: Option<TestAnnotationRef>,
    pub git_blame: Vec<GitBlameRef>,

    // Quantitative + temporal context
    pub quantities: Vec<QuantityRef>,
    pub related_events: Vec<EventTriple>,

    // Cluster context
    pub cluster_contradictions: Vec<ContradictionRef>,
    pub cluster_gaps: Vec<KnownUnknown>,

    // Ranking
    pub fused_score: f32,
    pub score_breakdown: ScoreBreakdown,

    // Caveats
    pub caveats: Vec<RetrievalCaveat>,
}

/// Row projection of `code_signatures` (CCC §4.4). Lives here rather than
/// `engine.rs` because it's only consumed by hybrid retrieval today.
#[derive(Debug, Clone, Serialize)]
pub struct CodeSignatureRef {
    pub claim_id: String,
    pub parameters_json: String,
    pub return_type: String,
    pub visibility: String,
    pub trait_name: String,
}

// ---------------------------------------------------------------------------
// Byte-span bundle (spec §7)
// ---------------------------------------------------------------------------

/// Coalesced byte ranges across every substrate table the hit rests on.
/// The desktop sheet + `tr-render` use this to highlight source bytes.
#[derive(Debug, Clone, Serialize)]
pub struct ByteSpanBundle {
    pub spans_by_source: HashMap<String, Vec<ByteSpan>>,
    pub primary_span: SourceByteSpan,
    pub stitched_byte_count: u64,
    pub row_count_per_table: HashMap<String, u32>,
}

/// One coalesced byte range plus the table names that contributed to it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ByteSpan {
    pub byte_start: u64,
    pub byte_end: u64,
    pub contributed_by: Vec<String>,
}

// ---------------------------------------------------------------------------
// Score breakdown (spec §5.3)
// ---------------------------------------------------------------------------

/// Per-component score values for explainability. Sum equals `fused`
/// (modulo the two penalty subtractions and IEEE 754 rounding — see
/// `score_fusion_is_deterministic_under_input_reordering`).
#[derive(Debug, Clone, Serialize)]
pub struct ScoreBreakdown {
    pub vector: f32,
    pub admission: f32,
    pub trial: f32,
    pub source_authority: f32,
    pub recency: f32,
    pub freshness_penalty: f32,
    pub complexity: f32,
    pub marker: f32,
    pub gap_proximity: f32,
    pub contradiction_penalty: f32,
    pub test_origin_penalty: f32,
    pub fused: f32,
    /// SOTA Lever 1 — normalised cross-encoder relevance score in [0,1]
    /// when `ScoringProfile::use_cross_encoder = true`, else `None`. The
    /// final returned score is `weight * cross_encoder + (1 - weight) * fused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_encoder: Option<f32>,
}

// ---------------------------------------------------------------------------
// Caveats (mirrors AEP `ProbeCaveat` + 1 hybrid-only variant)
// ---------------------------------------------------------------------------

/// Caveats surfaced *inside* the response — never as typed errors. Mirrors
/// `engine::ProbeCaveat` plus the hybrid-only `DroppedQuarantined` variant
/// emitted when the score-fusion gate drops a candidate.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetrievalCaveat {
    StaleRow {
        table: String,
        expected_blake3: String,
        actual_blake3: String,
    },
    UnresolvedContradiction {
        with_claim_id: String,
        explanation: String,
    },
    SupersededByNewerClaim {
        successor_id: String,
    },
    DerivedFromTest {
        framework: String,
    },
    GapAdjacent {
        gap_id: String,
        expected_claim_type: String,
    },
    SensitivityRedaction {
        hidden_field: String,
        required_clearance: Sensitivity,
    },
    LowConfidence {
        measured: f32,
        threshold: f32,
    },
    DroppedQuarantined {
        count: u32,
    },
    BytesUnavailable {
        source_id: String,
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Scoring profile (spec §5.2)
// ---------------------------------------------------------------------------

/// Per-call scoring weights + tunables. `Default::default()` is the balanced
/// profile; named presets (`compliance()`) live in `scoring_profiles.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoringProfile {
    pub w_vector: f32,
    pub w_admission: f32,
    pub w_trial: f32,
    pub w_source_authority: f32,
    pub w_recency: f32,
    pub w_complexity: f32,
    pub w_marker: f32,
    pub w_gap_proximity: f32,
    pub w_contradiction: f32,
    pub w_test_origin_penalty: f32,
    pub recency_half_life_days: f32,
    pub require_rooted_only: bool,
    /// Below this candidate count, the planner forces Datalog-only mode
    /// (spec §17 Q3 — small workspaces don't benefit from vector recall).
    ///
    /// **Default `100` post-Track-32 polish (2026-05-16)** — the original
    /// 500 was set when vector recall was the only post-recall signal and
    /// cold-loading the embed model was a multi-second hit. With the
    /// cross-encoder rerank now default-on (gte-modernbert FP16 cleans up
    /// noisy cosine results in ~250 ms warm) AND the embed model staged
    /// at install time via Track 32's bundle pipeline (no first-run
    /// download), the conservative gate is obsolete. 100 covers the
    /// "user has a real workspace" case while still skipping single-file
    /// hello-worlds where vector cosine genuinely under-performs lexical.
    pub total_candidate_threshold: usize,
    /// SOTA Lever 1 — cross-encoder rerank as the final stage.
    ///
    /// **Default `true` post-Track-32 (2026-05-16)** — the swap from
    /// Jina Turbo (280 MB, 120-200 ms top-20) to gte-reranker-modernbert-base
    /// (300 MB, ~150-250 ms top-20, +1.5-2.5% Hit@1 lift on independent
    /// benchmarks) makes default-on viable: the latency is invisible
    /// behind LLM-streaming TTFT (500 ms-2 s) for every flow except
    /// instant typeahead, which uses `ScoringProfile::instant()` to
    /// explicitly disable.
    ///
    /// Serde-defaults to `false` for **back-compat**: pre-Track-32
    /// `ScoringProfile` JSON round-trips stay byte-equal on the wire
    /// (a v0.9.x daemon receiving a v0.9.y request payload that omits
    /// the field gets the old behaviour, not silent upgrade).
    /// `Default::default()` in-process construction picks up the new
    /// `true` default.
    #[serde(default)]
    pub use_cross_encoder: bool,
    /// Blend weight applied to the cross-encoder score when fusing with the
    /// 11-component pre-rerank score. `0.0` = ignore CE, `1.0` = trust CE
    /// only. Default `0.7` matches OMEGA's published blend coefficient.
    /// Ignored when `use_cross_encoder = false`.
    #[serde(default = "default_cross_encoder_weight")]
    pub cross_encoder_weight: f32,
}

fn default_cross_encoder_weight() -> f32 {
    0.7
}

impl Default for ScoringProfile {
    fn default() -> Self {
        Self {
            w_vector: 0.30,
            w_admission: 0.15,
            w_trial: 0.15,
            w_source_authority: 0.10,
            w_recency: 0.10,
            w_complexity: 0.05,
            w_marker: 0.05,
            w_gap_proximity: 0.05,
            w_contradiction: 0.05,
            w_test_origin_penalty: 0.05,
            recency_half_life_days: 180.0,
            require_rooted_only: false,
            total_candidate_threshold: 100,
            // Track 32 (2026-05-16) flipped this on by default — see field docstring.
            use_cross_encoder: true,
            cross_encoder_weight: 0.7,
        }
    }
}

impl ScoringProfile {
    /// Instant-retrieval profile for typeahead / autocomplete flows
    /// that need `<25 ms p95`. Disables cross-encoder rerank
    /// (the only stage that exceeds the budget) but keeps all
    /// 11 fuse-score components.
    ///
    /// Use when the caller cannot afford the rerank pass — UI search
    /// suggestions, Brain-graph hover hints, low-latency MCP probes.
    /// Every other flow should take `Default::default()`.
    pub fn instant() -> Self {
        Self {
            use_cross_encoder: false,
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Routing shape (which planner branch handled the call)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingShape {
    VectorFirst,
    DatalogFirst,
    Interleaved,
    DatalogOnlyForced,
}

// ---------------------------------------------------------------------------
// Response envelope
// ---------------------------------------------------------------------------

/// Top-level response from `hybrid_retrieve`. `redactions` carries one
/// `SensitivityRedaction` per hit hidden by the clearance gate so callers
/// see "N hits hidden behind clearance" without seeing what was hidden.
#[derive(Debug, Clone, Serialize)]
pub struct HybridResponse {
    pub hits: Vec<RetrievalHit>,
    pub redactions: Vec<RetrievalCaveat>,
    pub routing_shape: RoutingShape,
    pub elapsed_ms: f32,
}

// ---------------------------------------------------------------------------
// Backward-compat shim — `From<RetrievalHit> for ClaimSearchHit`
// ---------------------------------------------------------------------------

/// Maps a `RetrievalHit` down to the legacy 6-field `ClaimSearchHit`
/// preserved by `synthesizer.rs::ask`, `reranker.rs::rerank_claims`, and
/// the `search_claims` MCP tool.
///
/// Confidence mapping: trial-score average when present, otherwise an
/// admission-tier proxy. Pinned by `from_retrieval_hit_*` tests.
impl From<RetrievalHit> for ClaimSearchHit {
    fn from(h: RetrievalHit) -> Self {
        let confidence = h
            .trial_scores
            .as_ref()
            .map(|t| {
                (t.provenance_score
                    + t.contradiction_score
                    + t.predicate_score
                    + t.topology_score
                    + t.temporal_score)
                    / 5.0
            })
            .unwrap_or_else(|| match h.admission_tier {
                AdmissionTier::Rooted => 1.0,
                AdmissionTier::Attested => 0.7,
                AdmissionTier::Quarantined | AdmissionTier::Rejected => 0.0,
            });
        Self {
            id: h.claim_id,
            statement: h.statement,
            claim_type: h.claim_type,
            confidence,
            source_uri: h.source_uri,
            relevance: h.fused_score,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SourceByteSpan;

    fn empty_bundle() -> ByteSpanBundle {
        ByteSpanBundle {
            spans_by_source: HashMap::new(),
            primary_span: SourceByteSpan {
                source_id: String::new(),
                byte_start: 0,
                byte_end: 0,
            },
            stitched_byte_count: 0,
            row_count_per_table: HashMap::new(),
        }
    }

    fn hit_with(admission: AdmissionTier, trial: Option<TrialScores>) -> RetrievalHit {
        RetrievalHit {
            claim_id: "c-1".into(),
            statement: "x".into(),
            claim_type: "fact".into(),
            byte_spans: empty_bundle(),
            source_blake3s: vec![],
            source_authority: TrustLevel::Trusted,
            source_uri: "u".into(),
            admission_tier: admission,
            trial_scores: trial,
            certificate_hash: None,
            grounding_score: None,
            grounding_method: None,
            valid_window: (None, None),
            superseded_by_chain: vec![],
            derivation_parents: vec![],
            derivation_root: None,
            sensitivity: Sensitivity::Public,
            code_signature: None,
            code_metrics: None,
            callers: vec![],
            callees: vec![],
            doc_tags: vec![],
            markers: vec![],
            test_origin: None,
            git_blame: vec![],
            quantities: vec![],
            related_events: vec![],
            cluster_contradictions: vec![],
            cluster_gaps: vec![],
            fused_score: 0.42,
            score_breakdown: ScoreBreakdown {
                vector: 0.0,
                admission: 0.0,
                trial: 0.0,
                source_authority: 0.0,
                recency: 0.0,
                freshness_penalty: 1.0,
                complexity: 0.0,
                marker: 0.0,
                gap_proximity: 0.0,
                contradiction_penalty: 0.0,
                test_origin_penalty: 0.0,
                fused: 0.42,
                cross_encoder: None,
            },
            caveats: vec![],
        }
    }

    #[test]
    fn retrieval_request_default_clearance_is_public_only() {
        let json = r#"{"query_text":"x","session_id":"s"}"#;
        let req: RetrievalRequest = serde_json::from_str(json).expect("parse");
        assert_eq!(req.clearance, vec![Sensitivity::Public]);
        assert_eq!(req.top_k, 50);
        assert!(!req.require_certificate);
        assert!(!req.include_test_origin);
        assert!(!req.include_quarantined);
        assert!(!req.require_provenance_verified);
    }

    #[test]
    fn from_retrieval_hit_for_claim_search_hit_uses_trial_aggregate() {
        let trial = TrialScores {
            provenance_score: 1.0,
            contradiction_score: 0.5,
            predicate_score: 0.5,
            topology_score: 0.5,
            temporal_score: 0.5,
        };
        let hit = hit_with(AdmissionTier::Quarantined, Some(trial));
        let legacy: ClaimSearchHit = hit.into();
        // (1.0 + 0.5*4) / 5 = 0.6
        assert!((legacy.confidence - 0.6).abs() < 1e-9);
        assert_eq!(legacy.relevance, 0.42);
        assert_eq!(legacy.id, "c-1");
    }

    #[test]
    fn from_retrieval_hit_for_claim_search_hit_falls_back_to_admission_tier_score() {
        let rooted: ClaimSearchHit = hit_with(AdmissionTier::Rooted, None).into();
        assert!((rooted.confidence - 1.0).abs() < 1e-9);

        let attested: ClaimSearchHit = hit_with(AdmissionTier::Attested, None).into();
        assert!((attested.confidence - 0.7).abs() < 1e-9);

        let quar: ClaimSearchHit = hit_with(AdmissionTier::Quarantined, None).into();
        assert!((quar.confidence - 0.0).abs() < 1e-9);

        let rej: ClaimSearchHit = hit_with(AdmissionTier::Rejected, None).into();
        assert!((rej.confidence - 0.0).abs() < 1e-9);
    }

    #[test]
    fn scoring_profile_default_weights_sum_to_one_within_epsilon() {
        let p = ScoringProfile::default();
        // Positive components only; penalties (`w_contradiction`,
        // `w_test_origin_penalty`) are subtracted, not added, so they don't
        // count toward the unit-sum invariant the doc claims.
        let sum = p.w_vector
            + p.w_admission
            + p.w_trial
            + p.w_source_authority
            + p.w_recency
            + p.w_complexity
            + p.w_marker
            + p.w_gap_proximity;
        // 0.30 + 0.15·2 + 0.10·2 + 0.05·3 = 0.95. The 5% headroom is
        // intentional — penalty subtractions can never push fused below 0.
        assert!((sum - 0.95).abs() < 1e-5, "positive sum: {sum}");
        assert_eq!(p.total_candidate_threshold, 100);
        assert_eq!(p.recency_half_life_days, 180.0);
        assert!(!p.require_rooted_only);
    }

    #[test]
    fn typed_predicate_round_trips_through_serde() {
        let preds = vec![
            TypedPredicate::EntityType { value: "Service".into() },
            TypedPredicate::QuantityRange {
                metric: "rps".into(),
                min: 100.0,
                max: 1000.0,
            },
            TypedPredicate::HasMarker {
                kinds: vec!["TODO".into(), "FIXME".into()],
            },
        ];
        let json = serde_json::to_string(&preds).expect("ser");
        let back: Vec<TypedPredicate> = serde_json::from_str(&json).expect("de");
        assert_eq!(preds, back);
    }
}
