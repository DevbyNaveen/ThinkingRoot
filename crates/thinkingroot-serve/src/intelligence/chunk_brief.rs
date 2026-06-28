//! Per-chunk **memory brief** — a one-sentence, plain-English summary of a
//! passage, produced in a SINGLE batched LLM call per source.
//!
//! In the Console's Memory Graph a "memory" = a chunk's facts grouped together.
//! Phase 1 headlines each memory with its lead fact's statement (free). This
//! module is Phase 2: a consolidated one-liner ("This passage describes the
//! durability model for Root Functions…") so each memory reads as a sentence,
//! not a raw fact. One LLM call summarises every chunk of a source at once
//! (bounded into batches); a chunk with no usable summary falls back to its lead
//! fact, so the pipeline never blocks on this stage.

use thinkingroot_llm::llm::LlmClient;

/// One passage to summarise: its id, the facts extracted from it, and a short
/// verbatim excerpt (grounding when facts are sparse).
#[derive(Debug, Clone)]
pub struct ChunkBriefInput {
    pub chunk_id: String,
    pub facts: Vec<String>,
    pub preview: String,
}

/// Bound the prompt: summarise at most this many chunks per LLM call.
const BATCH: usize = 12;

fn system() -> String {
    "You write a ONE-sentence, plain-English summary for each passage of a document, \
given a few facts extracted from it (and a short excerpt). Return ONLY a JSON array — \
one object per input item, in the SAME order — each object exactly {\"id\": string, \
\"summary\": string}. Copy `id` verbatim so we can align your answer. The summary must \
be a single declarative sentence (<= 28 words), grounded strictly in the provided facts/\
excerpt, no speculation. No markdown, no commentary — only the JSON array."
        .to_string()
}

fn build_prompt(batch: &[ChunkBriefInput]) -> String {
    let arr: Vec<serde_json::Value> = batch
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.chunk_id,
                "facts": c.facts.iter().take(8).cloned().collect::<Vec<_>>(),
                "excerpt": c.preview.chars().take(400).collect::<String>(),
            })
        })
        .collect();
    let json = serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string());
    format!("Passages:\n{json}\n\nSummaries (JSON array, same order):")
}

/// Pull the first top-level `[ ... ]` JSON array out of a model response.
fn extract_array(resp: &str) -> Option<String> {
    let start = resp.find('[')?;
    let end = resp.rfind(']')?;
    if end > start {
        Some(resp[start..=end].to_string())
    } else {
        None
    }
}

/// Deterministic fallback summary for one passage when the LLM gives us nothing
/// usable: the lead fact, else the first sentence of the excerpt.
fn fallback(c: &ChunkBriefInput) -> String {
    if let Some(f) = c.facts.first() {
        if !f.trim().is_empty() {
            return f.trim().to_string();
        }
    }
    let s = c.preview.trim();
    let cut = s.find(['.', '\n']).map(|i| i + 1).unwrap_or(s.len().min(160));
    s[..cut.min(s.len())].trim().to_string()
}

/// Parse one batch's response into `(chunk_id → summary)`, aligned by id, with a
/// per-item deterministic fallback for anything the model dropped/mangled.
fn parse_batch(resp: &str, batch: &[ChunkBriefInput]) -> Vec<(String, String)> {
    use std::collections::BTreeMap;
    let arr: Vec<serde_json::Value> = extract_array(resp)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let mut by_id: BTreeMap<String, String> = BTreeMap::new();
    for v in &arr {
        if let (Some(id), Some(sum)) = (
            v.get("id").and_then(|x| x.as_str()),
            v.get("summary").and_then(|x| x.as_str()),
        ) {
            let sum = sum.trim();
            if !sum.is_empty() {
                by_id.insert(id.to_string(), sum.to_string());
            }
        }
    }
    batch
        .iter()
        .map(|c| {
            let sum = by_id.get(&c.chunk_id).cloned().unwrap_or_else(|| fallback(c));
            (c.chunk_id.clone(), sum)
        })
        .collect()
}

/// Summarise every chunk of a source. One LLM call per batch of `BATCH`; on any
/// LLM error the whole batch falls back to lead-fact summaries. Returns
/// `(chunk_id, summary)` for every input.
pub async fn generate_chunk_briefs(
    llm: &LlmClient,
    inputs: &[ChunkBriefInput],
) -> Vec<(String, String)> {
    if inputs.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(inputs.len());
    for batch in inputs.chunks(BATCH) {
        let sys = system();
        let prompt = build_prompt(batch);
        match llm.chat(&sys, &prompt).await {
            Ok(resp) => out.extend(parse_batch(&resp, batch)),
            Err(e) => {
                tracing::warn!("chunk briefs LLM failed ({e}); using lead-fact fallback");
                out.extend(batch.iter().map(|c| (c.chunk_id.clone(), fallback(c))));
            }
        }
    }
    out
}

/// Layer 1 — CONTEXTUAL RETRIEVAL (Anthropic-style). Build the *embedding* text
/// for an atomic fact: a short situating prefix — the document title plus the
/// chunk's one-line memory brief — followed by the verbatim statement.
///
/// This lets a fact like "it is 768-dimensional" match a query about "the
/// ThinkingRoot embedder" even though the bare statement names neither, because
/// the chunk brief ("ThinkingRoot's embedder is gte-modernbert at 768 dims")
/// carries the missing referents. Only the embedded TEXT changes — the caller
/// keeps the stored statement, the vector key, and the `fact|` meta verbatim, so
/// a *retrieved* fact is still the exact quote.
///
/// Empty pieces are skipped so a missing brief/title never injects noise; with
/// neither, it degrades to the bare statement (no regression vs. the old path).
pub fn contextualize_fact_text(title: &str, brief: Option<&str>, statement: &str) -> String {
    let title = title.trim();
    let brief = brief.map(str::trim).filter(|s| !s.is_empty());
    let mut ctx = String::new();
    if !title.is_empty() {
        ctx.push_str(title);
    }
    if let Some(b) = brief {
        // Skip a brief that just restates this exact fact (the lead-fact fallback
        // can equal the statement) — it would add no situating context.
        if !b.eq_ignore_ascii_case(statement.trim()) {
            if !ctx.is_empty() {
                ctx.push_str(" — ");
            }
            ctx.push_str(b);
        }
    }
    if ctx.is_empty() {
        statement.to_string()
    } else {
        format!("{ctx}\n{statement}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contextualize_prepends_title_and_brief() {
        let t = contextualize_fact_text(
            "ThinkingRoot Architecture",
            Some("The embedder is gte-modernbert at 768 dimensions."),
            "It is 768-dimensional.",
        );
        assert_eq!(
            t,
            "ThinkingRoot Architecture — The embedder is gte-modernbert at 768 dimensions.\nIt is 768-dimensional."
        );
        // The verbatim statement is always the last line (retrieval returns it intact).
        assert!(t.ends_with("It is 768-dimensional."));
    }

    #[test]
    fn contextualize_handles_missing_pieces_without_noise() {
        // No title → brief only.
        assert_eq!(
            contextualize_fact_text("", Some("A passage about X."), "X happened."),
            "A passage about X.\nX happened."
        );
        // No brief → title only.
        assert_eq!(
            contextualize_fact_text("Doc Title", None, "X happened."),
            "Doc Title\nX happened."
        );
        // Neither → bare statement (no regression, no stray separators).
        assert_eq!(contextualize_fact_text("  ", Some("  "), "X happened."), "X happened.");
    }

    #[test]
    fn contextualize_skips_brief_that_equals_statement() {
        // The lead-fact fallback can make the brief == the statement; don't duplicate it.
        let t = contextualize_fact_text("Doc", Some("X happened."), "X happened.");
        assert_eq!(t, "Doc\nX happened.");
    }

    fn inp(id: &str, facts: &[&str], preview: &str) -> ChunkBriefInput {
        ChunkBriefInput {
            chunk_id: id.to_string(),
            facts: facts.iter().map(|s| s.to_string()).collect(),
            preview: preview.to_string(),
        }
    }

    #[test]
    fn parse_aligns_by_id() {
        let batch = vec![inp("rc:1", &["Orion launched a rover."], ""), inp("rc:2", &["Berlin is the HQ."], "")];
        let resp = r#"[{"id":"rc:1","summary":"Orion launched a rover this quarter."},
                       {"id":"rc:2","summary":"The company is headquartered in Berlin."}]"#;
        let got = parse_batch(resp, &batch);
        assert_eq!(got[0].0, "rc:1");
        assert!(got[0].1.contains("rover"));
        assert!(got[1].1.contains("Berlin"));
    }

    #[test]
    fn parse_falls_back_on_missing() {
        let batch = vec![inp("rc:1", &["Lead fact here."], "Excerpt text.")];
        let got = parse_batch("not json", &batch);
        assert_eq!(got[0].0, "rc:1");
        assert_eq!(got[0].1, "Lead fact here.");
    }

    #[test]
    fn parse_tolerates_fences_and_drops() {
        let batch = vec![inp("rc:1", &["A."], ""), inp("rc:2", &["B fact."], "")];
        // fenced + second item dropped by the model
        let resp = "```json\n[{\"id\":\"rc:1\",\"summary\":\"Summary one.\"}]\n```";
        let got = parse_batch(resp, &batch);
        assert_eq!(got[0].1, "Summary one.");
        assert_eq!(got[1].1, "B fact."); // fallback
    }

    #[test]
    fn fallback_uses_excerpt_when_no_facts() {
        let c = inp("rc:9", &[], "This passage explains the model. More text.");
        assert!(fallback(&c).starts_with("This passage explains the model."));
    }
}
