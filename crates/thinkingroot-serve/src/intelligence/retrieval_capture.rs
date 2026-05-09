// crates/thinkingroot-serve/src/intelligence/retrieval_capture.rs
//
// Streaming-time collector that watches `AgentEvent::ToolCallFinished`
// payloads from retrieval-shaped tools and folds them into the
// `RetrievalHit` shape the post-stream verifier consumes.
//
// The agent loop is the only place that knows which tool calls
// happened on a given response — capturing here means we don't need
// the verifier to re-run any retrieval at trust-receipt emit time.
// All inputs are already on the wire; we just parse them.
//
// Two retrieval-shaped tools live in the agent's builtin registry
// (intelligence/builtin_tools.rs) today:
//
//   * `search_claims` — returns JSON `{"claims":[{"id","stmt",
//     "confidence","relevance",…}]}`. Score source: `relevance`
//     (the hybrid-fused score from `engine.search`'s ranking).
//
//   * `search` — returns prose-formatted text mixing entities + claims;
//     deliberately NOT structured because that tool is the
//     human-readable variant. We skip it for retrieval capture.
//
// And one MCP-only tool not in the agent registry (so it never shows
// up here in v1.0, but the parser still understands its shape so the
// future agent + MCP++ work doesn't need a second module):
//
//   * `hybrid_retrieve` — returns `HybridResponse` JSON with
//     `hits: [{"claim_id","fused_score","certificate_hash","statement",…}]`.
//
// All parse paths are best-effort: a malformed payload yields zero
// hits, never an error. Tool errors (`is_error: true`) are skipped
// entirely.

use std::collections::HashSet;

use crate::intelligence::verifier::{RetrievalHit, Substrate};

/// Streaming-time collector for retrieval results emitted by the
/// agent. One instance per agent run; folded into a `Vec<RetrievalHit>`
/// at the end via `into_hits`.
///
/// De-duplicates by `claim_id`: if the agent calls `search_claims`
/// twice on overlapping queries, the higher-scoring duplicate wins.
/// This matches the verifier's semantics — the auto-cite picks the
/// top-scoring resolvable hit, so keeping the best score per claim
/// preserves the behaviour the verifier would produce against an
/// un-deduped feed.
#[derive(Debug, Default)]
pub struct RetrievalCapture {
    /// claim_id → (best score so far, hit). Insertion-ordered isn't
    /// load-bearing — the verifier sorts by score before picking.
    by_id: std::collections::HashMap<String, RetrievalHit>,
}

impl RetrievalCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one `AgentEvent::ToolCallFinished` payload. Tool errors
    /// and unrecognised tool names are silently ignored — the capture
    /// is an opportunistic feed, not an authority.
    pub fn observe_tool_finished(&mut self, name: &str, content: &str, is_error: bool) {
        if is_error {
            return;
        }
        match name {
            "search_claims" => self.parse_search_claims(content),
            "hybrid_retrieve" => self.parse_hybrid_retrieve(content),
            _ => {}
        }
    }

    /// Drain the collector into a Vec sorted by descending score —
    /// the shape the verifier expects. (Verifier picks the
    /// highest-scoring resolvable hit; a sorted feed lets it stop at
    /// the first match instead of scanning the whole vec.)
    pub fn into_hits(self) -> Vec<RetrievalHit> {
        let mut v: Vec<RetrievalHit> = self.by_id.into_values().collect();
        // Highest first. NaN-safe: NaN scores sink to the end.
        v.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    /// Borrow view of every captured `claim_id`. Used by the SSE
    /// handler to build the batch existence query before constructing
    /// the substrate.
    pub fn claim_ids(&self) -> impl Iterator<Item = &String> {
        self.by_id.keys()
    }

    /// True when nothing was captured. Lets the SSE handler skip the
    /// substrate batch query entirely on chitchat / no-retrieval runs.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    fn upsert(&mut self, hit: RetrievalHit) {
        match self.by_id.get(&hit.claim_id) {
            Some(existing) if existing.score >= hit.score => {}
            _ => {
                self.by_id.insert(hit.claim_id.clone(), hit);
            }
        }
    }

    fn parse_search_claims(&mut self, content: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
            return;
        };
        let Some(arr) = v.get("claims").and_then(|c| c.as_array()) else {
            return;
        };
        for c in arr {
            let Some(claim_id) = c.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            // Prefer `relevance` (the search ranking score) over `confidence`
            // (per-claim trust score). Either is acceptable — `relevance` is
            // the more semantically right input for an auto-cite floor.
            let score = c
                .get("relevance")
                .and_then(|v| v.as_f64())
                .or_else(|| c.get("confidence").and_then(|v| v.as_f64()))
                .unwrap_or(0.0) as f32;
            let snippet = c
                .get("stmt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            self.upsert(RetrievalHit {
                claim_id: claim_id.to_string(),
                score,
                certificate_hash: None,
                snippet,
            });
        }
    }

    fn parse_hybrid_retrieve(&mut self, content: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
            return;
        };
        let Some(arr) = v.get("hits").and_then(|h| h.as_array()) else {
            return;
        };
        for h in arr {
            let Some(claim_id) = h.get("claim_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let score = h
                .get("fused_score")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32;
            let certificate_hash = h
                .get("certificate_hash")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let snippet = h
                .get("statement")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            self.upsert(RetrievalHit {
                claim_id: claim_id.to_string(),
                score,
                certificate_hash,
                snippet,
            });
        }
    }
}

/// Thin `HashSet`-backed `Substrate` for the verifier. Built from the
/// async `engine.claim_exists_batch` lookup so the verifier's policy
/// pipeline can stay sync.
///
/// Only claim_ids in the wrapped set will report `claim_exists == true`;
/// all others are treated as not-resolvable. The set is the authoritative
/// answer for one verifier call — newly-arrived claims won't appear.
/// Acceptable: the verifier runs after the agent's `Done` event, so the
/// substrate snapshot is the freshest one this run will see.
pub struct HashSetSubstrate {
    existing: HashSet<String>,
}

impl HashSetSubstrate {
    pub fn new(existing: HashSet<String>) -> Self {
        Self { existing }
    }

    pub fn empty() -> Self {
        Self {
            existing: HashSet::new(),
        }
    }
}

impl Substrate for HashSetSubstrate {
    fn claim_exists(&self, claim_id: &str) -> bool {
        self.existing.contains(claim_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_unrecognised_tool_names() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished("read_file", "{\"content\":\"...\"}", false);
        assert!(c.is_empty());
    }

    #[test]
    fn ignores_error_results() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished(
            "search_claims",
            "{\"claims\":[{\"id\":\"c1\",\"stmt\":\"x\",\"relevance\":0.9}]}",
            true,
        );
        assert!(c.is_empty());
    }

    #[test]
    fn parses_search_claims_with_relevance() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[{"id":"c1","stmt":"a","relevance":0.9,"confidence":0.5}]}"#,
            false,
        );
        let hits = c.into_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].claim_id, "c1");
        // Relevance must beat confidence when both are present.
        assert!((hits[0].score - 0.9).abs() < 1e-6);
        assert_eq!(hits[0].snippet, "a");
        assert!(hits[0].certificate_hash.is_none());
    }

    #[test]
    fn search_claims_falls_back_to_confidence_when_no_relevance() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[{"id":"c1","stmt":"a","confidence":0.42}]}"#,
            false,
        );
        let hits = c.into_hits();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 0.42).abs() < 1e-6);
    }

    #[test]
    fn parses_hybrid_retrieve_with_certificate_hash() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished(
            "hybrid_retrieve",
            r#"{"hits":[{"claim_id":"c2","statement":"b","fused_score":0.71,"certificate_hash":"abc123"}]}"#,
            false,
        );
        let hits = c.into_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].claim_id, "c2");
        assert!((hits[0].score - 0.71).abs() < 1e-6);
        assert_eq!(hits[0].certificate_hash.as_deref(), Some("abc123"));
        assert_eq!(hits[0].snippet, "b");
    }

    #[test]
    fn dedup_keeps_higher_score() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[{"id":"c1","stmt":"a","relevance":0.4}]}"#,
            false,
        );
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[{"id":"c1","stmt":"a-better","relevance":0.8}]}"#,
            false,
        );
        let hits = c.into_hits();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 0.8).abs() < 1e-6);
        assert_eq!(hits[0].snippet, "a-better");
    }

    #[test]
    fn dedup_keeps_first_when_later_score_is_lower() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[{"id":"c1","stmt":"high","relevance":0.9}]}"#,
            false,
        );
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[{"id":"c1","stmt":"low","relevance":0.2}]}"#,
            false,
        );
        let hits = c.into_hits();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 0.9).abs() < 1e-6);
        assert_eq!(hits[0].snippet, "high");
    }

    #[test]
    fn into_hits_sorts_descending_by_score() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[
                {"id":"low","stmt":"l","relevance":0.1},
                {"id":"high","stmt":"h","relevance":0.9},
                {"id":"mid","stmt":"m","relevance":0.5}
            ]}"#,
            false,
        );
        let hits = c.into_hits();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].claim_id, "high");
        assert_eq!(hits[1].claim_id, "mid");
        assert_eq!(hits[2].claim_id, "low");
    }

    #[test]
    fn nan_score_does_not_panic_in_sort() {
        let mut c = RetrievalCapture::new();
        c.upsert(RetrievalHit {
            claim_id: "n".into(),
            score: f32::NAN,
            certificate_hash: None,
            snippet: "n".into(),
        });
        c.upsert(RetrievalHit {
            claim_id: "ok".into(),
            score: 0.5,
            certificate_hash: None,
            snippet: "ok".into(),
        });
        let hits = c.into_hits();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn malformed_search_claims_payload_yields_no_hits() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished("search_claims", "not-valid-json", false);
        c.observe_tool_finished("search_claims", "{}", false);
        c.observe_tool_finished("search_claims", r#"{"claims":"not-an-array"}"#, false);
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[{"missing-id":true}]}"#,
            false,
        );
        assert!(c.is_empty());
    }

    #[test]
    fn malformed_hybrid_retrieve_payload_yields_no_hits() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished("hybrid_retrieve", "{}", false);
        c.observe_tool_finished("hybrid_retrieve", r#"{"hits":[{"no_id":1}]}"#, false);
        assert!(c.is_empty());
    }

    #[test]
    fn claim_ids_iter_round_trips() {
        let mut c = RetrievalCapture::new();
        c.observe_tool_finished(
            "search_claims",
            r#"{"claims":[{"id":"a","stmt":"","relevance":0.1},{"id":"b","stmt":"","relevance":0.2}]}"#,
            false,
        );
        let mut ids: Vec<&String> = c.claim_ids().collect();
        ids.sort();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "a");
        assert_eq!(ids[1], "b");
    }

    #[test]
    fn hashset_substrate_reports_membership() {
        let mut s = HashSet::new();
        s.insert("c1".to_string());
        let sub = HashSetSubstrate::new(s);
        assert!(sub.claim_exists("c1"));
        assert!(!sub.claim_exists("c2"));
    }

    #[test]
    fn hashset_substrate_empty_treats_everything_as_unknown() {
        let sub = HashSetSubstrate::empty();
        assert!(!sub.claim_exists("anything"));
    }
}
