// crates/thinkingroot-serve/src/intelligence/agent_streaming.rs
//
// Streaming entry point for the agent. Bridges the per-chat HTTP /
// Tauri request to:
//
//   * `Agent::run_streaming` — emits `AgentEvent`s through an mpsc
//     channel as they happen.
//   * `ToolApprovalRouter` — registers a oneshot per write tool,
//     keyed by `tool_use_id`, in the shared `PendingApprovalMap` on
//     `AppState`. The `/ask/approval/{id}` POST handler resolves the
//     oneshot when the desktop UI / CLI sends a decision.
//
// The function wraps both pieces and returns a Stream of SSE-ready
// `AgentEvent`s plus the per-write-tool `tool_use_id`s the streaming
// handler needs to surface in its `approval_requested` events.
//
// Wire shape: every `AgentEvent` becomes one SSE `event:` of the
// matching kind, with the payload from `agent_event_to_sse_payload`
// as the `data:` JSON body. Call sites in `rest.rs` map the
// `Stream<Item = AgentEvent>` returned here to `axum::response::sse::Event`s.

use std::path::PathBuf;
use std::sync::Arc;

use thinkingroot_llm::llm::{ChatMessage, LlmClient, ToolChoice};
use tokio::sync::{RwLock, mpsc};

use crate::engine::QueryEngine;
use crate::intelligence::agent::{Agent, AgentEvent, AgentRequest, LlmBackend};
use crate::intelligence::approval::{ApprovalGate, PendingApprovalMap, ToolApprovalRouter};
use crate::intelligence::builtin_tools::{ToolContext, register_builtin_tools};
use crate::intelligence::permissions_gate::PermissionsGate;
use crate::intelligence::session::SessionStore;
use crate::intelligence::skills::SkillRegistry;
use crate::intelligence::trace::SharedTraceLog;
use thinkingroot_core::permissions::PermissionStore;

/// Inputs to one streaming agent invocation. Pulled out as a struct
/// so the REST handler can fill it once and pass it through, rather
/// than juggling 10 positional arguments.
pub struct StreamAgentRequest {
    pub workspace: String,
    pub workspace_root: PathBuf,
    pub session_id: String,
    pub agent_id: String,
    pub system_prompt: String,
    pub user_question: String,
    pub history: Vec<ChatMessage>,
    pub skills: Arc<SkillRegistry>,
}

/// Dependencies the streaming runner needs from the surrounding
/// application — mostly references handed off from `AppState`.
pub struct StreamAgentDeps {
    pub engine: Arc<RwLock<QueryEngine>>,
    pub llm: Arc<LlmClient>,
    pub sessions: SessionStore,
    pub pending_approvals: PendingApprovalMap,
    pub trace: Option<SharedTraceLog>,
    /// Shared per-deployment engram manager — same instance the SSE
    /// transport hands to `mcp::dispatch`. Required so the McpBridge
    /// adapter can call `mcp::tools::handle_call` for the bridged
    /// AEP tools (`materialize_engram`, `probe_engram`,
    /// `list_engrams`, `expire_engram`) without minting a parallel
    /// manager that would diverge from the REST/SSE pointer space.
    pub engram_manager: Arc<crate::intelligence::engram::EngramManager>,
    /// Phase D Wave 1 (2026-05-17) — shared identity-level
    /// permission store. The [`PermissionsGate`] wraps the
    /// per-request [`ToolApprovalRouter`] with rule-based path /
    /// command authorisation: `DEFAULT_DENY` paths (`~/.ssh/**`,
    /// `~/.aws/**`, browser profiles, etc.) are refused without
    /// prompting; user-authored `allow_always` rules bypass the
    /// prompt for paths they explicitly enabled.
    pub permission_store: Arc<RwLock<PermissionStore>>,
}

/// Spawn the agent in a tokio task and return the receiver side of the
/// event stream. The router is exposed so the REST handler can call
/// `set_pending_id` immediately before the agent dispatches a write
/// tool — the handler watches the event stream for `ToolCallProposed`
/// events with `is_write: true` and registers the matching pending
/// approval.
///
/// Channel buffer is intentionally small (`16`): the agent emits at
/// most one event per LLM round-trip + tool dispatch, and a slow SSE
/// consumer applying back-pressure is the right behaviour — better to
/// let the agent wait than to buffer a runaway loop.
///
/// **SOTA Lever 3 wire-in (2026-05-15):** every completed agent run
/// records the (user_prompt, assistant_reply) pair into the engine's
/// per-session [`crate::intelligence::observer::Observer`]. When the
/// Reflector threshold trips, the relay also drives
/// [`crate::engine::QueryEngine::flush_observations`] so condensed
/// observations and reflections land in the witness substrate
/// automatically — no client-side `flush_observations` MCP call
/// required. Cancellation or upstream error skips the observation
/// (honest: only completed turns produce memory entries).
pub fn spawn_agent_run(
    req: StreamAgentRequest,
    deps: StreamAgentDeps,
) -> (mpsc::Receiver<AgentEvent>, Arc<ToolApprovalRouter>) {
    let (tx, rx) = mpsc::channel::<AgentEvent>(16);

    let router = Arc::new(ToolApprovalRouter::new(deps.pending_approvals.clone()));
    // Phase D Wave 1 — wrap the SSE-bridge router with the
    // PermissionsGate so DEFAULT_DENY + user rules fire BEFORE
    // surfacing an `approval_requested` event to the UI. Allow
    // rules short-circuit without prompting; Deny rules reject
    // without prompting; Ask delegates to the router for the
    // existing UI prompt flow.
    let permissions_gate: Arc<dyn ApprovalGate> = Arc::new(PermissionsGate::new(
        deps.permission_store.clone(),
        router.clone() as Arc<dyn ApprovalGate>,
    ));

    // Capture the inputs the Observer wire-in needs BEFORE we hand
    // them off downstream. `req.user_question` is moved into
    // `messages` below, and `deps.engine` / `deps.sessions` are moved
    // into `ToolContext`; clones keep ownership available for the
    // post-run observation recording.
    let user_question_for_obs = req.user_question.clone();
    let session_id_for_obs = req.session_id.clone();
    let workspace_for_obs = req.workspace.clone();
    let engine_for_obs = deps.engine.clone();
    let sessions_for_obs = deps.sessions.clone();
    // Phase β.2 — auto-commit needs the model id for the
    // `CommitAuthor::Agent { model, principal }` projection.
    let llm_model_for_commit = deps.llm.model_name().to_string();

    // `register_builtin_tools` is async (it walks the live MCP
    // catalogue via the bridge), so ctx + registry + agent
    // construction is deferred into the spawned task below.
    // `agent_router` is the clone the agent itself takes; the
    // outer `router` Arc is what we return to the caller for
    // approval-decision dispatch.
    // The agent receives the PermissionsGate-wrapped gate; the
    // raw router stays exposed to the caller so the streaming
    // handler can still call `set_pending_id` before every write
    // dispatch (the Ask-delegation path needs that).
    let agent_gate = permissions_gate.clone();
    let router_for_pre_emit = router.clone();
    tokio::spawn(async move {
        let _ = router_for_pre_emit; // hold the Arc alive

        let ctx = ToolContext {
            engine: deps.engine,
            workspace: req.workspace.clone(),
            workspace_root: req.workspace_root,
            session_id: req.session_id.clone(),
            sessions: deps.sessions,
            agent_id: req.agent_id,
            skills: req.skills,
            engram_manager: deps.engram_manager,
        };
        let registry = register_builtin_tools(ctx).await;

        let llm: Arc<dyn LlmBackend> = deps.llm;
        let mut agent = Agent::new(llm, registry, agent_gate);
        if let Some(trace) = deps.trace {
            agent = agent.with_trace_log(trace);
        }

        // Build the initial conversation: history + the latest user
        // question as the final turn. The agent appends its own
        // assistant_tool_calls / tool_results turns as it iterates.
        let mut messages = req.history;
        messages.push(ChatMessage::User(req.user_question));

        let agent_req = AgentRequest {
            system: req.system_prompt,
            // C5: no refresher wired yet — will be set in Task 5/9
            // (system-reminder bus). Until then, agent path uses
            // static identity captured at request entry. Bug C5,
            // plan 2026-05-09.
            system_refresher: None,
            history: messages,
            tool_choice: ToolChoice::Auto,
        };

        // Relay channel between the agent and the caller's `tx`.
        // The agent writes events here; this task forwards them to
        // the caller AND captures `final_text` from the terminal
        // `Done` event so the post-run hook can record an observation.
        //
        // Why a relay vs. wiring the observation into the agent
        // loop: keeps the agent crate transport-agnostic — `Agent`
        // doesn't know about session stores or observers, only its
        // own `mpsc::Sender<AgentEvent>`. The relay is the seam where
        // session-aware concerns (Observer, future telemetry) land.
        let (relay_tx, mut relay_rx) = mpsc::channel::<AgentEvent>(16);
        let agent_handle = tokio::spawn(async move {
            agent.run_streaming(agent_req, relay_tx).await;
        });

        let captured_final = relay_events(&mut relay_rx, &tx).await;
        let _ = agent_handle.await;

        // Observe the completed turn. Best-effort, never propagates
        // an error: the chat reply has already streamed to the user
        // and the Observer is a downstream consolidation layer.
        if let Some(reply) = captured_final {
            // Phase β.2 — clone what the cognition-commit hook needs
            // BEFORE moving fields into `record_completed_turn`.
            let engine_for_commit = engine_for_obs.clone();
            let workspace_for_commit = workspace_for_obs.clone();
            let user_question_for_commit = user_question_for_obs.clone();
            let reply_for_commit = reply.clone();

            record_completed_turn(
                engine_for_obs,
                sessions_for_obs,
                workspace_for_obs,
                session_id_for_obs,
                user_question_for_obs,
                reply,
            )
            .await;

            // Phase β.2 — auto-commit the turn as a CognitionCommit
            // on `main`. Runs AFTER the observer so the substrate
            // already has any new witnesses the agent's tool calls
            // produced; their ids are then verifiable when this
            // commit's citations resolve. Best-effort: a failure here
            // doesn't reach the user (the reply has already streamed).
            record_cognition_commit_for_turn(
                engine_for_commit,
                workspace_for_commit,
                user_question_for_commit,
                reply_for_commit,
                llm_model_for_commit,
            )
            .await;
        }
    });

    (rx, router)
}

/// Forward every event from `agent_rx` to the caller's `tx`, capturing
/// `final_text` from the terminal `Done` event. When the caller drops
/// `tx`, the helper drains `agent_rx` to completion so the agent task
/// is never left blocked on a full channel.
///
/// Returns the captured `final_text` if a `Done` event was observed
/// before the relay terminated, or `None` if the agent finished
/// without `Done` (error path, cancellation, or upstream disconnect
/// before completion). Honest semantics: only fully-completed turns
/// produce an Observer recording.
async fn relay_events(
    agent_rx: &mut mpsc::Receiver<AgentEvent>,
    tx: &mpsc::Sender<AgentEvent>,
) -> Option<String> {
    let mut captured: Option<String> = None;
    while let Some(event) = agent_rx.recv().await {
        if let AgentEvent::Done { final_text, .. } = &event {
            captured = Some(final_text.clone());
        }
        if tx.send(event).await.is_err() {
            // Caller disconnected. Drain remaining events from the
            // agent so its spawned task can complete without
            // back-pressure, but stop forwarding upstream.
            while agent_rx.recv().await.is_some() {}
            break;
        }
    }
    captured
}

/// Persist one completed (user_prompt, assistant_reply) pair into the
/// engine's Observer. When the Reflector threshold trips, drains the
/// session's staged observations into the witness substrate.
///
/// All failures are logged at WARN and swallowed: this runs AFTER the
/// chat reply has been delivered, so a flush error doesn't reach the
/// user and shouldn't cancel a successful conversation.
async fn record_completed_turn(
    engine: Arc<RwLock<QueryEngine>>,
    sessions: crate::intelligence::session::SessionStore,
    workspace: String,
    session_id: String,
    user_prompt: String,
    assistant_reply: String,
) {
    // Allocate the next chat-turn ordinal under the session-store
    // mutex. Separate from the engine read-lock acquisition below so
    // we hold each lock for the minimum window.
    let turn_number = {
        let mut store = sessions.lock().await;
        let session = store
            .entry(session_id.clone())
            .or_insert_with(|| {
                crate::intelligence::session::SessionContext::new(&session_id, &workspace)
            });
        session.next_chat_turn()
    };

    // Snapshot the Observer handle so we can release the engine
    // read-lock before the (potentially slow) flush path.
    let observer = {
        let engine_guard = engine.read().await;
        engine_guard.observer()
    };

    observer.record_turn(crate::intelligence::observer::ChatTurn {
        session_id: session_id.clone(),
        turn_number,
        user_prompt,
        assistant_reply,
        at: chrono::Utc::now(),
    });

    if observer.should_reflect(&session_id) {
        let engine_guard = engine.read().await;
        match engine_guard.flush_observations(&workspace, &session_id).await {
            Ok(n) => {
                tracing::debug!(
                    target: "observer",
                    workspace = %workspace,
                    session_id = %session_id,
                    inserted = n,
                    "auto-flush after chat turn persisted observations"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "observer",
                    workspace = %workspace,
                    session_id = %session_id,
                    error = %e,
                    "auto-flush after chat turn failed (non-fatal)"
                );
            }
        }
    }
}

/// Auto-commit one completed agent turn as a `CognitionCommit` on the
/// workspace's `main` branch.
///
/// The commit threads to the previous `main` commit as parent so the
/// chat history forms a real DAG. Citations are extracted from the
/// assistant reply via `citation_markers::extract_witness_citations`
/// and filtered to those that actually resolve to a real Witness in
/// the workspace — fabricated markers are silently dropped (with a
/// debug log) rather than failing the entire commit. The first
/// branch-genesis commit has `parent = None`.
///
/// All failures log at WARN and never propagate: this runs AFTER the
/// chat reply has been delivered, so a commit error must not reach
/// the user. The "real revolution" piece (chat history IS the commit
/// DAG) is honest about being best-effort.
async fn record_cognition_commit_for_turn(
    engine: Arc<RwLock<QueryEngine>>,
    workspace: String,
    user_prompt: String,
    assistant_reply: String,
    llm_model: String,
) {
    use thinkingroot_core::types::{CognitionCommit, CommitAuthor};

    const AUTOCOMMIT_BRANCH: &str = "main";
    const AGENT_PRINCIPAL: &str = "thinkingroot";

    // Extract + verify citations off the read lock first so we hold
    // it for the minimum window across the commit write.
    let raw_citations = crate::intelligence::citation_markers::extract_witness_citations(
        &assistant_reply,
    );
    let mut verified_citations: Vec<thinkingroot_core::types::WitnessId> = Vec::new();
    {
        let engine_guard = engine.read().await;
        for id in &raw_citations {
            match engine_guard.get_witness(&workspace, &id.to_hex()).await {
                Ok(Some(_)) => verified_citations.push(*id),
                Ok(None) => tracing::debug!(
                    target: "cognition_commit",
                    workspace = %workspace,
                    witness_id = %id.to_hex(),
                    "auto-commit: dropping fabricated citation marker"
                ),
                Err(e) => {
                    tracing::warn!(
                        target: "cognition_commit",
                        workspace = %workspace,
                        witness_id = %id.to_hex(),
                        error = %e,
                        "auto-commit: get_witness failed for citation"
                    );
                }
            }
        }
    }

    // Look up the parent (latest commit on `main`) so the new commit
    // threads into the DAG. None on a fresh branch is correct — that
    // commit becomes the genesis.
    let parent = {
        let engine_guard = engine.read().await;
        match engine_guard
            .list_cognition_commits(&workspace, AUTOCOMMIT_BRANCH, Some(1))
            .await
        {
            Ok(commits) => commits.first().map(|c| c.id),
            Err(e) => {
                tracing::warn!(
                    target: "cognition_commit",
                    workspace = %workspace,
                    error = %e,
                    "auto-commit: list_cognition_commits failed; skipping commit"
                );
                return;
            }
        }
    };

    let author = CommitAuthor::Agent {
        model: llm_model,
        principal: AGENT_PRINCIPAL.to_string(),
    };
    let commit = CognitionCommit::new(
        parent,
        AUTOCOMMIT_BRANCH.to_string(),
        author,
        user_prompt,
        assistant_reply,
        Vec::new(), // witnesses_added — populated by explicit `commit_cognition` calls
        verified_citations,
        Vec::new(), // gaps_surfaced — populated by explicit calls
        chrono::Utc::now(),
    );

    let engine_guard = engine.read().await;
    match engine_guard.commit_cognition(&workspace, &commit).await {
        Ok(()) => {
            tracing::debug!(
                target: "cognition_commit",
                workspace = %workspace,
                commit_id = %commit.id,
                citations = commit.citations.len(),
                "auto-commit recorded agent turn"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "cognition_commit",
                workspace = %workspace,
                error = %e,
                "auto-commit: commit_cognition failed (non-fatal)"
            );
        }
    }
}

/// Translate one [`AgentEvent`] into the (event_name, json_data) pair
/// the SSE wire emits. Stable contract — the desktop's `chat-event`
/// Tauri channel keys off the `type` field in the JSON body too, so
/// changes here ripple to `apps/thinkingroot-desktop/src-tauri/src/
/// commands/chat.rs::ChatEvent`.
pub fn agent_event_to_sse(event: &AgentEvent) -> (&'static str, serde_json::Value) {
    use serde_json::json;
    match event {
        AgentEvent::Text { content } => ("token", json!({"text": content})),
        AgentEvent::ToolCallProposed {
            id,
            name,
            input,
            is_write,
        } => (
            "tool_call_proposed",
            json!({
                "id": id,
                "name": name,
                "input": input,
                "is_write": is_write,
            }),
        ),
        AgentEvent::ToolCallRejected { id, name, reason } => (
            "tool_call_rejected",
            json!({"id": id, "name": name, "reason": reason}),
        ),
        AgentEvent::ToolCallExecuting { id, name } => {
            ("tool_call_executing", json!({"id": id, "name": name}))
        }
        AgentEvent::ToolCallFinished {
            id,
            name,
            content,
            is_error,
        } => (
            "tool_call_finished",
            json!({
                "id": id,
                "name": name,
                "content": content,
                "is_error": is_error,
            }),
        ),
        AgentEvent::Done {
            final_text,
            iterations,
        } => (
            "final",
            json!({
                "full_text": final_text,
                "iterations": iterations,
            }),
        ),
        AgentEvent::Error { message } => ("error", json!({"message": message})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_event_to_sse_maps_text_to_token() {
        let (kind, payload) = agent_event_to_sse(&AgentEvent::Text {
            content: "hello".into(),
        });
        assert_eq!(kind, "token");
        assert_eq!(payload["text"], "hello");
    }

    #[test]
    fn agent_event_to_sse_maps_tool_call_proposed_with_is_write() {
        let (kind, payload) = agent_event_to_sse(&AgentEvent::ToolCallProposed {
            id: "c1".into(),
            name: "create_branch".into(),
            input: json!({"name": "exp"}),
            is_write: true,
        });
        assert_eq!(kind, "tool_call_proposed");
        assert_eq!(payload["id"], "c1");
        assert_eq!(payload["name"], "create_branch");
        assert_eq!(payload["is_write"], true);
        assert_eq!(payload["input"]["name"], "exp");
    }

    #[test]
    fn agent_event_to_sse_maps_done_to_final_with_full_text() {
        let (kind, payload) = agent_event_to_sse(&AgentEvent::Done {
            final_text: "done answer".into(),
            iterations: 3,
        });
        assert_eq!(kind, "final");
        assert_eq!(payload["full_text"], "done answer");
        assert_eq!(payload["iterations"], 3);
    }

    #[test]
    fn agent_event_to_sse_maps_error_to_error_event() {
        let (kind, payload) = agent_event_to_sse(&AgentEvent::Error {
            message: "boom".into(),
        });
        assert_eq!(kind, "error");
        assert_eq!(payload["message"], "boom");
    }

    #[test]
    fn agent_event_to_sse_maps_rejected_with_reason() {
        let (kind, payload) = agent_event_to_sse(&AgentEvent::ToolCallRejected {
            id: "c1".into(),
            name: "create_branch".into(),
            reason: "user said no".into(),
        });
        assert_eq!(kind, "tool_call_rejected");
        assert_eq!(payload["reason"], "user said no");
    }

    #[tokio::test]
    async fn relay_events_forwards_every_event_verbatim_and_captures_done() {
        let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(4);
        let (caller_tx, mut caller_rx) = mpsc::channel::<AgentEvent>(4);

        let producer = tokio::spawn(async move {
            agent_tx
                .send(AgentEvent::Text { content: "hello".into() })
                .await
                .unwrap();
            agent_tx
                .send(AgentEvent::ToolCallExecuting {
                    id: "t1".into(),
                    name: "search".into(),
                })
                .await
                .unwrap();
            agent_tx
                .send(AgentEvent::Done {
                    final_text: "the answer is 42".into(),
                    iterations: 2,
                })
                .await
                .unwrap();
            // drop sender → relay sees end-of-stream
        });

        let captured = relay_events(&mut agent_rx, &caller_tx).await;
        producer.await.unwrap();

        // Caller receives the three events verbatim.
        let first = caller_rx.recv().await.unwrap();
        assert!(matches!(first, AgentEvent::Text { content } if content == "hello"));
        let second = caller_rx.recv().await.unwrap();
        assert!(matches!(
            second,
            AgentEvent::ToolCallExecuting { name, .. } if name == "search"
        ));
        let third = caller_rx.recv().await.unwrap();
        assert!(matches!(
            third,
            AgentEvent::Done { final_text, iterations } if final_text == "the answer is 42" && iterations == 2
        ));

        assert_eq!(
            captured.as_deref(),
            Some("the answer is 42"),
            "Done event must be captured for Observer recording"
        );
    }

    #[tokio::test]
    async fn relay_events_returns_none_when_no_done_arrives() {
        let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(4);
        let (caller_tx, _caller_rx) = mpsc::channel::<AgentEvent>(4);

        let producer = tokio::spawn(async move {
            agent_tx
                .send(AgentEvent::Text { content: "partial".into() })
                .await
                .unwrap();
            agent_tx
                .send(AgentEvent::Error { message: "boom".into() })
                .await
                .unwrap();
            // No Done — drop sender.
        });

        let captured = relay_events(&mut agent_rx, &caller_tx).await;
        producer.await.unwrap();

        assert!(
            captured.is_none(),
            "no Done event → no Observer recording (honest: incomplete turns produce no memory)"
        );
    }

    #[tokio::test]
    async fn relay_events_drains_when_caller_disconnects() {
        let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(4);
        let (caller_tx, caller_rx) = mpsc::channel::<AgentEvent>(4);

        // Caller drops their receiver immediately.
        drop(caller_rx);

        let producer = tokio::spawn(async move {
            for i in 0..3 {
                let _ = agent_tx
                    .send(AgentEvent::Text {
                        content: format!("msg{i}"),
                    })
                    .await;
            }
            agent_tx
                .send(AgentEvent::Done {
                    final_text: "irrelevant".into(),
                    iterations: 1,
                })
                .await
                .unwrap_or(());
        });

        // Even with the caller gone, relay_events must terminate
        // (drain loop) so the agent task can complete.
        let _captured = relay_events(&mut agent_rx, &caller_tx).await;
        producer.await.unwrap();
        // No assertion on `_captured` — the Done may or may not have
        // been read before the caller-disconnect break; what matters
        // is that the call returned.
    }

    #[test]
    fn agent_event_to_sse_maps_executing_and_finished() {
        let (kind, _payload) = agent_event_to_sse(&AgentEvent::ToolCallExecuting {
            id: "c1".into(),
            name: "search".into(),
        });
        assert_eq!(kind, "tool_call_executing");

        let (kind2, payload2) = agent_event_to_sse(&AgentEvent::ToolCallFinished {
            id: "c1".into(),
            name: "search".into(),
            content: "ok".into(),
            is_error: false,
        });
        assert_eq!(kind2, "tool_call_finished");
        assert_eq!(payload2["content"], "ok");
        assert_eq!(payload2["is_error"], false);
    }
}
