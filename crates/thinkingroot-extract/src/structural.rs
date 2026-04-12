//! Tier 0 structural extractor — zero LLM, zero hallucination.
//!
//! Converts AST-rich chunks (FunctionDef, TypeDef, Import, ManifestDependency,
//! Heading, Prose) into `ExtractionResult` deterministically using only the
//! metadata that the parse crate already computed via tree-sitter.  Every claim
//! produced here carries `extraction_tier: ExtractionTier::Structural` and a
//! confidence value appropriate to the evidence certainty.

use thinkingroot_core::ir::{Chunk, ChunkType};
use thinkingroot_core::types::ExtractionTier;

use crate::schema::{ExtractedClaim, ExtractedEntity, ExtractedRelation, ExtractionResult};

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns `true` if this chunk can be handled by the structural extractor
/// without any LLM calls.
pub fn is_structurally_extractable(chunk: &Chunk) -> bool {
    matches!(
        chunk.chunk_type,
        ChunkType::FunctionDef
            | ChunkType::TypeDef
            | ChunkType::Import
            | ChunkType::ManifestDependency
            | ChunkType::Heading
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
        ChunkType::ManifestDependency => extract_manifest_dep(chunk, source_uri),
        ChunkType::Heading => extract_heading(chunk, source_uri),
        ChunkType::Prose => extract_prose(chunk, source_uri),
        ChunkType::Comment | ChunkType::ModuleDoc => extract_doc_comment(chunk, source_uri),
        _ => ExtractionResult::empty(),
    }
}

// ── Internal extractors ───────────────────────────────────────────────────────

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

/// FunctionDef → Entity(function) + Claim(api_signature) + Claim(definition)
///             + Relation(file contains func) + optional Relation(parent contains method)
///             + optional Relation(calls) for each callee in calls_functions
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

    // Gap 2: emit calls relations for each function this function calls.
    for callee in &chunk.metadata.calls_functions {
        if callee.is_empty() {
            continue;
        }
        let callee_entity = ExtractedEntity {
            name: callee.clone(),
            entity_type: "function".to_string(),
            aliases: Vec::new(),
            description: Some(format!("Function called by {name}")),
        };
        let calls_rel = ExtractedRelation {
            from_entity: name.clone(),
            to_entity: callee.clone(),
            relation_type: "calls".to_string(),
            description: Some(format!("{name} calls {callee}")),
            confidence: 0.99,
        };
        result.entities.push(callee_entity);
        result.relations.push(calls_rel);
    }

    result
}

/// TypeDef → Entity(inferred type) + Claim(definition) + Relation(file contains type)
///         + optional Relation(implements) if trait_name is set
///         + optional Relation(depends_on) for each field_type
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

    let mut result = ExtractionResult {
        claims: vec![def_claim],
        entities: vec![entity, file_entity],
        relations: vec![file_contains],
    };

    // If this is `impl Trait for Type`, emit an `implements` relation.
    if let Some(trait_name) = &chunk.metadata.trait_name {
        if !trait_name.is_empty() {
            let trait_entity = ExtractedEntity {
                name: trait_name.clone(),
                entity_type: "concept".to_string(),
                aliases: Vec::new(),
                description: Some(format!("Trait implemented by {name}")),
            };
            let implements_rel = ExtractedRelation {
                from_entity: name.clone(),
                to_entity: trait_name.clone(),
                relation_type: "implements".to_string(),
                description: Some(format!("{name} implements {trait_name}")),
                confidence: 0.99,
            };
            result.entities.push(trait_entity);
            result.relations.push(implements_rel);

            let impl_claim = ExtractedClaim {
                statement: format!("{name} implements the {trait_name} trait"),
                claim_type: "definition".to_string(),
                confidence: 0.99,
                entities: vec![name.clone(), trait_name.clone()],
                source_quote: Some(chunk.content.lines().next().unwrap_or("").to_string()),
                extraction_tier: ExtractionTier::Structural,
            };
            result.claims.push(impl_claim);
        }
    }

    // For each field type, emit a `depends_on` relation.
    for field_type in &chunk.metadata.field_types {
        let field_entity = ExtractedEntity {
            name: field_type.clone(),
            entity_type: "concept".to_string(),
            aliases: Vec::new(),
            description: None,
        };
        let depends_rel = ExtractedRelation {
            from_entity: name.clone(),
            to_entity: field_type.clone(),
            relation_type: "depends_on".to_string(),
            description: Some(format!("{name} has a field of type {field_type}")),
            confidence: 0.99,
        };
        result.entities.push(field_entity);
        result.relations.push(depends_rel);
    }

    result
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

/// Prose → dispatches to extract_git_commit (if git:// URI) and/or extract_prose_links (if links present)
fn extract_prose(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let mut result = ExtractionResult::empty();

    // Git authorship: source_uri starts with git:// AND commit_author is set.
    if source_uri.starts_with("git://") {
        if let Some(author) = &chunk.metadata.commit_author {
            if !author.is_empty() {
                result.merge(extract_git_commit(chunk, source_uri, author));
            }
        }
    }

    // Link extraction: any Prose with non-empty links.
    if !chunk.metadata.links.is_empty() {
        result.merge(extract_prose_links(chunk, source_uri));
    }

    result
}

/// Git commit Prose → Entity(author) + Relation(created_by file→author at 0.7) + Claim(fact)
fn extract_git_commit(chunk: &Chunk, source_uri: &str, author: &str) -> ExtractionResult {
    if chunk.metadata.changed_files.is_empty() {
        return ExtractionResult::empty();
    }

    // The SHA is the last path segment of the git:// URI.
    let sha = file_name_from_uri(source_uri);

    let author_entity = ExtractedEntity {
        name: author.to_string(),
        entity_type: "person".to_string(),
        aliases: Vec::new(),
        description: Some(format!("{author} is a contributor")),
    };

    let mut result = ExtractionResult {
        claims: Vec::new(),
        entities: vec![author_entity],
        relations: Vec::new(),
    };

    for file_path in &chunk.metadata.changed_files {
        let file_entity = ExtractedEntity {
            name: file_path.clone(),
            entity_type: "file".to_string(),
            aliases: Vec::new(),
            description: None,
        };
        let created_by = ExtractedRelation {
            from_entity: file_path.clone(),
            to_entity: author.to_string(),
            relation_type: "created_by".to_string(),
            description: Some(format!("{author} modified {file_path}")),
            confidence: 0.7,
        };
        let fact_claim = ExtractedClaim {
            statement: format!("{author} modified {file_path} in commit {sha}"),
            claim_type: "fact".to_string(),
            confidence: 0.7,
            entities: vec![author.to_string(), file_path.clone()],
            source_quote: None,
            extraction_tier: ExtractionTier::Structural,
        };
        result.entities.push(file_entity);
        result.relations.push(created_by);
        result.claims.push(fact_claim);
    }

    result
}

/// Prose links → Relation(related_to doc→link): relative at 0.99, absolute at 0.7
fn extract_prose_links(chunk: &Chunk, source_uri: &str) -> ExtractionResult {
    let doc_name = file_name_from_uri(source_uri);

    let doc_entity = ExtractedEntity {
        name: doc_name.clone(),
        entity_type: "file".to_string(),
        aliases: Vec::new(),
        description: None,
    };

    let mut result = ExtractionResult {
        claims: Vec::new(),
        entities: vec![doc_entity],
        relations: Vec::new(),
    };

    for url in &chunk.metadata.links {
        // Relative path (starts with '.' or has no scheme '://') → 0.99
        // Absolute URL (has '://') → 0.7
        let confidence = if url.contains("://") { 0.7 } else { 0.99 };

        let link_entity = ExtractedEntity {
            name: url.clone(),
            entity_type: "file".to_string(),
            aliases: Vec::new(),
            description: None,
        };
        let rel = ExtractedRelation {
            from_entity: doc_name.clone(),
            to_entity: url.clone(),
            relation_type: "related_to".to_string(),
            description: Some(format!("{doc_name} references {url}")),
            confidence,
        };
        result.entities.push(link_entity);
        result.relations.push(rel);
    }

    result
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

    // ── 4. Prose → empty (default metadata, no git:// URI, no links) ─────────

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
            ChunkType::ManifestDependency,
            ChunkType::Heading,
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

    #[test]
    fn impl_with_trait_produces_implements_relation() {
        let mut chunk = Chunk::new(
            "impl Serialize for MyStruct {}",
            ChunkType::TypeDef,
            1, 1,
        );
        chunk.metadata = ChunkMetadata {
            type_name: Some("MyStruct".to_string()),
            trait_name: Some("Serialize".to_string()),
            ..Default::default()
        };

        let result = extract_structural(&chunk, "src/models.rs");

        let implements = result.relations.iter()
            .find(|r| r.relation_type == "implements");
        assert!(
            implements.is_some(),
            "impl Trait for Struct must produce an implements relation"
        );
        let rel = implements.unwrap();
        assert_eq!(rel.from_entity, "MyStruct");
        assert_eq!(rel.to_entity, "Serialize");
    }

    #[test]
    fn struct_with_field_types_produces_depends_on_relations() {
        let mut chunk = Chunk::new(
            "struct Engine { storage: StorageBackend, config: EngineConfig }",
            ChunkType::TypeDef,
            1, 3,
        );
        chunk.metadata = ChunkMetadata {
            type_name: Some("Engine".to_string()),
            field_types: vec!["StorageBackend".to_string(), "EngineConfig".to_string()],
            ..Default::default()
        };

        let result = extract_structural(&chunk, "src/engine.rs");

        let deps: Vec<_> = result.relations.iter()
            .filter(|r| r.relation_type == "depends_on")
            .collect();
        assert_eq!(deps.len(), 2, "two field types → two depends_on relations");
        assert!(deps.iter().any(|r| r.to_entity == "StorageBackend"));
        assert!(deps.iter().any(|r| r.to_entity == "EngineConfig"));
    }

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
        let rel = refs.iter().find(|r| r.to_entity == "./oauth.md").unwrap();
        assert_eq!(rel.confidence, 0.99);
        let abs = refs.iter().find(|r| r.to_entity == "https://example.com").unwrap();
        assert_eq!(abs.confidence, 0.7);
    }

    // ── Gap 4: Git authorship ────────────────────────────────────────────────

    #[test]
    fn git_commit_produces_created_by_relations() {
        let meta = ChunkMetadata {
            commit_author: Some("Alice".to_string()),
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
}
