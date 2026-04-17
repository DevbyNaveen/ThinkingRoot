// crates/thinkingroot-serve/src/intelligence/router.rs
//
// Query Router — classifies incoming queries as Fast or Agentic path.
//
// Fast path (sub-5ms):  vector search + in-memory cache lookups.
// Agentic path (200ms-2s): ReAct loop with event calendar + multi-hop reasoning.
//
// The router is a lightweight keyword-based classifier.  It runs before any
// I/O so it adds < 1µs overhead.  False positives (routing to Agentic when
// Fast would suffice) are harmless — they add latency but not errors.  False
// negatives (routing to Fast when Agentic is needed) produce less accurate
// answers, so the thresholds err on the side of over-routing to Agentic.

use crate::intelligence::session::SessionContext;

/// The retrieval path chosen for a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryPath {
    /// Sub-5ms: served from in-memory cache + vector index.
    Fast,
    /// 200ms-2s: multi-turn ReAct loop with event calendar + graph traversal.
    Agentic,
}

/// Classify a query into a retrieval path.
///
/// The `_session` parameter is reserved for future session-context-aware
/// routing (e.g., routing to Agentic if the entity has >N unseen claims).
pub fn classify_query(q: &str, _session: &SessionContext) -> QueryPath {
    let lower = q.to_lowercase();

    // Temporal signals — require event calendar + date arithmetic.
    const TEMPORAL: &[&str] = &[
        "when ",
        "last ",
        "before ",
        "after ",
        " ago",
        "recently",
        "first time",
        "yesterday",
        "last week",
        "last month",
        "last year",
        "what day",
        "what date",
        "how long ago",
        "since when",
        " between ",
        "in the past",
        "this week",
        "this month",
        // Duration + counting signals
        "how long",
        "how many days",
        "how many months",
        "how many weeks",
        "how many hours",
        "how much time",
        // Ordering signals — require event-date comparison across multiple claims
        "which came first",
        "which happened first",
        "which was first",
        "came first",
        "happened first",
        "attend first",
        "attended first",
        "finish first",
        "finished first",
        "start first",
        "started first",
        "buy first",
        "bought first",
        "get first",
        "got first",
        "set up first",
        "complete first",
        "completed first",
        "take care of first",
        "which did i",
        // Sequence signals
        "first issue",
        "first time",
        "most recently",
        "most recent",
    ];

    // Multi-hop / analytical signals — require graph traversal or synthesis.
    const MULTIHOP: &[&str] = &[
        "relationship between",
        "compare ",
        "why did",
        "what caused",
        "all the times",
        "how often",
        "what changed",
        "what happened",
        "tell me everything",
        "summary of",
        "history of",
        "timeline",
        "sequence of",
        "what has ",
        "what have ",
        // Counting signals — require K=V expansion across all sessions.
        "how many",
        "how much",
        "how often",
        "total number",
        "count of",
        // Preference / recommendation signals — route to Agentic so Turn 4 runs.
        "prefer",
        "favourite",
        "favorite",
        "recommend",
        "gift for",
        "present for",
        "birthday",
        "what should i get",
        "what would",
        "enjoy",
        "what does",
        "what do they",
        "what kind of",
        "what type of",
        "which type",
        "which kind",
    ];

    if TEMPORAL.iter().any(|kw| lower.contains(kw)) {
        return QueryPath::Agentic;
    }
    if MULTIHOP.iter().any(|kw| lower.contains(kw)) {
        return QueryPath::Agentic;
    }

    QueryPath::Fast
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::session::SessionContext;

    fn session() -> SessionContext {
        SessionContext::new("test", "ws")
    }

    #[test]
    fn temporal_query_routes_agentic() {
        assert_eq!(
            classify_query("what did Alice do last week?", &session()),
            QueryPath::Agentic
        );
        assert_eq!(
            classify_query("when did the team launch the feature?", &session()),
            QueryPath::Agentic
        );
    }

    #[test]
    fn multihop_query_routes_agentic() {
        assert_eq!(
            classify_query("what is the relationship between Alice and Bob?", &session()),
            QueryPath::Agentic
        );
        assert_eq!(
            classify_query("timeline of the authentication system", &session()),
            QueryPath::Agentic
        );
    }

    #[test]
    fn simple_lookup_routes_fast() {
        assert_eq!(
            classify_query("what are Alice's skills?", &session()),
            QueryPath::Fast
        );
        assert_eq!(
            classify_query("show me all Fact claims", &session()),
            QueryPath::Fast
        );
    }
}
