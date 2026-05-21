//! Failure modes the flow orchestrator surfaces.
//!
//! Every error variant is structured (not a free-text `String`)
//! so the runtime + CLI + desktop UI can branch on the kind without
//! string-matching. Mirrors the project-wide error discipline:
//! `Error::is_permanent()`-style classification is provided via
//! [`FlowError::is_retryable`].

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FlowError {
    /// YAML/TOML parse failure when loading a flow definition from
    /// disk or string. `path` is `None` for in-memory parse calls.
    #[error("flow definition parse failed at {path:?}: {message}")]
    DefinitionParse {
        path: Option<PathBuf>,
        message: String,
    },

    /// File extension didn't match a supported format. We accept
    /// `.yaml`, `.yml`, `.toml`; anything else is rejected loudly
    /// rather than guessing.
    #[error(
        "unsupported flow definition file extension at {path:?} \
         (got '{extension}'); supported: yaml, yml, toml"
    )]
    UnsupportedExtension { path: PathBuf, extension: String },

    /// IO failure (file not found, permission denied) when reading
    /// a flow definition.
    #[error("flow definition io failure at {path:?}: {source}")]
    DefinitionIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The DAG contains a cycle. `nodes` lists the cycle's node ids
    /// in the order the cycle traverses them (first id repeats at
    /// the end implicitly).
    #[error("flow DAG contains a cycle: {nodes:?}")]
    CycleDetected { nodes: Vec<String> },

    /// An edge references a node id that doesn't exist in the
    /// definition's `nodes` map.
    #[error("flow edge references unknown node id '{node_id}'")]
    UnknownNode { node_id: String },

    /// A `NodeType::McpTool` references a tool name not present in
    /// the runtime's known-tool set. `available_count` lets the
    /// caller phrase a helpful "did you mean..." hint without
    /// exposing the full list.
    #[error(
        "flow node '{node_id}' references unknown MCP tool '{tool}' \
         (runtime has {available_count} tools registered)"
    )]
    UnknownTool {
        node_id: String,
        tool: String,
        available_count: usize,
    },

    /// A `NodeType::Deterministic` references a function not
    /// registered in the deterministic executor's function table.
    #[error(
        "flow node '{node_id}' references unknown deterministic \
         function '{function}'"
    )]
    UnknownFunction {
        node_id: String,
        function: String,
    },

    /// Input/output types don't line up across a connected edge.
    #[error(
        "flow edge {from} -> {to} type mismatch: \
         producer emits '{producer_type}', consumer expects '{consumer_type}'"
    )]
    TypeMismatch {
        from: String,
        to: String,
        producer_type: String,
        consumer_type: String,
    },

    /// A node's executor returned an error mid-flow. The runtime
    /// decides whether to retry (per `max_node_retries` in the
    /// definition) or fail the whole run.
    #[error("flow node '{node_id}' failed: {message}")]
    NodeFailed { node_id: String, message: String },

    /// The flow run was cancelled mid-execution — either the
    /// caller dropped the response future, sent
    /// `notifications/cancelled`, or hit the flow's `timeout_secs`.
    #[error("flow run cancelled at node '{at_node}': {reason}")]
    Cancelled {
        at_node: String,
        reason: String,
    },

    /// Storage layer failure — typically a CozoDB write error or
    /// JSON serialization issue on the `flow_runs` relation.
    #[error("flow storage error: {0}")]
    Storage(String),

    /// Input validation against the definition's declared `inputs`
    /// schema failed at flow-run start.
    #[error("flow run input validation failed: {0}")]
    InputValidation(String),
}

impl FlowError {
    /// Best-effort classification used by the runtime to decide
    /// whether to retry a failed node. Matches the in-app agent's
    /// `Error::is_permanent()` convention but inverted: returns
    /// `true` when the runtime should retry.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            FlowError::NodeFailed { .. } | FlowError::Storage(_)
        )
    }

    /// Returns true for failures that originated outside the flow
    /// runtime proper (parse failure, schema violation, validator
    /// rejection). These should fail the run-attempt immediately
    /// rather than entering the per-node retry loop.
    pub fn is_definition_error(&self) -> bool {
        matches!(
            self,
            FlowError::DefinitionParse { .. }
                | FlowError::UnsupportedExtension { .. }
                | FlowError::DefinitionIo { .. }
                | FlowError::CycleDetected { .. }
                | FlowError::UnknownNode { .. }
                | FlowError::UnknownTool { .. }
                | FlowError::UnknownFunction { .. }
                | FlowError::TypeMismatch { .. }
                | FlowError::InputValidation(_)
        )
    }
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, FlowError>;
