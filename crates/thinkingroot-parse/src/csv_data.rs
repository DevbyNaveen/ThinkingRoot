//! Wedge 4: deterministic CSV / TSV row extractor.
//!
//! Each non-header record becomes a `ChunkType::DataRow` chunk whose
//! `row_columns` map header → cell. The first non-empty record is treated
//! as the header. RFC 4180 quoting (quoted fields, embedded newlines,
//! doubled quotes) is honored via the `csv` crate's defaults.
//!
//! There is **no row cap**. Files that don't fit the workspace's
//! `parsers.max_file_size` budget are skipped wholesale by the walker
//! before this parser runs, so partial truncation never happens.

use std::path::Path;

use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId, SourceMetadata, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse a delimited file. `delimiter` is `b','` for CSV or `b'\t'` for TSV.
pub fn parse(path: &Path, delimiter: u8) -> Result<DocumentIR> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::io_path(path, e))?;
    parse_content(path, &content, delimiter)
}

pub(crate) fn parse_content(path: &Path, content: &str, delimiter: u8) -> Result<DocumentIR> {
    let hash = ContentHash::from_bytes(content.as_bytes());

    let mut doc = DocumentIR::new(
        SourceId::new(),
        path.to_string_lossy().to_string(),
        SourceType::File,
    );
    doc.content_hash = hash;
    doc.metadata = SourceMetadata {
        file_extension: path.extension().and_then(|e| e.to_str()).map(String::from),
        relative_path: Some(path.to_string_lossy().to_string()),
        ..Default::default()
    };

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(true)
        .flexible(true)
        .from_reader(content.as_bytes());

    let headers: Vec<String> = match reader.headers() {
        Ok(h) => h.iter().map(str::to_string).collect(),
        Err(e) => {
            tracing::warn!(
                "csv header read failed at {}: {e}; falling back to text",
                path.display()
            );
            return crate::markdown::parse_as_text(path);
        }
    };

    if headers.is_empty() {
        // Empty file → no rows to emit. Surface an empty document so the
        // file is still acknowledged but produces no claims.
        return Ok(doc);
    }

    let mut row_idx: u32 = 0;
    for record in reader.records() {
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "csv row {row_idx} read failed at {}: {e}; skipping",
                    path.display()
                );
                continue;
            }
        };

        let columns: Vec<(String, String)> = headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let cell = record.get(i).unwrap_or("").to_string();
                (h.clone(), cell)
            })
            .collect();

        let display = columns
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" | ");

        let line_no = row_idx + 2; // header is line 1, first data row line 2
        let mut chunk = Chunk::new(display, ChunkType::DataRow, line_no, line_no);
        chunk.metadata = ChunkMetadata {
            row_index: Some(row_idx),
            row_columns: columns,
            ..Default::default()
        };
        doc.add_chunk(chunk);
        row_idx += 1;
    }

    doc.fill_byte_ranges(content);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse_str(content: &str, delim: u8) -> DocumentIR {
        parse_content(&PathBuf::from("test.csv"), content, delim).expect("csv parse")
    }

    #[test]
    fn csv_emits_one_row_per_record_with_header_columns() {
        let doc = parse_str("name,age\nalice,30\nbob,25\n", b',');
        let rows: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::DataRow)
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].metadata.row_index, Some(0));
        assert_eq!(rows[1].metadata.row_index, Some(1));
        let r0 = &rows[0].metadata.row_columns;
        assert!(r0.iter().any(|(k, v)| k == "name" && v == "alice"));
        assert!(r0.iter().any(|(k, v)| k == "age" && v == "30"));
    }

    #[test]
    fn csv_handles_quoted_fields_and_embedded_newlines() {
        let doc = parse_str("name,bio\n\"alice\",\"line1\nline2\"\n", b',');
        let rows: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::DataRow)
            .collect();
        assert_eq!(rows.len(), 1);
        let bio = rows[0]
            .metadata
            .row_columns
            .iter()
            .find(|(k, _)| k == "bio")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(bio.contains("line1\nline2"));
    }

    #[test]
    fn csv_handles_doubled_quotes() {
        let doc = parse_str("col\n\"she said \"\"hi\"\"\"\n", b',');
        let cell = doc.chunks[0]
            .metadata
            .row_columns
            .iter()
            .find(|(k, _)| k == "col")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(cell, "she said \"hi\"");
    }

    #[test]
    fn tsv_uses_tab_delimiter() {
        let doc = parse_str("a\tb\n1\t2\n", b'\t');
        let rows: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::DataRow)
            .collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].metadata.row_columns.iter().any(|(k, v)| k == "a" && v == "1"));
        assert!(rows[0].metadata.row_columns.iter().any(|(k, v)| k == "b" && v == "2"));
    }

    #[test]
    fn csv_empty_file_emits_no_rows() {
        let doc = parse_str("", b',');
        assert!(doc.chunks.is_empty());
    }

    #[test]
    fn csv_short_row_pads_missing_cells_as_empty() {
        // Header has 3 columns; second row has only 2.  The third cell
        // becomes an empty string, not a panic.
        let doc = parse_str("a,b,c\n1,2,3\nx,y\n", b',');
        let rows: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::DataRow)
            .collect();
        assert_eq!(rows.len(), 2);
        let last = &rows[1].metadata.row_columns;
        assert!(last.iter().any(|(k, v)| k == "c" && v.is_empty()));
    }
}
