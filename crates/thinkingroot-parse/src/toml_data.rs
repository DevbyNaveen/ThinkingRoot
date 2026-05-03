//! Wedge 4: deterministic TOML config-tree extractor.
//!
//! Walks the parsed `toml::Value` tree and emits one `ChunkType::ConfigEntry`
//! chunk per leaf scalar (and per table header) so the structural extractor
//! can produce key=value claims without ever calling the LLM.
//!
//! Path syntax: dotted keys for tables (`database.pool_size`); `[N]` for
//! array indices (`servers[0].host`).
//!
//! Manifest files (Cargo.toml / pyproject.toml) are *not* routed here — they
//! keep going through `manifest::parse` for richer dependency-aware handling.
//! `lib.rs` enforces the precedence by filename match.

use std::path::Path;

use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId, SourceMetadata, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse a generic TOML file into one `ConfigEntry` chunk per leaf scalar
/// plus one section-header chunk per non-empty table.
pub fn parse(path: &Path) -> Result<DocumentIR> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::io_path(path, e))?;
    parse_content(path, &content)
}

pub(crate) fn parse_content(path: &Path, content: &str) -> Result<DocumentIR> {
    let hash = ContentHash::from_bytes(content.as_bytes());

    let mut doc = DocumentIR::new(
        SourceId::new(),
        path.to_string_lossy().to_string(),
        SourceType::File,
    );
    doc.content_hash = hash;
    doc.metadata = SourceMetadata {
        file_extension: Some("toml".to_string()),
        relative_path: Some(path.to_string_lossy().to_string()),
        ..Default::default()
    };

    let value: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(e) => {
            // Bad TOML is non-fatal — degrade to a single text chunk so the
            // file isn't silently dropped from the graph.  Same policy as
            // markdown::parse_as_text.
            tracing::warn!(
                "toml parse failed at {}: {e}; falling back to text",
                path.display()
            );
            return crate::markdown::parse_as_text(path);
        }
    };

    walk_value(&value, "", content, &mut doc);

    // Backfill byte ranges for any chunk whose content matched in source order.
    // toml::Value loses span info on the stable crate, so we rely on the
    // substring-search backfill that markdown / manifest already use.
    doc.fill_byte_ranges(content);

    Ok(doc)
}

fn walk_value(value: &toml::Value, prefix: &str, source: &str, doc: &mut DocumentIR) {
    match value {
        toml::Value::Table(table) => {
            // Emit the section header as a ConfigEntry of type "table"
            // (only for non-root tables — the root has no name).
            if !prefix.is_empty() {
                emit_section(prefix, "table", source, doc);
            }
            for (key, child) in table {
                let next = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk_value(child, &next, source, doc);
            }
        }
        toml::Value::Array(items) => {
            // Always record the array itself as a "array" entry so the
            // graph knows the path exists even when items are non-scalar.
            if !prefix.is_empty() {
                emit_section(prefix, "array", source, doc);
            }
            for (idx, child) in items.iter().enumerate() {
                let next = format!("{prefix}[{idx}]");
                walk_value(child, &next, source, doc);
            }
        }
        toml::Value::String(s) => emit_scalar(prefix, s, "string", source, doc),
        toml::Value::Integer(i) => emit_scalar(prefix, &i.to_string(), "int", source, doc),
        toml::Value::Float(f) => emit_scalar(prefix, &f.to_string(), "float", source, doc),
        toml::Value::Boolean(b) => emit_scalar(prefix, &b.to_string(), "bool", source, doc),
        toml::Value::Datetime(dt) => emit_scalar(prefix, &dt.to_string(), "string", source, doc),
    }
}

fn emit_scalar(key: &str, value: &str, ty: &str, _source: &str, doc: &mut DocumentIR) {
    if key.is_empty() {
        return;
    }
    let display = format!("{key} = {value}");
    let mut chunk = Chunk::new(display.clone(), ChunkType::ConfigEntry, 1, 1);
    chunk.metadata = ChunkMetadata {
        config_key: Some(key.to_string()),
        config_value: Some(value.to_string()),
        config_value_type: Some(ty.to_string()),
        ..Default::default()
    };
    doc.add_chunk(chunk);
}

fn emit_section(key: &str, ty: &str, _source: &str, doc: &mut DocumentIR) {
    let display = format!("[{key}]");
    let mut chunk = Chunk::new(display, ChunkType::ConfigEntry, 1, 1);
    chunk.metadata = ChunkMetadata {
        config_key: Some(key.to_string()),
        config_value: None,
        config_value_type: Some(ty.to_string()),
        ..Default::default()
    };
    doc.add_chunk(chunk);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse_str(content: &str) -> DocumentIR {
        parse_content(&PathBuf::from("test.toml"), content).expect("toml parse")
    }

    #[test]
    fn toml_extracts_scalar_leaves() {
        let doc = parse_str("name = \"thinkingroot\"\nport = 8080\nenabled = true\n");
        let names: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(names.contains(&"name"));
        assert!(names.contains(&"port"));
        assert!(names.contains(&"enabled"));
    }

    #[test]
    fn toml_extracts_nested_tables_with_dotted_paths() {
        let doc = parse_str("[database]\npool_size = 10\n[database.timeouts]\nconnect = 30\n");
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"database.pool_size"));
        assert!(keys.contains(&"database.timeouts.connect"));
        // Section headers are also emitted.
        assert!(keys.contains(&"database"));
        assert!(keys.contains(&"database.timeouts"));
    }

    #[test]
    fn toml_extracts_arrays_with_indexed_paths() {
        let doc = parse_str("hosts = [\"a\", \"b\", \"c\"]\n");
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"hosts"));
        assert!(keys.contains(&"hosts[0]"));
        assert!(keys.contains(&"hosts[2]"));
    }

    #[test]
    fn toml_records_value_types() {
        let doc = parse_str("name = \"x\"\nport = 8080\nratio = 0.5\nenabled = false\n");
        let by_key = |k: &str| {
            doc.chunks
                .iter()
                .find(|c| c.metadata.config_key.as_deref() == Some(k))
                .and_then(|c| c.metadata.config_value_type.clone())
                .unwrap_or_default()
        };
        assert_eq!(by_key("name"), "string");
        assert_eq!(by_key("port"), "int");
        assert_eq!(by_key("ratio"), "float");
        assert_eq!(by_key("enabled"), "bool");
    }

    #[test]
    fn toml_invalid_input_falls_back_to_text() {
        // Unbalanced quote -> parse failure -> text fallback.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "broken = \"unterminated\n").unwrap();
        let doc = parse(tmp.path()).expect("fallback parse");
        // Text fallback emits a single Prose chunk, not ConfigEntry.
        assert!(
            doc.chunks
                .iter()
                .all(|c| !matches!(c.chunk_type, ChunkType::ConfigEntry)),
            "fallback should not produce ConfigEntry chunks"
        );
    }
}
