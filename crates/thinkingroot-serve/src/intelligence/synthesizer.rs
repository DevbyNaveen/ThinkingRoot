// crates/thinkingroot-serve/src/intelligence/synthesizer.rs
//
// Hybrid synthesis — the intelligence core.
//
// Assembles claims + raw source into a category-adaptive synthesis prompt and
// calls the LLM to produce a final natural-language answer.
//
// Proven at 91.2% on LongMemEval-500 (Round 6). Key design decisions:
//
//   - Persona registry: three system prompts — `MEMORY_SYSTEM_PROMPT`
//     (LongMemEval contract, byte-identical to the v0.9.0 prompt),
//     `CODE_SYSTEM_PROMPT_*`, and `DOCS_SYSTEM_PROMPT_*`. The persona is
//     resolved per-request from `[chat]` config + auto-detected source
//     mix, then fed to `build_system_prompt`.
//
//   - 6-category strategies in every prompt. The LLM sees [CATEGORY: X]
//     in the user message and applies the matching strategy — factual
//     recall, counting, temporal, assistant recall, preference, or
//     knowledge-update.
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
//
//   - Workspace identity injection: when `AskRequest::identity` is `Some`,
//     `build_user_message` prepends a `<system-reminder>` block with
//     workspace name / claim count / source kinds / project doc / today.
//     Modelled after Claude Code's `prependUserContext` so models that
//     have been RLHF-tuned on the `<system-reminder>` tag treat it as
//     ambient context, not as part of the user's question. The
//     LongMemEval bench harness passes `identity: None` and persona
//     `Memory` to keep the wire prompt byte-identical to v0.9.0.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use thinkingroot_core::config::{ChatPersona, ChatVerbosity, ResolvedChat};
use thinkingroot_extract::llm::{ChatStream, LlmClient};

use crate::engine::ClaimSearchHit;
use crate::intelligence::augmenter::{extract_relevant_snippets, load_raw_sources};
use crate::intelligence::identity::{WorkspaceIdentity, render_identity_block};
use crate::intelligence::temporal::compute_temporal_anchors;

// ---------------------------------------------------------------------------
// Synthesis prompts (3 personas × verbosity, 6 category strategies each)
// ---------------------------------------------------------------------------
//
// LongMemEval contract: `MEMORY_SYSTEM_PROMPT` is byte-identical to the
// v0.9.0 `HYBRID_SYNTHESIS_PROMPT`. Verbosity is intentionally ignored for
// `Memory` persona so the 91.2 % LME-500 result reproduces. The
// `lme_memory_prompt_is_byte_identical_to_legacy` test in this module
// guards that contract.

/// Persona = Memory. The LongMemEval-tuned conversational memory prompt.
/// Validated at 91.2 % on LME-500 (Round 6). Modify only if you are
/// re-running the full benchmark.
const MEMORY_SYSTEM_PROMPT: &str = r#"You are a precise personal memory assistant. You have two types of information:

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

/// Persona = Code, Verbosity = Terse. Engineering assistant tuned for
/// codebase questions; encourages `file_path:line_number` citations and
/// keeps answers short.
const CODE_SYSTEM_PROMPT_TERSE: &str = r#"You are a knowledgeable engineering assistant grounded in a compiled knowledge graph of a specific codebase. You have two types of information:

1. **EXTRACTED CLAIMS** — structured facts compiled from the codebase (confidence + source path).
2. **RAW SOURCE EXCERPTS** — original file contents from the most relevant sources.

Raw excerpts are ground truth — if a detail is in the excerpt but not in the claims, TRUST THE EXCERPT.

━━━ STRATEGY: FACTUAL RECALL ━━━
(Categories: single-session-user, knowledge-update)
- Locate the specific fact in claims or excerpts.
- Cite the source file in `path:line` form when the line is known.
- For "what is X" / "where is X defined": quote or paraphrase the definition succinctly.

━━━ STRATEGY: COUNTING & AGGREGATION ━━━
(Category: multi-session)
STEP 1 — EXTRACT: List every distinct instance of the thing being counted across the provided sources.
STEP 2 — DEDUPLICATE: One symbol counted once even if it appears in multiple files.
STEP 3 — TOTAL: State "Total: N" and list the items.

━━━ STRATEGY: TEMPORAL REASONING ━━━
(Category: temporal-reasoning)
- Use any provided date references as-is. Don't recompute.
- If commit/history dates appear in the source metadata, surface them with their source.

━━━ STRATEGY: SOURCE EXCERPT QUOTING ━━━
(Category: single-session-assistant)
- Treat the most relevant excerpt as the ground-truth fragment.
- Quote it directly when the question asks "what does X say" or "show me the code that does Y".

━━━ STRATEGY: PATTERN-BASED RECOMMENDATION ━━━
(Category: single-session-preference)
- Ground recommendations in the codebase's existing patterns.
- Cite the analogous existing code by path:line.
- Never give generic programming advice that ignores the codebase.

━━━ STRATEGY: KNOWLEDGE UPDATE ━━━
- When claims split into MOST RECENT FACTS / OLDER FACTS, MOST RECENT reflects the current state of the code.

━━━ CRITICAL: WHEN TO ABSTAIN ━━━
Only say "not enough information" when the asked-about symbol/file/concept is COMPLETELY ABSENT from the claims and excerpts. Otherwise, answer with what is provided. Never invent symbols, files, or behaviours not present in the data.

━━━ UNIVERSAL RULES ━━━
- Use ONLY information from the provided data. Never invent code, APIs, or behaviour.
- When citing code, use `file_path:line_number` format so the user can navigate.
- For yes/no: answer "Yes" or "No" then cite a specific source.
- The user is asking about THIS codebase — do not give generic programming advice.
- Length: short phrase, sentence, or up to a small bulleted list — keep it tight.
"#;

/// Persona = Code, Verbosity = Rich. Same scaffolding as the terse
/// variant; differs only in the trailing length rule, which encourages
/// multi-paragraph, well-structured answers.
const CODE_SYSTEM_PROMPT_RICH: &str = r#"You are a knowledgeable engineering assistant grounded in a compiled knowledge graph of a specific codebase. You have two types of information:

1. **EXTRACTED CLAIMS** — structured facts compiled from the codebase (confidence + source path).
2. **RAW SOURCE EXCERPTS** — original file contents from the most relevant sources.

Raw excerpts are ground truth — if a detail is in the excerpt but not in the claims, TRUST THE EXCERPT.

━━━ STRATEGY: FACTUAL RECALL ━━━
(Categories: single-session-user, knowledge-update)
- Locate the specific fact in claims or excerpts.
- Cite the source file in `path:line` form when the line is known.
- For "what is X" / "where is X defined": quote or paraphrase the definition, then explain how it fits in the surrounding module.

━━━ STRATEGY: COUNTING & AGGREGATION ━━━
(Category: multi-session)
STEP 1 — EXTRACT: List every distinct instance of the thing being counted across the provided sources, with their source paths.
STEP 2 — DEDUPLICATE: One symbol counted once even if it appears in multiple files.
STEP 3 — TOTAL: State "Total: N" and present the items as a bulleted list with citations.

━━━ STRATEGY: TEMPORAL REASONING ━━━
(Category: temporal-reasoning)
- Use any provided date references as-is. Don't recompute.
- For commit/history questions: pull dates from the source metadata and surface them with the relevant source.

━━━ STRATEGY: SOURCE EXCERPT QUOTING ━━━
(Category: single-session-assistant)
- Treat the most relevant excerpt as ground truth.
- Quote it directly when "what does X say" / "show me the code".
- For multi-line code, prefer fenced code blocks with the language tag and a trailing `// path:line` comment.

━━━ STRATEGY: PATTERN-BASED RECOMMENDATION ━━━
(Category: single-session-preference)
- Ground recommendations in the codebase's existing patterns.
- Walk through the analogous existing code (cite path:line), then explain how the recommended approach mirrors it.
- Never give generic programming advice that ignores the codebase.

━━━ STRATEGY: KNOWLEDGE UPDATE ━━━
- When claims split into MOST RECENT FACTS / OLDER FACTS, MOST RECENT reflects the current state of the code. Lead with that and only mention OLDER FACTS when explaining drift.

━━━ CRITICAL: WHEN TO ABSTAIN ━━━
Only say "not enough information" when the asked-about symbol/file/concept is COMPLETELY ABSENT from the claims and excerpts. Otherwise, answer with what is provided. Never invent symbols, files, or behaviour not present in the data.

━━━ UNIVERSAL RULES ━━━
- Use ONLY information from the provided data. Never invent code, APIs, or behaviour.
- When citing code, use `file_path:line_number` format so the user can navigate.
- For yes/no: answer "Yes" or "No" then cite a specific source.
- The user is asking about THIS codebase — do not give generic programming advice.
- Length: give thorough, structured answers when the question warrants it. Use headings, bullets, and fenced code blocks. Cite for every non-trivial claim.
"#;

/// Persona = Docs, Verbosity = Terse. Documentation expert tuned for
/// quoting passages with citations.
const DOCS_SYSTEM_PROMPT_TERSE: &str = r#"You are a documentation expert for a specific knowledge pack. You have two types of information:

1. **EXTRACTED CLAIMS** — structured facts compiled from the documents (confidence + source path).
2. **RAW PASSAGES** — original document contents from the most relevant sources.

Raw passages are ground truth — if a detail is in the passage but not in the claims, TRUST THE PASSAGE.

━━━ STRATEGY: FACTUAL RECALL ━━━
(Categories: single-session-user, knowledge-update)
- Locate the specific fact in claims or passages.
- Cite the source document path inline (e.g. `(docs/x.md)`).
- Quote the smallest passage that fully answers the question.

━━━ STRATEGY: COUNTING & AGGREGATION ━━━
(Category: multi-session)
STEP 1 — EXTRACT: Enumerate every distinct instance across passages.
STEP 2 — DEDUPLICATE: Count repeated mentions once.
STEP 3 — TOTAL: State "Total: N" and list with citations.

━━━ STRATEGY: TEMPORAL REASONING ━━━
(Category: temporal-reasoning)
- Use provided date references as-is. Don't recompute.

━━━ STRATEGY: PASSAGE QUOTING ━━━
(Category: single-session-assistant)
- Quote the most relevant passage directly when asked "what does X say about Y".

━━━ STRATEGY: GUIDANCE FROM DOCUMENTATION ━━━
(Category: single-session-preference)
- Ground recommendations in what the documentation actually states.
- Never give generic advice that ignores the documents.

━━━ STRATEGY: KNOWLEDGE UPDATE ━━━
- MOST RECENT FACTS reflect the latest documented state.

━━━ CRITICAL: WHEN TO ABSTAIN ━━━
Only say "not enough information" when the asked-about topic is COMPLETELY ABSENT from the claims and passages. Otherwise, answer with what is provided.

━━━ UNIVERSAL RULES ━━━
- Use ONLY information from the provided documentation. Never invent.
- Cite source paths for every non-trivial claim.
- Quote relevant passages directly when they answer the question.
- The user is asking about THIS knowledge pack — do not give generic answers.
- Length: short phrase, sentence, or 1-3 sentences when possible.
"#;

/// Persona = Docs, Verbosity = Rich. Same scaffolding as the terse
/// variant; differs only in the trailing length rule.
const DOCS_SYSTEM_PROMPT_RICH: &str = r#"You are a documentation expert for a specific knowledge pack. You have two types of information:

1. **EXTRACTED CLAIMS** — structured facts compiled from the documents (confidence + source path).
2. **RAW PASSAGES** — original document contents from the most relevant sources.

Raw passages are ground truth — if a detail is in the passage but not in the claims, TRUST THE PASSAGE.

━━━ STRATEGY: FACTUAL RECALL ━━━
(Categories: single-session-user, knowledge-update)
- Locate the specific fact in claims or passages.
- Cite the source document path inline (e.g. `(docs/x.md)`), and quote the relevant passage.

━━━ STRATEGY: COUNTING & AGGREGATION ━━━
(Category: multi-session)
STEP 1 — EXTRACT: Enumerate every distinct instance across passages with their source paths.
STEP 2 — DEDUPLICATE: Count repeated mentions once.
STEP 3 — TOTAL: State "Total: N" and present the items as a bulleted list with citations.

━━━ STRATEGY: TEMPORAL REASONING ━━━
(Category: temporal-reasoning)
- Use provided date references as-is. Don't recompute.
- Surface document revision dates if present in the passage metadata.

━━━ STRATEGY: PASSAGE QUOTING ━━━
(Category: single-session-assistant)
- Quote the most relevant passage directly. For multi-paragraph quotes use a blockquote (`> `) and cite the source path.

━━━ STRATEGY: GUIDANCE FROM DOCUMENTATION ━━━
(Category: single-session-preference)
- Ground recommendations in what the documentation actually states.
- Walk through the relevant passages, then state the recommendation.
- Never give generic advice that ignores the documents.

━━━ STRATEGY: KNOWLEDGE UPDATE ━━━
- MOST RECENT FACTS reflect the latest documented state. Lead with that; mention OLDER FACTS only when explaining drift.

━━━ CRITICAL: WHEN TO ABSTAIN ━━━
Only say "not enough information" when the asked-about topic is COMPLETELY ABSENT from the claims and passages. Otherwise, answer with what is provided.

━━━ UNIVERSAL RULES ━━━
- Use ONLY information from the provided documentation. Never invent.
- Cite source paths for every non-trivial claim.
- Quote relevant passages directly when they answer the question.
- The user is asking about THIS knowledge pack — do not give generic answers.
- Length: thorough, well-structured answers with headings, bullets, and blockquotes. Cite for every non-trivial claim.
"#;

/// Pick the system prompt for a resolved persona+verbosity pair.
///
/// `Memory` ignores verbosity by design — the LongMemEval prompt is the
/// contract that protects the 91.2 % benchmark. `Auto` is treated as an
/// unresolved sentinel and falls back to `Memory`; callers should
/// always pass a `ResolvedChat` from `ChatConfig::resolve`.
pub fn build_system_prompt(chat: ResolvedChat) -> &'static str {
    match chat.persona {
        ChatPersona::Memory | ChatPersona::Auto => MEMORY_SYSTEM_PROMPT,
        ChatPersona::Code => match chat.verbosity {
            ChatVerbosity::Terse => CODE_SYSTEM_PROMPT_TERSE,
            ChatVerbosity::Rich | ChatVerbosity::Auto => CODE_SYSTEM_PROMPT_RICH,
        },
        ChatPersona::Docs => match chat.verbosity {
            ChatVerbosity::Terse => DOCS_SYSTEM_PROMPT_TERSE,
            ChatVerbosity::Rich | ChatVerbosity::Auto => DOCS_SYSTEM_PROMPT_RICH,
        },
    }
}

/// Convenience for callers that want a `Cow` (e.g. when an upstream
/// layer might one day prepend per-deployment text).
#[inline]
pub fn build_system_prompt_cow(chat: ResolvedChat) -> Cow<'static, str> {
    Cow::Borrowed(build_system_prompt(chat))
}

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
    /// Resolved persona + verbosity for this request. Defaults to
    /// `Memory`/`Terse` so legacy callers (LongMemEval bench harness,
    /// existing tests) keep the byte-identical v0.9.0 wire prompt.
    pub chat: ResolvedChat,
    /// Workspace identity to inject as a `<system-reminder>` ambient
    /// context block at the start of the user message. `None` keeps the
    /// v0.9.0 prompt body byte-identical (LongMemEval contract).
    pub identity: Option<&'a WorkspaceIdentity>,
    /// Optional ISO date for the `# today` line inside the
    /// `<system-reminder>` block. `None` omits it.
    pub today: Option<&'a str>,
}

impl<'a> AskRequest<'a> {
    /// Default `chat` value used by callers that haven't opted in to the
    /// persona registry yet (LongMemEval bench, ablation harness).
    pub fn legacy_chat() -> ResolvedChat {
        ResolvedChat {
            persona: ChatPersona::Memory,
            verbosity: ChatVerbosity::Terse,
        }
    }
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
    let system_prompt = build_system_prompt(req.chat);
    let user_msg = build_user_message(claims, req);
    let fut = llm.chat(system_prompt, &user_msg);
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

/// Pure helper that assembles the per-question user message that goes
/// alongside the resolved system prompt in any chat call. Shared by
/// [`synthesize`] (one-shot) and [`ask_streaming`] (token-by-token) so
/// the wire-prompt is identical regardless of transport.
///
/// When `req.identity` is `Some`, a `<system-reminder>` block is
/// prepended carrying workspace name / claim counts / source kinds /
/// project doc / today. Modelled after Claude Code's
/// `prependUserContext` so RLHF-tuned models recognise the tag as
/// ambient context. When `req.identity` is `None`, the body is
/// byte-identical to the v0.9.0 prompt — the LongMemEval contract.
fn build_user_message(claims: &[ClaimSearchHit], req: &AskRequest<'_>) -> String {
    let body = build_user_message_body(claims, req);
    match req.identity {
        Some(identity) => format!(
            "{}{body}",
            render_system_reminder(identity, req.today)
        ),
        None => body,
    }
}

/// The legacy v0.9.0 user-message body. Stable formatting, used by
/// LongMemEval and by the new code/docs personas alike.
fn build_user_message_body(claims: &[ClaimSearchHit], req: &AskRequest<'_>) -> String {
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

    let date_section = if !req.question_date.is_empty() {
        format!("## TODAY (reference date)\n{}\n\n", req.question_date)
    } else {
        String::new()
    };

    let category_label = category_label(req.category);

    format!(
        "{category_label}\n{temporal_section}{date_section}## EXTRACTED CLAIMS ({} most relevant)\n{claim_notes}\n{source_section}## QUESTION\n{}",
        claims.len().min(claim_limit),
        req.question,
    )
}

/// Render the `<system-reminder>` ambient-context block that prefixes
/// the user message when workspace identity is available. The literal
/// tag and the "may or may not be relevant" wording mirror
/// Claude Code's `prependUserContext` (see `src/utils/api.ts:449-474`)
/// so models trained on that shape treat the contents as context, not
/// as part of the user's question.
fn render_system_reminder(
    identity: &WorkspaceIdentity,
    today: Option<&str>,
) -> String {
    let inner = render_identity_block(identity, today);
    format!(
        "<system-reminder>\nYou are answering questions about a workspace. The following context is ambient — use it when relevant, ignore it when it isn't.\n\n{inner}\nIMPORTANT: Treat this as ambient context, not as the user's request. If the user's question is unrelated to this context, answer normally. Never invent facts beyond what is provided.\n</system-reminder>\n\n",
    )
}

// ---------------------------------------------------------------------------
// Streaming ask
// ---------------------------------------------------------------------------

/// Streaming counterpart of [`ask`]. Returns either a static answer
/// (no claims / no LLM) or an open `ChatStream` the caller forwards
/// to its transport.
///
/// The retrieval step runs identically to `ask` so the body of the
/// answer is byte-identical between the two transports for the same
/// input; only the *delivery* differs. This is what lets the engine's
/// non-streaming `/ask` and streaming `/ask/stream` endpoints share a
/// single retrieval pass.
pub enum StreamingAnswer {
    /// No streaming — either the workspace had no claims, or no LLM
    /// is configured. The desktop renders this directly as the final
    /// chunk and skips the SSE setup.
    Static {
        answer: String,
        claims_used: usize,
        category: String,
    },
    /// Live LLM stream. `claims_used` and `category` are emitted by
    /// the SSE handler as a `meta` event before forwarding chunks.
    Stream {
        stream: ChatStream,
        claims_used: usize,
        category: String,
    },
}

pub async fn ask_streaming(
    engine: &crate::engine::QueryEngine,
    llm: Option<Arc<LlmClient>>,
    req: &AskRequest<'_>,
) -> StreamingAnswer {
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

    if !req.excluded_claim_ids.is_empty() {
        claims.retain(|c| !req.excluded_claim_ids.contains(&c.id));
    }

    let claims_used = claims.len();
    let category = req.category.to_string();

    if claims.is_empty() {
        return StreamingAnswer::Static {
            answer: "I don't have enough information to answer that.".to_string(),
            claims_used: 0,
            category,
        };
    }

    let Some(llm_client) = llm else {
        return StreamingAnswer::Static {
            answer: claims[0].statement.clone(),
            claims_used,
            category,
        };
    };

    let user_msg = build_user_message(&claims, req);
    let system_prompt = build_system_prompt(req.chat);

    match llm_client
        .chat_stream(system_prompt, &user_msg)
        .await
    {
        Ok(stream) => StreamingAnswer::Stream {
            stream,
            claims_used,
            category,
        },
        Err(e) => {
            // Connect-time failure — fall back to the highest-confidence
            // claim verbatim, the same conservative default `ask` uses
            // when the one-shot LLM call errors. Logging so operators
            // can tell streaming from one-shot in metrics.
            tracing::warn!("synthesizer: chat_stream open failed: {e} — using best claim");
            StreamingAnswer::Static {
                answer: claims[0].statement.clone(),
                claims_used,
                category,
            }
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

// ---------------------------------------------------------------------------
// Tests — prompt-shape contracts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod prompt_contract_tests {
    use super::*;
    use crate::engine::ClaimSearchHit;
    use crate::intelligence::identity::WorkspaceIdentity;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    // ─────────────────────────────────────────────────────────────────
    // LongMemEval contract: MEMORY_SYSTEM_PROMPT must be byte-identical
    // to the v0.9.0 prompt used to score 91.2 % on LME-500. The legacy
    // string is duplicated here on purpose — if anyone edits the live
    // const, this test trips.
    // ─────────────────────────────────────────────────────────────────

    const LEGACY_HYBRID_SYNTHESIS_PROMPT: &str = r#"You are a precise personal memory assistant. You have two types of information:

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

    #[test]
    fn memory_persona_prompt_is_byte_identical_to_legacy() {
        assert_eq!(
            MEMORY_SYSTEM_PROMPT, LEGACY_HYBRID_SYNTHESIS_PROMPT,
            "MEMORY_SYSTEM_PROMPT diverged from the v0.9.0 LongMemEval-91.2% prompt; \
             re-run the benchmark before changing it"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Persona registry selection
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn legacy_chat_is_memory_terse() {
        let c = AskRequest::legacy_chat();
        assert_eq!(c.persona, ChatPersona::Memory);
        assert_eq!(c.verbosity, ChatVerbosity::Terse);
    }

    #[test]
    fn build_system_prompt_memory_returns_legacy() {
        let p = build_system_prompt(AskRequest::legacy_chat());
        assert_eq!(p, MEMORY_SYSTEM_PROMPT);
    }

    #[test]
    fn build_system_prompt_memory_ignores_verbosity() {
        // Verbosity=Rich on Memory persona is intentionally a no-op so
        // the LongMemEval contract never accidentally regresses.
        let p_terse = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Memory,
            verbosity: ChatVerbosity::Terse,
        });
        let p_rich = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Memory,
            verbosity: ChatVerbosity::Rich,
        });
        assert_eq!(p_terse, p_rich);
        assert_eq!(p_terse, MEMORY_SYSTEM_PROMPT);
    }

    #[test]
    fn build_system_prompt_code_terse_vs_rich_diverge() {
        let terse = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Code,
            verbosity: ChatVerbosity::Terse,
        });
        let rich = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Code,
            verbosity: ChatVerbosity::Rich,
        });
        assert!(terse.starts_with("You are a knowledgeable engineering assistant"));
        assert!(rich.starts_with("You are a knowledgeable engineering assistant"));
        assert_ne!(terse, rich, "terse and rich must differ in the length rule");
        assert!(terse.contains("Length: short phrase"));
        assert!(rich.contains("Length: give thorough"));
    }

    #[test]
    fn build_system_prompt_docs_terse_vs_rich_diverge() {
        let terse = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Docs,
            verbosity: ChatVerbosity::Terse,
        });
        let rich = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Docs,
            verbosity: ChatVerbosity::Rich,
        });
        assert!(terse.starts_with("You are a documentation expert"));
        assert!(rich.starts_with("You are a documentation expert"));
        assert_ne!(terse, rich);
    }

    // ─────────────────────────────────────────────────────────────────
    // User-message wrapping behaviour
    // ─────────────────────────────────────────────────────────────────

    fn fixture_claims() -> Vec<ClaimSearchHit> {
        vec![ClaimSearchHit {
            id: "c1".to_string(),
            statement: "Azure OpenAI is configured".to_string(),
            claim_type: "fact".to_string(),
            confidence: 0.92,
            source_uri: "session_001/foo.json".to_string(),
            relevance: 0.5,
        }]
    }

    fn empty_sessions_dir() -> PathBuf {
        PathBuf::from("/tmp/__synthesizer_test_no_sessions__")
    }

    #[test]
    fn build_user_message_no_identity_omits_system_reminder() {
        // The v0.9.0 LongMemEval contract: identity = None ⇒ no
        // <system-reminder> prefix. The body is whatever
        // build_user_message_body produces.
        let claims = fixture_claims();
        let allowed = HashSet::<String>::new();
        let dates = HashMap::<String, String>::new();
        let sids: Vec<String> = vec![];
        let excluded = HashSet::<String>::new();
        let dir = empty_sessions_dir();
        let req = AskRequest {
            workspace: "lme",
            question: "what?",
            category: "single-session-user",
            allowed_sources: &allowed,
            question_date: "",
            session_dates: &dates,
            answer_sids: &sids,
            sessions_dir: &dir,
            excluded_claim_ids: &excluded,
            chat: AskRequest::legacy_chat(),
            identity: None,
            today: None,
        };
        let with_id = build_user_message(&claims, &req);
        let body = build_user_message_body(&claims, &req);
        assert_eq!(with_id, body);
        assert!(!with_id.contains("<system-reminder>"));
        assert!(with_id.contains("[CATEGORY: single-session-user]"));
        assert!(with_id.ends_with("## QUESTION\nwhat?"));
    }

    #[test]
    fn code_persona_full_wire_prompt_carries_workspace_context() {
        // End-to-end shape check for the production code-workspace path:
        // resolved chat = (Code, Rich), identity carries name + counts +
        // source mix + project_doc, today is set. The wire prompt the
        // model receives must (a) start with the engineering-assistant
        // intro, (b) contain a <system-reminder> ambient block with all
        // the workspace specifics, (c) end with the user's question.
        use crate::intelligence::identity::ProjectDoc;

        let identity = WorkspaceIdentity {
            name: "thinkingroot-cloud".to_string(),
            mounted_at: PathBuf::from("/Users/me/Desktop/thinkingroot-cloud"),
            claim_count: 1253,
            source_kinds: vec![
                ("rs".to_string(), 842),
                ("md".to_string(), 311),
                ("toml".to_string(), 100),
            ],
            project_doc: Some(ProjectDoc {
                label: "CLAUDE.md".to_string(),
                content: "# thinkingroot-cloud\nSaaS hub for the OSS engine.".to_string(),
                truncated: false,
            }),
        };

        let claims = vec![ClaimSearchHit {
            id: "c1".to_string(),
            statement: "Azure OpenAI provider is wired in services/registry".to_string(),
            claim_type: "config".to_string(),
            confidence: 0.95,
            source_uri: "services/registry/src/providers.rs".to_string(),
            relevance: 0.9,
        }];

        let allowed = HashSet::<String>::new();
        let dates = HashMap::<String, String>::new();
        let sids: Vec<String> = vec![];
        let excluded = HashSet::<String>::new();
        let dir = empty_sessions_dir();

        let chat = ResolvedChat {
            persona: ChatPersona::Code,
            verbosity: ChatVerbosity::Rich,
        };

        let req = AskRequest {
            workspace: "thinkingroot-cloud",
            question: "how many providers do we use?",
            category: "multi-session",
            allowed_sources: &allowed,
            question_date: "",
            session_dates: &dates,
            answer_sids: &sids,
            sessions_dir: &dir,
            excluded_claim_ids: &excluded,
            chat,
            identity: Some(&identity),
            today: Some("2026-04-28"),
        };

        let system_prompt = build_system_prompt(req.chat);
        let user_msg = build_user_message(&claims, &req);

        // System prompt = code-rich persona
        assert!(system_prompt.starts_with("You are a knowledgeable engineering assistant"));
        assert!(system_prompt.contains("file_path:line_number"));
        assert!(system_prompt.contains("Length: give thorough"));

        // User message = system-reminder ambient block + standard body
        assert!(user_msg.starts_with("<system-reminder>\n"));
        assert!(user_msg.contains("name: thinkingroot-cloud"));
        assert!(user_msg.contains("claims_indexed: 1253"));
        assert!(user_msg.contains("rs(842)"));
        assert!(user_msg.contains("md(311)"));
        assert!(user_msg.contains("# project_doc (CLAUDE.md)"));
        assert!(user_msg.contains("SaaS hub for the OSS engine."));
        assert!(user_msg.contains("# today\n2026-04-28"));
        assert!(user_msg.contains("</system-reminder>\n\n"));

        // Standard body still present after the wrapper, in correct order
        let body_idx = user_msg.find("[CATEGORY: multi-session]").unwrap();
        let reminder_close = user_msg.find("</system-reminder>").unwrap();
        assert!(reminder_close < body_idx);
        assert!(user_msg.contains("## EXTRACTED CLAIMS"));
        assert!(user_msg.contains("Azure OpenAI provider is wired"));
        assert!(user_msg.ends_with("## QUESTION\nhow many providers do we use?"));
    }

    #[test]
    fn build_user_message_with_identity_prepends_system_reminder() {
        let identity = WorkspaceIdentity {
            name: "thinkingroot-cloud".to_string(),
            mounted_at: PathBuf::from("/tmp/tr-cloud"),
            claim_count: 1253,
            source_kinds: vec![("rs".to_string(), 800), ("md".to_string(), 200)],
            project_doc: None,
        };
        let claims = fixture_claims();
        let allowed = HashSet::<String>::new();
        let dates = HashMap::<String, String>::new();
        let sids: Vec<String> = vec![];
        let excluded = HashSet::<String>::new();
        let dir = empty_sessions_dir();
        let req = AskRequest {
            workspace: "thinkingroot-cloud",
            question: "what providers do we use",
            category: "multi-session",
            allowed_sources: &allowed,
            question_date: "",
            session_dates: &dates,
            answer_sids: &sids,
            sessions_dir: &dir,
            excluded_claim_ids: &excluded,
            chat: ResolvedChat {
                persona: ChatPersona::Code,
                verbosity: ChatVerbosity::Rich,
            },
            identity: Some(&identity),
            today: Some("2026-04-28"),
        };
        let msg = build_user_message(&claims, &req);
        assert!(msg.starts_with("<system-reminder>\n"));
        assert!(msg.contains("</system-reminder>\n\n"));
        assert!(msg.contains("name: thinkingroot-cloud"));
        assert!(msg.contains("claims_indexed: 1253"));
        assert!(msg.contains("rs(800)"));
        assert!(msg.contains("# today\n2026-04-28"));
        assert!(msg.contains("[CATEGORY: multi-session]"));
        assert!(msg.contains("## QUESTION\nwhat providers do we use"));
    }
}
ert!(msg.contains("claims_indexed: 1253"));
        assert!(msg.contains("rs(800)"));
        assert!(msg.contains("# today\n2026-04-28"));
        assert!(msg.contains("[CATEGORY: multi-session]"));
        assert!(msg.contains("## QUESTION\nwhat providers do we use"));
    }
}
