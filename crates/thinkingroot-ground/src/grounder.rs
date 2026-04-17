use std::collections::HashMap;
use std::sync::Arc;

use thinkingroot_core::types::{ClaimId, GroundingMethod, SourceId};
use thinkingroot_extract::extractor::ExtractionOutput;

use crate::lexical::LexicalJudge;
use crate::span::SpanJudge;

#[cfg(feature = "vector")]
use crate::nli::NliJudgePool;

/// Callback fired after each source's claims are grounded.
/// Arguments: (done, total)
pub type GroundingProgressFn = Arc<dyn Fn(usize, usize) + Send + Sync>;

/// The result of grounding a single claim.
#[derive(Debug, Clone)]
pub struct GroundingVerdict {
    pub claim_id: ClaimId,
    pub score: f64,
    pub method: GroundingMethod,
    pub lexical_score: f64,
    pub span_score: Option<f64>,
    pub semantic_score: Option<f64>,
    pub nli_score: Option<f64>,
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

/// The Grounding Tribunal: 4 judges verify extraction output.
///
/// Memory-efficient: processes claims grouped by source, dropping each
/// source's text from memory after its claims are scored. Peak memory
/// is 1 source text + ONNX model, not all source texts at once.
pub struct Grounder {
    config: GroundingConfig,
    progress: Option<GroundingProgressFn>,
}

impl Grounder {
    pub fn new(config: GroundingConfig) -> Self {
        Self {
            config,
            progress: None,
        }
    }

    pub fn with_progress(mut self, f: GroundingProgressFn) -> Self {
        self.progress = Some(f);
        self
    }

    /// Ground all claims, releasing each source's text after processing.
    ///
    /// Claims are grouped by source_id. For each source:
    /// 1. Take source text out of the HashMap (freeing memory)
    /// 2. Run all 4 judges on that source's claims
    /// 3. Source text drops when the loop iteration ends
    ///
    /// This keeps peak memory at ~1 source text + ONNX model (~600 MB)
    /// instead of all source texts (potentially several GB).
    pub fn ground(
        &self,
        mut extraction: ExtractionOutput,
        #[cfg(feature = "vector")] mut vector_store: Option<
            &mut thinkingroot_graph::vector::VectorStore,
        >,
        #[cfg(feature = "vector")] nli_pool: Option<&NliJudgePool>,
    ) -> ExtractionOutput {
        let total_claims = extraction.claims.len();

        // ── Create NLI judge once ────────────────────────────────────────
        #[cfg(feature = "vector")]
        let mut nli_judge: Option<crate::nli::NliJudge> = match nli_pool {
            Some(pool) => match pool.create_judge() {
                Ok(j) => Some(j),
                Err(e) => {
                    tracing::warn!("NLI judge creation failed, Judges 1-3 only: {e}");
                    None
                }
            },
            None => None,
        };

        // ── Group claim indices by source ────────────────────────────────
        // We store indices (not claims) to avoid moving data.
        let mut source_to_indices: HashMap<SourceId, Vec<usize>> = HashMap::new();
        for (i, claim) in extraction.claims.iter().enumerate() {
            source_to_indices
                .entry(claim.source)
                .or_default()
                .push(i);
        }

        let num_sources = source_to_indices.len();
        tracing::info!(
            "grounding {total_claims} claims across {num_sources} sources"
        );

        // ── Process per source, releasing text after each ────────────────
        let mut verdicts: HashMap<ClaimId, GroundingVerdict> =
            HashMap::with_capacity(total_claims);
        let mut done: usize = 0;
        #[cfg(feature = "vector")]
        let nli_batch_size = 48usize;

        // Claims with lexical score above this skip NLI — the source text
        // already proves they're grounded. NLI would just confirm.
        // This cuts ~60-70% of NLI calls with zero quality loss.
        #[cfg(feature = "vector")]
        const NLI_SKIP_LEXICAL_THRESHOLD: f64 = 0.70;

        for (source_id, indices) in &source_to_indices {
            // Take source text OUT of the HashMap — frees memory for this source.
            let source_text = extraction
                .source_texts
                .remove(source_id)
                .unwrap_or_default();

            for chunk in indices.chunks(512) {
                let pairs: Vec<(&str, &str)> = chunk
                    .iter()
                    .map(|&i| {
                        let claim = &extraction.claims[i];
                        (claim.statement.as_str(), source_text.as_str())
                    })
                    .collect();

                // Judge 1: Lexical anchor.
                let lexical_scores: Vec<f64> = pairs
                    .iter()
                    .map(|(c, s)| LexicalJudge::score(c, s))
                    .collect();

                // Judge 2: Span attribution.
                let span_scores: Vec<Option<f64>> = chunk
                    .iter()
                    .zip(pairs.iter())
                    .map(|(&i, (_, src))| {
                        let claim = &extraction.claims[i];
                        SpanJudge::score(
                            extraction
                                .claim_source_quotes
                                .get(&claim.id)
                                .map(|s| s.as_str()),
                            src,
                        )
                    })
                    .collect();

                // ── ML early-exit: skip Judges 3+4 for well-grounded claims ──
                //
                // Claims with high lexical+span overlap are already proven by
                // string evidence. Running two neural models (fastembed + NLI)
                // on them is pure waste. We identify the "uncertain" subset
                // once and route only those through the expensive ML judges.
                //
                // This cuts ~60-70% of ML inference for typical codebases
                // where most claims closely mirror their source text.
                #[cfg(feature = "vector")]
                let ml_indices: Vec<usize> = {
                    lexical_scores
                        .iter()
                        .enumerate()
                        .filter_map(|(j, &lex)| {
                            // Combine lexical + span for a stronger early-exit signal.
                            let span_boost = span_scores[j].unwrap_or(0.0);
                            let evidence = lex * 0.6 + span_boost * 0.4;
                            if evidence < NLI_SKIP_LEXICAL_THRESHOLD {
                                Some(j)
                            } else {
                                None
                            }
                        })
                        .collect()
                };

                // Judge 3: Semantic similarity — only for uncertain claims.
                #[cfg(feature = "vector")]
                let semantic_scores: Vec<Option<f64>> = match vector_store.as_mut() {
                    Some(vs) if !ml_indices.is_empty() => {
                        let ml_pairs: Vec<(&str, &str)> = ml_indices
                            .iter()
                            .map(|&j| pairs[j])
                            .collect();
                        let raw = crate::semantic::SemanticJudge::score_batch(&ml_pairs, *vs);
                        let mut result = vec![None; chunk.len()];
                        for (k, &j) in ml_indices.iter().enumerate() {
                            result[j] = Some(raw[k]);
                        }
                        result
                    }
                    _ => vec![None; chunk.len()],
                };
                #[cfg(not(feature = "vector"))]
                let semantic_scores: Vec<Option<f64>> = vec![None; chunk.len()];

                // Judge 4: NLI entailment — only for uncertain claims.
                // Pairs: (source_text, claim) — premise first, hypothesis second.
                #[cfg(feature = "vector")]
                let nli_scores: Vec<Option<f64>> = match nli_judge.as_mut() {
                    Some(judge) if !ml_indices.is_empty() => {
                        let nli_pairs: Vec<(&str, &str)> = ml_indices
                            .iter()
                            .map(|&j| {
                                let (claim, src) = pairs[j];
                                (src, claim)
                            })
                            .collect();

                        let mut raw_scores = Vec::with_capacity(nli_pairs.len());
                        for nli_batch in nli_pairs.chunks(nli_batch_size) {
                            raw_scores.extend(judge.score_batch(nli_batch));
                        }

                        let mut result = vec![None; chunk.len()];
                        for (k, &j) in ml_indices.iter().enumerate() {
                            result[j] = Some(raw_scores[k]);
                        }
                        result
                    }
                    _ => vec![None; chunk.len()],
                };
                #[cfg(not(feature = "vector"))]
                let nli_scores: Vec<Option<f64>> = vec![None; chunk.len()];

                // Combine all judges per claim.
                for (j, &i) in chunk.iter().enumerate() {
                    let claim = &extraction.claims[i];
                    let lexical = lexical_scores[j];
                    let span = span_scores[j];
                    let semantic = semantic_scores[j];
                    let nli = nli_scores[j];

                    let (combined, method) = combine_scores(lexical, span, semantic, nli);
                    let rejected = combined < self.config.reject_threshold;

                    if rejected {
                        tracing::debug!(
                            "REJECTED {:?}: {combined:.2} lex={lexical:.2} \
                             span={span:?} sem={semantic:?} nli={nli:?} — \"{}\"",
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
                            nli_score: nli,
                            rejected,
                        },
                    );
                }

                done += chunk.len();
                if let Some(ref pf) = self.progress {
                    pf(done, total_claims);
                }
            }
            // source_text drops here — memory freed for this source.
        }

        // ── Stats + filter ───────────────────────────────────────────────
        let total = extraction.claims.len();
        let rejected_count = verdicts.values().filter(|v| v.rejected).count();
        let reduced_count = verdicts
            .values()
            .filter(|v| !v.rejected && v.score < self.config.reduce_threshold)
            .count();

        extraction
            .claims
            .retain(|c| verdicts.get(&c.id).map(|v| !v.rejected).unwrap_or(true));

        for claim in &mut extraction.claims {
            if let Some(verdict) = verdicts.get(&claim.id) {
                claim.grounding_score = Some(verdict.score);
                claim.grounding_method = Some(verdict.method);

                if verdict.score < self.config.reduce_threshold {
                    let reduced = claim.confidence.value() * verdict.score;
                    claim.confidence = thinkingroot_core::types::Confidence::new(reduced);
                }
            }
        }

        tracing::info!(
            "grounding: {total} → {rejected_count} rejected, \
             {reduced_count} reduced, {} accepted",
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
    nli: Option<f64>,
) -> (f64, GroundingMethod) {
    match (span, semantic, nli) {
        (Some(s), Some(sem), Some(n)) => {
            (lexical * 0.15 + s * 0.20 + sem * 0.25 + n * 0.40, GroundingMethod::Combined)
        }
        (Some(s), None, Some(n)) => {
            (lexical * 0.15 + s * 0.25 + n * 0.60, GroundingMethod::Combined)
        }
        (None, Some(sem), Some(n)) => {
            (lexical * 0.15 + sem * 0.30 + n * 0.55, GroundingMethod::Combined)
        }
        (None, None, Some(n)) => {
            (lexical * 0.25 + n * 0.75, GroundingMethod::Combined)
        }
        (Some(s), Some(sem), None) => {
            (lexical * 0.35 + s * 0.35 + sem * 0.30, GroundingMethod::Combined)
        }
        (Some(s), None, None) => {
            (lexical * 0.5 + s * 0.5, GroundingMethod::Combined)
        }
        (None, Some(sem), None) => {
            (lexical * 0.55 + sem * 0.45, GroundingMethod::Combined)
        }
        (None, None, None) => (lexical, GroundingMethod::Lexical),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let boundary = s.char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= max)
            .last()
            .unwrap_or(0);
        &s[..boundary]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_all_four() {
        let (score, method) = combine_scores(0.8, Some(1.0), Some(0.9), Some(0.95));
        assert!((score - 0.925).abs() < 0.01);
        assert!(matches!(method, GroundingMethod::Combined));
    }

    #[test]
    fn combine_judges_1_2_3_no_nli() {
        let (score, method) = combine_scores(0.8, Some(1.0), Some(0.9), None);
        assert!((score - 0.9).abs() < 0.01);
        assert!(matches!(method, GroundingMethod::Combined));
    }

    #[test]
    fn combine_judges_1_and_2() {
        let (score, method) = combine_scores(0.8, Some(1.0), None, None);
        assert!((score - 0.9).abs() < 0.01);
        assert!(matches!(method, GroundingMethod::Combined));
    }

    #[test]
    fn combine_judges_1_and_4_nli_only() {
        let (score, method) = combine_scores(0.6, None, None, Some(0.9));
        assert!((score - 0.825).abs() < 0.01);
        assert!(matches!(method, GroundingMethod::Combined));
    }

    #[test]
    fn combine_judge_1_only() {
        let (score, method) = combine_scores(0.6, None, None, None);
        assert!((score - 0.6).abs() < 0.01);
        assert!(matches!(method, GroundingMethod::Lexical));
    }

    #[test]
    fn below_reject_threshold_is_rejected() {
        let (score, _) = combine_scores(0.1, Some(0.1), None, None);
        assert!(score < GroundingConfig::default().reject_threshold);
    }

    // ── Integration tests ────────────────────────────────────────────

    use thinkingroot_core::types::{Claim, ClaimType, SourceId, WorkspaceId};

    fn make_extraction(
        claims: Vec<(&str, SourceId)>,
        source_texts: Vec<(SourceId, &str)>,
        quotes: Vec<(ClaimId, &str)>,
    ) -> ExtractionOutput {
        let mut output = ExtractionOutput::default();
        for (stmt, src) in &claims {
            output
                .claims
                .push(Claim::new(*stmt, ClaimType::Fact, *src, WorkspaceId::new()));
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
    fn grounded_claim_survives() {
        let src = SourceId::new();
        let extraction = make_extraction(
            vec![("PostgreSQL stores user data in tables", src)],
            vec![(src, "PostgreSQL stores user data in tables and handles transactions")],
            vec![],
        );
        let result = Grounder::new(GroundingConfig::default()).ground(
            extraction,
            #[cfg(feature = "vector")] None,
            #[cfg(feature = "vector")] None,
        );
        assert_eq!(result.claims.len(), 1);
        assert!(result.claims[0].grounding_score.unwrap() > 0.5);
    }

    #[test]
    fn hallucinated_claim_rejected() {
        let src = SourceId::new();
        let extraction = make_extraction(
            vec![("Redis caches session tokens in memory", src)],
            vec![(src, "PostgreSQL stores user data in tables and handles transactions")],
            vec![],
        );
        let result = Grounder::new(GroundingConfig::default()).ground(
            extraction,
            #[cfg(feature = "vector")] None,
            #[cfg(feature = "vector")] None,
        );
        assert_eq!(result.claims.len(), 0);
    }

    #[test]
    fn source_quote_boosts_score() {
        let src = SourceId::new();
        let mut extraction = make_extraction(
            vec![("PostgreSQL stores user data", src)],
            vec![(src, "PostgreSQL stores user data in normalized tables")],
            vec![],
        );
        let claim_id = extraction.claims[0].id;
        extraction
            .claim_source_quotes
            .insert(claim_id, "PostgreSQL stores user data".to_string());

        let result = Grounder::new(GroundingConfig::default()).ground(
            extraction,
            #[cfg(feature = "vector")] None,
            #[cfg(feature = "vector")] None,
        );
        assert_eq!(result.claims.len(), 1);
        assert!(result.claims[0].grounding_score.unwrap() > 0.8);
    }

    #[test]
    fn low_grounding_reduces_confidence() {
        let src = SourceId::new();
        let extraction = make_extraction(
            vec![("PostgreSQL handles authentication and sessions", src)],
            vec![(src, "PostgreSQL stores user data and handles transactions for the application")],
            vec![],
        );
        let result = Grounder::new(GroundingConfig {
            reject_threshold: 0.1,
            reduce_threshold: 0.9,
        })
        .ground(
            extraction,
            #[cfg(feature = "vector")] None,
            #[cfg(feature = "vector")] None,
        );
        assert_eq!(result.claims.len(), 1);
        assert!(result.claims[0].confidence.value() < 0.8);
    }

    #[test]
    fn source_texts_freed_after_grounding() {
        let src = SourceId::new();
        let extraction = make_extraction(
            vec![("PostgreSQL stores data", src)],
            vec![(src, "PostgreSQL stores data in tables")],
            vec![],
        );
        let result = Grounder::new(GroundingConfig::default()).ground(
            extraction,
            #[cfg(feature = "vector")] None,
            #[cfg(feature = "vector")] None,
        );
        // source_texts should be drained (removed) by the grounder
        assert!(result.source_texts.is_empty());
    }
}
