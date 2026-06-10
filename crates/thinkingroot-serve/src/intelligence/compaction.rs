// Context compaction on long runs (final-plan §5 input #5).
//
// As an agent conversation grows, the rendered history block grows unbounded
// and eventually dominates the prompt (and busts the cache-stable layout by
// pushing volatile turns ever higher). Anthropic measured ~84% token reduction
// from compacting long runs. This is the STRUCTURAL half: keep the most-recent
// turns that fit a token budget and elide the older middle, marking the gap so
// the model knows context was dropped (not that the conversation restarted).
// Generative summarization of the elided span is a deeper, LLM-dependent
// follow-up; this structural pass needs no model and is exact + testable.

use super::synthesizer::ChatTurn;

/// ~4 chars/token (Claude tokeniser estimate) plus a small per-turn overhead
/// for the rendered `[role]: ` prefix and newline.
fn turn_tokens(t: &ChatTurn) -> usize {
    (t.content.len() / 4) + 4
}

/// Index into `history` from which to KEEP turns so the kept suffix fits
/// `budget_tokens`, prioritising the most recent turns. Returns 0 when the
/// whole history fits (the common case → no compaction). Always keeps at least
/// the final turn, even if it alone exceeds the budget (dropping the live turn
/// would be worse than overflowing slightly).
pub fn history_keep_from(history: &[ChatTurn], budget_tokens: usize) -> usize {
    if history.is_empty() {
        return 0;
    }
    let mut used = 0usize;
    // Walk newest → oldest; the first turn that would overflow is the cut.
    for (offset, turn) in history.iter().rev().enumerate() {
        used += turn_tokens(turn);
        if used > budget_tokens {
            // Keep everything newer than this turn. If the very newest turn
            // already overflowed (offset 0), still keep it (index len-1).
            let keep = offset.max(1);
            return history.len() - keep;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::synthesizer::{ChatRole, ChatTurn};

    fn turn(content: &str) -> ChatTurn {
        ChatTurn {
            role: ChatRole::User,
            content: content.to_string(),
        }
    }

    #[test]
    fn keeps_all_when_within_budget() {
        let h = vec![turn("a"), turn("b"), turn("c")];
        assert_eq!(history_keep_from(&h, 1000), 0);
    }

    #[test]
    fn empty_history_keeps_all() {
        assert_eq!(history_keep_from(&[], 10), 0);
    }

    #[test]
    fn drops_oldest_turns_to_fit_budget() {
        // Each turn ~ 25/4 + 4 ≈ 10 tokens. Budget 25 fits ~2 most-recent.
        let h = vec![
            turn(&"x".repeat(24)),
            turn(&"y".repeat(24)),
            turn(&"z".repeat(24)),
            turn(&"w".repeat(24)),
        ];
        let from = history_keep_from(&h, 25);
        assert!(from > 0, "older turns dropped");
        // Kept suffix is the most recent turns.
        assert_eq!(h[from..].last().unwrap().content, "w".repeat(24));
        // And it actually fits the budget.
        let kept_tokens: usize = h[from..].iter().map(turn_tokens).sum();
        assert!(kept_tokens <= 25, "kept suffix within budget ({kept_tokens})");
    }

    #[test]
    fn always_keeps_at_least_the_final_turn() {
        // A single huge turn that alone exceeds the budget is still kept.
        let h = vec![turn(&"big".repeat(1000))];
        assert_eq!(history_keep_from(&h, 5), 0, "the only turn is kept");
        // Two huge turns, tiny budget → keep just the last.
        let h2 = vec![turn(&"a".repeat(4000)), turn(&"b".repeat(4000))];
        assert_eq!(history_keep_from(&h2, 5), 1, "keep only the most recent");
    }
}
