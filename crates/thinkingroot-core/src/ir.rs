use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{ContentHash, SourceId, SourceMetadata, SourceType};

/// The Intermediate Representation produced by Stage 1 (Parse).
/// Every parser converts its input into this normalized format before
/// extraction begins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentIR {
    pub source_id: SourceId,
    pub uri: String,
    pub source_type: SourceType,
    pub timestamp: DateTime<Utc>,
    pub author: Option<String>,
    pub content_hash: ContentHash,
    pub chunks: Vec<Chunk>,
    pub metadata: SourceMetadata,
}

impl DocumentIR {
    pub fn new(source_id: SourceId, uri: String, source_type: SourceType) -> Self {
        Self {
            source_id,
            uri,
            source_type,
            timestamp: Utc::now(),
            author: None,
            content_hash: ContentHash::empty(),
            chunks: Vec::new(),
            metadata: SourceMetadata::default(),
        }
    }

    pub fn add_chunk(&mut self, chunk: Chunk) {
        self.chunks.push(chunk);
    }

    /// Total character count across all chunks.
    pub fn total_chars(&self) -> usize {
        self.chunks.iter().map(|c| c.content.len()).sum()
    }

    /// Total number of chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

/// A chunk is a semantically meaningful segment of the document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub content: String,
    pub chunk_type: ChunkType,
    pub start_line: u32,
    pub end_line: u32,
    pub heading: Option<String>,
    pub language: Option<String>,
    pub metadata: ChunkMetadata,
}

impl Chunk {
    pub fn new(
        content: impl Into<String>,
        chunk_type: ChunkType,
        start_line: u32,
        end_line: u32,
    ) -> Self {
        Self {
            content: content.into(),
            chunk_type,
            start_line,
            end_line,
            heading: None,
            language: None,
            metadata: ChunkMetadata::default(),
        }
    }

    pub fn with_heading(mut self, heading: impl Into<String>) -> Self {
        self.heading = Some(heading.into());
        self
    }

    pub fn with_language(mut self, lang: impl Into<String>) -> Self {
        self.language = Some(lang.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkType {
    /// Prose / narrative text (from markdown, docs).
    Prose,
    /// A code block or entire code file.
    Code,
    /// A heading / title.
    Heading,
    /// A list (ordered or unordered).
    List,
    /// A table.
    Table,
    /// A function or method definition.
    FunctionDef,
    /// A struct / class / type definition.
    TypeDef,
    /// An import / use statement.
    Import,
    /// A comment block.
    Comment,
    /// Module-level documentation.
    ModuleDoc,
}

/// Additional metadata for a chunk, depending on its type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkMetadata {
    /// For FunctionDef: the function name.
    pub function_name: Option<String>,
    /// For TypeDef: the type name.
    pub type_name: Option<String>,
    /// For FunctionDef: parameter signatures.
    pub parameters: Option<Vec<String>>,
    /// For FunctionDef: return type.
    pub return_type: Option<String>,
    /// For Import: the imported module/path.
    pub import_path: Option<String>,
    /// Visibility (pub, pub(crate), private).
    pub visibility: Option<String>,
    /// Parent scope name (e.g., the struct a method belongs to).
    pub parent: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_ir_basics() {
        let mut doc = DocumentIR::new(
            SourceId::new(),
            "file:///test.md".to_string(),
            SourceType::File,
        );

        doc.add_chunk(Chunk::new("# Hello World", ChunkType::Heading, 1, 1));
        doc.add_chunk(Chunk::new(
            "This is a paragraph about Rust.",
            ChunkType::Prose,
            3,
            3,
        ));

        assert_eq!(doc.chunk_count(), 2);
        assert!(doc.total_chars() > 0);
    }
}
