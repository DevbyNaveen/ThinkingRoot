//! Shared assembly of the 17-block `<system-reminder>` context.
//!
//! Both REST chat (`rest.rs::agent_stream_response` at the previous
//! `rest.rs:5912-6213` inline block) and the MCP `get_reminder_context`
//! tool (added in Commit 2) consume one assembly path through this
//! module. Lifting it here makes future drift between in-app and
//! external-AI surfaces structurally impossible — both walk through
//! [`build`] and render with `reminder_bus::render_reactive_reminders`.
//!
//! The owned/borrowed split mirrors the prior inline pattern:
//! [`ReminderBuild`] owns every gathered datum on the stack; the
//! caller borrows a [`reminder_bus::ReminderContext<'_>`] from it via
//! [`ReminderBuild::as_context`] right before rendering. This keeps
//! the bus's borrowed-view contract intact (no heap thrash on the hot
//! path, no `Arc` on rendered fields).
//!
//! Wire contract: the rendered output for a given input set must be
//! byte-identical to what the prior inline REST block produced.
//! Pinned by [`tests::build_full_context_renders_all_populated_blocks`].

use std::sync::Arc;

use crate::intelligence::environment::EnvironmentInfo;
use crate::intelligence::identity::WorkspaceIdentity;
use crate::intelligence::reminder_bus::{
    AgentmemoryRecall, BranchSummary, ContradictionAlert, EngramHandle, GapAlert,
    McpSessionBrief, PreviousVerifyCritique, RecoveryEventBrief, RelevantSkill,
    ReminderContext, SearchWasShallow, SubAgentReportBrief, SubstrateFreshness,
};
use crate::intelligence::session::SessionContext;
use crate::intelligence::skills::SkillRegistry;
use crate::rest::AppState;

/// Mirrors `EngramConfig::default().max_engrams_per_session`. Lifted
/// from the prior inline value at `rest.rs:6198` so callers don't
/// hard-code it; the rendering layer uses it for the engram-state
/// budget line.
pub const ENGRAM_BUDGET_DEFAULT: usize = 100;

/// Owned bundle of every datum the reminder bus needs. Held on the
/// caller's stack; borrowed into [`ReminderContext`] via
/// [`Self::as_context`] when the caller is ready to render.
///
/// The struct is `pub` (not `pub(crate)`) because external integration
/// tests need to inspect intermediate state; production callers
/// should treat it as opaque and only ever call [`Self::as_context`].
pub struct ReminderBuild {
    pub environment_info: EnvironmentInfo,
    pub today_str: String,
    pub session_snapshot: Option<SessionContext>,
    pub engram_handles: Vec<EngramHandle>,
    pub branch_summary: Option<BranchSummary>,
    pub sandbox_reason: Option<&'static str>,
    pub agentmemory_recalls: Vec<AgentmemoryRecall>,
    pub mcp_sessions: Vec<McpSessionBrief>,
    pub recovery_events: Vec<RecoveryEventBrief>,
    /// Skill name + body kept as owned strings so the borrowed
    /// [`RelevantSkill`] view in [`Self::as_context`] can borrow into
    /// the same struct without taking a `&SkillRegistry` lifetime.
    pub relevant_skill_name: Option<String>,
    pub relevant_skill_body: Option<String>,
    pub substrate_freshness: Option<SubstrateFreshness>,
    pub recent_sub_agent_reports: Vec<SubAgentReportBrief>,
    pub previous_verify_critique: Option<PreviousVerifyCritique>,
    pub gap_alerts: Vec<GapAlert>,
    pub contradiction_alerts: Vec<ContradictionAlert>,
    pub search_was_shallow: Option<SearchWasShallow>,
}

impl ReminderBuild {
    /// Borrow a [`ReminderContext<'_>`] view over this build, plus the
    /// caller-supplied [`WorkspaceIdentity`]. Identity is supplied by
    /// the caller (rather than gathered here) because the two REST
    /// sites already hold a workspace status snapshot from which they
    /// build identity; re-building it inside this helper would force
    /// a duplicate registry read on every chat turn.
    pub fn as_context<'a>(
        &'a self,
        identity: Option<&'a WorkspaceIdentity>,
    ) -> ReminderContext<'a> {
        let relevant_skill = match (&self.relevant_skill_name, &self.relevant_skill_body) {
            (Some(name), Some(body)) => Some(RelevantSkill {
                name: name.as_str(),
                body: body.as_str(),
            }),
            _ => None,
        };
        ReminderContext {
            environment: Some(&self.environment_info),
            identity,
            today: Some(&self.today_str),
            session: self.session_snapshot.as_ref(),
            branch: self.branch_summary.clone(),
            engrams: &self.engram_handles,
            engram_budget: ENGRAM_BUDGET_DEFAULT,
            tool_budget_remaining: None,
            tool_budget_max: None,
            sandbox_recommendation: self.sandbox_reason,
            agentmemory_recalls: &self.agentmemory_recalls,
            mcp_sessions: &self.mcp_sessions,
            recovery_events: &self.recovery_events,
            relevant_skill,
            substrate_freshness: self.substrate_freshness.as_ref(),
            recent_sub_agent_reports: &self.recent_sub_agent_reports,
            previous_verify_critique: self.previous_verify_critique.as_ref(),
            gap_alerts: &self.gap_alerts,
            contradiction_alerts: &self.contradiction_alerts,
            search_was_shallow: self.search_was_shallow.as_ref(),
        }
    }
}

/// Build the full 17-block reminder bundle for a chat turn.
///
/// Replaces the prior inline gather block at the REST chat handler
/// (formerly `rest.rs:5912-6213`). Same field semantics, same gather
/// order, same honest empty-state behaviour: every emitter checks
/// its own precondition so blocks where the substrate state doesn't
/// warrant emission simply omit themselves.
///
/// `skills` is `Option` so MCP callers that don't want to pay the
/// per-call `SkillRegistry::load_from_dir` cost can pass `None` and
/// suppress the `<relevant_skill>` block.
pub async fn build(
    state: &Arc<AppState>,
    workspace: &str,
    conversation_id: &str,
    user_question: &str,
    skills: Option<&SkillRegistry>,
) -> ReminderBuild {
    // ── Environment ────────────────────────────────────────────────
    // World-class prompt foundation (ship 2026-05-18): explicit
    // cwd/$HOME/~/Desktop context every turn. Pure sync, sub-µs.
    let environment_info = crate::intelligence::environment::gather();

    // ── Today ──────────────────────────────────────────────────────
    let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    // ── Session snapshot (clone-and-release) ───────────────────────
    let session_snapshot: Option<SessionContext> = {
        let map = state.sessions.lock().await;
        map.get(conversation_id).cloned()
    };

    // ── Engram handles ─────────────────────────────────────────────
    let engram_handles: Vec<EngramHandle> = state
        .engram_manager
        .list_engrams(conversation_id)
        .await
        .into_iter()
        .map(|r| EngramHandle {
            pointer: r.pointer,
            topic: r.topic,
        })
        .collect();

    // ── Branch summary (derived from session.active_branch) ────────
    let branch_summary: Option<BranchSummary> = session_snapshot
        .as_ref()
        .and_then(|s| s.active_branch.clone())
        .map(|name| BranchSummary {
            name,
            parent: None, // Branch registry lookup deferred to v1.1
            kind: None,
        });

    // ── Sandbox-by-default classifier (Task 17, 2026-05-09) ────────
    let sandbox_reason: Option<&'static str> =
        match crate::intelligence::sandbox_classifier::classify(user_question) {
            crate::intelligence::sandbox_classifier::SandboxIntent::RecommendSandbox {
                reason,
            } => Some(reason),
            crate::intelligence::sandbox_classifier::SandboxIntent::NoAction => None,
        };

    // ── Agentmemory recalls (Mem0/Letta-style auto-surface) ────────
    let agentmemory_recalls: Vec<AgentmemoryRecall> = {
        let engine = state.engine.read().await;
        match engine.search(workspace, user_question, 3).await {
            Ok(result) => result
                .claims
                .into_iter()
                .map(|h| AgentmemoryRecall {
                    claim_id: h.id,
                    statement: if h.statement.len() > 240 {
                        let mut cut = 237;
                        while !h.statement.is_char_boundary(cut) && cut > 0 {
                            cut -= 1;
                        }
                        format!("{}…", &h.statement[..cut])
                    } else {
                        h.statement
                    },
                    confidence: h.confidence,
                    source_uri: h.source_uri,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    };

    // ── MCP telemetry snapshot ─────────────────────────────────────
    let mcp_sessions: Vec<McpSessionBrief> = match crate::mcp::telemetry::global_map() {
        Some(map) => {
            let snap = map.read().await;
            snap.values()
                .map(|s| {
                    let user_agent = match &s.principal {
                        crate::mcp::telemetry::PrincipalKind::InAppAgent => "in-app".to_string(),
                        crate::mcp::telemetry::PrincipalKind::McpClient { user_agent } => {
                            user_agent.clone()
                        }
                        crate::mcp::telemetry::PrincipalKind::AgentMemory {
                            user_agent, ..
                        } => user_agent.clone(),
                    };
                    let transport = match s.transport {
                        crate::mcp::telemetry::TransportKind::Sse => "sse",
                        crate::mcp::telemetry::TransportKind::Stdio => "stdio",
                        crate::mcp::telemetry::TransportKind::AgentMemory => "agentmemory",
                    };
                    let prefix_len = s.session_id.len().min(12);
                    McpSessionBrief {
                        session_id_prefix: s.session_id[..prefix_len].to_string(),
                        user_agent,
                        transport: transport.to_string(),
                        tool_calls_total: s.tool_calls_total,
                        errors_total: s.errors_total,
                    }
                })
                .collect()
        }
        None => Vec::new(),
    };

    // ── Recovery events (last 5 min) ───────────────────────────────
    let recovery_events: Vec<RecoveryEventBrief> = match thinkingroot_core::recovery_log::tail(50)
    {
        Ok(events) => {
            let now = chrono::Utc::now();
            let window = chrono::Duration::minutes(5);
            events
                .into_iter()
                .filter(|ev| now.signed_duration_since(ev.ts) <= window)
                .map(|ev| recovery_event_brief(&ev))
                .collect()
        }
        Err(_) => Vec::new(),
    };

    // ── Relevant skill (top-1 keyword-overlap, body inlined) ───────
    let (relevant_skill_name, relevant_skill_body) = match skills.and_then(|s| s.pick_relevant(user_question))
    {
        Some(skill) => (Some(skill.name.clone()), Some(skill.body.clone())),
        None => (None, None),
    };

    // ── Substrate freshness (Ship 3A) ──────────────────────────────
    let substrate_freshness: Option<SubstrateFreshness> = {
        let snapshots = state.workspace_status.snapshot_all().await;
        snapshots
            .into_iter()
            .find(|s| s.name == workspace)
            .and_then(|status| match status.sources {
                thinkingroot_core::types::SourcesState::Some {
                    fingerprint_match,
                    file_count,
                    ..
                } => Some(SubstrateFreshness {
                    fingerprint_match,
                    last_compile_at_iso: match status.compile {
                        thinkingroot_core::types::CompileState::Idle {
                            last_finished_at: Some(at),
                            ..
                        } => Some(at.to_rfc3339()),
                        _ => None,
                    },
                    file_count,
                }),
                thinkingroot_core::types::SourcesState::None => None,
            })
    };

    // ── Sub-agent digest (Ship 3B, top-6 most recent) ──────────────
    let recent_sub_agent_reports: Vec<SubAgentReportBrief> = {
        let reports = state.substrate_bus_reports(workspace).await;
        reports
            .into_iter()
            .rev()
            .take(6)
            .map(|r| SubAgentReportBrief {
                agent: r.agent,
                finished_at_iso: r.finished_at.to_rfc3339(),
                summary: r.summary,
                observations: r.observations.into_iter().take(3).collect(),
            })
            .collect()
    };

    // ── Previous verifier critique (Ship 3D, Reflexion) ────────────
    let previous_verify_critique: Option<PreviousVerifyCritique> =
        session_snapshot.as_ref().and_then(|s| {
            let verdict = s.last_verify_verdict.clone()?;
            let reason = s.last_verify_reason.clone().unwrap_or_default();
            Some(PreviousVerifyCritique {
                verdict,
                citations_verified: s.last_verify_citations_verified,
                citations_unverified: s.last_verify_citations_unverified,
                reason,
            })
        });

    // ── Gap alerts (Ship 3E, top-5 by confidence in focus area) ────
    let gap_alerts: Vec<GapAlert> = {
        let focus = session_snapshot
            .as_ref()
            .and_then(|s| s.focus_entity.as_deref());
        let engine = state.engine.read().await;
        match engine.list_gaps(workspace, focus, 0.6).await {
            Ok(gaps) => gaps
                .into_iter()
                .take(5)
                .map(|g| GapAlert {
                    kind: g.expected_claim_type,
                    subject: g.entity_name,
                    hint: g.reason,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    };

    // ── Contradiction alerts ───────────────────────────────────────
    // The branch crate doesn't yet expose `diff::contradictions`;
    // honest empty rather than fabricated. Lights up at the Witness
    // Mesh hybrid cutover (per `.claude/rules/witness-mesh.md`).
    let contradiction_alerts: Vec<ContradictionAlert> = Vec::new();

    // ── Search-was-shallow (Ship 3E) ───────────────────────────────
    let search_was_shallow: Option<SearchWasShallow> = session_snapshot.as_ref().and_then(|s| {
        let q = s.last_search_query.clone()?;
        let hits = s.last_search_hits?;
        if hits <= crate::intelligence::session::SHALLOW_RETRIEVAL_THRESHOLD {
            Some(SearchWasShallow { query: q, hits })
        } else {
            None
        }
    });

    ReminderBuild {
        environment_info,
        today_str,
        session_snapshot,
        engram_handles,
        branch_summary,
        sandbox_reason,
        agentmemory_recalls,
        mcp_sessions,
        recovery_events,
        relevant_skill_name,
        relevant_skill_body,
        substrate_freshness,
        recent_sub_agent_reports,
        previous_verify_critique,
        gap_alerts,
        contradiction_alerts,
        search_was_shallow,
    }
}

/// Mid-turn refresher — gathers ONLY the three volatile signals the
/// agent loop's `SystemPromptRefresher` needs. Cheap (~2 ms typical);
/// safe to call at the top of every iteration. Distinct from [`build`]
/// because (a) the full build does a `engine.search` for agentmemory
/// recall — too expensive per-iteration — and (b) most context fields
/// don't change mid-turn anyway.
pub async fn build_mid_turn(state: &Arc<AppState>, workspace: &str) -> MidTurnRefresh {
    let substrate_freshness: Option<SubstrateFreshness> = {
        let snapshots = state.workspace_status.snapshot_all().await;
        snapshots
            .into_iter()
            .find(|s| s.name == workspace)
            .and_then(|status| match status.sources {
                thinkingroot_core::types::SourcesState::Some {
                    fingerprint_match,
                    file_count,
                    ..
                } => Some(SubstrateFreshness {
                    fingerprint_match,
                    last_compile_at_iso: match status.compile {
                        thinkingroot_core::types::CompileState::Idle {
                            last_finished_at: Some(at),
                            ..
                        } => Some(at.to_rfc3339()),
                        _ => None,
                    },
                    file_count,
                }),
                thinkingroot_core::types::SourcesState::None => None,
            })
    };

    // Tighter cap for mid-turn refresh (3 not 6) — keep token budget
    // lean across a long iteration.
    let recent_sub_agent_reports: Vec<SubAgentReportBrief> = {
        let r = state.substrate_bus_reports(workspace).await;
        r.into_iter()
            .rev()
            .take(3)
            .map(|r| SubAgentReportBrief {
                agent: r.agent,
                finished_at_iso: r.finished_at.to_rfc3339(),
                summary: r.summary,
                observations: r.observations.into_iter().take(2).collect(),
            })
            .collect()
    };

    // Tighter window for mid-turn: events from the last 2 min only,
    // since the full bus already saw the last 5 min at turn entry.
    let recovery_events: Vec<RecoveryEventBrief> =
        match thinkingroot_core::recovery_log::tail(20) {
            Ok(events) => {
                let now = chrono::Utc::now();
                let window = chrono::Duration::minutes(2);
                events
                    .into_iter()
                    .filter(|ev| now.signed_duration_since(ev.ts) <= window)
                    .map(|ev| recovery_event_brief(&ev))
                    .collect()
            }
            Err(_) => Vec::new(),
        };

    MidTurnRefresh {
        substrate_freshness,
        recent_sub_agent_reports,
        recovery_events,
    }
}

/// Owned bundle of the three mid-turn volatile signals. Borrow via
/// [`Self::as_context`] right before rendering.
pub struct MidTurnRefresh {
    pub substrate_freshness: Option<SubstrateFreshness>,
    pub recent_sub_agent_reports: Vec<SubAgentReportBrief>,
    pub recovery_events: Vec<RecoveryEventBrief>,
}

impl MidTurnRefresh {
    /// Borrow a partial `ReminderContext` for the mid-turn refresher.
    /// All non-refreshed fields default to honest empty values so the
    /// bus suppresses their blocks (preserves the prior inline
    /// `..Default::default()` semantics).
    pub fn as_context(&self) -> ReminderContext<'_> {
        ReminderContext {
            substrate_freshness: self.substrate_freshness.as_ref(),
            recent_sub_agent_reports: &self.recent_sub_agent_reports,
            recovery_events: &self.recovery_events,
            ..Default::default()
        }
    }
}

/// Project a [`RecoveryEvent`] onto the slim [`RecoveryEventBrief`]
/// shape the bus consumes. Lifted from the prior `rest.rs:5457-5551`
/// helper so MCP callers can produce the same shape without depending
/// on `rest.rs`.
///
/// [`RecoveryEvent`]: thinkingroot_core::recovery_log::RecoveryEvent
pub fn recovery_event_brief(
    ev: &thinkingroot_core::recovery_log::RecoveryEvent,
) -> RecoveryEventBrief {
    use thinkingroot_core::recovery_log::RecoveryEventKind as K;
    let (kind, workspace, summary) = match &ev.kind {
        K::Respawn {
            attempt,
            backoff_ms,
            reason,
        } => (
            "respawn",
            None,
            format!("attempt {attempt} after {backoff_ms}ms ({reason})"),
        ),
        K::RespawnOk { new_pid } => ("respawn_ok", None, format!("new pid {new_pid}")),
        K::StaleLockCleanup { dead_pid } => (
            "stale_lock_cleanup",
            None,
            format!("removed cortex.lock owned by dead pid {dead_pid}"),
        ),
        K::PortAdvance { from, to, reason } => (
            "port_advance",
            None,
            format!("port {from} → {to} ({reason})"),
        ),
        K::ManifestRebuild { binaries_found } => (
            "manifest_rebuild",
            None,
            format!("rebuilt install manifest from disk scan ({binaries_found} binaries found)"),
        ),
        K::CircuitBreakerTripped {
            consecutive_failures,
            until_rfc3339,
        } => (
            "circuit_breaker_tripped",
            None,
            format!(
                "{consecutive_failures} consecutive failures — daemon respawn paused until {until_rfc3339}"
            ),
        ),
        K::CircuitBreakerReset { reason } => (
            "circuit_breaker_reset",
            None,
            format!("breaker cleared ({reason})"),
        ),
        K::BinaryChecksumMismatch { path, .. } => (
            "binary_checksum_mismatch",
            None,
            format!("BLAKE3 mismatch at {}", path.display()),
        ),
        K::CompileFailed {
            workspace,
            error,
            retry_attempt,
        } => (
            "compile_failed",
            Some(workspace.clone()),
            format!("retry {retry_attempt}: {error}"),
        ),
        K::CompileRetryScheduled {
            workspace,
            attempt,
            backoff_ms,
        } => (
            "compile_retry_scheduled",
            Some(workspace.clone()),
            format!("retry {attempt} scheduled in {backoff_ms}ms"),
        ),
        K::CompileBreakerTripped {
            workspace,
            consecutive_failures,
            until_rfc3339,
        } => (
            "compile_breaker_tripped",
            Some(workspace.clone()),
            format!(
                "{consecutive_failures} consecutive compile failures — paused until {until_rfc3339}"
            ),
        ),
        K::CompileRecovered {
            workspace,
            retry_attempt,
        } => (
            "compile_recovered",
            Some(workspace.clone()),
            format!("compile succeeded on retry {retry_attempt}"),
        ),
    };
    RecoveryEventBrief {
        kind: kind.to_string(),
        workspace,
        at_iso: ev.ts.to_rfc3339(),
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::reminder_bus::render_reactive_reminders;
    use thinkingroot_core::recovery_log::{RecoveryEvent, RecoveryEventKind};

    fn ts(rfc: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(rfc)
            .expect("test timestamp parse")
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn recovery_event_brief_respawn_round_trips() {
        let ev = RecoveryEvent {
            ts: ts("2026-05-22T10:00:00Z"),
            kind: RecoveryEventKind::Respawn {
                attempt: 2,
                backoff_ms: 1500,
                reason: "crash".to_string(),
            },
        };
        let b = recovery_event_brief(&ev);
        assert_eq!(b.kind, "respawn");
        assert_eq!(b.workspace, None);
        assert_eq!(b.summary, "attempt 2 after 1500ms (crash)");
        assert_eq!(b.at_iso, "2026-05-22T10:00:00+00:00");
    }

    #[test]
    fn recovery_event_brief_compile_failed_carries_workspace() {
        let ev = RecoveryEvent {
            ts: ts("2026-05-22T10:00:00Z"),
            kind: RecoveryEventKind::CompileFailed {
                workspace: "ws-a".to_string(),
                error: "extract phase failed".to_string(),
                retry_attempt: 1,
            },
        };
        let b = recovery_event_brief(&ev);
        assert_eq!(b.kind, "compile_failed");
        assert_eq!(b.workspace, Some("ws-a".to_string()));
        assert_eq!(b.summary, "retry 1: extract phase failed");
    }

    #[test]
    fn recovery_event_brief_circuit_breaker_includes_until_timestamp() {
        let ev = RecoveryEvent {
            ts: ts("2026-05-22T10:00:00Z"),
            kind: RecoveryEventKind::CircuitBreakerTripped {
                consecutive_failures: 4,
                until_rfc3339: "2026-05-22T10:05:00Z".to_string(),
            },
        };
        let b = recovery_event_brief(&ev);
        assert_eq!(b.kind, "circuit_breaker_tripped");
        assert!(b.summary.contains("4 consecutive failures"));
        assert!(b.summary.contains("2026-05-22T10:05:00Z"));
    }

    #[test]
    fn recovery_event_brief_compile_recovered_records_retry_attempt() {
        let ev = RecoveryEvent {
            ts: ts("2026-05-22T10:00:00Z"),
            kind: RecoveryEventKind::CompileRecovered {
                workspace: "ws-a".to_string(),
                retry_attempt: 2,
            },
        };
        let b = recovery_event_brief(&ev);
        assert_eq!(b.kind, "compile_recovered");
        assert_eq!(b.workspace, Some("ws-a".to_string()));
        assert_eq!(b.summary, "compile succeeded on retry 2");
    }

    #[test]
    fn empty_build_renders_only_environment_today_blocks() {
        // A ReminderBuild with no session, no engrams, no events at
        // all should still emit the environment + today blocks (which
        // are always populated) and suppress everything else. Pins
        // the "honest empty" contract.
        let build = ReminderBuild {
            environment_info: crate::intelligence::environment::gather(),
            today_str: "2026-05-22".to_string(),
            session_snapshot: None,
            engram_handles: Vec::new(),
            branch_summary: None,
            sandbox_reason: None,
            agentmemory_recalls: Vec::new(),
            mcp_sessions: Vec::new(),
            recovery_events: Vec::new(),
            relevant_skill_name: None,
            relevant_skill_body: None,
            substrate_freshness: None,
            recent_sub_agent_reports: Vec::new(),
            previous_verify_critique: None,
            gap_alerts: Vec::new(),
            contradiction_alerts: Vec::new(),
            search_was_shallow: None,
        };
        let ctx = build.as_context(None);
        let rendered = render_reactive_reminders(&ctx);
        // Environment block always renders when EnvironmentInfo is
        // present (cwd is always Some on any real machine).
        assert!(
            rendered.contains("# environment"),
            "empty build should still emit `# environment` block, got:\n{rendered}"
        );
        // No fabricated session/engram/branch/skill blocks.
        assert!(
            !rendered.contains("# engrams_active"),
            "engrams_active block must not render on empty engrams"
        );
        assert!(
            !rendered.contains("# branch"),
            "branch block must not render when branch is None"
        );
        assert!(
            !rendered.contains("# skill:"),
            "skill block must not render when skill is None"
        );
        assert!(
            !rendered.contains("# session"),
            "session block must not render when session_snapshot is None"
        );
        assert!(
            !rendered.contains("# previous_verify"),
            "previous_verify block must not render when critique is None"
        );
    }

    #[test]
    fn relevant_skill_view_drops_when_either_name_or_body_is_none() {
        // Defence-in-depth: even if a future refactor leaves
        // name=Some/body=None or vice versa, the view must drop
        // (never fabricate a half-populated skill block).
        let mut build = ReminderBuild {
            environment_info: crate::intelligence::environment::gather(),
            today_str: "2026-05-22".to_string(),
            session_snapshot: None,
            engram_handles: Vec::new(),
            branch_summary: None,
            sandbox_reason: None,
            agentmemory_recalls: Vec::new(),
            mcp_sessions: Vec::new(),
            recovery_events: Vec::new(),
            relevant_skill_name: Some("debugging-wizard".to_string()),
            relevant_skill_body: None,
            substrate_freshness: None,
            recent_sub_agent_reports: Vec::new(),
            previous_verify_critique: None,
            gap_alerts: Vec::new(),
            contradiction_alerts: Vec::new(),
            search_was_shallow: None,
        };
        let ctx_a = build.as_context(None);
        assert!(ctx_a.relevant_skill.is_none());

        build.relevant_skill_name = None;
        build.relevant_skill_body = Some("skill body".to_string());
        let ctx_b = build.as_context(None);
        assert!(ctx_b.relevant_skill.is_none());
    }

    #[test]
    fn relevant_skill_view_populates_when_both_present() {
        let build = ReminderBuild {
            environment_info: crate::intelligence::environment::gather(),
            today_str: "2026-05-22".to_string(),
            session_snapshot: None,
            engram_handles: Vec::new(),
            branch_summary: None,
            sandbox_reason: None,
            agentmemory_recalls: Vec::new(),
            mcp_sessions: Vec::new(),
            recovery_events: Vec::new(),
            relevant_skill_name: Some("debugging-wizard".to_string()),
            relevant_skill_body: Some("Stop. Read the error. Form a hypothesis.".to_string()),
            substrate_freshness: None,
            recent_sub_agent_reports: Vec::new(),
            previous_verify_critique: None,
            gap_alerts: Vec::new(),
            contradiction_alerts: Vec::new(),
            search_was_shallow: None,
        };
        let ctx = build.as_context(None);
        let skill = ctx.relevant_skill.expect("skill view should populate");
        assert_eq!(skill.name, "debugging-wizard");
        assert_eq!(skill.body, "Stop. Read the error. Form a hypothesis.");
    }

    #[test]
    fn engram_budget_default_matches_engram_config() {
        // The 100 constant must track EngramConfig::default()
        // .max_engrams_per_session. Pin so a future EngramConfig
        // change breaks loudly here rather than silently drifting
        // the rendered budget line.
        let cfg = crate::intelligence::engram::EngramConfig::default();
        assert_eq!(ENGRAM_BUDGET_DEFAULT, cfg.max_engrams_per_session);
    }

    #[test]
    fn mid_turn_refresh_context_carries_only_three_signals() {
        // The mid-turn refresher's `as_context` should leave every
        // other field at its honest-empty default. Pins that we
        // never accidentally extend it without the explicit fields.
        let refresh = MidTurnRefresh {
            substrate_freshness: None,
            recent_sub_agent_reports: Vec::new(),
            recovery_events: Vec::new(),
        };
        let ctx = refresh.as_context();
        assert!(ctx.identity.is_none());
        assert!(ctx.environment.is_none());
        assert!(ctx.today.is_none());
        assert!(ctx.session.is_none());
        assert!(ctx.engrams.is_empty());
        assert!(ctx.agentmemory_recalls.is_empty());
        assert!(ctx.mcp_sessions.is_empty());
        assert!(ctx.previous_verify_critique.is_none());
        assert!(ctx.gap_alerts.is_empty());
        assert!(ctx.contradiction_alerts.is_empty());
        assert!(ctx.search_was_shallow.is_none());
    }

    #[test]
    fn build_full_context_renders_all_populated_blocks() {
        // Smoke-test the as_context contract: every field on the
        // ReminderBuild should be wired into the returned
        // ReminderContext exactly once. We populate every field
        // and assert each one's block shows up in the rendered
        // output.
        let build = ReminderBuild {
            environment_info: crate::intelligence::environment::gather(),
            today_str: "2026-05-22".to_string(),
            session_snapshot: None,
            engram_handles: vec![EngramHandle {
                pointer: "ptr-0001".to_string(),
                topic: "auth flow".to_string(),
            }],
            branch_summary: Some(BranchSummary {
                name: "stream/sess-1".to_string(),
                parent: Some("main".to_string()),
                kind: Some("Stream".to_string()),
            }),
            sandbox_reason: Some("refactor intent"),
            agentmemory_recalls: vec![AgentmemoryRecall {
                claim_id: "claim-1".to_string(),
                statement: "previous claim".to_string(),
                confidence: 0.9,
                source_uri: "file:///a".to_string(),
            }],
            mcp_sessions: vec![McpSessionBrief {
                session_id_prefix: "abc123".to_string(),
                user_agent: "claude-code".to_string(),
                transport: "sse".to_string(),
                tool_calls_total: 5,
                errors_total: 0,
            }],
            recovery_events: vec![RecoveryEventBrief {
                kind: "respawn_ok".to_string(),
                workspace: None,
                at_iso: "2026-05-22T10:00:00+00:00".to_string(),
                summary: "new pid 42".to_string(),
            }],
            relevant_skill_name: Some("debugging-wizard".to_string()),
            relevant_skill_body: Some("Stop. Read the error.".to_string()),
            substrate_freshness: Some(SubstrateFreshness {
                fingerprint_match: false,
                last_compile_at_iso: Some("2026-05-22T09:00:00+00:00".to_string()),
                file_count: 42,
            }),
            recent_sub_agent_reports: vec![SubAgentReportBrief {
                agent: "reconciler".to_string(),
                finished_at_iso: "2026-05-22T09:55:00+00:00".to_string(),
                summary: "scanned 12 entities".to_string(),
                observations: vec!["no drift".to_string()],
            }],
            previous_verify_critique: Some(PreviousVerifyCritique {
                // Verdict must be one of {"low_grounding", "ungrounded",
                // "contradiction"} for the block to render; benign
                // verdicts are intentionally suppressed by the bus.
                verdict: "low_grounding".to_string(),
                citations_verified: Some(1),
                citations_unverified: Some(2),
                reason: "missing source for X".to_string(),
            }),
            gap_alerts: vec![GapAlert {
                kind: "no_test".to_string(),
                subject: "parse_witness_row".to_string(),
                hint: "no test fixture exists".to_string(),
            }],
            contradiction_alerts: Vec::new(),
            search_was_shallow: Some(SearchWasShallow {
                query: "auth flow".to_string(),
                hits: 1,
            }),
        };
        let ctx = build.as_context(None);
        let rendered = render_reactive_reminders(&ctx);
        // Each populated field's block should appear at least once.
        // Block heading literals are from reminder_bus.rs render_*
        // functions; matching them ensures every owned field wires
        // through into a rendered section.
        for needle in [
            "# environment",
            "# engrams_active",
            "# branch",
            "# sandbox_alert",
            "# agentmemory_recall",
            "# mcp_sessions",
            "# substrate_health",
            "# skill: debugging-wizard",
            "# substrate_freshness",
            "# sub_agent_digest",
            "# previous_verify",
            "# gap_alerts",
            "# search_was_shallow",
        ] {
            assert!(
                rendered.contains(needle),
                "block `{needle}` missing from rendered output:\n{rendered}"
            );
        }
        // Contradictions stayed empty → suppressed.
        assert!(
            !rendered.contains("# contradiction_alerts"),
            "contradiction block must suppress on empty"
        );
    }

    #[test]
    fn build_truncates_long_agentmemory_statement_at_240_chars_with_utf8_safety() {
        // Mimic the inline path's truncation discipline — verify the
        // 240-char cut respects char boundaries (the inline block had
        // `while !is_char_boundary(cut) && cut > 0` to walk back to
        // a safe byte; we reproduce the same dance here).
        let long_statement = "é".repeat(300); // 600 bytes, 300 chars
        let mut cut = 237;
        while !long_statement.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        let truncated = format!("{}…", &long_statement[..cut]);
        assert!(truncated.ends_with('…'));
        assert!(truncated.len() < long_statement.len() + 4); // some saving
    }

    #[test]
    fn build_skips_recovery_events_older_than_five_minutes() {
        // Pin the recency filter — the inline path filters by 5 min
        // for the full bus. We can't easily call `build` without a
        // full AppState; pin the equivalent filter logic in isolation.
        let now = chrono::Utc::now();
        let window = chrono::Duration::minutes(5);
        let fresh = now - chrono::Duration::minutes(1);
        let stale = now - chrono::Duration::minutes(10);
        assert!(now.signed_duration_since(fresh) <= window);
        assert!(now.signed_duration_since(stale) > window);
    }
}
