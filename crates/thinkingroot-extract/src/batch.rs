use serde::Deserialize;

use crate::schema::{ExtractedClaim, ExtractedEntity, ExtractedRelation, ExtractionResult};

/// One chunk in a batch request sent to the LLM.
#[derive(Debug, Clone)]
pub struct BatchChunk {
    /// Stable ID used to match output back to input (index in the batch vec).
    pub id: usize,
    /// The chunk content to extract from.
    pub content: String,
    /// Metadata context string e.g. "Source: foo.rs, Language: rust, Section: auth"
    pub context: String,
    /// AST anchor + graph-primed context (may be empty).
    pub ast_anchor: String,
}

/// One chunk's extracted results, keyed back to its BatchChunk.id.
#[derive(Debug, Clone)]
pub struct BatchChunkResult {
    pub id: usize,
    pub result: ExtractionResult,
}

// ── Serde types for parsing batch LLM response ───────────────────────────────

#[derive(Debug, Deserialize)]
struct BatchResponse {
    results: Vec<BatchResultEntry>,
}

#[derive(Debug, Deserialize)]
struct BatchResultEntry {
    chunk_id: usize,
    #[serde(default)]
    claims: Vec<ExtractedClaim>,
    #[serde(default)]
    entities: Vec<ExtractedEntity>,
    #[serde(default)]
    relations: Vec<ExtractedRelation>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Build the user prompt for a batch of chunks.
///
/// Each chunk is wrapped in `<chunk id="N" context="...">` tags.
/// The known_entities_section (graph context) is prepended once and shared.
/// Instructs the LLM NOT to create relations between entities from different chunks.
pub fn build_batch_prompt(chunks: &[BatchChunk], known_entities_section: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    parts.push(
        "Extract knowledge from each chunk below. For EACH chunk, return its results independently under its chunk_id.\n".to_string(),
    );

    if !known_entities_section.is_empty() {
        parts.push(format!("{known_entities_section}\n"));
    }

    parts.push(
        "Return JSON matching this exact schema:\n\
         {\"results\":[{\"chunk_id\":0,\"claims\":[...],\"entities\":[...],\"relations\":[...]}]}\n\n\
         Do NOT create relations between entities from different chunks.\n"
            .to_string(),
    );

    for chunk in chunks {
        let mut chunk_parts: Vec<String> = Vec::new();
        chunk_parts.push(format!(
            "<chunk id=\"{}\" context=\"{}\">",
            chunk.id, chunk.context
        ));
        if !chunk.ast_anchor.is_empty() {
            chunk_parts.push(chunk.ast_anchor.clone());
        }
        chunk_parts.push(chunk.content.clone());
        chunk_parts.push("</chunk>".to_string());
        parts.push(chunk_parts.join("\n"));
    }

    parts.join("\n")
}

/// Parse the LLM response for a batch call.
///
/// Returns one `BatchChunkResult` per expected_id.
/// Missing chunks → empty ExtractionResult (never fails the whole batch).
/// Malformed JSON → empty results for ALL expected_ids.
pub fn parse_batch_response(response: &str, expected_ids: &[usize]) -> Vec<BatchChunkResult> {
    // Strip markdown fences if present (same logic as the single-chunk parser).
    let text = response.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text)
        .trim_start()
        .trim_end_matches("```")
        .trim();

    let parsed: BatchResponse = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "batch response parse failed ({e}) — returning empty for all {} chunks",
                expected_ids.len()
            );
            return expected_ids
                .iter()
                .map(|&id| BatchChunkResult {
                    id,
                    result: ExtractionResult::empty(),
                })
                .collect();
        }
    };

    // Map chunk_id → ExtractionResult.
    let mut map: std::collections::HashMap<usize, ExtractionResult> = parsed
        .results
        .into_iter()
        .map(|entry| {
            (
                entry.chunk_id,
                ExtractionResult {
                    claims: entry.claims,
                    entities: entry.entities,
                    relations: entry.relations,
                },
            )
        })
        .collect();

    // Return one entry per expected id — fill with empty if missing.
    expected_ids
        .iter()
        .map(|&id| BatchChunkResult {
            id,
            result: map.remove(&id).unwrap_or_else(ExtractionResult::empty),
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_batch_prompt_contains_chunk_tags() {
        let chunks = vec![
            BatchChunk {
                id: 0,
                content: "fn main() {}".into(),
                context: "Source: main.rs, Language: rust".into(),
                ast_anchor: String::new(),
            },
            BatchChunk {
                id: 1,
                content: "struct Foo {}".into(),
                context: "Source: foo.rs, Language: rust".into(),
                ast_anchor: String::new(),
            },
        ];
        let prompt = build_batch_prompt(&chunks, "");
        assert!(prompt.contains("<chunk id=\"0\""), "must contain chunk 0 tag");
        assert!(prompt.contains("<chunk id=\"1\""), "must contain chunk 1 tag");
        assert!(prompt.contains("fn main()"), "must contain chunk 0 content");
        assert!(prompt.contains("struct Foo"), "must contain chunk 1 content");
    }

    #[test]
    fn build_batch_prompt_includes_known_entities() {
        let chunks = vec![BatchChunk {
            id: 0,
            content: "fn auth() {}".into(),
            context: "Source: auth.rs".into(),
            ast_anchor: String::new(),
        }];
        let known = "## KNOWN_ENTITIES\n- AuthService (service)";
        let prompt = build_batch_prompt(&chunks, known);
        assert!(prompt.contains("KNOWN_ENTITIES"), "must embed known entities section");
    }

    #[test]
    fn build_batch_prompt_includes_ast_anchor() {
        let chunks = vec![BatchChunk {
            id: 0,
            content: "fn validate() {}".into(),
            context: "Source: auth.rs".into(),
            ast_anchor: "Function: validate\nCalls: [decode]".into(),
        }];
        let prompt = build_batch_prompt(&chunks, "");
        assert!(prompt.contains("validate"), "must include ast anchor content");
        assert!(prompt.contains("decode"), "must include called function name");
    }

    #[test]
    fn parse_batch_response_extracts_per_chunk_results() {
        let response = r#"{
  "results": [
    {
      "chunk_id": 0,
      "claims": [{"statement": "Rust is fast", "claim_type": "fact", "confidence": 0.9, "entities": ["Rust"], "source_quote": "Rust is fast", "event_date": null}],
      "entities": [{"name": "Rust", "entity_type": "concept", "aliases": [], "description": null}],
      "relations": []
    },
    {
      "chunk_id": 1,
      "claims": [{"statement": "Foo is a struct", "claim_type": "fact", "confidence": 0.95, "entities": ["Foo"], "source_quote": "struct Foo", "event_date": null}],
      "entities": [{"name": "Foo", "entity_type": "module", "aliases": [], "description": null}],
      "relations": []
    }
  ]
}"#;
        let results = parse_batch_response(response, &[0, 1]);
        assert_eq!(results.len(), 2);
        let r0 = results.iter().find(|r| r.id == 0).unwrap();
        assert_eq!(r0.result.claims[0].statement, "Rust is fast");
        let r1 = results.iter().find(|r| r.id == 1).unwrap();
        assert_eq!(r1.result.claims[0].statement, "Foo is a struct");
    }

    #[test]
    fn parse_batch_response_missing_chunk_returns_empty() {
        let response = r#"{"results": [{"chunk_id": 0, "claims": [], "entities": [], "relations": []}]}"#;
        let results = parse_batch_response(response, &[0, 1]);
        assert_eq!(results.len(), 2, "must return entry for every expected id");
        let r1 = results.iter().find(|r| r.id == 1).unwrap();
        assert!(r1.result.claims.is_empty(), "missing chunk must return empty result");
    }

    #[test]
    fn parse_batch_response_falls_back_on_malformed_json() {
        let results = parse_batch_response("this is not json", &[0, 1]);
        assert_eq!(results.len(), 2, "must return empty results for all ids on failure");
        assert!(results[0].result.claims.is_empty());
        assert!(results[1].result.claims.is_empty());
    }
}
