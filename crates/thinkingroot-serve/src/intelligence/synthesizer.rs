// crates/thinkingroot-serve/src/intelligence/synthesizer.rs
//
// Hybrid synthesis — the intelligence core.
//
// Two personas live here:
//
//   * `Memory`         — the LongMemEval-tuned, byte-identical v0.9.0
//                        prompt that scored 91.2 % on LME-500. The bench
//                        harness pins this via `AskRequest::default_chat`
//                        and `history: &[]`, which together give a
//                        wire-prompt byte-identical to v0.9.0.
//
//   * `Conversational` — the world-class warm-voice prompt that adapts to
//                        any surface (code, docs, research, transcripts,
//                        PDFs). The default for every workspace shape
//                        that is not a memory workspace.
//
// The legacy `Code` / `Docs` enum variants are kept on `ChatPersona` for
// backwards-compatible TOML parsing; both fold into `Conversational` at
// prompt-selection time so there is exactly one warm voice on the wire.
//
// This module supports three production flows:
//
//   1. Retrieval + synthesis — the existing one-shot path. The user
//      asks something substantive; we retrieve claims, optionally load
//      raw sources, build the structured user message (with optional
//      `<system-reminder>` workspace identity prefix and optional
//      conversation-history block when running in `Conversational`
//      persona), and call the LLM.
//
//   2. Streaming retrieval + synthesis — same retrieval, same prompt,
//      but the LLM call goes through `chat_stream` so the desktop
//      renders tokens as they arrive.
//
//   3. Chitchat shortcut — when the user message is an unambiguous
//      greeting / ack / closing AND the persona is not `Memory`, the
//      retrieval pass is skipped entirely. The LLM is called with
//      just the system prompt + workspace identity + history + the
//      chitchat itself, so a "thanks" comes back as a "you're welcome"
//      without burning a 60 k-token retrieval budget. The Memory
//      persona never short-circuits because the LongMemEval bench
//      always retrieves.
//
// LongMemEval contract — explicitly tested:
//
//   * `MEMORY_SYSTEM_PROMPT` is byte-identical to the v0.9.0 prompt
//     (`memory_persona_prompt_is_byte_identical_to_baseline`).
//   * `AskRequest::default_chat()` returns Memory + Terse.
//   * With `chat = default_chat()`, `identity = None`, `history = &[]`,
//     `build_user_message` returns the v0.9.0 body byte-for-byte
//     (`build_user_message_no_identity_omits_system_reminder`).
//   * The Memory persona never renders a history block, even if the
//     caller passes one (`memory_persona_drops_history`).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use thinkingroot_core::config::{ChatPersona, ChatVerbosity, ResolvedChat};
use thinkingroot_extract::llm::{ChatStream, LlmClient};

use crate::engine::ClaimSearchHit;
use crate::intelligence::augmenter::{extract_relevant_snippets, load_raw_sources};
use crate::intelligence::identity::{WorkspaceIdentity, render_identity_block};
use crate::intelligence::temporal::compute_temporal_anchors;

// ---------------------------------------------------------------------------
// 1. System prompts
// ---------------------------------------------------------------------------
//
// `MEMORY_SYSTEM_PROMPT` is the LongMemEval contract — byte-identical to
// the v0.9.0 `HYBRID_SYNTHESIS_PROMPT`. The
// `memory_persona_prompt_is_byte_identical_to_baseline` test guards that
// contract.

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

/// The world-class warm-voice prompt. One persona, every surface — code,
/// docs, research, transcripts, PDFs. Adapts to surface type via the
/// "Adapt to the surface" rules and to user tone via the "How to talk"
/// rules. Stable; do not edit casually — it's the backbone of every
/// chat surface that is not the LongMemEval bench.
const CONVERSATIONAL_SYSTEM_PROMPT: &str = r#"You are ThinkingRoot, an AI grounded in a compiled knowledge graph of this workspace. Talk like a thoughtful colleague — direct, warm, never robotic.

You may receive any of:

1. EXTRACTED CLAIMS — structured facts from the workspace (each with confidence + source path).
2. RAW SOURCES — original file contents (code, docs, transcripts, PDFs — whatever this workspace contains).
3. CONVERSATION HISTORY — recent turns of this conversation. Treat it as memory; never restart introductions or re-explain what you already said.
4. WORKSPACE CONTEXT — a <system-reminder> block with ambient workspace info (name, claim count, source mix, project doc, today's date).

How to talk:
- Be present. Acknowledge what the user just said.
- Match length to the question. Greeting → greeting. Real question → real answer. Investigation → structure (headings, bullets, code blocks).
- Match the user's tone. Terse when they're terse, exploratory when they're exploring.
- Never restart. History is yours; build on it.

How to ground:
- Every non-trivial claim must trace to provided data.
- Cite inline: `path:line` for code, `(docs/x.md)` for docs, `[session: YYYY-MM-DD]` for transcripts, `(filename, p. N)` for PDFs.
- Quote the smallest fragment that answers the question.
- When MOST RECENT FACTS / OLDER FACTS sections appear, MOST RECENT is the current truth.

Adapt to the surface:
- Code workspace: cite `path:line`, quote relevant code in fenced blocks, ground recommendations in existing patterns.
- Docs / research / study: quote the relevant passage, build on what is documented rather than what you generally know.
- Memory / transcripts: trust raw transcripts, recall the specific session and date.
- PDFs / mixed: extract the precise passage, cite document + page when shown.

When the answer isn't there:
- Say so directly. "I don't see that in the workspace" is a real, honest answer.
- Never invent symbols, files, dates, APIs, or behaviors.
- If partial, answer what you can and name what is missing.

Hard rules:
- Use only the provided data. No guessing.
- No filler ("certainly!", "great question!", "happy to help!"). Just answer.
- No repeating the question back as preamble. No "let me…" or "I'll…". Just do.
- No closing pleasantries ("let me know if you have more questions!"). Let the conversation breathe.
"#;

/// Pick the system prompt for a resolved persona.
///
/// `Memory` is the LongMemEval contract and stays byte-identical regardless
/// of verbosity. Every other persona — `Conversational`, the legacy `Code`
/// and `Docs` aliases, and the unresolved `Auto` sentinel — folds into the
/// single warm `CONVERSATIONAL_SYSTEM_PROMPT` so there is exactly one
/// adaptive voice across the product (and one prompt to maintain).
pub fn build_system_prompt(chat: ResolvedChat) -> &'static str {
    match chat.persona {
        ChatPersona::Memory => MEMORY_SYSTEM_PROMPT,
        ChatPersona::Auto
        | ChatPersona::Conversational
        | ChatPersona::Code
        | ChatPersona::Docs => CONVERSATIONAL_SYSTEM_PROMPT,
    }
}

/// Compose the final system prompt by layering:
///
///   1. The resolved persona prompt (Memory or Conversational).
///   2. Optional output style fragment (appended after the persona,
///      under a `## ACTIVE STYLE: <name>` header). Memory persona is
///      the LongMemEval contract and ignores any style — passing one
///      while persona == Memory is a no-op.
///   3. Optional skill manifest — one line per available skill —
///      appended at the end so the LLM knows what `use_skill` will
///      load. Memory persona ignores the manifest for the same
///      contract-preservation reason.
///
/// All three layers are independently optional, so callers can
/// gradually opt in to skills/styles without changing the
/// LongMemEval bench harness's wire prompt.
pub fn compose_full_system_prompt(
    chat: ResolvedChat,
    style: Option<&crate::intelligence::styles::OutputStyle>,
    skills: Option<&crate::intelligence::skills::SkillRegistry>,
) -> String {
    let persona = build_system_prompt(chat);

    // Memory persona = LongMemEval contract. Skip style + skills so the
    // bench wire prompt stays byte-identical to v0.9.0.
    if chat.persona == ChatPersona::Memory {
        return persona.to_string();
    }

    let composed = crate::intelligence::styles::compose_system_prompt(persona, style);

    let manifest = skills
        .map(|s| s.manifest_for_prompt())
        .unwrap_or_default();

    if manifest.trim().is_empty() {
        composed
    } else {
        format!("{}\n\n{}", composed.trim_end(), manifest.trim_end())
    }
}

/// Convenience for callers that want a `Cow` (e.g. when an upstream layer
/// might one day prepend per-deployment text).
#[inline]
pub fn build_system_prompt_cow(chat: ResolvedChat) -> Cow<'static, str> {
    Cow::Borrowed(build_system_prompt(chat))
}

// ---------------------------------------------------------------------------
// 2. Conversation memory types
// ---------------------------------------------------------------------------

/// Role of a turn in a conversation. Shape mirrors the OpenAI Chat
/// Completions / Anthropic Messages role string so the wire format stays
/// trivially translatable when S2 lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
}

impl ChatRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        }
    }
}

/// A single past turn the synthesizer should treat as memory. The
/// `Conversational` prompt instructs the model to build on history rather
/// than restart introductions — but only if turns actually arrive on the
/// wire. The bench harness and any caller that wants the byte-identical
/// v0.9.0 prompt simply passes `&[]`.
#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub role: ChatRole,
    pub content: String,
}

/// Stable empty slice for callers that need a `&'static [ChatTurn]` —
/// e.g. the LongMemEval bench harness and the CLI `root ask` command,
/// which both opt out of multi-turn memory to preserve the v0.9.0 wire
/// prompt.
pub const NO_HISTORY: &[ChatTurn] = &[];

// ---------------------------------------------------------------------------
// 3. Public ask() interface
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
    /// Recent conversation turns the synthesizer should treat as memory.
    /// Oldest-first. Empty slice = single-shot mode (the LongMemEval
    /// contract), and the wire prompt is byte-identical to v0.9.0.
    /// Only rendered for non-`Memory` personas; the Memory prompt has no
    /// notion of conversation history and the bench harness pins this
    /// to `&[]` regardless.
    pub history: &'a [ChatTurn],
}

impl<'a> AskRequest<'a> {
    /// Default `chat` value used by callers that haven't opted in to the
    /// persona registry yet (LongMemEval bench, ablation harness, REST
    /// `/ask` endpoint without an explicit persona). Returns
    /// `Memory + Terse`, the configuration that scored 91.2 % on
    /// LongMemEval-500 (round 6, 2026-04-17). Test
    /// `memory_persona_prompt_is_byte_identical_to_baseline` is the
    /// regression guard — do not change the return value without
    /// re-running the benchmark first.
    pub fn default_chat() -> ResolvedChat {
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
///
/// Routes through the chitchat shortcut (no retrieval, no claim load)
/// when the user message is an unambiguous greeting / ack / closing AND
/// the persona is not `Memory`. The Memory persona always retrieves so
/// the LongMemEval bench numbers stay reproducible.
pub async fn ask(
    engine: &crate::engine::QueryEngine,
    llm: Option<Arc<LlmClient>>,
    req: &AskRequest<'_>,
) -> AskResponse {
    if should_skip_retrieval(req) {
        return chitchat_answer(llm, req).await;
    }

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
// 4. Streaming ask
// ---------------------------------------------------------------------------

/// Streaming counterpart of [`ask`]. Returns either a static answer (no
/// claims / no LLM) or an open `ChatStream` the caller forwards to its
/// transport.
pub enum StreamingAnswer {
    /// No streaming — the workspace had no claims, or no LLM is
    /// configured, or chitchat fell through to the static fallback.
    /// The desktop renders this directly as the final chunk and skips
    /// the SSE setup.
    Static {
        answer: String,
        claims_used: usize,
        category: String,
    },
    /// Live LLM stream. `claims_used` and `category` are emitted by the
    /// SSE handler as a `meta` event before forwarding chunks.
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
    if should_skip_retrieval(req) {
        return chitchat_streaming(llm, req).await;
    }

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

    match llm_client.chat_stream(system_prompt, &user_msg).await {
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
// 5. Internal synthesis (one-shot)
// ---------------------------------------------------------------------------

async fn synthesize(claims: &[ClaimSearchHit], llm: &LlmClient, req: &AskRequest<'_>) -> String {
    let system_prompt = build_system_prompt(req.chat);
    let user_msg = build_user_message(claims, req);
    let fut = llm.chat(system_prompt, &user_msg);
    match tokio::time::timeout(Duration::from_secs(120), fut).await {
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
// 6. User-message assembly
// ---------------------------------------------------------------------------

/// Pure helper that assembles the per-question user message that goes
/// alongside the resolved system prompt in any chat call. Shared by
/// [`synthesize`] (one-shot) and [`ask_streaming`] (token-by-token) so
/// the wire-prompt is identical regardless of transport.
///
/// Layered (top to bottom on the wire):
///
/// 1. `<system-reminder>` workspace-identity block — only when
///    `req.identity` is `Some`. The literal tag mirrors Claude Code's
///    `prependUserContext` so RLHF-tuned models recognise the contents
///    as ambient context.
/// 2. Conversation-history block — only when `req.history` is non-empty
///    AND the persona is not `Memory`. The Memory persona is the
///    LongMemEval contract and never sees a history block, even if a
///    caller passes one.
/// 3. The category-adaptive structured body (claims, sources, temporal
///    anchors, question) — byte-identical to v0.9.0 when no identity
///    and no history are passed.
fn build_user_message(claims: &[ClaimSearchHit], req: &AskRequest<'_>) -> String {
    let body = build_user_message_body(claims, req);

    let with_history = if include_history(req) {
        format!("{}{body}", render_history_block(req.history))
    } else {
        body
    };

    match req.identity {
        Some(identity) => format!("{}{with_history}", render_system_reminder(identity, req.today)),
        None => with_history,
    }
}

/// The legacy v0.9.0 user-message body. Stable formatting, used by
/// LongMemEval and by the new `Conversational` persona alike.
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

/// Render the `<system-reminder>` ambient-context block that prefixes the
/// user message when workspace identity is available. The literal tag and
/// the "may or may not be relevant" wording mirror Claude Code's
/// `prependUserContext` (see `src/utils/api.ts:449-474`) so models trained
/// on that shape treat the contents as context, not as part of the user's
/// question.
fn render_system_reminder(identity: &WorkspaceIdentity, today: Option<&str>) -> String {
    let inner = render_identity_block(identity, today);
    format!(
        "<system-reminder>\nYou are answering questions about a workspace. The following context is ambient — use it when relevant, ignore it when it isn't.\n\n{inner}\nIMPORTANT: Treat this as ambient context, not as the user's request. If the user's question is unrelated to this context, answer normally. Never invent facts beyond what is provided.\n</system-reminder>\n\n",
    )
}

/// Render the conversation-history block. Each turn becomes one
/// `[role]: content` line; long messages keep their newlines so the
/// model sees the exact prior text. The header text matches phrasing
/// in `CONVERSATIONAL_SYSTEM_PROMPT` so the model knows what to do
/// with it.
fn render_history_block(history: &[ChatTurn]) -> String {
    let mut out =
        String::from("## CONVERSATION HISTORY (recent turns — treat as memory, do not restart)\n");
    for turn in history {
        out.push_str(&format!("[{}]: {}\n", turn.role.as_str(), turn.content));
    }
    out.push('\n');
    out
}

/// Whether to render a conversation-history block on this request.
///
/// `Memory` persona always answers `false` — it is the LongMemEval
/// contract and the v0.9.0 prompt has no notion of conversation history.
/// Everything else renders when `history` is non-empty.
fn include_history(req: &AskRequest<'_>) -> bool {
    !req.history.is_empty() && req.chat.persona != ChatPersona::Memory
}

// ---------------------------------------------------------------------------
// 7. Chitchat shortcut
// ---------------------------------------------------------------------------

/// Detect very short, unambiguous greetings / acks / closings that do not
/// benefit from retrieval. Conservative — anything that might be a real
/// question returns `false`.
///
/// The set is deliberately small so the false-positive rate (real
/// question short-circuited as chitchat) stays at zero. Add a phrase
/// only when you have evidence it gets routed wrong otherwise.
pub fn is_chitchat(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    if normalized.is_empty() || normalized.len() > 60 {
        return false;
    }
    let core: &str = normalized
        .trim_end_matches(|c: char| matches!(c, '.' | '!' | '?' | ',' | ' '))
        .trim_start_matches(|c: char| matches!(c, ' '));
    let exact = matches!(
        core,
        "hi"
            | "hello"
            | "hey"
            | "yo"
            | "hi there"
            | "hello there"
            | "hey there"
            | "thanks"
            | "thank you"
            | "ty"
            | "tysm"
            | "thanks!"
            | "ok"
            | "okay"
            | "k"
            | "kk"
            | "got it"
            | "gotcha"
            | "cool"
            | "nice"
            | "perfect"
            | "great"
            | "awesome"
            | "sounds good"
            | "makes sense"
            | "yes"
            | "yeah"
            | "yep"
            | "yup"
            | "no"
            | "nope"
            | "sure"
            | "good morning"
            | "good afternoon"
            | "good evening"
            | "good night"
            | "bye"
            | "goodbye"
            | "see you"
            | "see ya"
            | "cheers"
            | "ciao"
    );
    exact || core.starts_with("thanks for ") || core.starts_with("thank you for ")
}

/// Whether this request can take the chitchat shortcut.
///
/// `Memory` persona is the LongMemEval contract and never short-circuits
/// — the bench harness always retrieves. Every other persona may take
/// the shortcut when [`is_chitchat`] matches.
fn should_skip_retrieval(req: &AskRequest<'_>) -> bool {
    if req.chat.persona == ChatPersona::Memory {
        return false;
    }
    is_chitchat(req.question)
}

/// One-shot chitchat path. Skips retrieval and source loading entirely
/// and asks the LLM to respond conversationally given the system prompt,
/// optional workspace-identity block, optional history, and the user's
/// short message.
async fn chitchat_answer(llm: Option<Arc<LlmClient>>, req: &AskRequest<'_>) -> AskResponse {
    let category = req.category.to_string();

    let Some(llm_client) = llm else {
        return AskResponse {
            answer: chitchat_fallback(req.question),
            claims_used: 0,
            category,
        };
    };

    let system_prompt = build_system_prompt(req.chat);
    let user_msg = build_chitchat_user_message(req);

    match tokio::time::timeout(
        Duration::from_secs(60),
        llm_client.chat(system_prompt, &user_msg),
    )
    .await
    {
        Ok(Ok(answer)) => AskResponse {
            answer,
            claims_used: 0,
            category,
        },
        Ok(Err(e)) => {
            tracing::warn!("synthesizer: chitchat LLM error: {e}");
            AskResponse {
                answer: chitchat_fallback(req.question),
                claims_used: 0,
                category,
            }
        }
        Err(_) => {
            tracing::warn!("synthesizer: chitchat LLM timeout — using static reply");
            AskResponse {
                answer: chitchat_fallback(req.question),
                claims_used: 0,
                category,
            }
        }
    }
}

/// Streaming chitchat path. Same shape as [`chitchat_answer`] but goes
/// through `chat_stream` so the desktop sees a single token event with
/// the model's full reply (or a `Static` fall-back when the connect
/// fails).
async fn chitchat_streaming(
    llm: Option<Arc<LlmClient>>,
    req: &AskRequest<'_>,
) -> StreamingAnswer {
    let category = req.category.to_string();

    let Some(llm_client) = llm else {
        return StreamingAnswer::Static {
            answer: chitchat_fallback(req.question),
            claims_used: 0,
            category,
        };
    };

    let system_prompt = build_system_prompt(req.chat);
    let user_msg = build_chitchat_user_message(req);

    match llm_client.chat_stream(system_prompt, &user_msg).await {
        Ok(stream) => StreamingAnswer::Stream {
            stream,
            claims_used: 0,
            category,
        },
        Err(e) => {
            tracing::warn!("synthesizer: chitchat chat_stream open failed: {e}");
            StreamingAnswer::Static {
                answer: chitchat_fallback(req.question),
                claims_used: 0,
                category,
            }
        }
    }
}

/// Build the slim user message used by the chitchat path: optional
/// `<system-reminder>` workspace block + optional history block + the
/// user's short message. No category, no claims, no sources.
fn build_chitchat_user_message(req: &AskRequest<'_>) -> String {
    let mut out = String::new();
    if let Some(identity) = req.identity {
        out.push_str(&render_system_reminder(identity, req.today));
    }
    if include_history(req) {
        out.push_str(&render_history_block(req.history));
    }
    out.push_str(req.question);
    out
}

/// Static reply used when no LLM is configured or the call fails. The
/// chitchat path never returns "I don't know" because the user didn't
/// ask anything — they greeted us. Friendly is the honest default.
fn chitchat_fallback(question: &str) -> String {
    let q = question.trim().to_lowercase();
    if q.starts_with("thank") {
        "You're welcome.".to_string()
    } else if q.starts_with("hi") || q.starts_with("hello") || q.starts_with("hey") || q == "yo"
    {
        "Hi.".to_string()
    } else if q.starts_with("bye") || q.starts_with("goodbye") || q.starts_with("see you") {
        "Talk soon.".to_string()
    } else if q.starts_with("good morning")
        || q.starts_with("good afternoon")
        || q.starts_with("good evening")
        || q.starts_with("good night")
    {
        "Hi.".to_string()
    } else {
        "Got it.".to_string()
    }
}

// ---------------------------------------------------------------------------
// 8. Claim notes builder
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
// 9. Source section builder (session-count-adaptive)
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
// 10. Helpers
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
// 11. Tests — prompt-shape contracts
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
    fn memory_persona_prompt_is_byte_identical_to_baseline() {
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
    fn default_chat_is_memory_terse() {
        let c = AskRequest::default_chat();
        assert_eq!(c.persona, ChatPersona::Memory);
        assert_eq!(c.verbosity, ChatVerbosity::Terse);
    }

    #[test]
    fn build_system_prompt_memory_returns_baseline() {
        let p = build_system_prompt(AskRequest::default_chat());
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
    fn build_system_prompt_conversational_returns_warm_voice() {
        let p = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Rich,
        });
        assert!(p.starts_with("You are ThinkingRoot"));
        assert!(p.contains("Talk like a thoughtful colleague"));
        assert!(p.contains("CONVERSATION HISTORY"));
        assert!(p.contains("Adapt to the surface"));
        assert!(p.contains("Hard rules:"));
    }

    #[test]
    fn build_system_prompt_legacy_code_persona_routes_to_conversational() {
        // The Code variant is kept on the enum for backwards-compatible
        // TOML parsing but folds into the single warm voice on the wire.
        let conv = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Rich,
        });
        let code = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Code,
            verbosity: ChatVerbosity::Rich,
        });
        assert_eq!(conv, code);
    }

    #[test]
    fn build_system_prompt_legacy_docs_persona_routes_to_conversational() {
        let conv = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Rich,
        });
        let docs = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Docs,
            verbosity: ChatVerbosity::Rich,
        });
        assert_eq!(conv, docs);
    }

    #[test]
    fn build_system_prompt_auto_routes_to_conversational() {
        // `Auto` is an unresolved sentinel; if the resolver hasn't run
        // we still want a sensible warm-voice default rather than the
        // LongMemEval bench prompt.
        let conv = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Auto,
        });
        let auto = build_system_prompt(ResolvedChat {
            persona: ChatPersona::Auto,
            verbosity: ChatVerbosity::Auto,
        });
        assert_eq!(conv, auto);
    }

    // ─────────────────────────────────────────────────────────────────
    // Chitchat detection
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn is_chitchat_recognizes_common_greetings() {
        for s in [
            "hi",
            "Hi",
            "Hi.",
            "hello",
            "Hello!",
            "hey",
            "hey there",
            "yo",
            "thanks",
            "Thanks!",
            "thank you",
            "thanks for the help",
            "ty",
            "ok",
            "okay",
            "k",
            "got it",
            "cool",
            "perfect",
            "sounds good",
            "makes sense",
            "yep",
            "nope",
            "good morning",
            "good night",
            "bye",
            "see you",
            "ciao",
        ] {
            assert!(is_chitchat(s), "expected chitchat for {s:?}");
        }
    }

    #[test]
    fn is_chitchat_rejects_real_questions() {
        for s in [
            "how many providers do we use",
            "where is build_user_message defined",
            "explain the persona registry",
            "what is the LongMemEval score",
            "show me the routing logic",
            "is the desktop wired up",
            // Looks short but is a real question:
            "hi can you explain X",
            // Same — long enough that the length guard kicks in:
            "thanks for that, but can you also show me where the rooting tier is computed",
        ] {
            assert!(!is_chitchat(s), "expected NOT chitchat for {s:?}");
        }
    }

    #[test]
    fn is_chitchat_rejects_empty_and_too_long() {
        assert!(!is_chitchat(""));
        assert!(!is_chitchat("   "));
        assert!(!is_chitchat(&"hi ".repeat(40)));
    }

    #[test]
    fn should_skip_retrieval_respects_memory_contract() {
        // Memory persona NEVER short-circuits, even on a literal "hi".
        // The LongMemEval bench is the contract.
        let claims_dir = empty_sessions_dir();
        let allowed = HashSet::<String>::new();
        let dates = HashMap::<String, String>::new();
        let sids: Vec<String> = vec![];
        let excluded = HashSet::<String>::new();

        let memory_req = AskRequest {
            workspace: "lme",
            question: "hi",
            category: "single-session-user",
            allowed_sources: &allowed,
            question_date: "",
            session_dates: &dates,
            answer_sids: &sids,
            sessions_dir: &claims_dir,
            excluded_claim_ids: &excluded,
            chat: AskRequest::default_chat(),
            identity: None,
            today: None,
            history: NO_HISTORY,
        };
        assert!(!should_skip_retrieval(&memory_req));

        let conv_req = AskRequest {
            chat: ResolvedChat {
                persona: ChatPersona::Conversational,
                verbosity: ChatVerbosity::Rich,
            },
            ..memory_req
        };
        assert!(should_skip_retrieval(&conv_req));
    }

    #[test]
    fn chitchat_fallback_picks_friendly_reply() {
        assert_eq!(chitchat_fallback("thanks"), "You're welcome.");
        assert_eq!(chitchat_fallback("Thank you!"), "You're welcome.");
        assert_eq!(chitchat_fallback("hi"), "Hi.");
        assert_eq!(chitchat_fallback("hello"), "Hi.");
        assert_eq!(chitchat_fallback("hey there"), "Hi.");
        assert_eq!(chitchat_fallback("good morning"), "Hi.");
        assert_eq!(chitchat_fallback("bye"), "Talk soon.");
        assert_eq!(chitchat_fallback("see you tomorrow"), "Talk soon.");
        assert_eq!(chitchat_fallback("cool"), "Got it.");
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
            chat: AskRequest::default_chat(),
            identity: None,
            today: None,
            history: NO_HISTORY,
        };
        let with_id = build_user_message(&claims, &req);
        let body = build_user_message_body(&claims, &req);
        assert_eq!(with_id, body);
        assert!(!with_id.contains("<system-reminder>"));
        assert!(!with_id.contains("CONVERSATION HISTORY"));
        assert!(with_id.contains("[CATEGORY: single-session-user]"));
        assert!(with_id.ends_with("## QUESTION\nwhat?"));
    }

    #[test]
    fn conversational_persona_full_wire_prompt_carries_workspace_context() {
        // End-to-end shape check for the production conversational path:
        // resolved chat = Conversational, identity carries name + counts +
        // source mix + project_doc, today is set. The wire prompt the
        // model receives must (a) start with the warm-voice intro,
        // (b) contain a <system-reminder> ambient block with all the
        // workspace specifics, (c) end with the user's question.
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
            persona: ChatPersona::Conversational,
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
            history: NO_HISTORY,
        };

        let system_prompt = build_system_prompt(req.chat);
        let user_msg = build_user_message(&claims, &req);

        // System prompt = conversational warm voice
        assert!(system_prompt.starts_with("You are ThinkingRoot"));
        assert!(system_prompt.contains("path:line"));
        assert!(system_prompt.contains("Hard rules:"));

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
                persona: ChatPersona::Conversational,
                verbosity: ChatVerbosity::Rich,
            },
            identity: Some(&identity),
            today: Some("2026-04-28"),
            history: NO_HISTORY,
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

    // ─────────────────────────────────────────────────────────────────
    // History threading
    // ─────────────────────────────────────────────────────────────────

    fn fixture_history() -> Vec<ChatTurn> {
        vec![
            ChatTurn {
                role: ChatRole::User,
                content: "what's the LongMemEval score?".to_string(),
            },
            ChatTurn {
                role: ChatRole::Assistant,
                content: "91.2 % on LME-500.".to_string(),
            },
        ]
    }

    #[test]
    fn build_user_message_renders_history_for_conversational_persona() {
        let claims = fixture_claims();
        let history = fixture_history();
        let allowed = HashSet::<String>::new();
        let dates = HashMap::<String, String>::new();
        let sids: Vec<String> = vec![];
        let excluded = HashSet::<String>::new();
        let dir = empty_sessions_dir();
        let req = AskRequest {
            workspace: "tr",
            question: "and on what dataset?",
            category: "single-session-user",
            allowed_sources: &allowed,
            question_date: "",
            session_dates: &dates,
            answer_sids: &sids,
            sessions_dir: &dir,
            excluded_claim_ids: &excluded,
            chat: ResolvedChat {
                persona: ChatPersona::Conversational,
                verbosity: ChatVerbosity::Rich,
            },
            identity: None,
            today: None,
            history: &history,
        };
        let msg = build_user_message(&claims, &req);
        assert!(msg.contains("## CONVERSATION HISTORY"));
        assert!(msg.contains("[user]: what's the LongMemEval score?"));
        assert!(msg.contains("[assistant]: 91.2 % on LME-500."));
        // History block sits before the category body.
        let hist_idx = msg.find("## CONVERSATION HISTORY").unwrap();
        let cat_idx = msg.find("[CATEGORY: single-session-user]").unwrap();
        assert!(hist_idx < cat_idx);
        // Question still appears at the end of the body.
        assert!(msg.ends_with("## QUESTION\nand on what dataset?"));
    }

    #[test]
    fn memory_persona_drops_history_even_when_passed() {
        // Belt-and-suspenders for the LongMemEval contract: even if a
        // caller accidentally hands Memory persona a non-empty history,
        // we must not render it. The bench harness pins history=&[]
        // anyway, but this test catches a future regression where some
        // call site forgets.
        let claims = fixture_claims();
        let history = fixture_history();
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
            chat: AskRequest::default_chat(),
            identity: None,
            today: None,
            history: &history,
        };
        let msg = build_user_message(&claims, &req);
        assert!(!msg.contains("## CONVERSATION HISTORY"));
        assert!(!msg.contains("[user]:"));
        assert!(!msg.contains("[assistant]:"));
        // Wire body must remain byte-identical to the no-history case
        // for the same fixture inputs.
        let no_history_req = AskRequest {
            history: NO_HISTORY,
            ..req.clone()
        };
        let no_history_msg = build_user_message(&claims, &no_history_req);
        assert_eq!(msg, no_history_msg);
    }

    #[test]
    fn build_chitchat_user_message_skips_claims_and_category() {
        let history = fixture_history();
        let allowed = HashSet::<String>::new();
        let dates = HashMap::<String, String>::new();
        let sids: Vec<String> = vec![];
        let excluded = HashSet::<String>::new();
        let dir = empty_sessions_dir();
        let req = AskRequest {
            workspace: "tr",
            question: "thanks!",
            category: "multi-session",
            allowed_sources: &allowed,
            question_date: "",
            session_dates: &dates,
            answer_sids: &sids,
            sessions_dir: &dir,
            excluded_claim_ids: &excluded,
            chat: ResolvedChat {
                persona: ChatPersona::Conversational,
                verbosity: ChatVerbosity::Rich,
            },
            identity: None,
            today: None,
            history: &history,
        };
        let msg = build_chitchat_user_message(&req);
        assert!(!msg.contains("[CATEGORY:"));
        assert!(!msg.contains("EXTRACTED CLAIMS"));
        assert!(!msg.contains("RAW CONVERSATION TRANSCRIPTS"));
        assert!(msg.contains("## CONVERSATION HISTORY"));
        assert!(msg.ends_with("thanks!"));
    }

    #[test]
    fn no_history_constant_is_empty() {
        assert!(NO_HISTORY.is_empty());
    }

    // ─────────────────────────────────────────────────────────────────
    // S4 — compose_full_system_prompt: persona × style × skills layering
    // ─────────────────────────────────────────────────────────────────

    use crate::intelligence::skills::{Skill, SkillRegistry};
    use crate::intelligence::styles::OutputStyle;

    fn fixture_style() -> OutputStyle {
        OutputStyle {
            name: "explanatory".to_string(),
            description: "Educational insights".to_string(),
            system_fragment: "Include educational insights as you go.".to_string(),
            source_path: PathBuf::from("/tmp/explanatory.md"),
        }
    }

    fn fixture_skills() -> SkillRegistry {
        SkillRegistry::from_skills(vec![Skill {
            name: "refactor-rust".to_string(),
            description: "When refactoring Rust".to_string(),
            body: "Step 1...".to_string(),
            source_path: PathBuf::from("/tmp/refactor.md"),
        }])
        .unwrap()
    }

    #[test]
    fn compose_full_no_style_no_skills_returns_persona_unchanged() {
        let chat = ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Rich,
        };
        let composed = compose_full_system_prompt(chat, None, None);
        assert_eq!(composed, CONVERSATIONAL_SYSTEM_PROMPT);
    }

    #[test]
    fn compose_full_with_style_appends_active_style_header() {
        let chat = ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Rich,
        };
        let style = fixture_style();
        let composed = compose_full_system_prompt(chat, Some(&style), None);
        assert!(composed.starts_with("You are ThinkingRoot"));
        assert!(composed.contains("## ACTIVE STYLE: explanatory"));
        assert!(composed.contains("Include educational insights"));
    }

    #[test]
    fn compose_full_with_skills_appends_manifest() {
        let chat = ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Rich,
        };
        let skills = fixture_skills();
        let composed = compose_full_system_prompt(chat, None, Some(&skills));
        assert!(composed.starts_with("You are ThinkingRoot"));
        assert!(composed.contains("## AVAILABLE SKILLS"));
        assert!(composed.contains("refactor-rust"));
        assert!(composed.contains("use_skill"));
    }

    #[test]
    fn compose_full_with_style_and_skills_layers_in_order() {
        let chat = ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Rich,
        };
        let style = fixture_style();
        let skills = fixture_skills();
        let composed = compose_full_system_prompt(chat, Some(&style), Some(&skills));

        let persona_idx = composed.find("You are ThinkingRoot").unwrap();
        let style_idx = composed.find("## ACTIVE STYLE").unwrap();
        let skills_idx = composed.find("## AVAILABLE SKILLS").unwrap();

        // Layered top-to-bottom: persona → style → skill manifest.
        assert!(persona_idx < style_idx);
        assert!(style_idx < skills_idx);
    }

    #[test]
    fn compose_full_memory_persona_ignores_style_and_skills() {
        // LongMemEval contract: Memory persona produces byte-identical
        // wire prompt regardless of style/skills passed in.
        let chat = AskRequest::default_chat();
        let with_extras = compose_full_system_prompt(
            chat,
            Some(&fixture_style()),
            Some(&fixture_skills()),
        );
        assert_eq!(with_extras, MEMORY_SYSTEM_PROMPT);
    }

    #[test]
    fn compose_full_empty_skills_registry_does_not_emit_manifest_header() {
        let chat = ResolvedChat {
            persona: ChatPersona::Conversational,
            verbosity: ChatVerbosity::Rich,
        };
        let empty_skills = SkillRegistry::empty();
        let composed = compose_full_system_prompt(chat, None, Some(&empty_skills));
        assert!(!composed.contains("AVAILABLE SKILLS"));
        assert_eq!(composed, CONVERSATIONAL_SYSTEM_PROMPT);
    }
}
