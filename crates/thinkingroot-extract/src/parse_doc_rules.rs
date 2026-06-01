//! Rule-catalog adapter for documentation chunks.
//!
//! Wraps the existing `thinkingroot-parse::doctags` output (already
//! populated on `Comment` / `ModuleDoc` / `FunctionDef` chunks) and
//! `markdown` chunks (`Heading` / `Prose` / `List`) into typed
//! Witnesses keyed by the rule-catalog name.
//!
//! Mapping decisions:
//!
//! - A `DocTag { kind: "param", … }` on a Rust chunk emits a
//!   `rustdoc::param-doc@v1` Witness. The same tag on a JS/TS chunk
//!   emits `jsdoc::param@v1`. On Java, `javadoc::param@v1`.
//!   Languages outside the supported set emit no Witness (mechanical
//!   honesty: we ship Witnesses only for the rules we have).
//! - A `DocTag { kind: "returns", … }` on Rust/JS/Java emits a
//!   `function-summary` Witness in the corresponding family. The
//!   "returns" tag carries the return description which is part of
//!   what a function summary covers; collapsing into one rule
//!   matches the v1.0 catalog shape (separate `returns-doc` rules
//!   are v1.1).
//! - A `ChunkType::Heading` emits `markdown::heading@v1`.
//! - A `ChunkType::Prose` emits `markdown::paragraph@v1` when it
//!   is not also a commit-author chunk (those become git witnesses).
//! - A `ChunkType::List` emits `markdown::list-item@v1` (one
//!   Witness per chunk; sub-item granularity is v1.1 because the
//!   chunker does not yet emit per-item byte ranges).
//! - A `ChunkType::Code` chunk with a language hint emits
//!   `markdown::code-block@v1`.
//!
//! Span correctness: all Witnesses span the entire chunk's
//! authoritative byte range (`byte_start..byte_end`). Chunks with the
//! `(0, 0)` sentinel are skipped — without a verifiable anchor,
//! emitting a Witness would let a tampered span slip through.

use chrono::{DateTime, Utc};
use thinkingroot_core::ir::{Chunk, ChunkType, DocTag};
use thinkingroot_core::types::{
    Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
};

/// Result of adapting a single chunk.
pub struct DocRuleOutput {
    pub witnesses: Vec<Witness>,
}

/// Extract all documentation Witnesses for a chunk. Returns an empty
/// vec for chunks that match no rule in this family (other adapters
/// handle code / data / git / comment-claims).
/// Heuristic: are these bytes human-readable text rather than binary noise?
/// Rejects PDF/stream object markers and anything with a low printable-char
/// ratio (control chars / UTF-8 replacement chars). Used to keep binary files
/// (PDFs ingested as raw bytes) from polluting the witness mesh at ingest.
pub fn bytes_are_probably_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let text = String::from_utf8_lossy(bytes);
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if t.contains("FlateDecode") || t.contains("endstream") || t.contains("/Filter") {
        return false;
    }
    let total = t.chars().count();
    let printable = t
        .chars()
        .filter(|c| {
            *c == '\n' || *c == '\t' || *c == '\r' || (!c.is_control() && *c != '\u{FFFD}')
        })
        .count();
    (printable as f64 / total as f64) >= 0.85
}

pub fn extract_witnesses_from_chunk(
    chunk: &Chunk,
    source_bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> DocRuleOutput {
    let mut out = DocRuleOutput { witnesses: vec![] };
    if chunk.byte_start == 0 && chunk.byte_end == 0 {
        return out;
    }
    if chunk.byte_end <= chunk.byte_start
        || chunk.byte_start as usize >= source_bytes.len()
    {
        return out;
    }

    // Ingest-time binary guard: never emit witnesses for chunks whose bytes are
    // not human-readable text (e.g. a PDF ingested as raw bytes → FlateDecode
    // streams). Without this, binary files re-ingest on every compile/boot and
    // pollute recall with byte-noise "claims". Deterministic, zero-LLM.
    {
        let end = (chunk.byte_end as usize).min(source_bytes.len());
        let slice = &source_bytes[chunk.byte_start as usize..end];
        if !bytes_are_probably_text(slice) {
            return out;
        }
    }

    // Per-chunk-type emission.
    match chunk.chunk_type {
        ChunkType::Heading => {
            push_chunk_witness(
                &mut out.witnesses,
                chunk,
                "markdown::heading@v1",
                "documents::heading",
                source_bytes,
                file_blake3,
                source_id,
                workspace_id,
                now,
            );
        }
        ChunkType::Prose => {
            // Prose with a commit_author is a git Witness, not a
            // markdown paragraph (the git adapter owns it).
            if chunk.metadata.commit_author.is_none() {
                // Emit ONE witness per sentence (not per paragraph), so a
                // multi-fact paragraph yields multiple byte-anchored,
                // individually-searchable claims. Deterministic, zero-LLM.
                push_prose_sentence_witnesses(
                    &mut out.witnesses,
                    chunk,
                    source_bytes,
                    file_blake3,
                    source_id,
                    workspace_id,
                    now,
                );
            }
        }
        ChunkType::List => {
            push_chunk_witness(
                &mut out.witnesses,
                chunk,
                "markdown::list-item@v1",
                "documents::list-item",
                source_bytes,
                file_blake3,
                source_id,
                workspace_id,
                now,
            );
        }
        ChunkType::Code if chunk.language.is_some() => {
            push_chunk_witness(
                &mut out.witnesses,
                chunk,
                "markdown::code-block@v1",
                "documents::code-block",
                source_bytes,
                file_blake3,
                source_id,
                workspace_id,
                now,
            );
        }
        _ => {}
    }

    // Doc-tag emission for chunks that carry parsed tags.
    if !chunk.metadata.doc_tags.is_empty() {
        for tag in &chunk.metadata.doc_tags {
            if let Some((rule, witness_type)) = rule_for_doctag(tag, chunk.language.as_deref())
            {
                push_chunk_witness(
                    &mut out.witnesses,
                    chunk,
                    rule,
                    witness_type,
                    source_bytes,
                    file_blake3,
                    source_id,
                    workspace_id,
                    now,
                );
            }
        }
    }

    out
}

/// Map a `(tag.kind, language)` to a `(rule, witness_type)` pair, or
/// `None` if no rule in the v1.0 catalog applies. Returning `None` is
/// the mechanical-honesty path — we never invent a rule.
fn rule_for_doctag(
    tag: &DocTag,
    language: Option<&str>,
) -> Option<(&'static str, &'static str)> {
    let kind = tag.kind.as_str();
    let lang = language.unwrap_or("");
    match (kind, lang) {
        // Rust / rustdoc
        ("param", "rust") => Some(("rustdoc::param-doc@v1", "documents::param-doc")),
        ("returns", "rust") => Some((
            "rustdoc::function-summary@v1",
            "documents::function-summary",
        )),
        // JS / TS / jsdoc
        ("param", "javascript") | ("param", "typescript") => {
            Some(("jsdoc::param@v1", "documents::param-doc"))
        }
        ("returns", "javascript") | ("returns", "typescript") => Some((
            "jsdoc::function-summary@v1",
            "documents::function-summary",
        )),
        // Java / javadoc
        ("param", "java") => Some(("javadoc::param@v1", "documents::param-doc")),
        ("returns", "java") => Some(("javadoc::summary@v1", "documents::function-summary")),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn push_chunk_witness(
    out: &mut Vec<Witness>,
    chunk: &Chunk,
    rule: &str,
    witness_type: &str,
    source_bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) {
    let start = chunk.byte_start as usize;
    let end = (chunk.byte_end as usize).min(source_bytes.len());
    if end <= start {
        return;
    }
    let span = WitnessSpan {
        file_blake3: file_blake3.to_string(),
        start: chunk.byte_start,
        end: end as u64,
    };
    let content_blake3 = blake3::hash(&source_bytes[start..end]).to_hex().to_string();
    let mut witness = Witness::new(
        rule.to_string(),
        witness_type.to_string(),
        vec![WitnessInput::ByteRef {
            file_blake3: file_blake3.to_string(),
            start: span.start,
            end: span.end,
        }],
        vec![span],
        source_id,
        workspace_id,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        now,
    );
    if let Some(sym) = chunk
        .metadata
        .function_name
        .as_ref()
        .or(chunk.metadata.type_name.as_ref())
    {
        witness = witness.with_symbol(sym);
    }
    out.push(witness);
}

/// Minimum unit length (bytes) to emit as its own witness. Kept low so short
/// conversational facts ("My dog is Rex.") are not dropped — the mechanical
/// fact-quality gate (finite verb / not a label) is now the real noise filter,
/// not this blunt length floor.
const MIN_SENTENCE_BYTES: usize = 12;

/// Emit ONE `markdown::paragraph@v1` witness PER SENTENCE in a prose chunk,
/// each anchored to its own byte sub-range. This is the granularity fix that
/// turns a multi-fact paragraph into multiple individually-retrievable claims
/// (so the cognition graph grows past the hybrid vector-search threshold and
/// each claim is a single searchable fact) — fully deterministic, zero-LLM,
/// consistent with the Witness-Mesh "structural only at compile time" design.
///
/// Segmentation: split on ASCII `.`/`!`/`?` followed by whitespace or
/// end-of-chunk. ASCII terminators are single-byte, so the byte offsets stay
/// UTF-8-exact even when a sentence contains multibyte characters. Falls back
/// to a single whole-chunk witness when the paragraph has fewer than two
/// substantive sentences (preserving prior behaviour for one-liners and never
/// dropping a claim).
#[allow(clippy::too_many_arguments)]
fn push_prose_sentence_witnesses(
    out: &mut Vec<Witness>,
    chunk: &Chunk,
    source_bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) {
    let start = chunk.byte_start as usize;
    let end = (chunk.byte_end as usize).min(source_bytes.len());
    if end <= start {
        return;
    }
    let slice = &source_bytes[start..end];

    // Decode the chunk's byte slice. On invalid UTF-8 we cannot run the
    // SRX-style segmenter, so fall back to a single whole-chunk witness
    // (never drop a claim on uncertainty).
    let text = match std::str::from_utf8(slice) {
        Ok(t) => t,
        Err(_) => {
            push_chunk_witness(
                out, chunk, "markdown::paragraph@v1", "documents::paragraph",
                source_bytes, file_blake3, source_id, workspace_id, now,
            );
            return;
        }
    };

    // Atomic-unit segmentation (2026-06-01): SRX-style sentence boundaries
    // (abbreviation/decimal/initial/ellipsis-aware) then ClausIE-lite clause
    // splitting so a compound sentence yields one retrievable unit per fact.
    // Offsets are bytes relative to `slice`, preserving exact anchoring.
    let mut units: Vec<(usize, usize)> = Vec::new();
    for sent in crate::segment::sentence_spans(text) {
        units.extend(crate::segment::clause_spans(text, sent));
    }

    let kept: Vec<(usize, usize)> = units
        .into_iter()
        .filter(|(s, e)| e.saturating_sub(*s) >= MIN_SENTENCE_BYTES)
        // Mechanical fact-quality gate: drop label/heading fragments that
        // survive byte-length but carry no asserted fact ("Additional Tips:").
        .filter(|(s, e)| crate::fact_quality::is_useful_fact(&text[*s..*e]))
        .collect();

    // No substantive unit survived the gate. Fall back to a single whole-chunk
    // witness so a real one-liner is never lost — UNLESS the whole chunk is
    // itself a low-value fragment (a heading/label misrouted as prose), in
    // which case emitting it would re-introduce the pollution we just filtered.
    if kept.is_empty() {
        if crate::fact_quality::is_useful_fact(text) {
            push_chunk_witness(
                out,
                chunk,
                "markdown::paragraph@v1",
                "documents::paragraph",
                source_bytes,
                file_blake3,
                source_id,
                workspace_id,
                now,
            );
        }
        return;
    }

    for (s, e) in kept {
        let abs_start = (start + s) as u64;
        let abs_end = (start + e) as u64;
        let span = WitnessSpan {
            file_blake3: file_blake3.to_string(),
            start: abs_start,
            end: abs_end,
        };
        let content_blake3 = blake3::hash(&source_bytes[start + s..start + e])
            .to_hex()
            .to_string();
        let mut witness = Witness::new(
            "markdown::paragraph@v1".to_string(),
            "documents::paragraph".to_string(),
            vec![WitnessInput::ByteRef {
                file_blake3: file_blake3.to_string(),
                start: abs_start,
                end: abs_end,
            }],
            vec![span],
            source_id,
            workspace_id,
            Sensitivity::Public,
            Confidence::new(0.99),
            content_blake3,
            now,
        );
        // Temporal anchoring (2026-06-01): if the unit states an absolute date
        // ("On April 10, 2023 …"), anchor the witness's `valid_from` to that
        // EVENT time so `/claims/as-of` + temporal-reasoning queries resolve
        // against the fact's real date, not ingestion time. Mechanical, no LLM.
        if let Some(event_date) = crate::temporal::extract_event_date(&text[s..e]) {
            witness = witness.with_valid_from(event_date);
        }
        out.push(witness);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_at(content: &str, ty: ChunkType, byte_start: u64, byte_end: u64) -> Chunk {
        let mut c = Chunk::new(content, ty, 1, 1);
        c.byte_start = byte_start;
        c.byte_end = byte_end;
        c
    }

    #[test]
    fn binary_guard_keeps_text_rejects_pdf_bytes() {
        assert!(bytes_are_probably_text(b"The deployment uses blue-green rollout."));
        assert!(bytes_are_probably_text(b"# Heading\n\nSome prose with code() and links."));
        assert!(!bytes_are_probably_text(b"<</N 3\n/Filter /FlateDecode\n/Length 294>> stream"));
        assert!(!bytes_are_probably_text(&[0x44, 0x00, 0x4f, 0x01, 0x5a, 0x1e, 0x00, 0x05, 0x1a, 0x63]));
        assert!(!bytes_are_probably_text(b""));
    }

    #[test]
    fn prose_chunk_of_binary_emits_no_witness() {
        let source: &[u8] = &[0x25, 0x50, 0x44, 0x46, 0x00, 0x01, 0x02, 0x03, 0xff, 0xfe, 0x80, 0x81];
        let chunk = chunk_at("\u{0}\u{1}\u{2}", ChunkType::Prose, 0, source.len() as u64);
        let out = extract_witnesses_from_chunk(
            &chunk, source, "blake", SourceId::new(), WorkspaceId::new(), Utc::now(),
        );
        assert!(out.witnesses.is_empty(), "binary chunk must emit zero witnesses");
    }

    #[test]
    fn heading_chunk_emits_heading_witness() {
        let source = b"# Title\n";
        let chunk = chunk_at("# Title", ChunkType::Heading, 0, 7);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source,
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.witnesses.len(), 1);
        assert_eq!(out.witnesses[0].rule, "markdown::heading@v1");
        assert_eq!(out.witnesses[0].witness_type, "documents::heading");
    }

    #[test]
    fn prose_chunk_emits_paragraph_witness() {
        // A real sentence (finite verb + terminal punctuation) must emit.
        let source = b"The system stores facts.\n";
        let chunk = chunk_at("The system stores facts.", ChunkType::Prose, 0, 24);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source,
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.witnesses.len(), 1);
        assert_eq!(out.witnesses[0].rule, "markdown::paragraph@v1");
    }

    #[test]
    fn dated_prose_anchors_valid_from_to_event_date() {
        use chrono::Datelike;
        // A prose sentence stating an absolute date must anchor the witness's
        // bitemporal `valid_from` to that EVENT date, not ingestion time.
        let source = b"On 2023/04/10 I bought a new electric car.\n";
        let chunk = chunk_at(
            "On 2023/04/10 I bought a new electric car.",
            ChunkType::Prose,
            0,
            42,
        );
        let out = extract_witnesses_from_chunk(
            &chunk,
            source,
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.witnesses.len(), 1);
        let w = &out.witnesses[0];
        assert_eq!(
            (w.valid_from.year(), w.valid_from.month(), w.valid_from.day()),
            (2023, 4, 10),
            "valid_from should be anchored to the stated event date"
        );
    }

    #[test]
    fn prose_fragment_is_dropped() {
        // Mechanical fact-quality gate (2026-06-01): a verbless, unpunctuated
        // noun-phrase prose chunk carries no fact and must NOT become a witness.
        let source = b"a paragraph\n";
        let chunk = chunk_at("a paragraph", ChunkType::Prose, 0, 11);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source,
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(
            out.witnesses.is_empty(),
            "expected fragment to be dropped, got {:?}",
            out.witnesses.iter().map(|w| &w.rule).collect::<Vec<_>>()
        );
    }

    #[test]
    fn prose_chunk_with_commit_author_skips_paragraph_emit() {
        let mut chunk = chunk_at("commit 1234", ChunkType::Prose, 0, 11);
        chunk.metadata.commit_author = Some("Alice".into());
        let out = extract_witnesses_from_chunk(
            &chunk,
            b"commit 1234\n",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        // Git chunks are owned by a different adapter (Commit 2's
        // git_rules); this adapter steps back.
        assert!(out.witnesses.is_empty());
    }

    #[test]
    fn list_chunk_emits_list_item_witness() {
        let source = b"- item 1\n- item 2\n";
        let chunk = chunk_at("- item 1\n- item 2", ChunkType::List, 0, 17);
        let out = extract_witnesses_from_chunk(
            &chunk,
            source,
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.witnesses.len(), 1);
        assert_eq!(out.witnesses[0].rule, "markdown::list-item@v1");
    }

    #[test]
    fn code_chunk_with_language_emits_code_block_witness() {
        let source = b"```rust\nfn x() {}\n```\n";
        let mut chunk = chunk_at("```rust\nfn x() {}\n```", ChunkType::Code, 0, 21);
        chunk.language = Some("rust".into());
        let out = extract_witnesses_from_chunk(
            &chunk,
            source,
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.witnesses.len(), 1);
        assert_eq!(out.witnesses[0].rule, "markdown::code-block@v1");
    }

    #[test]
    fn code_chunk_without_language_emits_nothing() {
        let chunk = chunk_at("raw code", ChunkType::Code, 0, 8);
        let out = extract_witnesses_from_chunk(
            &chunk,
            b"raw code",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(out.witnesses.is_empty());
    }

    #[test]
    fn rustdoc_param_doctag_emits_param_doc_witness() {
        let source = b"/// @param x the input\nfn f(x: i32) {}\n";
        let mut chunk = chunk_at(
            "/// @param x the input",
            ChunkType::Comment,
            0,
            22,
        );
        chunk.language = Some("rust".into());
        chunk.metadata.doc_tags = vec![DocTag {
            kind: "param".into(),
            name: Some("x".into()),
            description: "the input".into(),
        }];
        let out = extract_witnesses_from_chunk(
            &chunk,
            source,
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.witnesses.len(), 1);
        assert_eq!(out.witnesses[0].rule, "rustdoc::param-doc@v1");
        assert_eq!(out.witnesses[0].witness_type, "documents::param-doc");
    }

    #[test]
    fn jsdoc_param_doctag_emits_jsdoc_param_witness() {
        let mut chunk = chunk_at(
            "/** @param x desc */",
            ChunkType::Comment,
            0,
            20,
        );
        chunk.language = Some("typescript".into());
        chunk.metadata.doc_tags = vec![DocTag {
            kind: "param".into(),
            name: Some("x".into()),
            description: "desc".into(),
        }];
        let out = extract_witnesses_from_chunk(
            &chunk,
            b"/** @param x desc */",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.witnesses.len(), 1);
        assert_eq!(out.witnesses[0].rule, "jsdoc::param@v1");
    }

    #[test]
    fn javadoc_param_doctag_emits_javadoc_param_witness() {
        let mut chunk = chunk_at(
            "/** @param x desc */",
            ChunkType::Comment,
            0,
            20,
        );
        chunk.language = Some("java".into());
        chunk.metadata.doc_tags = vec![DocTag {
            kind: "param".into(),
            name: Some("x".into()),
            description: "desc".into(),
        }];
        let out = extract_witnesses_from_chunk(
            &chunk,
            b"/** @param x desc */",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.witnesses.len(), 1);
        assert_eq!(out.witnesses[0].rule, "javadoc::param@v1");
    }

    #[test]
    fn unsupported_language_doctag_emits_nothing() {
        // Doc-tags on a Haskell chunk — the v1.0 catalog has no
        // `haskelldoc` family, so we honestly emit no Witness.
        let mut chunk = chunk_at("-- @param x desc", ChunkType::Comment, 0, 16);
        chunk.language = Some("haskell".into());
        chunk.metadata.doc_tags = vec![DocTag {
            kind: "param".into(),
            name: Some("x".into()),
            description: "desc".into(),
        }];
        let out = extract_witnesses_from_chunk(
            &chunk,
            b"-- @param x desc",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(out.witnesses.is_empty());
    }

    #[test]
    fn sentinel_byte_range_emits_nothing() {
        let chunk = chunk_at("# H", ChunkType::Heading, 0, 0);
        let out = extract_witnesses_from_chunk(
            &chunk,
            b"# H",
            "f",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(out.witnesses.is_empty());
    }
}
