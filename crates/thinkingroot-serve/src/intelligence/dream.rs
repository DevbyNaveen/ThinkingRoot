//! §11 #26 — Night Shift dreaming (generative abstraction).
//!
//! `sleep_consolidate` already resolves contradictions + expires stale claims.
//! Dreaming adds the GENERATIVE half: synthesize higher-level
//! insights/playbooks from clusters of existing claims, in a QUARANTINED dream
//! branch (A2 branch isolation), kept only if they pass verify-before-merge.
//! The abstraction uses the workspace's OWN LLM (customer's model — not a new
//! neural model), so it stays inside the "no extra neural brain" line; what's
//! novel is that it's branch-quarantined + verified + provenance-tracked
//! (vs OpenAI Dreaming's opaque, audit-removed version).
//!
//! This module is the pure, testable text core (prompt + parse). The loop
//! (fork → abstract → verify → merge/discard) lives on `QueryEngine::dream`.

/// System prompt for the abstraction step. Forces insight-only output, one per
/// line, grounded in the supplied claims (no fabrication beyond them).
pub const DREAM_SYSTEM: &str = "You are the night-shift consolidator of a memory system. Given a \
set of existing memory claims, synthesize a FEW higher-level INSIGHTS or PLAYBOOKS that the claims \
collectively support — durable generalizations, recurring patterns, or actionable rules. Rules: \
each insight must be entailed by the claims (never invent facts not supported by them); be concise \
and declarative; output ONE insight per line; no numbering, no preamble, no markdown. If the \
claims support no meaningful generalization, output nothing.";

/// Build the user prompt: the claim set the dream abstracts over.
pub fn build_dream_prompt(claims: &[String]) -> String {
    let mut p = String::from("Existing claims:\n");
    for c in claims {
        let c = c.trim();
        if !c.is_empty() {
            p.push_str("- ");
            p.push_str(c);
            p.push('\n');
        }
    }
    p.push_str("\nInsights/playbooks they support:");
    p
}

/// Parse the LLM output into insight statements: one per line, bullet/number
/// markers stripped, blanks + too-short fragments dropped. Bounded to `max`
/// (a dream shouldn't flood the graph). Deterministic + unit-tested.
pub fn parse_dream_insights(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let s = line
            .trim()
            .trim_start_matches(|c: char| c == '-' || c == '*' || c == '•' || c.is_numeric() || c == '.' || c == ')')
            .trim();
        // Drop empties, headers, and trivially short fragments.
        if s.len() < 12 || s.ends_with(':') {
            continue;
        }
        out.push(s.to_string());
        if out.len() >= max {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_lists_claims() {
        let p = build_dream_prompt(&["A uses B".into(), "  ".into(), "B fails on C".into()]);
        assert!(p.contains("- A uses B"));
        assert!(p.contains("- B fails on C"));
        assert!(!p.contains("-   \n"), "blank claim skipped");
        assert!(p.trim_end().ends_with("they support:"));
    }

    #[test]
    fn parse_strips_markers_and_filters() {
        let text = "Insights:\n- Customers who churn stop using exports first\n2. Demos drive Pro upgrades\n* x\n\n• Friday launches need a Thursday freeze";
        let got = parse_dream_insights(text, 10);
        assert_eq!(
            got,
            vec![
                "Customers who churn stop using exports first",
                "Demos drive Pro upgrades",
                "Friday launches need a Thursday freeze",
            ]
        );
        // "Insights:" header dropped (ends with ':'); "x" dropped (too short).
    }

    #[test]
    fn parse_bounds_count_and_handles_empty() {
        assert!(parse_dream_insights("", 5).is_empty());
        let many = (0..20).map(|i| format!("insight number {i} is here")).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_dream_insights(&many, 3).len(), 3);
    }
}
