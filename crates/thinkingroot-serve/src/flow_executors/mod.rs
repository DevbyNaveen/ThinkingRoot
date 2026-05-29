//! Serve-side flow executor implementations.
//!
//! The `thinkingroot-flow` crate defines the [`NodeExecutor`] trait
//! and ships the [`DeterministicExecutor`] (C11). Serve provides
//! the executor implementations that need access to AppState,
//! engine, MCP transports, approval gate, etc.:
//!
//! - [`local_llm`] (C12) — single-shot LLM call via the workspace's
//!   configured `LlmClient`. The system prompt comes from the node
//!   spec; the user message is composed from the node's resolved
//!   inputs. No tool dispatch, no iteration loop — that's an
//!   `mcp_tool` node calling the `ask` tool's job. This keeps the
//!   executor model honest: one node ≈ one LLM call.
//! - `mcp_tool` (C15, follow-up) — wraps `tools::handle_call` to
//!   dispatch any registered MCP tool from inside a flow.
//! - `client_sampling` (C14, follow-up) — back-call the connected
//!   MCP client's LLM via `sampling/createMessage`.
//! - `human` (C16, follow-up) — pause for approval via
//!   `ApprovalGate`.
//!
//! The daemon registers each available executor on
//! `AppState.flow_runtime` at construction time.

pub mod client_sampling;
pub mod human;
pub mod local_llm;
pub mod mcp_tool;
pub mod root_function;
