//! Audio-file metadata parser.
//!
//! Mirrors `image_meta::parse`: read bytes once, BLAKE3-hash them,
//! return a chunkless `DocumentIR`. The Witness Mesh feature
//! extractors in `thinkingroot-extract::audio_rules` run at
//! extract time and emit per-rule witnesses (duration, spectral
//! fingerprint, decode-fail honest absence).

use std::path::Path;

use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::{ContentHash, SourceId, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse an audio file into a chunkless `DocumentIR`. Audio content
/// is dense PCM — chunking it would require codec-aware framing
/// the rule layer already handles. Whole-file anchor is the honest
/// model.
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
        let bytes = b"RIFF\x00\x00\x00\x00WAVEfmt fake";
        tmp.as_file().write_all(bytes).unwrap();
        let path = tmp.path();
        let doc = parse(path).unwrap();
        assert!(doc.chunks.is_empty());
        assert_eq!(doc.content_hash, ContentHash::from_bytes(bytes));
    }
}
