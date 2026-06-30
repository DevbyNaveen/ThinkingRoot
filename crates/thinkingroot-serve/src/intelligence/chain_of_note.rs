//! Reader upgrade — **Chain-of-Note (CoN) reading** (the dominant accuracy lever).
//!
//! Instead of answering directly from a pile of retrieved evidence (where one
//! irrelevant-but-similar passage can derail the answer), the model first writes
//! a one-line *relevance note* per evidence item — relevant / partial /
//! irrelevant — and only then synthesizes the answer from the notes it judged
//! relevant. This "note, then answer" pass measurably improves grounded QA
//! (LongMemEval: oracle reading 0.870 → 0.924) and is a pure prompt technique —
//! no model change, no retrieval change.
//!
//! It is **additive and reversible**: this module does NOT touch the byte-pinned
//! `MEMORY_SYSTEM_PROMPT`. When enabled (`TR_CHAIN_OF_NOTE=1`, default off), a
//! CoN directive is appended to the per-question user message and the final
//! answer is extracted from after the `FINAL ANSWER:` marker (the notes are
//! stripped before the answer is returned/cited). Applies to the one-shot
//! synthesis path; streaming is left direct (notes would stream before the
//! answer). The directive builder and the answer extractor are pure + unit-tested.

/// Marker the model must emit before its final answer. Matched case-insensitively
/// so a model that writes "Final Answer:" or "FINAL ANSWER:" both parse.
const ANSWER_MARKER: &str = "FINAL ANSWER:";

/// Is Chain-of-Note reading enabled? Gated by `TR_CHAIN_OF_NOTE` (default OFF —
/// it changes the generation format, so it ships dark and is eval-gated on).
pub fn enabled() -> bool {
    std::env::var("TR_CHAIN_OF_NOTE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// The CoN directive appended to the user message (after the evidence + question).
/// Instructs the note-then-answer pass and pins the `FINAL ANSWER:` marker so the
/// answer is mechanically extractable.
pub fn directive() -> &'static str {
    "\n\nHOW TO ANSWER (think step by step, then answer):\n\
1. For EACH numbered piece of evidence above, write one line: `[n] relevant | partial | irrelevant — <short reason>`. Judge it ONLY by whether it helps answer the question.\n\
2. Then, on a new line, write `FINAL ANSWER:` followed by the answer, using ONLY the evidence you marked relevant or partial. If none of the evidence answers the question, the FINAL ANSWER must say the information is not available."
}

/// Extract the final answer from a Chain-of-Note response: everything after the
/// LAST `FINAL ANSWER:` marker, trimmed. If the marker is absent (a model that
/// ignored the format), return the whole response trimmed — never lose the
/// answer. The relevance notes before the marker are discarded.
pub fn extract_answer(resp: &str) -> String {
    // Find the last case-insensitive occurrence of the marker.
    let lower = resp.to_lowercase();
    let needle = ANSWER_MARKER.to_lowercase();
    match lower.rfind(&needle) {
        Some(idx) => {
            let after = &resp[idx + ANSWER_MARKER.len()..];
            let ans = after.trim();
            if ans.is_empty() {
                resp.trim().to_string()
            } else {
                ans.to_string()
            }
        }
        None => resp.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_answer_after_marker() {
        let resp = "[1] relevant — gives the city\n[2] irrelevant — about food\n\
                    FINAL ANSWER: She lives in Berlin.";
        assert_eq!(extract_answer(resp), "She lives in Berlin.");
    }

    #[test]
    fn case_insensitive_marker() {
        let resp = "[1] relevant\nFinal Answer:  Tokyo";
        assert_eq!(extract_answer(resp), "Tokyo");
    }

    #[test]
    fn uses_last_marker_if_repeated() {
        let resp = "FINAL ANSWER: draft\nactually, reconsider\nFINAL ANSWER: real answer";
        assert_eq!(extract_answer(resp), "real answer");
    }

    #[test]
    fn no_marker_returns_whole_response() {
        let resp = "She lives in Berlin.";
        assert_eq!(extract_answer(resp), "She lives in Berlin.");
    }

    #[test]
    fn empty_after_marker_falls_back() {
        let resp = "[1] relevant\nFINAL ANSWER:";
        // Degenerate: marker with nothing after → don't return empty.
        assert_eq!(extract_answer(resp), resp.trim());
    }

    #[test]
    fn directive_pins_the_marker() {
        assert!(directive().contains(ANSWER_MARKER));
        assert!(directive().contains("relevant"));
    }
}
