# Maximum AST Utilization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate four structural extraction gaps (manifest deps, call graph, markdown structure, git authorship) to produce richer knowledge graphs with zero additional LLM cost.

**Architecture:** New metadata fields on `ChunkMetadata` carry parsed structural data through the pipeline; parsers populate those fields; the structural extractor converts them to relations at 0.99 confidence. The router and extractor fallthrough condition are updated to classify and preserve the new chunk types.

**Tech Stack:** Rust, tree-sitter, pulldown-cmark 0.13, serde_json (already in parse crate), toml 0.8 (workspace dep, needs adding to parse crate)

---

## File Map

| File | Change |
|---|---|
| `crates/thinkingroot-core/src/ir.rs` | +5 fields on `ChunkMetadata`, +`ManifestDependency` variant on `ChunkType` |
| `crates/thinkingroot-parse/Cargo.toml` | Add `toml = { workspace = true }` |
| `crates/thinkingroot-parse/src/manifest.rs` | **NEW** — parse Cargo.toml, package.json, go.mod, requirements.txt, pyproject.toml |
| `crates/thinkingroot-parse/src/lib.rs` | Export `manifest` module, add dispatcher routing |
| `crates/thinkingroot-parse/src/code.rs` | Add `collect_calls` + `last_identifier`; populate `calls_functions` in FunctionDef arm |
| `crates/thinkingroot-parse/src/markdown.rs` | Capture heading level + parent stack; collect link URLs |
| `crates/thinkingroot-parse/src/git.rs` | Populate `metadata.author` + `metadata.changed_files` on Prose chunk |
| `crates/thinkingroot-extract/src/router.rs` | Add `ManifestDependency`, `Heading`, git/link Prose → Structural |
| `crates/thinkingroot-extract/src/extractor.rs` | Fix fallthrough to also check `!relations.is_empty()` |
| `crates/thinkingroot-extract/src/structural.rs` | New `extract_manifest_dep`, `extract_heading`, `extract_git_commit`, `extract_prose_links`; extend `extract_function_def`; update dispatch + `is_structurally_extractable` |

---

### Task 1: Core types — new ChunkMetadata fields + ManifestDependency

**Files:**
- Modify: `crates/thinkingroot-core/src/ir.rs:117-138`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block at the bottom of `ir.rs`:

```rust
#[test]
fn chunk_metadata_new_fields_default() {
    let m = ChunkMetadata::default();
    assert!(m.calls_functions.is_empty());
    assert!(m.heading_level.is_none());
    assert!(m.links.is_empty());
    assert!(m.author.is_none());
    assert!(m.changed_files.is_empty());
}

#[test]
fn manifest_dependency_chunk_type_roundtrips() {
    let chunk = Chunk::new("serde = \"1\"", ChunkType::ManifestDependency, 1, 1);
    let json = serde_json::to_string(&chunk.chunk_type).unwrap();
    assert_eq!(json, "\"manifest_dependency\"");
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test -p thinkingroot-core 2>&1 | grep -E "error|FAILED|chunk_metadata_new"
```
Expected: compile error — `ManifestDependency` does not exist yet.

- [ ] **Step 3: Add 5 new fields to ChunkMetadata**

In `crates/thinkingroot-core/src/ir.rs`, after the existing `field_types: Vec<String>` field (line 138), add:

```rust
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
    pub author: Option<String>,
    /// File paths changed in this commit (from diff --stat output).
    pub changed_files: Vec<String>,
```

- [ ] **Step 4: Add ManifestDependency variant to ChunkType**

In `crates/thinkingroot-core/src/ir.rs`, after `ModuleDoc` (line 113):

```rust
    /// A single dependency declaration from a project manifest file
    /// (Cargo.toml, package.json, go.mod, requirements.txt, pyproject.toml).
    ManifestDependency,
```

- [ ] **Step 5: Run tests to confirm they pass**

```bash
cargo test -p thinkingroot-core
```
Expected: all pass. Also run `cargo check --workspace` to catch any exhaustive match breakage across the workspace.

```bash
cargo check --workspace 2>&1 | grep "error"
```
Expected: no errors. If any match arms need `ChunkType::ManifestDependency =>` added, fix them now (they'll be in `router.rs` and `structural.rs` — add `_ => ...` fallthrough or explicit arm as appropriate for context).

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-core/src/ir.rs
git commit -m "feat(core): add 5 ChunkMetadata fields + ManifestDependency chunk type"
```

---

### Task 2: Manifest parser — Cargo.toml, package.json, go.mod, requirements.txt, pyproject.toml

**Files:**
- Modify: `crates/thinkingroot-parse/Cargo.toml`
- Create: `crates/thinkingroot-parse/src/manifest.rs`
- Modify: `crates/thinkingroot-parse/src/lib.rs`

- [ ] **Step 1: Add toml dependency to parse crate**

In `crates/thinkingroot-parse/Cargo.toml`, add to `[dependencies]`:

```toml
toml = { workspace = true }
```

- [ ] **Step 2: Write failing tests**

Create `crates/thinkingroot-parse/src/manifest.rs` with just the test module first:

```rust
use std::path::Path;
use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId, SourceMetadata, SourceType};
use thinkingroot_core::{Error, Result};

pub fn parse(_path: &Path) -> Result<DocumentIR> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fake_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(name)
    }

    #[test]
    fn cargo_toml_extracts_deps() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1"
tokio = { version = "1", features = ["full"] }

[dev-dependencies]
tempfile = "3"
"#;
        let chunks = parse_cargo_toml(content);
        assert!(chunks.len() >= 3, "expected serde, tokio, tempfile");
        let dep_names: Vec<_> = chunks.iter()
            .filter_map(|c| c.metadata.import_path.as_deref())
            .collect();
        assert!(dep_names.contains(&"serde"));
        assert!(dep_names.contains(&"tokio"));
        assert!(dep_names.contains(&"tempfile"));
        // All chunks carry the project name
        assert!(chunks.iter().all(|c| c.metadata.type_name.as_deref() == Some("my-crate")));
        assert!(chunks.iter().all(|c| c.chunk_type == ChunkType::ManifestDependency));
    }

    #[test]
    fn package_json_extracts_deps() {
        let content = r#"{"name":"my-app","dependencies":{"react":"18"},"devDependencies":{"jest":"29"}}"#;
        let chunks = parse_package_json(content);
        assert_eq!(chunks.len(), 2);
        let names: Vec<_> = chunks.iter().filter_map(|c| c.metadata.import_path.as_deref()).collect();
        assert!(names.contains(&"react"));
        assert!(names.contains(&"jest"));
        assert!(chunks.iter().all(|c| c.metadata.type_name.as_deref() == Some("my-app")));
    }

    #[test]
    fn go_mod_extracts_deps() {
        let content = "module github.com/myorg/myapp\n\ngo 1.21\n\nrequire (\n\tgithub.com/foo/bar v1.2.3\n\tgolang.org/x/text v0.3.0\n)\n";
        let chunks = parse_go_mod(content);
        assert_eq!(chunks.len(), 2);
        let paths: Vec<_> = chunks.iter().filter_map(|c| c.metadata.import_path.as_deref()).collect();
        assert!(paths.contains(&"github.com/foo/bar"));
        assert!(paths.iter().all(|p| !p.contains('v') || p.starts_with('v') == false || p.contains('/')));
        assert!(chunks.iter().all(|c| c.metadata.type_name.as_deref() == Some("github.com/myorg/myapp")));
    }

    #[test]
    fn requirements_txt_extracts_deps() {
        let content = "# comment\nrequests>=2.28\nDjango==4.2\nnumpy~=1.24\n-r other.txt\n";
        let chunks = parse_requirements_txt(content);
        assert_eq!(chunks.len(), 3);
        let names: Vec<_> = chunks.iter().filter_map(|c| c.metadata.import_path.as_deref()).collect();
        assert!(names.contains(&"requests"));
        assert!(names.contains(&"Django"));
        assert!(names.contains(&"numpy"));
    }

    #[test]
    fn pyproject_toml_extracts_poetry_deps() {
        let content = r#"
[tool.poetry]
name = "my-python-app"

[tool.poetry.dependencies]
python = "^3.11"
httpx = ">=0.24"
pydantic = "^2"
"#;
        let chunks = parse_pyproject_toml(content);
        // python is filtered out
        assert_eq!(chunks.len(), 2);
        let names: Vec<_> = chunks.iter().filter_map(|c| c.metadata.import_path.as_deref()).collect();
        assert!(names.contains(&"httpx"));
        assert!(names.contains(&"pydantic"));
        assert!(chunks.iter().all(|c| c.metadata.type_name.as_deref() == Some("my-python-app")));
    }
}
```

- [ ] **Step 3: Run tests to confirm they fail**

```bash
cargo test -p thinkingroot-parse manifest 2>&1 | head -20
```
Expected: compile error — `parse_cargo_toml` etc not defined.

- [ ] **Step 4: Implement manifest.rs**

Replace the `todo!()` stub and add all parse functions. Full file:

```rust
use std::path::Path;

use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId, SourceMetadata, SourceType};
use thinkingroot_core::{Error, Result};

/// Parse a manifest file into ManifestDependency chunks.
pub fn parse(path: &Path) -> Result<DocumentIR> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::io_path(path, e))?;
    let hash = ContentHash::from_bytes(content.as_bytes());
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

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

    let chunks = match filename {
        "Cargo.toml" => parse_cargo_toml(&content),
        "pyproject.toml" => parse_pyproject_toml(&content),
        "package.json" => parse_package_json(&content),
        "go.mod" => parse_go_mod(&content),
        "requirements.txt" => parse_requirements_txt(&content),
        _ => {
            return Err(Error::UnsupportedFileType {
                extension: "unknown-manifest".to_string(),
            })
        }
    };

    for chunk in chunks {
        doc.add_chunk(chunk);
    }
    Ok(doc)
}

fn make_dep_chunk(raw_line: &str, project_name: &str, dep_name: &str) -> Chunk {
    let mut chunk = Chunk::new(raw_line, ChunkType::ManifestDependency, 0, 0);
    chunk.metadata = ChunkMetadata {
        type_name: Some(project_name.to_string()),
        import_path: Some(dep_name.to_string()),
        ..Default::default()
    };
    chunk
}

fn parse_cargo_toml(content: &str) -> Vec<Chunk> {
    let value: toml::Value = match toml::from_str(content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let project_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("unknown")
        .to_string();

    let mut chunks = Vec::new();
    for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = value.get(*section).and_then(|v| v.as_table()) {
            for (dep_name, _) in deps {
                chunks.push(make_dep_chunk(
                    &format!("{section}.{dep_name}"),
                    &project_name,
                    dep_name,
                ));
            }
        }
    }
    chunks
}

fn parse_package_json(content: &str) -> Vec<Chunk> {
    let value: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let project_name = value["name"].as_str().unwrap_or("unknown").to_string();
    let mut chunks = Vec::new();
    for section in &["dependencies", "devDependencies"] {
        if let Some(deps) = value[section].as_object() {
            for (dep_name, _) in deps {
                chunks.push(make_dep_chunk(
                    &format!("{section}.{dep_name}"),
                    &project_name,
                    dep_name,
                ));
            }
        }
    }
    chunks
}

fn parse_go_mod(content: &str) -> Vec<Chunk> {
    let mut project_name = "unknown".to_string();
    let mut in_require = false;
    let mut chunks = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("module ") {
            project_name = trimmed["module ".len()..].trim().to_string();
        } else if trimmed == "require (" {
            in_require = true;
        } else if trimmed == ")" && in_require {
            in_require = false;
        } else if in_require && !trimmed.is_empty() && !trimmed.starts_with("//") {
            // "github.com/foo/bar v1.2.3" — name is first token
            if let Some(dep_name) = trimmed.split_whitespace().next() {
                chunks.push(make_dep_chunk(trimmed, &project_name, dep_name));
            }
        } else if trimmed.starts_with("require ") && !trimmed.contains('(') {
            // single-line: `require github.com/foo/bar v1.2.3`
            let rest = trimmed["require ".len()..].trim();
            if let Some(dep_name) = rest.split_whitespace().next() {
                chunks.push(make_dep_chunk(rest, &project_name, dep_name));
            }
        }
    }
    chunks
}

fn parse_requirements_txt(content: &str) -> Vec<Chunk> {
    let project_name = "python-project".to_string();
    let mut chunks = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        // Strip version specifiers and extras: requests>=2.28[security] -> requests
        let dep_name = trimmed
            .split(|c: char| matches!(c, '>' | '<' | '=' | '!' | '~' | '[' | ';' | ' '))
            .next()
            .unwrap_or(trimmed)
            .to_string();
        if !dep_name.is_empty() {
            chunks.push(make_dep_chunk(trimmed, &project_name, &dep_name));
        }
    }
    chunks
}

fn parse_pyproject_toml(content: &str) -> Vec<Chunk> {
    let value: toml::Value = match toml::from_str(content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let project_name = value
        .get("project")
        .and_then(|p| p.get("name"))
        .or_else(|| {
            value
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.get("name"))
        })
        .and_then(|n| n.as_str())
        .unwrap_or("unknown")
        .to_string();

    let mut chunks = Vec::new();

    // [tool.poetry.dependencies]
    if let Some(deps) = value
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (dep_name, _) in deps {
            if dep_name == "python" {
                continue;
            }
            chunks.push(make_dep_chunk(
                &format!("tool.poetry.dependencies.{dep_name}"),
                &project_name,
                dep_name,
            ));
        }
    }

    // [project] dependencies = ["requests>=2.28", ...]
    if let Some(deps) = value
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep_val in deps {
            if let Some(dep_str) = dep_val.as_str() {
                let dep_name = dep_str
                    .split(|c: char| matches!(c, '>' | '<' | '=' | '!' | '~' | '[' | ';'))
                    .next()
                    .unwrap_or(dep_str)
                    .trim()
                    .to_string();
                if !dep_name.is_empty() {
                    chunks.push(make_dep_chunk(dep_str, &project_name, &dep_name));
                }
            }
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    // ... tests from Step 2 go here ...
}
```

(Copy the full test module from Step 2 into the `mod tests` block.)

- [ ] **Step 5: Wire manifest module into lib.rs**

In `crates/thinkingroot-parse/src/lib.rs`:

Add `pub mod manifest;` after the existing module declarations (line 5).

Replace the dispatcher match in `parse_file` — change:
```rust
"txt" | "toml" | "yaml" | "yml" | "json" | "cfg" | "ini" | "env" => {
    markdown::parse_as_text(path)
}
```
with:
```rust
"toml" if path.file_name().map_or(false, |n| n == "Cargo.toml" || n == "pyproject.toml") => {
    manifest::parse(path)
}
"json" if path.file_name().map_or(false, |n| n == "package.json") => {
    manifest::parse(path)
}
"mod" if path.file_name().map_or(false, |n| n == "go.mod") => {
    manifest::parse(path)
}
"txt" if path.file_name().map_or(false, |n| n == "requirements.txt") => {
    manifest::parse(path)
}
"txt" | "toml" | "yaml" | "yml" | "json" | "cfg" | "ini" | "env" => {
    markdown::parse_as_text(path)
}
```

- [ ] **Step 6: Run all tests**

```bash
cargo test -p thinkingroot-parse
```
Expected: all pass including the 5 new manifest tests.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-parse/Cargo.toml crates/thinkingroot-parse/src/manifest.rs crates/thinkingroot-parse/src/lib.rs
git commit -m "feat(parse): add manifest parser for Cargo.toml/package.json/go.mod/requirements/pyproject"
```

---

### Task 3: Code call graph — populate calls_functions in FunctionDef chunks

**Files:**
- Modify: `crates/thinkingroot-parse/src/code.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `code.rs`:

```rust
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
    assert!(calls.contains(&"helper_one".to_string()), "must detect call to helper_one");
    assert!(calls.contains(&"helper_two".to_string()), "must detect method call to helper_two");
    // Self-recursion must be excluded
    assert!(!calls.contains(&"outer".to_string()), "must not list self-recursion");
}

#[test]
fn calls_functions_deduplicated() {
    // Calling the same function twice → appears once in calls_functions
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

    let process = doc.chunks.iter().find(|c| {
        c.metadata.function_name.as_deref() == Some("process")
    }).unwrap();
    let transform_count = process.metadata.calls_functions.iter()
        .filter(|n| n.as_str() == "transform")
        .count();
    assert_eq!(transform_count, 1, "same callee must appear only once");
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test -p thinkingroot-parse function_body_calls 2>&1 | head -5
```
Expected: FAILED — `calls_functions` is always empty.

- [ ] **Step 3: Add collect_calls and last_identifier helpers**

Add these two functions anywhere after `extract_visibility` in `code.rs`:

```rust
/// Walk a function body subtree up to `depth` levels and collect all called
/// function/method names (final identifier only, deduplicated).
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
    let last = text
        .split(|c: char| c == '.' || c == ':')
        .filter(|s| !s.is_empty())
        .last()?;
    if !last.is_empty() && last.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(last.to_string())
    } else {
        None
    }
}
```

- [ ] **Step 4: Extend the FunctionDef arm to populate calls_functions**

In `extract_chunks`, the FunctionDef arm currently sets `chunk.metadata` using a struct literal (lines ~82-89). Replace that arm with:

```rust
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

    // Walk the function body for call expressions (depth-limited to avoid O(n) blowup).
    let body = find_child_by_field(&child, "body")
        .or_else(|| find_child_by_field(&child, "block"))
        .or_else(|| find_child_by_field(&child, "statement_block"));
    let mut calls = body
        .map(|b| collect_calls(source, b, 5))
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
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p thinkingroot-parse
```
Expected: all pass including `function_body_calls_are_collected` and `calls_functions_deduplicated`.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-parse/src/code.rs
git commit -m "feat(parse/code): populate calls_functions from function body AST walk"
```

---

### Task 4: Markdown structure — heading_level, heading parent, link URLs

**Files:**
- Modify: `crates/thinkingroot-parse/src/markdown.rs`

- [ ] **Step 1: Write the failing tests**

Add to `#[cfg(test)] mod tests` in `markdown.rs`:

```rust
#[test]
fn heading_level_is_captured() {
    let content = "# H1 Title\n\n## H2 Section\n\n### H3 Sub\n";
    let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
    let headings: Vec<_> = doc.chunks.iter()
        .filter(|c| c.chunk_type == ChunkType::Heading)
        .collect();
    assert_eq!(headings.len(), 3);
    assert_eq!(headings[0].metadata.heading_level, Some(1));
    assert_eq!(headings[1].metadata.heading_level, Some(2));
    assert_eq!(headings[2].metadata.heading_level, Some(3));
}

#[test]
fn heading_parent_is_set_from_stack() {
    let content = "# Top\n\n## Child\n\n### Grandchild\n\n## Sibling\n";
    let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
    let headings: Vec<_> = doc.chunks.iter()
        .filter(|c| c.chunk_type == ChunkType::Heading)
        .collect();
    // Top (H1) → no parent
    assert!(headings[0].metadata.parent.is_none(), "H1 has no parent");
    // Child (H2) → parent is "Top"
    assert_eq!(headings[1].metadata.parent.as_deref(), Some("Top"));
    // Grandchild (H3) → parent is "Child"
    assert_eq!(headings[2].metadata.parent.as_deref(), Some("Child"));
    // Sibling (H2) → parent is "Top" (sibling of Child, not child of Grandchild)
    assert_eq!(headings[3].metadata.parent.as_deref(), Some("Top"));
}

#[test]
fn prose_links_are_collected() {
    let content = "# Sec\n\nSee [OAuth docs](./oauth.md) and [external](https://example.com/docs).\n";
    let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
    let prose = doc.chunks.iter().find(|c| c.chunk_type == ChunkType::Prose).unwrap();
    assert!(prose.metadata.links.contains(&"./oauth.md".to_string()));
    assert!(prose.metadata.links.contains(&"https://example.com/docs".to_string()));
}

#[test]
fn fragment_only_links_are_skipped() {
    let content = "See [section](#intro) for details.\n";
    let doc = parse_markdown_content(Path::new("test.md"), content).unwrap();
    let prose = doc.chunks.iter().find(|c| c.chunk_type == ChunkType::Prose).unwrap();
    assert!(
        prose.metadata.links.iter().all(|l| !l.starts_with('#')),
        "fragment-only links must not be collected"
    );
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test -p thinkingroot-parse heading_level 2>&1 | head -5
```
Expected: FAILED.

- [ ] **Step 3: Add state variables to parse_markdown_content**

In `parse_markdown_content`, after the existing variable declarations (after `let mut in_list = false;`), add:

```rust
    let mut heading_stack: Vec<(u8, String)> = Vec::new(); // (level, text)
    let mut current_heading_level: u8 = 1;
    let mut current_links: Vec<String> = Vec::new();
```

- [ ] **Step 4: Update flush_prose signature to drain links**

Change the `flush_prose` function signature and body to accept and drain the links vec:

```rust
fn flush_prose(
    doc: &mut DocumentIR,
    text: &mut String,
    start_line: u32,
    end_line: u32,
    heading: &Option<String>,
    links: &mut Vec<String>,
) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        let mut chunk = Chunk::new(trimmed, ChunkType::Prose, start_line, end_line);
        if let Some(h) = heading {
            chunk = chunk.with_heading(h.clone());
        }
        chunk.metadata.links = std::mem::take(links);
        doc.add_chunk(chunk);
    }
    text.clear();
    links.clear(); // ensure cleared even if text was empty
}
```

Update all 4 callers of `flush_prose` inside `parse_markdown_content` to pass `&mut current_links` as the last argument. They are at:
1. `Event::Start(Tag::Heading { .. })` — add `, &mut current_links`
2. `Event::Start(Tag::CodeBlock(_))` — add `, &mut current_links`
3. `Event::Start(Tag::List(_))` — add `, &mut current_links`
4. The trailing call after the event loop — add `, &mut current_links`

- [ ] **Step 5: Capture heading level and build parent stack**

Replace the `Event::Start(Tag::Heading { level: _, .. })` arm with:

```rust
Event::Start(Tag::Heading { level, .. }) => {
    flush_prose(
        &mut doc,
        &mut current_text,
        current_start_line,
        line_counter,
        &current_heading,
        &mut current_links,
    );
    in_heading = true;
    heading_text.clear();
    current_heading_level = match level {
        pulldown_cmark::HeadingLevel::H1 => 1,
        pulldown_cmark::HeadingLevel::H2 => 2,
        pulldown_cmark::HeadingLevel::H3 => 3,
        pulldown_cmark::HeadingLevel::H4 => 4,
        pulldown_cmark::HeadingLevel::H5 => 5,
        pulldown_cmark::HeadingLevel::H6 => 6,
    };
}
```

Replace the `Event::End(TagEnd::Heading(_))` arm with:

```rust
Event::End(TagEnd::Heading(_)) => {
    in_heading = false;
    let heading = heading_text.trim().to_string();
    if !heading.is_empty() {
        // Pop stack entries at same or deeper level than current heading.
        while heading_stack.last().map_or(false, |(l, _)| *l >= current_heading_level) {
            heading_stack.pop();
        }
        let parent = heading_stack.last().map(|(_, t)| t.clone());

        let mut heading_chunk =
            Chunk::new(&heading, ChunkType::Heading, line_counter, line_counter)
                .with_heading(heading.clone());
        heading_chunk.metadata.heading_level = Some(current_heading_level);
        heading_chunk.metadata.parent = parent;
        doc.add_chunk(heading_chunk);

        heading_stack.push((current_heading_level, heading.clone()));
        current_heading = Some(heading);
    }
    current_start_line = line_counter + 1;
}
```

- [ ] **Step 6: Collect link URLs**

Add a new match arm inside the event loop, just before the `_ => {}` catch-all:

```rust
Event::Start(Tag::Link { dest_url, .. }) => {
    let url = dest_url.to_string();
    if !url.is_empty() && !url.starts_with('#') {
        current_links.push(url);
    }
}
```

- [ ] **Step 7: Run tests**

```bash
cargo test -p thinkingroot-parse
```
Expected: all pass including the 4 new heading/link tests.

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-parse/src/markdown.rs
git commit -m "feat(parse/markdown): capture heading_level, heading parent stack, and link URLs"
```

---

### Task 5: Git authorship — populate author and changed_files on Prose chunk

**Files:**
- Modify: `crates/thinkingroot-parse/src/git.rs`

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `git.rs`:

```rust
#[test]
fn parse_changed_files_extracts_paths() {
    let stat = " src/main.rs | 12 +++---\n crates/core/src/lib.rs |  4 ++\n 2 files changed, 16 insertions(+), 2 deletions(-)\n";
    let files = parse_changed_files(stat);
    assert_eq!(files.len(), 2);
    assert!(files.contains(&"src/main.rs".to_string()));
    assert!(files.contains(&"crates/core/src/lib.rs".to_string()));
}

#[test]
fn parse_changed_files_ignores_summary_line() {
    // The last line "N files changed..." has no pipe → must not be included
    let stat = "foo.rs | 1 +\n1 file changed, 1 insertion(+)\n";
    let files = parse_changed_files(stat);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], "foo.rs");
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test -p thinkingroot-parse parse_changed_files 2>&1 | head -5
```
Expected: compile error — `parse_changed_files` not defined.

- [ ] **Step 3: Add parse_changed_files helper**

Add this function to `git.rs` (outside the existing `parse_git_log` function, after it):

```rust
/// Extract file paths from `git diff --stat` output.
/// Each file line looks like: " path/to/file.rs | 12 +++---"
/// The summary line ("N files changed, ...") has no " | " and is skipped.
fn parse_changed_files(diff_stat: &str) -> Vec<String> {
    diff_stat
        .lines()
        .filter_map(|line| {
            let pipe_pos = line.find(" |")?;
            let path = line[..pipe_pos].trim().to_string();
            if path.is_empty() { None } else { Some(path) }
        })
        .collect()
}
```

- [ ] **Step 4: Restructure the commit parsing loop to get diff before creating Prose chunk**

In `parse_git_log`, replace the section that creates the Prose chunk and runs git diff (currently lines ~69-89) with:

```rust
        // Run diff stat first so we can embed it in the Prose chunk metadata.
        let changed_files = if let Ok(diff_output) = Command::new("git")
            .args(["diff", &format!("{sha}^..{sha}"), "--stat"])
            .current_dir(repo_path)
            .output()
        {
            if diff_output.status.success() {
                let diff_stat = String::from_utf8_lossy(&diff_output.stdout);
                parse_changed_files(&diff_stat)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Commit message as a Prose chunk, carrying author + changed files metadata.
        let message = if body.is_empty() {
            subject.to_string()
        } else {
            format!("{subject}\n\n{body}")
        };
        let mut prose_chunk = Chunk::new(&message, ChunkType::Prose, 1, 1);
        prose_chunk.metadata.author = Some(author.to_string());
        prose_chunk.metadata.changed_files = changed_files;
        doc.add_chunk(prose_chunk);
```

Remove the old `let message = ...` block and the old `if let Ok(diff_output) = ...` block that added a separate Code chunk — they are now replaced by the code above.

- [ ] **Step 5: Add ChunkMetadata to the git.rs imports**

The file currently imports `use thinkingroot_core::ir::{Chunk, ChunkType, DocumentIR};`. Add `ChunkMetadata`:

```rust
use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
```

- [ ] **Step 6: Run tests**

```bash
cargo test -p thinkingroot-parse
```
Expected: all pass. The existing `non_git_dir_returns_empty` test must still pass.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-parse/src/git.rs
git commit -m "feat(parse/git): populate metadata.author and metadata.changed_files on commit Prose chunks"
```

---

### Task 6: Router + extractor — classify new types, fix fallthrough

**Files:**
- Modify: `crates/thinkingroot-extract/src/router.rs`
- Modify: `crates/thinkingroot-extract/src/extractor.rs:160`

- [ ] **Step 1: Write failing tests in router.rs**

Add to `#[cfg(test)] mod tests` in `router.rs`:

```rust
#[test]
fn manifest_dependency_is_structural() {
    let c = chunk(ChunkType::ManifestDependency, ChunkMetadata::default());
    assert_eq!(classify(&c), Tier::Structural);
}

#[test]
fn heading_is_structural() {
    let c = chunk(ChunkType::Heading, ChunkMetadata::default());
    assert_eq!(classify(&c), Tier::Structural);
}

#[test]
fn prose_with_author_is_structural() {
    let c = chunk(
        ChunkType::Prose,
        ChunkMetadata {
            author: Some("Alice".to_string()),
            ..Default::default()
        },
    );
    assert_eq!(classify(&c), Tier::Structural);
}

#[test]
fn prose_with_links_is_structural() {
    let c = chunk(
        ChunkType::Prose,
        ChunkMetadata {
            links: vec!["./foo.md".to_string()],
            ..Default::default()
        },
    );
    assert_eq!(classify(&c), Tier::Structural);
}

#[test]
fn prose_without_author_or_links_is_llm() {
    let c = chunk(ChunkType::Prose, ChunkMetadata::default());
    assert_eq!(classify(&c), Tier::Llm);
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test -p thinkingroot-extract manifest_dependency_is_structural 2>&1 | head -5
```
Expected: FAILED — `ManifestDependency` falls to `_ => Tier::Llm`.

- [ ] **Step 3: Update classify() in router.rs**

Replace the `_ => Tier::Llm` arm and add new arms. The full updated `classify` function:

```rust
pub fn classify(chunk: &Chunk) -> Tier {
    match chunk.chunk_type {
        ChunkType::FunctionDef => {
            if chunk.metadata.function_name.as_deref().is_some_and(|n| !n.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        ChunkType::TypeDef => {
            if chunk.metadata.type_name.as_deref().is_some_and(|n| !n.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        ChunkType::Import => {
            if chunk.metadata.import_path.as_deref().is_some_and(|p| !p.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        // ManifestDependency always carries type_name + import_path (set by manifest parser).
        ChunkType::ManifestDependency => Tier::Structural,
        // Heading always carries heading_level (set by markdown parser).
        ChunkType::Heading => Tier::Structural,
        // Git commit Prose (has author) and link-bearing Prose are structurally extractable.
        ChunkType::Prose => {
            if chunk.metadata.author.is_some() || !chunk.metadata.links.is_empty() {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        _ => Tier::Llm,
    }
}
```

- [ ] **Step 4: Fix fallthrough condition in extractor.rs**

In `crates/thinkingroot-extract/src/extractor.rs`, find the structural fallthrough check (around line 160):

```rust
if !result.claims.is_empty() || !result.entities.is_empty() {
```

Replace with:

```rust
if !result.claims.is_empty() || !result.entities.is_empty() || !result.relations.is_empty() {
```

This ensures that Prose chunks with only link relations (no claims/entities) are captured structurally rather than silently discarded and sent to LLM.

- [ ] **Step 5: Run all tests**

```bash
cargo test -p thinkingroot-extract
```
Expected: all pass. The existing `prose_is_always_llm` test must be updated since Prose with default metadata is still LLM — verify that specific test still reads `ChunkMetadata::default()` (no author, no links) → passes.

Also run the integration test:
```bash
cargo test -p thinkingroot-extract router_correctly_splits_mixed_document
```
Expected: pass — the Prose chunk in that test has default metadata (no author/links) → still LLM.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-extract/src/router.rs crates/thinkingroot-extract/src/extractor.rs
git commit -m "feat(extract/router): classify ManifestDependency+Heading+git/link Prose as Structural"
```

---

### Task 7: Structural extractors — 4 new functions + extend existing + dispatch

**Files:**
- Modify: `crates/thinkingroot-extract/src/structural.rs`

- [ ] **Step 1: Write failing tests**

Add to `#[cfg(test)] mod tests` in `structural.rs`:

```rust
// ── Gap 1: ManifestDependency ─────────────────────────────────────────────

#[test]
fn manifest_dep_produces_depends_on_relation() {
    let mut chunk = Chunk::new("serde = \"1\"", ChunkType::ManifestDependency, 1, 1);
    chunk.metadata = ChunkMetadata {
        type_name: Some("my-crate".to_string()),
        import_path: Some("serde".to_string()),
        ..Default::default()
    };
    let result = extract_structural(&chunk, "Cargo.toml");
    let dep = result.relations.iter().find(|r| r.relation_type == "depends_on");
    assert!(dep.is_some(), "must emit depends_on relation");
    let dep = dep.unwrap();
    assert_eq!(dep.from_entity, "my-crate");
    assert_eq!(dep.to_entity, "serde");
    assert_eq!(dep.confidence, 0.99);
    let claim = result.claims.iter().find(|c| c.claim_type == "dependency");
    assert!(claim.is_some(), "must emit dependency claim");
    assert!(claim.unwrap().statement.contains("my-crate"));
    assert!(claim.unwrap().statement.contains("serde"));
}

#[test]
fn manifest_dep_missing_fields_returns_empty() {
    let chunk = make_chunk(ChunkType::ManifestDependency, "", ChunkMetadata::default());
    let result = extract_structural(&chunk, "Cargo.toml");
    assert!(result.claims.is_empty());
    assert!(result.relations.is_empty());
}

// ── Gap 2: FunctionDef call graph ─────────────────────────────────────────

#[test]
fn function_def_with_calls_produces_calls_relations() {
    let meta = ChunkMetadata {
        function_name: Some("process".to_string()),
        calls_functions: vec!["validate".to_string(), "persist".to_string()],
        ..Default::default()
    };
    let chunk = make_chunk(ChunkType::FunctionDef, "fn process() {}", meta);
    let result = extract_structural(&chunk, "src/handler.rs");
    let calls: Vec<_> = result.relations.iter().filter(|r| r.relation_type == "calls").collect();
    assert_eq!(calls.len(), 2, "one calls relation per callee");
    assert!(calls.iter().any(|r| r.to_entity == "validate"));
    assert!(calls.iter().any(|r| r.to_entity == "persist"));
    assert!(calls.iter().all(|r| r.from_entity == "process"));
    assert!(calls.iter().all(|r| r.confidence == 0.99));
}

// ── Gap 3: Heading hierarchy ──────────────────────────────────────────────

#[test]
fn heading_with_no_parent_uses_file_as_container() {
    let mut chunk = Chunk::new("Introduction", ChunkType::Heading, 1, 1);
    chunk.heading = Some("Introduction".to_string());
    chunk.metadata.heading_level = Some(1);
    // No parent set
    let result = extract_structural(&chunk, "docs/guide.md");
    let contains = result.relations.iter().find(|r| r.relation_type == "contains");
    assert!(contains.is_some(), "must emit contains relation");
    assert_eq!(contains.unwrap().from_entity, "guide.md");
    assert_eq!(contains.unwrap().to_entity, "Introduction");
}

#[test]
fn heading_with_parent_uses_parent_as_container() {
    let mut chunk = Chunk::new("Sub-section", ChunkType::Heading, 5, 5);
    chunk.heading = Some("Sub-section".to_string());
    chunk.metadata.heading_level = Some(2);
    chunk.metadata.parent = Some("Overview".to_string());
    let result = extract_structural(&chunk, "docs/guide.md");
    let contains = result.relations.iter().find(|r| r.relation_type == "contains");
    assert!(contains.is_some());
    assert_eq!(contains.unwrap().from_entity, "Overview");
    assert_eq!(contains.unwrap().to_entity, "Sub-section");
}

// ── Gap 3b: Prose links ───────────────────────────────────────────────────

#[test]
fn prose_links_produce_related_to_relations() {
    let meta = ChunkMetadata {
        links: vec!["./oauth.md".to_string(), "https://example.com".to_string()],
        ..Default::default()
    };
    let chunk = make_chunk(ChunkType::Prose, "See oauth.md and example.com.", meta);
    let result = extract_structural(&chunk, "docs/guide.md");
    let refs: Vec<_> = result.relations.iter().filter(|r| r.relation_type == "related_to").collect();
    assert_eq!(refs.len(), 2);
    // relative → 0.99, absolute → 0.7
    let rel = refs.iter().find(|r| r.to_entity == "./oauth.md").unwrap();
    assert_eq!(rel.confidence, 0.99);
    let abs = refs.iter().find(|r| r.to_entity == "https://example.com").unwrap();
    assert_eq!(abs.confidence, 0.7);
}

// ── Gap 4: Git authorship ────────────────────────────────────────────────

#[test]
fn git_commit_produces_created_by_relations() {
    let meta = ChunkMetadata {
        author: Some("Alice".to_string()),
        changed_files: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
        ..Default::default()
    };
    let chunk = make_chunk(ChunkType::Prose, "fix: correct off-by-one error", meta);
    let result = extract_structural(&chunk, "git://abc123def456");
    let created: Vec<_> = result.relations.iter()
        .filter(|r| r.relation_type == "created_by")
        .collect();
    assert_eq!(created.len(), 2, "one created_by per changed file");
    assert!(created.iter().all(|r| r.to_entity == "Alice"));
    assert!(created.iter().all(|r| r.confidence == 0.7));
    // Claim must include the SHA
    assert!(result.claims.iter().any(|c| c.statement.contains("abc123def456")));
}

#[test]
fn git_commit_missing_author_returns_empty() {
    let meta = ChunkMetadata {
        changed_files: vec!["src/lib.rs".to_string()],
        ..Default::default()
    };
    let chunk = make_chunk(ChunkType::Prose, "commit msg", meta);
    let result = extract_structural(&chunk, "git://abc123");
    assert!(result.claims.is_empty());
    assert!(result.relations.is_empty());
}

// ── Predicate ────────────────────────────────────────────────────────────

#[test]
fn is_structurally_extractable_includes_new_types() {
    for ct in [ChunkType::ManifestDependency, ChunkType::Heading] {
        let chunk = make_chunk(ct, "", ChunkMetadata::default());
        assert!(
            is_structurally_extractable(&chunk),
            "{ct:?} should be structurally extractable"
        );
    }
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test -p thinkingroot-extract manifest_dep_produces 2>&1 | head -5
```
Expected: FAILED.

- [ ] **Step 3: Add extract_manifest_dep**

Add this function before `extract_import` in `structural.rs`:

```rust
/// ManifestDependency → Entity(project) + Entity(library) + Relation(depends_on) + Claim(dependency)
fn extract_manifest_dep(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let project = match &chunk.metadata.type_name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => return ExtractionResult::empty(),
    };
    let library = match &chunk.metadata.import_path {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return ExtractionResult::empty(),
    };
    let file_name = file_name_from_uri(source_uri);

    let project_entity = ExtractedEntity {
        name: project.clone(),
        entity_type: "system".to_string(),
        aliases: Vec::new(),
        description: Some(format!("{project} is a project defined in {file_name}")),
    };
    let library_entity = ExtractedEntity {
        name: library.clone(),
        entity_type: "library".to_string(),
        aliases: Vec::new(),
        description: Some(format!("{library} is a dependency of {project}")),
    };
    let dep_relation = ExtractedRelation {
        from_entity: project.clone(),
        to_entity: library.clone(),
        relation_type: "depends_on".to_string(),
        description: Some(format!("{project} depends on {library}")),
        confidence: 0.99,
    };
    let dep_claim = ExtractedClaim {
        statement: format!("{project} depends on {library}"),
        claim_type: "dependency".to_string(),
        confidence: 0.99,
        entities: vec![project.clone(), library.clone()],
        source_quote: Some(chunk.content.lines().next().unwrap_or("").to_string()),
        extraction_tier: ExtractionTier::Structural,
    };

    ExtractionResult {
        claims: vec![dep_claim],
        entities: vec![project_entity, library_entity],
        relations: vec![dep_relation],
    }
}
```

- [ ] **Step 4: Add extract_heading**

```rust
/// Heading → Entity(heading) + Relation(container contains heading) + Claim(definition)
fn extract_heading(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let heading_text = match chunk.heading.as_deref() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => {
            let t = chunk.content.trim().to_string();
            if t.is_empty() {
                return ExtractionResult::empty();
            }
            t
        }
    };

    let file_name = file_name_from_uri(source_uri);
    let container_name = chunk.metadata.parent.clone().unwrap_or_else(|| file_name.clone());
    let container_type = if chunk.metadata.parent.is_some() { "concept" } else { "file" };

    let heading_entity = ExtractedEntity {
        name: heading_text.clone(),
        entity_type: "concept".to_string(),
        aliases: Vec::new(),
        description: Some(format!("Section in {file_name}")),
    };
    let container_entity = ExtractedEntity {
        name: container_name.clone(),
        entity_type: container_type.to_string(),
        aliases: Vec::new(),
        description: None,
    };
    let contains_rel = ExtractedRelation {
        from_entity: container_name.clone(),
        to_entity: heading_text.clone(),
        relation_type: "contains".to_string(),
        description: Some(format!("{container_name} contains section {heading_text}")),
        confidence: 0.99,
    };
    let def_claim = ExtractedClaim {
        statement: format!("{heading_text} is a section in {file_name}"),
        claim_type: "definition".to_string(),
        confidence: 0.99,
        entities: vec![heading_text.clone(), file_name.clone()],
        source_quote: None,
        extraction_tier: ExtractionTier::Structural,
    };

    ExtractionResult {
        claims: vec![def_claim],
        entities: vec![heading_entity, container_entity],
        relations: vec![contains_rel],
    }
}
```

- [ ] **Step 5: Add extract_git_commit**

```rust
/// Git commit Prose → Entity(author) + Entity(file) × N + Relation(created_by) × N + Claim(fact) × N
fn extract_git_commit(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let author = match &chunk.metadata.author {
        Some(a) if !a.is_empty() => a.clone(),
        _ => return ExtractionResult::empty(),
    };
    if chunk.metadata.changed_files.is_empty() {
        return ExtractionResult::empty();
    }

    let sha = source_uri.trim_start_matches("git://");
    let author_entity = ExtractedEntity {
        name: author.clone(),
        entity_type: "person".to_string(),
        aliases: Vec::new(),
        description: Some(format!("{author} is a code contributor")),
    };

    let mut result = ExtractionResult {
        claims: Vec::new(),
        entities: vec![author_entity],
        relations: Vec::new(),
    };

    for file_path in &chunk.metadata.changed_files {
        result.entities.push(ExtractedEntity {
            name: file_path.clone(),
            entity_type: "file".to_string(),
            aliases: Vec::new(),
            description: None,
        });
        result.relations.push(ExtractedRelation {
            from_entity: file_path.clone(),
            to_entity: author.clone(),
            relation_type: "created_by".to_string(),
            description: Some(format!("{author} modified {file_path} in commit {sha}")),
            confidence: 0.7,
        });
        result.claims.push(ExtractedClaim {
            statement: format!("{author} modified {file_path} in commit {sha}"),
            claim_type: "fact".to_string(),
            confidence: 0.99,
            entities: vec![author.clone(), file_path.clone()],
            source_quote: None,
            extraction_tier: ExtractionTier::Structural,
        });
    }
    result
}
```

- [ ] **Step 6: Add extract_prose_links**

```rust
/// Prose with links → Entity(source_doc) + Entity(target) × N + Relation(related_to) × N
/// Relative paths get confidence 0.99; absolute URLs get 0.7.
fn extract_prose_links(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    if chunk.metadata.links.is_empty() {
        return ExtractionResult::empty();
    }

    let source_doc = file_name_from_uri(source_uri);
    let source_entity = ExtractedEntity {
        name: source_doc.clone(),
        entity_type: "file".to_string(),
        aliases: Vec::new(),
        description: None,
    };

    let mut result = ExtractionResult {
        claims: Vec::new(),
        entities: vec![source_entity],
        relations: Vec::new(),
    };

    for url in &chunk.metadata.links {
        // Relative: starts with '.' or has no scheme ("://")
        let is_relative = url.starts_with('.') || !url.contains("://");
        let confidence = if is_relative { 0.99 } else { 0.7 };

        result.entities.push(ExtractedEntity {
            name: url.clone(),
            entity_type: if is_relative { "file" } else { "concept" }.to_string(),
            aliases: Vec::new(),
            description: None,
        });
        result.relations.push(ExtractedRelation {
            from_entity: source_doc.clone(),
            to_entity: url.clone(),
            // `references` is not a RelationType variant yet; related_to is the approved fallback.
            relation_type: "related_to".to_string(),
            description: Some(format!("{source_doc} links to {url}")),
            confidence,
        });
    }
    result
}
```

- [ ] **Step 7: Extend extract_function_def for calls_functions**

In the existing `extract_function_def` function, at the very end — after the `if let Some(parent) = ...` block and before `result` is returned — add:

```rust
    // For each called function, emit a calls relation and claim.
    for callee in &chunk.metadata.calls_functions {
        result.entities.push(ExtractedEntity {
            name: callee.clone(),
            entity_type: "function".to_string(),
            aliases: Vec::new(),
            description: Some(format!("Function called by {name}")),
        });
        result.relations.push(ExtractedRelation {
            from_entity: name.clone(),
            to_entity: callee.clone(),
            relation_type: "calls".to_string(),
            description: Some(format!("{name} calls {callee}")),
            confidence: 0.99,
        });
        result.claims.push(ExtractedClaim {
            statement: format!("{name} calls {callee}"),
            claim_type: "dependency".to_string(),
            confidence: 0.99,
            entities: vec![name.clone(), callee.clone()],
            source_quote: None,
            extraction_tier: ExtractionTier::Structural,
        });
    }
```

- [ ] **Step 8: Update extract_structural dispatch and is_structurally_extractable**

Replace the `extract_structural` function body:

```rust
pub fn extract_structural(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    match chunk.chunk_type {
        ChunkType::FunctionDef => extract_function_def(chunk, source_uri),
        ChunkType::TypeDef => extract_type_def(chunk, source_uri),
        ChunkType::Import => extract_import(chunk, source_uri),
        ChunkType::Comment | ChunkType::ModuleDoc => extract_doc_comment(chunk, source_uri),
        ChunkType::ManifestDependency => extract_manifest_dep(chunk, source_uri),
        ChunkType::Heading => extract_heading(chunk, source_uri),
        ChunkType::Prose => {
            if chunk.metadata.author.is_some() {
                extract_git_commit(chunk, source_uri)
            } else {
                extract_prose_links(chunk, source_uri)
            }
        }
        _ => ExtractionResult::empty(),
    }
}
```

Replace `is_structurally_extractable`:

```rust
pub fn is_structurally_extractable(chunk: &Chunk) -> bool {
    matches!(
        chunk.chunk_type,
        ChunkType::FunctionDef
            | ChunkType::TypeDef
            | ChunkType::Import
            | ChunkType::ManifestDependency
            | ChunkType::Heading
    ) || (chunk.chunk_type == ChunkType::Prose
        && (chunk.metadata.author.is_some() || !chunk.metadata.links.is_empty()))
}
```

Update the existing `is_structurally_extractable_rejects_prose_code_etc` test — remove `ChunkType::Heading` from the "should not be extractable" list since it now is:

```rust
#[test]
fn is_structurally_extractable_rejects_prose_code_etc() {
    for ct in [
        ChunkType::Prose,  // default metadata → no author/links → not extractable
        ChunkType::Code,
        ChunkType::List,
        ChunkType::Table,
        ChunkType::Comment,
        ChunkType::ModuleDoc,
    ] {
        let chunk = make_chunk(ct, "", ChunkMetadata::default());
        assert!(
            !is_structurally_extractable(&chunk),
            "{ct:?} should NOT be structurally extractable"
        );
    }
}
```

- [ ] **Step 9: Run all tests**

```bash
cargo test -p thinkingroot-extract
```
Expected: all pass. Key tests to verify:
- `manifest_dep_produces_depends_on_relation` ✓
- `function_def_with_calls_produces_calls_relations` ✓
- `heading_with_no_parent_uses_file_as_container` ✓
- `prose_links_produce_related_to_relations` ✓
- `git_commit_produces_created_by_relations` ✓
- All existing tests still pass ✓

- [ ] **Step 10: Run full workspace check**

```bash
cargo test --no-default-features 2>&1 | tail -20
```
Expected: all tests pass across all crates.

- [ ] **Step 11: Commit**

```bash
git add crates/thinkingroot-extract/src/structural.rs
git commit -m "feat(extract/structural): add manifest_dep/heading/git_commit/prose_links extractors + call graph"
```

---

## Final Verification

After all 7 tasks are complete:

```bash
# Full workspace compile check
cargo check --workspace

# Full test suite
cargo test --no-default-features

# Clippy
cargo clippy --workspace -- -D warnings
```

All three must pass with zero errors before declaring the feature complete.
