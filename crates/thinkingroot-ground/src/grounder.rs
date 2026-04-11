use std::collections::HashMap;

use thinkingroot_core::types::{ClaimId, GroundingMethod};
use thinkingroot_extract::extractor::ExtractionOutput;

use crate::lexical::LexicalJudge;
use crate::span::SpanJudge;

/// The result of grounding a single claim.
#[derive(Debug, Clone)]
pub struct GroundingVerdict {
    pub claim_id: ClaimId,
    pub score: f64,
    pub method: GroundingMethod,
    pub lexical_score: f64,
    pub span_score: Option<f64>,
    pub semantic_score: Option<f64>,
    /// If true, this claim should be rejected (not stored).
    pub rejected: bool,
}

/// Configuration for the grounding system.
pub struct GroundingConfig {
    /// Claims with combined score below this are rejected.
    pub reject_threshold: f64,
    /// Claims with combined score below this get confidence reduced.
    pub reduce_threshold: f64,
}

impl Default for GroundingConfig {
    fn default() -> Self {
        Self {
            reject_threshold: 0.25,
            reduce_threshold: 0.5,
        }
    }
}

/// The Grounding Tribunal: chains 3 judges to verify extraction output.
pub struct Grounder {
    config: GroundingConfig,
    #[cfg(feature = "vector")]
    vector_store: Option<std::sync::Arc<std::sync::Mutex<thinkingroot_graph::vector::VectorStore>>>,
}

impl Grounder {
    pub fn new(config: GroundingConfig) -> Self {
        Self {
            config,
            #[cfg(feature = "vector")]
            vector_store: None,
        }
    }

    /// Attach a vector store for Judge 3 (semantic similarity).
    #[cfg(feature = "vector")]
    pub fn with_vector_store(
        mut self,
        store: std::sync::Arc<std::sync::Mutex<thinkingroot_graph::vector::VectorStore>>,
    ) -> Self {
        self.vector_store = Some(store);
        self
    }

    /// Ground all claims in an extraction output.
    ///
    /// Returns the extraction output with:
    /// - Rejected claims removed
    /// - Surviving claims annotated with grounding_score and grounding_method
    /// - Confidence reduced for low-grounding claims
    pub fn ground(&self, mut extraction: ExtractionOutput) -> ExtractionOutput {
        let source_texts = &extraction.source_texts;
        let source_quotes = &extraction.claim_source_quotes;

        let mut verdicts: HashMap<ClaimId, GroundingVerdict> = HashMap::new();

        for claim in &extraction.claims {
            let source_text = source_texts
                .get(&claim.source)
                .map(|s| s.as_str())
                .unwrap_or("");

            // Judge 1: Lexical anchor.
            let lexical = LexicalJudge::score(&claim.statement, source_text);

            // Judge 2: Span attribution.
            let span = SpanJudge::score(
                source_quotes.get(&claim.id).map(|s| s.as_str()),
                source_text,
            );

            // Judge 3: Semantic similarity (if vector feature enabled).
            #[cfg(feature = "vector")]
            let semantic = self.vector_store.as_ref().and_then(|vs| {
                vs.lock().ok().map(|mut guard| {
                    crate::semantic::SemanticJudge::score(&claim.statement, source_text, &mut guard)
                })
            });
            #[cfg(not(feature = "vector"))]
            let semantic: Option<f64> = None;

            // Combine scores: weighted average of available judges.
            let (combined, method) = combine_scores(lexical, span, semantic);

            let rejected = combined < self.config.reject_threshold;

            if rejected {
                tracing::debug!(
                    "grounding REJECTED claim {:?}: score={combined:.2} lexical={lexical:.2} \
                     span={span:?} semantic={semantic:?} — \"{}\"",
                    claim.id,
                    truncate(&claim.statement, 60),
                );
            }

            verdicts.insert(
                claim.id,
                GroundingVerdict {
                    claim_id: claim.id,
                    score: combined,
                    method,
                    lexical_score: lexical,
                    span_score: span,
                    semantic_score: semantic,
                    rejected,
                },
            );
        }

        // Count stats before filtering.
        let total = extraction.claims.len();
        let rejected_count = verdicts.values().filter(|v| v.rejected).count();
        let reduced_count = verdicts
            .values()
            .filter(|v| !v.rejected && v.score < self.config.reduce_threshold)
            .count();

        // Remove rejected claims.
        extraction.claims.retain(|c| {
            verdicts
                .get(&c.id)
                .map(|v| !v.rejected)
                .unwrap_or(true)
        });

        // Annotate surviving claims with grounding scores.
        for claim in &mut extraction.claims {
            if let Some(verdict) = verdicts.get(&claim.id) {
                claim.grounding_score = Some(verdict.score);
                claim.grounding_method = Some(verdict.method);

                // If grounding is low, reduce confidence.
                if verdict.score < self.config.reduce_threshold {
                    let reduced = claim.confidence.value() * verdict.score;
                    claim.confidence = thinkingroot_core::types::Confidence::new(reduced);
                }
            }
        }

        tracing::info!(
            "grounding complete: {total} claims → {rejected_count} rejected, \
             {reduced_count} confidence-reduced, {} accepted",
            total - rejected_count,
        );

        extraction
    }
}

/// Combine scores from available judges into a single grounding score.
fn combine_scores(
    lexical: f64,
    span: Option<f64>,
    semantic: Option<f64>,
) -> (f64, GroundingMethod) {
    match (span, semantic) {
        // All 3 judges available.
        (Some(s), Some(sem)) => {
            let combined = lexical * 0.35 + s * 0.35 + sem * 0.30;
            (combined, GroundingMethod::Combined)
        }
        // Judges 1 + 2 only.
        (Some(s), None) => {
            let combined = lexical * 0.5 + s * 0.5;
            (combined, GroundingMethod::Combined)
        }
        // Judges 1 + 3 only (no source_quote from LLM).
        (None, Some(sem)) => {
            let combined = lexical * 0.55 + sem * 0.45;
            (combined, GroundingMethod::Combined)
        }
        // Judge 1 only.
        (None, None) => (lexical, GroundingMethod::Lexical),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_all_three() {
        let (score, method) = combine_scores(0.8, Some(1.0), Some(0.9));
        // 0.8*0.35 + 1.0*0.35 + 0.9*0.30 = 0.28 + 0.35 + 0.27 = 0.90
        assert!((score - 0.9).abs() < 0.01);
        assert!(matches!(method, GroundingMethod::Combined));
    }

    #[test]
    fn combine_judges_1_and_2() {
        let (score, method) = combine_scores(0.8, Some(1.0), None);
        // 0.8*0.5 + 1.0*0.5 = 0.4 + 0.5 = 0.9
        assert!((score - 0.9).abs() < 0.01);
        assert!(matches!(method, GroundingMethod::Combined));
    }

    #[test]
    fn combine_judge_1_only() {
        let (score, method) = combine_scores(0.6, None, None);
        assert!((score - 0.6).abs() < 0.01);
        assert!(matches!(method, GroundingMethod::Lexical));
    }

    #[test]
    fn below_reject_threshold_is_rejected() {
        let config = GroundingConfig {
            reject_threshold: 0.25,
            reduce_threshold: 0.5,
        };
        // Score = 0.1 (below 0.25) → rejected
        let (score, _) = combine_scores(0.1, Some(0.1), None);
        assert!(score < config.reject_threshold);
    }

    // ── Integration tests ────────────────────────────────────────────

    use thinkingroot_core::types::{Claim, ClaimType, WorkspaceId, SourceId};

    fn make_extraction(
        claims: Vec<(&str, SourceId)>,
        source_texts: Vec<(SourceId, &str)>,
        quotes: Vec<(ClaimId, &str)>,
    ) -> ExtractionOutput {
        let mut output = ExtractionOutput::default();
        for (stmt, src) in &claims {
            let c = Claim::new(*stmt, ClaimType::Fact, *src, WorkspaceId::new());
            output.claims.push(c);
        }
        for (sid, text) in source_texts {
            output.source_texts.insert(sid, text.to_string());
        }
        for (cid, quote) in quotes {
            output.claim_source_quotes.insert(cid, quote.to_string());
        }
        output
    }

    #[test]
    fn grounded_claim_survives_and_gets_score() {
        let src = SourceId::new();
        let extraction = make_extraction(
            vec![("PostgreSQL stores user data in tables", src)],
            vec![(src, "PostgreSQL stores user data in tables and handles transactions")],
            vec![],
        );
        let grounder = Grounder::new(GroundingConfig::default());
        let result = grounder.ground(extraction);

        assert_eq!(result.claims.len(), 1);
        let claim = &result.claims[0];
        assert!(claim.grounding_score.is_some());
        let score = claim.grounding_score.unwrap();
        assert!(score > 0.5, "well-grounded claim should score > 0.5, got {score}");
    }

    #[test]
    fn hallucinated_claim_rejected() {
        let src = SourceId::new();
        let extraction = make_extraction(
            vec![("Redis caches session tokens in memory", src)],
            vec![(src, "PostgreSQL stores user data in tables and handles transactions")],
            vec![],
        );
        let grounder = Grounder::new(GroundingConfig::default());
        let result = grounder.ground(extraction);

        // With zero word overlap, lexical score ≈ 0.0, which is below reject threshold (0.25)
        assert_eq!(result.claims.len(), 0, "hallucinated claim should be rejected");
    }

    #[test]
    fn source_quote_boosts_grounding_score() {
        let src = SourceId::new();
        let mut extraction = make_extraction(
            vec![("PostgreSQL stores user data", src)],
            vec![(src, "PostgreSQL stores user data in normalized tables")],
            vec![],
        );
        // Add source quote for the claim
        let claim_id = extraction.claims[0].id;
        extraction.claim_source_quotes.insert(
            claim_id,
            "PostgreSQL stores user data".to_string(),
        );

        let grounder = Grounder::new(GroundingConfig::default());
        let result = grounder.ground(extraction);

        assert_eq!(result.claims.len(), 1);
        let score = result.claims[0].grounding_score.unwrap();
        // With both lexical + exact span match, should be very high
        assert!(score > 0.8, "claim with exact quote match should score high, got {score}");
    }

    #[test]
    fn low_grounding_reduces_confidence() {
        let src = SourceId::new();
        // Partial overlap: some words match, some don't
        let extraction = make_extraction(
            vec![("PostgreSQL handles authentication and sessions", src)],
            vec![(src, "PostgreSQL stores user data and handles transactions for the application")],
            vec![],
        );
        let grounder = Grounder::new(GroundingConfig {
            reject_threshold: 0.1, // low threshold so it doesn't get rejected
            reduce_threshold: 0.9, // high threshold so confidence gets reduced
        });
        let result = grounder.ground(extraction);

        assert_eq!(result.claims.len(), 1);
        let claim = &result.claims[0];
        // Original confidence is 0.8 (default from Claim::new)
        // With partial overlap, confidence should be reduced
        assert!(
            claim.confidence.value() < 0.8,
            "low-grounding claim should have reduced confidence, got {}",
            claim.confidence.value()
        );
    }
}
