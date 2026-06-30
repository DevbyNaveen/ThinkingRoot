//! Phase 1d — **write-boundary entity CANONICALIZATION** (EDC stage 4).
//!
//! The promotion path dedups entities by EXACT name only, so "Orion Labs" and
//! "Orion Laboratory" (or "Priya" / "Priya Raman") fragment into separate nodes
//! → the graph loses the cross-document links that multi-session reasoning needs.
//! The structural linker has a *mechanical* fuzzy resolver, but mechanical
//! similarity both **misses** semantic dupes (acronyms, paraphrases) and
//! **false-merges** look-alikes (two different "Raj Patel"s).
//!
//! This stage is the SOTA two-tier resolver's Tier 2 (arXiv 2501.13956 funnel):
//!   1. **Block** (cheap, pure) — for each NEW entity, find EXISTING entities of
//!      the same type whose name is in a *gray zone*: similar enough to possibly
//!      be the same, but not an exact match. Capped, so cost is bounded.
//!   2. **Judge** (LLM, off-lock) — ask the model, using each side's one-sentence
//!      DEFINITION (EDC stage 2), whether the pair is the SAME physical entity.
//!
//! The judge is **link-don't-merge on doubt**: only a confident YES merges; any
//! uncertainty, omission, or LLM error defaults to "keep separate". A wrong merge
//! is a confabulation baked into the graph, which violates the product's core
//! promise — so the bias is deliberately toward NOT merging.
//!
//! The merge is applied at *create time* (the new name is rewritten to the
//! existing survivor's canonical, and registered as an alias) so a duplicate node
//! is never created — no risky redirect-and-delete of live nodes. Runs in the
//! async enrichment queue, OFF the storage lock; gated by `TR_ENTITY_CANON`
//! (default on). The blocking + parsing are pure and unit-tested.

use std::collections::BTreeMap;

use thinkingroot_llm::llm::LlmClient;

/// An entity to canonicalize against the existing graph: its canonical name, its
/// type tag (`{:?}` of `EntityType`, compared verbatim), and its DEFINE-stage
/// definition (the judge's main signal; empty is allowed but weaker).
#[derive(Debug, Clone)]
pub struct EntityRef {
    pub name: String,
    pub entity_type: String,
    pub definition: String,
}

/// A blocked candidate pair awaiting the LLM merge verdict: a NEW entity and the
/// EXISTING survivor it might duplicate, plus the blocking similarity (for
/// ordering/capping).
#[derive(Debug, Clone)]
pub struct MergePair {
    pub new: EntityRef,
    pub existing: EntityRef,
    pub similarity: f64,
}

/// Block on raw name closeness alone at/above this similarity ("Postgres" ~
/// "PostgreSQL"). Below it we additionally require a shared significant token —
/// because "Orion Labs" ~ "Orion Laboratory" only scores ~0.56 (the long
/// "-oratory" suffix drags char-distance down) yet is an obvious dupe candidate.
const STRONG_SIM: f64 = 0.72;
/// With a shared significant token, block down to this floor. Recall-biased — the
/// LLM judge is the precision gate, so blocking offers candidates rather than
/// dropping them (a dropped pair can never be merged; an extra pair just costs a
/// judged row, capped by [`MAX_PAIRS`]).
const WEAK_SIM: f64 = 0.40;

/// Hard cap on pairs judged per source — bounds the LLM cost of this stage.
/// Pairs are kept by descending similarity, so the most-likely dupes win the budget.
const MAX_PAIRS: usize = 60;

/// Normalized Levenshtein similarity in `[0,1]` over lowercased chars
/// (`1.0` = identical). Self-contained (the `serve` crate has no `strsim`).
pub fn name_similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.trim().to_lowercase().chars().collect();
    let b: Vec<char> = b.trim().to_lowercase().chars().collect();
    let (la, lb) = (a.len(), b.len());
    if la == 0 && lb == 0 {
        return 1.0;
    }
    if la == 0 || lb == 0 {
        return 0.0;
    }
    // Two-row DP edit distance.
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut cur = vec![0usize; lb + 1];
    for i in 1..=la {
        cur[0] = i;
        for j in 1..=lb {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let dist = prev[lb] as f64;
    1.0 - dist / (la.max(lb) as f64)
}

/// True when one name's whitespace tokens are a subset of the other's — catches
/// "Orion" ⊆ "Orion Labs" and "Priya" ⊆ "Priya Raman", which edit-distance alone
/// scores low. Both must have ≥1 token; the shorter must be fully contained.
fn token_subset(a: &str, b: &str) -> bool {
    let ta: Vec<String> = a.to_lowercase().split_whitespace().map(str::to_string).collect();
    let tb: Vec<String> = b.to_lowercase().split_whitespace().map(str::to_string).collect();
    if ta.is_empty() || tb.is_empty() || ta.len() == tb.len() {
        return false;
    }
    let (short, long) = if ta.len() < tb.len() { (&ta, &tb) } else { (&tb, &ta) };
    short.iter().all(|t| long.contains(t))
}

/// True when the names share a meaningful token: equal first tokens (≥3 chars,
/// the usual entity head like "Orion"/"Priya") or any common token ≥4 chars.
/// This is the recall signal that lets a moderate char-similarity pair through
/// the gray-zone block (e.g. "Orion Labs" vs "Orion Laboratory").
fn shares_significant_token(a: &str, b: &str) -> bool {
    let ta: Vec<&str> = a.split_whitespace().collect();
    let tb: Vec<&str> = b.split_whitespace().collect();
    if let (Some(fa), Some(fb)) = (ta.first(), tb.first()) {
        if fa == fb && fa.chars().count() >= 3 {
            return true;
        }
    }
    ta.iter().any(|t| t.chars().count() >= 4 && tb.contains(t))
}

/// Block: produce the gray-zone candidate pairs to adjudicate. Same type, name
/// not exactly equal (case-insensitive — exact is already deduped upstream), and
/// either similarity ≥ [`SIM_FLOOR`] or a token-subset. Sorted by descending
/// similarity and capped at [`MAX_PAIRS`].
pub fn block_merge_pairs(new: &[EntityRef], existing: &[EntityRef]) -> Vec<MergePair> {
    let mut pairs: Vec<MergePair> = Vec::new();
    for n in new {
        let nl = n.name.trim().to_lowercase();
        for e in existing {
            if n.entity_type != e.entity_type {
                continue;
            }
            let el = e.name.trim().to_lowercase();
            if nl == el {
                continue; // exact match → already the same node, nothing to judge
            }
            let sim = name_similarity(&nl, &el);
            let blocked = token_subset(&nl, &el)
                || sim >= STRONG_SIM
                || (sim >= WEAK_SIM && shares_significant_token(&nl, &el));
            if blocked {
                pairs.push(MergePair {
                    new: n.clone(),
                    existing: e.clone(),
                    similarity: sim,
                });
            }
        }
    }
    pairs.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal));
    pairs.truncate(MAX_PAIRS);
    pairs
}

/// System prompt: per-pair SAME/DIFFERENT verdict, biased toward DIFFERENT.
pub fn judge_system() -> String {
    "You decide whether pairs of knowledge-graph entities refer to the SAME real-world \
thing, so duplicates can be merged. You are given a JSON array of pairs; each has entity \
A and entity B, with a name and a one-sentence definition. For EACH pair output one object:\n\
- `i`: the pair's index (copied from the input).\n\
- `same`: boolean. TRUE only if A and B are confidently the SAME entity (e.g. \"Orion Labs\" \
and \"Orion Laboratory\", both defined as the same storage company). FALSE if they are \
different things that merely share a name or look similar (e.g. two different people both \
named \"Raj Patel\", or \"Nova\" the product vs \"Nova\" the team). When the definitions \
conflict or are too thin to be sure, answer FALSE.\n\
Rules: default to FALSE on any doubt — a wrong merge corrupts the graph. Output ONLY a JSON \
array, one object per input pair, same order, no markdown fences."
        .to_string()
}

/// Build the judge prompt: a compact JSON array of `{i, a_name, a_def, b_name, b_def}`.
pub fn build_judge_prompt(pairs: &[MergePair]) -> String {
    let arr: Vec<serde_json::Value> = pairs
        .iter()
        .enumerate()
        .map(|(i, p)| {
            serde_json::json!({
                "i": i,
                "a_name": p.new.name,
                "a_def": p.new.definition.chars().take(200).collect::<String>(),
                "b_name": p.existing.name,
                "b_def": p.existing.definition.chars().take(200).collect::<String>(),
            })
        })
        .collect();
    let json = serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string());
    format!("Pairs:\n{json}\n\nVerdicts (JSON array, same order):")
}

/// Parse the judge response into a per-pair merge verdict, aligned by the echoed
/// `i` index. **Link-don't-merge on doubt:** any pair the model omitted, or that
/// isn't an explicit `true`, defaults to `false` (keep separate).
pub fn parse_judgments(resp: &str, n_pairs: usize) -> Vec<bool> {
    let arr: Vec<serde_json::Value> = extract_json_array(resp)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let mut out = vec![false; n_pairs];
    for v in &arr {
        let Some(i) = v.get("i").and_then(|i| i.as_u64()) else {
            continue;
        };
        let i = i as usize;
        if i < n_pairs {
            out[i] = v.get("same").and_then(|s| s.as_bool()).unwrap_or(false);
        }
    }
    out
}

fn extract_json_array(resp: &str) -> Option<String> {
    let start = resp.find('[')?;
    let end = resp.rfind(']')?;
    if end > start {
        Some(resp[start..=end].to_string())
    } else {
        None
    }
}

/// Adjudicate the blocked pairs in one LLM call. On error, all-`false` (the
/// safe, separate-keeping default — the pipeline never stalls or false-merges).
pub async fn judge_merges(llm: &LlmClient, pairs: &[MergePair]) -> Vec<bool> {
    if pairs.is_empty() {
        return Vec::new();
    }
    match llm.chat(&judge_system(), &build_judge_prompt(pairs)).await {
        Ok(resp) => parse_judgments(&resp, pairs.len()),
        Err(e) => {
            tracing::warn!("entity canonicalization judge failed ({e}); keeping all separate");
            vec![false; pairs.len()]
        }
    }
}

/// A confirmed merge: the new extracted name should resolve to the existing
/// survivor's canonical node (and be registered as an alias of it).
#[derive(Debug, Clone)]
pub struct Merge {
    /// The new entity's canonical name (lowercased) — the merge-map key.
    pub new_name_lc: String,
    /// The surviving existing entity's canonical name (verbatim).
    pub survivor: String,
}

/// End-to-end Tier-2 canonicalization: block → judge → confirmed merges. Best
/// match wins per new entity (pairs are similarity-sorted). Returns the merges to
/// apply; an empty result means nothing was confidently a duplicate.
pub async fn canonicalize(llm: &LlmClient, new: &[EntityRef], existing: &[EntityRef]) -> Vec<Merge> {
    let pairs = block_merge_pairs(new, existing);
    if pairs.is_empty() {
        return Vec::new();
    }
    let verdicts = judge_merges(llm, &pairs).await;
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    let mut merges = Vec::new();
    for (p, same) in pairs.iter().zip(verdicts.into_iter()) {
        if !same {
            continue;
        }
        let key = p.new.name.trim().to_lowercase();
        // Don't merge a name into itself, and keep the FIRST (highest-similarity)
        // survivor for each new name.
        if key == p.existing.name.trim().to_lowercase() || seen.contains_key(&key) {
            continue;
        }
        seen.insert(key.clone(), ());
        merges.push(Merge {
            new_name_lc: key,
            survivor: p.existing.name.clone(),
        });
    }
    merges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eref(name: &str, ty: &str, def: &str) -> EntityRef {
        EntityRef { name: name.into(), entity_type: ty.into(), definition: def.into() }
    }

    #[test]
    fn similarity_basic() {
        assert!((name_similarity("Orion Labs", "Orion Labs") - 1.0).abs() < 1e-9);
        // The long "-oratory" suffix drags char-distance down to ~0.56 — which is
        // exactly why blocking can't rely on char-similarity alone (see below).
        assert!(name_similarity("Orion Labs", "Orion Laboratory") > 0.5);
        assert!(name_similarity("Orion Labs", "Redis") < 0.4);
    }

    #[test]
    fn shared_token_lets_moderate_pairs_through() {
        // ~0.56 char-sim, but a shared head token "orion" → must block.
        assert!(shares_significant_token("orion labs", "orion laboratory"));
        // Different heads, no shared ≥4-char token → not blocked on this signal.
        assert!(!shares_significant_token("redis", "postgres"));
    }

    #[test]
    fn token_subset_catches_acronym_and_first_name() {
        assert!(token_subset("Orion", "Orion Labs"));
        assert!(token_subset("Priya", "Priya Raman"));
        assert!(!token_subset("Orion Labs", "Acme Corp"));
        assert!(!token_subset("Orion Labs", "Orion Labs")); // equal length → not a subset
    }

    #[test]
    fn blocking_same_type_grayzone_only() {
        let new = vec![
            eref("Orion Laboratory", "Organization", "A storage company."),
            eref("Redis", "Database", "An in-memory store."),
        ];
        let existing = vec![
            eref("Orion Labs", "Organization", "A storage company."),
            eref("Postgres", "Database", "A relational DB."), // different type from Orion, low sim to Redis
        ];
        let pairs = block_merge_pairs(&new, &existing);
        // Orion Laboratory ~ Orion Labs (same type, high sim) → blocked.
        assert!(pairs.iter().any(|p| p.new.name == "Orion Laboratory" && p.existing.name == "Orion Labs"));
        // Redis vs Postgres: different names, low sim, same type → NOT blocked.
        assert!(!pairs.iter().any(|p| p.new.name == "Redis"));
    }

    #[test]
    fn blocking_skips_exact_and_cross_type() {
        let new = vec![eref("Nova", "Product", "A query engine.")];
        let existing = vec![
            eref("Nova", "Product", "A query engine."), // exact → skip
            eref("Novo", "Person", "A person."),        // cross-type → skip
        ];
        assert!(block_merge_pairs(&new, &existing).is_empty());
    }

    #[test]
    fn parse_defaults_false_on_doubt() {
        // index 1 omitted, index 2 says false explicitly, index 0 true.
        let resp = r#"[{"i":0,"same":true},{"i":2,"same":false}]"#;
        let v = parse_judgments(resp, 3);
        assert_eq!(v, vec![true, false, false]);
    }

    #[test]
    fn parse_tolerates_fences_and_garbage() {
        let resp = "```json\n[{\"i\":0,\"same\":true}]\n```";
        assert_eq!(parse_judgments(resp, 1), vec![true]);
        assert_eq!(parse_judgments("not json", 2), vec![false, false]);
    }

    #[test]
    fn build_prompt_embeds_pairs() {
        let pairs = block_merge_pairs(
            &[eref("Orion Laboratory", "Organization", "A storage company.")],
            &[eref("Orion Labs", "Organization", "A storage company.")],
        );
        let p = build_judge_prompt(&pairs);
        assert!(p.contains("Orion Laboratory"));
        assert!(p.contains("Orion Labs"));
        assert!(p.contains("JSON array"));
    }
}
