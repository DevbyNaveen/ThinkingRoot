# Maximum Source Utilization Design

**Date:** 2026-04-12  
**Status:** Approved  
**Branch:** feat/max-ast-utilization

## Goal

Eliminate four structural extraction gaps so ThinkingRoot produces a richer, denser knowledge graph from every source it already ingests — with zero additional LLM cost. All new relations are emitted at 0.99 confidence by the structural extractor.

## Background

ThinkingRoot currently ingests code (5 languages), markdown, PDFs, git commits, and config files. The structural extractor uses tree-sitter AST outputs and document structure to emit relations without LLM. Four gaps exist where parsed data is thrown away:

1. **Config/manifest files** (Cargo.toml, package.json, go.mod, etc.) are treated as plain prose — dependency declarations are never extracted as `depends_on` relations.
2. **Function call expressions** are parsed by tree-sitter but `calls_functions` is never populated — no call graph is produced.
3. **Markdown headings and hyperlinks** produce `ChunkType::Heading` chunks but the structural extractor returns empty for them — no heading hierarchy or cross-document references.
4. **Git commit authorship** is parsed but author name and changed files are never linked — no `created_by` or `owned_by` relations.

## Architecture

Same 3-layer pattern as existing structural extraction:

```
Parser → ChunkMetadata fields → Structural extractor → Relations (0.99 confidence)
```

Changes span all three layers: new fields on `ChunkMetadata`, new/extended parsers, new structural extractor functions.

## Layer 1: Core Type Changes (`crates/thinkingroot-core/src/ir.rs`)

### New ChunkMetadata fields

```rust
pub struct ChunkMetadata {
    // ... existing fields unchanged ...

    // Gap 2: Code call graph
    /// Functions/methods called within this function body.
    /// Each entry is the callee's simple name (final identifier only).
    /// e.g. `user_service.find_by_email()` → `"find_by_email"`
    pub calls_functions: Vec<String>,

    // Gap 3: Markdown structure
    /// Heading depth: H1=1, H2=2, H3=3, etc. None for non-heading chunks.
    pub heading_level: Option<u8>,
    /// Hyperlink targets (URLs or relative paths) found in this chunk.
    pub links: Vec<String>,

    // Gap 4: Git history
    /// Commit author name (git commits only).
    pub author: Option<String>,
    /// File paths changed in this commit (from diff stats).
    pub changed_files: Vec<String>,
}
```

All new fields derive `Default` automatically (`Vec::new()`, `None`). No manual Default impl changes needed.

### New ChunkType variant

```rust
pub enum ChunkType {
    // ... existing variants unchanged ...
    /// A single dependency declaration from a project manifest file.
    /// e.g. one line from Cargo.toml [dependencies], package.json, go.mod.
    ManifestDependency,
}
```

## Layer 2: Parser Changes

### Gap 1: New manifest parser (`crates/thinkingroot-parse/src/manifest.rs`)

Parses project manifest files into one `ManifestDependency` chunk per dependency. Each chunk carries:
- `content`: the raw dependency declaration line
- `metadata.type_name`: the project/package name (from `[package] name` or `"name"` key)
- `metadata.import_path`: the dependency name (stripped of version)

**Supported formats:**

| File | Parse target |
|---|---|
| `Cargo.toml` | `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` sections |
| `package.json` | `dependencies` and `devDependencies` objects |
| `go.mod` | `require` block lines |
| `requirements.txt` | Each non-comment line (package name only, strip `>=`, `==`, `~=`) |
| `pyproject.toml` | `[tool.poetry.dependencies]` and `[project] dependencies` |

**Project name extraction:**
- Cargo.toml: `[package] name = "..."` 
- package.json: `"name": "..."`
- go.mod: `module <name>` first line
- requirements.txt / pyproject.toml: filename's parent directory name as fallback

**Dispatcher routing** (`crates/thinkingroot-parse/src/lib.rs`):

```rust
"toml" if filename == "Cargo.toml" || filename == "pyproject.toml" => manifest::parse(path),
"json" if filename == "package.json" => manifest::parse(path),
"mod"  if filename == "go.mod"       => manifest::parse(path),
"txt"  if filename == "requirements.txt" => manifest::parse(path),
// other .toml/.json/.txt → existing plain-text fallback
```

### Gap 2: Code parser extension (`crates/thinkingroot-parse/src/code.rs`)

When building a `FunctionDef` chunk, walk the function body subtree and collect all call expression targets into `calls_functions`.

**Node types by language:**

| Language | Call node | Name extraction |
|---|---|---|
| Rust | `call_expression` | `function` field → last identifier |
| Rust | `method_call_expression` | `method` field → identifier text |
| Python | `call` | `function` field → last identifier or attribute |
| JavaScript/TypeScript | `call_expression` | `function` field → identifier or member property |
| Go | `call_expression` | `function` field → last identifier or selector |

**Name normalization:** Extract the final identifier only. `user_service.find_by_email()` → `"find_by_email"`. `AuthService::validate()` → `"validate"`. This avoids receiver/namespace noise while preserving the actionable name. Deduplicate within a single function body.

**Depth limit:** Walk up to 5 levels deep into the function body to avoid O(n) blowup on deeply nested closures. Enough to capture 99% of real call sites.

### Gap 3: Markdown parser extension (`crates/thinkingroot-parse/src/markdown.rs`)

**Heading hierarchy:**
- When emitting a `Heading` chunk, set `metadata.heading_level` from the pulldown-cmark `HeadingLevel` enum (H1→1, H2→2, etc.)
- Track current heading stack (vec of `(level, text)`) to set `metadata.parent` to the nearest ancestor heading

**Hyperlinks:**
- During pulldown-cmark event processing, collect `Tag::Link(_, url, _)` URLs into the current chunk's `metadata.links`
- Include only non-empty, non-anchor-only URLs (skip `#fragment-only` links)
- Relative paths kept as-is (e.g. `./oauth.md`), absolute URLs kept as-is

### Gap 4: Git parser extension (`crates/thinkingroot-parse/src/git.rs`)

When building the commit `Prose` chunk:
- Set `metadata.author` from the `git log` author field
- Parse the diff stat chunk to extract file paths → set `metadata.changed_files`

Diff stat format: `path/to/file.rs | 12 +++---` — extract the path before the `|`.

## Layer 3: Structural Extractor Changes (`crates/thinkingroot-extract/src/structural.rs`)

### Gap 1: `extract_manifest_dep` (new function)

Called for `ChunkType::ManifestDependency`.

Emits:
- Entity: project name (from `metadata.type_name`)
- Entity: library name (from `metadata.import_path`)
- Relation: `depends_on(project → library)` at confidence 0.99
- Claim: `"{project} depends on {library}"` type=`dependency`

### Gap 2: `extract_function_def` extension

After existing logic, iterate `chunk.metadata.calls_functions`:

For each callee name:
- Entity: callee function (type=`function`, description=`"Function called by {caller}"`)
- Relation: `calls(caller → callee)` at confidence 0.99
- Claim: `"{caller} calls {callee}"` type=`dependency`

Skip callees whose name matches the caller (self-recursion is uninteresting noise).

### Gap 3: `extract_heading` (new function)

Called for `ChunkType::Heading`.

Emits:
- Entity: heading text as concept entity
- If `metadata.parent` is set: `contains(parent_heading → this_heading)` at 0.99
- If no parent: `contains(source_file → this_heading)` at 0.99
- Claim: `"{heading} is a section in {source_file}"` type=`definition`

`extract_prose` extension — for links:

For each URL in `metadata.links`:
- If relative path (starts with `.` or no scheme): emit `references(current_doc → linked_doc)` at 0.99
- If absolute URL: emit `references(current_doc → URL)` at 0.7 (external, lower confidence)

### Gap 4: `extract_git_commit` (new function)

Called for `ChunkType::Prose` where `source_uri` starts with `git://`.

For each file in `metadata.changed_files`:
- Entity: file path (type=`file`)
- Entity: author name (type=`person`)
- Relation: `created_by(file → author)` at confidence 0.7 (one commit ≠ full ownership; noisy-OR across commits will build up strength)

Also emits:
- Claim: `"{author} modified {file} in commit {sha}"` type=`fact`

**Note:** `owned_by` is NOT emitted here — ownership should emerge from accumulated `created_by` strength via noisy-OR across many commits, not from a single commit.

### `is_structurally_extractable` predicate update

Add `ChunkType::ManifestDependency` and `ChunkType::Heading` to the set of extractable types.

## Relation types used (all pre-existing)

| Gap | Relation | Already in RelationType enum |
|---|---|---|
| Manifest | `depends_on` | Yes |
| Call graph | `calls` | Yes |
| Heading hierarchy | `contains` | Yes |
| Markdown links | `references` | No — maps to `related_to` temporarily, upgrade in future |
| Git authorship | `created_by` | Yes |

**Note on `references`:** `RelationType` does not have a `References` variant. Cross-document links will use `RelatedTo` at 0.7 confidence until a `References` type is added to the core type system. This is explicitly a known limitation — tracked for Phase 4.

## File Map

| File | Change |
|---|---|
| `crates/thinkingroot-core/src/ir.rs` | Add 5 fields to `ChunkMetadata`, add `ManifestDependency` to `ChunkType` |
| `crates/thinkingroot-parse/src/manifest.rs` | **NEW** — manifest parser for all 5 formats |
| `crates/thinkingroot-parse/src/lib.rs` | Add manifest routing, export `manifest` module |
| `crates/thinkingroot-parse/src/code.rs` | Populate `calls_functions` from function body walk |
| `crates/thinkingroot-parse/src/markdown.rs` | Populate `heading_level`, `links` |
| `crates/thinkingroot-parse/src/git.rs` | Populate `author`, `changed_files` |
| `crates/thinkingroot-extract/src/structural.rs` | New `extract_manifest_dep`, `extract_heading`, `extract_git_commit`; extend `extract_function_def`; extend `extract_prose`; update dispatch |

## Testing Strategy

Each gap has unit tests in the relevant extractor/parser:
- Manifest: parse a minimal Cargo.toml string → assert `depends_on` relations
- Call graph: construct a `FunctionDef` chunk with `calls_functions` set → assert `calls` relations
- Heading: construct a `Heading` chunk with parent set → assert `contains` relation
- Git: construct a git `Prose` chunk with author/changed_files → assert `created_by` relations

Parser-level tests (in `code.rs`, `markdown.rs`, `git.rs`) verify the metadata fields are populated correctly before the extractor sees them.

## Explicitly Out of Scope

- Adding new programming languages (separate sub-project)
- PDF structure extraction (requires different library, separate sub-project)
- `References` as a first-class `RelationType` (requires core type change, Phase 4)
- Generic type bounds extraction (high noise/low signal)
- Parameter types → `depends_on` (redundant with field_types and imports already handled)
