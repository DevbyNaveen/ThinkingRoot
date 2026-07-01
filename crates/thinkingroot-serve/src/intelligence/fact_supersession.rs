//! Phase 3 — **cross-source knowledge-update supersession** (the bi-temporal
//! read-side completer).
//!
//! Per-source supersession (`supersede_facts_not_in`) already closes facts a
//! *re-extraction of the same document* no longer confirms. But the hard
//! knowledge-update case is **cross-document**: a user says "I work at Orion" in
//! session 1 and "I work at Acme" in session 5 — two different sources, so the
//! per-source path leaves BOTH facts live, and recall returns the stale value
//! alongside the current one. (This is exactly the LongMemEval knowledge-update
//! failure mode.)
//!
//! This stage closes that gap with the Engram slot-match pattern:
//!   1. **Slot collision (cheap, pure)** — find existing LIVE facts that share a
//!      `(subject, predicate)` "slot" with a NEW fact but assert a DIFFERENT
//!      object, and are OLDER. These are *candidate* updates.
//!   2. **Contradiction judge (LLM, off-lock)** — for each candidate, decide:
//!      does the new fact UPDATE/REPLACE the old (value changed → close the old),
//!      or are both independently true (e.g. "likes coffee" / "likes tea" — a
//!      multi-valued predicate → keep both)?
//!
//! **Keep-both on doubt:** only a confident "this is an update" supersedes; any
//! uncertainty, omission, or LLM error keeps both facts live. Wrongly tombstoning
//! a true fact silently lowers recall, so the bias is deliberately conservative.
//!
//! Runs in the async enrichment queue, OFF the storage lock. Because tombstoning
//! a fact removes it from recall, this is an accuracy-affecting layer: gated by
//! `TR_FACT_SUPERSEDE`, **default OFF** (eval-gated flip), per the
//! flags-default-off doctrine.

use thinkingroot_llm::llm::LlmClient;

/// The minimal projection of an atomic fact this stage needs.
#[derive(Debug, Clone)]
pub struct FactRef {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub statement: String,
    /// When the fact became valid (for "older" ordering).
    pub valid_from: f64,
    /// Explicit EVENT time (epoch secs) mechanically extracted from the
    /// statement (`fact_event_date` sidecar / `extract_event_date`), when the
    /// statement names an absolute date. `None` = undated — ordering falls
    /// back to ingest time.
    pub event_date: Option<f64>,
}

/// A candidate update: a NEW fact and an OLDER existing fact sharing a
/// `(subject, predicate)` slot with different objects.
#[derive(Debug, Clone)]
pub struct SlotCollision {
    pub new_statement: String,
    pub old_id: String,
    pub old_statement: String,
}

/// Hard cap on collisions judged per drain — bounds the LLM cost.
const MAX_COLLISIONS: usize = 40;

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Is `e` (existing) strictly OLDER than `n` (new) — i.e. is `n` a candidate
/// UPDATE of `e`? (Memory-SOTA Phase 4a: event-time beats ingest-time.)
///
/// The stale-value failure this fixes: "I worked at Orion until March" said
/// in session 5, "I work at Acme since April" in session 2 — ingest order is
/// backwards, EVENT order is truth. Rules, adversarial-review guard included:
///
/// - **Both explicitly dated** → compare event dates (the truth axis).
/// - **Old dated, new undated** → NEVER a candidate: an undated newcomer's
///   ingest-time fallback must not close an explicitly-dated predecessor.
/// - **Otherwise** (new dated / neither dated) → today's ingest-time
///   ordering — the pre-Phase-4a behaviour, unchanged.
fn is_older(e: &FactRef, n: &FactRef) -> bool {
    match (e.event_date, n.event_date) {
        (Some(ev_e), Some(ev_n)) => ev_e < ev_n,
        (Some(_), None) => false, // guard: dated predecessor is protected
        _ => e.valid_from < n.valid_from,
    }
}

/// Find `(subject, predicate)` slot collisions: existing LIVE facts that share a
/// new fact's subject+predicate but assert a different object and are strictly
/// older (per [`is_older`] — event-time when both dated, else ingest-time).
/// Pure. Capped at [`MAX_COLLISIONS`] (oldest-first is irrelevant; we just
/// bound the count). Self-collisions (same fact id) are excluded.
pub fn find_collisions(new: &[FactRef], existing: &[FactRef]) -> Vec<SlotCollision> {
    let mut out = Vec::new();
    for n in new {
        let (ns, np, no) = (norm(&n.subject), norm(&n.predicate), norm(&n.object));
        if ns.is_empty() || np.is_empty() {
            continue;
        }
        for e in existing {
            if e.id == n.id {
                continue;
            }
            if norm(&e.subject) == ns
                && norm(&e.predicate) == np
                && norm(&e.object) != no
                && is_older(e, n)
            {
                out.push(SlotCollision {
                    new_statement: n.statement.clone(),
                    old_id: e.id.clone(),
                    old_statement: e.statement.clone(),
                });
                if out.len() >= MAX_COLLISIONS {
                    return out;
                }
            }
        }
    }
    out
}

/// System prompt: per-collision UPDATE vs BOTH-TRUE verdict, biased toward keep-both.
pub fn judge_system() -> String {
    "You maintain a knowledge graph's currentness. You are given a JSON array of pairs; each \
has an OLD statement and a NEW statement about the same subject and relation. For EACH pair \
decide whether the NEW statement UPDATES (replaces) the OLD one, so the old should be retired:\n\
- `i`: the pair's index (copied from the input).\n\
- `supersede`: boolean. TRUE only when the relation is single-valued and the value CHANGED, so \
the old is no longer true (e.g. OLD \"works at Orion Labs\" / NEW \"works at Acme Corp\"; OLD \
\"deadline is March\" / NEW \"deadline is May\"). FALSE when BOTH can be true at once — a \
multi-valued relation (OLD \"likes coffee\" / NEW \"likes tea\"; OLD \"visited Paris\" / NEW \
\"visited Tokyo\"), or when you are unsure.\n\
Rules: default to FALSE on any doubt — retiring a still-true fact loses real knowledge. Output \
ONLY a JSON array, one object per input pair, same order, no markdown fences."
        .to_string()
}

/// Build the judge prompt: a compact JSON array of `{i, old, new}`.
pub fn build_judge_prompt(collisions: &[SlotCollision]) -> String {
    let arr: Vec<serde_json::Value> = collisions
        .iter()
        .enumerate()
        .map(|(i, c)| {
            serde_json::json!({
                "i": i,
                "old": c.old_statement.chars().take(220).collect::<String>(),
                "new": c.new_statement.chars().take(220).collect::<String>(),
            })
        })
        .collect();
    let json = serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string());
    format!("Pairs:\n{json}\n\nVerdicts (JSON array, same order):")
}

/// Parse the judge response into a per-collision supersede verdict, aligned by
/// the echoed `i`. **Keep-both on doubt:** any pair omitted, or not an explicit
/// `true`, defaults to `false`.
pub fn parse_verdicts(resp: &str, n: usize) -> Vec<bool> {
    let arr: Vec<serde_json::Value> = extract_json_array(resp)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let mut out = vec![false; n];
    for v in &arr {
        let Some(i) = v.get("i").and_then(|i| i.as_u64()) else {
            continue;
        };
        let i = i as usize;
        if i < n {
            out[i] = v.get("supersede").and_then(|s| s.as_bool()).unwrap_or(false);
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

/// High-level: find collisions, judge them, and return the OLD fact ids to
/// tombstone. On LLM error, returns empty (keep everything — the safe default).
pub async fn superseded_old_ids(
    llm: &LlmClient,
    new: &[FactRef],
    existing: &[FactRef],
) -> Vec<String> {
    let collisions = find_collisions(new, existing);
    if collisions.is_empty() {
        return Vec::new();
    }
    let verdicts = match llm.chat(&judge_system(), &build_judge_prompt(&collisions)).await {
        Ok(resp) => parse_verdicts(&resp, collisions.len()),
        Err(e) => {
            tracing::warn!("fact supersession judge failed ({e}); keeping all facts live");
            return Vec::new();
        }
    };
    let mut ids = Vec::new();
    for (c, supersede) in collisions.iter().zip(verdicts.into_iter()) {
        if supersede && !ids.contains(&c.old_id) {
            ids.push(c.old_id.clone());
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fr(id: &str, subj: &str, pred: &str, obj: &str, vf: f64) -> FactRef {
        FactRef {
            id: id.into(),
            subject: subj.into(),
            predicate: pred.into(),
            object: obj.into(),
            statement: format!("{subj} {pred} {obj}"),
            valid_from: vf,
            event_date: None,
        }
    }

    fn fr_dated(id: &str, subj: &str, pred: &str, obj: &str, vf: f64, ev: f64) -> FactRef {
        FactRef {
            event_date: Some(ev),
            ..fr(id, subj, pred, obj, vf)
        }
    }

    #[test]
    fn collision_on_same_slot_different_object_older() {
        let new = vec![fr("n1", "Priya", "works at", "Acme Corp", 100.0)];
        let existing = vec![
            fr("o1", "Priya", "works at", "Orion Labs", 50.0), // older, diff object → collision
            fr("o2", "Priya", "likes", "coffee", 50.0),        // diff predicate → no collision
            fr("o3", "Priya", "works at", "Acme Corp", 50.0),  // same object → no collision
        ];
        let c = find_collisions(&new, &existing);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].old_id, "o1");
    }

    #[test]
    fn no_collision_when_existing_is_newer() {
        // Existing fact is NEWER than the "new" one → not an update of it.
        let new = vec![fr("n1", "Priya", "works at", "Acme", 10.0)];
        let existing = vec![fr("o1", "Priya", "works at", "Orion", 50.0)];
        assert!(find_collisions(&new, &existing).is_empty());
    }

    #[test]
    fn no_self_collision() {
        let f = fr("x", "A", "is", "B", 10.0);
        // Same id present on both sides → never collides with itself.
        assert!(find_collisions(&[f.clone()], &[f]).is_empty());
    }

    #[test]
    fn event_time_beats_ingest_time_when_both_dated() {
        // Sessions discussed out of order: the OLD row was INGESTED later
        // (vf 200 > 100) but its EVENT is earlier (March < April). Event
        // order is truth → collision fires despite backwards ingest order.
        let new = vec![fr_dated("n1", "Priya", "works at", "Acme", 100.0, 1_680_300_000.0)];
        let existing = vec![fr_dated("o1", "Priya", "works at", "Orion", 200.0, 1_677_600_000.0)];
        let c = find_collisions(&new, &existing);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].old_id, "o1");
        // And the reverse: an event-OLDER new fact must NOT supersede an
        // event-newer existing one, even if ingested later.
        let new = vec![fr_dated("n2", "Priya", "works at", "Zeta", 300.0, 1_600_000_000.0)];
        let existing = vec![fr_dated("o2", "Priya", "works at", "Acme", 100.0, 1_680_300_000.0)];
        assert!(find_collisions(&new, &existing).is_empty());
    }

    #[test]
    fn undated_newcomer_never_closes_dated_predecessor() {
        // Guard (adversarial-review revision): the existing fact carries an
        // explicit event date; the new fact is undated (ingest-time fallback
        // only). Even with a newer valid_from, no collision.
        let new = vec![fr("n1", "Priya", "works at", "Acme", 500.0)];
        let existing = vec![fr_dated("o1", "Priya", "works at", "Orion", 50.0, 1_677_600_000.0)];
        assert!(find_collisions(&new, &existing).is_empty());
    }

    #[test]
    fn dated_newcomer_vs_undated_old_falls_back_to_ingest_order() {
        // New is dated, old is not → pre-Phase-4a ingest ordering decides.
        let new = vec![fr_dated("n1", "Priya", "works at", "Acme", 100.0, 1_680_300_000.0)];
        let older = vec![fr("o1", "Priya", "works at", "Orion", 50.0)];
        assert_eq!(find_collisions(&new, &older).len(), 1);
        let newer = vec![fr("o2", "Priya", "works at", "Orion", 500.0)];
        assert!(find_collisions(&new, &newer).is_empty());
    }

    #[test]
    fn parse_keeps_both_on_doubt() {
        // index 1 omitted, index 2 false, index 0 true.
        let resp = r#"[{"i":0,"supersede":true},{"i":2,"supersede":false}]"#;
        assert_eq!(parse_verdicts(resp, 3), vec![true, false, false]);
    }

    #[test]
    fn parse_tolerates_garbage() {
        assert_eq!(parse_verdicts("not json", 2), vec![false, false]);
        assert_eq!(parse_verdicts("```json\n[{\"i\":0,\"supersede\":true}]\n```", 1), vec![true]);
    }

    #[test]
    fn build_prompt_embeds_pairs() {
        let c = find_collisions(
            &[fr("n1", "Priya", "works at", "Acme", 100.0)],
            &[fr("o1", "Priya", "works at", "Orion", 50.0)],
        );
        let p = build_judge_prompt(&c);
        assert!(p.contains("Orion"));
        assert!(p.contains("Acme"));
        assert!(p.contains("JSON array"));
    }
}
