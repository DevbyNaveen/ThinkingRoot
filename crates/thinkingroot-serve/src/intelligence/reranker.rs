// crates/thinkingroot-serve/src/intelligence/reranker.rs
//
// Lightweight BM25 reranker for retrieved search results.
//
// Reranking blends the original vector similarity score with a BM25 term-overlap
// score.  This mirrors the cross-encoder reranking step in Chronos (SOTA 95.6%)
// without requiring an additional ML model or inference runtime.
//
// BM25 parameters follow the standard defaults (k1=1.5, b=0.75) used in
// Elasticsearch and the original Robertson et al. paper.
//
// Usage:
//   let reranker = Reranker::new(&query);
//   reranker.rerank_claims(&mut claim_hits);
//   reranker.rerank_entities(&mut entity_hits);

use crate::engine::{ClaimSearchHit, EntitySearchHit};

// BM25 hyperparameters.
const K1: f32 = 1.5;
const B: f32 = 0.75;

// Blending weight: how much BM25 contributes vs. original vector score.
// 0.4 = 40% BM25, 60% vector. Tuned for LongMemEval temporal/preference queries.
const BM25_WEIGHT: f32 = 0.4;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct Reranker {
    query_terms: Vec<String>,
}

impl Reranker {
    /// Build a reranker for `query`. Tokenises and lowercases the query terms.
    pub fn new(query: &str) -> Self {
        Self {
            query_terms: tokenise(query),
        }
    }

    /// Rerank `claims` in-place, blending BM25 score with existing relevance.
    pub fn rerank_claims(&self, claims: &mut [ClaimSearchHit]) {
        if self.query_terms.is_empty() || claims.is_empty() {
            return;
        }

        let avg_dl = claims
            .iter()
            .map(|c| tokenise(&c.statement).len() as f32)
            .sum::<f32>()
            / claims.len() as f32;

        for hit in claims.iter_mut() {
            let bm25 = self.bm25_score(&hit.statement, avg_dl);
            hit.relevance = blend(hit.relevance, bm25);
        }

        claims.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Rerank `entities` in-place, blending BM25 score with existing relevance.
    pub fn rerank_entities(&self, entities: &mut [EntitySearchHit]) {
        if self.query_terms.is_empty() || entities.is_empty() {
            return;
        }

        let avg_dl = entities
            .iter()
            .map(|e| tokenise(&e.name).len() as f32)
            .sum::<f32>()
            / entities.len() as f32;

        for hit in entities.iter_mut() {
            let bm25 = self.bm25_score(&hit.name, avg_dl);
            hit.relevance = blend(hit.relevance, bm25);
        }

        entities.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}

// ---------------------------------------------------------------------------
// BM25 helpers (pure functions)
// ---------------------------------------------------------------------------

/// BM25 score for a single document `text` against `self.query_terms`.
impl Reranker {
    fn bm25_score(&self, text: &str, avg_dl: f32) -> f32 {
        let doc_terms = tokenise(text);
        let dl = doc_terms.len() as f32;
        // N=1 (single document) — IDF collapses to 1.0 for all terms,
        // so this reduces to the term-frequency component of BM25.
        let mut score = 0.0f32;
        for qt in &self.query_terms {
            let tf = doc_terms.iter().filter(|t| *t == qt).count() as f32;
            if tf > 0.0 {
                let numerator = tf * (K1 + 1.0);
                let denominator = tf + K1 * (1.0 - B + B * dl / avg_dl.max(1.0));
                score += numerator / denominator;
            }
        }
        // Normalise to [0, 1] range — max possible score is query_terms.len() * (K1+1)/1
        let max_score = self.query_terms.len() as f32 * (K1 + 1.0);
        if max_score > 0.0 {
            score / max_score
        } else {
            0.0
        }
    }
}

/// Linear blend: (1 - w) * vector + w * bm25.
#[inline]
fn blend(vector_score: f32, bm25_score: f32) -> f32 {
    (1.0 - BM25_WEIGHT) * vector_score + BM25_WEIGHT * bm25_score
}

/// Tokenise text: lowercase, split on non-alphanumeric, drop stop-words and short tokens.
fn tokenise(text: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "could", "should", "may", "might", "shall", "can",
        "to", "of", "in", "for", "on", "at", "by", "from", "with", "as", "and", "or", "but", "not",
        "this", "that", "it", "its", "i", "my", "me", "you", "your", "we",
    ];

    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2 && !STOP_WORDS.contains(t))
        .map(String::from)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reranker_boosts_term_matching_claim() {
        let mut claims = vec![
            ClaimSearchHit {
                id: "1".into(),
                statement: "Alice visited Paris last Tuesday".into(),
                claim_type: "fact".into(),
                confidence: 0.9,
                source_uri: "test".into(),
                relevance: 0.5,
            },
            ClaimSearchHit {
                id: "2".into(),
                statement: "Bob likes pizza".into(),
                claim_type: "preference".into(),
                confidence: 0.8,
                source_uri: "test".into(),
                relevance: 0.6, // higher initial vector score
            },
        ];

        let reranker = Reranker::new("where did Alice visit last Tuesday?");
        reranker.rerank_claims(&mut claims);

        // Alice claim should rank first after reranking despite lower initial score.
        assert_eq!(claims[0].id, "1");
    }

    #[test]
    fn empty_query_leaves_order_unchanged() {
        let mut claims = vec![
            ClaimSearchHit {
                id: "1".into(),
                statement: "foo".into(),
                claim_type: "fact".into(),
                confidence: 0.9,
                source_uri: "test".into(),
                relevance: 0.8,
            },
            ClaimSearchHit {
                id: "2".into(),
                statement: "bar".into(),
                claim_type: "fact".into(),
                confidence: 0.9,
                source_uri: "test".into(),
                relevance: 0.9,
            },
        ];

        let reranker = Reranker::new("");
        reranker.rerank_claims(&mut claims);
        // Order unchanged since no query terms.
        assert_eq!(claims[0].id, "1");
    }

    #[test]
    fn tokenise_removes_stop_words() {
        let tokens = tokenise("the quick brown fox");
        assert!(!tokens.contains(&"the".to_string()));
        assert!(tokens.contains(&"quick".to_string()));
        assert!(tokens.contains(&"brown".to_string()));
        assert!(tokens.contains(&"fox".to_string()));
    }

    #[test]
    fn bm25_score_zero_for_no_overlap() {
        let r = Reranker::new("alice paris tuesday");
        let score = r.bm25_score("unrelated random words here", 5.0);
        assert_eq!(score, 0.0);
    }
}
