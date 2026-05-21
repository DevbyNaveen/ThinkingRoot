// crates/thinkingroot-serve/src/intelligence/intent.rs
//
// Ship 3C (2026-05-20) — Cognitive mode router.
//
// Classifies each chat turn into one of four modes BEFORE the agent
// runs, so the system prompt's workflow appendix matches the turn's
// intent rather than forcing one-size-fits-all "READ first" behaviour
// onto planning + operator + change-implementation turns.
//
// **Four modes:**
//
//   * `Plan`      — the user is sharing a plan / draft / brainstorm.
//                   Long pasted content, framing words ("plan",
//                   "design", "let's think"), or proposal-shaped
//                   asks ("what do you think of X", "should we Y").
//                   Workflow: engage with their content; skip
//                   retrieval unless they reference workspace state
//                   explicitly.
//
//   * `Substrate` — the user is querying or asking-for-explanation
//                   about the compiled knowledge graph. "Where is X",
//                   "how does Y work", "show me Z". Workflow: READ
//                   first via hybrid_retrieve / search / query_claims
//                   / probe_engram. This is the existing default.
//
//   * `Act`       — the user wants the agent to MAKE a change.
//                   "Implement", "fix", "refactor", "edit file
//                   X to do Y". Workflow: read context first (grep,
//                   read_file), then propose write tools that route
//                   through the ApprovalGate.
//
//   * `Operator`  — substrate self-heal. Recent recovery events
//                   indicate compile failures, breaker trips, stale
//                   locks. Workflow: surface `recovery_log_tail` /
//                   `doctor_run` / `reset_*_breaker` tools, walk
//                   through the diagnosis, propose a fix.
//
// **Classifier design:** deterministic keyword + signal heuristics,
// fast (sub-µs), correct on the obvious cases. Returns `Intent::Auto`
// for the ambiguous middle — the agent then runs under the default
// `Substrate` workflow which is the safe choice (retrieve, ground,
// answer).
//
// We DON'T LLM-tiebreak from a separate API call: the cost would
// dominate the turn for marginal accuracy, and the keyword classifier
// already routes the high-leverage cases (planning vs lookup) with
// no false positives that matter. The system prompt's "Classify the
// turn first" preamble (Ship 2, synthesizer.rs) handles the
// fine-grained ambiguity at LLM-decision time anyway.

use serde::{Deserialize, Serialize};

/// The four cognitive modes the agent can run in. Picked by
/// [`classify_intent`] per turn; threaded into the system prompt's
/// workflow appendix so the model gets mode-appropriate guidance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// User is brainstorming / sharing a plan / proposing a design.
    /// Don't force retrieval; engage with their content.
    Plan,
    /// User is querying the compiled substrate. Default workflow:
    /// READ first via the canonical retrieval tools.
    Substrate,
    /// User wants the agent to make a change. File ops + write
    /// tools.
    Act,
    /// Substrate self-heal context. Doctor / recovery / breaker
    /// reset tools.
    Operator,
}

impl Intent {
    /// Stable slug used in trace logs + the `<workflow_mode>` reminder
    /// block. Pinned per release; renaming breaks logs.
    pub fn slug(&self) -> &'static str {
        match self {
            Intent::Plan => "plan",
            Intent::Substrate => "substrate",
            Intent::Act => "act",
            Intent::Operator => "operator",
        }
    }
}

/// Inputs the classifier uses. All optional except `question` so
/// CLI callers (no recovery context, no session) can still classify.
pub struct ClassifyInputs<'a> {
    /// The user's most-recent message — the primary signal.
    pub question: &'a str,
    /// `true` when the daemon's recovery log has events in the last
    /// 5 minutes (compile failures, breaker trips). When set with no
    /// stronger signal, the classifier picks `Operator`.
    pub has_recent_recovery_events: bool,
    /// `true` when the workspace status snapshot reports
    /// `compile.last_outcome` was `Failed`. Same operator-mode trigger
    /// as `has_recent_recovery_events`; either one suffices.
    pub last_compile_failed: bool,
}

/// Lower bound for treating a paste as PLAN regardless of keyword
/// match. Empirical: long unstructured pastes are overwhelmingly
/// plans / drafts / brainstorms. ≥ 240 chars + a planning verb
/// triggers Plan; just ≥ 240 chars alone falls through to Substrate
/// (it might be a paste-and-ask).
const PLAN_LENGTH_FLOOR: usize = 240;

/// Planning-intent keywords. Case-insensitive substring match against
/// the question's first 400 chars (a short user message that mentions
/// "plan" once is enough; we don't penalise wordy responses).
const PLAN_KEYWORDS: &[&str] = &[
    "plan",
    "brainstorm",
    "design",
    "ideate",
    "let's think",
    "what do you think",
    "should we",
    "propose",
    "draft",
    "sketch",
    "idea:",
    "thinking about",
];

/// Substrate-lookup keywords. Strong signal that the user wants
/// retrieval against the compiled graph.
const SUBSTRATE_KEYWORDS: &[&str] = &[
    "where is",
    "where's",
    "how does",
    "what does",
    "what is",
    "show me",
    "find",
    "look up",
    "explain",
    "tell me about",
    "list the",
    "which file",
];

/// Act / change-the-code keywords. Triggers Act mode so the agent
/// reaches for file_read / grep / shell + write tools rather than
/// hybrid_retrieve. Trailing spaces are deliberate — keep the
/// keyword from matching substrings of substantive nouns
/// (e.g., "implement" matching "implementation" in a where-query).
const ACT_KEYWORDS: &[&str] = &[
    "implement ",
    "add ",
    "fix ",
    "refactor ",
    "rename ",
    "delete ",
    "remove ",
    "rewrite ",
    "edit ",
    "update ",
    "change ",
    "make it",
    "make this",
    "write a",
    "create a",
];

/// Operator / self-heal keywords. Triggers Operator mode.
const OPERATOR_KEYWORDS: &[&str] = &[
    "doctor",
    "compile fail",
    "compile broke",
    "breaker",
    "recovery",
    "wedged",
    "stuck",
    "won't start",
    "won't compile",
    "can't compile",
    "engine crash",
];

/// Classify a turn's intent. Deterministic; same inputs → same
/// `Intent` byte-for-byte across runs. Sub-µs cost.
///
/// **Priority order** (first match wins):
///   1. Operator keywords OR `has_recent_recovery_events` /
///      `last_compile_failed` — substrate health beats all.
///   2. Plan keywords + length ≥ floor → Plan.
///   3. Act keywords → Act.
///   4. Substrate keywords → Substrate.
///   5. Fallback → Substrate (the safe default; retrieve before
///      answering).
pub fn classify_intent(inputs: &ClassifyInputs<'_>) -> Intent {
    let q_lower = inputs.question.trim().to_lowercase();
    let q_head: String = q_lower.chars().take(400).collect();

    // ── Operator override: substrate health beats content classification.
    // A compile-broken environment must surface diagnostics before
    // anything else, regardless of how the user phrased their question.
    if OPERATOR_KEYWORDS.iter().any(|k| q_head.contains(k)) {
        return Intent::Operator;
    }
    if (inputs.has_recent_recovery_events || inputs.last_compile_failed)
        // Only auto-route to Operator on weak content signals — if
        // the user is clearly asking a substrate question while a
        // breaker is tripped, respect their intent and let them
        // discover the failure via the `<substrate_health>`
        // reminder. Threshold: no other strong keyword present.
        && !ACT_KEYWORDS.iter().any(|k| q_head.contains(k))
        && !SUBSTRATE_KEYWORDS.iter().any(|k| q_head.contains(k))
    {
        return Intent::Operator;
    }

    // ── Plan classification: keyword + length floor jointly required.
    let has_plan_kw = PLAN_KEYWORDS.iter().any(|k| q_head.contains(k));
    if has_plan_kw && q_lower.len() >= PLAN_LENGTH_FLOOR {
        return Intent::Plan;
    }

    // ── Act vs Substrate: Act keywords are imperative + specific;
    //    Substrate keywords are interrogative. Imperative wins when
    //    both fire (an "implement the search that shows X" turn is
    //    Act, not Substrate).
    if ACT_KEYWORDS.iter().any(|k| q_head.contains(k)) {
        return Intent::Act;
    }

    // Substrate fallback — the safe default. Retrieval first is the
    // existing v1.0 behaviour; the prompt's "Classify the turn first"
    // preamble (Ship 2) handles the residual ambiguity at LLM-decision
    // time.
    Intent::Substrate
}

/// Workflow appendix to splice into the system prompt for each mode.
/// The base `CONVERSATIONAL_SYSTEM_PROMPT` carries the cross-mode
/// principles; the appendix tightens the workflow for the active mode.
///
/// Each appendix is rendered as a `<workflow_mode>` block. Stable
/// strings — the prompt-contract tests in `synthesizer.rs` pin the
/// substring presence.
pub fn workflow_appendix(intent: Intent) -> &'static str {
    match intent {
        Intent::Plan => PLAN_APPENDIX,
        Intent::Substrate => SUBSTRATE_APPENDIX,
        Intent::Act => ACT_APPENDIX,
        Intent::Operator => OPERATOR_APPENDIX,
    }
}

const PLAN_APPENDIX: &str = "\n\n<workflow_mode>\nMode: PLAN.\n\nThe user is sharing a plan, draft, design, or open question. \
Engage with their content directly — react, push back on weak assumptions, propose alternatives, surface tradeoffs. \
DO NOT call retrieval unless the user explicitly references a workspace symbol/file/branch or asks you to verify a \
claim against the substrate. \"Found in workspace\" is the wrong opener here. \
Defer write tools (`contribute_claim`, `create_branch`, `merge_branch`) until the user has agreed on a direction; \
proposing them mid-brainstorm is premature commitment.\n</workflow_mode>\n";

const SUBSTRATE_APPENDIX: &str = "\n\n<workflow_mode>\nMode: SUBSTRATE.\n\nThe user is asking about the compiled knowledge graph. \
Run the canonical READ-first protocol: `hybrid_retrieve` for ranked evidence, `query_claims` when you already know the entity, \
`probe_engram` for sustained drilling. Cite inline with `[claim:<id>]`. If retrieval comes back empty, say so plainly. \
Stop at the asked scope — don't bundle a redesign onto a \"where is X?\" turn.\n</workflow_mode>\n";

const ACT_APPENDIX: &str = "\n\n<workflow_mode>\nMode: ACT.\n\nThe user wants a change. Read the relevant files first \
(`file_read`, `grep`, `glob`) to ground the change in the current code — NOT just the substrate, which may be behind disk. \
Then propose your edit; write tools (`file_write`, `file_edit`, `shell_exec`, `contribute_claim`) route through the \
ApprovalGate and the user will confirm. When the change touches existing claims, consider opening a sandbox branch \
(`create_branch` with kind: Sandbox + merge_policy: Ephemeral) so the user can review before main commits.\n</workflow_mode>\n";

const OPERATOR_APPENDIX: &str = "\n\n<workflow_mode>\nMode: OPERATOR.\n\nThe substrate is in a degraded state — recent \
recovery events, a failed compile, or the user has explicitly invoked self-heal language. \
Use `recovery_log_tail` to read what happened, `restart_state_get` for breaker status, `doctor_run` for a full diagnostic, \
and `reset_circuit_breaker` / `reset_compile_breaker` ONLY after the underlying fault is understood. \
Don't paper over a wedged daemon with a `restart_engine_request`; surface the root cause first.\n</workflow_mode>\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(q: &str) -> ClassifyInputs<'_> {
        ClassifyInputs {
            question: q,
            has_recent_recovery_events: false,
            last_compile_failed: false,
        }
    }

    #[test]
    fn operator_keyword_wins_over_everything() {
        assert_eq!(
            classify_intent(&inputs("the compile failed, what do we do")),
            Intent::Operator
        );
        assert_eq!(
            classify_intent(&inputs("can you run the doctor")),
            Intent::Operator
        );
    }

    #[test]
    fn operator_signal_routes_when_content_is_neutral() {
        let i = ClassifyInputs {
            question: "hey",
            has_recent_recovery_events: true,
            last_compile_failed: false,
        };
        assert_eq!(classify_intent(&i), Intent::Operator);
    }

    #[test]
    fn operator_signal_respects_clear_substrate_question() {
        let i = ClassifyInputs {
            question: "where is the WebhookHandler defined?",
            has_recent_recovery_events: true,
            last_compile_failed: true,
        };
        // Substrate keyword overrides the operator-signal default.
        assert_eq!(classify_intent(&i), Intent::Substrate);
    }

    #[test]
    fn plan_requires_both_keyword_and_length() {
        // Keyword alone, short → not Plan.
        assert_ne!(classify_intent(&inputs("let's plan")), Intent::Plan);
        // Length alone (no keyword) → Substrate fallback.
        let long_no_kw = "a".repeat(500);
        assert_eq!(
            classify_intent(&inputs(&long_no_kw)),
            Intent::Substrate
        );
        // Both → Plan.
        let plan_paste = format!(
            "I'm thinking about how we should plan the next architecture phase. {}",
            "x ".repeat(150)
        );
        assert_eq!(classify_intent(&inputs(&plan_paste)), Intent::Plan);
    }

    #[test]
    fn act_keyword_routes_to_act() {
        assert_eq!(
            classify_intent(&inputs("implement a retry policy in the worker")),
            Intent::Act
        );
        assert_eq!(
            classify_intent(&inputs("fix the off-by-one in pagination")),
            Intent::Act
        );
        assert_eq!(
            classify_intent(&inputs("refactor the auth middleware")),
            Intent::Act
        );
    }

    #[test]
    fn substrate_keyword_routes_to_substrate() {
        assert_eq!(
            classify_intent(&inputs("where is the connector implementation?")),
            Intent::Substrate
        );
        assert_eq!(
            classify_intent(&inputs("how does the engram cache work?")),
            Intent::Substrate
        );
    }

    #[test]
    fn unknown_query_falls_back_to_substrate() {
        // Bare question with no classifier signal → Substrate (the
        // safe default — retrieve first).
        assert_eq!(classify_intent(&inputs("hello?")), Intent::Substrate);
    }

    #[test]
    fn intent_slug_is_stable() {
        // Slugs are wire-format for trace logs + the prompt block;
        // changing them breaks log greps + the prompt contract test.
        assert_eq!(Intent::Plan.slug(), "plan");
        assert_eq!(Intent::Substrate.slug(), "substrate");
        assert_eq!(Intent::Act.slug(), "act");
        assert_eq!(Intent::Operator.slug(), "operator");
    }

    #[test]
    fn workflow_appendices_carry_stable_mode_label() {
        // Each appendix must name its own mode so a future edit can't
        // silently swap them. The prompt-contract test in
        // synthesizer.rs pins the same strings.
        assert!(workflow_appendix(Intent::Plan).contains("Mode: PLAN"));
        assert!(workflow_appendix(Intent::Substrate).contains("Mode: SUBSTRATE"));
        assert!(workflow_appendix(Intent::Act).contains("Mode: ACT"));
        assert!(workflow_appendix(Intent::Operator).contains("Mode: OPERATOR"));
    }

    #[test]
    fn classify_is_deterministic() {
        let q = "show me where the WebhookHandler is";
        let a = classify_intent(&inputs(q));
        let b = classify_intent(&inputs(q));
        assert_eq!(a, b);
    }
}
