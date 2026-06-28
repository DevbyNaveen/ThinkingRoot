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

        let docs: Vec<Vec<String>> =
            claims.iter().map(|c| tokenise(&c.statement)).collect();
        let stats = CorpusStats::from(&docs);

        let raw_scores: Vec<f32> = docs.iter().map(|d| self.bm25_raw(d, &stats)).collect();
        let normalised = min_max_normalise(&raw_scores);

        for (hit, bm25) in claims.iter_mut().zip(normalised.iter()) {
            hit.relevance = blend(hit.relevance, *bm25);
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

        let docs: Vec<Vec<String>> =
            entities.iter().map(|e| tokenise(&e.name)).collect();
        let stats = CorpusStats::from(&docs);

        let raw_scores: Vec<f32> = docs.iter().map(|d| self.bm25_raw(d, &stats)).collect();
        let normalised = min_max_normalise(&raw_scores);

        for (hit, bm25) in entities.iter_mut().zip(normalised.iter()) {
            hit.relevance = blend(hit.relevance, *bm25);
        }

        entities.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}

/// Lexical RETRIEVAL arm (L2 hybrid): BM25-rank a corpus of `(id, text)` docs
/// against `query`, returning `(id, raw_bm25)` for docs with non-zero term
/// overlap, highest first. Ranks the WHOLE corpus (not just an existing
/// candidate set) so a rare-term fact the dense embedding missed still enters.
pub fn bm25_rank(query: &str, docs: &[(String, String)]) -> Vec<(String, f32)> {
    let r = Reranker::new(query);
    if r.query_terms.is_empty() || docs.is_empty() {
        return Vec::new();
    }
    let toks: Vec<Vec<String>> = docs.iter().map(|(_, t)| tokenise(t)).collect();
    let stats = CorpusStats::from(&toks);
    let mut scored: Vec<(String, f32)> = docs
        .iter()
        .zip(toks.iter())
        .map(|((id, _), d)| (id.clone(), r.bm25_raw(d, &stats)))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// Reciprocal Rank Fusion of several ranked id-lists. `rrf(id) = sum 1/(k+rank)`
/// (rank 0-based). Parameter-free, robust across incomparable score scales
/// (cosine vs BM25); k=60 canonical (Cormack et al. 2009). Ties break first-seen.
pub fn rrf_fuse(lists: &[&[String]], k: f64) -> Vec<String> {
    use std::collections::HashMap;
    let mut score: HashMap<&str, f64> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            let e = score.entry(id.as_str()).or_insert_with(|| {
                order.push(id.as_str());
                0.0
            });
            *e += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    order.sort_by(|a, b| score[b].partial_cmp(&score[a]).unwrap_or(std::cmp::Ordering::Equal));
    order.into_iter().map(String::from).collect()
}

/// Min-max normalisation across a candidate set: maps the highest
/// raw BM25 score to 1.0 and the lowest (≥0) to 0.0.  When every
/// candidate scores zero (no term overlap anywhere) the function
/// returns all zeros — a no-op for the blender.
fn min_max_normalise(raw: &[f32]) -> Vec<f32> {
    let max = raw.iter().cloned().fold(0.0f32, f32::max);
    if max <= 0.0 {
        return vec![0.0; raw.len()];
    }
    raw.iter().map(|s| (s / max).clamp(0.0, 1.0)).collect()
}

// ---------------------------------------------------------------------------
// BM25 helpers (pure functions)
// ---------------------------------------------------------------------------

/// Pre-computed corpus statistics: average document length plus
/// document-frequency for each query term.  Pre-fix the reranker
/// computed BM25 with N=1 (single doc) which collapses IDF to log(1)=0;
/// the previous code papered over that by hard-coding IDF=1.0,
/// reducing BM25 to a pure TF term that overweights generic-but-
/// frequent matches like "the" in queries that survive
/// stop-word filtering only by hyphenation.  Now we compute proper
/// IDF over the candidate document set so a query term that appears
/// in *every* candidate gets a low IDF weight (it doesn't help
/// distinguish), while a term that appears in only one document
/// gets the highest weight.
struct CorpusStats {
    avg_dl: f32,
    n: usize,
    /// Map from query term → number of documents containing it (df).
    /// Computed only for query terms (rarely > 5) so the cost is
    /// O(N · |Q|) which is cheaper than tokenising a corpus index.
    df: std::collections::HashMap<String, usize>,
}

impl CorpusStats {
    fn from(docs: &[Vec<String>]) -> Self {
        let n = docs.len();
        let avg_dl = if n == 0 {
            0.0
        } else {
            docs.iter().map(|d| d.len() as f32).sum::<f32>() / n as f32
        };
        // Build df only for terms that appear in at least one
        // document — the bm25_score loop only reads via .get() so
        // missing keys default-zero correctly.
        let mut df: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for doc in docs {
            // De-dupe per-document to count documents, not occurrences.
            let unique: std::collections::HashSet<&String> = doc.iter().collect();
            for term in unique {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
        }
        Self { avg_dl, n, df }
    }

    /// IDF with the BM25-canonical "+0.5" smoothing to keep the score
    /// non-negative even when a term appears in every doc.
    /// idf = ln( (N - df + 0.5) / (df + 0.5) + 1.0 )
    fn idf(&self, term: &str) -> f32 {
        let df = *self.df.get(term).unwrap_or(&0) as f32;
        let n = self.n as f32;
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    }
}

/// BM25 score for a single document `text` against `self.query_terms`.
///
/// Returns the *raw* BM25 sum — the caller is responsible for
/// normalising across the candidate set via `min_max_normalise` so
/// the blended score lands in [0, 1].  Pre-fix the per-document
/// normaliser divided by `Σ(idf × (K1+1))` over **all** query terms
/// including ones the document didn't match, which caused docs with
/// missing query terms to score arbitrarily low even when they had
/// strong matches on the present terms — the BM25 boost couldn't
/// then beat a slightly-higher initial vector score.
impl Reranker {
    fn bm25_raw(&self, doc_terms: &[String], stats: &CorpusStats) -> f32 {
        let dl = doc_terms.len() as f32;
        let avg_dl = stats.avg_dl.max(1.0);
        let mut score = 0.0f32;
        for qt in &self.query_terms {
            let tf = doc_terms.iter().filter(|t| *t == qt).count() as f32;
            if tf > 0.0 {
                let idf = stats.idf(qt);
                let numerator = tf * (K1 + 1.0);
                let denominator = tf + K1 * (1.0 - B + B * dl / avg_dl);
                score += idf * (numerator / denominator);
            }
        }
        score
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
                valid_from: 0,
            },
            ClaimSearchHit {
                id: "2".into(),
                statement: "Bob likes pizza".into(),
                claim_type: "preference".into(),
                confidence: 0.8,
                source_uri: "test".into(),
                relevance: 0.6, // higher initial vector score
                valid_from: 0,
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
                valid_from: 0,
            },
            ClaimSearchHit {
                id: "2".into(),
                statement: "bar".into(),
                claim_type: "fact".into(),
                confidence: 0.9,
                source_uri: "test".into(),
                relevance: 0.9,
                valid_from: 0,
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
        let docs = vec![
            tokenise("unrelated random words here"),
            tokenise("alice was in paris"),
        ];
        let stats = CorpusStats::from(&docs);
        let score = r.bm25_raw(&docs[0], &stats);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn bm25_idf_downweights_terms_present_in_all_docs() {
        // Regression: pre-fix IDF was hard-coded to 1.0 so a query
        // term appearing in every candidate scored full BM25 weight,
        // even though it has zero discriminative power.  With proper
        // IDF the boost for matching a "common" term must be lower
        // than the boost for matching a "rare" term.
        let r_common = Reranker::new("alice");
        let r_rare = Reranker::new("paris");

        // 5 candidate docs: every doc contains "alice"; only one
        // contains "paris".
        let docs = vec![
            tokenise("alice went home"),
            tokenise("alice cooked dinner"),
            tokenise("alice slept early"),
            tokenise("alice studied hard"),
            tokenise("alice visited paris yesterday"),
        ];
        let stats = CorpusStats::from(&docs);

        // Scoring the 5th doc — it matches both queries, but on the
        // "alice" query it shares the term with everyone (df=5/N=5)
        // while on the "paris" query it's unique (df=1/N=5).
        let alice_score = r_common.bm25_raw(&docs[4], &stats);
        let paris_score = r_rare.bm25_raw(&docs[4], &stats);

        assert!(
            paris_score > alice_score,
            "rare-term match must score higher than common-term match \
             (alice={alice_score}, paris={paris_score})"
        );
    }

    #[test]
    fn min_max_normalise_handles_all_zero() {
        let zeros = min_max_normalise(&[0.0, 0.0, 0.0]);
        assert_eq!(zeros, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn min_max_normalise_maps_max_to_one() {
        let out = min_max_normalise(&[0.5, 1.5, 2.0]);
        assert_eq!(out[2], 1.0);
        assert!((out[1] - 0.75).abs() < 1e-6);
        assert!((out[0] - 0.25).abs() < 1e-6);
    }
}
