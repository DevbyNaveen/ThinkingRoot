//! Wedge 3: AST-aware splitter for oversized code chunks.
//!
//! When a chunk's content exceeds the per-chunk token budget, the previous
//! line-based splitter would slice through function bodies, match arms, and
//! statement blocks indiscriminately.  This module re-parses the chunk
//! content with the appropriate tree-sitter grammar and packs *top-level
//! statements* into sub-chunks at statement boundaries.
//!
//! On any failure (unknown language, parser error, or no useful structural
//! breaks), the caller falls back to the line-based splitter — never panics,
//! never silently drops content.

use thinkingroot_parse::code::ts_language;

/// Split `content` along top-level statement boundaries of the given
/// `language`, packing siblings into sub-chunks no larger than
/// `max_chars` characters (token budget = `max_tokens × 4`).
///
/// Returns `Some(parts)` when AST splitting was applied AND produced more
/// than one sub-chunk.  Returns `None` when:
///
/// - the language has no tree-sitter grammar wired in `thinkingroot-parse`
/// - tree-sitter failed to parse the snippet
/// - the AST yielded zero or one top-level statement (so a line-based
///   fallback is no worse than what we'd produce anyway)
pub(crate) fn split_at_statement_boundaries(
    content: &str,
    language: &str,
    max_tokens: usize,
) -> Option<Vec<String>> {
    let max_chars = max_tokens.saturating_mul(4).max(1);
    if content.len() <= max_chars {
        return None;
    }

    let lang = ts_language(language).ok()?;
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        return None;
    }
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();

    // Collect top-level statement byte ranges (children of the root).
    let mut cursor = root.walk();
    let stmts: Vec<(usize, usize)> = root
        .children(&mut cursor)
        .filter_map(|child| {
            let s = child.start_byte();
            let e = child.end_byte();
            if e > s {
                Some((s, e))
            } else {
                None
            }
        })
        .collect();

    if stmts.len() <= 1 {
        return None;
    }

    let mut out: Vec<String> = Vec::new();
    let mut buf_start: Option<usize> = None;
    let mut buf_end: usize = 0;

    for (s, e) in stmts {
        let len = e - s;
        // A single statement that already exceeds the budget gets its own
        // sub-chunk — caller's line-based fallback can split it further if
        // needed (degenerate cases like one giant raw string).
        if len > max_chars {
            // Flush any prior accumulated batch first.
            if let Some(start) = buf_start {
                let slice = &content[start..buf_end];
                if !slice.trim().is_empty() {
                    out.push(slice.to_string());
                }
            }
            out.push(content[s..e].to_string());
            buf_start = None;
            buf_end = 0;
            continue;
        }

        // Greedy pack: would adding this statement push us over budget?
        match buf_start {
            Some(start) if (buf_end - start) + len > max_chars => {
                let slice = &content[start..buf_end];
                if !slice.trim().is_empty() {
                    out.push(slice.to_string());
                }
                buf_start = Some(s);
                buf_end = e;
            }
            Some(_) => {
                // Extend running batch to include this statement.  The
                // gap between previous statements (whitespace, comments)
                // is included so byte slices stay contiguous and source
                // remains parseable.
                buf_end = e;
            }
            None => {
                buf_start = Some(s);
                buf_end = e;
            }
        }
    }

    if let Some(start) = buf_start
        && start < buf_end
    {
        let slice = &content[start..buf_end];
        if !slice.trim().is_empty() {
            out.push(slice.to_string());
        }
    }

    if out.len() <= 1 {
        // No actual splitting happened.
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_function_split_at_top_level_statement_boundary() {
        // A module body with many independent functions.  Force a small
        // budget so the splitter must fire.
        let mut content = String::new();
        for i in 0..40 {
            content.push_str(&format!(
                "pub fn task_{i}() -> usize {{\n    let x = {i};\n    x * 2\n}}\n\n"
            ));
        }
        let max_tokens = 80; // 320 chars budget — way under the full module
        let parts =
            split_at_statement_boundaries(&content, "rust", max_tokens).expect("expected split");
        assert!(parts.len() > 1);
        // Every sub-chunk must close all braces (no mid-function cuts).
        for p in &parts {
            let opens = p.matches('{').count();
            let closes = p.matches('}').count();
            assert_eq!(opens, closes, "unbalanced braces in sub-chunk:\n{p}");
        }
    }

    #[test]
    fn python_split_at_top_level_def_boundary() {
        let mut content = String::new();
        for i in 0..15 {
            content.push_str(&format!("def task_{i}():\n    return {i}\n\n"));
        }
        let parts = split_at_statement_boundaries(&content, "python", 60)
            .expect("expected split for python");
        assert!(parts.len() > 1);
        for p in &parts {
            // Each sub-chunk should start with a `def` (top-level statement).
            assert!(p.trim_start().starts_with("def "));
        }
    }

    #[test]
    fn typescript_split_at_top_level_statement() {
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!(
                "export function fn{i}(x: number): number {{ return x + {i}; }}\n"
            ));
        }
        let parts = split_at_statement_boundaries(&content, "typescript", 30)
            .expect("expected split for ts");
        assert!(parts.len() > 1);
    }

    #[test]
    fn unknown_language_returns_none() {
        let content = "(((((((((".repeat(2000);
        assert!(split_at_statement_boundaries(&content, "cobol", 10).is_none());
    }

    #[test]
    fn small_content_returns_none() {
        let content = "fn x() {}";
        assert!(split_at_statement_boundaries(content, "rust", 10_000).is_none());
    }

    #[test]
    fn single_oversized_statement_emits_lone_subchunk() {
        // One giant top-level statement that itself exceeds the budget.
        // Splitter still returns >1 if there are siblings; if it's truly
        // alone, it returns None so the line-based fallback handles it.
        let single_giant = format!("static BIG: &str = \"{}\";", "x".repeat(8_000));
        // Just one item — no sibling statements → None (caller falls back to lines).
        assert!(split_at_statement_boundaries(&single_giant, "rust", 50).is_none());

        // Now pair the giant with a small sibling — the splitter must put
        // the giant into its own sub-chunk and the small one separately.
        let paired = format!("{single_giant}\nfn small() {{}}\n");
        let parts =
            split_at_statement_boundaries(&paired, "rust", 50).expect("expected split for paired");
        assert!(parts.len() >= 2);
    }
}
