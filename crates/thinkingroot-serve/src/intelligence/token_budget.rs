// crates/thinkingroot-serve/src/intelligence/token_budget.rs
//
// Bounded tool-result content for the agent loop.
//
// The agent loop (agent.rs::dispatch_calls) appends every ToolResult
// to history without truncation. A single `read_file` on a 10K-LOC
// source, or a `search` returning 50 hits, can blow the LLM's
// context budget — making subsequent iterations slower, more
// expensive, and (past the model's window) outright lossy.
//
// `truncate_tool_result` enforces a soft per-result token cap. The
// agent loop calls it on every ToolResult before pushing to history;
// content under the cap passes through unchanged, content above it
// is reshaped to a head + tail with an explicit truncation marker so
// the LLM (and any downstream verifier) can reason about the cut.
//
// Token estimation uses the workspace-wide ~4-chars-per-token rule
// shared with `compressor.rs`. Cheap (no tokenizer dep), deterministic,
// safe to under-estimate (we cut MORE than necessary, never less).
//
// (C6 fix, plan 2026-05-09. Defense-in-depth — does NOT replace the
// `max_iterations` loop ceiling, which still bounds total LLM calls.)

/// Per-tool-result token budget. Chosen so a single result never
/// dominates a typical 200K-context conversation: 2,048 tokens leaves
/// ample room for the surrounding history, the next user turn, and
/// the model's response. Tool authors needing more context should
/// summarize server-side or expose pagination — not push raw
/// 50K-token blobs through the agent loop.
pub const DEFAULT_TOOL_RESULT_TOKEN_BUDGET: usize = 2_048;

/// Marker the LLM (and downstream verifier) can recognise as evidence
/// of a truncation. Single line so it doesn't bloat the cut content.
const TRUNCATION_MARKER: &str =
    "\n\n<… truncated for token budget — call the tool again with a tighter scope to see more …>\n\n";

/// Estimate token count via the 4-chars-per-token rule used elsewhere
/// in the workspace (see `compressor.rs::estimate_tokens`). Conservative
/// for English code + prose; over-estimates for short ASCII strings,
/// which means we'll occasionally truncate slightly too eagerly — a
/// safer failure mode than under-estimating and overflowing context.
#[inline]
pub fn estimate_tokens(s: &str) -> usize {
    // Use byte len (.len()) rather than char count: byte len is what
    // the wire encoder actually pays for, and it never under-estimates
    // for multi-byte UTF-8 (where char count would).
    s.len() / 4
}

/// Truncate `content` so its estimated token count ≤ `budget`. When
/// the input already fits, returns it unchanged (no allocation). When
/// over budget, returns head + truncation marker + tail, with head and
/// tail each ~30% of the budget — preserving the most-likely-relevant
/// content (function signature at the start, return value or error at
/// the end) while still surfacing the cut to the LLM.
///
/// `budget = 0` is a valid "drop entirely" request: returns just the
/// marker. Callers that don't want truncation should pass
/// [`usize::MAX`] (or skip the call).
pub fn truncate_tool_result(content: String, budget: usize) -> String {
    let estimated = estimate_tokens(&content);
    if estimated <= budget {
        return content;
    }

    if budget == 0 {
        return TRUNCATION_MARKER.trim().to_string();
    }

    // Head + tail allocation: each gets ~30% of the budget. We leave
    // the remaining ~40% as headroom for the marker itself + the
    // chars-to-tokens estimation slack. Multiplying by 4 converts the
    // token budget back to a byte budget.
    let head_bytes = (budget * 4 * 30) / 100;
    let tail_bytes = (budget * 4 * 30) / 100;

    // Snap to UTF-8 char boundaries so we never split a multi-byte
    // codepoint mid-sequence (would panic the formatter downstream).
    let head_end = floor_char_boundary(&content, head_bytes);
    let tail_start = ceil_char_boundary(&content, content.len().saturating_sub(tail_bytes));

    // Don't bother emitting both head and tail when they overlap —
    // the input is short enough that the marker alone is fine.
    if tail_start <= head_end {
        return format!("{}{TRUNCATION_MARKER}", &content[..head_end]);
    }

    let head = &content[..head_end];
    let tail = &content[tail_start..];
    format!("{head}{TRUNCATION_MARKER}{tail}")
}

/// Round `idx` down to the nearest UTF-8 char boundary, never past 0.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Round `idx` up to the nearest UTF-8 char boundary, never past `s.len()`.
fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_budget_returns_unchanged() {
        let content = "small result".to_string();
        let out = truncate_tool_result(content.clone(), 1024);
        assert_eq!(out, content);
    }

    #[test]
    fn at_exact_budget_returns_unchanged() {
        // 4 chars = 1 token at the 4-cpt estimate.
        let content = "ab".repeat(2048); // 4096 chars ≈ 1024 tokens
        let out = truncate_tool_result(content.clone(), 1024);
        assert_eq!(out, content);
    }

    #[test]
    fn over_budget_inserts_truncation_marker() {
        let content = "x".repeat(20_000); // ≈ 5,000 tokens
        let out = truncate_tool_result(content, 1_024);
        assert!(
            out.contains("truncated for token budget"),
            "expected truncation marker in: {out}"
        );
        // Must be smaller than the input.
        assert!(out.len() < 20_000);
        // Must respect the budget within slack (4-cpt estimate +
        // marker headroom, so 1.5× budget is the realistic ceiling).
        assert!(
            estimate_tokens(&out) <= 1_024 * 2,
            "truncated output {} tokens still over budget",
            estimate_tokens(&out)
        );
    }

    #[test]
    fn budget_zero_returns_marker_only() {
        let content = "anything at all".to_string();
        let out = truncate_tool_result(content, 0);
        assert!(out.contains("truncated for token budget"));
        assert!(!out.contains("anything"));
    }

    #[test]
    fn preserves_head_and_tail_signal() {
        let body = "Z".repeat(20_000);
        let content = format!("HEAD-MARKER{body}TAIL-MARKER");
        let out = truncate_tool_result(content, 1_024);
        assert!(out.starts_with("HEAD-MARKER"), "head not preserved: {out:.100}");
        assert!(out.ends_with("TAIL-MARKER"), "tail not preserved: {out:.100}");
    }

    #[test]
    fn never_splits_multi_byte_codepoints() {
        // Chinese characters are 3 bytes each in UTF-8. A naive byte
        // slice in the middle of one would panic the formatter.
        let content = "中".repeat(10_000); // 30,000 bytes ≈ 7,500 tokens
        let out = truncate_tool_result(content, 1_024);
        // Round-trip the formatted output to prove it's valid UTF-8.
        assert!(out.is_char_boundary(0));
        assert!(out.is_char_boundary(out.len()));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn estimate_tokens_byte_len_div_4() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        // 3-byte char: 3 / 4 = 0 tokens at this scale — fine, we only
        // care about getting LARGE inputs right (where rounding error
        // disappears into the noise).
        assert_eq!(estimate_tokens("中"), 0);
    }
}
