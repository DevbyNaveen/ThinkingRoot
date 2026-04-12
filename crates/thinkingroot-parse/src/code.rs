use std::path::Path;

use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId, SourceMetadata, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse a code file using tree-sitter into a DocumentIR.
pub fn parse(path: &Path, language: &str) -> Result<DocumentIR> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::io_path(path, e))?;
    let hash = ContentHash::from_bytes(content.as_bytes());

    let ts_language = get_language(language)?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&ts_language)
        .map_err(|e| Error::Parse {
            source_path: path.to_path_buf(),
            message: format!("failed to set language: {e}"),
        })?;

    let tree = parser.parse(&content, None).ok_or_else(|| Error::Parse {
        source_path: path.to_path_buf(),
        message: "tree-sitter parse returned None".to_string(),
    })?;

    let mut doc = DocumentIR::new(
        SourceId::new(),
        path.to_string_lossy().to_string(),
        SourceType::File,
    );
    doc.content_hash = hash;
    doc.metadata = SourceMetadata {
        file_extension: path.extension().and_then(|e| e.to_str()).map(String::from),
        language: Some(language.to_string()),
        relative_path: Some(path.to_string_lossy().to_string()),
        ..Default::default()
    };

    extract_chunks(&content, tree.root_node(), language, &mut doc);

    Ok(doc)
}

fn get_language(name: &str) -> Result<tree_sitter::Language> {
    match name {
        "rust" => Ok(tree_sitter_rust::LANGUAGE.into()),
        "python" => Ok(tree_sitter_python::LANGUAGE.into()),
        "javascript" => Ok(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" => Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Ok(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "go" => Ok(tree_sitter_go::LANGUAGE.into()),
        other => Err(Error::UnsupportedFileType {
            extension: other.to_string(),
        }),
    }
}

fn extract_chunks(source: &str, node: tree_sitter::Node, language: &str, doc: &mut DocumentIR) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        let start_line = child.start_position().row as u32 + 1;
        let end_line = child.end_position().row as u32 + 1;
        let text = &source[child.byte_range()];

        match child.kind() {
            // Rust
            "function_item"
            | "function_definition"
            | "method_definition"
            | "function_declaration"
            | "method_declaration" => {
                let name =
                    find_child_by_field(&child, "name").map(|n| source[n.byte_range()].to_string());
                let params = find_child_by_field(&child, "parameters")
                    .map(|n| source[n.byte_range()].to_string());
                let ret = find_child_by_field(&child, "return_type")
                    .map(|n| source[n.byte_range()].to_string());

                let mut chunk = Chunk::new(text, ChunkType::FunctionDef, start_line, end_line)
                    .with_language(language);
                chunk.metadata = ChunkMetadata {
                    function_name: name,
                    parameters: params.map(|p| vec![p]),
                    return_type: ret,
                    visibility: extract_visibility(source, &child),
                    ..Default::default()
                };
                doc.add_chunk(chunk);
            }

            // impl blocks: `impl Trait for Type` or `impl Type`
            "impl_item" => {
                // For `impl Trait for Type`, tree-sitter Rust grammar uses:
                //   field "type"  → the implementing type (after `for`)
                //   field "trait" → the trait being implemented
                // For `impl Type` (no trait), field "trait" is absent and
                // field "type" holds the type name directly.
                let type_node = find_child_by_field(&child, "type");
                let trait_node = find_child_by_field(&child, "trait");

                let (type_name, trait_name) = match (type_node, trait_node) {
                    (Some(ty), Some(tr)) => {
                        // `impl Trait for Type`
                        (
                            Some(source[ty.byte_range()].to_string()),
                            Some(source[tr.byte_range()].to_string()),
                        )
                    }
                    (Some(ty), None) => {
                        // `impl Type { ... }`
                        (Some(source[ty.byte_range()].to_string()), None)
                    }
                    _ => (None, None),
                };

                let mut chunk = Chunk::new(text, ChunkType::TypeDef, start_line, end_line)
                    .with_language(language);
                chunk.metadata = ChunkMetadata {
                    type_name,
                    trait_name,
                    visibility: extract_visibility(source, &child),
                    ..Default::default()
                };
                doc.add_chunk(chunk);
            }

            // Struct / class / interface / type definitions (non-impl)
            "struct_item"
            | "enum_item"
            | "type_item"
            | "trait_item"
            | "class_definition"
            | "class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "type_spec" => {
                let name =
                    find_child_by_field(&child, "name").map(|n| source[n.byte_range()].to_string());
                let field_types = extract_field_types(source, &child);

                let mut chunk = Chunk::new(text, ChunkType::TypeDef, start_line, end_line)
                    .with_language(language);
                chunk.metadata = ChunkMetadata {
                    type_name: name,
                    field_types,
                    visibility: extract_visibility(source, &child),
                    ..Default::default()
                };
                doc.add_chunk(chunk);
            }

            // Use / import statements
            "use_declaration" | "import_statement" | "import_declaration" | "import_spec" => {
                let chunk = Chunk::new(text, ChunkType::Import, start_line, end_line)
                    .with_language(language);
                doc.add_chunk(chunk);
            }

            // Comments (doc comments, block comments)
            "line_comment" | "block_comment" | "comment" => {
                if text.len() > 20 {
                    // Only include substantial comments.
                    let chunk = Chunk::new(text, ChunkType::Comment, start_line, end_line)
                        .with_language(language);
                    doc.add_chunk(chunk);
                }
            }

            // Module-level doc attributes in Rust
            "inner_attribute_item" if text.starts_with("#![doc") || text.starts_with("//!") => {
                let chunk = Chunk::new(text, ChunkType::ModuleDoc, start_line, end_line)
                    .with_language(language);
                doc.add_chunk(chunk);
            }

            _ => {
                // Recurse into children for nested definitions.
                if child.child_count() > 0 {
                    extract_chunks(source, child, language, doc);
                }
            }
        }
    }
}

fn find_child_by_field<'a>(
    node: &'a tree_sitter::Node<'a>,
    field: &str,
) -> Option<tree_sitter::Node<'a>> {
    node.child_by_field_name(field)
}

fn extract_visibility(source: &str, node: &tree_sitter::Node) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return Some(source[child.byte_range()].to_string());
        }
    }
    None
}

/// Walk struct/class body and collect non-primitive field type names.
/// Returns base type names with generics stripped (e.g., `Vec<String>` → `Vec`).
fn extract_field_types(source: &str, node: &tree_sitter::Node) -> Vec<String> {
    let mut types = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "field_declaration_list" | "declaration_list" | "class_body"
        ) {
            let mut inner = child.walk();
            for field in child.children(&mut inner) {
                if matches!(
                    field.kind(),
                    "field_declaration" | "typed_parameter" | "public_field_definition"
                ) {
                    if let Some(type_node) = field.child_by_field_name("type") {
                        let raw = source[type_node.byte_range()].trim().to_string();
                        let base = raw
                            .trim_start_matches('&')
                            .trim_start_matches("mut ")
                            .trim_start_matches("Option<")
                            .trim_start_matches("Vec<")
                            .trim_start_matches("Arc<")
                            .trim_start_matches("Box<")
                            .trim_start_matches("dyn ")
                            .split('<')
                            .next()
                            .unwrap_or(&raw)
                            .trim_end_matches('>')
                            .trim()
                            .to_string();
                        if !base.is_empty() && !is_primitive_type(&base) {
                            types.push(base);
                        }
                    }
                }
            }
        }
    }
    types.sort();
    types.dedup();
    types
}

fn is_primitive_type(s: &str) -> bool {
    matches!(
        s,
        "bool"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "char"
            | "str"
            | "String"
            | "()"
            | "Vec"
            | "Option"
            | "Arc"
            | "Box"
            | "HashMap"
            | "BTreeMap"
            | "HashSet"
            | "BTreeSet"
            | "Rc"
            | "Cell"
            | "RefCell"
            | "Result"
            | "Mutex"
            | "RwLock"
            | "Cow"
            | "PhantomData"
            | "Pin"
            | "Weak"
            | "OnceCell"
            | "LazyLock"
            | "MaybeUninit"
            | "UnsafeCell"
            | "ManuallyDrop"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rust_functions() {
        let source = r#"
pub fn hello(name: &str) -> String {
    format!("Hello, {name}!")
}

struct Config {
    name: String,
    value: i32,
}
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "test.rs".to_string(), SourceType::File);

        let ts_lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();

        extract_chunks(source, tree.root_node(), "rust", &mut doc);

        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::FunctionDef)
        );
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::TypeDef)
        );
    }
}
