//! Image-file metadata parser.
//!
//! Images carry no text to chunk, so the parser's job is narrow:
//! register the file as a `Source` (with content hash) and return a
//! chunkless [`DocumentIR`]. The Witness Mesh feature extractors in
//! `thinkingroot-extract::image_rules` consume the file bytes at
//! extract time and emit per-rule witnesses (perceptual hash,
//! colour histogram, edge summary, EXIF, dominant colours).
//!
//! Mirrors the `pdf::parse` pattern: read bytes once, BLAKE3-hash
//! them, populate `source_id` + `content_hash`, return — no chunk
//! emission.

use std::path::Path;

use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::{ContentHash, SourceId, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse an image file into a chunkless `DocumentIR`.
///
/// Reads the file bytes once to compute the BLAKE3 content hash;
/// the bytes themselves are re-read by the extractor when the rule
/// modules run (cheap — SSD; image files cap at 32 MiB per rule
/// budget). The returned document carries no chunks because image
/// content has no internal byte-range structure to chunk by — the
/// whole file is the witness anchor.
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
        let pixels = b"\x89PNG\r\n\x1a\nfake-but-hashable-bytes";
        tmp.as_file().write_all(pixels).unwrap();
        let path = tmp.path();
        let doc = parse(path).unwrap();
        assert!(doc.chunks.is_empty(), "image documents carry no chunks");
        assert!(!doc.content_hash.is_empty(), "content hash must be populated");
        assert_eq!(
            doc.content_hash,
            ContentHash::from_bytes(pixels),
            "content hash must equal blake3 of file bytes"
        );
        assert!(doc.uri.contains(path.file_name().unwrap().to_str().unwrap()));
    }
}
