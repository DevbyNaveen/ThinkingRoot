//! Wedge 4: deterministic YAML config-tree extractor.
//!
//! Walks a parsed `serde_yaml::Value` tree and emits one
//! `ChunkType::ConfigEntry` chunk per leaf scalar. Mapping nodes also emit a
//! "table" header chunk so cross-section navigation works the same way it
//! does for TOML.
//!
//! Anchors and aliases are resolved to their referent value at parse time
//! (we are extracting facts, not preserving syntax). Multi-document YAML
//! files have each document walked under a `doc[N]` prefix.

use std::path::Path;

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
        file_extension: path.extension().and_then(|e| e.to_str()).map(String::from),
        relative_path: Some(path.to_string_lossy().to_string()),
        ..Default::default()
    };

    // serde_yaml::Deserializer iterates over multi-document YAML. Single-doc
    // files yield one element.
    let docs: Vec<serde_yaml::Value> =
        match serde_yaml::Deserializer::from_str(content)
            .map(serde_yaml::Value::deserialize)
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "yaml parse failed at {}: {e}; falling back to text",
                    path.display()
                );
                return crate::markdown::parse_as_text(path);
            }
        };

    if docs.len() == 1 {
        walk(&docs[0], "", &mut doc);
    } else {
        for (idx, value) in docs.iter().enumerate() {
            walk(value, &format!("doc[{idx}]"), &mut doc);
        }
    }

    doc.fill_byte_ranges(content);
    Ok(doc)
}

use serde::Deserialize;

fn walk(value: &serde_yaml::Value, prefix: &str, doc: &mut DocumentIR) {
    match value {
        serde_yaml::Value::Mapping(map) => {
            if !prefix.is_empty() {
                emit_section(prefix, "table", doc);
            }
            for (k, v) in map {
                let key_str = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    other => format!("{other:?}"),
                };
                let next = if prefix.is_empty() {
                    key_str
                } else {
                    format!("{prefix}.{key_str}")
                };
                walk(v, &next, doc);
            }
        }
        serde_yaml::Value::Sequence(items) => {
            if !prefix.is_empty() {
                emit_section(prefix, "array", doc);
            }
            for (idx, child) in items.iter().enumerate() {
                let next = format!("{prefix}[{idx}]");
                walk(child, &next, doc);
            }
        }
        serde_yaml::Value::String(s) => emit_scalar(prefix, s, "string", doc),
        serde_yaml::Value::Number(n) => {
            let ty = if n.is_f64() { "float" } else { "int" };
            emit_scalar(prefix, &n.to_string(), ty, doc);
        }
        serde_yaml::Value::Bool(b) => emit_scalar(prefix, &b.to_string(), "bool", doc),
        serde_yaml::Value::Null => emit_scalar(prefix, "null", "null", doc),
        // Tagged values (`!!str`, etc.) — unwrap and continue.
        serde_yaml::Value::Tagged(tagged) => walk(&tagged.value, prefix, doc),
    }
}

fn emit_scalar(key: &str, value: &str, ty: &str, doc: &mut DocumentIR) {
    if key.is_empty() {
        return;
    }
    let display = format!("{key}: {value}");
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
    let display = format!("{key}:");
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
        parse_content(&PathBuf::from("test.yaml"), content).expect("yaml parse")
    }

    #[test]
    fn yaml_extracts_scalars() {
        let doc = parse_str("name: thinkingroot\nport: 8080\nenabled: true\n");
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"name"));
        assert!(keys.contains(&"port"));
        assert!(keys.contains(&"enabled"));
    }

    #[test]
    fn yaml_extracts_nested_mappings() {
        let doc = parse_str("database:\n  pool_size: 10\n  timeouts:\n    connect: 30\n");
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"database.pool_size"));
        assert!(keys.contains(&"database.timeouts.connect"));
    }

    #[test]
    fn yaml_extracts_sequences_with_indices() {
        let doc = parse_str("hosts:\n  - a\n  - b\n  - c\n");
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"hosts[0]"));
        assert!(keys.contains(&"hosts[2]"));
    }

    #[test]
    fn yaml_handles_anchors_and_aliases() {
        // serde_yaml resolves aliases to their target value automatically.
        let doc = parse_str("base: &base\n  port: 8080\nchild: *base\n");
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.contains(&"base.port"));
        assert!(
            keys.contains(&"child.port"),
            "alias should resolve to the same shape as the anchor: {keys:?}"
        );
    }

    #[test]
    fn yaml_handles_multi_document_files() {
        let doc = parse_str("---\nname: a\n---\nname: b\n");
        let keys: Vec<_> = doc
            .chunks
            .iter()
            .filter_map(|c| c.metadata.config_key.as_deref())
            .collect();
        assert!(keys.iter().any(|k| k.starts_with("doc[0].")));
        assert!(keys.iter().any(|k| k.starts_with("doc[1].")));
    }

    #[test]
    fn yaml_records_value_types() {
        let doc = parse_str("a: hi\nb: 42\nc: 1.5\nd: true\ne: null\n");
        let by_key = |k: &str| {
            doc.chunks
                .iter()
                .find(|c| c.metadata.config_key.as_deref() == Some(k))
                .and_then(|c| c.metadata.config_value_type.clone())
                .unwrap_or_default()
        };
        assert_eq!(by_key("a"), "string");
        assert_eq!(by_key("b"), "int");
        assert_eq!(by_key("c"), "float");
        assert_eq!(by_key("d"), "bool");
        assert_eq!(by_key("e"), "null");
    }
}
