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
// Deferred to v1.1 (require expensive per-turn substrate queries
// — see plan 2026-05-09 for the wiring requirements):
//   * `contradiction_alert` (needs branch::diff::contradictions
//     against the active focus entity)
//   * `gap_alert` (needs reflect::list_open_gaps filtered to focus)
//   * `search_was_shallow` (needs hook into hybrid_retrieve to
//     track last-call result count)
//
// (Task 9 / Day 2-4 P1, plan 2026-05-09.)

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

/// Snapshot of every input the bus draws on for one turn. All fields
/// are optional — the corresponding emitter is suppressed when its
/// data source is missing, which is the right behaviour for callers
/// like the LongMemEval bench harness that intentionally pass none.
#[derive(Debug, Clone, Default)]
pub struct ReminderContext<'a> {
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
    if let Some(s) = render_workspace_block(ctx) {
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
    if let Some(s) = render_sandbox_alert_block(ctx) {
        out.push_str(&s);
    }
    if let Some(s) = render_tool_budget_block(ctx) {
        out.push_str(&s);
    }
    out
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
}
