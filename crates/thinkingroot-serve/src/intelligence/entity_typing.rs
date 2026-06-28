//! Phase 1b — **write-boundary entity typing + literal rejection** (EDC stage).
//!
//! The atomic-fact extractor ([`super::atomic_extract`]) gives us grounded SVO
//! facts. Their subjects/objects become graph ENTITY NODES. Historically the
//! type of each node was guessed by *mechanical heuristics*
//! (`spine_inserts::guess_entity_type`) — which is the documented source of the
//! Neural-Graph bugs: a company labelled "Person", `50 people` / `2024` becoming
//! entity nodes, etc.
//!
//! This module is the SOTA fix (Extract-Define-Canonicalize, arXiv 2404.03868):
//! a SEPARATE, LLM-backed pass — run in the async enrichment queue, never inline,
//! never holding the storage lock — that for each candidate entity decides:
//!   * **keep** — is this a real named entity, or a literal/value/date/count
//!     (which belongs as a *property*, not a node)?  ← kills over-extraction
//!   * **type** — one of the closed ontology variants                ← kills mis-typing
//!   * **canonical** — a clean canonical surface form
//!
//! The LLM I/O is the only non-deterministic part; the literal guard and the
//! response parser are pure and unit-tested. A missing/erroring LLM degrades
//! gracefully to the mechanical fallback (`keep=true`, heuristic type) so the
//! pipeline never blocks on this stage.

use thinkingroot_core::types::EntityType;
use thinkingroot_llm::llm::LlmClient;

/// The closed type ontology offered to the LLM. Kept in lockstep with
/// [`EntityType`]; the prompt lists exactly these so the model can only choose a
/// valid wire type. Numbers/dates/money/counts are deliberately ABSENT — they
/// are properties, not nodes (enforced by [`is_literal_value`] + the prompt).
pub const ONTOLOGY: &[EntityType] = &[
    EntityType::Person,
    EntityType::Organization,
    EntityType::Team,
    EntityType::Product,
    EntityType::Concept,
    EntityType::Event,
    EntityType::Location,
    EntityType::System,
    EntityType::Service,
    EntityType::Api,
    EntityType::Database,
    EntityType::Library,
    EntityType::Module,
    EntityType::Function,
    EntityType::File,
    EntityType::Config,
];

/// A candidate entity surfaced from a source's facts: the raw name plus a short
/// grounding context (a fact statement it appears in) so the LLM can type it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    /// One representative fact sentence mentioning `name` (typing context).
    pub context: String,
}

/// The verdict for one candidate after the typing/verify pass.
#[derive(Debug, Clone)]
pub struct TypingDecision {
    /// Canonical surface form to store (may differ from the raw candidate).
    pub canonical: String,
    /// Resolved closed-ontology type.
    pub entity_type: EntityType,
    /// `false` ⇒ this is a literal/value/date/count — do NOT make it a node.
    pub keep: bool,
}

/// System prompt: closed-ontology typing + literal rejection. The model returns
/// a JSON array aligned to the input order, one object per candidate.
pub fn typing_system() -> String {
    let types = ONTOLOGY
        .iter()
        .map(|t| t.wire_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You clean a knowledge-graph's entities. You are given a JSON array of candidate \
entities, each with a `name` and a `context` sentence it appeared in. For EACH candidate, \
output one object with:\n\
- `name`: the input name, copied EXACTLY (so we can align your answer).\n\
- `keep`: boolean. Set FALSE when the name is NOT a real named entity but a literal value — a \
number, date, year, money amount, percentage, measurement, duration, or a bare count like \
\"50 people\" or \"3 items\". Those are PROPERTIES, not nodes. Set TRUE only for genuine named \
things (people, organizations, products, places, concepts, systems, etc.).\n\
- `type`: when keep is true, the single best-fitting type from this closed list: [{types}]. \
Use the context to disambiguate (e.g. \"Orion Labs\" mentioned as a company is `organization`, \
not `person`). When keep is false, use \"concept\".\n\
- `canonical`: a clean canonical name (trim noise/articles; fix obvious casing). Keep it short.\n\
Rules: choose `type` ONLY from the closed list. Do not invent types. Do not add commentary. \
Output ONLY a JSON array, same length and order as the input, no markdown fences."
    )
}

/// Build the user prompt: a compact JSON array of `{name, context}`.
pub fn build_typing_prompt(candidates: &[Candidate]) -> String {
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
    format!("Candidates:\n{json}\n\nDecisions (JSON array, same order):")
}

/// Deterministic literal guard — a fast pre-filter that rejects obvious
/// non-entities WITHOUT an LLM call (numbers, dates, money, percentages,
/// counts). Catches the common cases cheaply so the LLM only adjudicates the
/// genuinely ambiguous ones; also the safety net if the LLM is unavailable.
///
/// Returns `true` when `name` is a literal value (i.e. should NOT be a node).
pub fn is_literal_value(name: &str) -> bool {
    let s = name.trim();
    if s.is_empty() {
        return true;
    }
    let lc = s.to_lowercase();

    // Pure number / numeric-with-separators ("2024", "1,200", "3.5", "12:30").
    let stripped: String = s
        .chars()
        .filter(|c| !matches!(c, ',' | '.' | ':' | '%' | '$' | '€' | '£' | ' ' | '-' | '/'))
        .collect();
    if !stripped.is_empty() && stripped.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }

    // Leading currency or trailing percent ("$5M", "80%", "€20").
    if s.starts_with(['$', '€', '£']) || s.ends_with('%') {
        if s.chars().any(|c| c.is_ascii_digit()) {
            return true;
        }
    }

    // Starts with a number → almost always a quantity/count ("50 people",
    // "3 items", "2 weeks", "12 cameras").
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return true;
    }

    // Bare month / weekday / date-ish single tokens.
    const TIME_WORDS: &[&str] = &[
        "january", "february", "march", "april", "may", "june", "july", "august",
        "september", "october", "november", "december",
        "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday",
        "today", "tomorrow", "yesterday",
    ];
    let words: Vec<&str> = lc.split_whitespace().collect();
    // "March 2024", "June 5" → a date phrase (a time word + a number).
    if words.len() <= 3
        && words.iter().any(|w| TIME_WORDS.contains(w))
        && words.iter().any(|w| w.chars().any(|c| c.is_ascii_digit()))
    {
        return true;
    }
    // A lone time word with nothing else of substance.
    if words.len() == 1 && TIME_WORDS.contains(&lc.as_str()) {
        return true;
    }

    false
}

/// Parse the LLM response into per-candidate decisions, aligned by `name`
/// (order-independent — we match on the echoed name, not position, since models
/// sometimes drop/reorder). Any candidate the model omitted, or that fails the
/// literal guard regardless of the model, gets a safe default.
pub fn parse_typing(resp: &str, candidates: &[Candidate]) -> Vec<TypingDecision> {
    use std::collections::BTreeMap;

    // Locate the JSON array even if the model wrapped it in prose/fences.
    let arr: Vec<serde_json::Value> = extract_json_array(resp)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();

    // Index the model's verdicts by lowercased echoed name.
    let mut by_name: BTreeMap<String, &serde_json::Value> = BTreeMap::new();
    for v in &arr {
        if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
            by_name.insert(n.to_lowercase(), v);
        }
    }

    candidates
        .iter()
        .map(|c| {
            // Deterministic guard wins outright — a literal is never a node,
            // whatever the model said.
            if is_literal_value(&c.name) {
                return TypingDecision {
                    canonical: c.name.clone(),
                    entity_type: EntityType::Concept,
                    keep: false,
                };
            }
            match by_name.get(&c.name.to_lowercase()) {
                Some(v) => {
                    let keep = v.get("keep").and_then(|k| k.as_bool()).unwrap_or(true);
                    let entity_type = v
                        .get("type")
                        .and_then(|t| t.as_str())
                        .and_then(EntityType::from_any)
                        .unwrap_or(EntityType::Concept);
                    let canonical = v
                        .get("canonical")
                        .and_then(|c| c.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&c.name)
                        .to_string();
                    TypingDecision { canonical, entity_type, keep }
                }
                // Model omitted this one → keep it, neutral type (no data loss).
                None => TypingDecision {
                    canonical: c.name.clone(),
                    entity_type: EntityType::Concept,
                    keep: true,
                },
            }
        })
        .collect()
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

/// Max candidates per typing LLM call. A large source can surface hundreds of
/// distinct entities; typing them all in ONE call overruns the model's output
/// token budget (finish_reason=length → truncated → parse fails → the whole
/// source falls back to neutral literal-guard typing AND burns a full ~32k-token
/// generation per attempt). That was the dominant cost stalling large-doc atomic
/// extraction. Bounding each call keeps the response small, so typing succeeds
/// and stays cheap; only a genuinely-failing batch degrades, not the whole source.
const TYPING_BATCH: usize = 40;

/// Run the typing/verify pass over a source's candidate entities, in bounded
/// batches (see [`TYPING_BATCH`]). On any per-batch LLM error, that batch returns
/// the graceful fallback (keep-all, literal-guarded, neutral type) so the caller
/// can still promote — correctness degrades for that batch only, never a stall.
pub async fn type_source_entities(
    llm: &LlmClient,
    candidates: &[Candidate],
) -> Vec<TypingDecision> {
    if candidates.is_empty() {
        return Vec::new();
    }
    if candidates.len() <= TYPING_BATCH {
        return type_one_batch(llm, candidates).await;
    }
    let mut out = Vec::with_capacity(candidates.len());
    for batch in candidates.chunks(TYPING_BATCH) {
        out.extend(type_one_batch(llm, batch).await);
    }
    out
}

/// Type a single bounded batch of candidates in one LLM call.
async fn type_one_batch(llm: &LlmClient, candidates: &[Candidate]) -> Vec<TypingDecision> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let system = typing_system();
    let prompt = build_typing_prompt(candidates);
    match llm.chat(&system, &prompt).await {
        Ok(resp) => parse_typing(&resp, candidates),
        Err(e) => {
            tracing::warn!("entity typing batch failed ({e}); using literal-guard fallback");
            // Fallback: literal guard still applies; everything else kept neutral.
            candidates
                .iter()
                .map(|c| TypingDecision {
                    canonical: c.name.clone(),
                    entity_type: EntityType::Concept,
                    keep: !is_literal_value(&c.name),
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str) -> Candidate {
        Candidate { name: name.to_string(), context: format!("{name} appears here.") }
    }

    #[test]
    fn literal_guard_rejects_numbers_dates_money_counts() {
        for lit in [
            "2024", "1,200", "3.5", "80%", "$5M", "€20", "50 people", "3 items",
            "12 cameras", "March 2024", "June 5", "Monday", "12:30",
        ] {
            assert!(is_literal_value(lit), "{lit:?} should be a literal");
        }
    }

    #[test]
    fn literal_guard_keeps_real_entities() {
        for ent in [
            "Orion Labs", "Lena Park", "PostgreSQL", "San Francisco", "Stripe Billing",
            "WWDC", "the Q3 launch team",
        ] {
            assert!(!is_literal_value(ent), "{ent:?} should be kept");
        }
    }

    #[test]
    fn prompt_lists_only_closed_ontology() {
        let sys = typing_system();
        assert!(sys.contains("organization"));
        assert!(sys.contains("location"));
        // Numbers/dates are NOT offered as a type.
        assert!(!sys.contains("[number"));
    }

    #[test]
    fn build_prompt_embeds_candidates() {
        let p = build_typing_prompt(&[cand("Orion Labs")]);
        assert!(p.contains("Orion Labs"));
        assert!(p.contains("JSON array"));
    }

    #[test]
    fn parse_aligns_by_name_and_applies_type() {
        let cands = vec![cand("Orion Labs"), cand("Lena Park")];
        let resp = r#"[
            {"name":"Orion Labs","keep":true,"type":"organization","canonical":"Orion Labs"},
            {"name":"Lena Park","keep":true,"type":"person","canonical":"Lena Park"}
        ]"#;
        let decisions = parse_typing(resp, &cands);
        assert_eq!(decisions[0].entity_type, EntityType::Organization);
        assert_eq!(decisions[1].entity_type, EntityType::Person);
        assert!(decisions[0].keep && decisions[1].keep);
    }

    #[test]
    fn parse_literal_guard_overrides_model() {
        // Even if the model says keep=true, a literal is dropped.
        let cands = vec![cand("50 people")];
        let resp = r#"[{"name":"50 people","keep":true,"type":"team","canonical":"50 people"}]"#;
        let decisions = parse_typing(resp, &cands);
        assert!(!decisions[0].keep, "literal must be dropped regardless of model");
    }

    #[test]
    fn parse_tolerates_fences_and_omissions() {
        let cands = vec![cand("Orion Labs"), cand("Mars Rover")];
        // Model wrapped in a fence and dropped the second candidate.
        let resp = "```json\n[{\"name\":\"Orion Labs\",\"keep\":true,\"type\":\"organization\"}]\n```";
        let decisions = parse_typing(resp, &cands);
        assert_eq!(decisions[0].entity_type, EntityType::Organization);
        // Omitted candidate is kept with a neutral type (no data loss).
        assert!(decisions[1].keep);
        assert_eq!(decisions[1].entity_type, EntityType::Concept);
    }

    #[test]
    fn unknown_type_falls_back_to_concept() {
        let cands = vec![cand("Thing")];
        let resp = r#"[{"name":"Thing","keep":true,"type":"alien","canonical":"Thing"}]"#;
        let decisions = parse_typing(resp, &cands);
        assert_eq!(decisions[0].entity_type, EntityType::Concept);
    }
}
