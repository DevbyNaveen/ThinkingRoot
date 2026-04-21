use crate::graph_context::GraphPrimedContext;

/// Build focused prompts for entity extraction.
///
/// Returns `(system_prompt, user_prompt)`.
pub fn build_entity_extraction_prompt(
    content: &str,
    context: &str,
    graph_ctx: &GraphPrimedContext,
) -> (String, String) {
    let system = r#"You are a named-entity extraction engine for ThinkingRoot, a knowledge compiler.
Your job is to find and classify every named entity in the provided content.

You MUST return valid JSON matching this exact schema:

{
  "entities": [
    {
      "name": "Canonical name",
      "entity_type": "person|system|service|concept|team|api|database|library|file|module|function|config|organization",
      "aliases": ["alternate names or abbreviations"],
      "description": "Brief description of this entity"
    }
  ]
}

Rules:
1. Extract ALL named entities — people, systems, services, concepts, teams, APIs, databases, libraries, files, modules, functions, configs, and organizations.
2. If a KNOWN_ENTITIES section is provided, PREFER matching those exact names over creating new ones. Only create a new entity for concepts not already represented.
3. Canonical names should be the most common or formal form of the name.
4. aliases captures alternate spellings, abbreviations, or acronyms.
5. Return ONLY the JSON object. No markdown, no explanation, no preamble."#;

    let known_section = graph_ctx.prompt_section();
    let known_block = if known_section.is_empty() {
        String::new()
    } else {
        format!("\n\n{known_section}")
    };

    let user = format!(
        "Extract all named entities from the following content.\n\nContext: {context}{known_block}\n\n---\n\n{content}\n\n---\n\nReturn the JSON extraction result."
    );

    (system.to_string(), user)
}

/// Build focused prompts for relation extraction.
///
/// `entity_names` is the list of canonical entity names found in the entity
/// extraction pass — relations must only reference names from this list.
///
/// Returns `(system_prompt, user_prompt)`.
pub fn build_relation_extraction_prompt(
    content: &str,
    context: &str,
    entity_names: &[String],
) -> (String, String) {
    let system = r#"You are a relation extraction engine for ThinkingRoot, a knowledge compiler.
Your job is to find relationships between entities in the provided content.

You MUST return valid JSON matching this exact schema:

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
1. Only use entity names that appear in the provided ENTITIES list. Do NOT invent new entity names.
2. Every relation must have both from_entity and to_entity drawn from the ENTITIES list.
3. Choose the most specific relation_type that accurately describes the relationship.
4. description should be concise and self-contained.
5. Return ONLY the JSON object. No markdown, no explanation, no preamble."#;

    let entities_block = build_entities_section(entity_names);

    let user = format!(
        "Extract all relationships between entities from the following content.\n\nContext: {context}\n\n{entities_block}\n\n---\n\n{content}\n\n---\n\nReturn the JSON extraction result."
    );

    (system.to_string(), user)
}

/// Build focused prompts for claim extraction.
///
/// `entity_names` is the list of canonical entity names found in the entity
/// extraction pass so the model can link claims to known entities.
///
/// Returns `(system_prompt, user_prompt)`.
pub fn build_claim_extraction_prompt(
    content: &str,
    context: &str,
    entity_names: &[String],
) -> (String, String) {
    let system = r#"You are a factual claim extraction engine for ThinkingRoot, a knowledge compiler.
Your job is to extract structured, atomic claims from the provided content.

You MUST return valid JSON matching this exact schema:

{
  "claims": [
    {
      "statement": "A clear, atomic statement of fact or decision",
      "claim_type": "fact|decision|opinion|plan|requirement|metric|definition|dependency|api_signature|architecture",
      "confidence": 0.0,
      "entities": ["entity names mentioned in this claim"],
      "source_quote": "The exact verbatim phrase or sentence from the source that supports this claim",
      "predicate": {
        "language": "regex|rust_ast|jsonpath",
        "query": "pattern that will still match in the source if this claim is still true",
        "scope_globs": []
      }
    }
  ]
}

Rules:
1. Claims must be ATOMIC — one fact per claim. Do not combine multiple facts.
2. Claims must be SELF-CONTAINED — understandable without reading the source.
3. Confidence reflects how certain the source is: 0.5 = implied, 0.8 = stated, 0.95 = definitive.
4. entities should only include names from the provided ENTITIES list.
5. source_quote MUST be a verbatim substring copied from the source. Do NOT paraphrase.
6. Do NOT fabricate information. Extract only what is explicitly stated or clearly implied.
7. Return ONLY the JSON object. No markdown, no explanation, no preamble.

Predicate rules (optional — emit only when confident):
8. `predicate` is OPTIONAL. Omit it (`null` or absent) when you cannot write one
   that a deterministic engine can verify against the source bytes.
9. Use `"regex"` when the claim maps to a recognizable text pattern in the
   source. Write a regex that matches only if the claim is still true.
   Example: claim "AuthService exposes validate_token" →
            predicate {language:"regex", query:"fn\\s+validate_token\\b"}
10. Use `"jsonpath"` for claims about JSON/YAML config values.
    Example: claim "rate limit is 100 req/sec" →
             predicate {language:"jsonpath", query:"$.rate_limit[?(@ == 100)]"}
11. Use `"rust_ast"` for claims about Rust code structure (Week 4+).
12. `scope_globs` restricts where the predicate runs. Leave empty to run
    against the claim's own source — the common case.
13. If your predicate would be vague, ambiguous, or match unrelated text,
    OMIT it. A missing predicate is safer than a noisy one."#;

    let entities_block = build_entities_section(entity_names);

    let user = format!(
        "Extract all factual claims from the following content.\n\nContext: {context}\n\n{entities_block}\n\n---\n\n{content}\n\n---\n\nReturn the JSON extraction result."
    );

    (system.to_string(), user)
}

/// Build an `<ENTITIES>` XML section listing the known entity names.
///
/// Returns an empty string when `entity_names` is empty so callers can
/// detect the empty case without extra logic.
fn build_entities_section(entity_names: &[String]) -> String {
    if entity_names.is_empty() {
        return String::new();
    }

    let mut lines = vec!["<ENTITIES>".to_string()];
    lines.push(
        "Only use entity names from the following list when filling in from_entity / to_entity \
fields or the entities array of a claim."
            .to_string(),
    );
    lines.push(String::new());
    for name in entity_names {
        lines.push(format!("- {name}"));
    }
    lines.push("</ENTITIES>".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_context::{GraphPrimedContext, KnownEntity};

    #[test]
    fn entity_prompt_includes_known_entities() {
        let ctx = GraphPrimedContext::new(vec![KnownEntity {
            name: "GraphStore".to_string(),
            entity_type: "system".to_string(),
        }]);
        let (_sys, user) = build_entity_extraction_prompt("some content", "Source: test.rs", &ctx);
        assert!(
            user.contains("KNOWN_ENTITIES"),
            "user prompt should contain KNOWN_ENTITIES tag"
        );
        assert!(
            user.contains("GraphStore"),
            "user prompt should contain known entity name"
        );
    }

    #[test]
    fn relation_prompt_includes_entity_list() {
        let names = vec!["GraphStore".to_string(), "Claim".to_string()];
        let (_sys, user) =
            build_relation_extraction_prompt("some content", "Source: test.rs", &names);
        assert!(
            user.contains("GraphStore"),
            "user prompt should contain entity name GraphStore"
        );
        assert!(
            user.contains("Claim"),
            "user prompt should contain entity name Claim"
        );
        assert!(
            user.contains("<ENTITIES>"),
            "user prompt should contain ENTITIES section"
        );
    }

    #[test]
    fn claim_prompt_includes_entity_list() {
        let names = vec!["AuthService".to_string(), "UserDB".to_string()];
        let (_sys, user) = build_claim_extraction_prompt("some content", "Source: auth.rs", &names);
        assert!(
            user.contains("AuthService"),
            "user prompt should contain entity AuthService"
        );
        assert!(
            user.contains("UserDB"),
            "user prompt should contain entity UserDB"
        );
        assert!(
            user.contains("<ENTITIES>"),
            "user prompt should contain ENTITIES section"
        );
    }

    #[test]
    fn empty_graph_context_omits_known_entities_section() {
        let ctx = GraphPrimedContext::new(vec![]);
        let (_sys, user) = build_entity_extraction_prompt("some content", "Source: test.rs", &ctx);
        assert!(
            !user.contains("KNOWN_ENTITIES"),
            "user prompt should NOT contain KNOWN_ENTITIES when context is empty"
        );
    }
}
