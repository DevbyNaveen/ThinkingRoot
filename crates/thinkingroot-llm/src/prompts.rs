/// System prompt for the knowledge extraction LLM.
/// Kept ≤600 tokens: schema + relation types + critical rules only.
// Schema intentionally on one line — indenting adds ~50 tokens and would break the ≤600 token budget test.
pub const SYSTEM_PROMPT: &str = r#"You are a knowledge extraction engine. Extract structured knowledge from source documents.

Return valid JSON matching this exact schema:
{"claims":[{"statement":"atomic fact","claim_type":"fact|decision|opinion|plan|requirement|metric|definition|dependency|api_signature|architecture|preference","confidence":0.0,"entities":["names"],"source_quote":"verbatim substring","event_date":"YYYY-MM-DD or null","predicate":{"language":"regex|rust_ast|jsonpath","query":"pattern","scope_globs":[]}}],"entities":[{"name":"canonical","entity_type":"person|system|service|concept|team|api|database|library|file|module|function|config|organization","aliases":[],"description":"brief"}],"relations":[{"from_entity":"A","to_entity":"B","relation_type":"see below","confidence":0.0,"description":"one sentence"}]}

Relation types (use exactly one): depends_on, calls, implements, uses, contains, part_of, owned_by, created_by, configured_by, tested_by, replaces, contradicts, related_to

Rules:
1. Never use related_to as default — use skip_relation if uncertain.
2. Relations below confidence 0.3 → output skip_relation.
3. Claims must be ATOMIC (one fact) and SELF-CONTAINED (include subject name).
4. Every entity in a claim MUST appear in entities list.
5. source_quote MUST be a verbatim substring from the source.
6. Return ONLY the JSON object — no markdown, no preamble.
7. preference = implicit user preferences (food, habits, communication style).
8. event_date = ISO date when the event happened, NOT today. Null if unknown.
9. Conversation sources: always create entity "User" (entity_type: person) for the human.
10. Knowledge updates: extract both old claim (confidence 0.6) and new claim (confidence 0.9) with self-contained statements.
11. predicate (optional): emit only when a deterministic engine can re-verify. regex for text patterns, jsonpath for config values. OMIT if vague."#;

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
            lines.push(format!(
                "Field types: [{}]",
                metadata.field_types.join(", ")
            ));
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
        assert!(
            build_ast_anchor_section(&meta).is_empty(),
            "empty metadata must produce empty anchor"
        );
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
        assert!(
            section.to_lowercase().contains("exact")
                || section.contains("MUST")
                || section.contains("must"),
            "anchor must instruct LLM to use exact entity names"
        );
    }

    #[test]
    fn system_prompt_is_under_600_tokens() {
        let estimated_tokens = crate::prompts::SYSTEM_PROMPT.len() / 4;
        assert!(
            estimated_tokens <= 600,
            "SYSTEM_PROMPT is {estimated_tokens} tokens — must be ≤600. Trim examples, keep rules+schema."
        );
    }
}
