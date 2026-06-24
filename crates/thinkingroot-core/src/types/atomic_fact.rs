use serde::{Deserialize, Serialize};

/// An LLM-extracted atomic proposition (subject–predicate–object), grounded to
/// a verbatim chunk's byte span. Part of the north-star compile rebuild
/// (2026-06-24): the mother-node spine's *fact* layer.
///
/// Distinct from [`super::Witness`] (mechanical, content-addressed, never
/// paraphrased) and the legacy [`super::Claim`]. Ids are prefixed `af:` so
/// retrieval fusion never confuses a fact with a claim. Every fact carries a
/// byte span inside its chunk (anti-hallucination gate 1) — a fact whose
/// rendered statement is not a substring of its chunk is dropped, never
/// persisted.
///
/// Bi-temporal: `valid_until < 0.0` means "still valid". A superseding
/// re-extraction tombstones the prior fact's `valid_until` rather than
/// deleting it, preserving the version timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomicFact {
    /// `af:` + BLAKE3(source_id || byte_start || byte_end || predicate).
    pub id: String,
    pub source_id: String,
    /// FK → `raw_chunks.id` — the spine's chunk→fact edge.
    pub chunk_id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    /// Human-readable rendering; MUST be a substring of the chunk content.
    pub statement: String,
    pub confidence: f32,
    /// The model that authored this fact (provenance/audit).
    pub extraction_model: String,
    pub workspace_id: String,
    pub sensitivity: String,
    /// Absolute byte offsets within the source file.
    pub byte_start: u64,
    pub byte_end: u64,
    pub content_blake3: String,
    pub valid_from: f64,
    /// `< 0.0` ⇒ still valid; otherwise the supersession tombstone time.
    pub valid_until: f64,
    pub created_at: f64,
}

impl AtomicFact {
    /// Stable, content-addressed id with the `af:` namespace prefix.
    pub fn derive_id(source_id: &str, byte_start: u64, byte_end: u64, predicate: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(source_id.as_bytes());
        hasher.update(&byte_start.to_le_bytes());
        hasher.update(&byte_end.to_le_bytes());
        hasher.update(predicate.as_bytes());
        format!("af:{}", hasher.finalize().to_hex())
    }

    /// True while the fact has not been superseded.
    pub fn is_live(&self) -> bool {
        self.valid_until < 0.0
    }
}

/// The verbatim chunk an LLM extraction round runs against. Carries the
/// absolute byte offset so chunk-relative spans the model returns map back
/// to source coordinates.
#[derive(Debug, Clone)]
pub struct ChunkContext {
    pub source_id: String,
    pub chunk_id: String,
    /// Verbatim chunk text — MUST equal `source_bytes[byte_start..byte_end]`.
    pub content: String,
    /// Absolute byte offset of the chunk within the source file.
    pub byte_start: u64,
    pub workspace_id: String,
    pub extraction_model: String,
    pub created_at: f64,
}

/// One raw proposition as the model emits it: an SVO triple plus the
/// VERBATIM supporting quote (a sentence/clause copied exactly from the
/// chunk). Quote-based grounding is far more reliable than asking an LLM to
/// count character offsets — we locate the quote in the chunk ourselves.
#[derive(Debug, Clone, Deserialize)]
pub struct RawAtomicFact {
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub predicate: String,
    #[serde(default)]
    pub object: String,
    /// Exact substring of the chunk that supports the fact.
    #[serde(default)]
    pub quote: String,
    #[serde(default)]
    pub confidence: Option<f32>,
}

/// Parse an LLM extraction response into grounded [`AtomicFact`]s.
///
/// **Anti-hallucination gate 1 (extract):** every fact must (a) carry a
/// `quote` that is found VERBATIM in the chunk (located via `str::find`, a
/// byte offset by construction), (b) name a non-empty subject AND object, and
/// (c) ground both subject and object to the chunk text (case-insensitive
/// substring). The persisted `statement` is the verbatim quote, so it is a
/// substring of the chunk by construction. Facts failing any check are
/// dropped — never persisted. Tolerant of prose/markdown around the JSON.
pub fn parse_atomic_facts(response: &str, ctx: &ChunkContext) -> Vec<AtomicFact> {
    // Extract the JSON array even if the model wrapped it in prose / fences.
    let json = match (response.find('['), response.rfind(']')) {
        (Some(a), Some(b)) if b > a => &response[a..=b],
        _ => return Vec::new(),
    };
    let raws: Vec<RawAtomicFact> = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let content_lc = ctx.content.to_lowercase();
    let mut out = Vec::new();
    for raw in raws {
        let subject = raw.subject.trim().to_string();
        let object = raw.object.trim().to_string();
        let predicate = raw.predicate.trim().to_string();
        let quote = raw.quote.trim().to_string();

        // (b) non-empty SVO core + a non-trivial quote.
        if subject.is_empty() || object.is_empty() || quote.len() < 3 {
            continue;
        }
        // (a) the quote must appear VERBATIM in the chunk → its byte span.
        let Some(bs) = ctx.content.find(&quote) else {
            continue;
        };
        let be = bs + quote.len();
        let statement = ctx.content[bs..be].to_string();
        // (c) ground subject + object to the chunk text.
        if !content_lc.contains(&subject.to_lowercase())
            || !content_lc.contains(&object.to_lowercase())
        {
            continue;
        }

        let abs_start = ctx.byte_start + bs as u64;
        let abs_end = ctx.byte_start + be as u64;
        let content_blake3 = blake3::hash(statement.as_bytes()).to_hex().to_string();
        out.push(AtomicFact {
            id: AtomicFact::derive_id(&ctx.source_id, abs_start, abs_end, &predicate),
            source_id: ctx.source_id.clone(),
            chunk_id: ctx.chunk_id.clone(),
            subject,
            predicate,
            object,
            statement,
            confidence: raw.confidence.unwrap_or(0.8).clamp(0.0, 1.0),
            extraction_model: ctx.extraction_model.clone(),
            workspace_id: ctx.workspace_id.clone(),
            sensitivity: "Public".to_string(),
            byte_start: abs_start,
            byte_end: abs_end,
            content_blake3,
            valid_from: ctx.created_at,
            valid_until: -1.0,
            created_at: ctx.created_at,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ChunkContext {
        ChunkContext {
            source_id: "src1".into(),
            chunk_id: "ch1".into(),
            content: "Yuriy teaches the database course at the university.".into(),
            byte_start: 100,
            workspace_id: "ws".into(),
            extraction_model: "test-model".into(),
            created_at: 1.0,
        }
    }

    #[test]
    fn parses_grounded_fact_with_absolute_span() {
        let c = ctx();
        // The quote is located verbatim at chunk bytes 0..33.
        let resp = r#"[{"subject":"Yuriy","predicate":"teaches","object":"database course","quote":"Yuriy teaches the database course"}]"#;
        let facts = parse_atomic_facts(resp, &c);
        assert_eq!(facts.len(), 1);
        let f = &facts[0];
        assert!(f.id.starts_with("af:"));
        assert_eq!(f.byte_start, 100); // chunk byte_start + 0
        assert_eq!(f.byte_end, 133); // 100 + 33
        assert_eq!(f.statement, "Yuriy teaches the database course");
        assert!(f.is_live());
    }

    #[test]
    fn drops_fact_whose_quote_is_not_in_chunk() {
        let c = ctx();
        // The quote was never in the chunk → ungrounded → dropped.
        let resp = r#"[{"subject":"Yuriy","predicate":"teaches","object":"course","quote":"Yuriy works remotely from Berlin"}]"#;
        assert!(parse_atomic_facts(resp, &c).is_empty());
    }

    #[test]
    fn drops_fact_with_fabricated_entity() {
        let c = ctx();
        // "Microsoft" never appears in the chunk → ungrounded → dropped.
        let resp = r#"[{"subject":"Microsoft","predicate":"owns","object":"database course","quote":"Yuriy teaches the database course"}]"#;
        assert!(parse_atomic_facts(resp, &c).is_empty());
    }

    #[test]
    fn tolerates_prose_wrapped_json() {
        let c = ctx();
        let resp = "Here are the facts:\n```json\n[{\"subject\":\"Yuriy\",\"predicate\":\"teaches\",\"object\":\"university\",\"quote\":\"Yuriy teaches the database course at the university\"}]\n```";
        assert_eq!(parse_atomic_facts(resp, &c).len(), 1);
    }

    #[test]
    fn empty_or_garbage_response_yields_nothing() {
        let c = ctx();
        assert!(parse_atomic_facts("no json here", &c).is_empty());
        assert!(parse_atomic_facts("[]", &c).is_empty());
    }
}
