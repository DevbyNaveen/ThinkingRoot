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

use thinkingroot_extract::llm::{ChatMessage, LlmClient, ToolChoice};
use tokio::sync::{RwLock, mpsc};

use crate::engine::QueryEngine;
use crate::intelligence::agent::{Agent, AgentEvent, AgentRequest, LlmBackend};
use crate::intelligence::approval::{PendingApprovalMap, ToolApprovalRouter};
use crate::intelligence::builtin_tools::{ToolContext, register_builtin_tools};
use crate::intelligence::session::SessionStore;
use crate::intelligence::skills::SkillRegistry;
use crate::intelligence::trace::SharedTraceLog;

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
pub fn spawn_agent_run(
    req: StreamAgentRequest,
    deps: StreamAgentDeps,
) -> (mpsc::Receiver<AgentEvent>, Arc<ToolApprovalRouter>) {
    let (tx, rx) = mpsc::channel::<AgentEvent>(16);

    let router = Arc::new(ToolApprovalRouter::new(deps.pending_approvals.clone()));

    let ctx = ToolContext {
        engine: deps.engine,
        workspace: req.workspace.clone(),
        workspace_root: req.workspace_root,
        session_id: req.session_id.clone(),
        sessions: deps.sessions,
        agent_id: req.agent_id,
        skills: req.skills,
    };
    let registry = register_builtin_tools(ctx);

    let llm: Arc<dyn LlmBackend> = deps.llm;
    let mut agent = Agent::new(llm, registry, router.clone());
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
        history: messages,
        tool_choice: ToolChoice::Auto,
    };

    let router_for_pre_emit = router.clone();
    tokio::spawn(async move {
        // The router needs the tool_use_id of the next write call set
        // BEFORE the agent's write-tool dispatch hits the gate. The
        // simplest way to do that without hooking into the agent loop
        // is for the SSE-consuming handler to call
        // `router.set_pending_id` when it sees `ToolCallProposed`
        // with `is_write: true`. To make that happen, we forward
        // events through a relay: each event passes through here and
        // gets an extra side-effect when it's a write proposal.
        //
        // We can't reach across to the SSE handler from this task
        // without coupling them, so the router is shared via Arc and
        // the handler is responsible for the side-effect.
        //
        // (This relay task only forwards events; the handler does the
        // pending-id registration. Keeping them decoupled is what
        // makes the router testable on its own.)
        let _ = router_for_pre_emit; // hold the Arc alive
        agent.run_streaming(agent_req, tx).await;
    });

    (rx, router)
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
