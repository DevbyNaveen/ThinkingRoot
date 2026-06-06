# Language Expansion + AST→LLM Injection Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add 14 new languages via tree-sitter and wire AST metadata into the LLM prompt so structural facts anchor semantic extraction — collapsing two blind lanes into one coherent pipeline.

**Architecture:**
- Task 1: Add tree-sitter grammars for 14 languages. The existing `extract_chunks` match arms already cover most node types — we extend them for language-specific gaps and add new call node types to `collect_calls`.
- Task 2: Build an AST anchor section from `ChunkMetadata` and inject it into the LLM prompt. LLM is forced to describe the exact entities AST found (same names), guaranteeing Linker merge.

**Tech Stack:** Rust, tree-sitter, thinkingroot-parse, thinkingroot-extract

---

## File Map

| File | Change |
|---|---|
| `crates/thinkingroot-parse/Cargo.toml` | Add 14 tree-sitter language crates |
| `crates/thinkingroot-parse/src/code.rs` | `get_language()` + extend node type match arms + `collect_calls` |
| `crates/thinkingroot-parse/src/lib.rs` | File extension routing for 14 languages |
| `crates/thinkingroot-extract/src/prompts.rs` | Add `build_ast_anchor_section()` |
| `crates/thinkingroot-extract/src/extractor.rs` | Add `ast_anchor` to `ChunkWork`, thread into LLM prompt |

---

## Task 1: Language Expansion

**Files:**
- Modify: `crates/thinkingroot-parse/Cargo.toml`
- Modify: `crates/thinkingroot-parse/src/code.rs`
- Modify: `crates/thinkingroot-parse/src/lib.rs`

### Verified crate versions (confirmed on crates.io)

```toml
tree-sitter-java    = "0.23.5"
tree-sitter-c       = "0.24.1"
tree-sitter-cpp     = "0.23.4"
tree-sitter-c-sharp = "0.23.1"
tree-sitter-ruby    = "0.23.1"
tree-sitter-kotlin  = "0.3.8"
tree-sitter-swift   = "0.7.1"
tree-sitter-php     = "0.24.2"
tree-sitter-bash    = "0.25.1"
tree-sitter-lua     = "0.5.0"
tree-sitter-scala   = "0.25.0"
tree-sitter-elixir  = "0.3.5"
tree-sitter-haskell = "0.23.1"
tree-sitter-r       = "1.2.0"
```

- [ ] **Step 1: Write failing test**

Add to `crates/thinkingroot-parse/src/code.rs` test section:

```rust
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
    assert!(doc.chunks.iter().any(|c| c.chunk_type == ChunkType::FunctionDef),
        "java method_declaration must produce FunctionDef");
    let greet = doc.chunks.iter().find(|c| c.metadata.function_name.as_deref() == Some("greet"));
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
    assert!(doc.chunks.iter().any(|c| c.chunk_type == ChunkType::FunctionDef),
        "c function_definition must produce FunctionDef");
    assert!(doc.chunks.iter().any(|c| c.chunk_type == ChunkType::Import),
        "preproc_include must produce Import");
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
    assert!(doc.chunks.iter().any(|c| c.chunk_type == ChunkType::FunctionDef),
        "csharp method_declaration must produce FunctionDef");
    assert!(doc.chunks.iter().any(|c| c.chunk_type == ChunkType::Import),
        "using_directive must produce Import");
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
    assert!(doc.chunks.iter().any(|c| c.chunk_type == ChunkType::FunctionDef),
        "ruby method must produce FunctionDef");
    let greet = doc.chunks.iter().find(|c| c.metadata.function_name.as_deref() == Some("greet"));
    assert!(greet.is_some(), "greet method must be named");
}
```

- [ ] **Step 2: Run tests — verify they fail**

```bash
cargo test -p thinkingroot-parse --no-default-features 2>&1 | grep -E "FAILED|error"
```

Expected: compile errors (crates not yet added).

- [ ] **Step 3: Add crates to Cargo.toml**

Add to `[dependencies]` section in `crates/thinkingroot-parse/Cargo.toml` after existing tree-sitter grammars:

```toml
tree-sitter-java    = "0.23.5"
tree-sitter-c       = "0.24.1"
tree-sitter-cpp     = "0.23.4"
tree-sitter-c-sharp = "0.23.1"
tree-sitter-ruby    = "0.23.1"
tree-sitter-kotlin  = "0.3.8"
tree-sitter-swift   = "0.7.1"
tree-sitter-php     = "0.24.2"
tree-sitter-bash    = "0.25.1"
tree-sitter-lua     = "0.5.0"
tree-sitter-scala   = "0.25.0"
tree-sitter-elixir  = "0.3.5"
tree-sitter-haskell = "0.23.1"
tree-sitter-r       = "1.2.0"
```

- [ ] **Step 4: Extend `get_language()` in `code.rs`**

Add after the existing `"go"` arm. Check each crate's public API for the correct export — most use `LANGUAGE`, PHP uses `LANGUAGE_PHP`, Swift may use `language()`. Use `cargo doc -p tree-sitter-LANG --open` or check docs.rs to confirm:

```rust
"java"    => Ok(tree_sitter_java::LANGUAGE.into()),
"c"       => Ok(tree_sitter_c::LANGUAGE.into()),
"cpp"     => Ok(tree_sitter_cpp::LANGUAGE.into()),
"csharp"  => Ok(tree_sitter_c_sharp::LANGUAGE.into()),
"ruby"    => Ok(tree_sitter_ruby::LANGUAGE.into()),
"kotlin"  => Ok(tree_sitter_kotlin::LANGUAGE.into()),
"swift"   => Ok(tree_sitter_swift::LANGUAGE.into()),
"php"     => Ok(tree_sitter_php::LANGUAGE_PHP.into()),
"bash"    => Ok(tree_sitter_bash::LANGUAGE.into()),
"lua"     => Ok(tree_sitter_lua::LANGUAGE.into()),
"scala"   => Ok(tree_sitter_scala::LANGUAGE.into()),
"elixir"  => Ok(tree_sitter_elixir::LANGUAGE.into()),
"haskell" => Ok(tree_sitter_haskell::LANGUAGE.into()),
"r"       => Ok(tree_sitter_r::LANGUAGE.into()),
```

**IMPORTANT:** Run `cargo check -p thinkingroot-parse` after adding each language to catch wrong export names immediately. Fix the export name if compilation fails (e.g., `language()` vs `LANGUAGE`).

- [ ] **Step 5: Extend node type match arms in `extract_chunks`**

**Functions** — add to the existing function match arm:

```rust
"function_item"
| "function_definition"
| "method_definition"
| "function_declaration"
| "method_declaration"
| "constructor_declaration"    // Java, C#
| "local_function_statement"   // C#
| "local_function"             // Lua
| "singleton_method"           // Ruby
| "def"                        // Elixir (module-level)
| "defp"                       // Elixir (private)
| "signature"                  // Haskell top-level binding signature
=> { /* existing body unchanged */ }
```

**Types** — add to the existing type match arm:

```rust
"struct_item"
| "enum_item"
| "type_item"
| "trait_item"
| "class_definition"
| "class_declaration"
| "interface_declaration"
| "type_alias_declaration"
| "type_spec"
| "struct_specifier"           // C, C++
| "class_specifier"            // C++
| "enum_specifier"             // C, C++
| "type_definition"            // C typedef
| "enum_declaration"           // Java, C#
| "record_declaration"         // Java, C#
| "struct_declaration"         // C#, Swift
| "object_declaration"         // Kotlin
| "protocol_declaration"       // Swift
| "trait_declaration"          // PHP
| "trait_definition"           // Scala
| "object_definition"          // Scala
| "data_declaration"           // Haskell
| "newtype_declaration"        // Haskell
=> { /* existing body unchanged */ }
```

**Imports** — add to the existing import match arm:

```rust
"use_declaration"
| "import_statement"
| "import_declaration"
| "import_spec"
| "preproc_include"            // C, C++ (#include)
| "using_directive"            // C# (using System;)
| "import_header"              // Kotlin
| "namespace_use_declaration"  // PHP
=> { /* existing body unchanged */ }
```

- [ ] **Step 6: Extend `collect_calls()` for new call node types**

Add to the `match child.kind()` inside `collect_calls`:

```rust
// Java: method_invocation has field "name" for the method name
"method_invocation" => {
    if let Some(name_node) = child.child_by_field_name("name") {
        let raw = source[name_node.byte_range()].to_string();
        if !raw.is_empty() {
            calls.push(raw);
        }
    }
}
// C#: invocation_expression has field "function"
"invocation_expression" => {
    if let Some(func) = child.child_by_field_name("function") {
        let raw = &source[func.byte_range()];
        if let Some(name) = last_identifier(raw) {
            calls.push(name);
        }
    }
}
// PHP function call
"function_call_expression" => {
    if let Some(func) = child.child_by_field_name("function") {
        let raw = &source[func.byte_range()];
        if let Some(name) = last_identifier(raw) {
            calls.push(name);
        }
    }
}
// PHP method call
"member_call_expression" => {
    if let Some(method) = child.child_by_field_name("name") {
        let raw = source[method.byte_range()].to_string();
        if !raw.is_empty() {
            calls.push(raw);
        }
    }
}
// Lua function call
"function_call" => {
    if let Some(func) = child.child_by_field_name("name") {
        let raw = &source[func.byte_range()];
        if let Some(name) = last_identifier(raw) {
            calls.push(name);
        }
    }
}
```

- [ ] **Step 7: Add file extension routing in `lib.rs`**

Add after the existing `"go"` arm and before the manifest section:

```rust
"java"                        => code::parse(path, "java"),
"c" | "h"                     => code::parse(path, "c"),
"cpp" | "cc" | "cxx" | "hpp" | "hxx" => code::parse(path, "cpp"),
"cs"                          => code::parse(path, "csharp"),
"rb"                          => code::parse(path, "ruby"),
"kt" | "kts"                  => code::parse(path, "kotlin"),
"swift"                       => code::parse(path, "swift"),
"php"                         => code::parse(path, "php"),
"sh" | "bash"                 => code::parse(path, "bash"),
"lua"                         => code::parse(path, "lua"),
"scala"                       => code::parse(path, "scala"),
"ex" | "exs"                  => code::parse(path, "elixir"),
"hs"                          => code::parse(path, "haskell"),
"r"                           => code::parse(path, "r"),
```

- [ ] **Step 8: Run tests and verify all pass**

```bash
cargo test -p thinkingroot-parse --no-default-features 2>&1 | tail -20
```

Expected: all tests pass including the 4 new ones.

- [ ] **Step 9: Type-check entire workspace**

```bash
cargo check --workspace --no-default-features 2>&1 | tail -10
```

Expected: `Finished` with no errors.

- [ ] **Step 10: Commit**

```bash
git add crates/thinkingroot-parse/Cargo.toml \
        crates/thinkingroot-parse/src/code.rs \
        crates/thinkingroot-parse/src/lib.rs
git commit -m "feat(parse): add 14 languages via tree-sitter (Java, C, C++, C#, Ruby, Kotlin, Swift, PHP, Bash, Lua, Scala, Elixir, Haskell, R)"
```

---

## Task 2: AST → LLM Context Injection

**Files:**
- Modify: `crates/thinkingroot-extract/src/prompts.rs`
- Modify: `crates/thinkingroot-extract/src/extractor.rs`

**What this does:** When LLM processes a code chunk, inject the AST-extracted metadata (function name, call list, return type, visibility) as an anchor section in the prompt. LLM must use the exact entity names AST found, guaranteeing the Linker merges structural topology with LLM semantics into one node.

### Current state

`prompts.rs` has `build_extraction_prompt_with_context(content, context, known_entities_section)`.

`extractor.rs` `ChunkWork` struct:
```rust
struct ChunkWork {
    source_id: SourceId,
    source_uri: String,
    original_content: String,
    sub_chunks: Vec<String>,
    context: String,
}
```

The `known_entities_section` is built once globally and cloned into each task.

### Target state

Add `ast_anchor: String` to `ChunkWork`. In the spawn task, prepend `ast_anchor` to `known_entities_section` before passing to `extract_with_split`. Add `build_ast_anchor_section` to `prompts.rs`.

- [ ] **Step 1: Write failing test in `prompts.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::ir::ChunkMetadata;

    #[test]
    fn ast_anchor_empty_for_empty_metadata() {
        let meta = ChunkMetadata::default();
        let section = build_ast_anchor_section(&meta);
        assert!(section.is_empty(), "empty metadata must produce empty anchor");
    }

    #[test]
    fn ast_anchor_includes_function_name_and_calls() {
        let meta = ChunkMetadata {
            function_name: Some("validate_token".to_string()),
            calls_functions: vec!["decode".to_string(), "verify_sig".to_string()],
            return_type: Some("Result<Claims>".to_string()),
            visibility: Some("pub".to_string()),
            ..Default::default()
        };
        let section = build_ast_anchor_section(&meta);
        assert!(section.contains("validate_token"), "must contain function name");
        assert!(section.contains("decode"), "must contain callee");
        assert!(section.contains("verify_sig"), "must contain callee");
        assert!(section.contains("Result<Claims>"), "must contain return type");
        assert!(section.contains("pub"), "must contain visibility");
    }

    #[test]
    fn ast_anchor_includes_type_name_and_trait() {
        let meta = ChunkMetadata {
            type_name: Some("AuthService".to_string()),
            trait_name: Some("Service".to_string()),
            ..Default::default()
        };
        let section = build_ast_anchor_section(&meta);
        assert!(section.contains("AuthService"), "must contain type name");
        assert!(section.contains("Service"), "must contain trait name");
    }

    #[test]
    fn ast_anchor_entity_names_line_is_present() {
        let meta = ChunkMetadata {
            function_name: Some("do_thing".to_string()),
            calls_functions: vec!["helper".to_string()],
            ..Default::default()
        };
        let section = build_ast_anchor_section(&meta);
        // Must instruct LLM to use exact entity names
        assert!(section.contains("do_thing"), "anchor must name the entity");
        assert!(section.contains("helper"), "anchor must name callees");
    }
}
```

- [ ] **Step 2: Run tests — verify they fail**

```bash
cargo test -p thinkingroot-extract --no-default-features 2>&1 | grep "FAILED\|error\[E"
```

Expected: compile error (`build_ast_anchor_section` not found).

- [ ] **Step 3: Implement `build_ast_anchor_section` in `prompts.rs`**

Add after `build_context`:

```rust
/// Build an AST anchor section from chunk metadata.
///
/// When non-empty, this is prepended to the LLM prompt so the LLM describes
/// the exact entities that AST already found — guaranteeing entity name
/// alignment between structural (topology) and LLM (semantic) extraction.
/// The Linker merges by canonical name, so matching names = one graph node.
pub fn build_ast_anchor_section(metadata: &thinkingroot_core::ir::ChunkMetadata) -> String {
    let mut lines: Vec<String> = Vec::new();

    if let Some(ref name) = metadata.function_name {
        lines.push(format!("Function: {name}"));
        if let Some(ref vis) = metadata.visibility {
            lines.push(format!("Visibility: {vis}"));
        }
        if let Some(ref ret) = metadata.return_type {
            lines.push(format!("Returns: {ret}"));
        }
        if !metadata.calls_functions.is_empty() {
            lines.push(format!("Calls: [{}]", metadata.calls_functions.join(", ")));
        }
    } else if let Some(ref name) = metadata.type_name {
        lines.push(format!("Type: {name}"));
        if let Some(ref vis) = metadata.visibility {
            lines.push(format!("Visibility: {vis}"));
        }
        if let Some(ref trait_name) = metadata.trait_name {
            lines.push(format!("Implements: {trait_name}"));
        }
        if !metadata.field_types.is_empty() {
            lines.push(format!("Field types: [{}]", metadata.field_types.join(", ")));
        }
    } else if let Some(ref path) = metadata.import_path {
        lines.push(format!("Import: {path}"));
    }

    if lines.is_empty() {
        return String::new();
    }

    // Collect all entity names AST found so LLM is instructed to use them exactly.
    let mut entity_names: Vec<String> = Vec::new();
    if let Some(ref n) = metadata.function_name {
        entity_names.push(format!("\"{n}\""));
    }
    if let Some(ref n) = metadata.type_name {
        entity_names.push(format!("\"{n}\""));
    }
    for callee in &metadata.calls_functions {
        entity_names.push(format!("\"{callee}\""));
    }

    let names_instruction = if !entity_names.is_empty() {
        format!(
            "IMPORTANT: Use these EXACT entity names in your output: {}\n",
            entity_names.join(", ")
        )
    } else {
        String::new()
    };

    format!(
        "## AST Analysis (deterministic, from tree-sitter)\n\
         {}\n\
         {names_instruction}",
        lines.join("\n")
    )
}
```

- [ ] **Step 4: Run tests — verify they pass**

```bash
cargo test -p thinkingroot-extract --no-default-features 2>&1 | grep -E "test.*ok|FAILED"
```

Expected: all pass including the 4 new prompts tests.

- [ ] **Step 5: Add `ast_anchor` field to `ChunkWork` in `extractor.rs`**

Change the `ChunkWork` struct (inside `extract_all`):

```rust
struct ChunkWork {
    source_id: SourceId,
    source_uri: String,
    original_content: String,
    sub_chunks: Vec<String>,
    context: String,
    ast_anchor: String,   // NEW: AST metadata section for this chunk
}
```

- [ ] **Step 6: Populate `ast_anchor` when pushing to `llm_work`**

Change the `llm_work.push(ChunkWork { ... })` call:

```rust
llm_work.push(ChunkWork {
    source_id: doc.source_id,
    source_uri: doc.uri.clone(),
    original_content: chunk.content.clone(),
    sub_chunks,
    context: prompts::build_context(
        &doc.uri,
        chunk.language.as_deref(),
        chunk.heading.as_deref(),
    ),
    ast_anchor: prompts::build_ast_anchor_section(&chunk.metadata),  // NEW
});
```

- [ ] **Step 7: Thread `ast_anchor` into the LLM prompt in the spawn task**

In the `join_set.spawn(async move { ... })` block, find where `graph_ctx` is cloned and change how it's passed to `extract_with_split`:

```rust
// Before: extract_with_split uses graph_ctx directly
// After: prepend ast_anchor to graph_ctx so LLM sees AST facts first

let combined_ctx = if work.ast_anchor.is_empty() {
    graph_ctx
} else {
    format!("{}\n\n{}", work.ast_anchor, graph_ctx)
};

// Then use combined_ctx everywhere graph_ctx was used in the spawn task:
match extract_with_split(
    Arc::clone(&llm),
    sub_content.clone(),
    work.context.clone(),
    combined_ctx.clone(),  // was graph_ctx.clone()
    0,
)
```

- [ ] **Step 8: Run all extract tests**

```bash
cargo test -p thinkingroot-extract --no-default-features 2>&1 | tail -20
```

Expected: all tests pass (no regressions).

- [ ] **Step 9: Type-check entire workspace**

```bash
cargo check --workspace --no-default-features 2>&1 | tail -10
```

Expected: `Finished` with no errors.

- [ ] **Step 10: Commit**

```bash
git add crates/thinkingroot-extract/src/prompts.rs \
        crates/thinkingroot-extract/src/extractor.rs
git commit -m "feat(extract): inject AST anchor into LLM prompt — one coherent pipeline

AST output (function name, call graph, types) is now injected into the
LLM extraction prompt as an anchor section. LLM is instructed to use the
exact entity names AST found, guaranteeing that structural topology (0.99)
and LLM semantics (0.7-0.9) land on the same graph node after Linker merge.

Previously two blind parallel lanes. Now one coherent pipeline:
  AST finds validate_token → LLM describes what validate_token DOES
  Linker merges by name → single node with both topology and meaning."
```
