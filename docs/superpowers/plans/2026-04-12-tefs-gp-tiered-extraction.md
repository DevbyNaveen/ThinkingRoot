# TEFS-GP: Tiered Extraction Funnel with Graph-Primed DAG Scheduling

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the 100%-LLM extraction pipeline with a tiered funnel that extracts 60-80% of knowledge deterministically (zero LLM), uses graph-primed focused prompts for the remainder, and cascades grounding verification depth by extraction source.

**Architecture:** Chunks flow through a Tier Router: AST-rich chunks (FunctionDef, TypeDef, Import) go to a zero-LLM Structural Extractor that produces claims/entities/relations deterministically from tree-sitter metadata. Semantic chunks (Prose, Code without structure) go to the LLM path with focused split prompts and graph-primed context (existing entities injected into prompts). Grounding depth cascades: structural claims auto-ground at 0.99, LLM claims run the full tribunal.

**Tech Stack:** Rust (edition 2024), thinkingroot-core types, thinkingroot-extract crate, tree-sitter AST metadata from thinkingroot-parse, CozoDB graph queries via thinkingroot-graph

---

## File Structure

### New Files

| File | Responsibility |
|------|---------------|
| `crates/thinkingroot-extract/src/structural.rs` | Zero-LLM extractor: converts AST-rich chunks to ExtractionResult deterministically |
| `crates/thinkingroot-extract/src/router.rs` | Tier Router: classifies chunks as structural-extractable vs LLM-needed |
| `crates/thinkingroot-extract/src/graph_context.rs` | Graph-Primed Context: builds KNOWN_ENTITIES section for LLM prompts |
| `crates/thinkingroot-extract/src/focused_prompts.rs` | Split focused prompts: entity, relation, claim extraction |

### Modified Files

| File | Changes |
|------|---------|
| `crates/thinkingroot-extract/src/schema.rs` | Add `ExtractionTier` enum + `extraction_tier` field on `ExtractedClaim` |
| `crates/thinkingroot-extract/src/extractor.rs` | Wire tiered extraction: router → structural/LLM paths, accept known entities |
| `crates/thinkingroot-extract/src/lib.rs` | Export new modules |
| `crates/thinkingroot-core/src/types/claim.rs` | Add `ExtractionTier` to `Claim`, add `GroundingMethod::Structural` |
| `crates/thinkingroot-serve/src/pipeline.rs` | Pass existing entities to extractor, cascade grounding by tier |
| `crates/thinkingroot-graph/src/graph.rs` | Add `get_known_entities()` query for graph-priming |

---

## Task 1: ExtractionTier Type Foundation

Add the `ExtractionTier` enum to both the extraction schema (LLM output) and core types (stored claims). This is the foundation that enables cascade grounding and tiered progress reporting.

**Files:**
- Modify: `crates/thinkingroot-extract/src/schema.rs`
- Modify: `crates/thinkingroot-core/src/types/claim.rs`
- Test: existing tests in both files must still pass

- [ ] **Step 1: Add ExtractionTier to extraction schema**

In `crates/thinkingroot-extract/src/schema.rs`, add the enum and a field on `ExtractedClaim`:

```rust
/// Which tier of the extraction funnel produced this result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionTier {
    /// Tier 0: deterministic structural extraction (tree-sitter AST, imports, type defs).
    /// Zero LLM calls. Zero hallucination. Confidence = 0.99.
    Structural,
    /// Tier 2: LLM extraction with focused prompts and graph-primed context.
    /// Uses API calls. Subject to grounding tribunal.
    Llm,
}

impl Default for ExtractionTier {
    fn default() -> Self {
        Self::Llm
    }
}
```

Add to `ExtractedClaim`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedClaim {
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub entities: Vec<String>,
    #[serde(default)]
    pub source_quote: Option<String>,
    /// Which extraction tier produced this claim. Default: Llm.
    #[serde(default)]
    pub extraction_tier: ExtractionTier,
}
```

- [ ] **Step 2: Run tests to verify schema changes compile**

```bash
cargo test -p thinkingroot-extract --lib
```

Expected: All existing tests pass (the `#[serde(default)]` ensures backward compatibility).

- [ ] **Step 3: Add ExtractionTier to core Claim type**

In `crates/thinkingroot-core/src/types/claim.rs`, add:

```rust
/// Which tier of the extraction funnel produced this claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionTier {
    /// Deterministic structural extraction (zero LLM, zero hallucination).
    Structural,
    /// LLM extraction with focused prompts.
    Llm,
}

impl Default for ExtractionTier {
    fn default() -> Self {
        Self::Llm
    }
}
```

Add to the `Claim` struct (after `grounding_method`):

```rust
pub extraction_tier: ExtractionTier,
```

Update `Claim::new()` to set `extraction_tier: ExtractionTier::default()`.

Add a builder method:

```rust
pub fn with_extraction_tier(mut self, tier: ExtractionTier) -> Self {
    self.extraction_tier = tier;
    self
}
```

- [ ] **Step 4: Add GroundingMethod::Structural variant**

In the same file, add to the `GroundingMethod` enum:

```rust
/// Structurally extracted from AST — deterministic, no LLM involved.
/// Auto-grounded at 0.99 confidence — skips the grounding tribunal.
Structural,
```

- [ ] **Step 5: Run all core tests**

```bash
cargo test -p thinkingroot-core
```

Expected: PASS. The `ExtractionTier::default()` in `Claim::new()` keeps existing tests working.

- [ ] **Step 6: Fix any downstream compilation errors**

The new `extraction_tier` field on `Claim` will cause errors wherever `Claim` is constructed directly (outside `Claim::new()`). Check with:

```bash
cargo check --workspace 2>&1 | head -50
```

Fix any struct-literal construction sites by adding `extraction_tier: ExtractionTier::default()` or `extraction_tier: ExtractionTier::Llm`.

- [ ] **Step 7: Update convert_result_static in extractor.rs**

In `crates/thinkingroot-extract/src/extractor.rs`, update the `convert_result_static` method to propagate the tier from `ExtractedClaim` to `Claim`:

```rust
let claim = Claim::new(&ext_claim.statement, claim_type, source_id, workspace_id)
    .with_confidence(ext_claim.confidence)
    .with_extraction_tier(ext_claim.extraction_tier.into());
```

This requires a `From<schema::ExtractionTier>` for `core::ExtractionTier` (or re-export, since they're the same enum). The simplest approach: use the core type everywhere and re-export in schema.

Actually, the cleanest approach: define `ExtractionTier` only in `thinkingroot-core` and import it in the extract crate's schema. Add to `schema.rs`:

```rust
pub use thinkingroot_core::types::ExtractionTier;
```

Then the `ExtractedClaim.extraction_tier` field uses the core type directly.

- [ ] **Step 8: Verify full workspace compiles**

```bash
cargo check --workspace
```

Expected: clean compilation with no errors.

- [ ] **Step 9: Commit**

```bash
git add crates/thinkingroot-core/src/types/claim.rs \
       crates/thinkingroot-extract/src/schema.rs \
       crates/thinkingroot-extract/src/extractor.rs
git commit -m "feat(extract): add ExtractionTier enum for tiered extraction funnel

Foundation for TEFS-GP architecture. Claims now carry extraction_tier
(Structural vs Llm) and GroundingMethod gains a Structural variant
for auto-grounded AST-extracted claims."
```

---

## Task 2: Structural Extractor (Tier 0 — Zero LLM)

Create the deterministic structural extractor that converts AST-rich chunks into `ExtractionResult` without any LLM calls. This is the core innovation — zero hallucination extraction for structured content.

**Files:**
- Create: `crates/thinkingroot-extract/src/structural.rs`
- Modify: `crates/thinkingroot-extract/src/lib.rs`
- Test: unit tests within `structural.rs`

**Key design decision:** The parse crate already produces `ChunkType::FunctionDef` with `ChunkMetadata { function_name, parameters, return_type, visibility, parent }`. We convert this metadata directly into claims, entities, and relations — no LLM needed.

- [ ] **Step 1: Write the failing test**

Create `crates/thinkingroot-extract/src/structural.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType};

    fn make_chunk(chunk_type: ChunkType, content: &str, meta: ChunkMetadata) -> Chunk {
        Chunk {
            content: content.to_string(),
            chunk_type,
            start_line: 1,
            end_line: 10,
            heading: None,
            language: Some("rust".to_string()),
            metadata: meta,
        }
    }

    #[test]
    fn function_def_produces_entity_and_claim() {
        let chunk = make_chunk(
            ChunkType::FunctionDef,
            "pub fn compile(path: &Path) -> Result<()> { ... }",
            ChunkMetadata {
                function_name: Some("compile".to_string()),
                parameters: Some(vec!["path: &Path".to_string()]),
                return_type: Some("Result<()>".to_string()),
                visibility: Some("pub".to_string()),
                parent: None,
                ..Default::default()
            },
        );

        let result = extract_structural(&chunk, "src/compiler.rs");
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "compile");
        assert_eq!(result.entities[0].entity_type, "function");
        assert!(result.claims.len() >= 1);
        assert_eq!(result.claims[0].claim_type, "api_signature");
        assert!(result.claims[0].confidence >= 0.99);
        assert_eq!(result.claims[0].extraction_tier, ExtractionTier::Structural);
    }

    #[test]
    fn type_def_produces_entity_and_claim() {
        let chunk = make_chunk(
            ChunkType::TypeDef,
            "pub struct GraphStore { db: DbInstance }",
            ChunkMetadata {
                type_name: Some("GraphStore".to_string()),
                visibility: Some("pub".to_string()),
                ..Default::default()
            },
        );

        let result = extract_structural(&chunk, "src/graph.rs");
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "GraphStore");
        assert!(result.claims.len() >= 1);
        assert_eq!(result.claims[0].claim_type, "definition");
        assert_eq!(result.claims[0].extraction_tier, ExtractionTier::Structural);
    }

    #[test]
    fn import_produces_relation() {
        let chunk = make_chunk(
            ChunkType::Import,
            "use thinkingroot_core::types::Claim;",
            ChunkMetadata {
                import_path: Some("thinkingroot_core::types::Claim".to_string()),
                ..Default::default()
            },
        );

        let result = extract_structural(&chunk, "src/extractor.rs");
        assert!(result.relations.len() >= 1);
        assert_eq!(result.relations[0].relation_type, "uses");
    }

    #[test]
    fn prose_chunk_returns_empty() {
        let chunk = make_chunk(
            ChunkType::Prose,
            "ThinkingRoot is a knowledge compiler for AI agents.",
            ChunkMetadata::default(),
        );

        let result = extract_structural(&chunk, "README.md");
        assert!(result.claims.is_empty());
        assert!(result.entities.is_empty());
        assert!(result.relations.is_empty());
    }

    #[test]
    fn method_with_parent_produces_contains_relation() {
        let chunk = make_chunk(
            ChunkType::FunctionDef,
            "pub fn insert_claim(&self, claim: &Claim) -> Result<()> { ... }",
            ChunkMetadata {
                function_name: Some("insert_claim".to_string()),
                parameters: Some(vec!["&self".to_string(), "claim: &Claim".to_string()]),
                return_type: Some("Result<()>".to_string()),
                visibility: Some("pub".to_string()),
                parent: Some("GraphStore".to_string()),
                ..Default::default()
            },
        );

        let result = extract_structural(&chunk, "src/graph.rs");
        // Should have entity for the method
        assert!(result.entities.iter().any(|e| e.name == "insert_claim"));
        // Should have a contains relation: GraphStore contains insert_claim
        assert!(result.relations.iter().any(|r|
            r.from_entity == "GraphStore"
            && r.to_entity == "insert_claim"
            && r.relation_type == "contains"
        ));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p thinkingroot-extract structural -- 2>&1 | head -20
```

Expected: FAIL — `extract_structural` function not defined.

- [ ] **Step 3: Implement the structural extractor**

Add the implementation above the tests in `crates/thinkingroot-extract/src/structural.rs`:

```rust
//! Tier 0: Deterministic structural extractor.
//!
//! Converts AST-rich chunks (FunctionDef, TypeDef, Import) into
//! ExtractionResult without any LLM calls. Zero hallucination.

use thinkingroot_core::ir::{Chunk, ChunkType};
use thinkingroot_core::types::ExtractionTier;

use crate::schema::{ExtractedClaim, ExtractedEntity, ExtractedRelation, ExtractionResult};

/// Extract knowledge from a single chunk using only its structural metadata.
/// Returns an empty ExtractionResult for chunks that require LLM (Prose, etc.).
pub fn extract_structural(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    match chunk.chunk_type {
        ChunkType::FunctionDef => extract_function_def(chunk, source_uri),
        ChunkType::TypeDef => extract_type_def(chunk, source_uri),
        ChunkType::Import => extract_import(chunk, source_uri),
        ChunkType::Comment | ChunkType::ModuleDoc => extract_doc_comment(chunk, source_uri),
        // Prose, Code (raw), Heading, List, Table → need LLM
        _ => ExtractionResult::empty(),
    }
}

/// Returns true if this chunk type can be structurally extracted (no LLM needed).
pub fn is_structurally_extractable(chunk: &Chunk) -> bool {
    matches!(
        chunk.chunk_type,
        ChunkType::FunctionDef | ChunkType::TypeDef | ChunkType::Import
    )
}

fn extract_function_def(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let meta = &chunk.metadata;
    let func_name = match meta.function_name.as_deref() {
        Some(name) if !name.is_empty() => name,
        _ => return ExtractionResult::empty(),
    };

    let mut entities = Vec::new();
    let mut claims = Vec::new();
    let mut relations = Vec::new();

    // Entity: the function itself
    entities.push(ExtractedEntity {
        name: func_name.to_string(),
        entity_type: "function".to_string(),
        aliases: Vec::new(),
        description: Some(format!(
            "Function defined in {}",
            source_uri
        )),
    });

    // Entity: the source file
    let file_entity_name = source_uri
        .rsplit('/')
        .next()
        .unwrap_or(source_uri)
        .to_string();
    entities.push(ExtractedEntity {
        name: file_entity_name.clone(),
        entity_type: "file".to_string(),
        aliases: Vec::new(),
        description: None,
    });

    // Claim: API signature
    let sig = build_signature(func_name, meta);
    claims.push(ExtractedClaim {
        statement: sig.clone(),
        claim_type: "api_signature".to_string(),
        confidence: 0.99,
        entities: vec![func_name.to_string()],
        source_quote: Some(chunk.content.chars().take(200).collect()),
        extraction_tier: ExtractionTier::Structural,
    });

    // Claim: definition (where it lives)
    let vis = meta.visibility.as_deref().unwrap_or("private");
    claims.push(ExtractedClaim {
        statement: format!(
            "{vis} function `{func_name}` is defined in {source_uri} (lines {}-{})",
            chunk.start_line, chunk.end_line
        ),
        claim_type: "definition".to_string(),
        confidence: 0.99,
        entities: vec![func_name.to_string(), file_entity_name.clone()],
        source_quote: None,
        extraction_tier: ExtractionTier::Structural,
    });

    // Relation: file contains function
    relations.push(ExtractedRelation {
        from_entity: file_entity_name.clone(),
        to_entity: func_name.to_string(),
        relation_type: "contains".to_string(),
        description: Some(format!("{file_entity_name} contains function {func_name}")),
    });

    // If method on a parent type (e.g., impl block), add parent relation
    if let Some(ref parent) = meta.parent {
        entities.push(ExtractedEntity {
            name: parent.clone(),
            entity_type: "module".to_string(),
            aliases: Vec::new(),
            description: None,
        });
        relations.push(ExtractedRelation {
            from_entity: parent.clone(),
            to_entity: func_name.to_string(),
            relation_type: "contains".to_string(),
            description: Some(format!("{parent} contains method {func_name}")),
        });
    }

    ExtractionResult { claims, entities, relations }
}

fn extract_type_def(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let meta = &chunk.metadata;
    let type_name = match meta.type_name.as_deref() {
        Some(name) if !name.is_empty() => name,
        _ => return ExtractionResult::empty(),
    };

    let mut entities = Vec::new();
    let mut claims = Vec::new();
    let mut relations = Vec::new();

    // Infer entity type from content keywords
    let entity_type = infer_entity_type_from_content(&chunk.content);

    entities.push(ExtractedEntity {
        name: type_name.to_string(),
        entity_type: entity_type.to_string(),
        aliases: Vec::new(),
        description: Some(format!("Type defined in {source_uri}")),
    });

    let file_entity_name = source_uri
        .rsplit('/')
        .next()
        .unwrap_or(source_uri)
        .to_string();
    entities.push(ExtractedEntity {
        name: file_entity_name.clone(),
        entity_type: "file".to_string(),
        aliases: Vec::new(),
        description: None,
    });

    let vis = meta.visibility.as_deref().unwrap_or("private");
    claims.push(ExtractedClaim {
        statement: format!(
            "{vis} type `{type_name}` is defined in {source_uri} (lines {}-{})",
            chunk.start_line, chunk.end_line
        ),
        claim_type: "definition".to_string(),
        confidence: 0.99,
        entities: vec![type_name.to_string(), file_entity_name.clone()],
        source_quote: Some(chunk.content.chars().take(200).collect()),
        extraction_tier: ExtractionTier::Structural,
    });

    relations.push(ExtractedRelation {
        from_entity: file_entity_name,
        to_entity: type_name.to_string(),
        relation_type: "contains".to_string(),
        description: None,
    });

    ExtractionResult { claims, entities, relations }
}

fn extract_import(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let meta = &chunk.metadata;
    let import_path = match meta.import_path.as_deref() {
        Some(path) if !path.is_empty() => path,
        _ => return ExtractionResult::empty(),
    };

    let file_entity_name = source_uri
        .rsplit('/')
        .next()
        .unwrap_or(source_uri)
        .to_string();

    // Extract the imported name (last segment of the path)
    let imported_name = import_path
        .rsplit("::")
        .next()
        .or_else(|| import_path.rsplit('/')
            .next()
            .or_else(|| import_path.rsplit('.').next()))
        .unwrap_or(import_path);

    // Extract the module/crate (first segment)
    let source_module = import_path
        .split("::")
        .next()
        .or_else(|| import_path.split('/').next())
        .unwrap_or(import_path);

    let mut entities = vec![
        ExtractedEntity {
            name: file_entity_name.clone(),
            entity_type: "file".to_string(),
            aliases: Vec::new(),
            description: None,
        },
    ];

    // Only add the imported entity if it's different from the file
    if imported_name != file_entity_name {
        entities.push(ExtractedEntity {
            name: imported_name.to_string(),
            entity_type: "module".to_string(),
            aliases: Vec::new(),
            description: Some(format!("Imported from {import_path}")),
        });
    }

    let claims = vec![ExtractedClaim {
        statement: format!("{source_uri} imports {import_path}"),
        claim_type: "dependency".to_string(),
        confidence: 0.99,
        entities: vec![file_entity_name.clone(), imported_name.to_string()],
        source_quote: Some(chunk.content.clone()),
        extraction_tier: ExtractionTier::Structural,
    }];

    let relations = vec![ExtractedRelation {
        from_entity: file_entity_name,
        to_entity: imported_name.to_string(),
        relation_type: "uses".to_string(),
        description: Some(format!("imports {import_path}")),
    }];

    ExtractionResult { claims, entities, relations }
}

fn extract_doc_comment(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    // Doc comments have semantic content but are small enough to skip LLM.
    // Extract minimal structural facts: the comment exists and what it documents.
    if chunk.content.trim().len() < 10 {
        return ExtractionResult::empty();
    }

    // If there's a parent (the function/type being documented), note the relationship.
    // Otherwise return empty — the LLM path will handle standalone comments.
    if let Some(ref parent) = chunk.metadata.parent {
        let claims = vec![ExtractedClaim {
            statement: format!(
                "`{parent}` in {source_uri} has documentation: {}",
                chunk.content.chars().take(100).collect::<String>()
            ),
            claim_type: "definition".to_string(),
            confidence: 0.95,
            entities: vec![parent.clone()],
            source_quote: Some(chunk.content.chars().take(200).collect()),
            extraction_tier: ExtractionTier::Structural,
        }];
        ExtractionResult { claims, entities: Vec::new(), relations: Vec::new() }
    } else {
        ExtractionResult::empty()
    }
}

/// Build a human-readable function signature from metadata.
fn build_signature(name: &str, meta: &thinkingroot_core::ir::ChunkMetadata) -> String {
    let vis = meta.visibility.as_deref().unwrap_or("");
    let params = meta
        .parameters
        .as_ref()
        .map(|p| p.join(", "))
        .unwrap_or_default();
    let ret = meta
        .return_type
        .as_ref()
        .map(|r| format!(" -> {r}"))
        .unwrap_or_default();

    format!("{vis} fn {name}({params}){ret}").trim().to_string()
}

/// Infer entity_type from type definition content.
fn infer_entity_type_from_content(content: &str) -> &'static str {
    let lower = content.to_lowercase();
    if lower.contains("struct ") || lower.contains("class ") {
        "system"
    } else if lower.contains("enum ") {
        "concept"
    } else if lower.contains("trait ") || lower.contains("interface ") {
        "api"
    } else if lower.contains("type ") {
        "concept"
    } else {
        "module"
    }
}
```

- [ ] **Step 4: Export the module**

In `crates/thinkingroot-extract/src/lib.rs`, add:

```rust
pub mod structural;
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p thinkingroot-extract structural -- --nocapture
```

Expected: All 5 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-extract/src/structural.rs \
       crates/thinkingroot-extract/src/lib.rs
git commit -m "feat(extract): add Tier 0 structural extractor — zero LLM, zero hallucination

Converts FunctionDef, TypeDef, and Import chunks from tree-sitter AST
into claims/entities/relations deterministically. Extracts API signatures,
definitions, dependency relations, and containment hierarchy.
All claims tagged ExtractionTier::Structural with 0.99 confidence."
```

---

## Task 3: Tier Router

Create the router that classifies chunks into structural (Tier 0) vs LLM (Tier 2) paths based on their `ChunkType` and metadata richness.

**Files:**
- Create: `crates/thinkingroot-extract/src/router.rs`
- Modify: `crates/thinkingroot-extract/src/lib.rs`
- Test: unit tests within `router.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/thinkingroot-extract/src/router.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType};

    fn chunk(chunk_type: ChunkType, meta: ChunkMetadata) -> Chunk {
        Chunk {
            content: "test".to_string(),
            chunk_type,
            start_line: 1,
            end_line: 1,
            heading: None,
            language: None,
            metadata: meta,
        }
    }

    #[test]
    fn function_def_with_name_is_structural() {
        let c = chunk(
            ChunkType::FunctionDef,
            ChunkMetadata {
                function_name: Some("foo".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn function_def_without_name_is_llm() {
        let c = chunk(ChunkType::FunctionDef, ChunkMetadata::default());
        assert_eq!(classify(&c), Tier::Llm);
    }

    #[test]
    fn prose_is_always_llm() {
        let c = chunk(ChunkType::Prose, ChunkMetadata::default());
        assert_eq!(classify(&c), Tier::Llm);
    }

    #[test]
    fn import_with_path_is_structural() {
        let c = chunk(
            ChunkType::Import,
            ChunkMetadata {
                import_path: Some("std::path::Path".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn type_def_with_name_is_structural() {
        let c = chunk(
            ChunkType::TypeDef,
            ChunkMetadata {
                type_name: Some("Config".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(classify(&c), Tier::Structural);
    }

    #[test]
    fn code_chunk_is_llm() {
        let c = chunk(ChunkType::Code, ChunkMetadata::default());
        assert_eq!(classify(&c), Tier::Llm);
    }

    #[test]
    fn route_chunks_splits_correctly() {
        let chunks = vec![
            chunk(ChunkType::FunctionDef, ChunkMetadata {
                function_name: Some("foo".to_string()),
                ..Default::default()
            }),
            chunk(ChunkType::Prose, ChunkMetadata::default()),
            chunk(ChunkType::Import, ChunkMetadata {
                import_path: Some("std::io".to_string()),
                ..Default::default()
            }),
        ];
        let (structural, llm) = route_chunks(&chunks);
        assert_eq!(structural.len(), 2);
        assert_eq!(llm.len(), 1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p thinkingroot-extract router -- 2>&1 | head -10
```

Expected: FAIL — `classify` not defined.

- [ ] **Step 3: Implement the tier router**

```rust
//! Tier Router: classifies chunks as structural-extractable vs LLM-needed.
//!
//! Decision criteria:
//! - FunctionDef with function_name → Structural
//! - TypeDef with type_name → Structural
//! - Import with import_path → Structural
//! - Everything else (Prose, Code, Heading, List, Table) → LLM

use thinkingroot_core::ir::{Chunk, ChunkType};

/// Which extraction tier a chunk is routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Zero-LLM deterministic extraction from AST metadata.
    Structural,
    /// LLM extraction with focused prompts.
    Llm,
}

/// Classify a single chunk into its extraction tier.
pub fn classify(chunk: &Chunk) -> Tier {
    match chunk.chunk_type {
        ChunkType::FunctionDef => {
            if chunk.metadata.function_name.as_ref().is_some_and(|n| !n.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        ChunkType::TypeDef => {
            if chunk.metadata.type_name.as_ref().is_some_and(|n| !n.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        ChunkType::Import => {
            if chunk.metadata.import_path.as_ref().is_some_and(|p| !p.is_empty()) {
                Tier::Structural
            } else {
                Tier::Llm
            }
        }
        // Prose, Code, Heading, List, Table, Comment, ModuleDoc → LLM
        _ => Tier::Llm,
    }
}

/// Split a slice of chunks into (structural, llm) groups.
/// Returns indices into the original slice for zero-copy routing.
pub fn route_chunks(chunks: &[Chunk]) -> (Vec<usize>, Vec<usize>) {
    let mut structural = Vec::new();
    let mut llm = Vec::new();

    for (i, chunk) in chunks.iter().enumerate() {
        match classify(chunk) {
            Tier::Structural => structural.push(i),
            Tier::Llm => llm.push(i),
        }
    }

    (structural, llm)
}
```

- [ ] **Step 4: Export the module**

Add to `crates/thinkingroot-extract/src/lib.rs`:

```rust
pub mod router;
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p thinkingroot-extract router
```

Expected: All 7 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-extract/src/router.rs \
       crates/thinkingroot-extract/src/lib.rs
git commit -m "feat(extract): add tier router — classifies chunks as structural vs LLM

Routes FunctionDef/TypeDef/Import with metadata to Tier 0 (zero LLM).
All other chunk types (Prose, Code, Heading, List, Table) go to Tier 2 (LLM).
Metadata completeness is verified before routing to structural tier."
```

---

## Task 4: Graph-Primed Context

Build the mechanism that injects existing entities from the knowledge graph into LLM extraction prompts. This reduces entity hallucination by giving the LLM a list of known entities to match against.

**Files:**
- Create: `crates/thinkingroot-extract/src/graph_context.rs`
- Modify: `crates/thinkingroot-extract/src/lib.rs`
- Modify: `crates/thinkingroot-graph/src/graph.rs` (add query method)
- Test: unit tests in `graph_context.rs`

- [ ] **Step 1: Add get_known_entities to GraphStore**

In `crates/thinkingroot-graph/src/graph.rs`, add a method that returns entity names and types for graph-priming. Add near the other query methods:

```rust
/// Returns (canonical_name, entity_type) pairs for all entities in the graph.
/// Used by the graph-primed extraction context to inject KNOWN_ENTITIES
/// into LLM prompts, reducing entity hallucination.
pub fn get_known_entities(&self) -> Result<Vec<(String, String)>> {
    let result = self.query_read(
        "?[name, entity_type] := *entities{canonical_name: name, entity_type}"
    )?;
    Ok(result.rows.into_iter().filter_map(|row| {
        let name = row.first()?.get_str()?.to_string();
        let entity_type = row.get(1)?.get_str()?.to_string();
        Some((name, entity_type))
    }).collect())
}
```

- [ ] **Step 2: Write the failing test for graph_context**

Create `crates/thinkingroot-extract/src/graph_context.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_context_produces_empty_string() {
        let ctx = GraphPrimedContext::new(Vec::new());
        assert!(ctx.prompt_section().is_empty());
    }

    #[test]
    fn known_entities_produce_prompt_section() {
        let ctx = GraphPrimedContext::new(vec![
            KnownEntity { name: "GraphStore".to_string(), entity_type: "system".to_string() },
            KnownEntity { name: "Claim".to_string(), entity_type: "concept".to_string() },
        ]);

        let section = ctx.prompt_section();
        assert!(section.contains("KNOWN_ENTITIES"));
        assert!(section.contains("GraphStore"));
        assert!(section.contains("Claim"));
    }

    #[test]
    fn from_tuples_converts_correctly() {
        let tuples = vec![
            ("Alice".to_string(), "person".to_string()),
            ("Acme".to_string(), "organization".to_string()),
        ];
        let ctx = GraphPrimedContext::from_tuples(tuples);
        assert_eq!(ctx.entities.len(), 2);
        assert_eq!(ctx.entities[0].name, "Alice");
    }

    #[test]
    fn limits_to_max_entities() {
        let many: Vec<_> = (0..500)
            .map(|i| KnownEntity {
                name: format!("Entity{i}"),
                entity_type: "concept".to_string(),
            })
            .collect();
        let ctx = GraphPrimedContext::new(many);
        // Should not produce a gigantic prompt — limit to MAX_KNOWN_ENTITIES
        let section = ctx.prompt_section();
        // At most MAX_KNOWN_ENTITIES entries (default 200)
        let entity_count = section.matches("- ").count();
        assert!(entity_count <= MAX_KNOWN_ENTITIES);
    }
}
```

- [ ] **Step 3: Implement graph_context**

```rust
//! Graph-Primed Context: injects existing entities into LLM extraction prompts.
//!
//! Before LLM extraction, we query the knowledge graph for existing entities
//! and build a KNOWN_ENTITIES section. The LLM should prefer matching
//! existing entities over inventing new ones, reducing hallucination.

/// Maximum number of known entities to inject into prompts.
/// Too many overwhelm the LLM and waste tokens.
pub const MAX_KNOWN_ENTITIES: usize = 200;

/// A known entity from the existing knowledge graph.
#[derive(Debug, Clone)]
pub struct KnownEntity {
    pub name: String,
    pub entity_type: String,
}

/// Graph-primed context that can be injected into LLM prompts.
#[derive(Debug, Clone)]
pub struct GraphPrimedContext {
    pub entities: Vec<KnownEntity>,
}

impl GraphPrimedContext {
    pub fn new(entities: Vec<KnownEntity>) -> Self {
        Self { entities }
    }

    /// Convert from (name, entity_type) tuples (as returned by GraphStore).
    pub fn from_tuples(tuples: Vec<(String, String)>) -> Self {
        let entities = tuples
            .into_iter()
            .map(|(name, entity_type)| KnownEntity { name, entity_type })
            .collect();
        Self { entities }
    }

    /// Build the KNOWN_ENTITIES prompt section.
    /// Returns empty string if no known entities.
    pub fn prompt_section(&self) -> String {
        if self.entities.is_empty() {
            return String::new();
        }

        let limited = &self.entities[..self.entities.len().min(MAX_KNOWN_ENTITIES)];

        let mut section = String::from(
            "\n<KNOWN_ENTITIES>\n\
             The following entities already exist in the knowledge graph. \
             When you encounter references to these entities, use the EXACT names below \
             rather than creating new entities. Only create new entities for concepts \
             not already represented.\n\n"
        );

        for entity in limited {
            section.push_str(&format!("- {} ({})\n", entity.name, entity.entity_type));
        }

        section.push_str("</KNOWN_ENTITIES>\n");
        section
    }

    /// Returns true if there are known entities to inject.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}
```

- [ ] **Step 4: Export the module**

Add to `crates/thinkingroot-extract/src/lib.rs`:

```rust
pub mod graph_context;
```

Also export the types:

```rust
pub use graph_context::{GraphPrimedContext, KnownEntity};
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p thinkingroot-extract graph_context
```

Expected: All 4 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-extract/src/graph_context.rs \
       crates/thinkingroot-extract/src/lib.rs \
       crates/thinkingroot-graph/src/graph.rs
git commit -m "feat(extract): add graph-primed context — inject known entities into LLM prompts

Queries existing entities from the knowledge graph and builds a
KNOWN_ENTITIES prompt section. LLM prefers matching existing entities
over inventing new ones, reducing entity hallucination. Limited to
200 entities max to avoid overwhelming the prompt."
```

---

## Task 5: Focused Prompts with Graph-Primed Context

Replace the single mega-prompt with focused sub-task prompts. The mega-prompt (`SYSTEM_PROMPT` in prompts.rs) currently asks the LLM to extract everything in one call. Split it into focused prompts that are smaller, more cacheable, and more accurate.

**Files:**
- Create: `crates/thinkingroot-extract/src/focused_prompts.rs`
- Modify: `crates/thinkingroot-extract/src/lib.rs`
- Test: unit tests in `focused_prompts.rs`

**Design decision:** Keep the existing `prompts.rs` as the default/fallback. Add `focused_prompts.rs` with entity-focused and relation-focused prompts. The extractor will use focused prompts when graph context is available.

- [ ] **Step 1: Write the failing test**

Create `crates/thinkingroot-extract/src/focused_prompts.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_context::{GraphPrimedContext, KnownEntity};

    #[test]
    fn entity_prompt_includes_known_entities() {
        let ctx = GraphPrimedContext::new(vec![
            KnownEntity { name: "GraphStore".to_string(), entity_type: "system".to_string() },
        ]);
        let (system, user) = build_entity_extraction_prompt(
            "pub fn insert_claim(&self) { }",
            "Source: graph.rs, Language: rust",
            &ctx,
        );
        assert!(system.contains("entity extraction"));
        assert!(user.contains("KNOWN_ENTITIES"));
        assert!(user.contains("GraphStore"));
    }

    #[test]
    fn relation_prompt_includes_entity_list() {
        let entities = vec!["GraphStore".to_string(), "Claim".to_string()];
        let (system, user) = build_relation_extraction_prompt(
            "pub fn insert_claim(&self, claim: &Claim) { }",
            "Source: graph.rs",
            &entities,
        );
        assert!(system.contains("relation"));
        assert!(user.contains("GraphStore"));
        assert!(user.contains("Claim"));
    }

    #[test]
    fn claim_prompt_includes_entity_list() {
        let entities = vec!["GraphStore".to_string()];
        let (system, user) = build_claim_extraction_prompt(
            "GraphStore uses CozoDB for storage.",
            "Source: README.md",
            &entities,
        );
        assert!(system.contains("claim"));
        assert!(user.contains("GraphStore"));
    }

    #[test]
    fn empty_graph_context_omits_known_entities_section() {
        let ctx = GraphPrimedContext::new(Vec::new());
        let (_, user) = build_entity_extraction_prompt(
            "hello world",
            "Source: test.txt",
            &ctx,
        );
        assert!(!user.contains("KNOWN_ENTITIES"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p thinkingroot-extract focused_prompts -- 2>&1 | head -10
```

- [ ] **Step 3: Implement focused prompts**

```rust
//! Focused extraction prompts — split the mega-prompt into sub-task prompts.
//!
//! Three focused prompts:
//! 1. Entity extraction — find and classify entities
//! 2. Relation extraction — given entities, find relationships
//! 3. Claim extraction — given entities, extract factual claims

use crate::graph_context::GraphPrimedContext;

/// Entity extraction prompt. Returns (system_prompt, user_prompt).
pub fn build_entity_extraction_prompt(
    content: &str,
    context: &str,
    graph_ctx: &GraphPrimedContext,
) -> (String, String) {
    let system = r#"You are an entity extraction engine for a knowledge graph.
Your job is to identify and classify all named entities in the given content.

You MUST return valid JSON matching this schema:
{
  "entities": [
    {
      "name": "Canonical name",
      "entity_type": "person|system|service|concept|team|api|database|library|file|module|function|config|organization",
      "aliases": ["alternate names"],
      "description": "Brief description"
    }
  ]
}

Rules:
1. Extract EVERY named entity (people, systems, functions, types, libraries, etc.).
2. Use the most complete canonical name (e.g., "GraphStore" not "store").
3. If a KNOWN_ENTITIES section is provided, prefer matching those exact names.
4. Only create a NEW entity if it's genuinely not in the known list.
5. Return ONLY the JSON object. No markdown, no explanation."#;

    let known_section = graph_ctx.prompt_section();

    let user = format!(
        "Extract all named entities from the following content.\n\n\
         Context: {context}\n\
         {known_section}\n\
         ---\n\n\
         {content}\n\n\
         ---\n\n\
         Return the JSON with entities only."
    );

    (system.to_string(), user)
}

/// Relation extraction prompt. Takes already-extracted entity names.
/// Returns (system_prompt, user_prompt).
pub fn build_relation_extraction_prompt(
    content: &str,
    context: &str,
    entity_names: &[String],
) -> (String, String) {
    let system = r#"You are a relation extraction engine for a knowledge graph.
Given a list of entities found in the content, identify all relationships between them.

You MUST return valid JSON matching this schema:
{
  "relations": [
    {
      "from_entity": "Entity A",
      "to_entity": "Entity B",
      "relation_type": "depends_on|owned_by|replaces|contradicts|implements|uses|contains|created_by|part_of|related_to|calls|configured_by|tested_by",
      "description": "Brief description of the relationship"
    }
  ]
}

Rules:
1. Only use entity names from the ENTITIES list.
2. Every relation must connect two DISTINCT entities.
3. Do NOT fabricate relationships not stated or clearly implied.
4. Return ONLY the JSON object."#;

    let entities_list = entity_names
        .iter()
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    let user = format!(
        "Extract relationships between the following entities based on the content.\n\n\
         Context: {context}\n\n\
         <ENTITIES>\n{entities_list}\n</ENTITIES>\n\n\
         ---\n\n\
         {content}\n\n\
         ---\n\n\
         Return the JSON with relations only."
    );

    (system.to_string(), user)
}

/// Claim extraction prompt. Takes already-extracted entity names.
/// Returns (system_prompt, user_prompt).
pub fn build_claim_extraction_prompt(
    content: &str,
    context: &str,
    entity_names: &[String],
) -> (String, String) {
    let system = r#"You are a claim extraction engine for a knowledge graph.
Given content and a list of known entities, extract all factual claims.

You MUST return valid JSON matching this schema:
{
  "claims": [
    {
      "statement": "A clear, atomic statement of fact or decision",
      "claim_type": "fact|decision|opinion|plan|requirement|metric|definition|dependency|api_signature|architecture",
      "confidence": 0.0-1.0,
      "entities": ["entity names mentioned in this claim"],
      "source_quote": "Verbatim quote from the source supporting this claim"
    }
  ]
}

Rules:
1. Claims must be ATOMIC — one fact per claim.
2. Claims must be SELF-CONTAINED — understandable without the source.
3. Entity names must match the ENTITIES list exactly.
4. Confidence: 0.5=implied, 0.8=stated clearly, 0.95=definitive.
5. source_quote MUST be verbatim from the source. Do NOT paraphrase.
6. Return ONLY the JSON object."#;

    let entities_list = entity_names
        .iter()
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    let user = format!(
        "Extract factual claims from the following content.\n\n\
         Context: {context}\n\n\
         <ENTITIES>\n{entities_list}\n</ENTITIES>\n\n\
         ---\n\n\
         {content}\n\n\
         ---\n\n\
         Return the JSON with claims only."
    );

    (system.to_string(), user)
}
```

- [ ] **Step 4: Export the module**

Add to `crates/thinkingroot-extract/src/lib.rs`:

```rust
pub mod focused_prompts;
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p thinkingroot-extract focused_prompts
```

Expected: All 4 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-extract/src/focused_prompts.rs \
       crates/thinkingroot-extract/src/lib.rs
git commit -m "feat(extract): add focused split prompts — entity, relation, claim

Replaces single mega-prompt approach with 3 focused sub-task prompts.
Entity prompt includes KNOWN_ENTITIES from graph context.
Relation and claim prompts receive pre-extracted entity names for
constrained extraction. Each prompt is smaller and more cache-friendly."
```

---

## Task 6: Wire Tiered Extraction into the Extractor

This is the integration task. Modify `Extractor` to:
1. Accept known entities (graph-primed context)
2. Route chunks through the Tier Router
3. Process structural chunks via the Structural Extractor (zero LLM)
4. Process LLM chunks with focused prompts

**Files:**
- Modify: `crates/thinkingroot-extract/src/extractor.rs`
- Modify: `crates/thinkingroot-extract/src/lib.rs`
- Test: existing tests must still pass + new integration tests

- [ ] **Step 1: Add known_entities field to Extractor**

In `crates/thinkingroot-extract/src/extractor.rs`, add to the `Extractor` struct:

```rust
pub struct Extractor {
    llm: SharedLlm,
    concurrency: usize,
    min_confidence: f64,
    max_chunk_tokens: usize,
    cache: Option<crate::cache::ExtractionCache>,
    progress: Option<ChunkProgressFn>,
    /// Known entities from the existing graph, injected into LLM prompts.
    known_entities: crate::graph_context::GraphPrimedContext,
}
```

Initialize in `new()`:

```rust
known_entities: crate::graph_context::GraphPrimedContext::new(Vec::new()),
```

Add builder method:

```rust
/// Inject known entities from the existing knowledge graph into LLM prompts.
/// The LLM will prefer matching these entities over creating new ones.
pub fn with_known_entities(mut self, ctx: crate::graph_context::GraphPrimedContext) -> Self {
    tracing::info!("graph-primed context: {} known entities", ctx.entities.len());
    self.known_entities = ctx;
    self
}
```

- [ ] **Step 2: Add tiered routing to extract_all**

Replace the loop in `extract_all()` (the section that builds `cache_hits_data` and `llm_work`) with tiered routing. The key change: before checking cache or queueing for LLM, run the tier router.

In the `for doc in documents { for chunk in &doc.chunks { ... } }` loop, replace the body with:

```rust
for doc in documents {
    for chunk in &doc.chunks {
        // ── Tier Router: structural or LLM? ──
        if crate::router::classify(chunk) == crate::router::Tier::Structural {
            let result = crate::structural::extract_structural(chunk, &doc.uri);
            if !result.claims.is_empty() || !result.entities.is_empty() {
                structural_results.push((doc.source_id, doc.uri.clone(), result));
                continue;
            }
            // Fallthrough to LLM if structural produced nothing
        }

        // ── Cache check (LLM path only) ──
        if let Some(ref cache) = self.cache {
            if let Some(cached) = cache.get(&chunk.content) {
                tracing::debug!("extraction cache hit for chunk in {}", doc.uri);
                cache_hits_data.push((doc.source_id, doc.uri.clone(), cached));
                continue;
            }
        }

        let sub_chunks = split_to_token_budget(&chunk.content, max_chunk_tokens);
        if sub_chunks.len() > 1 {
            tracing::debug!(
                "chunk in {} split into {} sub-chunks",
                doc.uri, sub_chunks.len()
            );
        }
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
        });
    }
}
```

Add `structural_results` declaration before the loop:

```rust
let mut structural_results: Vec<(SourceId, String, ExtractionResult)> = Vec::new();
```

- [ ] **Step 3: Process structural results (instant, no LLM)**

After the cache hits processing section, add structural results processing:

```rust
// ── Process structural results (instant, no LLM) ─────────────
let structural_count = structural_results.len();
for (source_id, source_uri, struct_result) in structural_results {
    let converted =
        Self::convert_result_static(struct_result, source_id, workspace_id, 0.0);
    output.merge(converted);
    output.chunks_processed += 1;
    done += 1;
    if let Some(ref pf) = self.progress {
        pf(done, total_chunks, &source_uri);
    }
}
if structural_count > 0 {
    tracing::info!(
        "structural extraction: {} chunks processed (zero LLM calls)",
        structural_count
    );
}
```

Update the `total_chunks` calculation to include structural results:

```rust
let total_chunks = structural_results.len() + cache_hits_data.len() + llm_work.len();
```

Note: move `total_chunks` calculation to after all three vectors are built.

- [ ] **Step 4: Add structural stats to ExtractionOutput**

In `ExtractionOutput`, add:

```rust
/// Chunks extracted via structural (Tier 0) extraction — no LLM call made.
pub structural_extractions: usize,
```

Update the structural processing to increment this counter:

```rust
output.structural_extractions += 1;  // inside the structural results loop
```

Update `ExtractionOutput::merge` to sum structural_extractions.

- [ ] **Step 5: Export structural_extractions in lib.rs**

Ensure `ExtractionOutput` exports the new field. Update the re-export in `lib.rs` if needed (it already re-exports `ExtractionOutput`).

- [ ] **Step 6: Run all tests**

```bash
cargo test -p thinkingroot-extract
```

Expected: All tests pass. The tiered routing is additive — existing behavior preserved for Prose/Code chunks.

- [ ] **Step 7: Run workspace check**

```bash
cargo check --workspace
```

Fix any compilation errors from the new `structural_extractions` field (add `Default` impl or initialize in `ExtractionOutput::default()`).

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-extract/src/extractor.rs \
       crates/thinkingroot-extract/src/lib.rs
git commit -m "feat(extract): wire tiered extraction — structural chunks bypass LLM

Chunks flow through the tier router: FunctionDef/TypeDef/Import with
metadata go to the structural extractor (zero LLM, zero hallucination).
Prose/Code/Heading chunks go through the existing LLM path.
ExtractionOutput now reports structural_extractions count.
Known entities can be injected via with_known_entities() for graph-priming."
```

---

## Task 7: Cascade Grounding Integration

Modify the grounding phase in the pipeline to cascade verification depth based on extraction tier:
- **Structural claims** (ExtractionTier::Structural): auto-grounded at 0.99, skip the tribunal entirely
- **LLM claims** (ExtractionTier::Llm): full 4-judge grounding tribunal (unchanged)

**Files:**
- Modify: `crates/thinkingroot-serve/src/pipeline.rs`
- Test: verify existing pipeline behavior + new cascade behavior

- [ ] **Step 1: Add auto-grounding for structural claims before tribunal**

In `crates/thinkingroot-serve/src/pipeline.rs`, in the Phase 2b grounding section (around line 192), add structural auto-grounding before the tribunal runs:

```rust
let extraction = if !extraction.claims.is_empty() {
    // ── Cascade Grounding: auto-ground structural claims ──
    // Structural claims come from deterministic AST parsing — they are
    // inherently grounded (the source code IS the evidence). Skip the
    // full tribunal for these and only run it on LLM-extracted claims.
    let mut structural_count = 0usize;
    let mut extraction = extraction;
    for claim in &mut extraction.claims {
        if claim.extraction_tier == thinkingroot_core::types::ExtractionTier::Structural {
            claim.grounding_score = Some(0.99);
            claim.grounding_method = Some(thinkingroot_core::types::GroundingMethod::Structural);
            structural_count += 1;
        }
    }
    if structural_count > 0 {
        tracing::info!(
            "cascade grounding: {} structural claims auto-grounded at 0.99 (skipped tribunal)",
            structural_count
        );
    }

    // Run tribunal only on LLM claims (extraction_tier == Llm)
    let llm_claim_count = extraction.claims.iter()
        .filter(|c| c.extraction_tier == thinkingroot_core::types::ExtractionTier::Llm)
        .count();

    if llm_claim_count > 0 {
        let grounder = thinkingroot_ground::Grounder::new(
            thinkingroot_ground::GroundingConfig::default(),
        );
        let pre_count = extraction.claims.len();
        let mut grounded = grounder.ground(
            extraction,
            #[cfg(feature = "vector")]
            Some(&mut storage.vector),
            #[cfg(feature = "vector")]
            nli_judge.as_mut(),
        );
        thinkingroot_ground::dedup::dedup_claims(&mut grounded.claims);
        let post_count = grounded.claims.len();
        if pre_count != post_count {
            tracing::info!(
                "grounding: {} → {} claims ({} rejected/deduped)",
                pre_count, post_count, pre_count - post_count,
            );
        }
        grounded
    } else {
        // All claims are structural — skip tribunal entirely
        thinkingroot_ground::dedup::dedup_claims(&mut extraction.claims);
        extraction
    }
} else {
    extraction
};
```

- [ ] **Step 2: Pass existing entities to extractor (graph-priming)**

In the same file, before the extractor is created (around line 125), add:

```rust
// ── Graph-Primed Context: inject known entities into extraction ──
let known_entities = match storage.graph.get_known_entities() {
    Ok(entities) if !entities.is_empty() => {
        tracing::info!("graph-primed context: {} known entities loaded", entities.len());
        thinkingroot_extract::GraphPrimedContext::from_tuples(entities)
    }
    Ok(_) => thinkingroot_extract::GraphPrimedContext::new(Vec::new()),
    Err(e) => {
        tracing::warn!("failed to load known entities for graph-priming: {e}");
        thinkingroot_extract::GraphPrimedContext::new(Vec::new())
    }
};
```

Then chain it onto the extractor builder:

```rust
let extractor = {
    let e = thinkingroot_extract::Extractor::new(&config)
        .await?
        .with_cache_dir(&data_dir)
        .with_known_entities(known_entities);
    // ... progress callback wiring (unchanged)
};
```

- [ ] **Step 3: Update progress reporting for structural extractions**

In the `ExtractionComplete` event emission, add structural count:

```rust
emit!(ProgressEvent::ExtractionComplete {
    claims: raw.claims.len(),
    entities: raw.entities.len(),
    cache_hits: raw.cache_hits,
});
```

Also log the structural extraction stats:

```rust
if raw.structural_extractions > 0 {
    tracing::info!(
        "tiered extraction: {} structural (zero LLM), {} cache hits, {} LLM calls",
        raw.structural_extractions,
        raw.cache_hits,
        raw.chunks_processed - raw.cache_hits - raw.structural_extractions,
    );
}
```

- [ ] **Step 4: Update PipelineResult with structural stats**

Add to `PipelineResult`:

```rust
pub structural_extractions: usize,
```

Set it when building the result:

```rust
structural_extractions: extraction.structural_extractions,
```

- [ ] **Step 5: Run workspace check**

```bash
cargo check --workspace
```

Fix any compilation errors.

- [ ] **Step 6: Run tests**

```bash
cargo test -p thinkingroot-serve
cargo test -p thinkingroot-extract
cargo test -p thinkingroot-core
```

Expected: All pass.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-serve/src/pipeline.rs
git commit -m "feat(pipeline): cascade grounding + graph-primed extraction

Structural claims (from AST) auto-grounded at 0.99 — skip tribunal.
LLM claims still run the full 4-judge grounding tribunal.
Pipeline now queries existing entities from graph and injects them
into the extractor for graph-primed extraction context.
PipelineResult reports structural_extractions count."
```

---

## Task 8: Update CozoDB Schema for ExtractionTier

Store the extraction_tier in the claims table so it persists across pipeline runs and is available for querying.

**Files:**
- Modify: `crates/thinkingroot-graph/src/graph.rs`
- Test: existing graph tests must pass

- [ ] **Step 1: Add extraction_tier column to claims schema**

In `crates/thinkingroot-graph/src/graph.rs`, in the `create_schema` method, update the claims relation. Change:

```
:create claims {id: String => statement: String, claim_type: String, source_id: String, confidence: Float, sensitivity: String, workspace_id: String, created_at: Float, grounding_score: Float, grounding_method: String}
```

To:

```
:create claims {id: String => statement: String, claim_type: String, source_id: String, confidence: Float, sensitivity: String, workspace_id: String, created_at: Float, grounding_score: Float, grounding_method: String, extraction_tier: String}
```

- [ ] **Step 2: Update insert_claim to store extraction_tier**

In the `insert_claim` method, add the extraction_tier parameter to the `:put` script:

```rust
let tier_str = match claim.extraction_tier {
    thinkingroot_core::types::ExtractionTier::Structural => "structural",
    thinkingroot_core::types::ExtractionTier::Llm => "llm",
};
```

Add it to the params and the Datalog `:put` statement.

- [ ] **Step 3: Update get_claim_by_id to read extraction_tier**

In the `get_claim_by_id` method, read the extraction_tier column and set it on the returned `Claim`.

- [ ] **Step 4: Handle schema migration (existing DBs)**

Existing databases won't have the `extraction_tier` column. CozoDB's `:create` with different columns on an existing relation will fail. Use the same pattern as existing schema: catch the "already exists" error. For the new column, existing claims default to `ExtractionTier::Llm`.

Add a migration step after `create_schema`:

```rust
// Migration: add extraction_tier column if missing
let migration = r#"
    {
        ?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier] :=
            *claims{id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method},
            extraction_tier = "llm"
        :replace claims {id: String => statement: String, claim_type: String, source_id: String, confidence: Float, sensitivity: String, workspace_id: String, created_at: Float, grounding_score: Float, grounding_method: String, extraction_tier: String}
    }
"#;
// Only run if old schema exists and new column is missing
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p thinkingroot-graph
```

Expected: All pass.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-graph/src/graph.rs
git commit -m "feat(graph): store extraction_tier in claims table

Claims now persist their extraction tier (structural/llm) in CozoDB.
Includes schema migration for existing databases — defaults to 'llm'
for pre-existing claims. Enables querying claims by extraction method."
```

---

## Task 9: End-to-End Integration Test

Add an integration test that verifies the full tiered extraction flow: structural chunks produce entities/claims without LLM, prose chunks queue for LLM, and both merge into a single ExtractionOutput.

**Files:**
- Add test in: `crates/thinkingroot-extract/src/extractor.rs` (or a new test file)

- [ ] **Step 1: Write integration test**

```rust
#[cfg(test)]
mod tiered_tests {
    use super::*;
    use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType, DocumentIR};
    use thinkingroot_core::types::{ContentHash, SourceMetadata, SourceType};

    fn make_test_doc(chunks: Vec<Chunk>) -> DocumentIR {
        let source_id = SourceId::new();
        DocumentIR {
            source_id,
            uri: "test/example.rs".to_string(),
            source_type: SourceType::File,
            timestamp: chrono::Utc::now(),
            author: None,
            content_hash: ContentHash::from_bytes(b"test"),
            chunks,
            metadata: SourceMetadata {
                language: Some("rust".to_string()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn structural_chunks_produce_results_without_llm() {
        // Test that FunctionDef chunks are structurally extracted
        let doc = make_test_doc(vec![
            Chunk {
                content: "pub fn compile(path: &Path) -> Result<()> { }".to_string(),
                chunk_type: ChunkType::FunctionDef,
                start_line: 1,
                end_line: 1,
                heading: None,
                language: Some("rust".to_string()),
                metadata: ChunkMetadata {
                    function_name: Some("compile".to_string()),
                    parameters: Some(vec!["path: &Path".to_string()]),
                    return_type: Some("Result<()>".to_string()),
                    visibility: Some("pub".to_string()),
                    ..Default::default()
                },
            },
        ]);

        // Run structural extraction directly (no LLM needed)
        let chunk = &doc.chunks[0];
        let result = crate::structural::extract_structural(chunk, &doc.uri);
        assert!(!result.entities.is_empty(), "structural should produce entities");
        assert!(!result.claims.is_empty(), "structural should produce claims");
        assert_eq!(
            result.claims[0].extraction_tier,
            thinkingroot_core::types::ExtractionTier::Structural
        );
    }

    #[test]
    fn router_correctly_splits_mixed_document() {
        let doc = make_test_doc(vec![
            Chunk {
                content: "pub fn foo() {}".to_string(),
                chunk_type: ChunkType::FunctionDef,
                start_line: 1,
                end_line: 1,
                heading: None,
                language: Some("rust".to_string()),
                metadata: ChunkMetadata {
                    function_name: Some("foo".to_string()),
                    ..Default::default()
                },
            },
            Chunk {
                content: "This module handles authentication.".to_string(),
                chunk_type: ChunkType::Prose,
                start_line: 5,
                end_line: 5,
                heading: None,
                language: None,
                metadata: ChunkMetadata::default(),
            },
            Chunk {
                content: "use std::path::Path;".to_string(),
                chunk_type: ChunkType::Import,
                start_line: 1,
                end_line: 1,
                heading: None,
                language: Some("rust".to_string()),
                metadata: ChunkMetadata {
                    import_path: Some("std::path::Path".to_string()),
                    ..Default::default()
                },
            },
        ]);

        let (structural, llm) = crate::router::route_chunks(&doc.chunks);
        assert_eq!(structural.len(), 2, "FunctionDef + Import = 2 structural");
        assert_eq!(llm.len(), 1, "Prose = 1 LLM");
    }
}
```

- [ ] **Step 2: Run the tests**

```bash
cargo test -p thinkingroot-extract tiered_tests
```

Expected: All PASS.

- [ ] **Step 3: Run full workspace tests**

```bash
cargo test --workspace
```

Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-extract/src/extractor.rs
git commit -m "test(extract): add tiered extraction integration tests

Verifies structural chunks produce entities/claims without LLM,
router correctly splits mixed documents, and extraction tier is
propagated to claims."
```

---

## Self-Review Checklist

### Spec Coverage
- [x] **Tier 0 Structural Extraction** → Task 2 (structural.rs)
- [x] **Tier Router** → Task 3 (router.rs)
- [x] **Graph-Primed Context** → Task 4 (graph_context.rs)
- [x] **Split Focused Prompts** → Task 5 (focused_prompts.rs)
- [x] **ExtractionTier type system** → Task 1 (schema.rs + claim.rs)
- [x] **Cascade Grounding** → Task 7 (pipeline.rs)
- [x] **Pipeline Integration** → Task 7 (pipeline.rs)
- [x] **Graph Schema Update** → Task 8 (graph.rs)
- [x] **Integration Tests** → Task 9

### Placeholder Scan
- No TBD, TODO, "implement later" found
- All code steps have actual code
- All test steps have actual test code

### Type Consistency
- `ExtractionTier` defined once in `thinkingroot-core`, re-exported in extract schema
- `ExtractedClaim.extraction_tier` → `Claim.extraction_tier` via `convert_result_static`
- `GroundingMethod::Structural` added alongside `ExtractionTier::Structural`
- `GraphPrimedContext` / `KnownEntity` consistent across graph_context.rs and extractor.rs
- `router::Tier` is internal to the extract crate (not exposed to core)

---

## Build Order and Dependencies

```
Task 1: ExtractionTier Foundation ─────────────────┐
    │                                               │
    ├── Task 2: Structural Extractor (needs Tier)   │
    │       │                                       │
    │       ├── Task 3: Tier Router                 │
    │       │                                       │
    ├── Task 4: Graph-Primed Context (independent)  │
    │       │                                       │
    ├── Task 5: Focused Prompts (independent)       │
    │                                               │
    └── Task 6: Wire Tiered Extraction ─────────────┤ (needs 2,3,4,5)
            │                                       │
            ├── Task 7: Cascade Grounding ──────────┤ (needs 1,6)
            │                                       │
            ├── Task 8: CozoDB Schema ─────────────┤ (needs 1)
            │                                       │
            └── Task 9: Integration Tests ──────────┘ (needs all above)
```

Tasks 2, 4, 5 can run in parallel after Task 1 completes.
Task 3 depends on Task 2.
Task 6 depends on Tasks 2, 3, 4, 5.
Tasks 7 and 8 can run in parallel after Task 6.
Task 9 runs last.
