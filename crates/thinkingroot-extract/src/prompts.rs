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
      "relation_type": "depends_on|owned_by|replaces|contradicts|implements|uses|contains|created_by|part_of|related_to|calls|configured_by|tested_by",
      "description": "Brief description of the relationship"
    }
  ]
}

Rules:
1. Claims must be ATOMIC — one fact per claim. Do not combine multiple facts.
2. Claims must be SELF-CONTAINED — understandable without reading the source.
3. Every entity mentioned in a claim MUST appear in the entities list.
4. Confidence reflects how certain the source is (0.5=implied, 0.8=stated, 0.95=definitive).
5. Do NOT fabricate information. Extract only what is explicitly stated or clearly implied.
6. For code: extract function signatures, type definitions, dependencies, and architectural patterns.
7. For docs: extract decisions, requirements, facts, and relationships between concepts.
8. source_quote MUST be a verbatim substring copied from the source. Do NOT paraphrase.
9. Return ONLY the JSON object. No markdown, no explanation, no preamble."#;

/// Build the user prompt for a given chunk of content.
pub fn build_extraction_prompt(content: &str, context: &str) -> String {
    format!(
        "Extract knowledge from the following content.\n\nContext: {context}\n\n---\n\n{content}\n\n---\n\nReturn the JSON extraction result."
    )
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
