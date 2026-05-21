//! MCP tool executor (C15, 2026-05-22).
//!
//! Calls any MCP tool from the daemon's `tools/list` (internal
//! tools OR proxied `external::<server>::<tool>` external MCP
//! servers) as a flow node. The tool's response is unwrapped from
//! the JSON-RPC envelope and returned as the node output.
//!
//! Input mapping is honored: the node's `input_mapping` field
//! reshapes the runtime's standard `{__flow_inputs, <node_id>:
//! <output>, ...}` map into the specific argument shape the
//! target tool expects.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};
use thinkingroot_flow::definition::{NodeSpec, NodeType};
use thinkingroot_flow::error::{FlowError, Result as FlowResult};
use thinkingroot_flow::executors::{ExecutorContext, NodeExecutor, NodeInputs, NodeOutput};
use tokio::sync::RwLock;

use crate::engine::QueryEngine;
use crate::intelligence::engram::EngramManager;
use crate::intelligence::session::SessionStore;
use crate::mcp::tools;
use crate::rest::AppState;

pub struct McpToolExecutor {
    engine: Arc<RwLock<QueryEngine>>,
    sessions: SessionStore,
    engram_manager: Arc<EngramManager>,
    state: Arc<AppState>,
}

impl McpToolExecutor {
    pub fn new(
        engine: Arc<RwLock<QueryEngine>>,
        sessions: SessionStore,
        engram_manager: Arc<EngramManager>,
        state: Arc<AppState>,
    ) -> Self {
        Self {
            engine,
            sessions,
            engram_manager,
            state,
        }
    }
}

#[async_trait]
impl NodeExecutor for McpToolExecutor {
    async fn execute(
        &self,
        node: &NodeSpec,
        inputs: NodeInputs,
        ctx: ExecutorContext<'_>,
    ) -> FlowResult<NodeOutput> {
        let (tool_name, input_mapping) = match &node.node_type {
            NodeType::McpTool {
                tool,
                input_mapping,
            } => (tool.clone(), input_mapping.clone()),
            _ => {
                return Err(FlowError::NodeFailed {
                    node_id: ctx.node_id.to_string(),
                    message: "mcp_tool executor invoked on non-McpTool node".to_string(),
                });
            }
        };

        if ctx.cancel.is_cancelled() {
            return Err(FlowError::Cancelled {
                at_node: ctx.node_id.to_string(),
                reason: "cancel observed before tool dispatch".to_string(),
            });
        }

        // Build the tool arguments. When `input_mapping` is empty,
        // we pass through the resolved inputs as-is (the node's
        // upstream context). When `input_mapping` is non-empty,
        // we honor it: each mapping entry maps a tool argument
        // name → an input expression.
        //
        // v1 input expression grammar: `$inputs.<key>` →
        // value from `__flow_inputs[<key>]`; `$nodes.<id>` →
        // upstream node output; bare string → literal value.
        let mut arguments_obj = Map::new();
        if input_mapping.is_empty() {
            // Pass-through: copy every non-internal input.
            for (k, v) in inputs.iter() {
                if k == "__flow_inputs" {
                    continue;
                }
                arguments_obj.insert(k.clone(), v.clone());
            }
        } else {
            for (tool_arg, input_expr) in input_mapping {
                let resolved = resolve_input_expr(&input_expr, &inputs);
                arguments_obj.insert(tool_arg, resolved);
            }
        }

        // Ensure the workspace argument is present. The MCP tool
        // dispatcher uses `arguments.workspace` to resolve the
        // workspace; if the input mapping didn't include it,
        // default to the flow's workspace.
        arguments_obj
            .entry("workspace".to_string())
            .or_insert(Value::String(ctx.workspace.to_string()));

        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments_obj,
        });

        // Dispatch through the standard handle_call path so the
        // tool sees identical context to a regular MCP call
        // (session_actor, principal mapping, etc.).
        let engine_guard = self.engine.read().await;
        let response = tools::handle_call(
            None,
            &params,
            &*engine_guard,
            Some(ctx.workspace),
            // Use the flow's originating session if present;
            // otherwise synthesize one from flow_run_id so the
            // session_actor has something stable to key on.
            ctx.originating_session_id.unwrap_or(ctx.flow_run_id),
            &self.sessions,
            &self.engram_manager,
            Some(&self.state),
            ctx.cancel.clone(),
        )
        .await;
        drop(engine_guard);

        // Unwrap the JSON-RPC envelope into a node output.
        let response_value = serde_json::to_value(&response).map_err(|e| FlowError::NodeFailed {
            node_id: ctx.node_id.to_string(),
            message: format!("serialize tool response: {e}"),
        })?;

        if let Some(err) = response_value.get("error") {
            return Err(FlowError::NodeFailed {
                node_id: ctx.node_id.to_string(),
                message: format!(
                    "tool '{tool_name}' returned error: {}",
                    err.get("message").and_then(|v| v.as_str()).unwrap_or("unknown")
                ),
            });
        }

        // Standard tool result: result.content[0].text — return
        // the inner text. If the tool returned structured
        // content, return the full result object.
        let result = response_value.get("result").cloned().unwrap_or(Value::Null);
        Ok(result)
    }
}

/// Resolve an input expression against the runtime's inputs map.
/// Grammar:
/// - `$inputs.<key>` — look up `__flow_inputs[<key>]`
/// - `$nodes.<id>` — look up `<id>` (an upstream node's output)
/// - anything else — treated as a literal string value
fn resolve_input_expr(expr: &str, inputs: &NodeInputs) -> Value {
    if let Some(rest) = expr.strip_prefix("$inputs.") {
        if let Some(Value::Object(obj)) = inputs.get("__flow_inputs") {
            if let Some(v) = obj.get(rest) {
                return v.clone();
            }
        }
        return Value::Null;
    }
    if let Some(rest) = expr.strip_prefix("$nodes.") {
        if let Some(v) = inputs.get(rest) {
            return v.clone();
        }
        return Value::Null;
    }
    Value::String(expr.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_input_expr_handles_inputs_lookup() {
        let mut inputs = NodeInputs::new();
        inputs.insert("__flow_inputs".to_string(), json!({"topic": "AI"}));
        assert_eq!(
            resolve_input_expr("$inputs.topic", &inputs),
            json!("AI")
        );
    }

    #[test]
    fn resolve_input_expr_handles_nodes_lookup() {
        let mut inputs = NodeInputs::new();
        inputs.insert("scanner".to_string(), json!({"count": 5}));
        assert_eq!(
            resolve_input_expr("$nodes.scanner", &inputs),
            json!({"count": 5})
        );
    }

    #[test]
    fn resolve_input_expr_returns_null_on_missing_input_key() {
        let inputs = NodeInputs::new();
        assert_eq!(resolve_input_expr("$inputs.ghost", &inputs), Value::Null);
    }

    #[test]
    fn resolve_input_expr_returns_null_on_missing_node_key() {
        let inputs = NodeInputs::new();
        assert_eq!(resolve_input_expr("$nodes.ghost", &inputs), Value::Null);
    }

    #[test]
    fn resolve_input_expr_treats_bare_string_as_literal() {
        let inputs = NodeInputs::new();
        assert_eq!(
            resolve_input_expr("literal-string", &inputs),
            json!("literal-string")
        );
    }

    #[test]
    fn resolve_input_expr_handles_string_node_output_directly() {
        let mut inputs = NodeInputs::new();
        inputs.insert("greeting".to_string(), json!("hello"));
        assert_eq!(
            resolve_input_expr("$nodes.greeting", &inputs),
            json!("hello")
        );
    }
}
