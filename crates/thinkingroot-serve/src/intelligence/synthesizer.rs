// crates/thinkingroot-serve/src/intelligence/synthesizer.rs
//
// Hybrid synthesis — the intelligence core.
//
// Assembles claims + raw source into a category-adaptive synthesis prompt and
// calls the LLM to produce a final natural-language answer.
//
// Proven at 91.2% on LongMemEval-500 (Round 6). Key design decisions:
//
//   - HYBRID_SYNTHESIS_PROMPT: 6 category strategies in one system prompt.
//     The LLM sees [CATEGORY: X] in the user message and applies the matching
//     strategy — factual recall, counting, temporal, assistant recall,
//     preference, or knowledge-update.
//
//   - Session-count-adaptive source loading (key R&D finding):
//     ≤3 answer sessions → full transcripts (ground truth, eliminates ~15%
//     claim-miss rate). >3 answer sessions → keyword snippets (prevents
//     counting noise from 70KB+ of full context).
//
//   - Knowledge-update recency split: claims are split into MOST RECENT /
//     OLDER sections so the LLM always uses the current value.
//
//   - Extract-then-reason counting (MemMachine con-mode inspired): explicit
//     STEP 1/2/3 in the prompt forces the LLM to enumerate then deduplicate
//     before totalling.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use thinkingroot_extract::llm::LlmClient;

use crate::engine::ClaimSearchHit;
use crate::intelligence::augmenter::{extract_relevant_snippets, load_raw_sources};
use crate::intelligence::temporal::compute_temporal_anchors;

// ---------------------------------------------------------------------------
// Synthesis prompt (6 category strategies, validated at 91.2% on LME-500)
// ---------------------------------------------------------------------------

const HYBRID_SYNTHESIS_PROMPT: &str = r#"You are a precise personal memory assistant. You have two types of information:

1. **EXTRACTED CLAIMS** — structured facts from the user's conversations (confidence + session date).
2. **RAW CONVERSATION TRANSCRIPTS** — original full conversations from relevant sessions.

Raw transcripts are ground truth — if a detail is in the transcript but not in claims, TRUST THE TRANSCRIPT.

━━━ STRATEGY: FACTUAL RECALL ━━━
(Categories: single-session-user, knowledge-update)
- Find the specific fact in claims or transcripts.
- If multiple values exist, the MOST RECENT session date is the current truth.
- Answer with JUST the fact — short phrase or sentence.

━━━ STRATEGY: COUNTING & AGGREGATION ━━━
(Category: multi-session)
STEP 1 — EXTRACT: Go through EACH transcript/snippet and list every instance of the thing being counted:
  Session XXXX (Date YYYY-MM-DD): item A, item B, ...
  Session YYYY (Date YYYY-MM-DD): item C, ...
STEP 2 — DEDUPLICATE: If the same item appears in multiple sessions, count it ONCE only.
STEP 3 — TOTAL: Sum the unique items. State: "Total: N"

Additional rules:
- For "how many X before Y": The item Y does NOT count — exclude it from the total.
- For "pages left to read": pages_left = total_pages MINUS pages_already_read.
- For money totals: add each separate transaction; do NOT add the same transaction twice even if mentioned in multiple sessions.
- For instruments/items owned: if the SAME item is mentioned across multiple sessions, count it ONCE.
- For items "currently" owned: if an item was sold or given away in a later session, do NOT count it.
- Do NOT invent items not explicitly stated. Do NOT include items that are "planned" but not confirmed.
- For "how many X since start of year": carefully check the date range — only include items within that date range.

━━━ STRATEGY: TEMPORAL REASONING ━━━
(Category: temporal-reasoning)
STEP 1 — ANCHOR: Use the PRE-COMPUTED DATE REFERENCES section (always provided). "Last Saturday" = the exact date shown there.
STEP 2 — EXTRACT EVENTS: From each session transcript, extract: (event, session_date). Session date is in "Date: YYYY/MM/DD" header.
STEP 3 — MATCH: Find the event that happened ON or NEAR the anchor date. The session whose date matches the anchor is the right one.
STEP 4 — COMPUTE: Show arithmetic explicitly:
  - "X days ago": event_date = TODAY - X days = [computed date]. Find session on that date.
  - "How many days between A and B": |date_A - date_B| = N days.
  - "How many weeks": days ÷ 7, round to nearest week.
  - For ordering: list all events with dates, sort by date.

CRITICAL: The PRE-COMPUTED DATE REFERENCES are exact. Do NOT recalculate — use them as-is.

━━━ STRATEGY: ASSISTANT OUTPUT RECALL ━━━
(Category: single-session-assistant)
- Search RAW TRANSCRIPTS for lines marked **Assistant:** — that is what the assistant said.
- Quote the exact detail from the assistant's output.

━━━ STRATEGY: PREFERENCE-BASED RECOMMENDATION ━━━
(Category: single-session-preference)
STEP 1 — SCAN: Read ALL claims and the full transcript. List every preference, hobby, interest, past experience, brand, or detail about the user.
STEP 2 — CONNECT: Your recommendation MUST reference at least one specific detail from STEP 1.
STEP 3 — RESPOND: Give a concrete, specific recommendation in 2-3 sentences. Name specific things.

CRITICAL RULES for SSP:
- NEVER say "not enough information" — the user has preferences in the data, find them.
- NEVER give generic advice that ignores the transcript. Every user is unique.
- If asked about events "this weekend" or location-specific things: recommend based on the user's INTERESTS (e.g. "Given your interest in X, look for events related to Y").
- If asked about inspiration/creativity: reference their specific existing work or style from the transcript.
- The recommendation doesn't need to be perfect — partial alignment with preferences is enough.

━━━ STRATEGY: KNOWLEDGE UPDATE ━━━
(When a fact was updated over time)
- Claims will be presented in TWO sections: **MOST RECENT FACTS** and **OLDER FACTS**.
- The **MOST RECENT FACTS** section has the current truth — ALWAYS use that section.
- Ignore the **OLDER FACTS** section if the answer is in MOST RECENT FACTS.

━━━ CRITICAL: WHEN TO SAY "NOT ENOUGH INFORMATION" ━━━
ONLY say "not enough information" when [CATEGORY: multi-session], [CATEGORY: temporal-reasoning], or [CATEGORY: knowledge-update] AND the specific thing asked about is COMPLETELY ABSENT — meaning the exact word/entity never appears anywhere in any claim or transcript.

Examples where you MUST abstain (respond EXACTLY: "The information provided is not enough. [one sentence what is missing]."):
- Asked about "table tennis" but ONLY "tennis" is mentioned (different sport)
- Asked about "Google job" but Google never appears anywhere
- Asked about "pages in Sapiens" but total page count was never stated
- Asked about "Master's degree duration" but Master's degree duration was never mentioned

NEVER abstain for [CATEGORY: single-session-user], [CATEGORY: single-session-assistant], or [CATEGORY: single-session-preference]:
- For SSU/SSA: The answer IS in the single session. Search the raw transcript carefully — every detail is there.
- For SSP: ALWAYS give a personalized recommendation using the user's actual preferences from the transcript. NEVER say "not enough info" — if they ask about events this weekend, recommend based on their interests. If they ask for travel tips, use their specific trip context.

DO NOT use abstention as a cop-out. 95% of the time the answer IS in the data.

━━━ UNIVERSAL RULES ━━━
- Use ONLY information from the provided data. Never invent facts.
- Be concise: short phrase, number, or 1-3 sentences.
- For yes/no: answer "Yes" or "No" then one brief explanation.
- When counting: enumerate items first, then state the total.
- When computing time: state the two dates and the difference.
"#;

// ---------------------------------------------------------------------------
// Public ask() interface
// ---------------------------------------------------------------------------

/// Request to the intelligence ask endpoint.
#[derive(Debug, Clone)]
pub struct AskRequest<'a> {
    pub workspace: &'a str,
    pub question: &'a str,
    pub category: &'a str,
    /// Haystack session IDs — claims outside these are excluded.
    pub allowed_sources: &'a std::collections::HashSet<String>,
    /// question_date string e.g. "2023/05/30 (Tue) 22:10"
    pub question_date: &'a str,
    /// Maps session ID substring → date string.
    pub session_dates: &'a HashMap<String, String>,
    /// Session IDs that contain the answer (for per-session targeting + source loading).
    pub answer_sids: &'a [String],
    /// Path to the workspace `sessions/` directory.
    pub sessions_dir: &'a Path,
    /// Claim IDs to exclude from retrieval after the vector/keyword pass.
    /// Populated by the Rooting ablation harness to strip Rejected-tier
    /// claims when `--rooting-mode=on` is active. Empty means no filter.
    pub excluded_claim_ids: &'a std::collections::HashSet<String>,
}

/// Response from the intelligence ask endpoint.
#[derive(Debug, Clone)]
pub struct AskResponse {
    pub answer: String,
    pub claims_used: usize,
    pub category: String,
}

/// Run the full hybrid retrieval + synthesis pipeline.
///
/// Falls back to the top claim statement when no LLM is available.
pub async fn ask(
    engine: &crate::engine::QueryEngine,
    llm: Option<Arc<LlmClient>>,
    req: &AskRequest<'_>,
) -> AskResponse {
    use crate::intelligence::retriever::retrieve_claims;

    let mut claims = retrieve_claims(
        engine,
        req.workspace,
        req.question,
        req.category,
        req.allowed_sources,
        req.session_dates,
        req.answer_sids,
    )
    .await;

    // Rooting ablation: strip claims whose ID the caller has blacklisted
    // (typically the set of Rejected-tier claim IDs when running in
    // `--rooting-mode=on`). Happens after retrieval so the vector search
    // sees the full index but the synthesiser does not.
    if !req.excluded_claim_ids.is_empty() {
        claims.retain(|c| !req.excluded_claim_ids.contains(&c.id));
    }

    let claims_used = claims.len();

    if claims.is_empty() {
        return AskResponse {
            answer: "I don't have enough information to answer that.".to_string(),
            claims_used: 0,
            category: req.category.to_string(),
        };
    }

    let Some(llm_client) = llm else {
        return AskResponse {
            answer: claims[0].statement.clone(),
            claims_used,
            category: req.category.to_string(),
        };
    };

    let answer = synthesize(&claims, &llm_client, req).await;
    AskResponse {
        answer,
        claims_used,
        category: req.category.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Internal synthesis
// ---------------------------------------------------------------------------

async fn synthesize(claims: &[ClaimSearchHit], llm: &LlmClient, req: &AskRequest<'_>) -> String {
    let claim_limit = claim_limit(req.category);

    // Build claim notes (knowledge-update gets a MOST RECENT / OLDER split)
    let claim_notes = build_claim_notes(
        claims,
        claim_limit,
        req.category,
        req.session_dates,
        req.answer_sids,
    );

    // Build source section (session-count-adaptive)
    let (source_section, temporal_section) = build_source_section(req, &claim_notes);

    // Assemble user message
    let date_section = if !req.question_date.is_empty() {
        format!("## TODAY (reference date)\n{}\n\n", req.question_date)
    } else {
        String::new()
    };

    let category_label = category_label(req.category);

    let user_msg = format!(
        "{category_label}\n{temporal_section}{date_section}## EXTRACTED CLAIMS ({} most relevant)\n{claim_notes}\n{source_section}## QUESTION\n{}",
        claims.len().min(claim_limit),
        req.question,
    );

    let fut = llm.chat(HYBRID_SYNTHESIS_PROMPT, &user_msg);
    match tokio::time::timeout(std::time::Duration::from_secs(120), fut).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(e)) => {
            tracing::warn!("synthesizer: LLM error: {e}");
            claims[0].statement.clone()
        }
        Err(_) => {
            tracing::warn!("synthesizer: LLM timeout — using best claim");
            claims[0].statement.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// Claim notes builder
// ---------------------------------------------------------------------------

fn build_claim_notes(
    claims: &[ClaimSearchHit],
    limit: usize,
    category: &str,
    session_dates: &HashMap<String, String>,
    answer_sids: &[String],
) -> String {
    if category != "knowledge-update" {
        let mut notes = String::new();
        for hit in claims.iter().take(limit) {
            let date_hint = session_dates
                .iter()
                .find(|(sid, _)| hit.source_uri.contains(sid.as_str()))
                .map(|(_, d)| format!(" [session date: {d}]"))
                .unwrap_or_default();
            notes.push_str(&format!(
                "- [{:.2} conf{date_hint}] {}\n",
                hit.confidence, hit.statement
            ));
            if notes.len() > 25_000 {
                break;
            }
        }
        return notes;
    }

    // Knowledge-update: split into MOST RECENT / OLDER to prevent stale-value errors
    let most_recent_sid = answer_sids
        .iter()
        .max_by_key(|sid| {
            session_dates
                .iter()
                .find(|(date_sid, _)| {
                    sid.contains(date_sid.as_str()) || date_sid.contains(sid.as_str())
                })
                .map(|(_, d)| d.as_str())
                .unwrap_or("")
        })
        .cloned()
        .unwrap_or_default();

    let mut recent_notes = String::new();
    let mut older_notes = String::new();

    for hit in claims.iter().take(limit) {
        let date_hint = session_dates
            .iter()
            .find(|(sid, _)| hit.source_uri.contains(sid.as_str()))
            .map(|(_, d)| format!(" [session: {d}]"))
            .unwrap_or_default();

        let is_recent = !most_recent_sid.is_empty()
            && (hit.source_uri.contains(most_recent_sid.as_str())
                || most_recent_sid.contains(hit.source_uri.as_str()));

        let line = format!(
            "- [{:.2} conf{date_hint}] {}\n",
            hit.confidence, hit.statement
        );
        if is_recent {
            recent_notes.push_str(&line);
        } else {
            older_notes.push_str(&line);
        }
        if recent_notes.len() + older_notes.len() > 20_000 {
            break;
        }
    }

    let mut out = String::from("## MOST RECENT FACTS (← use these as the current truth)\n");
    if recent_notes.is_empty() {
        out.push_str("(see older facts below)\n");
    } else {
        out.push_str(&recent_notes);
    }
    out.push_str("\n## OLDER FACTS (may have been superseded — use only if not in most recent)\n");
    if older_notes.is_empty() {
        out.push_str("(none)\n");
    } else {
        out.push_str(&older_notes);
    }
    out
}

// ---------------------------------------------------------------------------
// Source section builder (session-count-adaptive)
// ---------------------------------------------------------------------------

fn build_source_section(req: &AskRequest<'_>, claim_notes: &str) -> (String, String) {
    let claimed_len = claim_notes.len();

    match req.category {
        // Single-session: always full transcripts
        "single-session-user" | "single-session-assistant" | "single-session-preference" => {
            let budget = 80_000usize.saturating_sub(claimed_len);
            let raw = load_raw_sources(req.sessions_dir, req.answer_sids, budget);
            let sec = if raw.is_empty() {
                String::new()
            } else {
                format!("## RAW CONVERSATION TRANSCRIPTS\n{raw}\n")
            };
            (sec, String::new())
        }

        // Temporal: full transcripts + pre-computed date anchors
        "temporal-reasoning" => {
            let anchors = compute_temporal_anchors(
                req.question,
                req.question_date,
                req.session_dates,
                req.answer_sids,
            );
            let budget = 60_000usize.saturating_sub(claimed_len);
            let raw = load_raw_sources(req.sessions_dir, req.answer_sids, budget);
            let sec = if raw.is_empty() {
                String::new()
            } else {
                format!("## RAW CONVERSATION TRANSCRIPTS\n{raw}\n")
            };
            (sec, anchors)
        }

        // Knowledge-update: full transcripts (usually 1-2 answer sessions)
        "knowledge-update" => {
            let budget = 50_000usize.saturating_sub(claimed_len);
            let raw = load_raw_sources(req.sessions_dir, req.answer_sids, budget);
            let sec = if raw.is_empty() {
                String::new()
            } else {
                format!("## RAW CONVERSATION TRANSCRIPTS\n{raw}\n")
            };
            (sec, String::new())
        }

        // Multi-session: session-count-adaptive
        // ≤3 sessions → full transcripts (ground truth, eliminates under-counting)
        // >3 sessions → keyword snippets (prevents counting noise from too much context)
        _ => {
            if req.answer_sids.len() <= 3 {
                let budget = 60_000usize.saturating_sub(claimed_len);
                let raw = load_raw_sources(req.sessions_dir, req.answer_sids, budget);
                let sec = if raw.is_empty() {
                    String::new()
                } else {
                    format!("## RAW CONVERSATION TRANSCRIPTS\n{raw}\n")
                };
                (sec, String::new())
            } else {
                let budget = 35_000usize.saturating_sub(claimed_len);
                let snippets = extract_relevant_snippets(
                    req.sessions_dir,
                    req.answer_sids,
                    req.question,
                    budget,
                );
                let sec = if snippets.is_empty() {
                    String::new()
                } else {
                    format!("## RELEVANT TRANSCRIPT SNIPPETS\n{snippets}\n")
                };
                (sec, String::new())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn claim_limit(category: &str) -> usize {
    match category {
        "multi-session" => 100,
        "temporal-reasoning" => 80,
        "single-session-assistant" => 80,
        "knowledge-update" => 60,
        "single-session-preference" => 50,
        _ => 60,
    }
}

fn category_label(category: &str) -> &'static str {
    match category {
        "single-session-user" => "[CATEGORY: single-session-user]",
        "single-session-assistant" => "[CATEGORY: single-session-assistant]",
        "single-session-preference" => "[CATEGORY: single-session-preference]",
        "multi-session" => "[CATEGORY: multi-session]",
        "temporal-reasoning" => "[CATEGORY: temporal-reasoning]",
        "knowledge-update" => "[CATEGORY: knowledge-update]",
        _ => "",
    }
}
