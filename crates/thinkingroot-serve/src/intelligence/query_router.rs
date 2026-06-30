//! Adaptive query routing (roadmap #4) — classify a query so the expensive
//! retrieval paths fire only when they help.
//!
//! The literature is unanimous that graph/multi-hop expansion *hurts* simple
//! single-fact lookups (it injects loosely-related noise the reader then has to
//! ignore) while it's decisive on genuine multi-hop / aggregate questions. So we
//! route: a **Simple** lookup stays on the fast vector path; a **MultiHop** or
//! **Global** query earns the graph-expansion / spreading-activation pass.
//!
//! The classifier is a cheap, deterministic heuristic — **no LLM, no latency**
//! on the hot path (an LLM router would blow the <200ms recall budget). It uses
//! the eval category when one is supplied (LongMemEval labels the type), and
//! falls back to lexical cues on a bare production query. Pure + unit-tested.

/// What kind of retrieval a query wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryClass {
    /// A single-fact lookup ("what is X", "who is Y") — fast vector path only.
    Simple,
    /// Spans/aggregates/links across memories ("how many", "compare", "list all",
    /// "before/after") — earns the graph-expansion multi-hop pass.
    MultiHop,
    /// A corpus-level question ("what is this whole thing about", "main themes") —
    /// wants summary-level retrieval (RAPTOR, when built) + multi-hop.
    Global,
}

/// Lexical cues that a question aggregates / links across multiple memories.
const MULTIHOP_CUES: &[&str] = &[
    "how many", "how often", "list all", "list the", "all of", "each of",
    "compare", "difference between", "versus", " vs ", "both", "between",
    "before", "after", "earlier", "later", "first", "last time", "since",
    "across", "relationship", "related to", "connected", "besides", "other than",
    "in total", "altogether", "count", "which of",
];

/// Lexical cues that a question is corpus-global rather than about one memory.
const GLOBAL_CUES: &[&str] = &[
    "overall", "in general", "main theme", "main topic", "summary of",
    "summarize", "what is this about", "whole", "entire", "everything about",
    "key points", "high level", "high-level",
];

/// Classify a bare query by lexical cues (no category available). Defaults to
/// `Simple` — the safe path that never injects graph noise.
pub fn classify_query(query: &str) -> QueryClass {
    let q = query.to_lowercase();
    if GLOBAL_CUES.iter().any(|c| q.contains(c)) {
        return QueryClass::Global;
    }
    if MULTIHOP_CUES.iter().any(|c| q.contains(c)) {
        return QueryClass::MultiHop;
    }
    QueryClass::Simple
}

/// Route using the eval category when it's a meaningful label, else fall back to
/// the lexical classifier. The single-session categories are pure recall (Simple);
/// multi-session / temporal / knowledge-update inherently span memories (MultiHop).
pub fn route(category: &str, query: &str) -> QueryClass {
    match category.trim().to_lowercase().as_str() {
        "multi-session" | "temporal-reasoning" | "knowledge-update" => QueryClass::MultiHop,
        "single-session-user" | "single-session-assistant" | "single-session-preference" => {
            QueryClass::Simple
        }
        // No / generic category (production) → decide from the query text.
        "" | "general" | "default" | "unknown" => classify_query(query),
        _ => classify_query(query),
    }
}

/// Should the graph-expansion / spreading-activation multi-hop pass run for this
/// query? True only for MultiHop/Global — keeps the fast path noise-free on
/// simple lookups (the "graphs hurt single-hop" finding).
pub fn wants_graph_expansion(category: &str, query: &str) -> bool {
    matches!(route(category, query), QueryClass::MultiHop | QueryClass::Global)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_lookups_stay_simple() {
        assert_eq!(classify_query("What is the capital of France?"), QueryClass::Simple);
        assert_eq!(classify_query("Who is Priya Raman?"), QueryClass::Simple);
        assert_eq!(classify_query("When did she move to Berlin?"), QueryClass::Simple);
    }

    #[test]
    fn aggregate_and_link_questions_are_multihop() {
        assert_eq!(classify_query("How many charity events did I attend?"), QueryClass::MultiHop);
        assert_eq!(classify_query("Compare my Q2 and Q3 goals"), QueryClass::MultiHop);
        assert_eq!(classify_query("List all the people I met in March"), QueryClass::MultiHop);
        assert_eq!(classify_query("What happened before the launch?"), QueryClass::MultiHop);
    }

    #[test]
    fn corpus_questions_are_global() {
        assert_eq!(classify_query("What is this whole project about?"), QueryClass::Global);
        assert_eq!(classify_query("Summarize the main themes"), QueryClass::Global);
    }

    #[test]
    fn category_overrides_query_text() {
        // A multi-session question phrased simply still routes MultiHop by category.
        assert_eq!(route("multi-session", "What did I say?"), QueryClass::MultiHop);
        // A single-session category never earns expansion, even with cue words.
        assert_eq!(route("single-session-user", "list all my preferences"), QueryClass::Simple);
        // No category → fall back to the query classifier.
        assert_eq!(route("", "How many times did I travel?"), QueryClass::MultiHop);
    }

    #[test]
    fn expansion_gate_matches_routing() {
        assert!(wants_graph_expansion("multi-session", "anything"));
        assert!(!wants_graph_expansion("single-session-user", "compare everything"));
        assert!(wants_graph_expansion("", "what is the whole thing about"));
        assert!(!wants_graph_expansion("", "who is Lena Park"));
    }
}
