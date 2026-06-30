//! RAPTOR / community summaries (roadmap #6) — corpus-level theme summaries for
//! GLOBAL questions ("what is this whole thing about", "the main themes").
//!
//! Spreading activation connects *specific* memories across sessions, but it
//! can't answer a question about the corpus as a whole — no single fact contains
//! "the overall picture". RAPTOR fills that: cluster the per-document summaries,
//! write one theme summary per cluster (a recursive abstraction over the docs),
//! and surface those theme summaries when the router classifies a query as
//! Global. Query-type-gated on purpose — community summaries *hurt* single-fact
//! precision (they're abstractions), so they only fire for global questions.
//!
//! This module is the pure core: the clustering and the LLM prompt/parse. The
//! orchestration (read doc summaries → summarize each cluster → store) lives in
//! the engine; gated by `TR_RAPTOR` (default off — adds LLM cost at ingest +
//! is eval-gated like every accuracy layer).

use thinkingroot_llm::llm::LlmClient;

/// Target documents per community cluster.
const CLUSTER_SIZE: usize = 8;
/// Hard cap on clusters (bounds the LLM summarization cost per rebuild).
const MAX_CLUSTERS: usize = 12;

/// Partition `n` document-summary indices into clusters. v1 is deterministic
/// sequential bucketing (cheap, replayable); clustering *quality* — grouping by
/// topic similarity instead of order — is the eval-tunable upgrade. Bounded to
/// [`MAX_CLUSTERS`]; empty input → no clusters.
pub fn cluster_indices(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let n_clusters = n.div_ceil(CLUSTER_SIZE).clamp(1, MAX_CLUSTERS);
    let per = n.div_ceil(n_clusters);
    (0..n_clusters)
        .map(|c| {
            let start = c * per;
            let end = ((c + 1) * per).min(n);
            (start..end).collect::<Vec<usize>>()
        })
        .filter(|v| !v.is_empty())
        .collect()
}

/// System prompt: synthesize ONE theme summary from a set of document summaries.
pub fn community_system() -> String {
    "You write a high-level THEME summary for a knowledge base. You are given a set \
of individual document summaries. Write ONE concise paragraph (2-4 sentences) capturing \
the COMMON themes, entities, and through-lines across these documents — the kind of \
overview that answers \"what is this collection about?\". Generalize; do not just \
concatenate the inputs, and do not invent anything not supported by them. Output ONLY \
the summary paragraph, no preamble, no markdown."
        .to_string()
}

/// Build the user prompt: the document summaries, one per line.
pub fn build_community_prompt(doc_summaries: &[&str]) -> String {
    let body = doc_summaries
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Document summaries:\n{body}\n\nTheme summary:")
}

/// Parse the model's theme summary — it's free prose, so just strip fences and
/// trim. Empty stays empty (the caller skips empty clusters).
pub fn parse_community_summary(resp: &str) -> String {
    resp.trim().trim_matches('`').trim().to_string()
}

/// Summarize one cluster of document summaries into a theme summary. Empty on
/// LLM error (the cluster is simply skipped — never blocks ingest).
pub async fn summarize_cluster(llm: &LlmClient, doc_summaries: &[&str]) -> String {
    if doc_summaries.is_empty() {
        return String::new();
    }
    match llm.chat(&community_system(), &build_community_prompt(doc_summaries)).await {
        Ok(resp) => parse_community_summary(&resp),
        Err(e) => {
            tracing::warn!("raptor: community summary failed ({e})");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clustering_buckets_and_caps() {
        assert!(cluster_indices(0).is_empty());
        // 5 docs → one cluster of 5.
        assert_eq!(cluster_indices(5), vec![vec![0, 1, 2, 3, 4]]);
        // 20 docs → multiple clusters covering every index exactly once.
        let cs = cluster_indices(20);
        assert!(cs.len() >= 2);
        let mut flat: Vec<usize> = cs.iter().flatten().copied().collect();
        flat.sort();
        assert_eq!(flat, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn clustering_respects_max_clusters() {
        let cs = cluster_indices(1000);
        assert!(cs.len() <= MAX_CLUSTERS);
        // Still covers everything.
        let total: usize = cs.iter().map(|c| c.len()).sum();
        assert_eq!(total, 1000);
    }

    #[test]
    fn prompt_embeds_summaries() {
        let p = build_community_prompt(&["Doc about Orion Labs.", "Doc about hiring."]);
        assert!(p.contains("Orion Labs"));
        assert!(p.contains("hiring"));
        assert!(p.contains("Theme summary:"));
    }

    #[test]
    fn parse_strips_fences_and_trims() {
        assert_eq!(parse_community_summary("  `A theme.`  "), "A theme.");
        assert_eq!(parse_community_summary("A plain theme."), "A plain theme.");
    }
}
