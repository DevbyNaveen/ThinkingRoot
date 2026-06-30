//! Phase 1c — **write-boundary entity DEFINE stage** (EDC stage 2).
//!
//! After [`super::entity_typing`] decides *which* candidates are real entities
//! and *what type* they are, this stage gives each kept entity a **one-sentence
//! definition** grounded only in the source it came from. A name alone is
//! ambiguous ("Nova" — a product? a company? a star?); the definition is the
//! semantic anchor that
//!   * makes the Neural-Graph node self-describing (the `entities.description`
//!     field, historically reserved-but-empty), and
//!   * powers SOTA entity **canonicalization**: the two-tier resolver's LLM-judge
//!     merge compares *definitions*, not bare names, so "Orion Labs" merges with
//!     "Orion Laboratory" while two different "Raj Patel"s stay separate.
//!
//! Like typing, this runs in the async enrichment queue, never inline, never
//! holding the storage lock, in bounded LLM batches. The LLM I/O is the only
//! non-deterministic part; the prompt builder and response parser are pure and
//! unit-tested. A missing/erroring LLM degrades gracefully to an **empty
//! definition** (the node is still created, just undescribed) so the pipeline
//! never blocks on this stage. Gated by `TR_ENTITY_DEFINE` (default on).

use thinkingroot_llm::llm::LlmClient;

/// A kept entity awaiting a definition: its canonical name plus a short grounding
/// context (a representative fact sentence it appears in).
#[derive(Debug, Clone)]
pub struct DefineCandidate {
    /// The canonical surface form (post-typing) to define and store.
    pub name: String,
    /// One representative fact sentence mentioning `name` (definition grounding).
    pub context: String,
}

/// Max chars kept for a stored definition — bounds graph bloat and keeps the
/// `entities.description` column tidy. A definition is a *sentence*, not a essay.
const MAX_DEF_LEN: usize = 240;

/// Max candidates per DEFINE LLM call. Same rationale as `entity_typing`'s
/// `TYPING_BATCH`: one call over hundreds of entities overruns the output-token
/// budget (truncation → parse failure → the whole source loses definitions).
const DEFINE_BATCH: usize = 40;

/// System prompt: produce a single grounded definition sentence per entity. The
/// model returns a JSON array aligned to the input order, one object per entity.
pub fn define_system() -> String {
    "You write one-sentence definitions for knowledge-graph entities. You are given \
a JSON array of entities, each with a `name` and a `context` sentence it appeared in. \
For EACH entity, output one object with:\n\
- `name`: the input name, copied EXACTLY (so we can align your answer).\n\
- `definition`: ONE concise plain-English sentence stating what the entity IS or DOES, \
grounded ONLY in the given context. State its kind and its most salient attribute (e.g. \
\"A high-performance query engine built by Orion Labs for concurrent index retrieval.\"). \
Do NOT speculate beyond the context, do NOT restate the name as a definition, do NOT add \
dates/numbers unless they appear in the context. If the context is too thin to define the \
entity, output an empty string.\n\
Rules: keep each definition under 30 words. Do not add commentary. Output ONLY a JSON array, \
same length and order as the input, no markdown fences."
        .to_string()
}

/// Build the user prompt: a compact JSON array of `{name, context}`.
pub fn build_define_prompt(candidates: &[DefineCandidate]) -> String {
    let arr: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                // Bound context length so a giant fact statement can't blow the prompt.
                "context": c.context.chars().take(240).collect::<String>(),
            })
        })
        .collect();
    let json = serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string());
    format!("Entities:\n{json}\n\nDefinitions (JSON array, same order):")
}

/// Parse the LLM response into per-candidate definitions, aligned by `name`
/// (order-independent — match on the echoed name, not position, since models
/// sometimes drop/reorder). Any candidate the model omitted gets an empty
/// definition (the node is still created, just undescribed — no data loss).
/// Definitions are trimmed and length-bounded.
pub fn parse_definitions(resp: &str, candidates: &[DefineCandidate]) -> Vec<String> {
    use std::collections::BTreeMap;

    let arr: Vec<serde_json::Value> = extract_json_array(resp)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();

    let mut by_name: BTreeMap<String, &serde_json::Value> = BTreeMap::new();
    for v in &arr {
        if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
            by_name.insert(n.to_lowercase(), v);
        }
    }

    candidates
        .iter()
        .map(|c| match by_name.get(&c.name.to_lowercase()) {
            Some(v) => v
                .get("definition")
                .and_then(|d| d.as_str())
                .map(str::trim)
                .map(bound_len)
                .unwrap_or_default(),
            None => String::new(),
        })
        .collect()
}

/// Trim a definition to [`MAX_DEF_LEN`] on a char boundary (never mid-codepoint).
fn bound_len(s: &str) -> String {
    if s.chars().count() <= MAX_DEF_LEN {
        s.to_string()
    } else {
        s.chars().take(MAX_DEF_LEN).collect()
    }
}

/// Pull the first top-level `[ ... ]` JSON array out of a model response,
/// tolerating ```json fences and leading/trailing prose.
fn extract_json_array(resp: &str) -> Option<String> {
    let start = resp.find('[')?;
    let end = resp.rfind(']')?;
    if end > start {
        Some(resp[start..=end].to_string())
    } else {
        None
    }
}

/// Generate a one-sentence definition for each candidate, in bounded batches
/// (see [`DEFINE_BATCH`]). Returns a `Vec<String>` aligned to the input order
/// (empty string where the model gave nothing). On any per-batch LLM error that
/// batch returns all-empty definitions — the entities are still created, just
/// undescribed; the pipeline never stalls on this stage.
pub async fn generate_entity_definitions(
    llm: &LlmClient,
    candidates: &[DefineCandidate],
) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    if candidates.len() <= DEFINE_BATCH {
        return define_one_batch(llm, candidates).await;
    }
    let mut out = Vec::with_capacity(candidates.len());
    for batch in candidates.chunks(DEFINE_BATCH) {
        out.extend(define_one_batch(llm, batch).await);
    }
    out
}

/// Define a single bounded batch of candidates in one LLM call.
async fn define_one_batch(llm: &LlmClient, candidates: &[DefineCandidate]) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let system = define_system();
    let prompt = build_define_prompt(candidates);
    match llm.chat(&system, &prompt).await {
        Ok(resp) => parse_definitions(&resp, candidates),
        Err(e) => {
            tracing::warn!("entity definition batch failed ({e}); entities created undescribed");
            vec![String::new(); candidates.len()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str) -> DefineCandidate {
        DefineCandidate {
            name: name.to_string(),
            context: format!("{name} appears in this sentence about the system."),
        }
    }

    #[test]
    fn build_prompt_embeds_candidates() {
        let p = build_define_prompt(&[cand("Orion Labs")]);
        assert!(p.contains("Orion Labs"));
        assert!(p.contains("JSON array"));
    }

    #[test]
    fn parse_aligns_by_name() {
        let cands = vec![cand("Nova"), cand("Orion Labs")];
        let resp = r#"[
            {"name":"Nova","definition":"A high-performance query engine built by Orion Labs."},
            {"name":"Orion Labs","definition":"A neural-storage company."}
        ]"#;
        let defs = parse_definitions(resp, &cands);
        assert_eq!(defs[0], "A high-performance query engine built by Orion Labs.");
        assert_eq!(defs[1], "A neural-storage company.");
    }

    #[test]
    fn parse_tolerates_fences_and_omissions() {
        let cands = vec![cand("Nova"), cand("Mars Rover")];
        // Model wrapped in a fence and dropped the second entity.
        let resp = "```json\n[{\"name\":\"Nova\",\"definition\":\"A query engine.\"}]\n```";
        let defs = parse_definitions(resp, &cands);
        assert_eq!(defs[0], "A query engine.");
        // Omitted entity → empty definition (still created, undescribed).
        assert_eq!(defs[1], "");
    }

    #[test]
    fn parse_bounds_definition_length() {
        let long = "x".repeat(500);
        let cands = vec![cand("Thing")];
        let resp = format!(r#"[{{"name":"Thing","definition":"{long}"}}]"#);
        let defs = parse_definitions(&resp, &cands);
        assert_eq!(defs[0].chars().count(), MAX_DEF_LEN);
    }

    #[test]
    fn empty_input_yields_empty() {
        let defs = parse_definitions("[]", &[]);
        assert!(defs.is_empty());
    }
}
