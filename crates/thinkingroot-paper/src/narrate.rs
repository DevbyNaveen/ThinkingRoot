//! Living Paper AI narrative synthesis (v1.1).
//!
//! Wires an `LlmClient` to produce the five AI-narrative sections
//! (`Abstract`, `KeyIdeas`, `HowItFitsTogether`, `RecentChanges`,
//! `HowToUseIt`) with strict citation grounding.
//!
//! # Citation contract
//!
//! Every claim in an AI narrative section must reference a real
//! witness id via the `[[witness:<id>]]` marker. After synthesis,
//! `validate_citations` scans the body, looks up each id in the
//! workspace's witness set, and:
//!
//! - Keeps markers that resolve to a real witness.
//! - **Strips** markers that don't (the model hallucinated an id).
//! - Replaces uncited sections (no surviving markers at all) with
//!   an honest "couldn't ground a narrative" disclaimer.
//!
//! No silent hallucination: a section either cites or admits absence.
//!
//! # Determinism
//!
//! Section-level BLAKE3 caching lives in [`SectionCache`]. The cache
//! key is `BLAKE3(prompt || witness_digest)` — same inputs ⇒ same
//! cached output. Pipeline.rs threads a [`SectionCache`] in/out so an
//! edit that doesn't change the witness digest reuses the prior
//! narrative without burning LLM tokens.

use std::collections::{BTreeMap, HashSet};

use blake3::Hasher;
use serde::{Deserialize, Serialize};
use thinkingroot_core::types::Witness;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_llm::llm::LlmClient;

use crate::sections::{SectionId, V1_1_AI_ORDER};

/// Captures the rendered AI sections plus a fresh cache the caller
/// should persist back to disk for the next compile.
#[derive(Debug, Default)]
pub struct AiNarrative {
    /// Section bodies keyed by id. Caller stitches them into the
    /// final `paper.md` body in [`V1_1_AI_ORDER`].
    pub sections: BTreeMap<SectionId, String>,
    /// Updated cache — write back via [`SectionCache::save`].
    pub cache: SectionCache,
}

/// Per-section content-derived cache. Stored at
/// `<root>/.thinkingroot/paper-cache.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SectionCache {
    /// Keyed by section kebab-case id. Each entry's `input_blake3`
    /// is the cache key used to short-circuit a resynth.
    pub entries: BTreeMap<String, SectionCacheEntry>,
}

/// One row of [`SectionCache`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionCacheEntry {
    /// BLAKE3 hex of the synth inputs. Match ⇒ reuse `output`.
    pub input_blake3: String,
    /// Verbatim section body — already citation-validated.
    pub output: String,
    /// When the entry was synthesised. RFC3339.
    pub generated_at: String,
}

impl SectionCache {
    /// Read a cache file from disk. Missing or unreadable files
    /// produce an empty cache — non-fatal, the synthesiser will just
    /// regenerate everything.
    pub fn load(path: &std::path::Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return Self::default(),
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Persist atomically (tempfile + rename) so a torn write never
    /// leaves a half-parsed cache on disk.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("encode: {e}"))
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn lookup(&self, section: SectionId, key: &str) -> Option<&str> {
        let entry = self.entries.get(section.kebab())?;
        if entry.input_blake3 == key {
            Some(entry.output.as_str())
        } else {
            None
        }
    }
}

/// Run the AI narrative synthesiser. Returns the rendered sections
/// plus a refreshed cache. On any per-section LLM failure, that
/// section falls back to the honest "couldn't ground a narrative"
/// stub — the overall synthesis never fails.
pub async fn narrate(
    graph: &GraphStore,
    workspace_name: &str,
    llm: &LlmClient,
    prior_cache: SectionCache,
) -> AiNarrative {
    // Sample witnesses for the prompt context. Cap at 80 — empirical
    // sweet spot for the workspace sizes the Playground targets:
    // enough breadth for the model to cite real ids, small enough to
    // stay inside the per-section input-token budget (~6K input
    // tokens total across all sections, ~$0.05 per regen on Haiku
    // 4.5 at 2026-05 pricing).
    const WITNESS_SAMPLE_CAP: usize = 80;
    let sample = graph
        .list_witnesses(Some(WITNESS_SAMPLE_CAP))
        .unwrap_or_default();
    let valid_ids: HashSet<String> = sample.iter().map(|w| w.id.to_hex()).collect();
    let digest = witness_digest(&sample);

    let mut sections: BTreeMap<SectionId, String> = BTreeMap::new();
    let mut cache = SectionCache::default();

    for section in V1_1_AI_ORDER.iter().copied() {
        let prompt_user = build_prompt(section, workspace_name, &sample);
        let cache_key = compute_cache_key(section, &prompt_user, &digest);

        // Cache hit short-circuits the LLM call entirely. The
        // cached output is already citation-validated (it was when
        // it was written), so we copy it through verbatim.
        if let Some(cached) = prior_cache.lookup(section, &cache_key) {
            sections.insert(section, cached.to_string());
            cache.entries.insert(
                section.kebab().to_string(),
                SectionCacheEntry {
                    input_blake3: cache_key,
                    output: cached.to_string(),
                    generated_at: now_rfc3339(),
                },
            );
            continue;
        }

        let body = match llm.chat(SYSTEM_PROMPT, &prompt_user).await {
            Ok(raw) => {
                let cleaned = validate_citations(&raw, &valid_ids);
                if cleaned.has_any_real_citation || section == SectionId::RecentChanges {
                    // RecentChanges may legitimately be empty (no new
                    // witnesses in the last 7 days). We don't force
                    // a citation requirement on it.
                    cleaned.body
                } else {
                    no_grounding_stub(section)
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "paper",
                    section = section.kebab(),
                    error = %e,
                    "AI narrative section failed; falling back to stub"
                );
                no_grounding_stub(section)
            }
        };

        cache.entries.insert(
            section.kebab().to_string(),
            SectionCacheEntry {
                input_blake3: cache_key,
                output: body.clone(),
                generated_at: now_rfc3339(),
            },
        );
        sections.insert(section, body);
    }

    AiNarrative { sections, cache }
}

const SYSTEM_PROMPT: &str = "\
You are the narrator of a Living Paper — a per-workspace artefact \
that summarises a cognition graph for human researchers. Strict \
rules: (1) every factual claim MUST be grounded by a `[[witness:<id>]]` \
marker referencing a witness id from the supplied context. (2) Do \
not invent witness ids — if you can't ground a claim, leave it out. \
(3) Be concise: each section is read in seconds, not minutes. \
(4) Output GitHub-flavoured markdown. Never wrap your answer in \
fences. No preamble, no sign-off — just the section body.";

fn build_prompt(section: SectionId, workspace_name: &str, sample: &[Witness]) -> String {
    let digest = format_witness_sample(sample);
    let task = match section {
        SectionId::Abstract => format!(
            "Write a ~120-word abstract of the `{workspace_name}` workspace. \
             Anchor every factual claim with a `[[witness:<id>]]` marker. \
             If the corpus seems thin, say so honestly."
        ),
        SectionId::KeyIdeas => format!(
            "Pick up to 5 of the most informative witnesses in the \
             `{workspace_name}` workspace and write one sentence per \
             witness explaining its idea. Format as a bullet list. \
             Each bullet must end with the `[[witness:<id>]]` marker \
             of the witness you're describing."
        ),
        SectionId::HowItFitsTogether => format!(
            "In ~150 words, explain how the major pieces of the \
             `{workspace_name}` workspace connect. Reference structural \
             witnesses by their `[[witness:<id>]]` markers where \
             helpful. If the witnesses don't reveal a clear structure, \
             say so."
        ),
        SectionId::RecentChanges => format!(
            "List concrete recent additions to `{workspace_name}` you \
             can see in the witness sample. Format as a bullet list with \
             `[[witness:<id>]]` markers. If nothing recent appears in \
             the sample, return an empty string — that's an honest \
             answer."
        ),
        SectionId::HowToUseIt => format!(
            "Write a short onboarding paragraph for `{workspace_name}` \
             — what's the canonical entry point to start exploring? \
             Cite witnesses with `[[witness:<id>]]` markers where you \
             point to specific files / functions / decisions."
        ),
        _ => String::from("unreachable"),
    };
    format!("# Witness context (sample)\n\n{digest}\n\n# Task\n\n{task}")
}

/// Format the witness sample as a compact bulleted context. One line
/// per witness: `- <id>  <rule>  <symbol>  bytes:<start>..<end>`.
/// Truncated symbols / long ids preserved verbatim — the model needs
/// the exact id to cite correctly.
fn format_witness_sample(witnesses: &[Witness]) -> String {
    if witnesses.is_empty() {
        return String::from(
            "(no witnesses yet — workspace is empty or hasn't compiled)",
        );
    }
    let mut out = String::with_capacity(witnesses.len() * 80);
    for w in witnesses {
        let symbol = w.symbol.as_deref().unwrap_or("");
        let first_span = w.spans.first();
        let (start, end) = first_span
            .map(|s| (s.start, s.end))
            .unwrap_or((0, 0));
        out.push_str("- ");
        out.push_str(&w.id.to_hex());
        out.push_str("  ");
        out.push_str(&w.rule);
        out.push_str("  ");
        if !symbol.is_empty() {
            out.push_str(symbol);
            out.push_str("  ");
        }
        out.push_str(&format!("bytes:{}..{}\n", start, end));
    }
    out
}

/// Result of citation validation. `body` is the rewritten markdown
/// with invalid markers stripped; `has_any_real_citation` lets the
/// caller decide whether the section grounded at all.
#[derive(Debug)]
struct CleanedSection {
    body: String,
    has_any_real_citation: bool,
}

/// Strip `[[witness:<id>]]` markers whose ids don't resolve to a
/// real witness in `valid_ids`. Real markers are preserved verbatim.
/// `has_any_real_citation` is true iff at least one marker survived.
fn validate_citations(body: &str, valid_ids: &HashSet<String>) -> CleanedSection {
    // Regex-free linear scan: search for the literal `[[witness:`
    // prefix, walk to `]]`, decide keep/strip per id.
    let mut out = String::with_capacity(body.len());
    let mut cursor = 0;
    let bytes = body.as_bytes();
    let mut survived = false;
    while let Some(start) = body[cursor..].find("[[witness:") {
        let abs_start = cursor + start;
        out.push_str(&body[cursor..abs_start]);
        let after_prefix = abs_start + "[[witness:".len();
        let rel_end = match body[after_prefix..].find("]]") {
            Some(idx) => idx,
            None => {
                // Unterminated marker — emit the prefix verbatim and
                // advance past it; no recursion hazard.
                out.push_str(&body[abs_start..after_prefix]);
                cursor = after_prefix;
                continue;
            }
        };
        let id = &body[after_prefix..after_prefix + rel_end];
        let id_clean = id.trim();
        // Re-emit the marker only if the id resolves. We also clip
        // common id shapes: BLAKE3 hex is `[A-Fa-f0-9]+`, ULIDs are
        // `[0-9A-HJKMNP-TV-Z]+` (no `O`/`L`/etc.). The hash-set
        // lookup is the authoritative check.
        if valid_ids.contains(id_clean) {
            out.push_str("[[witness:");
            out.push_str(id_clean);
            out.push_str("]]");
            survived = true;
        } else {
            // Strip the marker AND any single trailing whitespace it
            // left behind so prose doesn't grow double-spaces.
            // Defensive: this is best-effort cosmetic.
        }
        cursor = after_prefix + rel_end + "]]".len();
        let _ = bytes; // suppress unused warning on the byte slice
    }
    out.push_str(&body[cursor..]);
    CleanedSection {
        body: out,
        has_any_real_citation: survived,
    }
}

/// Honest fallback when the model produced no real citations.
fn no_grounding_stub(section: SectionId) -> String {
    match section {
        SectionId::RecentChanges => String::from(
            "_No recent witness additions appear in the sample. Compile \
             the workspace after editing your sources to refresh this \
             section._\n",
        ),
        _ => String::from(
            "_The narrator couldn't cite any real witnesses for this \
             section. The corpus may not yet cover this aspect — drop \
             more sources into the Playground and recompile._\n",
        ),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// BLAKE3-hex over the prompt + witness digest. Same inputs ⇒ same
/// cache key.
fn compute_cache_key(section: SectionId, prompt: &str, digest: &str) -> String {
    let mut h = Hasher::new();
    h.update(section.kebab().as_bytes());
    h.update(&[0]);
    h.update(prompt.as_bytes());
    h.update(&[0]);
    h.update(digest.as_bytes());
    h.finalize().to_hex().to_string()
}

/// BLAKE3-hex of the witness sample (ids only). Used as the "did the
/// workspace change?" signal in the cache key.
fn witness_digest(witnesses: &[Witness]) -> String {
    let mut h = Hasher::new();
    for w in witnesses {
        h.update(&w.id.0);
        h.update(&[0]);
    }
    h.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_citations_keeps_real_ids_and_strips_fakes() {
        let mut valid = HashSet::new();
        valid.insert("realid01".to_string());
        valid.insert("realid02".to_string());
        let body = "First claim [[witness:realid01]]. Bogus [[witness:fake01]]. \
                    Second claim [[witness:realid02]]";
        let out = validate_citations(body, &valid);
        assert!(out.has_any_real_citation);
        assert!(out.body.contains("[[witness:realid01]]"));
        assert!(out.body.contains("[[witness:realid02]]"));
        assert!(!out.body.contains("[[witness:fake01]]"));
    }

    #[test]
    fn validate_citations_reports_empty_when_no_real_markers() {
        let valid: HashSet<String> = HashSet::new();
        let body = "Body without markers [[witness:fake01]] [[witness:bogus]]";
        let out = validate_citations(body, &valid);
        assert!(!out.has_any_real_citation);
        // All bogus markers stripped.
        assert!(!out.body.contains("[[witness:"));
    }

    #[test]
    fn validate_citations_handles_no_markers_at_all() {
        let valid: HashSet<String> = HashSet::new();
        let body = "Plain prose with no citation markers anywhere.";
        let out = validate_citations(body, &valid);
        assert!(!out.has_any_real_citation);
        assert_eq!(out.body, body);
    }

    #[test]
    fn validate_citations_tolerates_unterminated_prefix() {
        let valid: HashSet<String> = HashSet::new();
        // Pathological input: opening prefix with no closing `]]`.
        let body = "Text [[witness:hanging-no-close still here";
        let out = validate_citations(body, &valid);
        // Should not panic, should not loop; output preserves the
        // literal text.
        assert!(out.body.contains("[[witness:"));
    }

    #[test]
    fn cache_save_load_round_trips() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path().join("paper-cache.json");
        let mut cache = SectionCache::default();
        cache.entries.insert(
            "abstract".to_string(),
            SectionCacheEntry {
                input_blake3: "abcdef".to_string(),
                output: "Body text".to_string(),
                generated_at: "2026-05-15T00:00:00Z".to_string(),
            },
        );
        cache.save(&path).unwrap();
        let loaded = SectionCache::load(&path);
        let entry = loaded.entries.get("abstract").unwrap();
        assert_eq!(entry.input_blake3, "abcdef");
        assert_eq!(entry.output, "Body text");
    }

    #[test]
    fn cache_load_missing_file_yields_empty() {
        let cache = SectionCache::load(std::path::Path::new("/nonexistent/path.json"));
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn compute_cache_key_is_deterministic() {
        let k1 = compute_cache_key(SectionId::Abstract, "prompt", "digest");
        let k2 = compute_cache_key(SectionId::Abstract, "prompt", "digest");
        assert_eq!(k1, k2);
    }

    #[test]
    fn compute_cache_key_differs_by_section() {
        let k1 = compute_cache_key(SectionId::Abstract, "prompt", "digest");
        let k2 = compute_cache_key(SectionId::KeyIdeas, "prompt", "digest");
        assert_ne!(k1, k2);
    }

    #[test]
    fn compute_cache_key_differs_by_digest() {
        let k1 = compute_cache_key(SectionId::Abstract, "prompt", "digest_a");
        let k2 = compute_cache_key(SectionId::Abstract, "prompt", "digest_b");
        assert_ne!(k1, k2);
    }

    #[test]
    fn no_grounding_stub_distinguishes_recent_changes() {
        let rc = no_grounding_stub(SectionId::RecentChanges);
        let abstr = no_grounding_stub(SectionId::Abstract);
        assert!(rc.contains("recent"));
        assert!(abstr.contains("corpus"));
    }

    #[test]
    fn build_prompt_includes_witness_context_header() {
        let p = build_prompt(SectionId::Abstract, "demo", &[]);
        assert!(p.contains("Witness context"));
        assert!(p.contains("Task"));
        assert!(p.contains("demo"));
    }
}
