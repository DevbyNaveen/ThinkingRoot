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
                push_chunk_witness(
                    &mut out.witnesses,
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
        assert_eq!(out.witnesses.len(), 1);
        assert_eq!(out.witnesses[0].rule, "markdown::paragraph@v1");
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
