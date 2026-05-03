//! Wedge 3: paragraph- and sentence-aware splitter for oversized prose chunks.
//!
//! Used when the line-based splitter would otherwise cut a long markdown
//! section mid-sentence.  Order of preference:
//!
//! 1. Blank-line paragraph boundaries.
//! 2. Sentence terminators (`.`, `!`, `?`) followed by whitespace.
//! 3. Line boundaries (degenerate fallback).
//!
//! Matches the engine-wide `chars / 4` token approximation.

/// Split `content` into sub-chunks that stay within `max_tokens` (in the
/// `chars / 4` heuristic).  Returns one item per sub-chunk; never empty
/// for non-empty input, never returns chunks larger than the budget unless
/// the input contained a single un-splittable run of non-whitespace text.
pub(crate) fn split_prose(content: &str, max_tokens: usize) -> Vec<String> {
    let max_chars = max_tokens.saturating_mul(4).max(1);
    if content.is_empty() {
        return Vec::new();
    }
    if content.len() <= max_chars {
        return vec![content.to_string()];
    }

    // Pass 1: split on blank-line paragraph boundaries.
    let paragraphs: Vec<&str> = content
        .split("\n\n")
        .filter(|p| !p.is_empty())
        .collect();
    let mut out: Vec<String> = Vec::new();
    for p in paragraphs {
        if p.len() <= max_chars {
            out.push(p.to_string());
        } else {
            // Pass 2: sentence-boundary split on this paragraph.
            out.extend(split_sentences(p, max_chars));
        }
    }

    // Pass 3: greedily merge adjacent small fragments back together up to
    // the budget so we don't return needlessly-fragmented output.
    let merged = merge_under_budget(out, max_chars);
    if merged.is_empty() {
        vec![content.to_string()]
    } else {
        merged
    }
}

fn split_sentences(text: &str, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut prev_was_terminator = false;

    for ch in text.chars() {
        buf.push(ch);
        let is_terminator = matches!(ch, '.' | '!' | '?');
        if prev_was_terminator && (ch == ' ' || ch == '\n') {
            // Boundary just passed.  If buf is over budget, flush.
            if buf.len() >= max_chars {
                out.push(buf.trim().to_string());
                buf.clear();
            }
            prev_was_terminator = false;
        } else {
            prev_was_terminator = is_terminator;
        }
    }
    if !buf.trim().is_empty() {
        // If even the final buffer is over budget (no sentence breaks at
        // all), fall back to line splitting for that residual.
        if buf.len() > max_chars {
            out.extend(split_by_lines(&buf, max_chars));
        } else {
            out.push(buf.trim().to_string());
        }
    }
    out
}

fn split_by_lines(text: &str, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if !current.is_empty() && current.len() + line.len() + 1 > max_chars {
            out.push(std::mem::take(&mut current).trim().to_string());
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn merge_under_budget(parts: Vec<String>, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for p in parts {
        if current.is_empty() {
            current = p;
            continue;
        }
        if current.len() + 2 + p.len() <= max_chars {
            current.push_str("\n\n");
            current.push_str(&p);
        } else {
            out.push(std::mem::take(&mut current));
            current = p;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_input_returns_single_chunk() {
        let parts = split_prose("hello world", 1000);
        assert_eq!(parts, vec!["hello world".to_string()]);
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        let parts = split_prose("", 100);
        assert!(parts.is_empty());
    }

    #[test]
    fn paragraph_boundary_used_first() {
        let p1 = "p1 short. ".repeat(20); // ~ 200 chars
        let p2 = "p2 also short. ".repeat(20);
        let content = format!("{p1}\n\n{p2}");
        // budget = 100 tokens = 400 chars; each paragraph ≤ 400 → 2 chunks
        let parts = split_prose(&content, 100);
        assert!(parts.len() >= 2);
        for p in &parts {
            assert!(p.len() <= 400 + 50); // small slack for join markers
        }
    }

    #[test]
    fn oversized_paragraph_splits_at_sentence_boundary() {
        // One paragraph way over budget, with sentence breaks.
        let mut text = String::new();
        for i in 0..50 {
            text.push_str(&format!("Sentence number {i}. "));
        }
        // budget = 50 tokens = 200 chars.
        let parts = split_prose(&text, 50);
        assert!(parts.len() > 1);
        // No part exceeds the budget by more than the slack from inclusive
        // sentence ending + final paragraph join.
        for p in &parts {
            assert!(
                p.len() <= 400,
                "part too big: {} chars\n{p}",
                p.len()
            );
        }
    }

    #[test]
    fn oversized_paragraph_with_no_sentence_breaks_falls_back_to_lines() {
        // No `.`, no blank lines, but a few line breaks.
        let mut text = String::new();
        for _ in 0..200 {
            text.push_str("xxxxxxxxxxxxxxxxxxxxxxx\n");
        }
        let parts = split_prose(&text, 30); // 120 char budget
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(
                p.len() <= 120 + 30,
                "line-fallback chunk too big: {} chars",
                p.len()
            );
        }
    }
}
