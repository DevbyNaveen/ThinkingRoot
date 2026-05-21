//! Node executor trait + per-type executor implementations.
//!
//! The runtime (C10) dispatches each ready node to the executor
//! matching its `NodeType` variant. Five executor types ship:
//!
//! - [`deterministic`] (C11) — registered Rust functions.
//! - `local_llm` (C12) — in-process agent loop with reminder bus.
//! - `client_sampling` (C14) — back-call MCP `sampling/createMessage`.
//! - `mcp_tool` (C15) — any tool from `tools/list` (incl. external).
//! - `human` (C16) — pause for approval via `ApprovalGate`.
//!
//! Only [`deterministic`] ships in this commit; the others land
//! in their own commits. Each is a small, focused trait
//! implementation against [`NodeExecutor`].

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::definition::NodeSpec;
use crate::error::Result;

pub mod deterministic;

/// Owned inputs to a node execution. Keyed by the input mapping
/// the runtime computed from upstream node outputs + flow inputs.
/// Free-form JSON shape — type-checking happens at validate-time
/// where possible, runtime when not.
pub type NodeInputs = serde_json::Map<String, Value>;

/// Owned output produced by a node execution. Free-form JSON;
/// downstream nodes see this as part of their `NodeInputs`.
pub type NodeOutput = Value;

/// Context the runtime threads into every executor call.
pub struct ExecutorContext<'a> {
    /// Branch name this node should write to. The runtime
    /// computes this based on the node's `BranchStrategy` +
    /// the run's parent branch. Read-only from the executor's
    /// perspective.
    pub branch: &'a str,
    /// The flow run's id — used in checkpoint commits + audit
    /// logs.
    pub flow_run_id: &'a str,
    /// The id of THIS node. Used by executors to enrich error
    /// messages + telemetry.
    pub node_id: &'a str,
    /// The flow's workspace name.
    pub workspace: &'a str,
    /// Cancellation token observed at executor phase boundaries.
    /// SSE drops + `notifications/cancelled` trip this; the
    /// executor returns [`FlowError::Cancelled`] cleanly.
    pub cancel: CancellationToken,
    /// The MCP session id that originated this flow run, when
    /// the run was started from an MCP `flow_run` tool call.
    /// `None` for CLI-launched runs + REST-launched runs that
    /// don't carry an MCP session. The `client_sampling`
    /// executor REQUIRES this to be `Some(...)` so it can
    /// back-call the connected client's LLM; other executors
    /// ignore it.
    pub originating_session_id: Option<&'a str>,
}

/// The trait every node executor implements. One method, one
/// responsibility: take the node spec + computed inputs, return
/// the output (or a typed error). Async because most executors
/// do I/O.
#[async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn execute(
        &self,
        node: &NodeSpec,
        inputs: NodeInputs,
        ctx: ExecutorContext<'_>,
    ) -> Result<NodeOutput>;
}
