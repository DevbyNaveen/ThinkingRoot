//! Root Function executor — the 6th flow node type.
//!
//! Runs a deployed Root Function (workspace-stored JS) in the
//! `deno_core` isolate via [`crate::engine::QueryEngine::invoke_function`].
//! Unlike [`super::super::flow_executors`]'s `Deterministic` executor
//! (compiled-in Rust functions), the code here is authored at runtime —
//! this is the mechanism JIT Class-3 (inline-code) acquisition produces.
//!
//! # Wire shape
//!
//! The node's resolved `inputs` map is passed verbatim as the `input`
//! argument to the function. The function's JSON return value becomes
//! this node's output (downstream nodes read it directly).
//!
//! # Build modes
//!
//! Execution itself is feature-gated (`root-functions`) inside
//! `invoke_function`. When the engine is built without the feature, the
//! node fails with a typed "feature not enabled" error rather than
//! silently succeeding — honest in every build.

use std::sync::Arc;

use async_trait::async_trait;
use thinkingroot_flow::definition::{NodeSpec, NodeType};
use thinkingroot_flow::error::{FlowError, Result as FlowResult};
use thinkingroot_flow::executors::{ExecutorContext, NodeExecutor, NodeInputs, NodeOutput};
use tokio::sync::RwLock;

use crate::engine::QueryEngine;

/// Holds a clone of the engine handle so it can resolve + run the named
/// function at call time (deploys can land without restarting).
pub struct RootFunctionExecutor {
    engine: Arc<RwLock<QueryEngine>>,
}

impl RootFunctionExecutor {
    pub fn new(engine: Arc<RwLock<QueryEngine>>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl NodeExecutor for RootFunctionExecutor {
    async fn execute(
        &self,
        node: &NodeSpec,
        inputs: NodeInputs,
        ctx: ExecutorContext<'_>,
    ) -> FlowResult<NodeOutput> {
        let function = match &node.node_type {
            NodeType::RootFunction { function, .. } => function.clone(),
            _ => {
                return Err(FlowError::NodeFailed {
                    node_id: ctx.node_id.to_string(),
                    message: "root_function executor invoked on non-RootFunction node".to_string(),
                });
            }
        };

        if ctx.cancel.is_cancelled() {
            return Err(FlowError::Cancelled {
                at_node: ctx.node_id.to_string(),
                reason: "cancel signal observed before function invocation".to_string(),
            });
        }

        // The whole resolved inputs map is the function's `input` arg.
        let input = serde_json::Value::Object(inputs);

        let engine = self.engine.read().await;
        engine
            .invoke_function(ctx.workspace, &function, &input)
            .await
            .map_err(|e| FlowError::NodeFailed {
                node_id: ctx.node_id.to_string(),
                message: format!("root function '{function}' failed: {e}"),
            })
    }
}
