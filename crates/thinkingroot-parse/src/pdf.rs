use std::path::Path;

use thinkingroot_core::ir::{Chunk, ChunkType, DocumentIR};
use thinkingroot_core::types::*;
use thinkingroot_core::{Error, Result};

/// Parse a PDF file into a DocumentIR.
/// Uses pdf-extract for text extraction, then chunks by paragraph boundaries.
pub fn parse(path: &Path) -> Result<DocumentIR> {
    let content = std::fs::read(path).map_err(|e| Error::io_path(path, e))?;
    let hash = ContentHash::from_bytes(&content);

    let text = pdf_extract::extract_text_from_mem(&content).map_err(|e| Error::Parse {
        source_path: path.to_path_buf(),
        message: format!("PDF extraction failed: {e}"),
    })?;

    let uri = format!("{}", path.display());
    let source_id = SourceId::new();
    let mut doc = DocumentIR::new(source_id, uri, SourceType::Document);
    doc.content_hash = hash;

    if text.trim().is_empty() {
        return Ok(doc);
    }

    // Split by double newlines (paragraph boundaries) for semantic chunking.
    let paragraphs: Vec<String> = text
        .split("\n\n")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut line = 1u32;
    for para in &paragraphs {
        let line_count = para.lines().count() as u32;
        let chunk = Chunk::new(para, ChunkType::Prose, line, line + line_count);
        doc.add_chunk(chunk);
        line += line_count + 1;
    }

    // PDF byte ranges refer to offsets within the EXTRACTED TEXT (not the
    // original PDF binary). Persist the extracted text as the source's
    // `anchored_text` so the byte store holds IT (not the raw PDF bytes) and
    // `materialize_statement` returns real text — not FlateDecode noise.
    // Without this, witnesses anchored to text offsets resolve against the
    // binary file and recall surfaces garbage.
    doc.fill_byte_ranges(&text);
    doc.anchored_text = Some(text);

    Ok(doc)
}

#[cfg(test)]
mod tests {
    // PDF tests require actual PDF files — tested via integration tests.
}
