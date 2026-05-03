//! Code-marker regex pass — Compile Completeness Contract §4.13.
//!
//! Emits one `code_markers` row per
//! `(TODO|FIXME|HACK|SAFETY|NOTE|XXX|BUG|PERF):? \s+ <text>` hit found
//! inside Code, Comment, or ModuleDoc chunks. The byte range stamped
//! onto each row is the **chunk's** byte range, not the per-marker
//! sub-range — which is correct for I-3 byte-coverage purposes (the
//! chunk's bytes are claimed by the marker rows even when there are
//! several markers in one chunk). Per-marker sub-ranges are a v1.1
//! refinement once the regex layer threads `Match::start/end` back
//! to absolute file-local bytes.

use std::sync::OnceLock;

use regex::Regex;
use thinkingroot_core::ir::Chunk;
use thinkingroot_extract::ExtractionOutput;
use thinkingroot_graph::{Blake3Cache, rows::CodeMarker};

use super::stable_row_id;

pub(super) fn emit(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    out: &mut Vec<CodeMarker>,
    extraction: &ExtractionOutput,
) {
    let re = marker_regex();
    let mut hits: Vec<(String, String)> = Vec::new();
    for caps in re.captures_iter(&chunk.content) {
        let kind = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
        let text = caps.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
        if !kind.is_empty() {
            hits.push((kind, text));
        }
    }
    if hits.is_empty() {
        return;
    }

    // Find the owning claim (if any) by exact byte-range match. Code
    // markers attach to the claim that covers their chunk so AEP can
    // surface "open TODOs in <claim>" without an extra join.
    let in_claim_id = extraction
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

    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();
    for (idx, (kind, text)) in hits.into_iter().enumerate() {
        let id = stable_row_id(
            "code_markers",
            source_id,
            chunk.byte_start,
            chunk.byte_end,
            &format!("{idx}|{kind}"),
        );
        out.push(CodeMarker {
            id,
            source_id: source_id.to_string(),
            kind,
            text,
            in_claim_id: in_claim_id.clone(),
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
            content_blake3: blake3_str.clone(),
        });
    }
}

fn marker_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?m)\b(TODO|FIXME|HACK|SAFETY|NOTE|XXX|BUG|PERF)\b:?\s+([^\r\n]+)")
            .expect("valid marker regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::ir::{Chunk, ChunkType};

    fn mk_chunk(content: &str) -> Chunk {
        let mut c = Chunk::new(content.to_string(), ChunkType::Comment, 1, 1);
        c.byte_start = 0;
        c.byte_end = content.len() as u64;
        c
    }

    #[test]
    fn picks_up_todo_and_fixme() {
        let chunk = mk_chunk(
            "// TODO: rename the variable\n// FIXME: handle null case\n// regular comment",
        );
        let bytes = chunk.content.as_bytes().to_vec();
        let mut cache = Blake3Cache::new(&bytes);
        let mut out = Vec::new();
        let extraction = ExtractionOutput::default();
        emit(&chunk, "src", &mut cache, &mut out, &extraction);
        assert_eq!(out.len(), 2);
        let kinds: Vec<&str> = out.iter().map(|m| m.kind.as_str()).collect();
        assert!(kinds.contains(&"TODO"));
        assert!(kinds.contains(&"FIXME"));
    }

    #[test]
    fn does_not_match_inside_word() {
        // "TODOs" and "XXXing" embed marker keywords but with adjacent
        // word characters — the trailing `\b` in the regex prevents
        // matches mid-identifier.
        let chunk = mk_chunk("// TODOs are tracked elsewhere\n// XXXing is benign");
        let bytes = chunk.content.as_bytes().to_vec();
        let mut cache = Blake3Cache::new(&bytes);
        let mut out = Vec::new();
        let extraction = ExtractionOutput::default();
        emit(&chunk, "src", &mut cache, &mut out, &extraction);
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn captures_marker_text() {
        let chunk = mk_chunk("// SAFETY: ptr is non-null because reasons");
        let bytes = chunk.content.as_bytes().to_vec();
        let mut cache = Blake3Cache::new(&bytes);
        let mut out = Vec::new();
        let extraction = ExtractionOutput::default();
        emit(&chunk, "src", &mut cache, &mut out, &extraction);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "SAFETY");
        assert_eq!(out[0].text, "ptr is non-null because reasons");
    }
}
