//! Wedge 4: deterministic JSON config / data extractor.
//!
//! Two-shape emission per the locked-in design:
//!
//! 1. If the **top-level** value is an array of objects (every element is a
//!    JSON object), emit one `ChunkType::DataRow` chunk per element AND a
//!    `ChunkType::ConfigEntry` per leaf scalar with indexed paths
//!    (`[3].address.city`). Lossless on both row-shape and field-shape
//!    queries.
//!
//! 2. Every other shape (object, mixed array, scalar array, primitive)
//!    emits `ConfigEntry` per leaf scalar only.
//!
//! Path syntax: dotted keys for objects (`database.pool_size`); `[N]` for
//! array indices (`servers[0].host`).

use std::path::Path;

use serde_json::Value;
use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId, SourceMetadata, SourceType};
use thinkingroot_core::{Error, Result};

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
        file_extension: Some("json".to_string()),
        relative_path: Some(path.to_string_lossy().to_string()),
        ..Default::default()
    };

    let value: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "json parse failed at {}: {e}; falling back to text",
                path.display()
            );
            return crate::markdown::parse_as_text(path);
        }
    };

    // Hybrid emission for top-level array-of-objects.
    if let Value::Array(items) = &value
        && !items.is_empty()
        && items.iter().all(|v| v.is_object())
    {
        for (idx, obj) in items.iter().enumerate() {
            emit_data_row(idx as u32, obj, &mut doc);
        }
    }

    // Always also emit ConfigEntry leaves for full path coverage.
    walk(&value, "", &mut doc);

    doc.fill_byte_ranges(content);
    Ok(doc)
}

fn walk(value: &Value, prefix: &str, doc: &mut DocumentIR) {
    match value {
        Value::Object(map) => {
            if !prefix.is_empty() {
                emit_section(prefix, "table", doc);
            }
            for (k, v) in map {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                walk(v, &next, doc);
            }
        }
        Value::Array(items) => {
            if !prefix.is_empty() {
                emit_section(prefix, "array", doc);
            }
            for (idx, child) in items.iter().enumerate() {
                let next = format!("{prefix}[{idx}]");
                walk(child, &next, doc);
            }
        }
        Value::String(s) => emit_scalar(prefix, s, "string", doc),
        Value::Number(n) => {
            let ty = if n.is_f64() && !n.is_i64() && !n.is_u64() {
                "float"
            } else {
                "int"
            };
            emit_scalar(prefix, &n.to_string(), ty, doc);
        }
        Value::Bool(b) => emit_scalar(prefix, &b.to_string(), "bool", doc),
        Value::Null => emit_scalar(prefix, "null", "null", doc),
    }
}

fn emit_scalar(key: &str, value: &str, ty: &str, doc: &mut DocumentIR) {
    if key.is_empty() {
        return;
    }
    let display = format!("{key} = {value}");
    let mut chunk = Chunk::new(display, ChunkType::ConfigEntry, 1, 1);
    chunk.metadata = ChunkMetadata {
        config_key: Some(key.to_string()),
        config_value: Some(value.to_string()),
        config_value_type: Some(ty.to_string()),
        ..Default::default()
    };
    doc.add_chunk(chunk);
}

fn emit_section(key: &str, ty: &str, doc: &mut DocumentIR) {
    let display = if ty == "array" {
        format!("{key}[]")
    } else {
        format!("{{{key}}}")
    };
    let mut chunk = Chunk::new(display, ChunkType::ConfigEntry, 1, 1);
    chunk.metadata = ChunkMetadata {
        config_key: Some(key.to_string()),
        config_value: None,
        config_value_type: Some(ty.to_string()),
        ..Default::default()
    };
    doc.add_chunk(chunk);
}

fn emit_data_row(idx: u32, obj: &Value, doc: &mut DocumentIR) {
    let Value::Object(map) = obj else {
        return;
    };
    let columns: Vec<(String, String)> = map
        .iter()
        .map(|(k, v)| (k.clone(), render_scalar(v)))
        .collect();
    let display = columns
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" | ");
    let mut chunk = Chunk::new(display, ChunkType::DataRow, 1, 1);
    chunk.metadata = ChunkMetadata {
        row_index: Some(idx),
        row_columns: columns,
        ..Default::default()
    };
    doc.add_chunk(chunk);
}

fn render_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Nested objects/arrays inside a row cell are JSON-stringified — the
        // ConfigEntry pass still produces individual leaf claims for every
        // nested scalar via `walk`, so no information is lost.
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse_str(content: &str) -> DocumentIR {
        parse_content(&PathBuf::from("test.json"), content).expect("json parse")
    }

    #[test]
    fn json_object_emits_config_entries_only() {
        let doc = parse_str(r#"{"database":{"pool_size":10}}"#);
        assert!(doc.chunks.iter().all(|c| c.chunk_type != ChunkType::DataRow));
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"database.pool_size"));
    }

    #[test]
    fn json_top_level_array_of_objects_emits_hybrid() {
        let doc = parse_str(r#"[{"id":1,"name":"a"},{"id":2,"name":"b"}]"#);

        // DataRow per element.
        let rows: Vec<_> = doc
            .chunks
            .iter()
            .filter(|c| c.chunk_type == ChunkType::DataRow)
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].metadata.row_index, Some(0));
        assert_eq!(rows[1].metadata.row_index, Some(1));
        assert!(rows[0]
            .metadata
            .row_columns
            .iter()
            .any(|(k, v)| k == "name" && v == "a"));

        // ConfigEntry per leaf with indexed paths.
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"[0].name"));
        assert!(keys.contains(&"[1].id"));
    }

    #[test]
    fn json_mixed_top_level_array_emits_only_config_entries() {
        // Mixed types -> NOT eligible for DataRow hybrid.
        let doc = parse_str(r#"[1, {"id":2}, "three"]"#);
        assert!(doc.chunks.iter().all(|c| c.chunk_type != ChunkType::DataRow));
    }

    #[test]
    fn json_array_indexed_paths_use_bracket_syntax() {
        let doc = parse_str(r#"{"servers":[{"host":"a"},{"host":"b"}]}"#);
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"servers[0].host"));
        assert!(keys.contains(&"servers[1].host"));
    }

    #[test]
    fn json_records_value_types() {
        let doc = parse_str(r#"{"s":"x","i":7,"f":1.5,"b":true,"n":null}"#);
        let by_key = |k: &str| {
            doc.chunks
                .iter()
                .find(|c| c.metadata.config_key.as_deref() == Some(k))
                .and_then(|c| c.metadata.config_value_type.clone())
                .unwrap_or_default()
        };
        assert_eq!(by_key("s"), "string");
        assert_eq!(by_key("i"), "int");
        assert_eq!(by_key("f"), "float");
        assert_eq!(by_key("b"), "bool");
        assert_eq!(by_key("n"), "null");
    }

    #[test]
    fn json_invalid_input_falls_back_to_text() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "{ broken").unwrap();
        let doc = parse(tmp.path()).expect("fallback");
        assert!(doc.chunks.iter().all(|c| c.chunk_type != ChunkType::ConfigEntry));
    }
}
