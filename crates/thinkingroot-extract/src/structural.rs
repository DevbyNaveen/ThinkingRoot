//! Tier 0 structural extractor — zero LLM, zero hallucination.
//!
//! Converts AST-rich chunks (FunctionDef, TypeDef, Import)
//! into `ExtractionResult` deterministically using only the metadata that the
//! parse crate already computed via tree-sitter.  Every claim produced here
//! carries `extraction_tier: ExtractionTier::Structural` and `confidence: 0.99`.

use thinkingroot_core::ir::{Chunk, ChunkType};
use thinkingroot_core::types::ExtractionTier;

use crate::schema::{ExtractedClaim, ExtractedEntity, ExtractedRelation, ExtractionResult};

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns `true` if this chunk can be handled by the structural extractor
/// without any LLM calls.
pub fn is_structurally_extractable(chunk: &Chunk) -> bool {
    matches!(
        chunk.chunk_type,
        ChunkType::FunctionDef | ChunkType::TypeDef | ChunkType::Import
    )
}

/// Main entry point.  Given a single chunk and the URI of its source file,
/// return all entities/claims/relations that can be determined without an LLM.
/// For unsupported chunk types or chunks with insufficient metadata, returns
/// `ExtractionResult::empty()`.
pub fn extract_structural(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    match chunk.chunk_type {
        ChunkType::FunctionDef => extract_function_def(chunk, source_uri),
        ChunkType::TypeDef => extract_type_def(chunk, source_uri),
        ChunkType::Import => extract_import(chunk, source_uri),
        ChunkType::Comment | ChunkType::ModuleDoc => extract_doc_comment(chunk, source_uri),
        // Prose, Code, Heading, List, Table — not structurally extractable.
        _ => ExtractionResult::empty(),
    }
}

// ── Internal extractors ───────────────────────────────────────────────────────

/// FunctionDef → Entity(function) + Claim(api_signature) + Claim(definition)
///             + Relation(file contains func) + optional Relation(parent contains method)
fn extract_function_def(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let name = match &chunk.metadata.function_name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => return ExtractionResult::empty(),
    };

    let file_name = file_name_from_uri(source_uri);
    let signature = build_signature(&name, chunk);

    // ── Entity ────────────────────────────────────────────────────────────────
    let entity = ExtractedEntity {
        name: name.clone(),
        entity_type: "function".to_string(),
        aliases: Vec::new(),
        description: Some(format!("Function defined in {file_name}")),
    };

    // ── Claims ────────────────────────────────────────────────────────────────
    let sig_claim = ExtractedClaim {
        statement: format!("{name} has signature: {signature}"),
        claim_type: "api_signature".to_string(),
        confidence: 0.99,
        entities: vec![name.clone()],
        source_quote: Some(chunk.content.lines().next().unwrap_or("").to_string()),
        extraction_tier: ExtractionTier::Structural,
    };

    let def_claim = ExtractedClaim {
        statement: format!("{name} is defined in {file_name}"),
        claim_type: "definition".to_string(),
        confidence: 0.99,
        entities: vec![name.clone(), file_name.clone()],
        source_quote: None,
        extraction_tier: ExtractionTier::Structural,
    };

    // ── Relations ─────────────────────────────────────────────────────────────
    // Ensure the file entity exists so the relation can resolve.
    let file_entity = ExtractedEntity {
        name: file_name.clone(),
        entity_type: "file".to_string(),
        aliases: Vec::new(),
        description: Some(format!("Source file {file_name}")),
    };

    let file_contains = ExtractedRelation {
        from_entity: file_name.clone(),
        to_entity: name.clone(),
        relation_type: "contains".to_string(),
        description: Some(format!("{file_name} contains function {name}")),
        confidence: 0.99,
    };

    let mut result = ExtractionResult {
        claims: vec![sig_claim, def_claim],
        entities: vec![entity, file_entity],
        relations: vec![file_contains],
    };

    // If this is a method (has a parent type), also record parent→method.
    if let Some(parent) = &chunk.metadata.parent {
        if !parent.is_empty() {
            let parent_entity = ExtractedEntity {
                name: parent.clone(),
                entity_type: "concept".to_string(), // conservative default for parent scope
                aliases: Vec::new(),
                description: Some(format!("Type defined in {file_name}")),
            };
            let parent_contains = ExtractedRelation {
                from_entity: parent.clone(),
                to_entity: name.clone(),
                relation_type: "contains".to_string(),
                description: Some(format!("{parent} contains method {name}")),
                confidence: 0.99,
            };
            result.entities.push(parent_entity);
            result.relations.push(parent_contains);
        }
    }

    result
}

/// TypeDef → Entity(inferred type) + Claim(definition) + Relation(file contains type)
fn extract_type_def(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let name = match &chunk.metadata.type_name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => return ExtractionResult::empty(),
    };

    let file_name = file_name_from_uri(source_uri);
    let entity_type = infer_entity_type_from_content(&chunk.content);

    let entity = ExtractedEntity {
        name: name.clone(),
        entity_type: entity_type.to_string(),
        aliases: Vec::new(),
        description: Some(format!("{name} is a type defined in {file_name}")),
    };

    let def_claim = ExtractedClaim {
        statement: format!("{name} is a type defined in {file_name}"),
        claim_type: "definition".to_string(),
        confidence: 0.99,
        entities: vec![name.clone(), file_name.clone()],
        source_quote: Some(chunk.content.lines().next().unwrap_or("").to_string()),
        extraction_tier: ExtractionTier::Structural,
    };

    let file_entity = ExtractedEntity {
        name: file_name.clone(),
        entity_type: "file".to_string(),
        aliases: Vec::new(),
        description: Some(format!("Source file {file_name}")),
    };

    let file_contains = ExtractedRelation {
        from_entity: file_name.clone(),
        to_entity: name.clone(),
        relation_type: "contains".to_string(),
        description: Some(format!("{file_name} contains type {name}")),
        confidence: 0.99,
    };

    ExtractionResult {
        claims: vec![def_claim],
        entities: vec![entity, file_entity],
        relations: vec![file_contains],
    }
}

/// Import → Entity(file) + Entity(imported module) + Claim(dependency) + Relation(uses)
fn extract_import(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let import_path = match &chunk.metadata.import_path {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return ExtractionResult::empty(),
    };

    let file_name = file_name_from_uri(source_uri);

    // Use the last segment of the import path as the canonical module name.
    // Supports Rust (::), path-style (/), and Python-style (.) imports.
    let module_name = import_path
        .rsplit("::")
        .next()
        .or_else(|| import_path.rsplit('/').next())
        .or_else(|| import_path.rsplit('.').next())
        .unwrap_or(&import_path)
        .trim_matches('"')
        .to_string();

    let file_entity = ExtractedEntity {
        name: file_name.clone(),
        entity_type: "file".to_string(),
        aliases: Vec::new(),
        description: Some(format!("Source file {file_name}")),
    };

    let module_entity = ExtractedEntity {
        name: module_name.clone(),
        entity_type: "module".to_string(),
        aliases: vec![import_path.clone()],
        description: Some(format!("Module imported as {import_path}")),
    };

    let dep_claim = ExtractedClaim {
        statement: format!("{file_name} depends on {import_path}"),
        claim_type: "dependency".to_string(),
        confidence: 0.99,
        entities: vec![file_name.clone(), module_name.clone()],
        source_quote: Some(chunk.content.trim().to_string()),
        extraction_tier: ExtractionTier::Structural,
    };

    let uses_relation = ExtractedRelation {
        from_entity: file_name.clone(),
        to_entity: module_name.clone(),
        relation_type: "uses".to_string(),
        description: Some(format!("{file_name} imports {import_path}")),
        confidence: 0.99,
    };

    ExtractionResult {
        claims: vec![dep_claim],
        entities: vec![file_entity, module_entity],
        relations: vec![uses_relation],
    }
}

/// Comment/ModuleDoc → Claim(definition) if a parent is present, empty otherwise.
fn extract_doc_comment(chunk: &Chunk, _source_uri: &str) -> ExtractionResult {
    let parent = match &chunk.metadata.parent {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return ExtractionResult::empty(),
    };

    let statement = format!(
        "{parent}: {}",
        chunk.content.trim().trim_start_matches("///").trim_start_matches("//!").trim()
    );

    let def_claim = ExtractedClaim {
        statement,
        claim_type: "definition".to_string(),
        confidence: 0.99,
        entities: vec![parent],
        source_quote: Some(chunk.content.trim().to_string()),
        extraction_tier: ExtractionTier::Structural,
    };

    ExtractionResult {
        claims: vec![def_claim],
        entities: Vec::new(),
        relations: Vec::new(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a human-readable function signature from available metadata.
pub fn build_signature(name: &str, chunk: &Chunk) -> String {
    let params = chunk
        .metadata
        .parameters
        .as_deref()
        .map(|ps| ps.join(", "))
        .unwrap_or_default();

    match &chunk.metadata.return_type {
        Some(ret) if !ret.is_empty() => format!("{name}({params}) -> {ret}"),
        _ => format!("{name}({params})"),
    }
}

/// Infer a broad entity type from the raw content of a TypeDef chunk.
/// Looks for Rust/Python/Go keywords at the start of the chunk.
pub fn infer_entity_type_from_content(content: &str) -> &'static str {
    let lower = content.trim_start().to_lowercase();
    if lower.starts_with("struct ") || lower.starts_with("class ") {
        "system"
    } else if lower.starts_with("enum ") {
        "concept"
    } else if lower.starts_with("trait ") || lower.starts_with("interface ") || lower.starts_with("protocol ") {
        "api"
    } else if lower.starts_with("type ") || lower.starts_with("typedef ") {
        "concept"
    } else {
        "concept"
    }
}

/// Extract the file name (last path segment) from a URI or file path.
fn file_name_from_uri(uri: &str) -> String {
    uri.rsplit('/').next().unwrap_or(uri).to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType};
    use thinkingroot_core::types::ExtractionTier;

    use super::*;

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

    // ── 1. FunctionDef → entity + api_signature claim ─────────────────────────

    #[test]
    fn function_def_produces_entity_and_claim() {
        let meta = ChunkMetadata {
            function_name: Some("parse_document".to_string()),
            parameters: Some(vec!["path: &Path".to_string(), "opts: Options".to_string()]),
            return_type: Some("Result<DocumentIR>".to_string()),
            ..Default::default()
        };
        let chunk = make_chunk(
            ChunkType::FunctionDef,
            "pub fn parse_document(path: &Path, opts: Options) -> Result<DocumentIR> { ... }",
            meta,
        );

        let result = extract_structural(&chunk, "src/parse/mod.rs");

        // Must have at least one entity named "parse_document" with type "function".
        let func_entity = result
            .entities
            .iter()
            .find(|e| e.name == "parse_document")
            .expect("expected function entity");
        assert_eq!(func_entity.entity_type, "function");

        // Must have an api_signature claim with Structural tier.
        let sig_claim = result
            .claims
            .iter()
            .find(|c| c.claim_type == "api_signature")
            .expect("expected api_signature claim");
        assert_eq!(sig_claim.extraction_tier, ExtractionTier::Structural);
        assert_eq!(sig_claim.confidence, 0.99);
        assert!(sig_claim.statement.contains("parse_document"));
        assert!(sig_claim.statement.contains("Result<DocumentIR>"));
    }

    // ── 2. TypeDef → entity + definition claim ────────────────────────────────

    #[test]
    fn type_def_produces_entity_and_claim() {
        let meta = ChunkMetadata {
            type_name: Some("GraphStore".to_string()),
            ..Default::default()
        };
        let chunk = make_chunk(
            ChunkType::TypeDef,
            "struct GraphStore { db: cozo::Db }",
            meta,
        );

        let result = extract_structural(&chunk, "crates/graph/src/store.rs");

        // Entity named "GraphStore" should be present.
        let entity = result
            .entities
            .iter()
            .find(|e| e.name == "GraphStore")
            .expect("expected GraphStore entity");
        // struct → "system"
        assert_eq!(entity.entity_type, "system");

        // Claim with type "definition" and Structural tier.
        let def_claim = result
            .claims
            .iter()
            .find(|c| c.claim_type == "definition")
            .expect("expected definition claim");
        assert_eq!(def_claim.extraction_tier, ExtractionTier::Structural);
        assert_eq!(def_claim.confidence, 0.99);
        assert!(def_claim.statement.contains("GraphStore"));
    }

    // ── 3. Import → relation(uses) ────────────────────────────────────────────

    #[test]
    fn import_produces_relation() {
        let meta = ChunkMetadata {
            import_path: Some("thinkingroot_core::ir::DocumentIR".to_string()),
            ..Default::default()
        };
        let chunk = make_chunk(
            ChunkType::Import,
            "use thinkingroot_core::ir::DocumentIR;",
            meta,
        );

        let result = extract_structural(&chunk, "src/extractor.rs");

        // Must have a "uses" relation from the file to the imported module.
        let uses_rel = result
            .relations
            .iter()
            .find(|r| r.relation_type == "uses")
            .expect("expected uses relation");
        assert_eq!(uses_rel.from_entity, "extractor.rs");
        // Last segment of the import path becomes the entity name.
        assert_eq!(uses_rel.to_entity, "DocumentIR");
    }

    // ── 4. Prose → empty ─────────────────────────────────────────────────────

    #[test]
    fn prose_chunk_returns_empty() {
        let chunk = make_chunk(
            ChunkType::Prose,
            "ThinkingRoot is a knowledge compiler.",
            ChunkMetadata::default(),
        );
        let result = extract_structural(&chunk, "docs/README.md");
        assert!(result.claims.is_empty());
        assert!(result.entities.is_empty());
        assert!(result.relations.is_empty());
    }

    // ── 5. Method with parent → contains relation from parent ─────────────────

    #[test]
    fn method_with_parent_produces_contains_relation() {
        let meta = ChunkMetadata {
            function_name: Some("save".to_string()),
            parent: Some("GraphStore".to_string()),
            parameters: Some(vec!["&self".to_string()]),
            return_type: Some("Result<()>".to_string()),
            ..Default::default()
        };
        let chunk = make_chunk(
            ChunkType::FunctionDef,
            "pub fn save(&self) -> Result<()> { ... }",
            meta,
        );

        let result = extract_structural(&chunk, "src/graph/store.rs");

        // Should have a "contains" relation from GraphStore → save.
        let parent_contains = result
            .relations
            .iter()
            .find(|r| r.relation_type == "contains" && r.from_entity == "GraphStore")
            .expect("expected GraphStore contains save relation");
        assert_eq!(parent_contains.to_entity, "save");
    }

    // ── Additional sanity checks ─────────────────────────────────────────────

    #[test]
    fn is_structurally_extractable_recognises_supported_types() {
        for ct in [
            ChunkType::FunctionDef,
            ChunkType::TypeDef,
            ChunkType::Import,
        ] {
            let chunk = make_chunk(ct, "", ChunkMetadata::default());
            assert!(
                is_structurally_extractable(&chunk),
                "{ct:?} should be structurally extractable"
            );
        }
    }

    #[test]
    fn is_structurally_extractable_rejects_prose_code_etc() {
        for ct in [
            ChunkType::Prose,
            ChunkType::Code,
            ChunkType::Heading,
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

    #[test]
    fn function_def_missing_name_returns_empty() {
        let chunk = make_chunk(ChunkType::FunctionDef, "fn ???() {}", ChunkMetadata::default());
        let result = extract_structural(&chunk, "src/lib.rs");
        assert!(result.claims.is_empty());
        assert!(result.entities.is_empty());
    }

    #[test]
    fn type_def_missing_name_returns_empty() {
        let chunk = make_chunk(ChunkType::TypeDef, "struct { }", ChunkMetadata::default());
        let result = extract_structural(&chunk, "src/lib.rs");
        assert!(result.claims.is_empty());
        assert!(result.entities.is_empty());
    }

    #[test]
    fn import_missing_path_returns_empty() {
        let chunk = make_chunk(ChunkType::Import, "use ;", ChunkMetadata::default());
        let result = extract_structural(&chunk, "src/lib.rs");
        assert!(result.claims.is_empty());
        assert!(result.entities.is_empty());
    }

    #[test]
    fn infer_entity_type_struct_is_system() {
        assert_eq!(infer_entity_type_from_content("struct Foo { }"), "system");
    }

    #[test]
    fn infer_entity_type_enum_is_concept() {
        assert_eq!(infer_entity_type_from_content("enum Color { Red, Green }"), "concept");
    }

    #[test]
    fn infer_entity_type_trait_is_api() {
        assert_eq!(infer_entity_type_from_content("trait Storage { fn save(&self); }"), "api");
    }

    #[test]
    fn build_signature_includes_return_type() {
        let meta = ChunkMetadata {
            function_name: Some("add".to_string()),
            parameters: Some(vec!["a: i32".to_string(), "b: i32".to_string()]),
            return_type: Some("i32".to_string()),
            ..Default::default()
        };
        let chunk = make_chunk(ChunkType::FunctionDef, "fn add(a: i32, b: i32) -> i32 { a + b }", meta);
        let sig = build_signature("add", &chunk);
        assert_eq!(sig, "add(a: i32, b: i32) -> i32");
    }

    #[test]
    fn doc_comment_without_parent_returns_empty() {
        let chunk = make_chunk(
            ChunkType::Comment,
            "/// This is a doc comment",
            ChunkMetadata::default(),
        );
        let result = extract_structural(&chunk, "src/lib.rs");
        assert!(result.claims.is_empty());
    }

    #[test]
    fn doc_comment_with_parent_produces_definition_claim() {
        let meta = ChunkMetadata {
            parent: Some("GraphStore".to_string()),
            ..Default::default()
        };
        let chunk = make_chunk(
            ChunkType::Comment,
            "/// Stores the compiled knowledge graph.",
            meta,
        );
        let result = extract_structural(&chunk, "src/graph.rs");
        assert_eq!(result.claims.len(), 1);
        assert_eq!(result.claims[0].claim_type, "definition");
        assert_eq!(result.claims[0].extraction_tier, ExtractionTier::Structural);
        assert!(result.claims[0].statement.contains("GraphStore"));
    }
}
