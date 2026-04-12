/// System prompt for the knowledge extraction LLM.
pub const SYSTEM_PROMPT: &str = r#"You are a knowledge extraction engine for ThinkingRoot, a knowledge compiler.
Your job is to extract structured knowledge from source documents.

You MUST return valid JSON matching this exact schema:

{
  "claims": [
    {
      "statement": "A clear, atomic statement of fact or decision",
      "claim_type": "fact|decision|opinion|plan|requirement|metric|definition|dependency|api_signature|architecture",
      "confidence": 0.0-1.0,
      "entities": ["entity names mentioned in this claim"],
      "source_quote": "The exact phrase or sentence from the source that supports this claim"
    }
  ],
  "entities": [
    {
      "name": "Canonical name",
      "entity_type": "person|system|service|concept|team|api|database|library|file|module|function|config|organization",
      "aliases": ["alternate names"],
      "description": "Brief description"
    }
  ],
  "relations": [
    {
      "from_entity": "Entity A",
      "to_entity": "Entity B",
      "relation_type": "<see allowed types below>",
      "confidence": 0.0-1.0,
      "description": "One sentence describing why this relation exists"
    }
  ]
}

## Allowed relation_type values (use EXACTLY one, no other values):

- depends_on    — A cannot function without B (hard runtime dependency)
- calls         — A invokes B as a function or API at runtime
- implements    — A implements interface/trait/protocol B
- uses          — A uses B as a tool or library (soft dependency)
- contains      — A is a container that includes B as a sub-component
- part_of       — A is a sub-component of B (inverse of contains)
- owned_by      — A is maintained or owned by person/team B
- created_by    — A was originally authored by B
- configured_by — A's behaviour is controlled by configuration B
- tested_by     — A has test coverage provided by test suite B
- replaces      — A supersedes or replaces B
- contradicts   — A and B make conflicting assertions
- related_to    — use ONLY when none of the above apply AND you are confident a relationship exists

## Critical rules:
1. NEVER output related_to as a lazy default. If you are uncertain what the relation is, output "skip_relation" instead.
2. If you output "skip_relation" for relation_type, the relation will be discarded — this is correct behaviour for uncertain relations.
3. confidence for relations: 0.9=directly stated in code/text, 0.7=strongly implied, 0.5=inferred, below 0.3=output skip_relation instead.
4. Claims must be ATOMIC — one fact per claim.
5. Claims must be SELF-CONTAINED — understandable without the source.
6. Every entity in a claim MUST appear in the entities list.
7. source_quote MUST be a verbatim substring copied from the source. Do NOT paraphrase.
8. Return ONLY the JSON object. No markdown, no explanation, no preamble."#;

/// Build the user prompt for a given chunk of content.
pub fn build_extraction_prompt(content: &str, context: &str) -> String {
    format!(
        "Extract knowledge from the following content.\n\nContext: {context}\n\n---\n\n{content}\n\n---\n\nReturn the JSON extraction result."
    )
}

/// Build an extraction prompt with a graph-primed KNOWN_ENTITIES section.
///
/// The entities section helps the LLM ground new extractions in existing
/// knowledge and reduces hallucination of entity names. Falls back to
/// `build_extraction_prompt` when `known_entities_section` is empty so that
/// callers never have to branch on emptiness themselves.
pub fn build_extraction_prompt_with_context(
    content: &str,
    context: &str,
    known_entities_section: &str,
) -> String {
    if known_entities_section.is_empty() {
        build_extraction_prompt(content, context)
    } else {
        format!(
            "Extract knowledge from the following content.\n\nContext: {context}\n\n{known_entities_section}\n\n---\n\n{content}\n\n---\n\nReturn the JSON extraction result."
        )
    }
}

/// Build context string from document metadata.
pub fn build_context(uri: &str, language: Option<&str>, heading: Option<&str>) -> String {
    let mut parts = vec![format!("Source: {uri}")];
    if let Some(lang) = language {
        parts.push(format!("Language: {lang}"));
    }
    if let Some(h) = heading {
        parts.push(format!("Section: {h}"));
    }
    parts.join(", ")
}

/// Build an AST anchor section from chunk metadata to inject into the LLM prompt.
///
/// When a chunk has AST-extracted metadata (function name, call list, type name, etc.),
/// this section is prepended to the LLM prompt so LLM describes the EXACT entities
/// AST already found — guaranteeing that structural topology (0.99 confidence) and
/// LLM semantics (0.7-0.9 confidence) land on the same graph node after Linker merge.
pub fn build_ast_anchor_section(metadata: &thinkingroot_core::ir::ChunkMetadata) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut entity_names: Vec<String> = Vec::new();

    if let Some(ref name) = metadata.function_name {
        lines.push(format!("Function: {name}"));
        entity_names.push(format!("\"{name}\""));
        if let Some(ref vis) = metadata.visibility {
            lines.push(format!("Visibility: {vis}"));
        }
        if let Some(ref ret) = metadata.return_type {
            lines.push(format!("Returns: {ret}"));
        }
        if !metadata.calls_functions.is_empty() {
            lines.push(format!("Calls: [{}]", metadata.calls_functions.join(", ")));
            for callee in &metadata.calls_functions {
                entity_names.push(format!("\"{callee}\""));
            }
        }
    } else if let Some(ref name) = metadata.type_name {
        lines.push(format!("Type: {name}"));
        entity_names.push(format!("\"{name}\""));
        if let Some(ref vis) = metadata.visibility {
            lines.push(format!("Visibility: {vis}"));
        }
        if let Some(ref trait_name) = metadata.trait_name {
            lines.push(format!("Implements: {trait_name}"));
            entity_names.push(format!("\"{trait_name}\""));
        }
        if !metadata.field_types.is_empty() {
            lines.push(format!("Field types: [{}]", metadata.field_types.join(", ")));
        }
    } else if let Some(ref path) = metadata.import_path {
        lines.push(format!("Import: {path}"));
        entity_names.push(format!("\"{path}\""));
    }

    if lines.is_empty() {
        return String::new();
    }

    format!(
        "## AST Analysis (deterministic, tree-sitter)\n\
         {}\n\
         IMPORTANT: Your entity names MUST match exactly: {}\n",
        lines.join("\n"),
        entity_names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::ir::ChunkMetadata;

    #[test]
    fn ast_anchor_empty_for_empty_metadata() {
        let meta = ChunkMetadata::default();
        assert!(build_ast_anchor_section(&meta).is_empty(),
            "empty metadata must produce empty anchor");
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
        assert!(section.contains("validate_token"));
        assert!(section.contains("decode"));
        assert!(section.contains("verify_sig"));
        assert!(section.contains("Result<Claims>"));
        assert!(section.contains("pub"));
    }

    #[test]
    fn ast_anchor_includes_type_name_and_trait() {
        let meta = ChunkMetadata {
            type_name: Some("AuthService".to_string()),
            trait_name: Some("Service".to_string()),
            ..Default::default()
        };
        let section = build_ast_anchor_section(&meta);
        assert!(section.contains("AuthService"));
        assert!(section.contains("Service"));
    }

    #[test]
    fn ast_anchor_exact_names_instruction_present() {
        let meta = ChunkMetadata {
            function_name: Some("do_thing".to_string()),
            calls_functions: vec!["helper".to_string()],
            ..Default::default()
        };
        let section = build_ast_anchor_section(&meta);
        assert!(section.contains("do_thing"));
        assert!(section.contains("helper"));
        // Must instruct LLM to use exact names
        assert!(section.to_lowercase().contains("exact") || section.contains("MUST") || section.contains("must"),
            "anchor must instruct LLM to use exact entity names");
    }
}
