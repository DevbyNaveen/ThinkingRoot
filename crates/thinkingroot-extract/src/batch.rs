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
/// Instructs the LLM to return ONE JSON object with a `results` array wrapper.
pub fn build_batch_prompt(chunks: &[BatchChunk], known_entities_section: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Show the required output format FIRST — before any content.
    // This overrides the LLM's prior for single-chunk extraction format.
    let chunk_ids: Vec<String> = chunks.iter().map(|c| c.id.to_string()).collect();
    parts.push(format!(
        "You will extract knowledge from {} document chunks.\n\
         \n\
         REQUIRED OUTPUT FORMAT — return EXACTLY ONE JSON object:\n\
         {{\"results\":[{{\"chunk_id\":0,\"claims\":[...],\"entities\":[...],\"relations\":[...]}},{{\"chunk_id\":1,...}}]}}\n\
         \n\
         RULES:\n\
         - Output ONE JSON object only. No multiple objects. No extra text.\n\
         - The top-level key MUST be \"results\" (array).\n\
         - Each element MUST have \"chunk_id\" matching the chunk's id: {}\n\
         - Do NOT create relations between entities from different chunks.\n\
         - Every chunk_id must appear in the results array.",
        chunks.len(),
        chunk_ids.join(", ")
    ));

    if !known_entities_section.is_empty() {
        parts.push(known_entities_section.to_string());
    }

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

    parts.push(format!(
        "\nRemember: return ONE JSON object with key \"results\" containing {} entries (chunk_ids: {}).",
        chunks.len(),
        chunk_ids.join(", ")
    ));

    parts.join("\n\n")
}

/// Parse the LLM response for a batch call.
///
/// Returns one `BatchChunkResult` per expected_id.
/// Missing chunks → empty ExtractionResult (never fails the whole batch).
/// Malformed JSON → empty results for ALL expected_ids.
///
/// Fallback: if the LLM ignores the wrapper format and returns N separate
/// JSON objects (one per chunk), we detect that and assign them in order.
pub fn parse_batch_response(response: &str, expected_ids: &[usize]) -> Vec<BatchChunkResult> {
    let text = strip_fences(response);

    // ── Primary path: wrapped {"results":[...]} format ───────────────────────
    if let Ok(parsed) = serde_json::from_str::<BatchResponse>(text) {
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
        return expected_ids
            .iter()
            .map(|&id| BatchChunkResult {
                id,
                result: map.remove(&id).unwrap_or_else(ExtractionResult::empty),
            })
            .collect();
    }

    // ── Fallback: LLM returned multiple separate JSON objects ─────────────────
    // Some models return one {"claims":[...],"entities":[...],"relations":[...]}
    // per chunk instead of the wrapper. Detect by splitting on `}\n{` boundaries
    // and parse each object as a single-chunk ExtractionResult, assigned in order.
    let objects = split_json_objects(text);
    if !objects.is_empty() && objects.len() <= expected_ids.len() {
        tracing::debug!(
            "batch: LLM returned {} separate JSON objects instead of wrapper — using fallback parser",
            objects.len()
        );
        let mut results: Vec<BatchChunkResult> = Vec::with_capacity(expected_ids.len());
        for (i, &id) in expected_ids.iter().enumerate() {
            let result = objects
                .get(i)
                .and_then(|obj| serde_json::from_str::<ExtractionResult>(obj).ok())
                .unwrap_or_else(ExtractionResult::empty);
            results.push(BatchChunkResult { id, result });
        }
        return results;
    }

    // ── Total failure: return empty for all ──────────────────────────────────
    tracing::warn!(
        "batch response parse failed — could not parse as wrapper or {} separate objects, returning empty for all chunks",
        expected_ids.len()
    );
    expected_ids
        .iter()
        .map(|&id| BatchChunkResult {
            id,
            result: ExtractionResult::empty(),
        })
        .collect()
}

/// Strip markdown code fences from LLM output.
fn strip_fences(text: &str) -> &str {
    text.trim()
        .strip_prefix("```json")
        .or_else(|| text.trim().strip_prefix("```"))
        .unwrap_or(text.trim())
        .trim_start()
        .trim_end_matches("```")
        .trim()
}

/// Split a string containing multiple top-level JSON objects into individual
/// object strings. Handles the case where an LLM returns N objects separated
/// by whitespace/newlines instead of a single wrapper array.
fn split_json_objects(text: &str) -> Vec<&str> {
    let mut objects: Vec<&str> = Vec::new();
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut start: Option<usize> = None;
    let mut in_string = false;
    let mut escape_next = false;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if escape_next {
            escape_next = false;
            i += 1;
            continue;
        }
        match b {
            b'\\' if in_string => escape_next = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = start
                {
                    objects.push(&text[s..=i]);
                    start = None;
                }
            }
            _ => {}
        }
        i += 1;
    }
    objects
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
        assert!(
            prompt.contains("<chunk id=\"0\""),
            "must contain chunk 0 tag"
        );
        assert!(
            prompt.contains("<chunk id=\"1\""),
            "must contain chunk 1 tag"
        );
        assert!(prompt.contains("fn main()"), "must contain chunk 0 content");
        assert!(
            prompt.contains("struct Foo"),
            "must contain chunk 1 content"
        );
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
        assert!(
            prompt.contains("KNOWN_ENTITIES"),
            "must embed known entities section"
        );
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
        assert!(
            prompt.contains("validate"),
            "must include ast anchor content"
        );
        assert!(
            prompt.contains("decode"),
            "must include called function name"
        );
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
        let response =
            r#"{"results": [{"chunk_id": 0, "claims": [], "entities": [], "relations": []}]}"#;
        let results = parse_batch_response(response, &[0, 1]);
        assert_eq!(results.len(), 2, "must return entry for every expected id");
        let r1 = results.iter().find(|r| r.id == 1).unwrap();
        assert!(
            r1.result.claims.is_empty(),
            "missing chunk must return empty result"
        );
    }

    #[test]
    fn parse_batch_response_falls_back_on_malformed_json() {
        let results = parse_batch_response("this is not json", &[0, 1]);
        assert_eq!(
            results.len(),
            2,
            "must return empty results for all ids on failure"
        );
        assert!(results[0].result.claims.is_empty());
        assert!(results[1].result.claims.is_empty());
    }

    #[test]
    fn parse_batch_response_fallback_handles_separate_json_objects() {
        // LLM ignored wrapper and returned two separate JSON objects
        let response = r#"{"claims":[{"statement":"Rust is fast","claim_type":"fact","confidence":0.9,"entities":["Rust"],"source_quote":"Rust is fast","event_date":null}],"entities":[{"name":"Rust","entity_type":"concept","aliases":[],"description":null}],"relations":[]}
{"claims":[{"statement":"Go is simple","claim_type":"fact","confidence":0.85,"entities":["Go"],"source_quote":"Go is simple","event_date":null}],"entities":[{"name":"Go","entity_type":"concept","aliases":[],"description":null}],"relations":[]}"#;
        let results = parse_batch_response(response, &[0, 1]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, 0);
        assert_eq!(results[0].result.claims[0].statement, "Rust is fast");
        assert_eq!(results[1].id, 1);
        assert_eq!(results[1].result.claims[0].statement, "Go is simple");
    }

    #[test]
    fn split_json_objects_handles_two_objects() {
        let text = r#"{"a":1}{"b":2}"#;
        let objects = split_json_objects(text);
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0], r#"{"a":1}"#);
        assert_eq!(objects[1], r#"{"b":2}"#);
    }

    #[test]
    fn build_batch_prompt_contains_required_output_format_instruction() {
        let chunks = vec![BatchChunk {
            id: 0,
            content: "fn main() {}".into(),
            context: "Source: main.rs".into(),
            ast_anchor: String::new(),
        }];
        let prompt = build_batch_prompt(&chunks, "");
        assert!(
            prompt.contains("\"results\""),
            "prompt must instruct LLM to use results key"
        );
        assert!(
            prompt.contains("chunk_id"),
            "prompt must mention chunk_id field"
        );
        assert!(
            prompt.contains("ONE JSON"),
            "prompt must emphasize single output object"
        );
    }
}
