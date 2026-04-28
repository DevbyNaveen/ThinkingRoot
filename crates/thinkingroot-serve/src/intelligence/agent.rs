// crates/thinkingroot-serve/src/intelligence/agent.rs
//
// Multi-turn tool-using agent.
//
// The agent loop drives `LlmClient::chat_with_tools` from S2 plus the
// `ToolRegistry` from S3. It owns three responsibilities:
//
//   1. **Iterate.** Call `chat_with_tools` with the running message
//      history. If the response is `Text`, emit it as the final
//      answer and stop. If it's `ToolCalls`, dispatch each call,
//      append the results to history, and call again.
//
//   2. **Gate writes.** Write tools (registered via
//      `ToolRegistry::register_write`) route through the configured
//      [`ApprovalGate`]. Reads dispatch unconditionally. Rejected
//      writes are fed back to the LLM as `is_error: true` ToolResults
//      so the model can adapt rather than crash.
//
//   3. **Stay bounded.** The loop has a hard ceiling on iterations
//      (`max_iterations`, default 8) so a misbehaving model cannot
//      spin forever burning tokens. Hitting the ceiling produces a
//      final answer assembled from whatever text the model has
//      emitted across iterations, with an `Error` event noting the
//      truncation cause.
//
// Wire format the loop emits is the [`AgentEvent`] enum. The HTTP
// streaming handler (in S5) maps each event to an SSE event the
// desktop's `ChatView` consumes — `text` deltas to token bubbles,
// `tool_call_*` events to claim cards, `done` / `error` to the final
// state.

use std::sync::Arc;

use async_trait::async_trait;
use thinkingroot_extract::llm::{
    ChatMessage, LlmClient, Tool, ToolCall, ToolChoice, ToolResult, ToolUseResponse,
};
use thinkingroot_core::{Error, Result};

use crate::intelligence::approval::{ApprovalDecision, ApprovalGate};
use crate::intelligence::synthesizer::{ChatRole, ChatTurn};
use crate::intelligence::tools::ToolRegistry;

/// The narrow LLM surface the agent loop needs. Production wires
/// `Arc<LlmClient>` (the trait is implemented for it via the impl
/// below); tests pass any stub that implements the same shape.
///
/// Pulled out as a trait so the agent loop is testable end-to-end
/// without spinning up a real provider — pure unit tests can assert
/// "given these LLM responses in sequence, the loop emits these
/// events".
#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse>;
}

#[async_trait]
impl LlmBackend for LlmClient {
    async fn chat_with_tools(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
        tool_choice: &ToolChoice,
    ) -> Result<ToolUseResponse> {
        LlmClient::chat_with_tools(self, system, messages, tools, tool_choice).await
    }
}

/// One observable thing the agent loop did. Streamed in order via
/// [`Agent::run`]. The HTTP / Tauri layer maps each variant to the
/// matching wire event.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The model emitted prose, either as the final answer or as a
    /// pre-amble before tool calls. Multiple `Text` events may fire
    /// per run when iterations interleave prose and tool use.
    Text { content: String },
    /// The model wants to call a tool. Fired before any approval
    /// check or dispatch so the UI can show an "AI is thinking about
    /// {tool}" affordance even if the call is later rejected.
    ToolCallProposed {
        id: String,
        name: String,
        input: serde_json::Value,
        is_write: bool,
    },
    /// Approval was sought for a write tool and the host said no. The
    /// rejection is fed to the LLM as a tool error result; this event
    /// lets the UI show "{tool} declined: {reason}".
    ToolCallRejected {
        id: String,
        name: String,
        reason: String,
    },
    /// Tool execution started (after approval, if write).
    ToolCallExecuting { id: String, name: String },
    /// Tool execution finished. `is_error` mirrors the
    /// [`ToolHandlerResult`] flag — UI can colour the card
    /// accordingly. `content` is the same string fed back to the
    /// LLM.
    ToolCallFinished {
        id: String,
        name: String,
        content: String,
        is_error: bool,
    },
    /// Loop terminated cleanly with the model's final text answer.
    /// `iterations` is the number of LLM round-trips taken (always
    /// at least 1, capped at `max_iterations`).
    Done {
        final_text: String,
        iterations: usize,
    },
    /// Loop hit a fatal error — most often a non-retryable LLM
    /// failure or hitting `max_iterations` without a terminal text
    /// reply. The UI surfaces this and stops the spinner.
    Error { message: String },
}

/// Inputs to one agent run.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// System prompt to pass to every `chat_with_tools` call.
    pub system: String,
    /// Initial message history. The caller is responsible for
    /// putting the user's most-recent question at the end (typically
    /// as the last `ChatMessage::User`). Subsequent turns are appended
    /// by the loop.
    pub history: Vec<ChatMessage>,
    /// Forwarded to every `chat_with_tools` call. `Auto` is the right
    /// default for conversational chat (model decides). `Any` forces
    /// a tool on the first turn — useful for "investigate" CLI flows.
    /// `None` disables tools entirely on this run.
    pub tool_choice: ToolChoice,
}

/// Configuration knobs for the agent. Defaults match the safe
/// production setpoint: 8 iterations max, AutoApprove disabled
/// (caller MUST supply a gate), no parallel dispatch.
pub struct AgentConfig {
    /// Maximum LLM round-trips per `run`. Hitting the ceiling causes
    /// the loop to terminate with whatever text has accumulated and
    /// emit an `Error` event noting the cause.
    pub max_iterations: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { max_iterations: 8 }
    }
}

/// The agent. Cheap to clone — every field is an `Arc` or a
/// reference-counted [`ToolRegistry`].
#[derive(Clone)]
pub struct Agent {
    llm: Arc<dyn LlmBackend>,
    registry: ToolRegistry,
    approval: Arc<dyn ApprovalGate>,
    max_iterations: usize,
}

impl Agent {
    pub fn new(
        llm: Arc<dyn LlmBackend>,
        registry: ToolRegistry,
        approval: Arc<dyn ApprovalGate>,
    ) -> Self {
        Self::with_config(llm, registry, approval, AgentConfig::default())
    }

    pub fn with_config(
        llm: Arc<dyn LlmBackend>,
        registry: ToolRegistry,
        approval: Arc<dyn ApprovalGate>,
        config: AgentConfig,
    ) -> Self {
        Self {
            llm,
            registry,
            approval,
            max_iterations: config.max_iterations,
        }
    }

    /// Tools registered for this agent. Surfaced for the synthesizer /
    /// REST layer that wants to show users which capabilities the
    /// agent has.
    pub fn tool_specs(&self) -> Vec<Tool> {
        self.registry.specs()
    }

    /// Run the loop, collecting every event into a `Vec`. Convenient
    /// for tests and CLI surfaces that don't need streaming.
    /// Production HTTP / Tauri callers will use the streaming variant
    /// in S5.
    pub async fn run_collected(&self, req: AgentRequest) -> Vec<AgentEvent> {
        let mut events: Vec<AgentEvent> = Vec::new();
        self.run_into(req, &mut events).await;
        events
    }

    /// Run the loop, pushing every event into `out`. Shared core
    /// between the sync `run_collected` and the streaming variant
    /// (S5) — the streaming wrapper turns `out` into an mpsc sender.
    pub async fn run_into(&self, req: AgentRequest, out: &mut Vec<AgentEvent>) {
        let tools = self.registry.specs();
        let mut history = req.history;
        let mut iterations: usize = 0;
        let mut accumulated_text = String::new();
        // First call uses the caller-supplied tool_choice; subsequent
        // calls always use `Auto` because forcing a tool on a
        // post-results turn would loop forever.
        let mut tool_choice = req.tool_choice.clone();

        while iterations < self.max_iterations {
            iterations += 1;

            let response = match self
                .llm
                .chat_with_tools(&req.system, &history, &tools, &tool_choice)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    out.push(AgentEvent::Error {
                        message: format!("LLM call failed on iteration {iterations}: {e}"),
                    });
                    return;
                }
            };

            match response {
                ToolUseResponse::Text { text, truncated, .. } => {
                    if !text.is_empty() {
                        accumulated_text.push_str(&text);
                        out.push(AgentEvent::Text {
                            content: text.clone(),
                        });
                    }
                    if truncated {
                        out.push(AgentEvent::Error {
                            message: format!(
                                "model output truncated at iteration {iterations} \
                                 (hit max_tokens)"
                            ),
                        });
                    }
                    out.push(AgentEvent::Done {
                        final_text: accumulated_text,
                        iterations,
                    });
                    return;
                }
                ToolUseResponse::ToolCalls {
                    calls,
                    text_preamble,
                    ..
                } => {
                    if !text_preamble.is_empty() {
                        accumulated_text.push_str(&text_preamble);
                        out.push(AgentEvent::Text {
                            content: text_preamble.clone(),
                        });
                    }
                    // Append the assistant's tool_use turn so the
                    // next call sees the conversation in shape.
                    history.push(ChatMessage::AssistantToolCalls(calls.clone()));
                    let results = self.dispatch_calls(&calls, out).await;
                    history.push(ChatMessage::ToolResults(results));
                    // Subsequent iterations always use Auto: forcing
                    // tools again would create an infinite loop.
                    tool_choice = ToolChoice::Auto;
                }
            }
        }

        // Fell off the iteration ceiling.
        out.push(AgentEvent::Error {
            message: format!(
                "agent stopped at iteration ceiling ({}). Partial text length: {}",
                self.max_iterations,
                accumulated_text.len()
            ),
        });
        out.push(AgentEvent::Done {
            final_text: accumulated_text,
            iterations,
        });
    }

    /// Dispatch one batch of tool calls. Each call:
    ///   * Emits `ToolCallProposed`.
    ///   * If write, gates via the [`ApprovalGate`]. Rejection produces
    ///     a `ToolCallRejected` event and a synthetic error
    ///     [`ToolResult`] that the model sees on the next turn.
    ///   * Otherwise emits `ToolCallExecuting`, dispatches via the
    ///     registry, emits `ToolCallFinished` with the result.
    ///
    /// Returns the [`ToolResult`] vector to append to history.
    /// Sequential dispatch (Anthropic cookbook recommendation): keeps
    /// the conversation shape clean and avoids tool race conditions
    /// when tools share state (e.g. `create_branch` then
    /// `contribute_claim` on that branch).
    async fn dispatch_calls(
        &self,
        calls: &[ToolCall],
        out: &mut Vec<AgentEvent>,
    ) -> Vec<ToolResult> {
        let mut results: Vec<ToolResult> = Vec::with_capacity(calls.len());
        for call in calls {
            let is_write = self.registry.is_write(&call.name);
            out.push(AgentEvent::ToolCallProposed {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
                is_write,
            });

            if is_write {
                let decision = self.approval.check(&call.name, &call.input).await;
                if let ApprovalDecision::Rejected { reason } = decision {
                    out.push(AgentEvent::ToolCallRejected {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        reason: reason.clone(),
                    });
                    // Feed rejection back to the model as a tool
                    // error so it can adapt (apologise, ask, etc.)
                    // rather than crashing.
                    results.push(ToolResult {
                        tool_use_id: call.id.clone(),
                        content: format!("user declined: {reason}"),
                        is_error: true,
                    });
                    continue;
                }
            }

            out.push(AgentEvent::ToolCallExecuting {
                id: call.id.clone(),
                name: call.name.clone(),
            });
            let res = self.registry.dispatch(&call.name, call.input.clone()).await;
            out.push(AgentEvent::ToolCallFinished {
                id: call.id.clone(),
                name: call.name.clone(),
                content: res.content.clone(),
                is_error: res.is_error,
            });
            results.push(ToolResult {
                tool_use_id: call.id.clone(),
                content: res.content,
                is_error: res.is_error,
            });
        }
        results
    }
}

/// Convert the synthesizer's [`ChatTurn`] history into the
/// [`ChatMessage`] shape the LLM client expects. Provided for
/// callers wiring chat surfaces that already pass `ChatTurn` —
/// e.g. the desktop UI's per-conversation history.
pub fn chat_turns_to_messages(turns: &[ChatTurn]) -> Vec<ChatMessage> {
    turns
        .iter()
        .map(|t| match t.role {
            ChatRole::User => ChatMessage::User(t.content.clone()),
            ChatRole::Assistant => ChatMessage::AssistantText(t.content.clone()),
        })
        .collect()
}

/// Best-effort summary of the final assistant text from a completed
/// event run. Returns `None` if the run never emitted `Done`. Useful
/// when the caller wants the answer string without iterating events
/// (e.g. when surfacing an agent reply through a non-streaming
/// transport).
pub fn final_text(events: &[AgentEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        AgentEvent::Done { final_text, .. } => Some(final_text.clone()),
        _ => None,
    })
}

/// Best-effort error summary. Returns the first `Error` message in
/// the event stream, or `None` if there were no errors.
pub fn first_error(events: &[AgentEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentEvent::Error { message } => Some(message.clone()),
        _ => None,
    })
}

/// A `Result` wrapper around a typical agent invocation: collected
/// events, plus shortcuts to the final text and any error message.
/// Keeps test-side assertions concise.
pub struct AgentRun {
    pub events: Vec<AgentEvent>,
}

impl AgentRun {
    pub fn final_text(&self) -> Option<String> {
        final_text(&self.events)
    }
    pub fn first_error(&self) -> Option<String> {
        first_error(&self.events)
    }
    pub fn iterations(&self) -> usize {
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                AgentEvent::Done { iterations, .. } => Some(*iterations),
                _ => None,
            })
            .unwrap_or(0)
    }
    pub fn tool_calls_executed(&self) -> Vec<&str> {
        self.events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCallFinished { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }
    pub fn tool_calls_rejected(&self) -> Vec<&str> {
        self.events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCallRejected { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// Helper used in tests: invoke an agent against a `Vec<ChatMessage>`
/// history and return a structured [`AgentRun`].
#[cfg(test)]
pub async fn run_to_completion(agent: &Agent, req: AgentRequest) -> AgentRun {
    AgentRun {
        events: agent.run_collected(req).await,
    }
}

/// Lightweight error wrapper used by the (future) streaming variant
/// that needs a `Result` boundary. Re-export so the streaming entry
/// point introduced in S5 has a stable type to surface.
pub type AgentError = Error;
pub type AgentResult<T> = Result<T>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::approval::{AutoApprove, DenyAll};
    use crate::intelligence::tools::{ToolHandler, ToolHandlerResult};
    use serde_json::json;
    use std::sync::Mutex;
    use thinkingroot_extract::scheduler::HeaderRateLimits;

    /// Stub LLM backend that replays a fixed script of responses.
    /// Each `chat_with_tools` call pops the next scripted response.
    /// Used to assert the agent loop's behaviour for both terminal
    /// text replies and multi-iteration tool-use sequences without
    /// touching a real provider.
    struct ScriptedLlm {
        script: Mutex<Vec<ToolUseResponse>>,
        calls_seen: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl ScriptedLlm {
        fn new(script: Vec<ToolUseResponse>) -> Self {
            Self {
                script: Mutex::new(script),
                calls_seen: Mutex::new(Vec::new()),
            }
        }

        fn calls_seen(&self) -> Vec<Vec<ChatMessage>> {
            self.calls_seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmBackend for ScriptedLlm {
        async fn chat_with_tools(
            &self,
            _system: &str,
            messages: &[ChatMessage],
            _tools: &[Tool],
            _tool_choice: &ToolChoice,
        ) -> Result<ToolUseResponse> {
            self.calls_seen.lock().unwrap().push(messages.to_vec());
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Err(Error::Extraction {
                    source_id: "scripted_llm".into(),
                    message: "script exhausted".into(),
                });
            }
            Ok(script.remove(0))
        }
    }

    /// Stub tool handler that captures every input it receives.
    /// Returns a fixed string the test asserts against.
    struct CapturingHandler {
        name: &'static str,
        captured: Arc<Mutex<Vec<serde_json::Value>>>,
        reply: String,
        is_error: bool,
    }

    #[async_trait]
    impl ToolHandler for CapturingHandler {
        async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
            self.captured.lock().unwrap().push(input);
            ToolHandlerResult {
                content: format!("{}:{}", self.name, self.reply),
                is_error: self.is_error,
            }
        }
    }

    fn fixture_tool(name: &'static str) -> Tool {
        Tool::new(
            name,
            format!("test tool {name}"),
            json!({"type": "object", "properties": {}}),
        )
    }

    fn empty_limits() -> HeaderRateLimits {
        HeaderRateLimits::default()
    }

    #[tokio::test]
    async fn run_emits_done_with_final_text_when_first_response_is_text() {
        let llm = Arc::new(ScriptedLlm::new(vec![ToolUseResponse::Text {
            text: "There are three providers: Azure, Anthropic, OpenAI.".to_string(),
            truncated: false,
            limits: empty_limits(),
        }]));
        let registry = ToolRegistry::new();
        let agent = Agent::new(llm, registry, Arc::new(AutoApprove));

        let req = AgentRequest {
            system: "you are helpful".to_string(),
            history: vec![ChatMessage::user("how many providers")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;

        assert_eq!(run.iterations(), 1);
        assert_eq!(
            run.final_text().as_deref(),
            Some("There are three providers: Azure, Anthropic, OpenAI.")
        );
        assert!(run.first_error().is_none());
        assert!(run.tool_calls_executed().is_empty());
        assert!(run.tool_calls_rejected().is_empty());
    }

    #[tokio::test]
    async fn run_dispatches_tool_then_synthesises_final_answer() {
        // Iter 1: model asks for tool.  Iter 2: model emits final text.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let registry = ToolRegistry::new().register_read(
            fixture_tool("search"),
            Arc::new(CapturingHandler {
                name: "search",
                captured: captured.clone(),
                reply: "Azure, Anthropic, OpenAI".to_string(),
                is_error: false,
            }),
        );

        let llm = Arc::new(ScriptedLlm::new(vec![
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "search".to_string(),
                    input: json!({"query": "providers"}),
                }],
                text_preamble: "Let me search.".to_string(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "Three providers: Azure, Anthropic, OpenAI.".to_string(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));

        let agent = Agent::new(llm.clone(), registry, Arc::new(AutoApprove));
        let req = AgentRequest {
            system: "sys".to_string(),
            history: vec![ChatMessage::user("how many providers")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;

        assert_eq!(run.iterations(), 2);
        assert_eq!(run.tool_calls_executed(), vec!["search"]);
        assert!(run.tool_calls_rejected().is_empty());
        assert!(run.first_error().is_none());

        // Final text accumulates the preamble and the iteration-2 reply.
        let final_text = run.final_text().expect("expected Done event");
        assert!(final_text.contains("Let me search."));
        assert!(final_text.contains("Three providers"));

        // Tool was called with the input the model emitted.
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["query"], "providers");

        // The LLM saw the tool result on its second call.
        let calls = llm.calls_seen();
        assert_eq!(calls.len(), 2);
        let second_call = &calls[1];
        // [0] user, [1] assistant tool_calls, [2] tool results
        assert_eq!(second_call.len(), 3);
        match &second_call[2] {
            ChatMessage::ToolResults(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].tool_use_id, "call_1");
                assert!(results[0].content.contains("Azure, Anthropic, OpenAI"));
                assert!(!results[0].is_error);
            }
            other => panic!("expected ToolResults at index 2, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_routes_write_tools_through_approval_gate_and_rejects() {
        // The model proposes a write; the gate denies; the loop feeds
        // the rejection back as an is_error tool result; the model then
        // emits a final text apologising. Tests that:
        //   1. write tools route through approval (not auto-dispatched)
        //   2. rejection emits a `ToolCallRejected` event
        //   3. the loop continues — the LLM gets a chance to recover
        let registry = ToolRegistry::new().register_write(
            fixture_tool("create_branch"),
            Arc::new(CapturingHandler {
                name: "create_branch",
                captured: Arc::new(Mutex::new(Vec::new())),
                reply: "this should never run".to_string(),
                is_error: false,
            }),
        );

        let llm = Arc::new(ScriptedLlm::new(vec![
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "call_w".to_string(),
                    name: "create_branch".to_string(),
                    input: json!({"name": "exp"}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "Got it — leaving the graph as-is.".to_string(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));

        let agent = Agent::new(llm, registry, Arc::new(DenyAll));
        let req = AgentRequest {
            system: "sys".to_string(),
            history: vec![ChatMessage::user("create a branch please")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;

        assert_eq!(run.iterations(), 2);
        assert!(run.tool_calls_executed().is_empty()); // denied, never executed
        assert_eq!(run.tool_calls_rejected(), vec!["create_branch"]);
        assert_eq!(
            run.final_text().as_deref(),
            Some("Got it — leaving the graph as-is.")
        );
    }

    #[tokio::test]
    async fn run_executes_read_tools_without_approval_check() {
        // A custom gate that records every check call. The agent should
        // never invoke it for a read tool.
        struct RecordingGate {
            checks: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ApprovalGate for RecordingGate {
            async fn check(
                &self,
                tool_name: &str,
                _input: &serde_json::Value,
            ) -> ApprovalDecision {
                self.checks.lock().unwrap().push(tool_name.to_string());
                ApprovalDecision::Approved
            }
        }

        let checks = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(RecordingGate {
            checks: checks.clone(),
        });

        let registry = ToolRegistry::new().register_read(
            fixture_tool("search"),
            Arc::new(CapturingHandler {
                name: "search",
                captured: Arc::new(Mutex::new(Vec::new())),
                reply: "ok".to_string(),
                is_error: false,
            }),
        );

        let llm = Arc::new(ScriptedLlm::new(vec![
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "c".into(),
                    name: "search".into(),
                    input: json!({}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "done".into(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));

        let agent = Agent::new(llm, registry, gate);
        let req = AgentRequest {
            system: "sys".to_string(),
            history: vec![ChatMessage::user("look it up")],
            tool_choice: ToolChoice::Auto,
        };
        let _ = run_to_completion(&agent, req).await;

        assert!(
            checks.lock().unwrap().is_empty(),
            "approval gate must NOT be consulted for read tools"
        );
    }

    #[tokio::test]
    async fn run_handles_unknown_tool_with_error_result() {
        // Model calls a tool that isn't registered. The registry
        // produces an is_error result; the agent feeds that back to
        // the model so it can recover; the next response is plain text.
        let registry = ToolRegistry::new().register_read(
            fixture_tool("search"),
            Arc::new(CapturingHandler {
                name: "search",
                captured: Arc::new(Mutex::new(Vec::new())),
                reply: "".to_string(),
                is_error: false,
            }),
        );

        let llm = Arc::new(ScriptedLlm::new(vec![
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "c".into(),
                    name: "nonexistent".into(),
                    input: json!({}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "I don't have that tool — let me try search instead.".into(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));

        let agent = Agent::new(llm.clone(), registry, Arc::new(AutoApprove));
        let req = AgentRequest {
            system: "sys".to_string(),
            history: vec![ChatMessage::user("hi")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;

        // The unknown tool was "executed" via the registry's
        // ToolHandlerResult::error fallback, so it appears in the
        // executed list — but the result was an error.
        let finished_event = run
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolCallFinished { name, content, is_error, .. } => {
                    Some((name.as_str(), content.as_str(), *is_error))
                }
                _ => None,
            })
            .expect("expected a ToolCallFinished event");
        assert_eq!(finished_event.0, "nonexistent");
        assert!(finished_event.2, "must be is_error");
        assert!(finished_event.1.contains("no such tool"));
    }

    #[tokio::test]
    async fn run_caps_iterations_and_emits_error_event() {
        // LLM keeps asking for tools forever — agent must cap and
        // emit an error.
        let mut script = Vec::new();
        for _ in 0..20 {
            script.push(ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "c".into(),
                    name: "search".into(),
                    input: json!({}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            });
        }
        let llm = Arc::new(ScriptedLlm::new(script));
        let registry = ToolRegistry::new().register_read(
            fixture_tool("search"),
            Arc::new(CapturingHandler {
                name: "search",
                captured: Arc::new(Mutex::new(Vec::new())),
                reply: "data".to_string(),
                is_error: false,
            }),
        );

        let agent = Agent::with_config(
            llm,
            registry,
            Arc::new(AutoApprove),
            AgentConfig { max_iterations: 3 },
        );
        let req = AgentRequest {
            system: "sys".to_string(),
            history: vec![ChatMessage::user("loop forever")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;

        // Hit the ceiling at 3 iterations. Error event MUST be present.
        let err = run.first_error().expect("expected ceiling error");
        assert!(err.contains("iteration ceiling"));
        assert_eq!(run.iterations(), 3);
        assert_eq!(run.tool_calls_executed().len(), 3); // 3 dispatches
    }

    #[tokio::test]
    async fn run_propagates_llm_error_as_event_and_stops() {
        // Empty script → first call errors → loop bails immediately.
        let llm = Arc::new(ScriptedLlm::new(vec![]));
        let agent = Agent::new(llm, ToolRegistry::new(), Arc::new(AutoApprove));
        let req = AgentRequest {
            system: "sys".to_string(),
            history: vec![ChatMessage::user("hi")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;
        let err = run.first_error().expect("expected error");
        assert!(err.contains("LLM call failed"));
        // No Done event, so iterations() returns the default 0.
        assert_eq!(run.iterations(), 0);
    }

    #[tokio::test]
    async fn run_marks_truncated_text_with_error_and_terminates() {
        let llm = Arc::new(ScriptedLlm::new(vec![ToolUseResponse::Text {
            text: "This is a partial".to_string(),
            truncated: true,
            limits: empty_limits(),
        }]));
        let agent = Agent::new(llm, ToolRegistry::new(), Arc::new(AutoApprove));
        let req = AgentRequest {
            system: "sys".to_string(),
            history: vec![ChatMessage::user("write me something long")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;
        // Truncation produces an Error event, but the Done event
        // still fires with the partial text — the host can decide
        // whether to retry with smaller context.
        let err = run.first_error().expect("expected truncation error");
        assert!(err.contains("truncated"));
        assert_eq!(run.final_text().as_deref(), Some("This is a partial"));
    }

    #[tokio::test]
    async fn chat_turns_to_messages_translates_role_pairs() {
        let turns = vec![
            ChatTurn {
                role: ChatRole::User,
                content: "ping".to_string(),
            },
            ChatTurn {
                role: ChatRole::Assistant,
                content: "pong".to_string(),
            },
        ];
        let messages = chat_turns_to_messages(&turns);
        assert_eq!(messages.len(), 2);
        match (&messages[0], &messages[1]) {
            (ChatMessage::User(u), ChatMessage::AssistantText(a)) => {
                assert_eq!(u, "ping");
                assert_eq!(a, "pong");
            }
            other => panic!("unexpected mapping: {other:?}"),
        }
    }

    #[tokio::test]
    async fn final_text_and_first_error_helpers_extract_correct_events() {
        let events = vec![
            AgentEvent::Text {
                content: "hello".into(),
            },
            AgentEvent::Error {
                message: "warn".into(),
            },
            AgentEvent::Done {
                final_text: "hello world".into(),
                iterations: 1,
            },
        ];
        assert_eq!(final_text(&events), Some("hello world".to_string()));
        assert_eq!(first_error(&events), Some("warn".to_string()));
    }

    #[tokio::test]
    async fn tool_specs_returns_registry_specs() {
        let registry = ToolRegistry::new()
            .register_read(fixture_tool("a"), Arc::new(stub_handler()))
            .register_write(fixture_tool("b"), Arc::new(stub_handler()));
        let llm = Arc::new(ScriptedLlm::new(vec![]));
        let agent = Agent::new(llm, registry, Arc::new(AutoApprove));
        let mut names: Vec<String> = agent.tool_specs().into_iter().map(|t| t.name).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    fn stub_handler() -> CapturingHandler {
        CapturingHandler {
            name: "stub",
            captured: Arc::new(Mutex::new(Vec::new())),
            reply: String::new(),
            is_error: false,
        }
    }
}
