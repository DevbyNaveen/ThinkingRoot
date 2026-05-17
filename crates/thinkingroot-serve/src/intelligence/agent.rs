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
use thinkingroot_core::{Error, Result};
use thinkingroot_llm::llm::{
    ChatMessage, LlmClient, Tool, ToolCall, ToolChoice, ToolResult, ToolUseResponse,
};
use tokio::sync::mpsc;

use crate::intelligence::approval::{ApprovalDecision, ApprovalGate};
use crate::intelligence::synthesizer::{ChatRole, ChatTurn};
use crate::intelligence::token_budget::{DEFAULT_TOOL_RESULT_TOKEN_BUDGET, truncate_tool_result};
use crate::intelligence::tools::ToolRegistry;
use crate::intelligence::trace::{SharedTraceLog, event_to_trace};

/// Where the agent loop pushes its events. Two production transports:
///
///   * `Buf` — append to a `Vec<AgentEvent>`. Used by `run_collected`
///     and tests; everything is collected before the caller sees it.
///   * `Channel` — send into an `mpsc::Sender<AgentEvent>`. Used by
///     the streaming HTTP / Tauri path so the desktop sees each event
///     the moment the agent emits it.
///
/// Both transports surface the same observable behaviour — the agent
/// loop is unaware of which is in use.
pub enum EventSink<'a> {
    Buf(&'a mut Vec<AgentEvent>),
    Channel(&'a mpsc::Sender<AgentEvent>),
}

impl EventSink<'_> {
    async fn push(&mut self, event: AgentEvent) {
        match self {
            EventSink::Buf(v) => v.push(event),
            EventSink::Channel(tx) => {
                // Receiver dropped just means the consumer has gone
                // away (e.g. SSE client disconnected). The agent
                // can't recover from that — stop emitting and let
                // the loop wind down naturally.
                let _ = tx.send(event).await;
            }
        }
    }
}

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

/// Refresh the system prompt at the top of each agent iteration.
///
/// The streaming REST path (rest.rs::agent_stream_response) supplies a
/// refresher so the workspace identity block (`<system-reminder>`
/// claim_count, source_kinds, today, project_doc) stays current across
/// long agent runs — particularly when a `compile` lands mid-stream
/// and the snapshot we captured at request entry is no longer accurate.
/// Tests and CLI flows that don't need refresh pass `None` and the
/// agent reuses `req.system` unchanged each iteration. Cheap by design:
/// the typical implementation reads from `engine.workspace_chat_snapshot`
/// which is an in-memory cache. (C5 fix, plan 2026-05-09.)
#[async_trait]
pub trait SystemPromptRefresher: Send + Sync {
    /// Return the system prompt to use for the next LLM call. The
    /// returned `String` replaces `req.system` for that single
    /// iteration. `iteration` is 1-based.
    async fn refresh(&self, iteration: usize) -> String;
}

/// Inputs to one agent run.
#[derive(Clone)]
pub struct AgentRequest {
    /// System prompt to pass to every `chat_with_tools` call.
    /// Used unchanged when `system_refresher` is `None`. When a
    /// refresher is set, this string is the fallback returned if
    /// the refresher panics or errors.
    pub system: String,
    /// Optional per-iteration refresher. When set, called at the
    /// top of every iteration to re-render the system prompt with
    /// fresh ambient context (workspace identity, branch state,
    /// reactive `<system-reminder>` blocks). When `None`, the agent
    /// reuses `system` unchanged across iterations — the right
    /// default for tests, CLI flows, and any caller that doesn't
    /// care about within-conversation drift.
    pub system_refresher: Option<Arc<dyn SystemPromptRefresher>>,
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

impl std::fmt::Debug for AgentRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRequest")
            .field("system", &self.system)
            .field(
                "system_refresher",
                &self.system_refresher.as_ref().map(|_| "<refresher>"),
            )
            .field("history", &self.history)
            .field("tool_choice", &self.tool_choice)
            .finish()
    }
}

/// Configuration knobs for the agent. Defaults match the safe
/// production setpoint: 8 iterations max, AutoApprove disabled
/// (caller MUST supply a gate), no parallel dispatch.
///
/// Phase E.7 (2026-05-17): adds same-tool-same-args loop detection on
/// top of the existing hard `max_iterations` cap. When the model
/// proposes the same `(tool_name, canonical_args_hash)` ≥ `threshold`
/// times within the trailing `window`-call buffer, the loop halts
/// with a forced-summary turn — saves the user's tokens vs. burning
/// through `max_iterations` on a stuck model. ON by default; tunable.
pub struct AgentConfig {
    /// Maximum LLM round-trips per `run`. Hitting the ceiling causes
    /// the loop to terminate with whatever text has accumulated and
    /// emit an `Error` event noting the cause.
    pub max_iterations: usize,
    /// Enable same-tool-same-args loop detection. Default `true`.
    /// Power users (CLI flows that intentionally retry, evaluator
    /// scripts) can disable this. When disabled only `max_iterations`
    /// guards against runaway loops.
    pub loop_detection: bool,
    /// Size of the trailing ring buffer (in tool-call entries) over
    /// which repetition is counted. Default 10.
    pub loop_detection_window: usize,
    /// Number of identical `(tool_name, canonical_args_hash)` entries
    /// in the window that triggers the halt. Default 3 — two repeats
    /// of the same call are common in legitimate multi-step flows
    /// (a planner retry after a tool-error, or fetching adjacent
    /// rows); three is solid evidence of a stuck model.
    pub loop_detection_threshold: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 8,
            loop_detection: true,
            loop_detection_window: 10,
            loop_detection_threshold: 3,
        }
    }
}

/// The agent. Cheap to clone — every field is an `Arc` or a
/// reference-counted [`ToolRegistry`].
#[derive(Clone)]
pub struct Agent {
    llm: Arc<dyn LlmBackend>,
    registry: ToolRegistry,
    approval: Arc<dyn ApprovalGate>,
    /// Optional signed trace log. When set, every [`AgentEvent`] is
    /// also appended (kind + payload) to the log. The log writes
    /// asynchronously; failures are logged via `tracing::warn` and
    /// do NOT abort the agent — an audit log being unreachable
    /// shouldn't kill a live conversation.
    trace_log: Option<SharedTraceLog>,
    max_iterations: usize,
    loop_detection: bool,
    loop_detection_window: usize,
    loop_detection_threshold: usize,
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
            trace_log: None,
            max_iterations: config.max_iterations,
            loop_detection: config.loop_detection,
            loop_detection_window: config.loop_detection_window.max(1),
            loop_detection_threshold: config.loop_detection_threshold.max(2),
        }
    }

    /// Builder-style setter to attach a [`SharedTraceLog`]. Pass an
    /// `InMemoryTraceLog` for tests, a `FileTraceLog` for production.
    pub fn with_trace_log(mut self, trace_log: SharedTraceLog) -> Self {
        self.trace_log = Some(trace_log);
        self
    }

    /// Tools registered for this agent. Surfaced for the synthesizer /
    /// REST layer that wants to show users which capabilities the
    /// agent has.
    pub fn tool_specs(&self) -> Vec<Tool> {
        self.registry.specs()
    }

    /// Run the loop, collecting every event into a `Vec`. Convenient
    /// for tests and CLI surfaces that don't need streaming.
    pub async fn run_collected(&self, req: AgentRequest) -> Vec<AgentEvent> {
        let mut events: Vec<AgentEvent> = Vec::new();
        self.run_into(req, &mut events).await;
        events
    }

    /// Run the loop, pushing every event into the supplied `Vec`.
    /// Equivalent to `run_collected` but lets the caller pre-allocate
    /// or post-process the buffer.
    pub async fn run_into(&self, req: AgentRequest, out: &mut Vec<AgentEvent>) {
        let mut sink = EventSink::Buf(out);
        self.drive(req, &mut sink).await;
    }

    /// Run the loop, sending every event into the mpsc channel as
    /// soon as it is emitted. Used by the HTTP / Tauri streaming
    /// path so the desktop sees `ToolCallProposed` /
    /// `ToolCallExecuting` etc. live, not at the end.
    ///
    /// Returns once the agent terminates. The caller is responsible
    /// for closing the channel (drop the `Sender`) when the
    /// conversation ends.
    pub async fn run_streaming(&self, req: AgentRequest, tx: mpsc::Sender<AgentEvent>) {
        let mut sink = EventSink::Channel(&tx);
        self.drive(req, &mut sink).await;
    }

    /// The actual loop. Shared between `run_into` and `run_streaming`
    /// via the `EventSink` abstraction; a single source of truth so
    /// the two transports can never diverge in observable behaviour.
    async fn drive(&self, req: AgentRequest, sink: &mut EventSink<'_>) {
        let tools = self.registry.specs();
        let mut history = req.history;
        let mut iterations: usize = 0;
        let mut accumulated_text = String::new();
        // First call uses the caller-supplied tool_choice; subsequent
        // calls always use `Auto` because forcing a tool on a
        // post-results turn would loop forever.
        let mut tool_choice = req.tool_choice.clone();

        // Phase E.7 (2026-05-17) — same-tool-same-args ring buffer.
        // Each entry is `(tool_name, canonical_args_hash)`. New
        // proposed calls are checked against the buffer BEFORE
        // dispatch so a confirmed loop saves both tool execution
        // and the next LLM round-trip. Cap: `loop_detection_window`.
        let mut tool_call_ring: Vec<(String, [u8; 32])> = Vec::new();

        while iterations < self.max_iterations {
            iterations += 1;

            // C5: refresh system prompt at the top of each iteration
            // when a refresher is configured. Keeps workspace identity
            // (claim_count, source_kinds, today) current across long
            // agent runs and post-mid-stream-compile scenarios. Static
            // `req.system` is the fallback when no refresher is wired.
            let current_system: String = match &req.system_refresher {
                Some(refresher) => refresher.refresh(iterations).await,
                None => req.system.clone(),
            };

            let response = match self
                .llm
                .chat_with_tools(&current_system, &history, &tools, &tool_choice)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    self.emit(
                        sink,
                        AgentEvent::Error {
                            message: format!("LLM call failed on iteration {iterations}: {e}"),
                        },
                    )
                    .await;
                    return;
                }
            };

            match response {
                ToolUseResponse::Text {
                    text, truncated, ..
                } => {
                    if !text.is_empty() {
                        accumulated_text.push_str(&text);
                        self.emit(
                            sink,
                            AgentEvent::Text {
                                content: text.clone(),
                            },
                        )
                        .await;
                    }
                    if truncated {
                        self.emit(
                            sink,
                            AgentEvent::Error {
                                message: format!(
                                    "model output truncated at iteration {iterations} \
                                     (hit max_tokens)"
                                ),
                            },
                        )
                        .await;
                    }
                    self.emit(
                        sink,
                        AgentEvent::Done {
                            final_text: accumulated_text,
                            iterations,
                        },
                    )
                    .await;
                    return;
                }
                ToolUseResponse::ToolCalls {
                    calls,
                    text_preamble,
                    ..
                } => {
                    if !text_preamble.is_empty() {
                        accumulated_text.push_str(&text_preamble);
                        self.emit(
                            sink,
                            AgentEvent::Text {
                                content: text_preamble.clone(),
                            },
                        )
                        .await;
                    }

                    // E.7 loop detection — check BEFORE dispatch so a
                    // confirmed loop saves the tool execution AND the
                    // next LLM round-trip. We treat any single call in
                    // the proposed batch matching the threshold as
                    // sufficient to halt: parallel-batches of the same
                    // (name, args) are unambiguously a loop, and even
                    // a single repeat-call is enough when the prior
                    // (threshold-1) hits are already in the buffer.
                    if self.loop_detection {
                        let triggering = calls.iter().find_map(|c| {
                            let h = canonical_args_hash(&c.input);
                            let prior = tool_call_ring
                                .iter()
                                .filter(|(n, hh)| n == &c.name && hh == &h)
                                .count();
                            if prior + 1 >= self.loop_detection_threshold {
                                Some((c.name.clone(), prior + 1))
                            } else {
                                None
                            }
                        });
                        if let Some((name, count)) = triggering {
                            self.emit(
                                sink,
                                AgentEvent::Error {
                                    message: format!(
                                        "loop detected: tool `{name}` called {count} times \
                                         with identical args within last {} calls. \
                                         Halting before dispatch to save tokens.",
                                        self.loop_detection_window
                                    ),
                                },
                            )
                            .await;
                            if !accumulated_text.is_empty() {
                                accumulated_text.push_str("\n\n");
                            }
                            accumulated_text.push_str(&format!(
                                "[Halted: agent was calling `{name}` repeatedly with the \
                                 same arguments — likely stuck. Partial progress preserved \
                                 above.]"
                            ));
                            self.emit(
                                sink,
                                AgentEvent::Done {
                                    final_text: accumulated_text,
                                    iterations,
                                },
                            )
                            .await;
                            return;
                        }
                        // Record this batch into the ring buffer.
                        for c in &calls {
                            tool_call_ring.push((c.name.clone(), canonical_args_hash(&c.input)));
                        }
                        // Trim to window — keep the most recent.
                        let len = tool_call_ring.len();
                        if len > self.loop_detection_window {
                            tool_call_ring.drain(0..(len - self.loop_detection_window));
                        }
                    }

                    // Append the assistant's tool_use turn so the
                    // next call sees the conversation in shape.
                    history.push(ChatMessage::AssistantToolCalls(calls.clone()));
                    let results = self.dispatch_calls(&calls, sink).await;
                    history.push(ChatMessage::ToolResults(results));
                    // Subsequent iterations always use Auto: forcing
                    // tools again would create an infinite loop.
                    tool_choice = ToolChoice::Auto;
                }
            }
        }

        // Fell off the iteration ceiling.
        self.emit(
            sink,
            AgentEvent::Error {
                message: format!(
                    "agent stopped at iteration ceiling ({}). Partial text length: {}",
                    self.max_iterations,
                    accumulated_text.len()
                ),
            },
        )
        .await;
        self.emit(
            sink,
            AgentEvent::Done {
                final_text: accumulated_text,
                iterations,
            },
        )
        .await;
    }

    /// Emit one event: push through the [`EventSink`] AND, when a
    /// trace log is configured, record the event in the signed trace.
    /// Trace-write failures are logged via `tracing::warn` rather
    /// than crashing the conversation — an unreachable audit log
    /// shouldn't break a live chat.
    async fn emit(&self, sink: &mut EventSink<'_>, event: AgentEvent) {
        if let Some(log) = &self.trace_log {
            let (kind, payload) = event_to_trace(&event);
            if let Err(e) = log.append(kind, payload).await {
                tracing::warn!("agent trace log append failed: {e}");
            }
        }
        sink.push(event).await;
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
        sink: &mut EventSink<'_>,
    ) -> Vec<ToolResult> {
        let mut results: Vec<ToolResult> = Vec::with_capacity(calls.len());
        for call in calls {
            let is_write = self.registry.is_write(&call.name);
            self.emit(
                sink,
                AgentEvent::ToolCallProposed {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                    is_write,
                },
            )
            .await;

            if is_write {
                let decision = self.approval.check(&call.name, &call.input).await;
                if let ApprovalDecision::Rejected { reason } = decision {
                    self.emit(
                        sink,
                        AgentEvent::ToolCallRejected {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            reason: reason.clone(),
                        },
                    )
                    .await;
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

            self.emit(
                sink,
                AgentEvent::ToolCallExecuting {
                    id: call.id.clone(),
                    name: call.name.clone(),
                },
            )
            .await;
            let res = self.registry.dispatch(&call.name, call.input.clone()).await;
            // C6: bound the per-result content so a single oversized
            // tool output (large file read, 50-hit search) cannot
            // starve subsequent iterations of context. The full
            // result is still emitted to the UI/trace via
            // ToolCallFinished — only the LLM-facing history copy is
            // truncated. (plan 2026-05-09)
            let bounded_content =
                truncate_tool_result(res.content.clone(), DEFAULT_TOOL_RESULT_TOKEN_BUDGET);
            self.emit(
                sink,
                AgentEvent::ToolCallFinished {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    content: res.content,
                    is_error: res.is_error,
                },
            )
            .await;
            results.push(ToolResult {
                tool_use_id: call.id.clone(),
                content: bounded_content,
                is_error: res.is_error,
            });
        }
        results
    }
}

/// E.7 loop detection — hash a tool's `input` Value to a canonical
/// 32-byte BLAKE3 digest that is order-independent over object keys.
///
/// `serde_json::Map` preserves insertion order by default; two
/// identical conceptual JSON objects can serialise to different
/// byte strings if they were constructed from differently-ordered
/// sources. We can't use raw `to_vec` for the loop-detection key.
///
/// Strategy: domain-tag each variant (1 byte), length-prefix
/// variable-size payloads, sort object keys lexicographically
/// before hashing.
fn canonical_args_hash(v: &serde_json::Value) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    canonical_args_feed(&mut h, v);
    *h.finalize().as_bytes()
}

fn canonical_args_feed(h: &mut blake3::Hasher, v: &serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Null => {
            h.update(&[0u8]);
        }
        Value::Bool(b) => {
            h.update(&[1u8, if *b { 1 } else { 0 }]);
        }
        Value::Number(n) => {
            h.update(&[2u8]);
            // Use the canonical decimal representation rather than
            // f64::to_le_bytes — JSON numbers may be ints OR floats
            // and round-trip stability of the textual form is what
            // matters for "same args" identity.
            let s = n.to_string();
            h.update(&(s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        }
        Value::String(s) => {
            h.update(&[3u8]);
            h.update(&(s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        }
        Value::Array(items) => {
            h.update(&[4u8]);
            h.update(&(items.len() as u64).to_le_bytes());
            for it in items {
                canonical_args_feed(h, it);
            }
        }
        Value::Object(map) => {
            h.update(&[5u8]);
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            h.update(&(keys.len() as u64).to_le_bytes());
            for k in keys {
                h.update(&(k.len() as u64).to_le_bytes());
                h.update(k.as_bytes());
                canonical_args_feed(h, &map[k]);
            }
        }
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
    use thinkingroot_llm::scheduler::HeaderRateLimits;

    /// Stub LLM backend that replays a fixed script of responses.
    /// Each `chat_with_tools` call pops the next scripted response.
    /// Used to assert the agent loop's behaviour for both terminal
    /// text replies and multi-iteration tool-use sequences without
    /// touching a real provider.
    struct ScriptedLlm {
        script: Mutex<Vec<ToolUseResponse>>,
        calls_seen: Mutex<Vec<Vec<ChatMessage>>>,
        systems_seen: Mutex<Vec<String>>,
    }

    impl ScriptedLlm {
        fn new(script: Vec<ToolUseResponse>) -> Self {
            Self {
                script: Mutex::new(script),
                calls_seen: Mutex::new(Vec::new()),
                systems_seen: Mutex::new(Vec::new()),
            }
        }

        fn calls_seen(&self) -> Vec<Vec<ChatMessage>> {
            self.calls_seen.lock().unwrap().clone()
        }

        /// Every system-prompt string this LLM saw, in call order.
        /// Used by the C5 refresher test (`agent_calls_system_refresher_per_iteration`).
        fn systems_seen(&self) -> Vec<String> {
            self.systems_seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmBackend for ScriptedLlm {
        async fn chat_with_tools(
            &self,
            system: &str,
            messages: &[ChatMessage],
            _tools: &[Tool],
            _tool_choice: &ToolChoice,
        ) -> Result<ToolUseResponse> {
            self.calls_seen.lock().unwrap().push(messages.to_vec());
            self.systems_seen.lock().unwrap().push(system.to_string());
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

    /// Test refresher that emits a unique string per iteration so the
    /// C5 regression test can assert the agent loop calls
    /// [`SystemPromptRefresher::refresh`] at the top of every turn.
    /// Captures every iteration number it was asked about so the test
    /// can verify `iteration` is 1-based and monotonic.
    struct SeqRefresher {
        prefix: String,
        seen: Arc<Mutex<Vec<usize>>>,
    }

    impl SeqRefresher {
        fn new(prefix: &str) -> (Arc<Self>, Arc<Mutex<Vec<usize>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let r = Arc::new(Self {
                prefix: prefix.to_string(),
                seen: seen.clone(),
            });
            (r, seen)
        }
    }

    #[async_trait]
    impl SystemPromptRefresher for SeqRefresher {
        async fn refresh(&self, iteration: usize) -> String {
            self.seen.lock().unwrap().push(iteration);
            format!("{}-iter-{iteration}", self.prefix)
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
            system_refresher: None,
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
            system_refresher: None,
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
            system_refresher: None,
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
            async fn check(&self, tool_name: &str, _input: &serde_json::Value) -> ApprovalDecision {
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
            system_refresher: None,
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
            system_refresher: None,
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
                AgentEvent::ToolCallFinished {
                    name,
                    content,
                    is_error,
                    ..
                } => Some((name.as_str(), content.as_str(), *is_error)),
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
            AgentConfig {
                max_iterations: 3,
                // This test specifically exercises the iteration-ceiling
                // path with identical-args repeats; with E.7's default
                // detector ON, the halt would fire before the ceiling.
                // Disable the detector here to keep this assertion
                // pointed at the ceiling code path.
                loop_detection: false,
                ..AgentConfig::default()
            },
        );
        let req = AgentRequest {
            system: "sys".to_string(),
            system_refresher: None,
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
            system_refresher: None,
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
            system_refresher: None,
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

    // ─────────────────────────────────────────────────────────────────
    // S4 — trace log integration
    // ─────────────────────────────────────────────────────────────────

    use crate::intelligence::trace::{InMemoryTraceLog, kind, verify_chain};

    #[tokio::test]
    async fn agent_writes_signed_trace_for_text_only_run() {
        let llm = Arc::new(ScriptedLlm::new(vec![ToolUseResponse::Text {
            text: "Three providers.".to_string(),
            truncated: false,
            limits: empty_limits(),
        }]));
        let trace = Arc::new(InMemoryTraceLog::new());
        let agent = Agent::new(llm, ToolRegistry::new(), Arc::new(AutoApprove))
            .with_trace_log(trace.clone());

        let req = AgentRequest {
            system: "sys".to_string(),
            system_refresher: None,
            history: vec![ChatMessage::user("how many providers")],
            tool_choice: ToolChoice::Auto,
        };
        let _ = agent.run_collected(req).await;

        let entries = trace.entries().await;
        // Two events for a clean text-only run: Text + Done.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, kind::AGENT_TEXT);
        assert_eq!(entries[1].kind, kind::AGENT_RUN_DONE);
        verify_chain(&entries).expect("trace must verify");
    }

    #[tokio::test]
    async fn agent_writes_signed_trace_for_full_tool_call_round_trip() {
        let registry = ToolRegistry::new().register_read(
            fixture_tool("search"),
            Arc::new(CapturingHandler {
                name: "search",
                captured: Arc::new(Mutex::new(Vec::new())),
                reply: "hit".to_string(),
                is_error: false,
            }),
        );
        let llm = Arc::new(ScriptedLlm::new(vec![
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "c1".to_string(),
                    name: "search".to_string(),
                    input: serde_json::json!({"q": "x"}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "Done.".to_string(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));
        let trace = Arc::new(InMemoryTraceLog::new());
        let agent = Agent::new(llm, registry, Arc::new(AutoApprove)).with_trace_log(trace.clone());

        let req = AgentRequest {
            system: "sys".to_string(),
            system_refresher: None,
            history: vec![ChatMessage::user("look it up")],
            tool_choice: ToolChoice::Auto,
        };
        let _ = agent.run_collected(req).await;

        let entries = trace.entries().await;
        // Sequence: ToolCallProposed → ToolCallExecuting → ToolCallFinished
        // → Text → Done.
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].kind, kind::AGENT_TOOL_PROPOSED);
        assert_eq!(entries[1].kind, kind::AGENT_TOOL_EXECUTING);
        assert_eq!(entries[2].kind, kind::AGENT_TOOL_FINISHED);
        assert_eq!(entries[3].kind, kind::AGENT_TEXT);
        assert_eq!(entries[4].kind, kind::AGENT_RUN_DONE);
        verify_chain(&entries).expect("full-flow trace must verify");
    }

    #[tokio::test]
    async fn agent_traces_rejection_for_write_tool() {
        let registry = ToolRegistry::new().register_write(
            fixture_tool("create_branch"),
            Arc::new(CapturingHandler {
                name: "create_branch",
                captured: Arc::new(Mutex::new(Vec::new())),
                reply: "should not run".to_string(),
                is_error: false,
            }),
        );
        let llm = Arc::new(ScriptedLlm::new(vec![
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "w".to_string(),
                    name: "create_branch".to_string(),
                    input: serde_json::json!({"name": "exp"}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "Got it.".to_string(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));
        let trace = Arc::new(InMemoryTraceLog::new());
        let agent = Agent::new(llm, registry, Arc::new(DenyAll)).with_trace_log(trace.clone());

        let req = AgentRequest {
            system: "sys".to_string(),
            system_refresher: None,
            history: vec![ChatMessage::user("create one")],
            tool_choice: ToolChoice::Auto,
        };
        let _ = agent.run_collected(req).await;

        let entries = trace.entries().await;
        // Proposed → Rejected → Text → Done. No Executing / Finished.
        let kinds: Vec<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&kind::AGENT_TOOL_PROPOSED));
        assert!(kinds.contains(&kind::AGENT_TOOL_REJECTED));
        assert!(!kinds.contains(&kind::AGENT_TOOL_EXECUTING));
        assert!(!kinds.contains(&kind::AGENT_TOOL_FINISHED));
        verify_chain(&entries).expect("rejection trace must verify");
    }

    #[tokio::test]
    async fn agent_with_no_trace_log_still_works() {
        // Sanity: omitting with_trace_log keeps every behaviour identical.
        let llm = Arc::new(ScriptedLlm::new(vec![ToolUseResponse::Text {
            text: "ok".to_string(),
            truncated: false,
            limits: empty_limits(),
        }]));
        let agent = Agent::new(llm, ToolRegistry::new(), Arc::new(AutoApprove));
        let req = AgentRequest {
            system: "sys".to_string(),
            system_refresher: None,
            history: vec![ChatMessage::user("hi")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;
        assert_eq!(run.final_text().as_deref(), Some("ok"));
        assert!(run.first_error().is_none());
    }

    // ─────────────────────────────────────────────────────────────────
    // C5 regression: refresh WorkspaceIdentity per agent iteration
    // (Task 2, plan 2026-05-09). Mocks an LLM that runs three iterations
    // (tool, tool, text) and a refresher that returns a unique string
    // per iteration. Asserts every chat_with_tools call received the
    // refresher's output instead of the static `req.system` fallback,
    // and that iteration numbers are 1-based + monotonic.
    // ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_calls_system_refresher_per_iteration() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let registry = ToolRegistry::new().register_read(
            fixture_tool("search"),
            Arc::new(CapturingHandler {
                name: "search",
                captured: captured.clone(),
                reply: "no results".to_string(),
                is_error: false,
            }),
        );

        // Iter 1+2: tool call. Iter 3: terminal text.
        let llm = Arc::new(ScriptedLlm::new(vec![
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "t1".to_string(),
                    name: "search".to_string(),
                    input: json!({"q": "auth"}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "t2".to_string(),
                    name: "search".to_string(),
                    input: json!({"q": "auth bug"}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "Looked into it.".to_string(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));

        let agent = Agent::new(llm.clone(), registry, Arc::new(AutoApprove));
        let (refresher, refresh_calls) = SeqRefresher::new("ws-pulse");

        let req = AgentRequest {
            system: "stale fallback that must NOT appear".to_string(),
            system_refresher: Some(refresher),
            history: vec![ChatMessage::user("fix the auth bug")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;

        // Sanity: agent ran three iterations and finished cleanly.
        assert_eq!(run.iterations(), 3);
        assert_eq!(run.final_text().as_deref(), Some("Looked into it."));

        // The refresher was called once per iteration, in order, 1-based.
        let seen = refresh_calls.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![1, 2, 3],
            "refresher must be called exactly once per iteration, 1-based"
        );

        // Every LLM call received the refreshed system prompt — not
        // the stale fallback. C5 contract: an out-of-date
        // `WorkspaceIdentity` snapshot from the request entry must not
        // leak into post-iteration-1 LLM calls.
        let systems = llm.systems_seen();
        assert_eq!(
            systems,
            vec![
                "ws-pulse-iter-1".to_string(),
                "ws-pulse-iter-2".to_string(),
                "ws-pulse-iter-3".to_string(),
            ],
            "each chat_with_tools call must use the refresher's output, \
             not the static AgentRequest.system fallback"
        );
        assert!(
            !systems.iter().any(|s| s.contains("stale fallback")),
            "static fallback must never reach the LLM when a refresher is set"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // C6 regression: token budget on agent loop tool results
    // (Task 3, plan 2026-05-09). Mocks a tool returning a 50K-byte
    // payload (~12K tokens) and asserts the next iteration's
    // ToolResults message is truncated below the default 2K-token
    // budget — while the UI-facing ToolCallFinished event still
    // carries the full content (so trace logs and claim cards stay
    // accurate).
    // ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_calls_truncates_oversized_tool_results_for_llm_only() {
        use crate::intelligence::token_budget::DEFAULT_TOOL_RESULT_TOKEN_BUDGET;

        // 50,000 bytes ≈ 12,500 tokens at 4-cpt. Well over the 2,048
        // default budget. Distinct head/tail markers let us verify
        // the truncation preserves boundary signal.
        let huge_body = "Z".repeat(50_000);
        let huge_payload = format!("HEAD-MARK{huge_body}TAIL-MARK");

        let captured = Arc::new(Mutex::new(Vec::new()));
        let registry = ToolRegistry::new().register_read(
            fixture_tool("read_file"),
            Arc::new(CapturingHandler {
                name: "read_file",
                captured: captured.clone(),
                reply: huge_payload.clone(),
                is_error: false,
            }),
        );

        // Iter 1: tool call. Iter 2: terminal text.
        let llm = Arc::new(ScriptedLlm::new(vec![
            ToolUseResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "rf1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "big.txt"}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "Read it.".to_string(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));

        let agent = Agent::new(llm.clone(), registry, Arc::new(AutoApprove));
        let req = AgentRequest {
            system: "sys".to_string(),
            system_refresher: None,
            history: vec![ChatMessage::user("read it")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;
        assert_eq!(run.iterations(), 2);

        // The LLM history on iteration 2 must contain the truncation
        // marker (the bounded copy), not the raw 50K-byte payload.
        let calls = llm.calls_seen();
        assert_eq!(calls.len(), 2, "expected two LLM calls (tool, then text)");
        let iter2_history = &calls[1];
        let tool_results_msg = iter2_history
            .iter()
            .find_map(|m| match m {
                ChatMessage::ToolResults(r) => Some(r),
                _ => None,
            })
            .expect("iteration 2 must include ToolResults");
        assert_eq!(tool_results_msg.len(), 1);
        let bounded = &tool_results_msg[0].content;
        assert!(
            bounded.contains("truncated for token budget"),
            "LLM-facing tool result must carry the truncation marker"
        );
        assert!(
            bounded.len() < huge_payload.len(),
            "LLM-facing tool result must be smaller than the raw payload \
             ({} bytes vs {} raw)",
            bounded.len(),
            huge_payload.len()
        );
        // Defense-in-depth: ensure the budget was actually respected.
        let est = bounded.len() / 4;
        assert!(
            est <= DEFAULT_TOOL_RESULT_TOKEN_BUDGET * 2,
            "bounded result still {est} tokens — over 2× budget"
        );

        // The UI-facing ToolCallFinished event must carry the FULL
        // content (so trace logs, claim cards, and the verifier see
        // exactly what the tool produced). CapturingHandler decorates
        // its reply as `{name}:{reply}` (line ~669), so the expected
        // event content is the full prefixed string.
        let expected_full = format!("read_file:{huge_payload}");
        let finished = run
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolCallFinished { content, .. } => Some(content),
                _ => None,
            })
            .expect("ToolCallFinished must be emitted");
        assert_eq!(
            finished, &expected_full,
            "UI-facing event must keep the FULL tool result \
             (truncation only applies to the LLM-history copy)"
        );
        assert!(
            finished.len() > bounded.len(),
            "full UI content ({}) must be larger than bounded LLM copy ({})",
            finished.len(),
            bounded.len()
        );
    }

    /// Without a refresher, the agent reuses `req.system` for every
    /// iteration — preserving the existing contract for tests, CLI
    /// flows, and any caller that doesn't need within-conversation
    /// freshness. Pin this so a future "refresh by default" change
    /// doesn't silently break LongMemEval / single-turn callers.
    #[tokio::test]
    async fn agent_without_refresher_reuses_static_system_each_iteration() {
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
                    id: "t1".to_string(),
                    name: "search".to_string(),
                    input: json!({"q": "x"}),
                }],
                text_preamble: String::new(),
                limits: empty_limits(),
            },
            ToolUseResponse::Text {
                text: "done".to_string(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));

        let agent = Agent::new(llm.clone(), registry, Arc::new(AutoApprove));
        let req = AgentRequest {
            system: "static-sys".to_string(),
            system_refresher: None,
            history: vec![ChatMessage::user("go")],
            tool_choice: ToolChoice::Auto,
        };
        let _ = run_to_completion(&agent, req).await;

        let systems = llm.systems_seen();
        assert_eq!(
            systems,
            vec!["static-sys".to_string(), "static-sys".to_string()],
            "without a refresher, every iteration must see the same \
             AgentRequest.system string byte-identical"
        );
    }

    // ─── Phase E.7 loop-detection tests ────────────────────────────────

    fn agent_with_loop_detection(
        llm: Arc<dyn LlmBackend>,
        registry: ToolRegistry,
        threshold: usize,
        window: usize,
    ) -> Agent {
        Agent::with_config(
            llm,
            registry,
            Arc::new(AutoApprove),
            AgentConfig {
                max_iterations: 16,
                loop_detection: true,
                loop_detection_window: window,
                loop_detection_threshold: threshold,
            },
        )
    }

    fn search_call(id: &str, q: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "search".to_string(),
            input: json!({ "query": q }),
        }
    }

    fn tool_calls(calls: Vec<ToolCall>) -> ToolUseResponse {
        ToolUseResponse::ToolCalls {
            calls,
            text_preamble: String::new(),
            limits: empty_limits(),
        }
    }

    #[test]
    fn canonical_args_hash_is_key_order_independent() {
        let a = json!({ "alpha": 1, "beta": "x", "gamma": [1, 2, 3] });
        let b = json!({ "gamma": [1, 2, 3], "beta": "x", "alpha": 1 });
        let c = json!({ "alpha": 1, "beta": "x", "gamma": [1, 2, 4] }); // different
        let ha = canonical_args_hash(&a);
        let hb = canonical_args_hash(&b);
        let hc = canonical_args_hash(&c);
        assert_eq!(ha, hb, "object key order must not affect canonical hash");
        assert_ne!(ha, hc, "different array element must change hash");
    }

    #[test]
    fn canonical_args_hash_distinguishes_null_zero_false() {
        // Different JSON types that all serialise to short tokens must
        // remain distinguishable — otherwise loop detection would
        // false-trigger on incidental coincidences.
        let n = canonical_args_hash(&json!(null));
        let z = canonical_args_hash(&json!(0));
        let f = canonical_args_hash(&json!(false));
        let empty_str = canonical_args_hash(&json!(""));
        let empty_obj = canonical_args_hash(&json!({}));
        let empty_arr = canonical_args_hash(&json!([]));
        let all = [n, z, f, empty_str, empty_obj, empty_arr];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "domain tags must distinguish empty/null/zero values (i={i}, j={j})");
            }
        }
    }

    #[tokio::test]
    async fn loop_detection_halts_at_threshold_with_identical_args() {
        // 3× same call → at the 3rd proposal the loop must halt
        // BEFORE the third dispatch (so dispatch ran twice, not three).
        let llm = Arc::new(ScriptedLlm::new(vec![
            tool_calls(vec![search_call("a1", "foo")]),
            tool_calls(vec![search_call("a2", "foo")]),
            tool_calls(vec![search_call("a3", "foo")]),
            // safety net — if loop-detect doesn't fire, this run
            // would eventually exhaust the script and Error out.
            ToolUseResponse::Text {
                text: "should not reach".into(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(CapturingHandler {
            name: "search",
            captured: captured.clone(),
            reply: "ok".to_string(),
            is_error: false,
        });
        let registry = ToolRegistry::new().register_read(fixture_tool("search"), handler);

        let agent = agent_with_loop_detection(llm, registry, 3, 10);
        let req = AgentRequest {
            system: "sys".into(),
            system_refresher: None,
            history: vec![ChatMessage::user("look")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;

        // The 3rd `search` proposal triggered the halt — so only
        // 2 dispatches reached the handler.
        assert_eq!(
            captured.lock().unwrap().len(),
            2,
            "dispatch must be skipped at the threshold-hitting call"
        );

        // Wire: must emit both an Error explaining the halt AND a
        // Done with the preserved partial text. Caller-side parsers
        // (HTTP SSE, Tauri ChatView) treat Done as the terminator.
        let err = run.first_error().expect("expected loop-detected error");
        assert!(
            err.contains("loop detected") && err.contains("search"),
            "error message should name the looping tool: {err}"
        );
        let done = run.final_text().expect("expected Done event");
        assert!(
            done.contains("Halted") && done.contains("search"),
            "final text should explain the halt: {done}"
        );
    }

    #[tokio::test]
    async fn loop_detection_does_not_fire_on_different_args() {
        // 3 calls to the same tool but with different args must not
        // trigger the halt — only `(name, args_hash)` repetition counts.
        let llm = Arc::new(ScriptedLlm::new(vec![
            tool_calls(vec![search_call("a1", "foo")]),
            tool_calls(vec![search_call("a2", "bar")]),
            tool_calls(vec![search_call("a3", "baz")]),
            ToolUseResponse::Text {
                text: "final summary".into(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(CapturingHandler {
            name: "search",
            captured: captured.clone(),
            reply: "ok".to_string(),
            is_error: false,
        });
        let registry = ToolRegistry::new().register_read(fixture_tool("search"), handler);

        let agent = agent_with_loop_detection(llm, registry, 3, 10);
        let req = AgentRequest {
            system: "sys".into(),
            system_refresher: None,
            history: vec![ChatMessage::user("look")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;

        assert_eq!(captured.lock().unwrap().len(), 3);
        assert_eq!(run.final_text().as_deref(), Some("final summary"));
        assert!(run.first_error().is_none(), "no loop should be reported");
    }

    #[tokio::test]
    async fn loop_detection_window_evicts_old_entries() {
        // With window=3 and threshold=3, an interleaved pattern A,B,A,B,A
        // never has 3× `A` in the *last 3* slots — should not halt.
        let llm = Arc::new(ScriptedLlm::new(vec![
            tool_calls(vec![search_call("1", "A")]),
            tool_calls(vec![search_call("2", "B")]),
            tool_calls(vec![search_call("3", "A")]),
            tool_calls(vec![search_call("4", "B")]),
            tool_calls(vec![search_call("5", "A")]),
            ToolUseResponse::Text {
                text: "ok".into(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(CapturingHandler {
            name: "search",
            captured: captured.clone(),
            reply: "x".to_string(),
            is_error: false,
        });
        let registry = ToolRegistry::new().register_read(fixture_tool("search"), handler);

        let agent = agent_with_loop_detection(llm, registry, 3, 3);
        let req = AgentRequest {
            system: "sys".into(),
            system_refresher: None,
            history: vec![ChatMessage::user("look")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;
        // 5 dispatches with no halt — window evicted older `A`s.
        assert_eq!(captured.lock().unwrap().len(), 5);
        assert!(run.first_error().is_none());
    }

    #[tokio::test]
    async fn loop_detection_can_be_disabled_via_config() {
        // With detection OFF, max_iterations is the only guard.
        // 4× same-args call must dispatch 4 times.
        let llm = Arc::new(ScriptedLlm::new(vec![
            tool_calls(vec![search_call("1", "x")]),
            tool_calls(vec![search_call("2", "x")]),
            tool_calls(vec![search_call("3", "x")]),
            tool_calls(vec![search_call("4", "x")]),
            ToolUseResponse::Text {
                text: "ok".into(),
                truncated: false,
                limits: empty_limits(),
            },
        ]));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(CapturingHandler {
            name: "search",
            captured: captured.clone(),
            reply: "y".to_string(),
            is_error: false,
        });
        let registry = ToolRegistry::new().register_read(fixture_tool("search"), handler);

        let agent = Agent::with_config(
            llm,
            registry,
            Arc::new(AutoApprove),
            AgentConfig {
                max_iterations: 8,
                loop_detection: false,
                loop_detection_window: 10,
                loop_detection_threshold: 3,
            },
        );
        let req = AgentRequest {
            system: "sys".into(),
            system_refresher: None,
            history: vec![ChatMessage::user("look")],
            tool_choice: ToolChoice::Auto,
        };
        let run = run_to_completion(&agent, req).await;
        assert_eq!(captured.lock().unwrap().len(), 4);
        assert!(run.first_error().is_none());
    }

    #[tokio::test]
    async fn loop_detection_min_threshold_clamps_at_two() {
        // Threshold=0 or 1 would mean "halt on first call ever" — a
        // footgun. with_config clamps to a minimum of 2.
        let llm = Arc::new(ScriptedLlm::new(vec![tool_calls(vec![search_call(
            "1", "x",
        )])]));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(CapturingHandler {
            name: "search",
            captured: captured.clone(),
            reply: "y".to_string(),
            is_error: false,
        });
        let registry = ToolRegistry::new().register_read(fixture_tool("search"), handler);

        let agent = Agent::with_config(
            llm,
            registry,
            Arc::new(AutoApprove),
            AgentConfig {
                max_iterations: 4,
                loop_detection: true,
                loop_detection_window: 10,
                loop_detection_threshold: 0, // clamped to 2
            },
        );
        let req = AgentRequest {
            system: "sys".into(),
            system_refresher: None,
            history: vec![ChatMessage::user("look")],
            tool_choice: ToolChoice::Auto,
        };
        let _ = run_to_completion(&agent, req).await;
        // First call should still execute (count 0+1=1 < 2).
        assert_eq!(captured.lock().unwrap().len(), 1);
    }
}
