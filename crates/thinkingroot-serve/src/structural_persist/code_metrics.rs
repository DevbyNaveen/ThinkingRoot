//! Code-metrics emitter — Compile Completeness Contract §4.16.
//!
//! Per-FunctionDef LOC + cyclomatic complexity. v1 ships:
//!
//! - **LOC for every FunctionDef**, regardless of language. Counts
//!   non-blank, non-comment-only lines via a cheap line walk.
//! - **Cyclomatic complexity for Rust + TypeScript + Python + Go + Java**
//!   via regex-driven decision-node counting (per contract §15 Q3).
//! - **Other 14 languages**: `cyclomatic = 0` + `complexity_method =
//!   "unsupported"` so queries can distinguish "0 because trivial" from
//!   "0 because language not yet supported" (per §15 Q3).
//!
//! `fan_in` / `fan_out` are stamped by Phase 7e
//! (`crates/thinkingroot-link/src/structural_resolve.rs` step 4) once
//! `function_calls.callee_claim_id` resolution is complete. Phase 6.7
//! emits them at 0 and Phase 7e overwrites via `:put` upsert.

use std::sync::OnceLock;

use regex::Regex;
use thinkingroot_core::ir::{Chunk, ChunkType};
use thinkingroot_extract::ExtractionOutput;
use thinkingroot_graph::{Blake3Cache, rows::CodeMetric};

use super::stable_row_id;

pub(super) fn emit(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    out: &mut Vec<CodeMetric>,
    extraction: &ExtractionOutput,
) {
    if !matches!(chunk.chunk_type, ChunkType::FunctionDef | ChunkType::TypeDef) {
        return;
    }

    let claim_id = extraction
        .claims
        .iter()
        .find(|c| {
            c.source_span
                .as_ref()
                .and_then(|s| match (s.byte_start, s.byte_end) {
                    (Some(bs), Some(be)) => Some(bs == chunk.byte_start && be == chunk.byte_end),
                    _ => None,
                })
                .unwrap_or(false)
        })
        .map(|c| c.id.to_string())
        .unwrap_or_default();

    let language = chunk.language.as_deref().unwrap_or("");
    let loc = count_loc(&chunk.content, language);
    let (cyclomatic, method) = match language {
        "rust" | "typescript" | "javascript" | "python" | "go" | "java" => {
            (count_decisions(&chunk.content, language), "mccabe")
        }
        _ => (0, "unsupported"),
    };

    let scope = if matches!(chunk.chunk_type, ChunkType::FunctionDef) {
        "function"
    } else {
        "type"
    };

    let id = stable_row_id(
        "code_metrics",
        source_id,
        chunk.byte_start,
        chunk.byte_end,
        scope,
    );
    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();

    out.push(CodeMetric {
        id,
        source_id: source_id.to_string(),
        scope: scope.to_string(),
        scope_claim_id: claim_id,
        loc: loc as u32,
        cyclomatic: cyclomatic as u32,
        fan_in: 0,  // resolved at Phase 7e
        fan_out: 0, // resolved at Phase 7e
        complexity_method: method.to_string(),
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        content_blake3: blake3_str,
    });
}

/// Lines of Code — non-blank, non-comment-only lines. The single-line
/// comment marker depends on language; we use a small per-language map
/// and fall back to "no-comment-stripping" for unknown langs.
fn count_loc(content: &str, language: &str) -> usize {
    let prefix = match language {
        "python" | "ruby" | "bash" | "r" | "elixir" | "perl" => "#",
        "lua" | "haskell" => "--",
        "rust" | "javascript" | "typescript" | "go" | "java" | "c" | "cpp" | "csharp"
        | "swift" | "kotlin" | "scala" | "php" => "//",
        _ => "", // unknown — count every non-blank line
    };
    content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return false;
            }
            if !prefix.is_empty() && trimmed.starts_with(prefix) {
                return false;
            }
            true
        })
        .count()
}

/// McCabe cyclomatic complexity — base 1 + count of decision nodes.
/// Decision nodes counted per language:
///
/// - **Rust**: `if `, ` else if `, `match `, ` => `, ` for `, ` while `,
///   `&&`, `||`, `?` (try operator)
/// - **TypeScript / JavaScript**: `if `, `else if`, `case `, `for `,
///   `while `, `&&`, `||`, `?:` (ternary)
/// - **Python**: `if `, `elif `, `for `, `while `, `except `, `and `,
///   `or `
/// - **Go**: `if `, `else if`, `switch `, `case `, `for `, `&&`, `||`
/// - **Java**: `if `, `else if`, `case `, `for `, `while `, `&&`, `||`,
///   `?:`
///
/// Each match adds 1 to the base. Multi-character operators that can
/// also appear in non-decision contexts (e.g. `?` in Rust generics) are
/// scoped tightly to avoid false positives.
fn count_decisions(content: &str, language: &str) -> usize {
    let mut count = 1usize; // base case
    let regex = decision_regex(language);
    for _ in regex.find_iter(content) {
        count += 1;
    }
    count
}

fn decision_regex(language: &str) -> &'static Regex {
    static RUST: OnceLock<Regex> = OnceLock::new();
    static TS_JS: OnceLock<Regex> = OnceLock::new();
    static PYTHON: OnceLock<Regex> = OnceLock::new();
    static GO: OnceLock<Regex> = OnceLock::new();
    static JAVA: OnceLock<Regex> = OnceLock::new();
    static FALLBACK: OnceLock<Regex> = OnceLock::new();

    match language {
        "rust" => RUST.get_or_init(|| {
            Regex::new(
                r"\bif\b|\belse if\b|\bmatch\b|=>|\bfor\b|\bwhile\b|&&|\|\||\?\s*[;,)\]\.}\s]",
            )
            .expect("rust decision regex")
        }),
        "typescript" | "javascript" => TS_JS.get_or_init(|| {
            Regex::new(
                r"\bif\b|\belse if\b|\bcase\b|\bfor\b|\bwhile\b|&&|\|\||\?\s*[^.:?]",
            )
            .expect("ts/js decision regex")
        }),
        "python" => PYTHON.get_or_init(|| {
            Regex::new(r"\bif\b|\belif\b|\bfor\b|\bwhile\b|\bexcept\b|\band\b|\bor\b")
                .expect("python decision regex")
        }),
        "go" => GO.get_or_init(|| {
            Regex::new(r"\bif\b|\belse if\b|\bswitch\b|\bcase\b|\bfor\b|&&|\|\|")
                .expect("go decision regex")
        }),
        "java" => JAVA.get_or_init(|| {
            Regex::new(r"\bif\b|\belse if\b|\bcase\b|\bfor\b|\bwhile\b|&&|\|\||\?\s*[^.:?]")
                .expect("java decision regex")
        }),
        _ => FALLBACK.get_or_init(|| Regex::new(r"$.").expect("noop fallback")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::ir::{Chunk, ChunkType};

    fn mk_fn_chunk(content: &str, language: &str) -> Chunk {
        let mut c = Chunk::new(content.to_string(), ChunkType::FunctionDef, 1, 10)
            .with_language(language);
        c.byte_start = 0;
        c.byte_end = content.len() as u64;
        c
    }

    fn metric_of(content: &str, language: &str) -> CodeMetric {
        let chunk = mk_fn_chunk(content, language);
        let bytes = chunk.content.as_bytes().to_vec();
        let mut cache = Blake3Cache::new(&bytes);
        let mut out = Vec::new();
        let extraction = ExtractionOutput::default();
        emit(&chunk, "src", &mut cache, &mut out, &extraction);
        assert_eq!(out.len(), 1);
        out.into_iter().next().unwrap()
    }

    #[test]
    fn loc_skips_blank_and_comment_lines_rust() {
        let m = metric_of(
            "fn x() {\n\n    // comment\n    let a = 1;\n    a + 1\n}",
            "rust",
        );
        // Counts: fn x() {, let a = 1;, a + 1, } → 4
        assert_eq!(m.loc, 4);
    }

    #[test]
    fn rust_cyclomatic_simple_function() {
        // No decisions → base 1.
        let m = metric_of("fn x() -> i32 { 42 }", "rust");
        assert_eq!(m.cyclomatic, 1);
        assert_eq!(m.complexity_method, "mccabe");
    }

    #[test]
    fn rust_cyclomatic_with_branches() {
        let m = metric_of(
            "fn x(a: i32) -> i32 { if a > 0 { 1 } else if a < 0 { -1 } else { 0 } }",
            "rust",
        );
        // base 1 + if + else if = 3.
        assert!(m.cyclomatic >= 3);
    }

    #[test]
    fn rust_cyclomatic_match_arms() {
        let m = metric_of(
            "fn x(a: i32) -> i32 { match a { 0 => 0, 1 => 1, _ => 2 } }",
            "rust",
        );
        // match keyword + 3 arms (`=>`).
        assert!(m.cyclomatic >= 4);
    }

    #[test]
    fn python_cyclomatic_with_elif() {
        let m = metric_of(
            "def x(a):\n    if a > 0:\n        return 1\n    elif a < 0:\n        return -1\n    return 0",
            "python",
        );
        // base + if + elif = 3.
        assert!(m.cyclomatic >= 3);
    }

    #[test]
    fn unsupported_language_marks_method() {
        let m = metric_of("fn x() {}", "haskell");
        assert_eq!(m.cyclomatic, 0);
        assert_eq!(m.complexity_method, "unsupported");
    }
}
