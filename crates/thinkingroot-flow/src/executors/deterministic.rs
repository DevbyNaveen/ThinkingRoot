//! Deterministic executor (C11, 2026-05-22).
//!
//! Calls a registered Rust function — no LLM, no MCP, no I/O
//! beyond what the function itself does. Functions are
//! registered by name into [`DeterministicRegistry`]; the
//! validator (C8) consults the same name set at validate-time.
//!
//! Default registry ([`DeterministicRegistry::default`]) ships
//! with a small set of pure built-ins useful for flow plumbing:
//! `noop`, `identity`, `concat`, `select_first`. The runtime
//! caller adds engine-backed functions (search, hybrid_retrieve,
//! ingest_path, etc.) at startup by calling `register()`.
//!
//! Why pure built-ins ship here: they're useful for testing
//! flows without spinning up a daemon, and they're the natural
//! fallback when a flow needs a "pass this through" or "pick
//! first non-empty" step.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{ExecutorContext, NodeExecutor, NodeInputs, NodeOutput};
use crate::definition::{NodeSpec, NodeType};
use crate::error::{FlowError, Result};

/// Async function signature for a registered deterministic
/// function. Receives the resolved inputs + a cancellation token
/// it should observe at any long-running step.
pub type DeterministicFn = Arc<
    dyn Fn(
            NodeInputs,
            CancellationToken,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<NodeOutput>> + Send>,
        > + Send
        + Sync,
>;

/// Registry of name → function. Cheap to clone (functions held
/// in `Arc`s); the runtime constructs one at startup and shares
/// it across all flow runs.
#[derive(Clone, Default)]
pub struct DeterministicRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    functions: parking_lot::RwLock<HashMap<String, DeterministicFn>>,
}

// Use std::sync::RwLock instead — `parking_lot` isn't in flow
// crate's dep tree.

impl DeterministicRegistry {
    /// Build a registry with built-in functions pre-registered.
    pub fn with_builtins() -> Self {
        let reg = Self::default();
        reg.register("noop", deterministic_noop);
        reg.register("identity", deterministic_identity);
        reg.register("concat", deterministic_concat);
        reg.register("select_first", deterministic_select_first);
        reg
    }

    /// Register a function. Replaces any prior registration
    /// with the same name (last write wins) — caller is
    /// responsible for not clobbering known names accidentally.
    pub fn register<F, Fut>(&self, name: &str, function: F)
    where
        F: Fn(NodeInputs, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<NodeOutput>> + Send + 'static,
    {
        let wrapped: DeterministicFn = Arc::new(move |inputs, cancel| {
            Box::pin(function(inputs, cancel))
        });
        self.inner
            .functions
            .write()
            .insert(name.to_string(), wrapped);
    }

    /// Return the set of registered function names. Used by
    /// the validator (C8) to type-check `Deterministic.function`
    /// references at validate-time.
    pub fn function_names(&self) -> std::collections::HashSet<String> {
        self.inner.functions.read().keys().cloned().collect()
    }

    /// Look up a function by name.
    pub fn get(&self, name: &str) -> Option<DeterministicFn> {
        self.inner.functions.read().get(name).cloned()
    }
}

// Manual RwLock wrapper — flow crate doesn't pull parking_lot.
// Replace the above field with std::sync::RwLock for the same API.
mod parking_lot {
    use std::sync::{RwLock as StdRwLock, RwLockReadGuard, RwLockWriteGuard};
    pub struct RwLock<T>(StdRwLock<T>);
    impl<T: Default> Default for RwLock<T> {
        fn default() -> Self {
            Self(StdRwLock::new(T::default()))
        }
    }
    impl<T> RwLock<T> {
        pub fn read(&self) -> RwLockReadGuard<'_, T> {
            self.0.read().expect("rwlock poisoned")
        }
        pub fn write(&self) -> RwLockWriteGuard<'_, T> {
            self.0.write().expect("rwlock poisoned")
        }
    }
}

/// The C11 executor itself. Owns a reference to the registry +
/// dispatches each `Deterministic` node to the matching function.
pub struct DeterministicExecutor {
    registry: DeterministicRegistry,
}

impl DeterministicExecutor {
    pub fn new(registry: DeterministicRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl NodeExecutor for DeterministicExecutor {
    async fn execute(
        &self,
        node: &NodeSpec,
        inputs: NodeInputs,
        ctx: ExecutorContext<'_>,
    ) -> Result<NodeOutput> {
        let function_name = match &node.node_type {
            NodeType::Deterministic { function, .. } => function.clone(),
            _ => {
                return Err(FlowError::NodeFailed {
                    node_id: ctx.node_id.to_string(),
                    message: "deterministic executor invoked on non-deterministic node"
                        .to_string(),
                });
            }
        };

        // Bail early if already cancelled.
        if ctx.cancel.is_cancelled() {
            return Err(FlowError::Cancelled {
                at_node: ctx.node_id.to_string(),
                reason: "cancel signal observed before executor entry".to_string(),
            });
        }

        let function = self.registry.get(&function_name).ok_or_else(|| {
            // Should have been caught by validator (C8) but keep
            // the defense — runtime should never panic on a
            // missing function.
            FlowError::UnknownFunction {
                node_id: ctx.node_id.to_string(),
                function: function_name.clone(),
            }
        })?;

        function(inputs, ctx.cancel.clone()).await
    }
}

// ── Built-in functions ────────────────────────────────────────

/// `noop` — return `Value::Null` regardless of inputs. Useful as a
/// barrier point in flows where you want a graph node to mark
/// progress but produce no data.
async fn deterministic_noop(
    _inputs: NodeInputs,
    _cancel: CancellationToken,
) -> Result<NodeOutput> {
    Ok(Value::Null)
}

/// `identity` — return inputs as-is wrapped in an object. Useful
/// for fan-in nodes that want to collect upstream outputs
/// without transformation.
async fn deterministic_identity(
    inputs: NodeInputs,
    _cancel: CancellationToken,
) -> Result<NodeOutput> {
    Ok(Value::Object(inputs))
}

/// `concat` — concatenate every input value that is a string,
/// joined by `inputs.separator` (default `""`). Strict on
/// non-string inputs — returns InputValidation rather than
/// silently coercing.
async fn deterministic_concat(
    inputs: NodeInputs,
    _cancel: CancellationToken,
) -> Result<NodeOutput> {
    let separator = inputs
        .get("separator")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut parts: Vec<String> = Vec::new();
    let mut keys: Vec<&String> = inputs.keys().filter(|k| k.as_str() != "separator").collect();
    keys.sort(); // deterministic order
    for k in keys {
        match inputs.get(k) {
            Some(Value::String(s)) => parts.push(s.clone()),
            Some(other) => {
                return Err(FlowError::InputValidation(format!(
                    "concat: input '{k}' is {:?}, expected string",
                    other
                )));
            }
            None => unreachable!(),
        }
    }
    Ok(Value::String(parts.join(&separator)))
}

/// `select_first` — return the first non-null input value. Useful
/// for fan-in patterns where multiple branches may produce
/// candidates and you want the first one that succeeded. Returns
/// Null if every input was Null.
async fn deterministic_select_first(
    inputs: NodeInputs,
    _cancel: CancellationToken,
) -> Result<NodeOutput> {
    let mut keys: Vec<&String> = inputs.keys().collect();
    keys.sort(); // deterministic
    for k in keys {
        if let Some(v) = inputs.get(k) {
            if !v.is_null() {
                return Ok(v.clone());
            }
        }
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::FlowDefinition;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn deterministic_node(function: &str) -> NodeSpec {
        NodeSpec {
            node_type: NodeType::Deterministic {
                function: function.to_string(),
                input_mapping: BTreeMap::new(),
            },
            branch_strategy: Default::default(),
            merge_strategy: Default::default(),
            no_approval: false,
        }
    }

    fn ctx(node_id: &str) -> (CancellationToken, ExecutorContext<'_>) {
        let cancel = CancellationToken::new();
        let ctx = ExecutorContext {
            branch: "main",
            flow_run_id: "test-run",
            node_id,
            workspace: "ws-test",
            cancel: cancel.clone(),
            originating_session_id: None,
        };
        (cancel, ctx)
    }

    #[tokio::test]
    async fn builtins_registry_includes_canonical_names() {
        let reg = DeterministicRegistry::with_builtins();
        let names = reg.function_names();
        for required in ["noop", "identity", "concat", "select_first"] {
            assert!(
                names.contains(required),
                "missing builtin '{required}': {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn noop_returns_null() {
        let exec = DeterministicExecutor::new(DeterministicRegistry::with_builtins());
        let node = deterministic_node("noop");
        let (_c, ctx) = ctx("n1");
        let out = exec.execute(&node, NodeInputs::new(), ctx).await.unwrap();
        assert_eq!(out, Value::Null);
    }

    #[tokio::test]
    async fn identity_passes_inputs_through_as_object() {
        let exec = DeterministicExecutor::new(DeterministicRegistry::with_builtins());
        let node = deterministic_node("identity");
        let mut inputs = NodeInputs::new();
        inputs.insert("a".to_string(), json!(1));
        inputs.insert("b".to_string(), json!("hello"));
        let (_c, ctx) = ctx("n2");
        let out = exec.execute(&node, inputs.clone(), ctx).await.unwrap();
        assert_eq!(out, Value::Object(inputs));
    }

    #[tokio::test]
    async fn concat_joins_string_inputs_in_sorted_key_order() {
        let exec = DeterministicExecutor::new(DeterministicRegistry::with_builtins());
        let node = deterministic_node("concat");
        let mut inputs = NodeInputs::new();
        inputs.insert("c".to_string(), json!("Three"));
        inputs.insert("a".to_string(), json!("One"));
        inputs.insert("b".to_string(), json!("Two"));
        inputs.insert("separator".to_string(), json!(" - "));
        let (_c, ctx) = ctx("n3");
        let out = exec.execute(&node, inputs, ctx).await.unwrap();
        assert_eq!(out, json!("One - Two - Three"));
    }

    #[tokio::test]
    async fn concat_rejects_non_string_input() {
        let exec = DeterministicExecutor::new(DeterministicRegistry::with_builtins());
        let node = deterministic_node("concat");
        let mut inputs = NodeInputs::new();
        inputs.insert("a".to_string(), json!(42)); // not a string
        let (_c, ctx) = ctx("n4");
        let err = exec.execute(&node, inputs, ctx).await.unwrap_err();
        assert!(matches!(err, FlowError::InputValidation(_)));
    }

    #[tokio::test]
    async fn select_first_returns_first_non_null() {
        let exec = DeterministicExecutor::new(DeterministicRegistry::with_builtins());
        let node = deterministic_node("select_first");
        let mut inputs = NodeInputs::new();
        inputs.insert("a".to_string(), Value::Null);
        inputs.insert("b".to_string(), json!("found"));
        inputs.insert("c".to_string(), json!("ignored"));
        let (_c, ctx) = ctx("n5");
        let out = exec.execute(&node, inputs, ctx).await.unwrap();
        assert_eq!(out, json!("found"));
    }

    #[tokio::test]
    async fn select_first_returns_null_when_all_null() {
        let exec = DeterministicExecutor::new(DeterministicRegistry::with_builtins());
        let node = deterministic_node("select_first");
        let mut inputs = NodeInputs::new();
        inputs.insert("a".to_string(), Value::Null);
        inputs.insert("b".to_string(), Value::Null);
        let (_c, ctx) = ctx("n6");
        let out = exec.execute(&node, inputs, ctx).await.unwrap();
        assert_eq!(out, Value::Null);
    }

    #[tokio::test]
    async fn cancelled_token_returns_typed_error_before_function_runs() {
        let exec = DeterministicExecutor::new(DeterministicRegistry::with_builtins());
        let node = deterministic_node("noop");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ExecutorContext {
            branch: "main",
            flow_run_id: "test",
            node_id: "n",
            workspace: "ws",
            cancel,
            originating_session_id: None,
        };
        let err = exec.execute(&node, NodeInputs::new(), ctx).await.unwrap_err();
        assert!(matches!(err, FlowError::Cancelled { .. }));
    }

    #[tokio::test]
    async fn unknown_function_returns_unknown_function_error() {
        let exec = DeterministicExecutor::new(DeterministicRegistry::with_builtins());
        let node = deterministic_node("not_registered");
        let (_c, ctx) = ctx("n7");
        let err = exec.execute(&node, NodeInputs::new(), ctx).await.unwrap_err();
        assert!(matches!(err, FlowError::UnknownFunction { .. }));
    }

    #[tokio::test]
    async fn custom_function_can_be_registered_and_called() {
        let reg = DeterministicRegistry::default();
        reg.register("double", |inputs: NodeInputs, _cancel| async move {
            let n = inputs.get("n").and_then(|v| v.as_i64()).ok_or_else(|| {
                FlowError::InputValidation("missing or non-int 'n'".to_string())
            })?;
            Ok(json!(n * 2))
        });

        let exec = DeterministicExecutor::new(reg);
        let node = deterministic_node("double");
        let mut inputs = NodeInputs::new();
        inputs.insert("n".to_string(), json!(21));
        let (_c, ctx) = ctx("n8");
        let out = exec.execute(&node, inputs, ctx).await.unwrap();
        assert_eq!(out, json!(42));
    }

    #[test]
    fn validator_can_query_function_names_from_registry() {
        let reg = DeterministicRegistry::with_builtins();
        let names = reg.function_names();
        // The validator (C8) uses HashSet<String> directly.
        let names_set: std::collections::HashSet<String> = names;
        assert!(names_set.contains("noop"));
        assert!(names_set.contains("identity"));
        // Pin that the validator's downstream type matches.
        let _: std::collections::HashSet<String> = names_set;
    }

    #[tokio::test]
    async fn lit_review_reference_flow_validates_against_runtime_function_set() {
        // Smoke that the validator (C8) accepts the lit-review
        // reference flow when the runtime registers the tools it
        // expects (ingest_path, extract_claims).
        let yaml = std::fs::read_to_string("../../docs/flows/lit-review.yaml")
            .or_else(|_| std::fs::read_to_string("docs/flows/lit-review.yaml"))
            .expect("read reference flow");
        let def = FlowDefinition::from_yaml(&yaml).expect("parse");

        // ingest_path + extract_claims are tools (declared on the
        // scanner's LocalLlm.tools), NOT deterministic functions.
        // The deterministic function set for this flow is empty
        // (no Deterministic nodes).
        let tools: std::collections::HashSet<String> = ["ingest_path", "extract_claims"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let functions: std::collections::HashSet<String> = std::collections::HashSet::new();
        let ctx = crate::validator::ValidatorContext::new(&tools, &functions);
        let result = crate::validator::validate(&def, &ctx);
        assert!(result.is_ok(), "lit-review reference flow should validate");
    }
}
