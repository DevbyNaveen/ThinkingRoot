//! Video-file metadata parser.
//!
//! Videos carry no chunkable text, so the parser's job is narrow:
//! register the file as a `Source` (with content hash) and return a
//! chunkless [`DocumentIR`]. The Witness Mesh feature extractors in
//! `thinkingroot-extract::video_rules` consume the file bytes at
//! extract time and emit per-rule witnesses (duration, per-keyframe,
//! scene-change inference, honest-absence skipped).
//!
//! Mirrors the `image_meta::parse` / `audio_meta::parse` pattern:
//! read bytes once, BLAKE3-hash them, populate `source_id` +
//! `content_hash`, return — no chunk emission.

use std::path::Path;

use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::{ContentHash, SourceId, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse a video file into a chunkless `DocumentIR`.
///
/// Reads the file bytes once to compute the BLAKE3 content hash; the
/// bytes themselves are re-read by the extractor when the rule
/// modules run. Capped at 1 GiB by the extractor's per-file budget,
/// not enforced here — small video files compile fast; oversized
/// uploads will surface a `video::skipped@v1` witness with the
/// reason field documenting the cap.
pub fn parse(path: &Path) -> Result<DocumentIR> {
    let bytes = std::fs::read(path).map_err(|e| Error::io_path(path, e))?;
    let content_hash = ContentHash::from_bytes(&bytes);
    let uri = format!("{}", path.display());
    let source_id = SourceId::new();
    let mut doc = DocumentIR::new(source_id, uri, SourceType::File);
    doc.content_hash = content_hash;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_produces_chunkless_document_with_real_hash() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // ISO BMFF magic — `ftyp` box header at offset 4. Real MP4
        // demux happens in `video_rules`, not here; the parser only
        // hashes.
        let mp4_header =
            b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isomavc1moov";
        tmp.as_file().write_all(mp4_header).unwrap();
        let path = tmp.path();
        let doc = parse(path).unwrap();
        assert!(doc.chunks.is_empty(), "video documents carry no chunks");
        assert!(
            !doc.content_hash.is_empty(),
            "content hash must be populated"
        );
        assert_eq!(doc.content_hash, ContentHash::from_bytes(mp4_header));
    }
}
