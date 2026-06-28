//! Per-document **brief** — a human TITLE + a one-line SUMMARY produced in a
//! SINGLE LLM call during enrichment.
//!
//! Why one call: the title and the summary are the same reasoning ("what is this
//! document, in a sentence"), so asking once gives a free title with no extra
//! latency or token cost over the summary we already wanted. Grounded in the
//! source's extracted facts (preferred) plus a short verbatim preview of its
//! first chunk (fallback when facts are sparse), so the title reflects the actual
//! content, never a hallucination.
//!
//! The LLM I/O is the only non-deterministic part; [`parse_brief`] is pure and
//! unit-tested. An LLM error degrades gracefully to a deterministic title derived
//! from the filename — the enrichment never blocks on this stage.

use thinkingroot_core::types::AtomicFact;
use thinkingroot_llm::llm::LlmClient;

/// The title + summary for one document.
#[derive(Debug, Clone, Default)]
pub struct DocBrief {
    pub title: String,
    pub summary: String,
}

/// System prompt: one compact JSON object, `{title, summary}`.
pub fn brief_system() -> String {
    "You write a concise TITLE and a one-sentence SUMMARY for a document, given a \
few grounded facts extracted from it (and optionally a short verbatim excerpt). \
Return ONLY a JSON object with exactly two string fields:\n\
- `title`: a short, specific, human title for the document (3–8 words). Capitalize \
like a headline. Do NOT include the file extension or quotes. If a real name/topic \
is evident, use it (e.g. \"Orion Labs Q3 Launch Plan\").\n\
- `summary`: ONE sentence (<= 30 words) stating what the document is about, grounded \
strictly in the provided facts/excerpt. Do not speculate beyond them.\n\
Output ONLY the JSON object, no markdown fences, no commentary."
        .to_string()
}

/// Build the user prompt from the doc's uri, its top facts, and a short preview.
pub fn build_brief_prompt(uri: &str, facts: &[AtomicFact], preview: &str) -> String {
    // The filename is a strong title hint; pass it explicitly.
    let filename = uri.rsplit(['/', '\\']).next().unwrap_or(uri);
    // Bound the fact list + preview so a huge doc can't blow the prompt.
    let facts_block = facts
        .iter()
        .take(20)
        .map(|f| format!("- {}", f.statement.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    let preview_block: String = preview.chars().take(600).collect();
    format!(
        "Filename: {filename}\n\nExtracted facts:\n{facts}\n\nExcerpt (verbatim, may be empty):\n{preview}\n\nReturn the JSON object now:",
        filename = filename,
        facts = if facts_block.is_empty() { "(none)".to_string() } else { facts_block },
        preview = preview_block,
    )
}

/// A deterministic title from the filename — the graceful fallback when the LLM
/// is unavailable or returns nothing usable. Strips the extension, turns
/// separators into spaces, and title-cases.
pub fn title_from_uri(uri: &str) -> String {
    let filename = uri.rsplit(['/', '\\']).next().unwrap_or(uri);
    let stem = filename.rsplit_once('.').map(|(a, _)| a).unwrap_or(filename);
    let spaced = stem.replace(['_', '-', '.'], " ");
    let words: Vec<String> = spaced
        .split_whitespace()
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
                None => String::new(),
            }
        })
        .collect();
    let t = words.join(" ");
    if t.trim().is_empty() {
        "Untitled document".to_string()
    } else {
        t
    }
}

/// Parse the model response into a [`DocBrief`], tolerating ```json fences and
/// stray prose. Falls back to `uri`-derived title / empty summary on failure.
pub fn parse_brief(resp: &str, uri: &str) -> DocBrief {
    let obj: Option<serde_json::Value> = extract_json_object(resp)
        .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok());
    let mut title = obj
        .as_ref()
        .and_then(|o| o.get("title"))
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default();
    let summary = obj
        .as_ref()
        .and_then(|o| o.get("summary"))
        .and_then(|t| t.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if title.is_empty() {
        title = title_from_uri(uri);
    }
    // Keep the title sane in length (a runaway model can't bloat the node).
    if title.chars().count() > 120 {
        title = title.chars().take(120).collect();
    }
    DocBrief { title, summary }
}

/// Pull the first top-level `{ ... }` JSON object out of a model response.
fn extract_json_object(resp: &str) -> Option<String> {
    let start = resp.find('{')?;
    let end = resp.rfind('}')?;
    if end > start {
        Some(resp[start..=end].to_string())
    } else {
        None
    }
}

/// Generate a doc brief in ONE LLM call. On any LLM error (or no signal at all)
/// returns a deterministic filename-derived title with an empty summary, so the
/// caller always has a usable title and never stalls.
pub async fn generate_doc_brief(
    llm: &LlmClient,
    uri: &str,
    facts: &[AtomicFact],
    preview: &str,
) -> DocBrief {
    // No grounding at all → don't spend an LLM call; use the filename title.
    if facts.is_empty() && preview.trim().is_empty() {
        return DocBrief { title: title_from_uri(uri), summary: String::new() };
    }
    let system = brief_system();
    let prompt = build_brief_prompt(uri, facts, preview);
    match llm.chat(&system, &prompt).await {
        Ok(resp) => parse_brief(&resp, uri),
        Err(e) => {
            tracing::warn!("doc brief LLM failed ({e}); using filename-derived title");
            DocBrief { title: title_from_uri(uri), summary: String::new() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_uri_strips_ext_and_titlecases() {
        assert_eq!(title_from_uri("/work/orion-spec.md"), "Orion Spec");
        assert_eq!(title_from_uri("nova_company.md"), "Nova Company");
        assert_eq!(title_from_uri("Gemini-2.5-Pro.txt"), "Gemini 2 5 Pro");
    }

    #[test]
    fn title_from_uri_handles_empty() {
        assert_eq!(title_from_uri(""), "Untitled document");
    }

    #[test]
    fn parse_brief_reads_json() {
        let b = parse_brief(
            r#"{"title":"Orion Labs Q3 Plan","summary":"A launch plan for Orion Labs."}"#,
            "x.md",
        );
        assert_eq!(b.title, "Orion Labs Q3 Plan");
        assert!(b.summary.starts_with("A launch plan"));
    }

    #[test]
    fn parse_brief_tolerates_fences() {
        let b = parse_brief(
            "```json\n{\"title\":\"Hello World\",\"summary\":\"Greeting.\"}\n```",
            "x.md",
        );
        assert_eq!(b.title, "Hello World");
        assert_eq!(b.summary, "Greeting.");
    }

    #[test]
    fn parse_brief_falls_back_to_filename_title() {
        let b = parse_brief("not json at all", "/work/my-doc.md");
        assert_eq!(b.title, "My Doc");
        assert!(b.summary.is_empty());
    }

    fn fact(statement: &str) -> AtomicFact {
        AtomicFact {
            id: "af:test".to_string(),
            source_id: "src:1".to_string(),
            chunk_id: "rc:1".to_string(),
            subject: String::new(),
            predicate: String::new(),
            object: String::new(),
            statement: statement.to_string(),
            confidence: 1.0,
            extraction_model: String::new(),
            workspace_id: String::new(),
            sensitivity: "Public".to_string(),
            byte_start: 0,
            byte_end: 0,
            content_blake3: String::new(),
            valid_from: 0.0,
            valid_until: -1.0,
            created_at: 0.0,
        }
    }

    #[test]
    fn build_prompt_includes_filename_and_facts() {
        let f = fact("Orion Labs launched a rover.");
        let p = build_brief_prompt("/w/orion.md", std::slice::from_ref(&f), "");
        assert!(p.contains("orion.md"));
        assert!(p.contains("Orion Labs launched a rover."));
    }
}
