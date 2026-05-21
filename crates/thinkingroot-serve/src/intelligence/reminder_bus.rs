// crates/thinkingroot-serve/src/intelligence/reminder_bus.rs
//
// Reactive `<system-reminder>` bus.
//
// Every chat turn injects a small amount of "ambient context" into
// the LLM's user message — workspace identity, branch state, session
// state, active engram pointers, tool budget. Each is wrapped in a
// `<system-reminder>` block; each emitter checks its precondition and
// returns `None` when nothing relevant changed. The aggregator
// concatenates the live ones in a stable order so the LLM sees the
// same shape every turn.
//
// This is the canonical 2026 Anthropic harness pattern: keep the
// system prompt static (frozen for prompt cache), inject dynamic
// context per-turn through user-message-level reminders that the
// model is told to treat as "ambient context, may or may not be
// relevant." Mirrors Claude Code's 37-category reminder bus
// (michaellivs.com/blog/system-reminders-steering-agents).
//
// v1.0 emitters (all fully wired against real substrate state):
//   * `workspace`      — name, claim_count, source mix, today's date
//   * `branch_state`   — active branch + its parent
//   * `session_state`  — focus entity, delivered-claim count
//   * `engram_state`   — active engram pointers + budget
//   * `tool_budget`    — remaining tool calls in this turn
//
// v1.2 emitters (shipped 2026-05-20 as part of the world-class
// SOTA harness ship — Ships 3A/3B/3D/3E):
//   * `substrate_freshness`  — "brain is N edits behind disk; recompile
//     before answering critical questions". Sourced from
//     `SourcesState.fingerprint_match` which Ship 1 made honest.
//   * `sub_agent_digest`     — last few `SubAgentReport`s from the
//     `SubAgentScheduler` (Reconciler / GapHunter / Curator / Watcher)
//     so the executive chat agent reads what the sleep-time
//     compute layer found. Closes the observer→executive loop.
//   * `previous_verify`      — last-turn verifier critique. When the
//     prior answer scored low-grounding or had contradictions,
//     surface it so the next turn self-corrects (Reflexion pattern).
//   * `gap_alert`            — open gaps from `reflect::list_open_gaps`
//     filtered to focus entity.
//   * `contradiction_alert`  — branch-diff contradictions vs focus.
//   * `search_was_shallow`   — last hybrid_retrieve returned ≤ K hits;
//     warn before re-running with same query.

use crate::intelligence::environment::{EnvironmentInfo, render_block as render_env_inner};
use crate::intelligence::identity::{WorkspaceIdentity, render_identity_block};
use crate::intelligence::session::SessionContext;

/// Slim view of an active engram for the reminder bus. Decoupled from
/// the heavyweight `EngramSummary` (which carries 20+ fields including
/// full call-graph edges, source authority, gaps, etc.) so the bus
/// stays a pure renderer and the engram crate's struct can evolve
/// without churning prompt-rendering tests.
///
/// Callers populate this from `EngramManager::list_engrams_for_session`
/// or equivalent — usually one entry per active engram pointer.
#[derive(Debug, Clone)]
pub struct EngramHandle {
    /// The engram pointer string emitted by `materialize_engram`,
    /// e.g. "0x7A3F". Rendered as-is.
    pub pointer: String,
    /// Free-text topic the engram was materialised for.
    pub topic: String,
}

/// Slim per-claim recall row injected as cross-session agentmemory
/// context. Top-K of these are surfaced automatically — Mem0/Letta-
/// style — so the AI starts each turn with the most-relevant prior
/// claims visible without burning a `search` / `hybrid_retrieve`
/// round-trip just to bootstrap.
///
/// Decoupled from `ClaimSearchHit` so reminder-bus tests don't drag
/// in the engine/CozoDB surface and the upstream struct can evolve.
/// Caller (typically `rest.rs::agent_stream_response`) computes the
/// recall via the same retrieval primitives the agent would call —
/// keeps the substrate-as-ground-truth contract.
#[derive(Debug, Clone)]
pub struct AgentmemoryRecall {
    /// Claim id (the `[claim:<id>]` form the citation parser expects).
    pub claim_id: String,
    /// Short statement text — pre-truncated by the caller so the bus
    /// doesn't have to make a budget decision. Recommended ≤ 240
    /// chars; the bus enforces a hard 480-char cap defensively.
    pub statement: String,
    /// Confidence in `[0.0, 1.0]`. Surfaced inline so the LLM can
    /// down-weight low-confidence recalls without re-querying.
    /// `f64` matches `ClaimSearchHit::confidence`.
    pub confidence: f64,
    /// Source path / URI for citation. Either `path:line` style or
    /// a `mcp://` / `connector://` URI. Free-form — the bus passes
    /// through.
    pub source_uri: String,
}

/// Slim view of an MCP-connected AI tool. Surfaces the User-Agent
/// and counters so the in-app operator-mode AI has cross-tool
/// awareness without calling `list_mcp_sessions` for every turn.
#[derive(Debug, Clone)]
pub struct McpSessionBrief {
    /// First 12 chars of the session UUID — long enough to identify,
    /// short enough to keep the reminder compact.
    pub session_id_prefix: String,
    /// User-Agent header at session open ("cursor/1.5.2", "claude-code/0.4.1",
    /// "python-httpx/0.27.0"). Free-form — Cursor / Claude Code /
    /// Cline / aider / OpenClaw all use distinct strings.
    pub user_agent: String,
    /// Transport: "sse" / "stdio" / "agentmemory".
    pub transport: String,
    /// Total tool calls observed on this session this run.
    pub tool_calls_total: u64,
    /// Total errors observed. Non-zero is a debug signal — the AI
    /// can surface "Cursor's session has 5 errors, want me to look?"
    pub errors_total: u64,
}

/// Honest substrate-freshness signal. Ship 1 (2026-05-20) wired
/// `SourcesState.fingerprint_match` to the truth via mtime
/// comparison; this struct surfaces it to the LLM via the reminder
/// bus so the agent stops answering from stale claims as if they
/// were current. CLAUDE.md §honesty rule §7 at the prompt layer.
///
/// The presence of this struct in `ReminderContext` means "we have
/// freshness data to report"; the renderer suppresses the block
/// when `fingerprint_match == true` because there's nothing useful
/// to say ("brain is current" is trivially true and burns context).
#[derive(Debug, Clone)]
pub struct SubstrateFreshness {
    /// `true` when the compiled substrate is at-or-ahead of disk
    /// (file mtimes ≤ fingerprints.json mtime). `false` when the
    /// user has edited/added/removed sources since the last compile.
    pub fingerprint_match: bool,
    /// ISO-8601 timestamp of the last successful compile (`fingerprints.json`
    /// mtime). `None` when no compile has ever run for this workspace.
    pub last_compile_at_iso: Option<String>,
    /// Best-effort count of source files seen by the latest probe.
    /// Sourced from `SourcesState::Some::file_count`. Honest 0 is
    /// possible — empty workspace.
    pub file_count: u64,
}

/// Slim per-tick summary from a background sub-agent
/// (`SubAgentScheduler`'s Reconciler / GapHunter / Curator /
/// Watcher). Surfaced via `recent_sub_agent_reports` so the
/// executive chat agent reads what the sleep-time compute layer
/// found in the background, closing the observer→executive loop
/// that was the canonical Letta / MemGPT pattern's missing wire.
///
/// Decoupled from `intelligence::substrate_bus::SubAgentReport` so
/// reminder-bus tests stay independent of the scheduler module's
/// concrete shape — caller projects.
#[derive(Debug, Clone)]
pub struct SubAgentReportBrief {
    /// Agent slug — `"reconciler"`, `"gap_hunter"`, `"curator"`,
    /// `"watcher"`. Rendered verbatim.
    pub agent: String,
    /// ISO-8601 timestamp the tick finished. Lets the LLM judge
    /// recency ("Reconciler ran 8 minutes ago…").
    pub finished_at_iso: String,
    /// One-line summary suitable for the reminder body. Empty when
    /// the tick was a no-op — the bus suppresses empty entries.
    pub summary: String,
    /// Up to 3 most-salient observations from this tick, already
    /// truncated by the caller. Rendered as sub-bullets.
    pub observations: Vec<String>,
}

/// One-shot critique from the post-Done verifier. When the previous
/// turn's grounding was weak (low-ratio cited claims, missing
/// substrate evidence, flagged contradictions), this carries the
/// critique forward into the next turn's reminder bus so the agent
/// reads its own past mistake and self-corrects on the redo. This
/// is the Reflexion pattern (Shinn et al 2023): post-task critique
/// that biases the next task's planning, never silently dropped.
///
/// Suppressed when `kind` is `"chitchat"` / `"skipped_rejection"` /
/// `"skipped_bench"` — those verdict classes don't carry useful
/// critique signal.
#[derive(Debug, Clone)]
pub struct PreviousVerifyCritique {
    /// Verdict slug as a snake_case string. Production values:
    /// `"high_grounding"`, `"low_grounding"`, `"ungrounded"`,
    /// `"contradiction"`, `"skipped_*"`, `"chitchat"`.
    pub verdict: String,
    /// Number of claims the previous answer cited that VERIFIED
    /// against the substrate. `None` when the verifier didn't track.
    pub citations_verified: Option<u32>,
    /// Number of claims cited that did NOT verify (claim id not in
    /// substrate, or content_blake3 mismatch). `None` when not tracked.
    pub citations_unverified: Option<u32>,
    /// One-line human-readable reason. Pre-formatted by the caller
    /// (`rest.rs::agent_stream_response` post-verify hook).
    pub reason: String,
}

/// Open structural gap surfaced by `reflect::list_open_gaps` filtered
/// to the session's focus entity. Lets the agent name the gaps in
/// its first response without paying a separate tool round-trip.
/// Truncated to top 5 by the caller; bus enforces ≤ 10 defensively.
#[derive(Debug, Clone)]
pub struct GapAlert {
    /// Free-form gap kind label from `reflect::OpenGap.kind`. Examples:
    /// `"missing_origin"`, `"missing_definition"`, `"untyped_relation"`.
    pub kind: String,
    /// Subject entity name the gap is about. Usually matches or
    /// neighbours the session's focus entity.
    pub subject: String,
    /// One-line "what's missing" hint for the LLM. Pre-formatted by
    /// the caller from the gap's `details` field.
    pub hint: String,
}

/// Branch-diff contradiction surfaced when the active session's
/// branch carries claims that contradict main (or vice-versa).
/// World-class agents surface conflict; lesser ones answer one
/// arbitrarily and hope. Caller computes via `branch::diff::contradictions`.
#[derive(Debug, Clone)]
pub struct ContradictionAlert {
    /// Subject entity the contradiction is about.
    pub subject: String,
    /// Statement from the active branch (the "new" claim).
    pub branch_says: String,
    /// Statement from main (the "old"/baseline claim).
    pub main_says: String,
}

/// Signal that the last `hybrid_retrieve` returned thin evidence
/// (≤ `SHALLOW_RETRIEVAL_THRESHOLD` hits). Warns the agent that
/// re-running the same query is unlikely to help; better to
/// rephrase, drill via `probe_engram`, or admit the substrate
/// doesn't carry the answer. Sourced from `RetrievalCapture`.
#[derive(Debug, Clone)]
pub struct SearchWasShallow {
    /// The query string that produced thin results. Quoted in the
    /// reminder so the LLM doesn't re-run it verbatim.
    pub query: String,
    /// Hit count from the last retrieval call. `0` is the worst
    /// case ("no hits at all"); ≤ threshold but > 0 is "weak".
    pub hits: u32,
}

/// Slim view of a recent self-heal event (compile failure, breaker
/// trip, stale-lock cleanup). Surfaced when non-empty in the last 5
/// minutes so the operator-mode AI proactively notices a wedged
/// substrate without polling `recovery_log_tail` every turn.
#[derive(Debug, Clone)]
pub struct RecoveryEventBrief {
    /// One of the canonical event kinds from `recovery_log::RecoveryEventKind`
    /// as a snake_case string ("compile_failed", "compile_breaker_tripped",
    /// "restart_breaker_tripped", "stale_lock_cleanup", "compile_recovered",
    /// "compile_retry_scheduled"). The bus renders as-is.
    pub kind: String,
    /// Workspace the event applies to, when scoped. `None` for
    /// daemon-global events.
    pub workspace: Option<String>,
    /// ISO-8601 timestamp of the event.
    pub at_iso: String,
    /// One-line summary the AI can quote inline. Pre-formatted by
    /// the caller from the event's payload.
    pub summary: String,
}

/// Snapshot of every input the bus draws on for one turn. All fields
/// are optional — the corresponding emitter is suppressed when its
/// data source is missing, which is the right behaviour for callers
/// like the LongMemEval bench harness that intentionally pass none.
#[derive(Debug, Clone, Default)]
pub struct ReminderContext<'a> {
    /// Host environment snapshot — cwd, OS, $HOME, ~/Desktop, etc.
    /// When `Some`, the bus emits an `# environment` block FIRST
    /// (before workspace identity) so the LLM can resolve common
    /// locations like "Desktop" without asking the user. Mirrors
    /// Claude Code's `computeSimpleEnvInfo` injection mechanism
    /// (`prompts.ts:651-710`).
    pub environment: Option<&'a EnvironmentInfo>,
    /// Workspace identity (name, claim_count, sources, project_doc).
    /// When `None`, the `<workspace>` reminder is omitted; this
    /// preserves the LongMemEval byte-identity contract for callers
    /// that pass `identity: None` on the synthesizer's `AskRequest`.
    pub identity: Option<&'a WorkspaceIdentity>,
    /// ISO-8601 date string ("2026-05-09") for the `# today` line.
    /// Only consumed when `identity` is also `Some`.
    pub today: Option<&'a str>,
    /// Per-session state: focus entity, delivered-claim dedup, etc.
    pub session: Option<&'a SessionContext>,
    /// Branch description for the active session branch. Constructed
    /// by the caller from the branch registry — the bus does NOT
    /// reach into the branch crate so it remains a pure renderer.
    pub branch: Option<BranchSummary>,
    /// Snapshot of every engram pointer currently materialised for
    /// the session. Empty slice when no engrams are active. Bus
    /// suppresses the `<engram_state>` block on empty.
    pub engrams: &'a [EngramHandle],
    /// Maximum engrams the session is allowed (mirrors the engram
    /// manager's `max_engrams_per_session`). Used in the `budget`
    /// line of the engram block.
    pub engram_budget: usize,
    /// Tool calls remaining for this turn. `None` means "no budget
    /// tracking" (CLI / bench paths); `Some(n)` triggers the
    /// `<tool_budget>` reminder when `n` is below the threshold.
    pub tool_budget_remaining: Option<usize>,
    /// Maximum tool calls per turn (typically `max_iterations` from
    /// `AgentConfig`). Used to render "remaining: 4 / 12" form.
    pub tool_budget_max: Option<usize>,
    /// Free-text "reason" string from the sandbox classifier
    /// (`intelligence/sandbox_classifier.rs`). When set, the bus
    /// emits a `<sandbox_alert>` block recommending the agent open
    /// an Ephemeral sandbox before any contribution. The classifier
    /// returns one of a small set of canonical reasons
    /// ("refactor intent", "fix intent", …) so the rendered text is
    /// stable per intent class.
    pub sandbox_recommendation: Option<&'a str>,
    /// Top-K agentmemory recalls — Mem0/Letta-style cross-session
    /// memory surfaced automatically per turn. Empty slice when the
    /// caller chose not to surface recalls (CLI flows, bench
    /// harness, or simply nothing matched). Bus suppresses the
    /// `<agentmemory_recall>` block on empty.
    pub agentmemory_recalls: &'a [AgentmemoryRecall],
    /// MCP-connected AI sessions (snapshot from
    /// `mcp::telemetry::snapshot`). When non-empty, the bus emits an
    /// `<mcp_sessions>` block so the operator-mode AI has cross-tool
    /// awareness without polling `list_mcp_sessions`. Empty slice
    /// suppresses the block entirely.
    pub mcp_sessions: &'a [McpSessionBrief],
    /// Recent (≤ 5 min) self-heal events worth surfacing. Caller
    /// (typically `rest.rs`) tails `recovery_log` and filters by
    /// recency + relevance. Empty slice suppresses the block.
    pub recovery_events: &'a [RecoveryEventBrief],
    /// Auto-surfaced skill — top-1 keyword match against the user's
    /// message — with full body inlined so the AI doesn't have to
    /// burn a `use_skill` round-trip on the common case. When
    /// `Some`, the bus renders the body in a `<relevant_skill>`
    /// block tagged with the skill name. Caller is responsible for
    /// the classification; the bus is a pure renderer.
    pub relevant_skill: Option<RelevantSkill<'a>>,
    /// Substrate-freshness signal (Ship 3A, 2026-05-20). Surfaced
    /// when the daemon's source-tree watcher has flipped
    /// `fingerprint_match` to false — the agent SEES that the brain
    /// is N edits behind disk and can decline / qualify / suggest
    /// recompile rather than answering from stale claims as if they
    /// were current.
    pub substrate_freshness: Option<&'a SubstrateFreshness>,
    /// Last few sub-agent reports (Ship 3B, 2026-05-20). Closes the
    /// observer→executive loop: the chat agent reads what the
    /// `SubAgentScheduler`'s background sleep-time-compute layer
    /// found (open gaps, structural patterns, branch drift). Empty
    /// slice suppresses the block.
    pub recent_sub_agent_reports: &'a [SubAgentReportBrief],
    /// Critique from the previous turn's verifier (Ship 3D,
    /// 2026-05-20). When the prior answer scored weak grounding,
    /// surface it so the agent self-corrects rather than repeating
    /// the same evidence-thin pattern. Reflexion (Shinn et al 2023).
    pub previous_verify_critique: Option<&'a PreviousVerifyCritique>,
    /// Open gaps in the focus area (Ship 3E, finishes v1.1 TODO).
    /// Empty slice suppresses.
    pub gap_alerts: &'a [GapAlert],
    /// Branch-diff contradictions (Ship 3E). Empty slice suppresses.
    pub contradiction_alerts: &'a [ContradictionAlert],
    /// Most recent thin retrieval (Ship 3E). `None` when the last
    /// retrieval either wasn't shallow or none has happened yet.
    pub search_was_shallow: Option<&'a SearchWasShallow>,
}

/// Slim view of an auto-surfaced skill — name plus body. The body
/// is borrowed from the caller's `SkillRegistry` so no allocation
/// is required on the hot path. Decoupled from `skills::Skill` so
/// bus tests don't carry the file-format machinery.
#[derive(Debug, Clone, Copy)]
pub struct RelevantSkill<'a> {
    pub name: &'a str,
    pub body: &'a str,
}

/// Subset of the branch crate's `BranchRef` the renderer consumes —
/// kept here as a value type so the serve crate's prompt-rendering
/// layer doesn't take a load-bearing dependency on the branch crate's
/// concrete shapes (which are still under active design pressure).
#[derive(Debug, Clone)]
pub struct BranchSummary {
    /// Branch name, e.g. `stream/chat-052` or `main`.
    pub name: String,
    /// Parent branch, if any. `None` when the branch IS main / root.
    pub parent: Option<String>,
    /// Optional kind tag — `Stream`, `Sandbox`, `Working`, `Tag`. Free-form
    /// because the typed enum lives in `thinkingroot-core` and we don't
    /// want a string-conversion step on every render.
    pub kind: Option<String>,
}

/// Threshold below which `<tool_budget>` fires. With 12 max calls per
/// turn (the AgentConfig default) the budget reminder appears in the
/// last ~25% of the turn so the model can wind down rather than be
/// surprised by the iteration ceiling.
const TOOL_BUDGET_WARN_THRESHOLD: usize = 3;

/// Render every applicable reminder block, in stable order, ready to
/// prepend to the user message. Returns an empty string when no
/// emitter has anything to say.
///
/// Order matters for prompt-cache stability: same context → same
/// rendered prefix → cache hit on the next turn that doesn't change
/// anything visible to the bus. Don't reorder these calls casually.
pub fn render_reactive_reminders(ctx: &ReminderContext<'_>) -> String {
    let mut out = String::new();
    // Order is load-bearing for prompt caching (stable prefix → cache
    // hit) AND for LLM attention budget: environment + workspace are
    // most universally relevant, agentmemory + relevant-skill prime
    // the answer, branch/session/engram tune behaviour, MCP/recovery
    // surface operator context, sandbox/tool_budget are advisory
    // wind-down signals.
    if let Some(s) = render_environment_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_workspace_block(ctx) {
        out.push_str(&s);
    }
    // Substrate freshness rides immediately after workspace identity:
    // the agent should know the brain may be behind disk BEFORE it
    // reads the agentmemory recall (which is sourced from the same
    // potentially-stale substrate).
    if let Some(s) = render_substrate_freshness_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_agentmemory_recall_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_relevant_skill_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_branch_state_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_session_state_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_engram_state_block(ctx) {
        out.push_str(&s);
    }
    // Gap + contradiction alerts ride after the workspace/branch/
    // session context so the LLM has named the focus entity before
    // hearing what's wrong with it.
    if let Some(s) = render_gap_alerts_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_contradiction_alerts_block(ctx) {
        out.push_str(&s);
    }
    // Sub-agent digest rides AFTER the substrate state so the LLM
    // can correlate "Reconciler found X gaps" with the workspace it
    // just read about. Closes the observer→executive loop.
    if let Some(s) = render_sub_agent_digest_block(ctx) {
        out.push_str(&s);
    }
    // Previous-turn verifier critique rides late so it's the last
    // thing the model reads before the user's new question — biases
    // the self-correction toward THIS turn's planning.
    if let Some(s) = render_previous_verify_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_search_was_shallow_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_mcp_sessions_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_recovery_events_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_sandbox_alert_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_tool_budget_block(ctx) {
        out.push_str(&s);
    }
    out
}

/// `<substrate_freshness>` — informs the LLM that the compiled
/// substrate may be N edits behind disk. Fires only when
/// `fingerprint_match == false` (no signal = no block = no token
/// burn). Tells the agent what to do: prefer file_read for fresh
/// content, qualify claims that may have drifted, surface "I'd want
/// to recompile" when stakes are high.
fn render_substrate_freshness_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let f = ctx.substrate_freshness?;
    // Fresh substrate is the silent default — no reminder.
    if f.fingerprint_match {
        return None;
    }
    let mut inner = String::from("# substrate_freshness\n");
    inner.push_str("status: behind\n");
    match &f.last_compile_at_iso {
        Some(at) => inner.push_str(&format!("last_compiled_at: {at}\n")),
        None => inner.push_str("last_compiled_at: never (substrate is fresh-built or pre-CCC)\n"),
    }
    inner.push_str(&format!("files_on_disk: {}\n", f.file_count));
    inner.push_str(
        "The compiled knowledge graph does NOT yet reflect the user's most-recent edits. \
         Claims you retrieve may reference symbols / lines that have moved or no longer exist. \
         When precision matters: prefer `file_read` / `grep` on the live disk over `query_claims` / \
         `hybrid_retrieve`; qualify substrate-derived answers with \"the indexed snapshot says X — \
         your current file may differ\"; suggest `compile` when the user is asking a verification or \
         publish-class question. Don't claim something synced when it didn't.\n",
    );
    Some(wrap_reminder(&inner))
}

/// `<sub_agent_digest>` — last few `SubAgentReport`s from the
/// background scheduler (Reconciler / GapHunter / Curator /
/// Watcher). Closes the observer→executive loop that was the
/// canonical Letta / MemGPT pattern's missing wire: the executive
/// chat agent reads what sleep-time compute observed in the
/// background, so a turn that follows a fresh Reconciler tick
/// surfaces the patterns / gaps it found WITHOUT the user asking.
///
/// Suppressed when no non-empty reports exist. A scheduler running
/// idle (all reports empty summary, no observations) correctly
/// produces no reminder — silence is honest.
fn render_sub_agent_digest_block(ctx: &ReminderContext<'_>) -> Option<String> {
    if ctx.recent_sub_agent_reports.is_empty() {
        return None;
    }
    // Skip the block if every report is empty (background scheduler
    // ran but found nothing worth saying — don't burn tokens on
    // "all quiet").
    let any_useful = ctx
        .recent_sub_agent_reports
        .iter()
        .any(|r| !r.summary.is_empty() || !r.observations.is_empty());
    if !any_useful {
        return None;
    }
    let mut inner = String::from("# sub_agent_digest\n");
    inner.push_str(
        "Background observers (sleep-time compute) found these signals since your last turn. \
         Use them to surface what the user hasn't asked about yet, not as gospel — verify before \
         acting on a write.\n",
    );
    for r in ctx.recent_sub_agent_reports {
        if r.summary.is_empty() && r.observations.is_empty() {
            continue;
        }
        inner.push_str(&format!("- {} (at {}):", r.agent, r.finished_at_iso));
        if !r.summary.is_empty() {
            inner.push_str(&format!(" {}", r.summary));
        }
        inner.push('\n');
        for obs in r.observations.iter().take(3) {
            inner.push_str(&format!("    · {obs}\n"));
        }
    }
    Some(wrap_reminder(&inner))
}

/// `<previous_verify>` — critique from the last turn's verifier.
/// Reflexion-pattern self-correction: when the prior answer scored
/// weak grounding (citations didn't verify, low ratio, flagged
/// contradiction), the next turn reads this and adjusts BEFORE
/// re-running the same evidence-thin pattern.
fn render_previous_verify_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let c = ctx.previous_verify_critique?;
    // Skip benign verdicts — no useful signal for the next turn.
    let useful = matches!(
        c.verdict.as_str(),
        "low_grounding" | "ungrounded" | "contradiction"
    );
    if !useful {
        return None;
    }
    let mut inner = String::from("# previous_verify\n");
    inner.push_str(&format!("verdict: {}\n", c.verdict));
    if let (Some(v), Some(u)) = (c.citations_verified, c.citations_unverified) {
        inner.push_str(&format!("citations: {v} verified, {u} unverified\n"));
    }
    inner.push_str(&format!("reason: {}\n", c.reason));
    inner.push_str(
        "On this turn: cite from retrieved evidence only, not from training data; \
         when the substrate can't ground a needed claim, say so plainly rather than \
         repeating the previous answer's pattern.\n",
    );
    Some(wrap_reminder(&inner))
}

/// `<gap_alerts>` — open structural gaps surfaced in the focus area.
/// Lets the agent name them in the first response without paying a
/// `reflect` round-trip.
fn render_gap_alerts_block(ctx: &ReminderContext<'_>) -> Option<String> {
    if ctx.gap_alerts.is_empty() {
        return None;
    }
    let mut inner = String::from("# gap_alerts\n");
    inner.push_str("Open structural gaps near the session's focus:\n");
    for g in ctx.gap_alerts.iter().take(10) {
        inner.push_str(&format!("- {} on {}: {}\n", g.kind, g.subject, g.hint));
    }
    inner.push_str(
        "Cite gaps when relevant; offer to open a proposal or contribute the missing edge. \
         Don't fabricate the missing information — the gap exists precisely because the \
         substrate doesn't carry it yet.\n",
    );
    Some(wrap_reminder(&inner))
}

/// `<contradiction_alerts>` — branch-diff contradictions vs main.
/// World-class agents surface conflict explicitly; lesser ones pick
/// one side silently. Caller computes via `branch::diff::contradictions`.
fn render_contradiction_alerts_block(ctx: &ReminderContext<'_>) -> Option<String> {
    if ctx.contradiction_alerts.is_empty() {
        return None;
    }
    let mut inner = String::from("# contradiction_alerts\n");
    inner.push_str(
        "The active branch contradicts main on the entities below. Surface BOTH sides \
         when relevant — never pick one silently.\n",
    );
    for c in ctx.contradiction_alerts.iter().take(5) {
        inner.push_str(&format!("- {}: branch says \"{}\"; main says \"{}\"\n",
            c.subject, c.branch_says, c.main_says));
    }
    Some(wrap_reminder(&inner))
}

/// `<search_was_shallow>` — warns that the prior retrieval returned
/// thin evidence; re-running the same query is unlikely to help.
fn render_search_was_shallow_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let s = ctx.search_was_shallow?;
    let mut inner = String::from("# search_was_shallow\n");
    inner.push_str(&format!(
        "Previous retrieval for {:?} returned {} hit(s).\n",
        s.query, s.hits
    ));
    inner.push_str(
        "Don't re-run the same query — rephrase, drill via `probe_engram` on a related entity, \
         or admit the substrate doesn't carry the answer.\n",
    );
    Some(wrap_reminder(&inner))
}

/// `<environment>` — host context (cwd, OS, shell, $HOME, common
/// well-known directories, today's date). Suppressed when the caller
/// passes `environment: None` (LongMemEval bench harness, byte-
/// identity callers).
fn render_environment_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let env = ctx.environment?;
    let inner = render_env_inner(env);
    Some(wrap_reminder(&inner))
}

/// `<agentmemory_recall>` — top-K semantic-match recalls from prior
/// sessions, surfaced automatically (Mem0/Letta pattern). The AI sees
/// the most-relevant claims for the user's current question before
/// deciding whether to dig deeper via `search` / `hybrid_retrieve`.
fn render_agentmemory_recall_block(ctx: &ReminderContext<'_>) -> Option<String> {
    if ctx.agentmemory_recalls.is_empty() {
        return None;
    }
    let mut inner = String::from("# agentmemory_recall\n");
    inner.push_str(
        "Top relevant claims from this workspace's prior turns (auto-surfaced; cite by [claim:<id>] if you use them).\n",
    );
    for r in ctx.agentmemory_recalls {
        // Defensive truncation — caller should already have trimmed
        // but we cap at 480 chars to keep one outlier from inflating
        // the whole turn's reminder budget.
        let statement = if r.statement.len() > 480 {
            let mut cut = 477;
            while !r.statement.is_char_boundary(cut) && cut > 0 {
                cut -= 1;
            }
            format!("{}…", &r.statement[..cut])
        } else {
            r.statement.clone()
        };
        inner.push_str(&format!(
            "- [claim:{}] [{:.2} conf] {} ({})\n",
            r.claim_id, r.confidence, statement, r.source_uri,
        ));
    }
    Some(wrap_reminder(&inner))
}

/// `<relevant_skill>` — top-1 auto-classified skill body inlined for
/// the turn. Saves the `use_skill` round-trip on the common case
/// where keyword overlap is strong. Caller-classified, bus is a
/// pure renderer.
///
/// The skill body is wrapped under a `# skill: <name>` header so the
/// LLM sees the name + full instructions in one block. Caller may
/// trim the body to a budget; the bus passes through.
fn render_relevant_skill_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let skill = ctx.relevant_skill?;
    let mut inner = format!("# skill: {}\n", skill.name);
    inner.push_str(
        "This skill matches the user's request — apply its instructions before reaching for general tool patterns.\n\n",
    );
    inner.push_str(skill.body);
    if !skill.body.ends_with('\n') {
        inner.push('\n');
    }
    Some(wrap_reminder(&inner))
}

/// `<mcp_sessions>` — connected AI tools (other agents that have
/// opened MCP / agentmemory sessions against this daemon). Surfaced
/// when at least one session is active so the operator-mode AI has
/// cross-tool awareness without polling.
fn render_mcp_sessions_block(ctx: &ReminderContext<'_>) -> Option<String> {
    if ctx.mcp_sessions.is_empty() {
        return None;
    }
    let mut inner = String::from("# mcp_sessions\n");
    inner.push_str("Other AI tools currently plugged into this ThinkingRoot daemon:\n");
    for s in ctx.mcp_sessions {
        inner.push_str(&format!(
            "- {} ({}, transport={}, calls={}, errors={})\n",
            s.session_id_prefix, s.user_agent, s.transport, s.tool_calls_total, s.errors_total,
        ));
    }
    inner.push_str(
        "When the user reports a cross-tool problem, call `mcp_session_health` or `mcp_error_log` to drill in.\n",
    );
    Some(wrap_reminder(&inner))
}

/// `<substrate_health>` — recent self-heal events (compile failures,
/// breaker trips, stale-lock cleanups). Surfaced when non-empty so
/// the operator-mode AI proactively notices a wedged substrate.
fn render_recovery_events_block(ctx: &ReminderContext<'_>) -> Option<String> {
    if ctx.recovery_events.is_empty() {
        return None;
    }
    let mut inner = String::from("# substrate_health\n");
    inner.push_str("Recent self-heal events (last few minutes):\n");
    for ev in ctx.recovery_events {
        let ws = ev
            .workspace
            .as_deref()
            .map(|w| format!(" workspace={w}"))
            .unwrap_or_default();
        inner.push_str(&format!("- {} at {}{}: {}\n", ev.kind, ev.at_iso, ws, ev.summary));
    }
    inner.push_str(
        "Operator tools available: `recovery_log_tail`, `restart_state_get`, `reset_circuit_breaker`, `reset_compile_breaker`. Read before you act.\n",
    );
    Some(wrap_reminder(&inner))
}

/// `<workspace>` — wraps the existing `render_identity_block` output
/// so the same workspace identity (name, claim_count, today,
/// project_doc) flows through the new bus path with byte-identical
/// content to what `render_system_reminder` already emits. This is
/// the only emitter that overlaps with the legacy single-reminder
/// path — callers who use the bus should NOT also call
/// `render_system_reminder`.
fn render_workspace_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let identity = ctx.identity?;
    let inner = render_identity_block(identity, ctx.today);
    Some(wrap_reminder(&inner))
}

/// `<branch_state>` — fires when the session is on a non-default
/// branch. Suppressed on `main` because there's nothing useful to
/// say there ("you're on main" doesn't help the LLM choose anything).
fn render_branch_state_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let branch = ctx.branch.as_ref()?;
    if branch.name == "main" {
        return None;
    }
    let mut inner = String::from("# branch\n");
    inner.push_str(&format!("active: {}\n", branch.name));
    if let Some(parent) = &branch.parent {
        inner.push_str(&format!("parent: {parent}\n"));
    }
    if let Some(kind) = &branch.kind {
        inner.push_str(&format!("kind: {kind}\n"));
    }
    Some(wrap_reminder(&inner))
}

/// `<session_state>` — fires when the session has accumulated state
/// the LLM should be aware of: a focus entity, delivered claims, or
/// an active branch (which the branch reminder covers separately —
/// here we just count delivered claims so the LLM doesn't repeat
/// content).
fn render_session_state_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let session = ctx.session?;
    let has_focus = session.focus_entity.is_some();
    let delivered_count = session.delivered_claim_ids.len();
    if !has_focus && delivered_count == 0 {
        return None;
    }
    let mut inner = String::from("# session\n");
    if let Some(focus) = &session.focus_entity {
        inner.push_str(&format!("focus_entity: {focus}\n"));
    }
    if delivered_count > 0 {
        inner.push_str(&format!(
            "delivered_claim_count: {delivered_count}  (avoid repeating these)\n"
        ));
    }
    Some(wrap_reminder(&inner))
}

/// `<engram_state>` — fires when at least one engram pointer is
/// materialised for this session. Reports the pointer ids + their
/// topics so the LLM can probe targeted clusters via `probe_engram`
/// rather than re-issuing `materialize_engram`.
fn render_engram_state_block(ctx: &ReminderContext<'_>) -> Option<String> {
    if ctx.engrams.is_empty() {
        return None;
    }
    let mut inner = String::from("# engrams_active\n");
    for e in ctx.engrams {
        inner.push_str(&format!("- {}: {}\n", e.pointer, e.topic));
    }
    if ctx.engram_budget > 0 {
        inner.push_str(&format!(
            "budget: {} / {}\n",
            ctx.engrams.len(),
            ctx.engram_budget
        ));
    }
    Some(wrap_reminder(&inner))
}

/// `<sandbox_alert>` — fires when the
/// `intelligence/sandbox_classifier.rs` classifier recommends opening
/// an Ephemeral sandbox before any write. Suppressed on read-only
/// questions and on the LongMemEval bench harness (which doesn't
/// classify intents).
///
/// The block names the recommended action explicitly so the model can
/// choose: open a sandbox, contribute there, propose merging the
/// result back to main. Stays advisory — there's no enforcement gate
/// in v1.0.
fn render_sandbox_alert_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let reason = ctx.sandbox_recommendation?;
    if reason.is_empty() {
        return None;
    }
    let mut inner = String::from("# sandbox_alert\n");
    inner.push_str(&format!("recommendation: open sandbox before write ({reason}).\n"));
    inner.push_str(
        "Use create_branch with kind: Sandbox + merge_policy: Ephemeral, then contribute_claim there. The change stays reversible and the user can review before merging back.\n",
    );
    Some(wrap_reminder(&inner))
}

/// `<tool_budget>` — fires when the agent has fewer than
/// `TOOL_BUDGET_WARN_THRESHOLD` tool calls remaining for the turn.
/// Lets the LLM decide to wind down (synthesize an answer with what
/// it has) rather than be cut off by the iteration ceiling.
fn render_tool_budget_block(ctx: &ReminderContext<'_>) -> Option<String> {
    let remaining = ctx.tool_budget_remaining?;
    if remaining > TOOL_BUDGET_WARN_THRESHOLD {
        return None;
    }
    let mut inner = String::from("# tool_budget\n");
    if let Some(max) = ctx.tool_budget_max {
        inner.push_str(&format!("remaining: {remaining} / {max}\n"));
    } else {
        inner.push_str(&format!("remaining: {remaining}\n"));
    }
    inner.push_str(
        "Wind down on the next turn — synthesize the best answer from what you have.\n",
    );
    Some(wrap_reminder(&inner))
}

/// Wrap an inner `# section` body into a `<system-reminder>…</system-reminder>`
/// block with the canonical "ambient context, may or may not be
/// relevant" framing. Mirrors the format `render_system_reminder`
/// (synthesizer.rs:646) emits for the legacy single-reminder path —
/// keeps the LLM's pattern matching consistent across both paths.
fn wrap_reminder(inner: &str) -> String {
    format!(
        "<system-reminder>\nThe following context is ambient — use it when relevant, ignore it when it isn't.\n\n{inner}\n</system-reminder>\n\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::identity::WorkspaceIdentity;
    use crate::intelligence::session::SessionContext;
    use std::path::PathBuf;

    fn fixture_identity() -> WorkspaceIdentity {
        WorkspaceIdentity {
            name: "test-ws".to_string(),
            mounted_at: PathBuf::from("/tmp/test-ws"),
            claim_count: 1234,
            source_kinds: vec![("rs".to_string(), 42), ("md".to_string(), 7)],
            project_doc: None,
        }
    }

    fn fixture_session_with_focus() -> SessionContext {
        let mut s = SessionContext::new("sess-1", "test-ws");
        s.focus_entity = Some("WebhookHandler".to_string());
        s.mark_delivered(&["c1".to_string(), "c2".to_string(), "c3".to_string()]);
        s
    }

    fn fixture_engram(pointer: &str, topic: &str) -> EngramHandle {
        EngramHandle {
            pointer: pointer.to_string(),
            topic: topic.to_string(),
        }
    }

    #[test]
    fn empty_context_renders_empty_string() {
        let ctx = ReminderContext::default();
        assert_eq!(render_reactive_reminders(&ctx), "");
    }

    #[test]
    fn workspace_block_fires_when_identity_present() {
        let id = fixture_identity();
        let ctx = ReminderContext {
            identity: Some(&id),
            today: Some("2026-05-09"),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("<system-reminder>"));
        assert!(out.contains("name: test-ws"));
        assert!(out.contains("claims_indexed: 1234"));
        assert!(out.contains("# today\n2026-05-09"));
    }

    #[test]
    fn workspace_block_suppressed_when_identity_absent() {
        let ctx = ReminderContext::default();
        assert!(render_workspace_block(&ctx).is_none());
    }

    #[test]
    fn branch_state_suppressed_on_main() {
        let ctx = ReminderContext {
            branch: Some(BranchSummary {
                name: "main".to_string(),
                parent: None,
                kind: None,
            }),
            ..Default::default()
        };
        assert!(render_branch_state_block(&ctx).is_none());
    }

    #[test]
    fn branch_state_fires_on_non_main() {
        let ctx = ReminderContext {
            branch: Some(BranchSummary {
                name: "stream/chat-052".to_string(),
                parent: Some("main".to_string()),
                kind: Some("Stream".to_string()),
            }),
            ..Default::default()
        };
        let block = render_branch_state_block(&ctx).expect("branch reminder");
        assert!(block.contains("active: stream/chat-052"));
        assert!(block.contains("parent: main"));
        assert!(block.contains("kind: Stream"));
    }

    #[test]
    fn session_state_suppressed_when_no_focus_or_delivered() {
        let session = SessionContext::new("sess-1", "test-ws");
        let ctx = ReminderContext {
            session: Some(&session),
            ..Default::default()
        };
        assert!(render_session_state_block(&ctx).is_none());
    }

    #[test]
    fn session_state_fires_when_focus_entity_set() {
        let session = fixture_session_with_focus();
        let ctx = ReminderContext {
            session: Some(&session),
            ..Default::default()
        };
        let block = render_session_state_block(&ctx).expect("session reminder");
        assert!(block.contains("focus_entity: WebhookHandler"));
        assert!(block.contains("delivered_claim_count: 3"));
    }

    #[test]
    fn engram_state_suppressed_when_empty() {
        let ctx = ReminderContext::default();
        assert!(render_engram_state_block(&ctx).is_none());
    }

    #[test]
    fn engram_state_renders_pointers_and_budget() {
        let engrams = vec![
            fixture_engram("0x7A3F", "auth-system"),
            fixture_engram("0x9C12", "webhooks"),
        ];
        let ctx = ReminderContext {
            engrams: &engrams,
            engram_budget: 100,
            ..Default::default()
        };
        let block = render_engram_state_block(&ctx).expect("engram reminder");
        assert!(block.contains("0x7A3F: auth-system"));
        assert!(block.contains("0x9C12: webhooks"));
        assert!(block.contains("budget: 2 / 100"));
    }

    #[test]
    fn tool_budget_suppressed_when_above_threshold() {
        let ctx = ReminderContext {
            tool_budget_remaining: Some(8),
            tool_budget_max: Some(12),
            ..Default::default()
        };
        assert!(render_tool_budget_block(&ctx).is_none());
    }

    #[test]
    fn tool_budget_fires_at_or_below_threshold() {
        let ctx = ReminderContext {
            tool_budget_remaining: Some(2),
            tool_budget_max: Some(12),
            ..Default::default()
        };
        let block = render_tool_budget_block(&ctx).expect("budget reminder");
        assert!(block.contains("remaining: 2 / 12"));
        assert!(block.contains("Wind down"));
    }

    #[test]
    fn full_context_renders_blocks_in_stable_order() {
        // Every emitter active. Asserting both presence and order
        // because the prompt-cache hit relies on stable prefixes.
        let id = fixture_identity();
        let session = fixture_session_with_focus();
        let engrams = vec![fixture_engram("0x7A3F", "auth")];
        let ctx = ReminderContext {
            identity: Some(&id),
            today: Some("2026-05-09"),
            session: Some(&session),
            branch: Some(BranchSummary {
                name: "stream/chat-1".to_string(),
                parent: Some("main".to_string()),
                kind: Some("Stream".to_string()),
            }),
            engrams: &engrams,
            engram_budget: 100,
            tool_budget_remaining: Some(2),
            tool_budget_max: Some(12),
            sandbox_recommendation: None,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);

        // Order: workspace → branch_state → session_state → engram_state → tool_budget
        let i_ws = out.find("name: test-ws").expect("workspace block");
        let i_br = out.find("active: stream/chat-1").expect("branch block");
        let i_sn = out.find("focus_entity: WebhookHandler").expect("session block");
        let i_eg = out.find("0x7A3F: auth").expect("engram block");
        let i_tb = out.find("Wind down").expect("budget block");

        assert!(i_ws < i_br, "workspace must precede branch_state");
        assert!(i_br < i_sn, "branch_state must precede session_state");
        assert!(i_sn < i_eg, "session_state must precede engram_state");
        assert!(i_eg < i_tb, "engram_state must precede tool_budget");
    }

    #[test]
    fn environment_block_fires_first_when_present() {
        // Env precedes workspace in stable-order. Critical for the
        // "AI knows where Desktop is" contract: the LLM reads the
        // <environment> block before the workspace block and so can
        // resolve "Desktop" as `~/Desktop` without asking the user.
        let env = EnvironmentInfo {
            cwd: Some(std::path::PathBuf::from("/Users/test/proj")),
            home: Some(std::path::PathBuf::from("/Users/test")),
            desktop: Some(std::path::PathBuf::from("/Users/test/Desktop")),
            documents: None,
            downloads: None,
            os: "macos",
            shell: Some("zsh".to_string()),
            today_iso: "2026-05-18".to_string(),
        };
        let id = fixture_identity();
        let ctx = ReminderContext {
            environment: Some(&env),
            identity: Some(&id),
            today: Some("2026-05-18"),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        let i_env = out.find("# environment").expect("env block must render");
        let i_ws = out.find("name: test-ws").expect("workspace block must render");
        assert!(i_env < i_ws, "environment must precede workspace");
        assert!(out.contains("desktop: /Users/test/Desktop"));
        assert!(out.contains("os: macos"));
    }

    #[test]
    fn environment_block_suppressed_when_absent() {
        let ctx = ReminderContext::default();
        let out = render_reactive_reminders(&ctx);
        assert!(!out.contains("# environment"));
    }

    #[test]
    fn agentmemory_recall_block_fires_when_recalls_present() {
        let recalls = vec![
            AgentmemoryRecall {
                claim_id: "c-001".to_string(),
                statement: "user prefers Rust over Go".to_string(),
                confidence: 0.92,
                source_uri: "session://2026-05-10".to_string(),
            },
            AgentmemoryRecall {
                claim_id: "c-002".to_string(),
                statement: "user lives in Bangalore".to_string(),
                confidence: 0.99,
                source_uri: "session://2026-05-12".to_string(),
            },
        ];
        let ctx = ReminderContext {
            agentmemory_recalls: &recalls,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# agentmemory_recall"));
        assert!(out.contains("[claim:c-001]"));
        assert!(out.contains("[claim:c-002]"));
        assert!(out.contains("user prefers Rust over Go"));
        assert!(out.contains("session://2026-05-10"));
        assert!(out.contains("0.92 conf"));
    }

    #[test]
    fn agentmemory_recall_block_suppressed_when_empty() {
        let ctx = ReminderContext::default();
        let out = render_reactive_reminders(&ctx);
        assert!(!out.contains("agentmemory_recall"));
    }

    #[test]
    fn agentmemory_recall_caps_oversized_statement() {
        // Defensive: a single 1000-char recall mustn't blow the
        // turn's reminder budget. Bus truncates to 477+ellipsis.
        let mut statement = String::with_capacity(1000);
        for _ in 0..1000 {
            statement.push('x');
        }
        let recalls = vec![AgentmemoryRecall {
            claim_id: "c-big".to_string(),
            statement,
            confidence: 1.0,
            source_uri: "file://big".to_string(),
        }];
        let ctx = ReminderContext {
            agentmemory_recalls: &recalls,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("…"), "must include truncation marker");
        // Defensive cap: total reminder line length stays well under
        // 600 chars (the line including header + claim_id + conf).
        // The raw statement-rendering cap is 480 chars.
        assert!(out.matches('x').count() <= 480);
    }

    #[test]
    fn relevant_skill_block_inlines_skill_body() {
        let body = "# Refactor Rust\n\nStep 1: read CLAUDE.md\nStep 2: identify the smell\n";
        let ctx = ReminderContext {
            relevant_skill: Some(RelevantSkill {
                name: "refactor-rust",
                body,
            }),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# skill: refactor-rust"));
        assert!(out.contains("Step 1: read CLAUDE.md"));
        assert!(out.contains("Step 2: identify the smell"));
    }

    #[test]
    fn relevant_skill_block_suppressed_when_none() {
        let ctx = ReminderContext::default();
        let out = render_reactive_reminders(&ctx);
        assert!(!out.contains("# skill:"));
    }

    #[test]
    fn mcp_sessions_block_fires_when_sessions_present() {
        let sessions = vec![
            McpSessionBrief {
                session_id_prefix: "abc123def456".to_string(),
                user_agent: "cursor/1.5.2".to_string(),
                transport: "sse".to_string(),
                tool_calls_total: 23,
                errors_total: 0,
            },
            McpSessionBrief {
                session_id_prefix: "789012345678".to_string(),
                user_agent: "claude-code/0.4".to_string(),
                transport: "stdio".to_string(),
                tool_calls_total: 7,
                errors_total: 2,
            },
        ];
        let ctx = ReminderContext {
            mcp_sessions: &sessions,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# mcp_sessions"));
        assert!(out.contains("cursor/1.5.2"));
        assert!(out.contains("claude-code/0.4"));
        assert!(out.contains("calls=23"));
        assert!(out.contains("errors=2"));
    }

    #[test]
    fn mcp_sessions_block_suppressed_when_empty() {
        let ctx = ReminderContext::default();
        let out = render_reactive_reminders(&ctx);
        assert!(!out.contains("mcp_sessions"));
    }

    #[test]
    fn recovery_events_block_fires_when_events_present() {
        let events = vec![
            RecoveryEventBrief {
                kind: "compile_breaker_tripped".to_string(),
                workspace: Some("desktop".to_string()),
                at_iso: "2026-05-18T12:34:56Z".to_string(),
                summary: "3 consecutive compile failures in workspace 'desktop'".to_string(),
            },
            RecoveryEventBrief {
                kind: "stale_lock_cleanup".to_string(),
                workspace: None,
                at_iso: "2026-05-18T12:35:00Z".to_string(),
                summary: "removed cortex.lock owned by dead pid 4242".to_string(),
            },
        ];
        let ctx = ReminderContext {
            recovery_events: &events,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# substrate_health"));
        assert!(out.contains("compile_breaker_tripped"));
        assert!(out.contains("workspace=desktop"));
        assert!(out.contains("dead pid 4242"));
        assert!(out.contains("`reset_compile_breaker`"));
    }

    #[test]
    fn recovery_events_block_suppressed_when_empty() {
        let ctx = ReminderContext::default();
        let out = render_reactive_reminders(&ctx);
        assert!(!out.contains("substrate_health"));
    }

    #[test]
    fn full_v2_context_renders_all_eleven_blocks_in_stable_order() {
        // The complete SOTA loadout: environment → workspace →
        // agentmemory_recall → relevant_skill → branch_state →
        // session_state → engram_state → mcp_sessions →
        // substrate_health → sandbox_alert → tool_budget.
        let env = EnvironmentInfo {
            cwd: Some(std::path::PathBuf::from("/u/x")),
            home: Some(std::path::PathBuf::from("/u")),
            desktop: Some(std::path::PathBuf::from("/u/Desktop")),
            documents: None,
            downloads: None,
            os: "macos",
            shell: Some("zsh".to_string()),
            today_iso: "2026-05-18".to_string(),
        };
        let id = fixture_identity();
        let session = fixture_session_with_focus();
        let engrams = vec![fixture_engram("0x7A3F", "auth")];
        let recalls = vec![AgentmemoryRecall {
            claim_id: "c-001".to_string(),
            statement: "fact".to_string(),
            confidence: 0.9,
            source_uri: "file:///a".to_string(),
        }];
        let mcp = vec![McpSessionBrief {
            session_id_prefix: "abc123def456".to_string(),
            user_agent: "cursor/1.0".to_string(),
            transport: "sse".to_string(),
            tool_calls_total: 5,
            errors_total: 0,
        }];
        let recovery = vec![RecoveryEventBrief {
            kind: "stale_lock_cleanup".to_string(),
            workspace: None,
            at_iso: "2026-05-18T12:00:00Z".to_string(),
            summary: "cleaned up dead lock".to_string(),
        }];
        let body = "step 1\n";
        let ctx = ReminderContext {
            environment: Some(&env),
            identity: Some(&id),
            today: Some("2026-05-18"),
            session: Some(&session),
            branch: Some(BranchSummary {
                name: "stream/chat-1".to_string(),
                parent: Some("main".to_string()),
                kind: Some("Stream".to_string()),
            }),
            engrams: &engrams,
            engram_budget: 100,
            tool_budget_remaining: Some(2),
            tool_budget_max: Some(12),
            sandbox_recommendation: Some("refactor intent"),
            agentmemory_recalls: &recalls,
            mcp_sessions: &mcp,
            recovery_events: &recovery,
            relevant_skill: Some(RelevantSkill {
                name: "refactor-rust",
                body,
            }),
            // Ship 3 fields default to empty; covered by per-block tests below.
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);

        let positions = [
            ("# environment", out.find("# environment").expect("env")),
            ("# workspace", out.find("name: test-ws").expect("ws")),
            (
                "# agentmemory_recall",
                out.find("# agentmemory_recall").expect("recall"),
            ),
            ("# skill: refactor-rust", out.find("# skill: refactor-rust").expect("skill")),
            ("# branch", out.find("active: stream/chat-1").expect("branch")),
            ("# session", out.find("focus_entity: WebhookHandler").expect("session")),
            ("# engrams_active", out.find("# engrams_active").expect("engrams")),
            ("# mcp_sessions", out.find("# mcp_sessions").expect("mcp")),
            ("# substrate_health", out.find("# substrate_health").expect("health")),
            ("# sandbox_alert", out.find("# sandbox_alert").expect("sandbox")),
            ("# tool_budget", out.find("# tool_budget").expect("budget")),
        ];
        for i in 1..positions.len() {
            assert!(
                positions[i - 1].1 < positions[i].1,
                "{} must precede {}",
                positions[i - 1].0,
                positions[i].0,
            );
        }
    }

    #[test]
    fn each_block_is_independently_wrapped_in_system_reminder() {
        // The bus emits ONE reminder per emitter so the LLM can
        // selectively attend to whichever ones matter for the turn.
        let id = fixture_identity();
        let session = fixture_session_with_focus();
        let ctx = ReminderContext {
            identity: Some(&id),
            today: Some("2026-05-09"),
            session: Some(&session),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        // Two emitters fired (workspace + session) → two reminder
        // blocks. Counting open tags is sufficient because every
        // open is immediately closed by `wrap_reminder`.
        let opens = out.matches("<system-reminder>").count();
        let closes = out.matches("</system-reminder>").count();
        assert_eq!(opens, 2, "expected 2 open tags, got {opens}");
        assert_eq!(closes, 2, "expected 2 close tags, got {closes}");
    }

    #[test]
    fn wrapping_is_byte_stable_across_runs() {
        // Determinism check: same input → same output, byte for byte.
        // Critical for prompt caching to actually hit.
        let id = fixture_identity();
        let session = fixture_session_with_focus();
        let ctx1 = ReminderContext {
            identity: Some(&id),
            today: Some("2026-05-09"),
            session: Some(&session),
            ..Default::default()
        };
        let id2 = fixture_identity();
        let session2 = fixture_session_with_focus();
        let ctx2 = ReminderContext {
            identity: Some(&id2),
            today: Some("2026-05-09"),
            session: Some(&session2),
            ..Default::default()
        };
        assert_eq!(render_reactive_reminders(&ctx1), render_reactive_reminders(&ctx2));
    }

    #[test]
    fn sandbox_alert_fires_when_recommendation_present() {
        let ctx = ReminderContext {
            sandbox_recommendation: Some("refactor intent"),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# sandbox_alert"), "missing sandbox_alert section: {out}");
        assert!(out.contains("refactor intent"), "missing reason: {out}");
        assert!(out.contains("create_branch"), "missing tool guidance: {out}");
    }

    #[test]
    fn sandbox_alert_suppressed_when_recommendation_none() {
        let ctx = ReminderContext::default();
        let out = render_reactive_reminders(&ctx);
        assert!(!out.contains("sandbox_alert"));
    }

    #[test]
    fn sandbox_alert_suppressed_when_reason_is_empty_string() {
        // Defensive: an empty-string reason should not fire (signals
        // a caller bug; better to suppress than render a "()" block).
        let ctx = ReminderContext {
            sandbox_recommendation: Some(""),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(!out.contains("sandbox_alert"));
    }

    #[test]
    fn sandbox_alert_appears_after_engrams_before_tool_budget() {
        // Stable order matters for prompt caching. Sandbox sits
        // between engrams and tool_budget so a session with engrams
        // active + sandbox fired renders both in a predictable
        // sequence.
        let id = fixture_identity();
        let engrams = vec![fixture_engram("0xa1b2", "auth")];
        let ctx = ReminderContext {
            identity: Some(&id),
            today: Some("2026-05-09"),
            engrams: &engrams,
            engram_budget: 100,
            sandbox_recommendation: Some("refactor intent"),
            tool_budget_remaining: Some(2),
            tool_budget_max: Some(12),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        let engram_pos = out.find("# engrams_active").expect("engram block missing");
        let sandbox_pos = out.find("# sandbox_alert").expect("sandbox block missing");
        let budget_pos = out.find("# tool_budget").expect("tool_budget block missing");
        assert!(engram_pos < sandbox_pos, "engram should precede sandbox");
        assert!(sandbox_pos < budget_pos, "sandbox should precede tool_budget");
    }

    // ── Ship 3 — substrate freshness ─────────────────────────────

    #[test]
    fn substrate_freshness_silent_when_fresh() {
        let fresh = SubstrateFreshness {
            fingerprint_match: true,
            last_compile_at_iso: Some("2026-05-20T10:00:00Z".to_string()),
            file_count: 42,
        };
        let ctx = ReminderContext {
            substrate_freshness: Some(&fresh),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(
            !out.contains("substrate_freshness"),
            "fresh substrate must NOT render a reminder (no signal = no token burn)"
        );
    }

    #[test]
    fn substrate_freshness_fires_when_behind() {
        let stale = SubstrateFreshness {
            fingerprint_match: false,
            last_compile_at_iso: Some("2026-05-20T09:00:00Z".to_string()),
            file_count: 42,
        };
        let ctx = ReminderContext {
            substrate_freshness: Some(&stale),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# substrate_freshness"));
        assert!(out.contains("status: behind"));
        assert!(out.contains("last_compiled_at: 2026-05-20T09:00:00Z"));
        assert!(
            out.contains("prefer `file_read`"),
            "must direct LLM to fresh-disk reads"
        );
    }

    #[test]
    fn substrate_freshness_handles_never_compiled() {
        let stale = SubstrateFreshness {
            fingerprint_match: false,
            last_compile_at_iso: None,
            file_count: 0,
        };
        let ctx = ReminderContext {
            substrate_freshness: Some(&stale),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("last_compiled_at: never"));
    }

    // ── Ship 3 — sub-agent digest ────────────────────────────────

    #[test]
    fn sub_agent_digest_suppressed_when_empty() {
        let ctx = ReminderContext::default();
        let out = render_reactive_reminders(&ctx);
        assert!(!out.contains("sub_agent_digest"));
    }

    #[test]
    fn sub_agent_digest_suppressed_when_all_reports_empty() {
        let reports = vec![
            SubAgentReportBrief {
                agent: "reconciler".to_string(),
                finished_at_iso: "2026-05-20T10:00:00Z".to_string(),
                summary: String::new(),
                observations: Vec::new(),
            },
            SubAgentReportBrief {
                agent: "gap_hunter".to_string(),
                finished_at_iso: "2026-05-20T10:01:00Z".to_string(),
                summary: String::new(),
                observations: Vec::new(),
            },
        ];
        let ctx = ReminderContext {
            recent_sub_agent_reports: &reports,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(
            !out.contains("sub_agent_digest"),
            "all-quiet ticks must not burn tokens"
        );
    }

    #[test]
    fn sub_agent_digest_renders_summary_and_observations() {
        let reports = vec![
            SubAgentReportBrief {
                agent: "reconciler".to_string(),
                finished_at_iso: "2026-05-20T10:00:00Z".to_string(),
                summary: "12 open gaps (+3 new this tick)".to_string(),
                observations: vec![
                    "pattern: WebhookHandler expects validation, frequency=0.87".to_string(),
                    "pattern: AuthMiddleware expects timeout, frequency=0.65".to_string(),
                ],
            },
            SubAgentReportBrief {
                agent: "gap_hunter".to_string(),
                finished_at_iso: "2026-05-20T10:01:00Z".to_string(),
                summary: String::new(),
                observations: Vec::new(),
            },
        ];
        let ctx = ReminderContext {
            recent_sub_agent_reports: &reports,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# sub_agent_digest"));
        assert!(out.contains("reconciler"));
        assert!(out.contains("12 open gaps"));
        assert!(out.contains("WebhookHandler expects validation"));
        // Empty gap_hunter report must be skipped (caller passes
        // empty when nothing happened; the bus suppresses).
        assert!(
            !out.contains("gap_hunter"),
            "empty reports must not render placeholder lines"
        );
    }

    // ── Ship 3 — previous-verify (Reflexion) ─────────────────────

    #[test]
    fn previous_verify_suppressed_for_benign_verdicts() {
        for verdict in ["high_grounding", "chitchat", "skipped_rejection", "skipped_bench"] {
            let critique = PreviousVerifyCritique {
                verdict: verdict.to_string(),
                citations_verified: Some(3),
                citations_unverified: Some(0),
                reason: "ok".to_string(),
            };
            let ctx = ReminderContext {
                previous_verify_critique: Some(&critique),
                ..Default::default()
            };
            let out = render_reactive_reminders(&ctx);
            assert!(
                !out.contains("previous_verify"),
                "{verdict} should not surface critique reminder"
            );
        }
    }

    #[test]
    fn previous_verify_fires_on_weak_grounding() {
        let critique = PreviousVerifyCritique {
            verdict: "low_grounding".to_string(),
            citations_verified: Some(1),
            citations_unverified: Some(4),
            reason: "4 of 5 [claim:…] markers did not resolve in the substrate".to_string(),
        };
        let ctx = ReminderContext {
            previous_verify_critique: Some(&critique),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# previous_verify"));
        assert!(out.contains("verdict: low_grounding"));
        assert!(out.contains("1 verified, 4 unverified"));
        assert!(out.contains("retrieved evidence only"));
    }

    // ── Ship 3 — gap + contradiction alerts ──────────────────────

    #[test]
    fn gap_alerts_render_when_present() {
        let gaps = vec![
            GapAlert {
                kind: "missing_origin".to_string(),
                subject: "WebhookHandler".to_string(),
                hint: "no source links into WebhookHandler".to_string(),
            },
            GapAlert {
                kind: "untyped_relation".to_string(),
                subject: "AuthMiddleware".to_string(),
                hint: "edge to TokenStore lacks a verb".to_string(),
            },
        ];
        let ctx = ReminderContext {
            gap_alerts: &gaps,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# gap_alerts"));
        assert!(out.contains("missing_origin on WebhookHandler"));
        assert!(out.contains("untyped_relation on AuthMiddleware"));
    }

    #[test]
    fn contradiction_alerts_render_both_sides() {
        let contras = vec![ContradictionAlert {
            subject: "WebhookHandler.timeout".to_string(),
            branch_says: "30s".to_string(),
            main_says: "10s".to_string(),
        }];
        let ctx = ReminderContext {
            contradiction_alerts: &contras,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# contradiction_alerts"));
        assert!(out.contains("branch says \"30s\""));
        assert!(out.contains("main says \"10s\""));
    }

    #[test]
    fn search_was_shallow_fires_when_set() {
        let s = SearchWasShallow {
            query: "AuthMiddleware retries".to_string(),
            hits: 1,
        };
        let ctx = ReminderContext {
            search_was_shallow: Some(&s),
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        assert!(out.contains("# search_was_shallow"));
        assert!(out.contains("AuthMiddleware retries"));
        assert!(out.contains("1 hit"));
        assert!(out.contains("rephrase"));
    }

    // ── Ship 3 — stable order including new blocks ───────────────

    #[test]
    fn ship3_blocks_render_in_stable_order() {
        let id = fixture_identity();
        let stale = SubstrateFreshness {
            fingerprint_match: false,
            last_compile_at_iso: Some("2026-05-20T09:00:00Z".to_string()),
            file_count: 42,
        };
        let reports = vec![SubAgentReportBrief {
            agent: "reconciler".to_string(),
            finished_at_iso: "2026-05-20T10:00:00Z".to_string(),
            summary: "3 gaps".to_string(),
            observations: vec![],
        }];
        let critique = PreviousVerifyCritique {
            verdict: "low_grounding".to_string(),
            citations_verified: Some(1),
            citations_unverified: Some(2),
            reason: "thin".to_string(),
        };
        let gaps = vec![GapAlert {
            kind: "missing_origin".to_string(),
            subject: "X".to_string(),
            hint: "y".to_string(),
        }];
        let ctx = ReminderContext {
            identity: Some(&id),
            today: Some("2026-05-20"),
            substrate_freshness: Some(&stale),
            recent_sub_agent_reports: &reports,
            previous_verify_critique: Some(&critique),
            gap_alerts: &gaps,
            ..Default::default()
        };
        let out = render_reactive_reminders(&ctx);
        let p_ws = out.find("name: test-ws").expect("workspace");
        let p_fr = out.find("# substrate_freshness").expect("freshness");
        let p_gp = out.find("# gap_alerts").expect("gaps");
        let p_sd = out.find("# sub_agent_digest").expect("digest");
        let p_pv = out.find("# previous_verify").expect("verify");

        assert!(p_ws < p_fr, "workspace must precede substrate_freshness");
        assert!(p_fr < p_gp, "substrate_freshness must precede gap_alerts");
        assert!(p_gp < p_sd, "gap_alerts must precede sub_agent_digest");
        assert!(p_sd < p_pv, "sub_agent_digest must precede previous_verify");
    }
}
