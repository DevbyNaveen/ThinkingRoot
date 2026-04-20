// crates/thinkingroot-serve/src/intelligence/react.rs
//
// Agentic ReAct (Reason + Act) path for the 20% of queries that require
// multi-turn temporal reasoning or multi-hop graph traversal.
//
// Architecture
// ────────────
// ReActEngine::run() executes up to MAX_TURNS steps:
//
//   Turn 1 — Temporal expansion + event calendar lookup
//   Turn 2 — Semantic vector search
//   Turn 3 — K=V fact key expansion (entity-scoped claim retrieval)
//   Synthesis — LLM synthesis from collected notes (falls back to Chain-of-Note)
//
// Synthesis uses the workspace LLM client if available to generate a fluent,
// accurate natural-language answer from all retrieved memory notes.  Without
// an LLM the notes are concatenated directly (Chain-of-Note fallback).
//
// LongMemEval categories covered:
//   TR  — temporal reasoning     (Turn 1: event calendar)
//   SSP — preference retrieval   (Turn 3: K=V with claim_type=preference)
//   MS  — multi-session          (Turn 2+3: semantic + entity expansion)
//   ABS — abstention             (LLM synthesis detects missing evidence)

use std::sync::Arc;

use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc};

use crate::engine::{ClaimFilter, QueryEngine};
use crate::intelligence::session::SessionContext;

const MAX_TURNS: usize = 5;

/// System prompt for the memory-assistant synthesis step.
const SYNTHESIS_SYSTEM_PROMPT: &str = "\
You are a precise personal memory assistant. \
You are given retrieved memory notes and a question. \
Answer the question using ONLY the information in the notes. \
Rules: \
(1) Be concise and specific — answer in 1-3 sentences max. \
(2) TEMPORAL ORDERING: When the question asks which event happened FIRST or LAST, compare the \
dates shown in [YYYY-MM-DD] or [X days ago] brackets. The earliest calendar date = happened first. \
(3) DATE ARITHMETIC: For 'how many days/months between X and Y', compute the difference between \
the dates shown in the notes. \
(4) KNOWLEDGE UPDATES: If multiple claims contradict each other, the claim with the HIGHER \
confidence or MORE RECENT date is correct. Phrases like 'currently', 'now', 'updated' signal \
the latest value. \
(5) COUNTING: Count only the distinct items explicitly mentioned in the notes. \
(6) PREFERENCES: Preference claims tagged [preference] are direct answers to 'what does X like/prefer'. \
(7) If the answer is genuinely not in the notes, respond with exactly: \
\"I don't have enough information to answer that.\"";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which retrieval tool a ReAct turn used.
#[derive(Debug, Clone)]
pub enum ReActTool {
    SearchEvents {
        entity: String,
        start_iso: String,
        end_iso: String,
    },
    SearchClaims {
        query: String,
        claim_type: Option<String>,
    },
    TraverseGraph {
        entity: String,
    },
}

/// A single (thought, tool, observation) step in the ReAct loop.
#[derive(Debug, Clone)]
pub struct ReActStep {
    pub thought: String,
    pub tool: ReActTool,
    pub observation: String,
}

/// The result returned by ReActEngine::run().
#[derive(Debug, Clone)]
pub struct ReActResult {
    pub answer: String,
    pub steps: Vec<ReActStep>,
    pub turns_used: usize,
}

// ---------------------------------------------------------------------------
// ReActEngine
// ---------------------------------------------------------------------------

pub struct ReActEngine<'q> {
    engine: &'q QueryEngine,
    ws: &'q str,
    llm: Option<Arc<thinkingroot_extract::llm::LlmClient>>,
}

impl<'q> ReActEngine<'q> {
    pub fn new(
        engine: &'q QueryEngine,
        ws: &'q str,
        llm: Option<Arc<thinkingroot_extract::llm::LlmClient>>,
    ) -> Self {
        Self { engine, ws, llm }
    }

    /// Execute the ReAct loop for the given query.
    /// Delegates to `run_with_anchor` with no temporal anchor (uses wall-clock).
    pub async fn run(&self, query: &str, session: &SessionContext) -> ReActResult {
        self.run_with_anchor(query, session, None).await
    }

    /// Execute the ReAct loop with an explicit temporal anchor.
    ///
    /// `temporal_anchor` is a Unix timestamp representing "now" from the user's
    /// perspective — typically the max event_date in the knowledge base.  When
    /// `Some`, relative markers ("last month", "3 days ago") are resolved relative
    /// to this date rather than the wall-clock time, which is critical for
    /// personal-memory workspaces where sessions were recorded months or years ago.
    pub async fn run_with_anchor(
        &self,
        query: &str,
        _session: &SessionContext,
        temporal_anchor: Option<f64>,
    ) -> ReActResult {
        let mut steps: Vec<ReActStep> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        // ── Turn 1: Temporal expansion + event calendar ───────────────────
        // Two sub-queries:
        //   (a) time-range scan  — events in the expanded date window
        //   (b) entity full-scan — ALL events for the identified entity (provides
        //       full chronological context so the synthesis LLM can compare dates
        //       and answer ordering questions like "which happened first?")
        if let Some((start_iso, end_iso)) = expand_temporal_query_anchored(query, temporal_anchor) {
            let start_ts = iso_to_unix(&start_iso).unwrap_or(0.0);
            let end_ts = iso_to_unix(&end_iso).unwrap_or(f64::MAX);

            let entity = extract_subject_entity(query).unwrap_or_default();
            let thought = format!(
                "Query contains temporal signal. Expanding '{query}' → [{start_iso}, {end_iso}]. \
                 Also fetching full event history for entity '{entity}'."
            );

            let observation = match self
                .engine
                .query_events_in_range(self.ws, start_ts, end_ts)
                .await
            {
                Ok(events) if !events.is_empty() => {
                    let mut obs = format!("## Events [{start_iso} → {end_iso}]\n");
                    for ev in events.iter().take(20) {
                        let date_label = if !ev.normalized_date.is_empty() {
                            format!(" ({})", ev.normalized_date)
                        } else {
                            String::new()
                        };
                        let object_part = if !ev.object_name.is_empty() {
                            format!(" {}", ev.object_name)
                        } else {
                            String::new()
                        };
                        obs.push_str(&format!(
                            "- {subject} {verb}{object}{date}\n",
                            subject = ev.subject_name,
                            verb = ev.verb,
                            object = object_part,
                            date = date_label,
                        ));
                    }
                    obs
                }
                Ok(_) => format!("No events found in range [{start_iso}, {end_iso}].\n"),
                Err(e) => format!("Event calendar query failed: {e}\n"),
            };

            // Augment with full entity event history for ordering context.
            // Even when the time-range scan found nothing, the entity's full
            // chronological timeline lets the synthesis LLM compare event dates.
            let mut final_observation = observation.clone();
            if !entity.is_empty()
                && let Ok(entity_events) = self
                    .engine
                    .query_events_in_range(self.ws, 0.0, f64::MAX)
                    .await
            {
                let entity_lower = entity.to_lowercase();
                let entity_hits: Vec<_> = entity_events
                    .iter()
                    .filter(|ev| {
                        ev.subject_name.to_lowercase().contains(&entity_lower)
                            || ev.object_name.to_lowercase().contains(&entity_lower)
                    })
                    .take(30)
                    .collect();
                if !entity_hits.is_empty() {
                    final_observation.push_str(&format!(
                        "\n## All Events for '{}' (chronological context)\n",
                        entity
                    ));
                    let mut sorted = entity_hits.clone();
                    sorted.sort_by_key(|a| a.normalized_date.clone());
                    for ev in sorted {
                        let date = if !ev.normalized_date.is_empty() {
                            format!(" [{}]", ev.normalized_date)
                        } else {
                            String::new()
                        };
                        let obj = if !ev.object_name.is_empty() {
                            format!(" {}", ev.object_name)
                        } else {
                            String::new()
                        };
                        final_observation.push_str(&format!(
                            "- {}{} {}{}\n",
                            ev.subject_name, date, ev.verb, obj
                        ));
                    }
                }
            }

            notes.push(final_observation.clone());
            steps.push(ReActStep {
                thought,
                tool: ReActTool::SearchEvents {
                    entity,
                    start_iso,
                    end_iso,
                },
                observation: final_observation,
            });
        }

        // ── Turn 1b: Full timeline scan for ordering queries ─────────────
        // When Turn 1 ran but the time-range expansion was None (query has ordering
        // signals like "which did I buy first?" but no relative time keywords),
        // fetch the entity's full event timeline so the synthesis LLM can compare dates.
        if steps.is_empty()
            && let Some(entity_name) = extract_subject_entity(query)
        {
            let lower_q = query.to_lowercase();
            let needs_ordering = lower_q.contains(" first")
                || lower_q.contains("came first")
                || lower_q.contains("happened first")
                || lower_q.contains("before")
                || lower_q.contains("after")
                || lower_q.contains("how many days")
                || lower_q.contains("how many months")
                || lower_q.contains("how long");
            if needs_ordering
                && let Ok(all_events) = self
                    .engine
                    .query_events_in_range(self.ws, 0.0, f64::MAX)
                    .await
            {
                let entity_lower = entity_name.to_lowercase();
                let mut relevant: Vec<_> = all_events
                    .into_iter()
                    .filter(|ev| {
                        ev.subject_name.to_lowercase().contains(&entity_lower)
                            || ev.object_name.to_lowercase().contains(&entity_lower)
                    })
                    .collect();
                if !relevant.is_empty() {
                    relevant.sort_by_key(|a| a.normalized_date.clone());
                    let mut obs = format!(
                        "## Full Event Timeline for '{}' (for ordering comparison)\n",
                        entity_name
                    );
                    for ev in relevant.iter().take(40) {
                        let date = if !ev.normalized_date.is_empty() {
                            format!(" [{}]", ev.normalized_date)
                        } else {
                            String::new()
                        };
                        let obj = if !ev.object_name.is_empty() {
                            format!(" {}", ev.object_name)
                        } else {
                            String::new()
                        };
                        obs.push_str(&format!(
                            "- {}{} {}{}\n",
                            ev.subject_name, date, ev.verb, obj
                        ));
                    }
                    notes.push(obs.clone());
                    steps.push(ReActStep {
                        thought: format!(
                            "Fetching full event timeline for '{entity_name}' to support ordering comparison."
                        ),
                        tool: ReActTool::SearchEvents {
                            entity: entity_name,
                            start_iso: "all".to_string(),
                            end_iso: "all".to_string(),
                        },
                        observation: obs,
                    });
                }
            }
        }

        if steps.len() >= MAX_TURNS {
            return self.synthesize(query, notes, steps);
        }

        // ── Turn 2: Semantic vector search ───────────────────────────────
        {
            let thought = format!("Performing semantic search for '{query}'.");
            let observation = match self.engine.search(self.ws, query, 10).await {
                Ok(result) => {
                    let mut obs = format!("## Semantic Search: '{query}'\n");
                    if result.entities.is_empty() && result.claims.is_empty() {
                        obs.push_str("No results found.\n");
                    }
                    for hit in result.entities.iter().take(5) {
                        obs.push_str(&format!("Entity: {} ({})\n", hit.name, hit.entity_type));
                    }
                    for hit in result.claims.iter().take(10) {
                        obs.push_str(&format!(
                            "Claim [{:.2}]: {}\n",
                            hit.relevance, hit.statement
                        ));
                    }
                    obs
                }
                Err(e) => format!("Semantic search failed: {e}\n"),
            };

            notes.push(observation.clone());
            steps.push(ReActStep {
                thought,
                tool: ReActTool::SearchClaims {
                    query: query.to_string(),
                    claim_type: None,
                },
                observation,
            });
        }

        if steps.len() >= MAX_TURNS {
            return self.synthesize(query, notes, steps);
        }

        // ── Turn 3: K=V fact key expansion ───────────────────────────────
        // Extract the subject entity from the query and retrieve all claims
        // about it.  This is the "K=V" technique from the LongMemEval paper
        // (+5% accuracy) — augmenting stored values with extracted facts.
        if let Some(entity_name) = extract_subject_entity(query) {
            let thought = format!("Expanding facts for entity '{entity_name}'.");
            let claim_type = infer_claim_type_hint(query);

            // For counting/aggregation queries, fetch more claims to ensure all
            // mentions across multi-session timelines are captured.
            let lower_q = query.to_lowercase();
            let is_counting = lower_q.contains("how many")
                || lower_q.contains("how much")
                || lower_q.contains("how often")
                || lower_q.contains("total")
                || lower_q.contains("count")
                || lower_q.contains("all the")
                || lower_q.contains("list all")
                || lower_q.contains("every ");
            let claim_limit = if is_counting { 50 } else { 20 };

            let filter = ClaimFilter {
                entity_name: Some(entity_name.clone()),
                claim_type: claim_type.clone(),
                limit: Some(claim_limit),
                ..Default::default()
            };

            let observation = match self.engine.list_claims(self.ws, filter).await {
                Ok(claims) if !claims.is_empty() => {
                    let mut obs = format!("## Facts about '{entity_name}'");
                    if let Some(ref ct) = claim_type {
                        obs.push_str(&format!(" (type={ct})"));
                    }
                    obs.push('\n');
                    for c in claims.iter().take(claim_limit) {
                        // Temporal label: show when the event happened, not when it was ingested.
                        let temporal = c.event_date.and_then(|ts| {
                            let dt = Utc.timestamp_opt(ts as i64, 0).single()?;
                            let days = (Utc::now() - dt).num_days();
                            Some(if days == 0 {
                                "today".to_string()
                            } else if days < 365 {
                                format!("{days} days ago")
                            } else {
                                dt.format("%Y-%m-%d").to_string()
                            })
                        });
                        match temporal {
                            Some(label) => obs.push_str(&format!(
                                "[{:.2}][{label}] {}\n",
                                c.confidence, c.statement
                            )),
                            None => {
                                obs.push_str(&format!("[{:.2}] {}\n", c.confidence, c.statement))
                            }
                        }
                    }
                    obs
                }
                Ok(_) => format!("No facts found for '{entity_name}'.\n"),
                Err(e) => format!("Fact expansion failed: {e}\n"),
            };

            notes.push(observation.clone());
            steps.push(ReActStep {
                thought,
                tool: ReActTool::TraverseGraph {
                    entity: entity_name.clone(),
                },
                observation,
            });

            // ── Turn 4: Preference second-pass ────────────────────────────────
            // Always run a dedicated preference fetch for the identified entity —
            // regardless of whether the query contains preference keywords.
            // SSP-category questions (e.g. "what should I get Alice for her birthday?")
            // don't contain "prefer/like/favourite" but the answer lives in preference
            // claims. This turn is cheap (in-memory cache filter) and only adds notes.
            if steps.len() < MAX_TURNS {
                let pref_filter = ClaimFilter {
                    entity_name: Some(entity_name.clone()),
                    claim_type: Some("preference".to_string()),
                    limit: Some(15),
                    ..Default::default()
                };
                let pref_thought = format!(
                    "Fetching preference claims for '{entity_name}' to cover implicit preference questions."
                );
                let pref_observation = match self.engine.list_claims(self.ws, pref_filter).await {
                    Ok(prefs) if !prefs.is_empty() => {
                        let mut obs = format!("## Preferences of '{entity_name}'\n");
                        for p in prefs.iter().take(15) {
                            obs.push_str(&format!("[preference] {}\n", p.statement));
                        }
                        obs
                    }
                    Ok(_) => String::new(), // No preferences — don't add noise to notes
                    Err(_) => String::new(),
                };
                if !pref_observation.is_empty() {
                    notes.push(pref_observation.clone());
                    steps.push(ReActStep {
                        thought: pref_thought,
                        tool: ReActTool::SearchClaims {
                            query: format!("{entity_name} preferences"),
                            claim_type: Some("preference".to_string()),
                        },
                        observation: pref_observation,
                    });
                }
            }
        }

        self.synthesize_async(query, notes, steps).await
    }

    fn synthesize(&self, query: &str, notes: Vec<String>, steps: Vec<ReActStep>) -> ReActResult {
        let turns_used = steps.len();
        // Chain-of-Note fallback (used when LLM is not available).
        let mut answer = format!("## ThinkingRoot — Analysis: {query}\n\n");
        for note in &notes {
            answer.push_str(note);
            answer.push('\n');
        }
        if notes.is_empty() {
            answer.push_str("No relevant information found in the knowledge base.\n");
        }
        ReActResult {
            answer,
            steps,
            turns_used,
        }
    }

    async fn synthesize_async(
        &self,
        query: &str,
        notes: Vec<String>,
        steps: Vec<ReActStep>,
    ) -> ReActResult {
        let turns_used = steps.len();

        if let Some(ref llm) = self.llm {
            // Build the user message: present all notes then ask the question.
            let notes_block = if notes.is_empty() {
                "No memory notes were retrieved.".to_string()
            } else {
                notes.join("\n\n")
            };
            let user_msg =
                format!("## Retrieved Memory Notes\n\n{notes_block}\n\n## Question\n\n{query}");

            match llm.chat(SYNTHESIS_SYSTEM_PROMPT, &user_msg).await {
                Ok(answer) => {
                    return ReActResult {
                        answer,
                        steps,
                        turns_used,
                    };
                }
                Err(e) => {
                    tracing::warn!("LLM synthesis failed, falling back to Chain-of-Note: {e}");
                }
            }
        }

        // Fallback: Chain-of-Note concatenation.
        self.synthesize(query, notes, steps)
    }
}

// ---------------------------------------------------------------------------
// Helper functions (pure, no I/O)
// ---------------------------------------------------------------------------

/// Expand a query containing a temporal marker to an ISO date range.
///
/// Returns `Some((start_iso, end_iso))` when a temporal marker is detected.
/// Both strings are `YYYY-MM-DD` format.
/// Expand a temporal query using an explicit anchor date.
///
/// `anchor_ts` — Unix epoch of the reference "now" for the workspace (max event_date).
/// Falls back to wall-clock when None (e.g., for live session queries).
pub fn expand_temporal_query_anchored(q: &str, anchor_ts: Option<f64>) -> Option<(String, String)> {
    let today = match anchor_ts {
        Some(ts) => Utc
            .timestamp_opt(ts as i64, 0)
            .single()
            .map(|dt| dt.date_naive())
            .unwrap_or_else(|| Utc::now().date_naive()),
        None => Utc::now().date_naive(),
    };
    expand_temporal_query_from_date(q, today)
}

pub fn expand_temporal_query(q: &str) -> Option<(String, String)> {
    expand_temporal_query_from_date(q, Utc::now().date_naive())
}

fn expand_temporal_query_from_date(q: &str, today: NaiveDate) -> Option<(String, String)> {
    let lower = q.to_lowercase();

    if lower.contains("yesterday") {
        let d = today - Duration::days(1);
        return Some((d.to_string(), d.to_string()));
    }
    if lower.contains("today") {
        return Some((today.to_string(), today.to_string()));
    }
    if lower.contains("last week") {
        let start = today - Duration::days(7);
        let end = today - Duration::days(1);
        return Some((start.to_string(), end.to_string()));
    }
    if lower.contains("this week") {
        let mon = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        return Some((mon.to_string(), today.to_string()));
    }
    if lower.contains("last month") {
        let first_this = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)?;
        let last_prev = first_this - Duration::days(1);
        let first_prev = NaiveDate::from_ymd_opt(last_prev.year(), last_prev.month(), 1)?;
        return Some((first_prev.to_string(), last_prev.to_string()));
    }
    if lower.contains("this month") {
        let start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)?;
        return Some((start.to_string(), today.to_string()));
    }
    if lower.contains("last year") {
        let start = NaiveDate::from_ymd_opt(today.year() - 1, 1, 1)?;
        let end = NaiveDate::from_ymd_opt(today.year() - 1, 12, 31)?;
        return Some((start.to_string(), end.to_string()));
    }

    // "last Monday" … "last Sunday"
    const DAYS: &[(&str, u32)] = &[
        ("last monday", 0),
        ("last tuesday", 1),
        ("last wednesday", 2),
        ("last thursday", 3),
        ("last friday", 4),
        ("last saturday", 5),
        ("last sunday", 6),
    ];
    for (kw, wd) in DAYS {
        if lower.contains(kw) {
            let today_wd = today.weekday().num_days_from_monday();
            let days_back = if today_wd >= *wd {
                today_wd - wd
            } else {
                7 - (wd - today_wd)
            };
            let d = today - Duration::days(days_back as i64);
            return Some((d.to_string(), d.to_string()));
        }
    }

    None
}

/// Extract the most likely subject entity name from a query.
/// Heuristics: PascalCase words or single-quoted/double-quoted names.
pub fn extract_subject_entity(q: &str) -> Option<String> {
    // Quoted name: 'Alice' or "Alice"
    let re_quoted = regex::Regex::new(r#"['"]([^'"]{2,40})['"]"#).ok()?;
    if let Some(cap) = re_quoted.captures(q) {
        return Some(cap[1].to_string());
    }

    // PascalCase word (e.g., "Alice", "ThinkingRoot", "AuthService")
    let re_pascal = regex::Regex::new(r"\b([A-Z][a-zA-Z0-9]{1,39})\b").ok()?;
    let candidates: Vec<String> = re_pascal
        .captures_iter(q)
        .map(|c| c[1].to_string())
        .filter(|s: &String| {
            // Skip common sentence-starters that aren't entities.
            !matches!(
                s.as_str(),
                "What"
                    | "When"
                    | "Where"
                    | "Who"
                    | "Why"
                    | "How"
                    | "Did"
                    | "Does"
                    | "Has"
                    | "Have"
                    | "Is"
                    | "Are"
                    | "Can"
                    | "Could"
                    | "Would"
                    | "Should"
                    | "Tell"
                    | "Show"
                    | "Give"
                    | "Get"
                    | "Find"
                    | "List"
            )
        })
        .collect();

    // Prefer the first entity that looks substantive (> 2 chars)
    if let Some(entity) = candidates.into_iter().find(|s: &String| s.len() > 2) {
        return Some(entity);
    }

    // First-person fallback: "I", "my", "me", "we", "our" → entity = "User"
    // Personal-memory sessions extract the conversation user as entity "User"
    // (per extraction prompt convention).  Without this fallback, Turn 3 + Turn 4
    // are silently skipped for the majority of benchmark questions.
    let lower = q.to_lowercase();
    let first_person = lower.starts_with("i ")
        || lower.contains(" i ")
        || lower.contains(" my ")
        || lower.contains(" me ")
        || lower.contains(" i've")
        || lower.contains(" i'm")
        || lower.contains(" i'd")
        || lower.contains(" i'll")
        || lower.starts_with("my ")
        || lower.starts_with("did i")
        || lower.starts_with("what did i")
        || lower.starts_with("when did i")
        || lower.starts_with("where did i")
        || lower.starts_with("which did i")
        || lower.starts_with("how did i")
        || lower.starts_with("which")   // "which ... did i ..."
        || lower.starts_with("how long")
        || lower.starts_with("how many");
    if first_person {
        return Some("User".to_string());
    }

    None
}

/// Infer a `claim_type` hint from the query text.
pub fn infer_claim_type_hint(q: &str) -> Option<String> {
    let lower = q.to_lowercase();
    if lower.contains("prefer") || lower.contains("like") || lower.contains("favourite") {
        return Some("preference".to_string());
    }
    if lower.contains("decided") || lower.contains("decision") || lower.contains("chose") {
        return Some("decision".to_string());
    }
    if lower.contains("plan") || lower.contains("intend") || lower.contains("going to") {
        return Some("plan".to_string());
    }
    None
}

/// Parse an ISO 8601 date string ("YYYY-MM-DD") to Unix epoch (f64).
pub fn iso_to_unix(iso: &str) -> Option<f64> {
    NaiveDate::parse_from_str(iso, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp() as f64)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_last_week_produces_range() {
        let result = expand_temporal_query("what did Alice do last week?");
        assert!(result.is_some(), "expected a range for 'last week'");
        let (start, end) = result.unwrap();
        assert!(start <= end, "start should be <= end");
    }

    #[test]
    fn expand_yesterday_produces_same_day() {
        let result = expand_temporal_query("what happened yesterday?");
        assert!(result.is_some());
        let (start, end) = result.unwrap();
        assert_eq!(start, end, "yesterday is a single day");
    }

    #[test]
    fn no_temporal_marker_returns_none() {
        let result = expand_temporal_query("what are Alice's skills?");
        assert!(result.is_none(), "no temporal marker → None");
    }

    #[test]
    fn extract_entity_from_pascal_case() {
        let entity = extract_subject_entity("What did Alice do?");
        assert_eq!(entity.as_deref(), Some("Alice"));
    }

    #[test]
    fn extract_entity_from_quoted() {
        let entity = extract_subject_entity("What did 'AuthService' return?");
        assert_eq!(entity.as_deref(), Some("AuthService"));
    }

    #[test]
    fn infer_preference_hint() {
        assert_eq!(
            infer_claim_type_hint("what food does Alice prefer?"),
            Some("preference".to_string())
        );
    }

    #[test]
    fn iso_to_unix_roundtrip() {
        let ts = iso_to_unix("2025-03-15").unwrap();
        assert!(ts > 0.0);
    }
}
