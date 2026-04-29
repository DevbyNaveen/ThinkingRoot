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
    /// Byte offset (inclusive) of this chunk within its source file.
    /// Populated by parsers that have access to byte-level positioning
    /// (tree-sitter `node.byte_range()`, markdown chunker offset tracker).
    /// Defaults to 0 for parsers that have not been upgraded yet — the
    /// structural and LLM extractors copy whatever value is here onto the
    /// emitted [`ExtractedClaim::byte_start`].
    #[serde(default)]
    pub byte_start: u64,
    /// Byte offset (exclusive) of this chunk within its source file.
    /// Defaults to 0 alongside [`Chunk::byte_start`]; the pair `(0, 0)` is
    /// the sentinel for "parser has not yet been upgraded to track byte
    /// offsets". Use [`Chunk::with_byte_range`] to set authoritative values.
    #[serde(default)]
    pub byte_end: u64,
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
            byte_start: 0,
            byte_end: 0,
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

    /// Set the chunk's byte range within its source file. Parsers should
    /// call this whenever they have authoritative byte offsets (tree-sitter
    /// `node.byte_range()`, markdown chunker byte cursor) so downstream
    /// extractors can emit verifiable byte-range citations on every claim.
    pub fn with_byte_range(mut self, byte_start: u64, byte_end: u64) -> Self {
        self.byte_start = byte_start;
        self.byte_end = byte_end;
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
    /// A single dependency declaration from a project manifest file
    /// (Cargo.toml, package.json, go.mod, requirements.txt, pyproject.toml).
    ManifestDependency,
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
    /// For TypeDef (impl_item): the trait being implemented, if any.
    /// Set when the chunk is `impl Trait for Type`.
    pub trait_name: Option<String>,
    /// For TypeDef (struct_item): the non-primitive field types.
    /// Each entry is the base type name (generics stripped).
    pub field_types: Vec<String>,
    // Gap 1: Code structure (function/type/import metadata)
    // Gap 2: Code call graph
    /// Functions/methods called within this function body (simple names, deduplicated).
    pub calls_functions: Vec<String>,
    // Gap 3: Markdown structure
    /// Heading depth: H1=1 … H6=6. `None` for non-heading chunks.
    pub heading_level: Option<u8>,
    /// Hyperlink targets found in this chunk (non-empty, non-fragment URLs).
    pub links: Vec<String>,
    // Gap 4: Git history
    /// Commit author name (git commits only).
    pub commit_author: Option<String>,
    /// File paths changed in this commit (from diff --stat output).
    pub changed_files: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_metadata_new_fields_default() {
        let m = ChunkMetadata::default();
        assert!(m.calls_functions.is_empty());
        assert!(m.heading_level.is_none());
        assert!(m.links.is_empty());
        assert!(m.commit_author.is_none());
        assert!(m.changed_files.is_empty());
    }

    #[test]
    fn manifest_dependency_chunk_type_roundtrips() {
        let chunk = Chunk::new("serde = \"1\"", ChunkType::ManifestDependency, 1, 1);
        let json = serde_json::to_string(&chunk.chunk_type).unwrap();
        assert_eq!(json, "\"manifest_dependency\"");
    }

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
