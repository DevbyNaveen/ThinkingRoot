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
    /// Compile Completeness Contract §I-3 exception class — set by
    /// parsers that strip a trailing newline. Phase 6.7 emits a
    /// `source_annotations` row of `kind = "trailing_newline_norm"`
    /// covering the dropped byte so the byte-coverage audit passes.
    #[serde(default)]
    pub trailing_newline_normalised: bool,
    /// For formats whose on-disk bytes are NOT the text the chunks/witnesses
    /// anchor to (e.g. PDF: binary file → extracted text), the parser sets this
    /// to the exact text its byte ranges index into. The byte store persists
    /// THIS (not the raw file) so `materialize_statement` returns real text, not
    /// binary noise. `None` for text-native formats (md/code/txt) where the raw
    /// file bytes ARE the anchored content.
    #[serde(default)]
    pub anchored_text: Option<String>,
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
            trailing_newline_normalised: false,
            anchored_text: None,
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

    /// Backfill `byte_start`/`byte_end` on every chunk that still has the
    /// `(0, 0)` "unknown" sentinel by searching for the chunk's content in
    /// `source`. Walks the source linearly with a cursor so equal-content
    /// chunks (e.g., two paragraphs containing the same text) get distinct
    /// ranges.
    ///
    /// Parsers that already populate authoritative byte ranges from a
    /// tree-sitter `Node::byte_range()` or another byte-aware mechanism
    /// are unaffected — chunks with non-zero ranges are skipped. Markdown,
    /// manifest, PDF, and git parsers should call this at the end of
    /// `parse_*` so v3 pack writes never emit a claim citing `(0, 0)`.
    ///
    /// The match is substring-based and tolerant of trimmed content
    /// (markdown's `flush_prose` trims the chunk before storing). When the
    /// content cannot be found at-or-after the cursor (e.g., the chunk was
    /// transformed beyond a substring search), the chunk is left unchanged
    /// and the cursor stays put — downstream consumers treat `(0, 0)` as
    /// "unknown" and fall back to line-based positioning.
    pub fn fill_byte_ranges(&mut self, source: &str) {
        let mut cursor = 0usize;
        for chunk in &mut self.chunks {
            if chunk.byte_start != 0 || chunk.byte_end != 0 {
                // Authoritative range already present — preserve it.
                continue;
            }
            let needle = chunk.content.trim();
            if needle.is_empty() {
                continue;
            }
            if let Some(found) = source[cursor..].find(needle) {
                let abs_start = cursor + found;
                let abs_end = abs_start + needle.len();
                chunk.byte_start = abs_start as u64;
                chunk.byte_end = abs_end as u64;
                cursor = abs_end;
            }
        }
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
    /// A single key-value entry (or section header) in a generic config
    /// or data tree such as TOML, YAML, or JSON. Carries `config_key`,
    /// `config_value`, and `config_value_type` in [`ChunkMetadata`].
    ConfigEntry,
    /// A single tabular row from CSV, TSV, a markdown GFM table, or a
    /// top-level JSON array-of-objects element. Carries `row_index` and
    /// `row_columns` (header → cell pairs) in [`ChunkMetadata`].
    DataRow,
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

    // Wedge 4: TOML/YAML/JSON config-tree extraction.
    /// Dotted path to this config leaf, e.g. `"database.pool_size"` or
    /// `"servers[0].host"`. Set on `ChunkType::ConfigEntry` chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_key: Option<String>,
    /// Primitive scalar rendered as a string (numbers and bools are stringified).
    /// `None` for table / array / null section headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_value: Option<String>,
    /// Type of the config value: `"string" | "int" | "bool" | "float"
    /// | "array" | "table" | "null"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_value_type: Option<String>,

    // Wedge 4: CSV/TSV/markdown-table row extraction.
    /// Zero-based row index, counted *after* the header row. Set on
    /// `ChunkType::DataRow` chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_index: Option<u32>,
    /// Header → cell pairs in the original column order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_columns: Vec<(String, String)>,

    // Wedge 4: doc-comment tag extraction (Rustdoc / JSDoc / Python / JavaDoc).
    /// Parsed `@param` / `@returns` / `@throws` / `@deprecated` / `@see`
    /// tags found inside a `Comment` or `ModuleDoc` chunk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub doc_tags: Vec<DocTag>,

    // ─── Compile Completeness Contract §4.16 — code metrics ─────────────
    /// McCabe cyclomatic complexity for FunctionDef chunks. `None` for
    /// non-code chunks. Computed by per-language tree-sitter queries in
    /// `code_metrics.rs` for Rust + TypeScript + Python + Go + Java
    /// (the v1 supported set per contract §15 Q3); other languages
    /// leave this at `None` and `complexity_method` reports
    /// `"unsupported"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cyclomatic: Option<u32>,
    /// Algorithm used to compute `cyclomatic`. `"mccabe"` for the 5
    /// supported languages, `"unsupported"` for the rest. Stored so
    /// queries that filter on cyclomatic can distinguish "0 because
    /// trivial fn" from "0 because language not yet supported".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity_method: Option<String>,
}

/// A single parsed doc-comment annotation tag.
///
/// Produced by the doctags parser for the inside of `Comment` / `ModuleDoc`
/// chunks. `kind` is normalised to a small set of strings (`"param"`,
/// `"returns"`, `"throws"`, `"deprecated"`, `"see"`); other tags are kept
/// with their literal kind for forward compatibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocTag {
    /// Tag kind: `"param" | "returns" | "throws" | "deprecated" | "see" | …`.
    pub kind: String,
    /// Tag target name, where applicable (e.g. the parameter name in
    /// `@param <name>` or the exception type in `@throws <Type>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free-form description for the tag.
    pub description: String,
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
