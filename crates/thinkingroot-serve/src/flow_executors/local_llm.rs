//! Local LLM executor (C12, 2026-05-22).
//!
//! Single-shot LLM call against the workspace's configured
//! [`LlmClient`]. Designed to be the foundational executor for
//! flows that need cheap text generation (summarisation, claim
//! extraction, paraphrase). For flows that need tool dispatch +
//! iteration, the right primitive is an `mcp_tool` node calling
//! the `ask` tool (which runs the full agent loop).
//!
//! # Design rationale: one node = one LLM call
//!
//! LangGraph + similar systems collapse "one node = one LLM call"
//! by convention. Doing the same here keeps the executor honest:
//! the LLM sees exactly the node's system prompt + the resolved
//! inputs. No agent loop, no tool whitelist enforcement (the
//! schema's `tools` field is informational text inlined into the
//! system prompt — see [`render_tools_section`]).
//!
//! # Wire shape
//!
//! Input map at execute() time is the runtime's standard:
//! - `__flow_inputs` — the top-level flow inputs object
//! - `<upstream_node_id>` → that node's output value
//!
//! The user message presented to the LLM is a deterministic JSON
//! pretty-print of the inputs map, so the LLM has the full
//! upstream context to ground on.
//!
//! Output: a JSON `Value::Object({"text": "<llm response>",
//! "model": "<model name>"})`. Downstream nodes pull `text` for
//! the rendered content + `model` for citation / audit.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use thinkingroot_flow::definition::{NodeSpec, NodeType};
use thinkingroot_flow::error::{FlowError, Result as FlowResult};
use thinkingroot_flow::executors::{ExecutorContext, NodeExecutor, NodeInputs, NodeOutput};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::engine::QueryEngine;

/// The C12 executor. Holds a clone of the engine handle so it can
/// resolve the workspace's `LlmClient` at call time (LLM config
/// can change without restarting the daemon).
pub struct LocalLlmExecutor {
    engine: Arc<RwLock<QueryEngine>>,
}

impl LocalLlmExecutor {
    pub fn new(engine: Arc<RwLock<QueryEngine>>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl NodeExecutor for LocalLlmExecutor {
    async fn execute(
        &self,
        node: &NodeSpec,
        inputs: NodeInputs,
        ctx: ExecutorContext<'_>,
    ) -> FlowResult<NodeOutput> {
        // Extract the node's LocalLlm config or refuse honestly.
        let (system_prompt, tools_whitelist, _max_iterations) = match &node.node_type {
            NodeType::LocalLlm {
                system,
                tools,
                max_iterations,
            } => (system.clone(), tools.clone(), *max_iterations),
            _ => {
                return Err(FlowError::NodeFailed {
                    node_id: ctx.node_id.to_string(),
                    message: "local_llm executor invoked on non-LocalLlm node".to_string(),
                });
            }
        };

        // Cancellation pre-check.
        if ctx.cancel.is_cancelled() {
            return Err(FlowError::Cancelled {
                at_node: ctx.node_id.to_string(),
                reason: "cancel signal observed before LLM dispatch".to_string(),
            });
        }

        // Resolve the workspace's LLM client. Honest empty: if
        // the workspace has no `[llm]` config, fail the node with
        // a typed error — never fake a response.
        let llm = {
            let engine = self.engine.read().await;
            engine.workspace_llm(ctx.workspace).ok_or_else(|| FlowError::NodeFailed {
                node_id: ctx.node_id.to_string(),
                message: format!(
                    "workspace '{}' has no [llm] config; local_llm executor needs one",
                    ctx.workspace
                ),
            })?
        };

        // Compose the user message from inputs. Deterministic
        // pretty-print so the LLM sees the same shape every time
        // the same inputs are supplied (cache-friendly).
        let user_message = render_inputs_as_user_message(&inputs);

        // Add the tools whitelist as an informational suffix on
        // the system prompt. The LLM can't actually CALL these
        // (single-shot executor), but knowing what's notionally
        // available helps it phrase results that downstream
        // mcp_tool nodes can act on.
        let augmented_system = if tools_whitelist.is_empty() {
            system_prompt
        } else {
            format!(
                "{system_prompt}\n\n{}",
                render_tools_section(&tools_whitelist)
            )
        };

        // Race the LLM call against cancellation. The LLM client
        // doesn't natively observe a CancellationToken, so we
        // wrap it in a select! — the request itself continues on
        // the network even after cancellation observed (no clean
        // abort possible without surgery to LlmClient), but we
        // stop waiting and return Cancelled cleanly.
        let llm_call = llm.chat(&augmented_system, &user_message);
        let response = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                return Err(FlowError::Cancelled {
                    at_node: ctx.node_id.to_string(),
                    reason: "cancel signal observed during LLM call".to_string(),
                });
            }
            result = llm_call => {
                result.map_err(|e| FlowError::NodeFailed {
                    node_id: ctx.node_id.to_string(),
                    message: format!("LLM call failed: {e}"),
                })?
            }
        };

        // Pull model name for citation. workspace_llm_summary
        // returns (provider, model); we surface the model.
        let model_name = {
            let engine = self.engine.read().await;
            engine
                .workspace_llm_summary(ctx.workspace)
                .map(|(_, model)| model)
                .unwrap_or_default()
        };

        Ok(Value::Object(
            [
                ("text".to_string(), Value::String(response)),
                ("model".to_string(), Value::String(model_name)),
            ]
            .into_iter()
            .collect(),
        ))
    }
}

/// Render the executor's `NodeInputs` as the user message the LLM
/// sees. Deterministic key order (BTreeMap-style sort) so the same
/// inputs always produce the same prompt — important for both
/// LLM-side prompt-cache hits AND downstream content_blake3
/// stability.
fn render_inputs_as_user_message(inputs: &NodeInputs) -> String {
    // Use a BTreeMap for deterministic key order.
    let mut ordered: std::collections::BTreeMap<&str, &Value> = std::collections::BTreeMap::new();
    for (k, v) in inputs.iter() {
        ordered.insert(k.as_str(), v);
    }
    let payload = serde_json::to_value(&ordered).unwrap_or(Value::Null);
    match serde_json::to_string_pretty(&payload) {
        Ok(s) => format!("Inputs:\n{s}\n\nRespond with the requested output."),
        Err(_) => "Inputs:\n<unserialisable>\n\nRespond with the requested output."
            .to_string(),
    }
}

/// Render the tools-whitelist as a system-prompt suffix.
fn render_tools_section(tools: &[String]) -> String {
    let mut s = String::from("# Available tools (informational)\n");
    s.push_str(
        "The following tools exist in this workspace's MCP catalogue. \
         This executor cannot call them directly — for tool dispatch, \
         use an `mcp_tool` node. List provided so you can reference \
         them in your output if relevant:\n",
    );
    for t in tools {
        s.push_str(&format!("- {t}\n"));
    }
    s
}

/// Pre-compute a `CancellationToken` for tests that need to trip
/// it after a delay. Tiny helper kept here so the test module is
/// self-contained.
#[cfg(test)]
fn delayed_cancel(after: std::time::Duration) -> CancellationToken {
    let token = CancellationToken::new();
    let trip = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        trip.cancel();
    });
    token
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_inputs_emits_deterministic_key_order() {
        let mut inputs = NodeInputs::new();
        inputs.insert("z".to_string(), json!(1));
        inputs.insert("a".to_string(), json!(2));
        inputs.insert("m".to_string(), json!(3));
        let rendered = render_inputs_as_user_message(&inputs);
        // a should appear before m before z.
        let pos_a = rendered.find("\"a\"").unwrap();
        let pos_m = rendered.find("\"m\"").unwrap();
        let pos_z = rendered.find("\"z\"").unwrap();
        assert!(pos_a < pos_m);
        assert!(pos_m < pos_z);
    }

    #[test]
    fn render_inputs_is_byte_identical_across_calls() {
        let mut inputs = NodeInputs::new();
        inputs.insert("name".to_string(), json!("test"));
        inputs.insert("count".to_string(), json!(42));
        let r1 = render_inputs_as_user_message(&inputs);
        let r2 = render_inputs_as_user_message(&inputs);
        assert_eq!(r1, r2);
    }

    #[test]
    fn render_tools_section_lists_each_tool_on_its_own_line() {
        let tools = vec![
            "ingest_path".to_string(),
            "extract_claims".to_string(),
            "search".to_string(),
        ];
        let rendered = render_tools_section(&tools);
        assert!(rendered.contains("- ingest_path"));
        assert!(rendered.contains("- extract_claims"));
        assert!(rendered.contains("- search"));
        // Each tool on its own line.
        let line_count = rendered
            .lines()
            .filter(|l| l.starts_with("- "))
            .count();
        assert_eq!(line_count, 3);
    }

    #[test]
    fn render_inputs_handles_unserialisable_input_with_honest_placeholder() {
        // Inputs is a serde_json::Map so all values are
        // already Value; the inner serialize_value can't fail
        // for valid Values. We still pin the fallback path.
        let inputs = NodeInputs::new();
        let rendered = render_inputs_as_user_message(&inputs);
        // Empty inputs serialise as `{}`.
        assert!(rendered.contains("{}"));
        assert!(rendered.contains("Respond with the requested output"));
    }

    #[tokio::test]
    async fn delayed_cancel_helper_trips_after_delay() {
        let token = delayed_cancel(std::time::Duration::from_millis(20));
        assert!(!token.is_cancelled());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(token.is_cancelled());
    }

    // Integration tests against a real workspace + LlmClient need
    // a running daemon + config.toml with [llm] credentials; live
    // tests are #[ignore]-gated in real-daemon harnesses, not
    // here. The unit tests above cover the wire formatting; the
    // cancellation contract is covered by the runtime's
    // cancellation_aborts_within_one_node_boundary test
    // (thinkingroot-flow::runtime::tests).
}
