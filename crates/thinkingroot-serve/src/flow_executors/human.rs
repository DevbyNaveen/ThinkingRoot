//! Human-approval executor (C16, 2026-05-22).
//!
//! Pauses the flow at this node and surfaces an approval prompt
//! to the user via the daemon's existing [`ApprovalGate`]
//! infrastructure. Production daemons wire in a
//! `ToolApprovalRouter` (the same one the in-app agent uses for
//! write-tool approvals) so the prompt lands in the desktop
//! modal / CLI prompt / IDE notification through the same path
//! as agent write approvals.
//!
//! # Approved → node outputs `{"approved": true}` and the flow continues.
//! # Rejected → node fails with `FlowError::NodeFailed { message: reason }`
//!   so the runtime can decide retry vs abort per `max_node_retries`.
//!
//! The `prompt_template` field on the node spec is rendered with
//! the same `{{inputs.X}}` / `{{nodes.X}}` substitution as
//! client_sampling so the approval prompt has the upstream
//! context to ground its decision on.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use thinkingroot_flow::definition::{NodeSpec, NodeType};
use thinkingroot_flow::error::{FlowError, Result as FlowResult};
use thinkingroot_flow::executors::{ExecutorContext, NodeExecutor, NodeInputs, NodeOutput};

use crate::intelligence::approval::{ApprovalDecision, ApprovalGate};

pub struct HumanExecutor {
    gate: Arc<dyn ApprovalGate>,
}

impl HumanExecutor {
    pub fn new(gate: Arc<dyn ApprovalGate>) -> Self {
        Self { gate }
    }
}

#[async_trait]
impl NodeExecutor for HumanExecutor {
    async fn execute(
        &self,
        node: &NodeSpec,
        inputs: NodeInputs,
        ctx: ExecutorContext<'_>,
    ) -> FlowResult<NodeOutput> {
        let prompt_template = match &node.node_type {
            NodeType::Human { prompt_template } => prompt_template.clone(),
            _ => {
                return Err(FlowError::NodeFailed {
                    node_id: ctx.node_id.to_string(),
                    message: "human executor invoked on non-Human node".to_string(),
                });
            }
        };

        if ctx.cancel.is_cancelled() {
            return Err(FlowError::Cancelled {
                at_node: ctx.node_id.to_string(),
                reason: "cancel observed before approval prompt".to_string(),
            });
        }

        // Render the prompt with upstream context. Same
        // substitution as client_sampling — keeps the two
        // user-facing surfaces consistent.
        let rendered_prompt = substitute_template(&prompt_template, &inputs);

        // Build the approval-gate input shape. The gate sees a
        // tool_use_id (we use flow_run_id:node_id for stable
        // routing), a tool_name (always "flow_human_approval"
        // so gates can recognise + style flow approvals
        // distinctly from agent write approvals), and an input
        // payload carrying the rendered prompt + upstream
        // inputs the user can inspect.
        let tool_use_id = format!("{}:{}", ctx.flow_run_id, ctx.node_id);
        let gate_input = json!({
            "prompt": rendered_prompt,
            "flow_run_id": ctx.flow_run_id,
            "node_id": ctx.node_id,
            "workspace": ctx.workspace,
            "branch": ctx.branch,
            "upstream_inputs": Value::Object(
                inputs.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            ),
        });

        // Race the approval-check against cancellation. Real
        // approval gates (ChannelApprovalGate / ToolApprovalRouter)
        // do their own internal `.await` on a oneshot — the
        // cancellation race here covers the case where the user
        // cancels the flow run while the approval modal is open.
        let check_future = self.gate.check(&tool_use_id, "flow_human_approval", &gate_input);
        let decision = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                return Err(FlowError::Cancelled {
                    at_node: ctx.node_id.to_string(),
                    reason: "cancel observed during approval wait".to_string(),
                });
            }
            d = check_future => d,
        };

        match decision {
            ApprovalDecision::Approved => Ok(json!({
                "approved": true,
                "prompt": rendered_prompt,
            })),
            ApprovalDecision::Rejected { reason } => Err(FlowError::NodeFailed {
                node_id: ctx.node_id.to_string(),
                message: format!("user rejected: {reason}"),
            }),
        }
    }
}

/// Same template substitution as `client_sampling::substitute_template`.
/// Re-implemented here rather than re-exported across module
/// boundaries because the flow-executor module structure keeps
/// each executor self-contained.
fn substitute_template(template: &str, inputs: &NodeInputs) -> String {
    let flow_inputs = inputs.get("__flow_inputs");
    let mut out = template.to_string();
    if let Some(Value::Object(flow_in_obj)) = flow_inputs {
        for (key, value) in flow_in_obj {
            let placeholder = format!("{{{{inputs.{key}}}}}");
            let rendered = render_value_for_template(value);
            out = out.replace(&placeholder, &rendered);
        }
    }
    for (key, value) in inputs {
        if key == "__flow_inputs" {
            continue;
        }
        let placeholder = format!("{{{{nodes.{key}}}}}");
        let rendered = render_value_for_template(value);
        out = out.replace(&placeholder, &rendered);
    }
    out
}

fn render_value_for_template(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::approval::{AutoApprove, DenyAll};
    use std::collections::BTreeMap;
    use thinkingroot_flow::definition::{BranchStrategy, MergeStrategy};
    use tokio_util::sync::CancellationToken;

    fn human_node(template: &str) -> NodeSpec {
        NodeSpec {
            node_type: NodeType::Human {
                prompt_template: template.to_string(),
            },
            branch_strategy: BranchStrategy::Inherit,
            merge_strategy: MergeStrategy::None,
            no_approval: false,
        }
    }

    fn ctx(node_id: &str) -> (CancellationToken, ExecutorContext<'_>) {
        let cancel = CancellationToken::new();
        let ctx = ExecutorContext {
            branch: "main",
            flow_run_id: "flow-run-test",
            node_id,
            workspace: "ws-test",
            cancel: cancel.clone(),
            originating_session_id: None,
        };
        (cancel, ctx)
    }

    #[tokio::test]
    async fn approved_decision_returns_approved_object() {
        let exec = HumanExecutor::new(Arc::new(AutoApprove));
        let node = human_node("Approve merge of {{inputs.target}}?");
        let mut inputs = NodeInputs::new();
        inputs.insert("__flow_inputs".to_string(), json!({"target": "main"}));
        let (_c, ectx) = ctx("reviewer");
        let out = exec.execute(&node, inputs, ectx).await.unwrap();
        assert_eq!(out["approved"], json!(true));
        assert_eq!(out["prompt"], json!("Approve merge of main?"));
    }

    #[tokio::test]
    async fn rejected_decision_returns_typed_node_failed_with_reason() {
        let exec = HumanExecutor::new(Arc::new(DenyAll));
        let node = human_node("Always rejected");
        let (_c, ectx) = ctx("reviewer");
        let err = exec.execute(&node, NodeInputs::new(), ectx).await.unwrap_err();
        match err {
            FlowError::NodeFailed { node_id, message } => {
                assert_eq!(node_id, "reviewer");
                assert!(message.contains("user rejected"));
            }
            other => panic!("expected NodeFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_before_approval_returns_typed_cancelled() {
        let exec = HumanExecutor::new(Arc::new(AutoApprove));
        let node = human_node("anything");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ExecutorContext {
            branch: "main",
            flow_run_id: "run",
            node_id: "n",
            workspace: "ws",
            cancel,
            originating_session_id: None,
        };
        let err = exec.execute(&node, NodeInputs::new(), ctx).await.unwrap_err();
        assert!(matches!(err, FlowError::Cancelled { .. }));
    }

    #[tokio::test]
    async fn template_substitutes_upstream_node_output() {
        let exec = HumanExecutor::new(Arc::new(AutoApprove));
        let node = human_node("Summary said: {{nodes.summarizer}}. Merge?");
        let mut inputs = NodeInputs::new();
        inputs.insert(
            "summarizer".to_string(),
            json!("12 claims, 3 contradictions"),
        );
        let (_c, ectx) = ctx("reviewer");
        let out = exec.execute(&node, inputs, ectx).await.unwrap();
        assert_eq!(
            out["prompt"],
            json!("Summary said: 12 claims, 3 contradictions. Merge?")
        );
    }

    #[tokio::test]
    async fn wrong_node_type_returns_typed_error() {
        use thinkingroot_flow::definition::NodeType;
        let exec = HumanExecutor::new(Arc::new(AutoApprove));
        let node = NodeSpec {
            node_type: NodeType::Deterministic {
                function: "noop".to_string(),
                input_mapping: BTreeMap::new(),
            },
            branch_strategy: BranchStrategy::Inherit,
            merge_strategy: MergeStrategy::None,
            no_approval: false,
        };
        let (_c, ectx) = ctx("misrouted");
        let err = exec.execute(&node, NodeInputs::new(), ectx).await.unwrap_err();
        assert!(matches!(err, FlowError::NodeFailed { .. }));
    }
}
