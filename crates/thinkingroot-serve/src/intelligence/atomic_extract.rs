//! Phase 1a — LLM atomic-fact extraction over verbatim chunks (the
//! north-star compile rebuild). The pure parse + anti-hallucination gate
//! live in `thinkingroot_core::types::atomic_fact`; this module is the thin
//! LLM-I/O glue.
//!
//! Rate-limiting + 429 backoff are handled INSIDE `LlmClient::chat` (it
//! drives the shared `ThroughputScheduler` internally), so the extractor just
//! awaits one `chat` per chunk and lets the maintenance task fan out
//! concurrency. Cost is approved (covered by subscription); correctness is the
//! constraint, so we extract one chunk per call (clean span attribution).

use thinkingroot_core::types::{parse_atomic_facts, AtomicFact, ChunkContext};
use thinkingroot_llm::llm::LlmClient;

/// System prompt. Forces a JSON array of grounded SVO facts, each carrying a
/// VERBATIM quote copied from the chunk (we locate it ourselves — never trust
/// the model to count character offsets). Ungrounded facts are dropped by the
/// parser, so the prompt's job is recall, not gatekeeping.
pub const ATOMIC_SYSTEM: &str = "You extract atomic facts from a passage of text. For each \
distinct, self-contained factual statement the passage asserts, output an object with: \
`subject` (the entity the fact is about), `predicate` (the relationship/verb), `object` (the \
value or other entity), and `quote` (the EXACT substring of the passage — copied verbatim, \
character-for-character — that states this fact). Rules: copy `quote` EXACTLY from the passage \
(do not paraphrase, fix typos, or add words); `subject` and `object` must be words that appear \
in the passage; extract only facts the passage actually states (never infer or add outside \
knowledge); skip opinions, questions, and boilerplate. Output ONLY a JSON array, no prose, no \
markdown fences. If the passage states no facts, output [].";

/// Build the per-chunk user prompt.
pub fn build_atomic_prompt(content: &str) -> String {
    format!("Passage:\n\"\"\"\n{content}\n\"\"\"\n\nFacts (JSON array):")
}

/// Extract grounded atomic facts from one chunk.
///
/// `Ok(facts)` — the LLM responded (the vec may be empty if the chunk states
/// no facts, or all candidates failed the grounding gate). `Err(_)` — a
/// transient LLM failure, so the caller can keep the source queued for retry.
/// Trivially short chunks short-circuit to `Ok(vec![])`.
pub async fn extract_chunk_facts(
    llm: &LlmClient,
    ctx: &ChunkContext,
) -> Result<Vec<AtomicFact>, thinkingroot_core::Error> {
    if ctx.content.trim().len() < 16 {
        return Ok(Vec::new());
    }
    let prompt = build_atomic_prompt(&ctx.content);
    let resp = llm.chat(ATOMIC_SYSTEM, &prompt).await?;
    Ok(parse_atomic_facts(&resp, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_the_passage() {
        let p = build_atomic_prompt("Yuriy teaches the course.");
        assert!(p.contains("Yuriy teaches the course."));
        assert!(p.contains("JSON array"));
    }
}
