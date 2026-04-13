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
        "java" => Ok(tree_sitter_java::LANGUAGE.into()),
        "c" => Ok(tree_sitter_c::LANGUAGE.into()),
        "cpp" => Ok(tree_sitter_cpp::LANGUAGE.into()),
        "csharp" => Ok(tree_sitter_c_sharp::LANGUAGE.into()),
        "ruby" => Ok(tree_sitter_ruby::LANGUAGE.into()),
        "kotlin" => Ok(tree_sitter_kotlin_ng::LANGUAGE.into()),
        "swift" => Ok(tree_sitter_swift::LANGUAGE.into()),
        "php" => Ok(tree_sitter_php::LANGUAGE_PHP.into()),
        "bash" => Ok(tree_sitter_bash::LANGUAGE.into()),
        "lua" => Ok(tree_sitter_lua::LANGUAGE.into()),
        "scala" => Ok(tree_sitter_scala::LANGUAGE.into()),
        "elixir" => Ok(tree_sitter_elixir::LANGUAGE.into()),
        "haskell" => Ok(tree_sitter_haskell::LANGUAGE.into()),
        "r" => Ok(tree_sitter_r::LANGUAGE.into()),
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
            // Rust / Python / JS / TS / Go / Java / C / C++ / C# / Ruby / Kotlin /
            // Swift / PHP / Bash / Lua / Scala / Elixir / Haskell / R
            "function_item"
            | "function_definition"
            | "method_definition"
            | "function_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "local_function_statement"
            | "local_function"
            | "singleton_method"
            // Ruby: `def method_name` → node kind is literally "method"
            | "method"
            // Haskell function definition
            | "function" => {
                let name =
                    find_child_by_field(&child, "name").map(|n| source[n.byte_range()].to_string());
                let params = find_child_by_field(&child, "parameters")
                    .map(|n| source[n.byte_range()].to_string());
                let ret = find_child_by_field(&child, "return_type")
                    .map(|n| source[n.byte_range()].to_string());

                // Walk the function body for call expressions (depth=6 captures up to 5
                // levels of nesting below the body node).
                let body = find_child_by_field(&child, "body")
                    .or_else(|| find_child_by_field(&child, "block"))
                    .or_else(|| find_child_by_field(&child, "statement_block"));
                let mut calls = body
                    .map(|b| collect_calls(source, b, 6))
                    .unwrap_or_default();
                calls.sort();
                calls.dedup();
                let func_name_str = name.as_deref().unwrap_or("").to_string();
                calls.retain(|c| !c.is_empty() && *c != func_name_str);

                let mut chunk = Chunk::new(text, ChunkType::FunctionDef, start_line, end_line)
                    .with_language(language);
                chunk.metadata = ChunkMetadata {
                    function_name: name,
                    parameters: params.map(|p| vec![p]),
                    return_type: ret,
                    visibility: extract_visibility(source, &child),
                    calls_functions: calls,
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
            | "type_spec"
            // Ruby: `class Foo` → node kind is literally "class"
            | "class"
            // C / C++
            | "struct_specifier"
            | "class_specifier"
            | "enum_specifier"
            | "type_definition"
            // Java / C#
            | "enum_declaration"
            | "record_declaration"
            // C# / Swift
            | "struct_declaration"
            // Kotlin
            | "object_declaration"
            // Swift
            | "protocol_declaration"
            // PHP
            | "trait_declaration"
            // Scala
            | "trait_definition"
            | "object_definition"
            // Haskell
            | "data_declaration"
            | "newtype_declaration" => {
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

                // Recurse into class/module bodies so nested method declarations
                // are also extracted as FunctionDef chunks (Java, C#, Ruby, etc.)
                extract_chunks(source, child, language, doc);
            }

            // Use / import statements
            "use_declaration"
            | "import_statement"
            | "import_declaration"
            | "import_spec"
            // C / C++ (#include)
            | "preproc_include"
            // C# (using System;)
            | "using_directive"
            // Kotlin (kotlin-ng uses "import", not "import_header")
            | "import"
            // PHP
            | "namespace_use_declaration" => {
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

            // Elixir: def/defp/defmacro are represented as `call` nodes in tree-sitter-elixir.
            // Detect them by checking if the first child's text is "def", "defp", or "defmacro".
            "call" if language == "elixir" => {
                // The call node's first named child is the function identifier ("def", "defp", etc.)
                // The second child (arguments) contains the function name and body.
                let mut call_cursor = child.walk();
                let call_children: Vec<_> = child.named_children(&mut call_cursor).collect();
                if let Some(head) = call_children.first() {
                    let head_text = &source[head.byte_range()];
                    if matches!(head_text, "def" | "defp" | "defmacro" | "defmacrop") {
                        // Function name is the first argument to the def call
                        if let Some(args) = call_children.get(1) {
                            // args is typically an "arguments" node; first child is the function name or call
                            let mut args_cursor = args.walk();
                            let arg_children: Vec<_> = args.named_children(&mut args_cursor).collect();
                            let name = arg_children.first().map(|n| {
                                // Could be an identifier (simple fn) or a call (fn with params)
                                let text = &source[n.byte_range()];
                                // Take just the identifier part before any '('
                                last_identifier(text).unwrap_or_else(|| text.to_string())
                            });
                            let start_line = child.start_position().row as u32 + 1;
                            let end_line = child.end_position().row as u32 + 1;
                            let text = &source[child.byte_range()];
                            let mut chunk = Chunk::new(text, ChunkType::FunctionDef, start_line, end_line)
                                .with_language(language);
                            chunk.metadata = ChunkMetadata {
                                function_name: name,
                                visibility: Some(if head_text.ends_with('p') { "private".to_string() } else { "public".to_string() }),
                                ..Default::default()
                            };
                            doc.add_chunk(chunk);
                        }
                    }
                }
                // Always recurse into call children for nested definitions
                if child.child_count() > 0 {
                    extract_chunks(source, child, language, doc);
                }
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

/// Walk a function body subtree. The caller passes depth=6 to capture up to 5
/// levels of nesting below the body node; each recursion step decrements depth.
fn collect_calls(source: &str, node: tree_sitter::Node, depth: u8) -> Vec<String> {
    if depth == 0 {
        return Vec::new();
    }
    let mut calls = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "call_expression" => {
                if let Some(func) = child.child_by_field_name("function") {
                    let raw = &source[func.byte_range()];
                    if let Some(name) = last_identifier(raw) {
                        calls.push(name);
                    }
                }
            }
            "method_call_expression" => {
                if let Some(method) = child.child_by_field_name("method") {
                    let raw = source[method.byte_range()].to_string();
                    if !raw.is_empty() {
                        calls.push(raw);
                    }
                }
            }
            "call" => {
                // Python: both function calls and method calls
                if let Some(func) = child.child_by_field_name("function") {
                    let raw = &source[func.byte_range()];
                    if let Some(name) = last_identifier(raw) {
                        calls.push(name);
                    }
                }
            }
            // Java: obj.method(args)
            "method_invocation" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let raw = source[name_node.byte_range()].to_string();
                    if !raw.is_empty() {
                        calls.push(raw);
                    }
                }
            }
            // C#: func(args) or obj.Method(args)
            "invocation_expression" => {
                if let Some(func) = child.child_by_field_name("function") {
                    let raw = &source[func.byte_range()];
                    if let Some(name) = last_identifier(raw) {
                        calls.push(name);
                    }
                }
            }
            // PHP: func(args)
            "function_call_expression" => {
                if let Some(func) = child.child_by_field_name("function") {
                    let raw = &source[func.byte_range()];
                    if let Some(name) = last_identifier(raw) {
                        calls.push(name);
                    }
                }
            }
            // PHP: $obj->method(args)
            "member_call_expression" => {
                if let Some(method) = child.child_by_field_name("name") {
                    let raw = source[method.byte_range()].to_string();
                    if !raw.is_empty() {
                        calls.push(raw);
                    }
                }
            }
            // Lua: func(args)
            "function_call" => {
                if let Some(func) = child.child_by_field_name("name") {
                    let raw = &source[func.byte_range()];
                    if let Some(name) = last_identifier(raw) {
                        calls.push(name);
                    }
                }
            }
            _ => {}
        }
        calls.extend(collect_calls(source, child, depth - 1));
    }
    calls
}

/// Extract the last identifier from a dotted or scoped name.
/// "user_service.find_by_email" → "find_by_email"
/// "AuthService::validate"      → "validate"
/// "foo"                        → "foo"
fn last_identifier(text: &str) -> Option<String> {
    let last = text.split(['.', ':']).rfind(|s| !s.is_empty())?;
    if !last.is_empty() && last.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(last.to_string())
    } else {
        None
    }
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
    fn function_body_calls_are_collected() {
        let source = r#"
fn outer(x: i32) -> i32 {
    let a = helper_one(x);
    let b = self.helper_two(a);
    a + b
}

fn helper_one(x: i32) -> i32 { x + 1 }
fn helper_two(&self, x: i32) -> i32 { x * 2 }
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "test.rs".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "rust", &mut doc);

        let outer = doc.chunks.iter().find(|c| {
            c.chunk_type == ChunkType::FunctionDef
                && c.metadata.function_name.as_deref() == Some("outer")
        });
        assert!(outer.is_some(), "outer function chunk must exist");
        let calls = &outer.unwrap().metadata.calls_functions;
        assert!(
            calls.contains(&"helper_one".to_string()),
            "must detect call to helper_one"
        );
        assert!(
            calls.contains(&"helper_two".to_string()),
            "must detect method call to helper_two"
        );
        assert!(
            !calls.contains(&"outer".to_string()),
            "must not list self-recursion"
        );
    }

    #[test]
    fn calls_functions_deduplicated() {
        let source = r#"
fn process(items: Vec<i32>) -> i32 {
    let a = transform(items[0]);
    let b = transform(items[1]);
    a + b
}
fn transform(x: i32) -> i32 { x }
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "test.rs".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "rust", &mut doc);

        let process = doc
            .chunks
            .iter()
            .find(|c| c.metadata.function_name.as_deref() == Some("process"))
            .unwrap();
        let transform_count = process
            .metadata
            .calls_functions
            .iter()
            .filter(|n| n.as_str() == "transform")
            .count();
        assert_eq!(transform_count, 1, "same callee must appear only once");
    }

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

    #[test]
    fn java_function_is_parsed() {
        let source = r#"
public class Main {
    public String greet(String name) {
        return "Hello " + name;
    }
}
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "Main.java".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "java", &mut doc);
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::FunctionDef),
            "java method_declaration must produce FunctionDef"
        );
        let greet = doc
            .chunks
            .iter()
            .find(|c| c.metadata.function_name.as_deref() == Some("greet"));
        assert!(greet.is_some(), "greet method must be named");
    }

    #[test]
    fn c_function_is_parsed() {
        let source = r#"
#include <stdio.h>

int add(int a, int b) {
    return a + b;
}
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "math.c".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "c", &mut doc);
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::FunctionDef),
            "c function_definition must produce FunctionDef"
        );
        assert!(
            doc.chunks.iter().any(|c| c.chunk_type == ChunkType::Import),
            "preproc_include must produce Import"
        );
    }

    #[test]
    fn csharp_method_is_parsed() {
        let source = r#"
using System;

public class MyClass {
    public string Hello(string name) {
        return $"Hello {name}";
    }
}
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "MyClass.cs".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_c_sharp::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "csharp", &mut doc);
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::FunctionDef),
            "csharp method_declaration must produce FunctionDef"
        );
        assert!(
            doc.chunks.iter().any(|c| c.chunk_type == ChunkType::Import),
            "using_directive must produce Import"
        );
    }

    #[test]
    fn ruby_method_is_parsed() {
        let source = r#"
class Greeter
  def greet(name)
    "Hello #{name}"
  end
end
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "greeter.rb".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_ruby::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "ruby", &mut doc);
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::FunctionDef),
            "ruby method must produce FunctionDef"
        );
        let greet = doc
            .chunks
            .iter()
            .find(|c| c.metadata.function_name.as_deref() == Some("greet"));
        assert!(greet.is_some(), "greet method must be named");
    }

    #[test]
    fn python_function_calls_are_collected() {
        let source = r#"
def process(data):
    result = transform(data)
    obj.method(result)
    return result
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "test.py".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "python", &mut doc);

        let process_chunk = doc.chunks.iter().find(|c| {
            c.chunk_type == ChunkType::FunctionDef
                && c.metadata.function_name.as_deref() == Some("process")
        });
        assert!(process_chunk.is_some(), "process function chunk must exist");
        let calls = &process_chunk.unwrap().metadata.calls_functions;
        assert!(
            calls.contains(&"transform".to_string()),
            "must detect transform call"
        );
        assert!(
            calls.contains(&"method".to_string()),
            "must detect obj.method call"
        );
    }

    #[test]
    fn kotlin_import_is_parsed() {
        let source = r#"
import com.example.Foo
import kotlin.collections.List

fun hello(): String = "hi"
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "Main.kt".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_kotlin_ng::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "kotlin", &mut doc);
        assert!(
            doc.chunks.iter().any(|c| c.chunk_type == ChunkType::Import),
            "kotlin import must produce Import chunk"
        );
    }

    #[test]
    fn haskell_function_is_parsed() {
        let source = r#"
module Main where

greet :: String -> String
greet name = "Hello " ++ name

main :: IO ()
main = putStrLn (greet "world")
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "Main.hs".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_haskell::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "haskell", &mut doc);
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::FunctionDef),
            "haskell function node must produce FunctionDef"
        );
    }

    #[test]
    fn elixir_def_is_parsed() {
        let source = r#"
defmodule Greeter do
  def greet(name) do
    "Hello #{name}"
  end

  defp helper(x), do: x
end
"#;
        let mut doc = DocumentIR::new(SourceId::new(), "greeter.ex".to_string(), SourceType::File);
        let ts_lang: tree_sitter::Language = tree_sitter_elixir::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        extract_chunks(source, tree.root_node(), "elixir", &mut doc);
        assert!(
            doc.chunks
                .iter()
                .any(|c| c.chunk_type == ChunkType::FunctionDef),
            "elixir def must produce FunctionDef"
        );
    }
}
