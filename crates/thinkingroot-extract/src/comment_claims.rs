//! Opt-in claim extractor: scans comment chunks for `@claim`,
//! `@invariant`, `@owns`, and `SAFETY:` tags and emits one Witness
//! per match.
//!
//! Why this is opt-in: every other rule family extracts what is
//! mechanically present in the source. These four tags let the
//! author *opt in* to a free-text assertion that the compiler then
//! ships as a typed Witness. The author may be wrong (so the
//! confidence is 0.95, not 0.99) but the extraction itself is
//! exact — we ship the verbatim comment bytes, not a paraphrase.
//!
//! Span correctness: the function operates on the raw `source_bytes`
//! slice rather than `chunk.content` so that:
//! - File-relative byte offsets are authoritative (no off-by-one if
//!   a parser trimmed leading whitespace from `chunk.content`).
//! - `content_blake3` is computed over the exact bytes the pack
//!   ships, matching what `tr-verify` re-checks.
//!
//! The caller is responsible for passing the chunk's source bytes;
//! the chunk's `byte_start`/`byte_end` define the slice. Empty-range
//! chunks (the `(0, 0)` sentinel) are skipped — without an
//! authoritative byte range, the Witness anchor cannot be computed.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use thinkingroot_core::ir::{Chunk, ChunkType};
use thinkingroot_core::types::{
    Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
};

/// Build a regex that matches a tag anchored at the start of a comment
/// line. Comment prefixes covered: `//`, `///`, `//!`, `#`, `--`, `*`
/// (multi-line `/* * */` continuation), `;` (lisp/elixir-style). The
/// pattern captures the tag content (everything after the tag name and
/// whitespace) up to end-of-line.
fn build_tag_regex(tag: &str) -> Regex {
    // (?m) — multi-line so `^` and `$` match line boundaries.
    // Comment-prefix alternation matches longest-first.
    // For `SAFETY:`, the tag is followed by `:`; for `@*` tags, by
    // whitespace.
    let pattern = if tag == "SAFETY" {
        r"(?m)^[ \t]*(?:///|//!|//|#|--|\*|;)[ \t]*SAFETY:[ \t]*(.+?)[ \t]*$".to_string()
    } else {
        format!(
            r"(?m)^[ \t]*(?:///|//!|//|#|--|\*|;)[ \t]*@{tag}[ \t]+(.+?)[ \t]*$"
        )
    };
    Regex::new(&pattern).expect("regex pattern is valid")
}

fn claim_re() -> &'static Regex {
    static CLAIM_RE: OnceLock<Regex> = OnceLock::new();
    CLAIM_RE.get_or_init(|| build_tag_regex("claim"))
}

fn invariant_re() -> &'static Regex {
    static INVARIANT_RE: OnceLock<Regex> = OnceLock::new();
    INVARIANT_RE.get_or_init(|| build_tag_regex("invariant"))
}

fn owns_re() -> &'static Regex {
    static OWNS_RE: OnceLock<Regex> = OnceLock::new();
    OWNS_RE.get_or_init(|| build_tag_regex("owns"))
}

fn safety_re() -> &'static Regex {
    static SAFETY_RE: OnceLock<Regex> = OnceLock::new();
    SAFETY_RE.get_or_init(|| build_tag_regex("SAFETY"))
}

/// Map each tag flavour to its rule + Witness-type pair.
struct TagSpec {
    rule: &'static str,
    witness_type: &'static str,
    regex: &'static Regex,
}

fn tag_specs() -> [TagSpec; 4] {
    [
        TagSpec {
            rule: "comment::@claim@v1",
            witness_type: "claim::@claim",
            regex: claim_re(),
        },
        TagSpec {
            rule: "comment::@invariant@v1",
            witness_type: "claim::@invariant",
            regex: invariant_re(),
        },
        TagSpec {
            rule: "comment::@owns@v1",
            witness_type: "claim::@owns",
            regex: owns_re(),
        },
        TagSpec {
            rule: "comment::SAFETY@v1",
            witness_type: "code::safety-justification",
            regex: safety_re(),
        },
    ]
}

/// Extract Witnesses from a single chunk. Returns an empty Vec for
/// non-comment chunks and for chunks without authoritative byte
/// positioning.
///
/// `source_bytes` is the entire file's source byte slice. The chunk's
/// `byte_start..byte_end` defines the search window inside it.
/// `file_blake3` is the BLAKE3 of the canonicalised source file (the
/// `file_blake3` used in `WitnessSpan`).
pub fn extract_witnesses_from_chunk(
    chunk: &Chunk,
    source_bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Vec<Witness> {
    if !matches!(chunk.chunk_type, ChunkType::Comment | ChunkType::ModuleDoc) {
        return Vec::new();
    }
    if chunk.byte_start == 0 && chunk.byte_end == 0 {
        // Sentinel: parser did not stamp byte offsets. Honest skip.
        return Vec::new();
    }
    if chunk.byte_end <= chunk.byte_start {
        return Vec::new();
    }
    let chunk_start = chunk.byte_start as usize;
    let chunk_end = (chunk.byte_end as usize).min(source_bytes.len());
    if chunk_start >= source_bytes.len() {
        return Vec::new();
    }
    let window_bytes = &source_bytes[chunk_start..chunk_end];
    // The regexes match against UTF-8 text. Comments are textual by
    // construction; on non-UTF-8 windows we honestly fall back to
    // String::from_utf8_lossy which preserves match-byte positions
    // for ASCII-only tag prefixes (the only characters our regexes
    // care about).
    let window_text = std::str::from_utf8(window_bytes)
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|_| String::from_utf8_lossy(window_bytes));

    let mut out: Vec<Witness> = Vec::new();
    for spec in tag_specs() {
        for m in spec.regex.captures_iter(&window_text) {
            let full = m.get(0).expect("regex match has a 0-group");
            let span_start = chunk_start + full.start();
            let span_end = chunk_start + full.end();
            if span_end > source_bytes.len() {
                // Lossy decode could in principle shift offsets on
                // non-ASCII; we never trust a span past the buffer.
                continue;
            }
            let span = WitnessSpan {
                file_blake3: file_blake3.to_string(),
                start: span_start as u64,
                end: span_end as u64,
            };
            let content_blake3 = blake3::hash(&source_bytes[span_start..span_end])
                .to_hex()
                .to_string();
            let witness = Witness::new(
                spec.rule,
                spec.witness_type,
                vec![WitnessInput::ByteRef {
                    file_blake3: file_blake3.to_string(),
                    start: span.start,
                    end: span.end,
                }],
                vec![span],
                source_id,
                workspace_id,
                Sensitivity::Public,
                Confidence::new(0.95),
                content_blake3,
                now,
            );
            // Attach the enclosing function/type symbol when the
            // chunk has it (helps Phase 7e correlation).
            let witness = match chunk
                .metadata
                .function_name
                .as_ref()
                .or(chunk.metadata.type_name.as_ref())
            {
                Some(sym) => witness.with_symbol(sym),
                None => witness,
            };
            out.push(witness);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_comment_chunk(content: &str, byte_start: u64, byte_end: u64) -> Chunk {
        let mut chunk = Chunk::new(content, ChunkType::Comment, 1, 1);
        chunk.byte_start = byte_start;
        chunk.byte_end = byte_end;
        chunk
    }

    #[test]
    fn extracts_claim_tag_from_rust_doc_comment() {
        let source = "/// @claim implements XYZ protocol\nfn foo() {}\n";
        let chunk = make_comment_chunk("/// @claim implements XYZ protocol", 0, 34);
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "fake_file_blake3",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        let w = &witnesses[0];
        assert_eq!(w.witness_type, "claim::@claim");
        assert_eq!(w.rule, "comment::@claim@v1");
        assert_eq!(w.confidence.value(), 0.95);
        assert!(w.content_blake3.len() == 64);
    }

    #[test]
    fn extracts_invariant_tag_from_python_comment() {
        let source = "# @invariant balance >= 0\n";
        let chunk = make_comment_chunk("# @invariant balance >= 0", 0, 25);
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].witness_type, "claim::@invariant");
        assert_eq!(witnesses[0].rule, "comment::@invariant@v1");
    }

    #[test]
    fn extracts_owns_tag() {
        let source = "// @owns auth-subsystem\n";
        let chunk = make_comment_chunk("// @owns auth-subsystem", 0, 23);
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].witness_type, "claim::@owns");
    }

    #[test]
    fn extracts_safety_tag_from_rust_unsafe_comment() {
        let source = "// SAFETY: pointer is non-null because checked at entry\n";
        let chunk = make_comment_chunk(
            "// SAFETY: pointer is non-null because checked at entry",
            0,
            55,
        );
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].witness_type, "code::safety-justification");
        assert_eq!(witnesses[0].rule, "comment::SAFETY@v1");
    }

    #[test]
    fn ignores_non_comment_chunks() {
        let mut chunk = Chunk::new("/// @claim ...", ChunkType::FunctionDef, 1, 1);
        chunk.byte_start = 0;
        chunk.byte_end = 14;
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            b"/// @claim ...",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(witnesses.is_empty());
    }

    #[test]
    fn ignores_chunks_with_sentinel_byte_range() {
        // (0, 0) is the parser's "not yet tracked" sentinel.
        let chunk = make_comment_chunk("// @claim hi", 0, 0);
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            b"// @claim hi",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(witnesses.is_empty());
    }

    #[test]
    fn tag_not_at_line_start_is_not_matched() {
        // The regex anchors at line start (with optional whitespace).
        // A `@claim` in the middle of prose should not match.
        let source = "// please @claim that this works\n";
        let chunk = make_comment_chunk("// please @claim that this works", 0, 32);
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        // No match — `@claim` is preceded by "please " which is not
        // a comment-marker.
        assert!(witnesses.is_empty());
    }

    #[test]
    fn multiple_tags_in_one_chunk_emit_distinct_witnesses() {
        let source = "/// @claim does X\n/// @invariant Y > 0\n";
        let chunk = make_comment_chunk(
            "/// @claim does X\n/// @invariant Y > 0",
            0,
            38,
        );
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 2);
        let types: Vec<&str> = witnesses.iter().map(|w| w.witness_type.as_str()).collect();
        assert!(types.contains(&"claim::@claim"));
        assert!(types.contains(&"claim::@invariant"));
    }

    #[test]
    fn witness_spans_are_file_relative() {
        // Chunk starts at byte 100 in the source. Match offset
        // should reflect that.
        let prefix = "x".repeat(100);
        let source = format!("{prefix}// @claim hi\n");
        let chunk_text = "// @claim hi";
        let chunk = make_comment_chunk(chunk_text, 100, 100 + chunk_text.len() as u64);
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        let span = &witnesses[0].spans[0];
        assert_eq!(span.start, 100, "span start should be file-relative");
        assert_eq!(span.end, 100 + chunk_text.len() as u64);
    }

    #[test]
    fn content_blake3_matches_actual_span_bytes() {
        let source = "// @claim payload\n";
        let chunk = make_comment_chunk("// @claim payload", 0, 17);
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        let span = &witnesses[0].spans[0];
        let expected = blake3::hash(&source.as_bytes()[span.start as usize..span.end as usize])
            .to_hex()
            .to_string();
        assert_eq!(witnesses[0].content_blake3, expected);
    }

    #[test]
    fn symbol_attached_when_chunk_has_function_name() {
        let source = "// @claim hi\n";
        let mut chunk = make_comment_chunk("// @claim hi", 0, 12);
        chunk.metadata.function_name = Some("my_function".into());
        let witnesses = extract_witnesses_from_chunk(
            &chunk,
            source.as_bytes(),
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].symbol.as_deref(), Some("my_function"));
    }
}
