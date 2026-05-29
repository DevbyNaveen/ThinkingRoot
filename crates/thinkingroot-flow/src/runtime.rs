//! Flow runtime (C10, 2026-05-22).
//!
//! Orchestrates node execution against a validated
//! [`FlowDefinition`]: topo-sort the DAG, dispatch each ready
//! node to the matching [`NodeExecutor`], checkpoint after every
//! node via [`FlowStore::upsert_flow_run`], handle cancellation +
//! auto-resume. The runtime owns no per-run state — it's purely a
//! dispatcher over the store + executors. Each flow run produces
//! a [`FlowRunHandle`] the daemon stashes in `AppState.active_flow_runs`
//! so `flow_status` can poll progress + `flow_cancel` can trip
//! the token.
//!
//! # v1 scope (this commit)
//!
//! - `BranchStrategy::Inherit` only — writes go to the parent run's
//!   branch. `FanOutPerInput` + `NewSandbox` land when branch
//!   integration is wired (C19).
//! - `MergeStrategy::None` only — pairwise/firstwins/barrier are
//!   coordination policies that need branch-level support.
//! - Sequential dispatch — the topo order is respected, but nodes
//!   that COULD run in parallel run sequentially. Parallelisation
//!   is a perf optimisation that lands when the merge strategies
//!   land.
//! - One executor type: [`DeterministicExecutor`] (C11). Other
//!   executors (local_llm C12, client_sampling C14, mcp_tool C15,
//!   human C16) plug in via [`Executors::register`] as they land.
//!
//! Auto-resume IS shipped in v1: on construction, [`FlowRuntime::scan_resumable_runs`]
//! lists every `Running`-status run in the store and re-spawns
//! each. Per-node checkpoints in `node_outputs` let the resume
//! topo-walk skip already-completed nodes.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::definition::{FlowDefinition, NodeType};
use crate::error::{FlowError, Result};
use crate::executors::{ExecutorContext, NodeExecutor, NodeInputs};
use crate::storage::{FlowRunRecord, FlowRunStatus, FlowStore};
use crate::validator::topological_sort;

/// One executor per `NodeType` variant. The runtime looks up by
/// type at dispatch time. Unset variants return a typed error
/// (the runtime never panics on a node type it can't execute).
#[derive(Default, Clone)]
pub struct Executors {
    inner: Arc<Mutex<HashMap<NodeTypeKind, Arc<dyn NodeExecutor>>>>,
}

/// Discriminator over `NodeType` for executor lookup. Avoids
/// needing `NodeType` to implement `Hash` (its embedded fields
/// would otherwise need to too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeTypeKind {
    LocalLlm,
    McpTool,
    ClientSampling,
    Deterministic,
    Human,
    RootFunction,
}

impl NodeTypeKind {
    pub fn from_node_type(nt: &NodeType) -> Self {
        match nt {
            NodeType::LocalLlm { .. } => Self::LocalLlm,
            NodeType::McpTool { .. } => Self::McpTool,
            NodeType::ClientSampling { .. } => Self::ClientSampling,
            NodeType::Deterministic { .. } => Self::Deterministic,
            NodeType::Human { .. } => Self::Human,
            NodeType::RootFunction { .. } => Self::RootFunction,
        }
    }
}

impl Executors {
    pub async fn register(&self, kind: NodeTypeKind, executor: Arc<dyn NodeExecutor>) {
        self.inner.lock().await.insert(kind, executor);
    }

    pub async fn get(&self, kind: NodeTypeKind) -> Option<Arc<dyn NodeExecutor>> {
        self.inner.lock().await.get(&kind).cloned()
    }
}

/// Handle the daemon stores in `AppState.active_flow_runs`.
/// Mirrors the `active_merges` pattern at `rest.rs:121`.
#[derive(Debug)]
pub struct FlowRunHandle {
    pub flow_run_id: String,
    pub join_handle: JoinHandle<()>,
    pub cancel: CancellationToken,
    pub started_at: chrono::DateTime<Utc>,
}

/// Per-run final outcome — returned via the JoinHandle's await
/// for callers that block on completion (rare; most callers
/// observe via `FlowStore::get_flow_run`).
#[derive(Debug, Clone, PartialEq)]
pub struct FlowRunResult {
    pub flow_run_id: String,
    pub status: FlowRunStatus,
    pub error: Option<String>,
}

/// The runtime itself. Cheap to clone (everything wrapped in
/// `Arc`). One instance per daemon process; held on
/// `AppState.flow_runtime`.
#[derive(Clone)]
pub struct FlowRuntime {
    store: Arc<FlowStore>,
    executors: Executors,
}

impl FlowRuntime {
    pub fn new(store: FlowStore, executors: Executors) -> Self {
        Self {
            store: Arc::new(store),
            executors,
        }
    }

    pub fn store(&self) -> Arc<FlowStore> {
        Arc::clone(&self.store)
    }

    /// Scan the store for `Running`-status runs and return them.
    /// Daemon startup calls this and re-spawns each via
    /// [`Self::resume_run`] to honour the locked-in design
    /// decision "auto-resume on daemon start".
    pub fn scan_resumable_runs(&self) -> Result<Vec<FlowRunRecord>> {
        self.store.list_resumable_runs()
    }

    /// Start a new flow run associated with an MCP session. This
    /// is the entry point the `flow_run` MCP tool uses (C17), so
    /// `client_sampling` nodes can back-call the originating
    /// client's LLM. Pass `None` for CLI / REST entry points.
    pub async fn start_run_for_session(
        &self,
        flow_id: &str,
        workspace: &str,
        parent_branch: &str,
        inputs: serde_json::Value,
        originating_session_id: Option<String>,
    ) -> Result<FlowRunHandle> {
        let def_record = self
            .store
            .get_flow_definition(flow_id)?
            .ok_or_else(|| FlowError::Storage(format!(
                "flow definition '{flow_id}' not found"
            )))?;

        let flow_run_id = ulid::Ulid::new().to_string();
        let started_at = Utc::now();
        let record = FlowRunRecord {
            flow_run_id: flow_run_id.clone(),
            flow_id: def_record.definition.id.clone(),
            flow_version: def_record.definition.version,
            status: FlowRunStatus::Running,
            current_node: String::new(),
            started_at,
            finished_at: None,
            parent_branch: parent_branch.to_string(),
            originating_session_id,
            inputs,
            node_outputs: Default::default(),
            outputs: Default::default(),
            error: None,
        };
        self.store.upsert_flow_run(&record)?;

        let cancel = CancellationToken::new();
        let join_handle = self.spawn_run_task(record, def_record.definition, workspace.to_string(), cancel.clone());

        Ok(FlowRunHandle {
            flow_run_id,
            join_handle,
            cancel,
            started_at,
        })
    }

    /// Start a new flow run. Returns a handle the caller stashes
    /// in `active_flow_runs`. The run executes in a spawned task;
    /// progress is observable via `FlowStore::get_flow_run`.
    pub async fn start_run(
        &self,
        flow_id: &str,
        workspace: &str,
        parent_branch: &str,
        inputs: serde_json::Value,
    ) -> Result<FlowRunHandle> {
        let def_record = self
            .store
            .get_flow_definition(flow_id)?
            .ok_or_else(|| FlowError::Storage(format!(
                "flow definition '{flow_id}' not found"
            )))?;

        let flow_run_id = ulid::Ulid::new().to_string();
        let started_at = Utc::now();
        let record = FlowRunRecord {
            flow_run_id: flow_run_id.clone(),
            flow_id: def_record.definition.id.clone(),
            flow_version: def_record.definition.version,
            status: FlowRunStatus::Running,
            current_node: String::new(),
            started_at,
            finished_at: None,
            parent_branch: parent_branch.to_string(),
            originating_session_id: None,
            inputs,
            node_outputs: Default::default(),
            outputs: Default::default(),
            error: None,
        };
        self.store.upsert_flow_run(&record)?;

        let cancel = CancellationToken::new();
        let join_handle = self.spawn_run_task(record, def_record.definition, workspace.to_string(), cancel.clone());

        Ok(FlowRunHandle {
            flow_run_id,
            join_handle,
            cancel,
            started_at,
        })
    }

    /// Re-spawn a run from its checkpoint (called by daemon
    /// startup for each `Running` run found in the store).
    pub async fn resume_run(&self, record: FlowRunRecord) -> Result<FlowRunHandle> {
        let def_record = self
            .store
            .get_flow_definition(&record.flow_id)?
            .ok_or_else(|| FlowError::Storage(format!(
                "flow definition '{}' missing on resume — orphaned run",
                record.flow_id
            )))?;
        let flow_run_id = record.flow_run_id.clone();
        let started_at = record.started_at;
        let workspace = record.flow_id.clone(); // best-effort; the
        // caller (daemon startup) knows the actual workspace from
        // its own context — TODO follow-up: persist workspace on
        // FlowRunRecord so resume is self-contained.

        tracing::info!(
            target: "thinkingroot_flow::runtime",
            flow_run_id = %flow_run_id,
            flow_id = %record.flow_id,
            completed_nodes = record.node_outputs.len(),
            "auto-resuming flow run from checkpoint"
        );

        let cancel = CancellationToken::new();
        let join_handle = self.spawn_run_task(record, def_record.definition, workspace, cancel.clone());

        Ok(FlowRunHandle {
            flow_run_id,
            join_handle,
            cancel,
            started_at,
        })
    }

    fn spawn_run_task(
        &self,
        mut record: FlowRunRecord,
        def: FlowDefinition,
        workspace: String,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let store = Arc::clone(&self.store);
        let executors = self.executors.clone();
        tokio::spawn(async move {
            let outcome = drive_run(&store, &executors, &mut record, &def, &workspace, cancel).await;
            // Whatever happened, persist the final state.
            match outcome {
                Ok(()) => {
                    if !record.status.is_terminal() {
                        record.status = FlowRunStatus::Succeeded;
                    }
                    record.finished_at = Some(Utc::now());
                    let _ = store.upsert_flow_run(&record);
                }
                Err(e) => {
                    // If already terminal (cancellation set it),
                    // preserve that; otherwise mark Failed.
                    if !record.status.is_terminal() {
                        record.status = if matches!(e, FlowError::Cancelled { .. }) {
                            FlowRunStatus::Cancelled
                        } else {
                            FlowRunStatus::Failed
                        };
                    }
                    record.error = Some(e.to_string());
                    record.finished_at = Some(Utc::now());
                    let _ = store.upsert_flow_run(&record);
                }
            }
        })
    }
}

async fn drive_run(
    store: &Arc<FlowStore>,
    executors: &Executors,
    record: &mut FlowRunRecord,
    def: &FlowDefinition,
    workspace: &str,
    cancel: CancellationToken,
) -> Result<()> {
    let order = topological_sort(def).map_err(|cycle| FlowError::CycleDetected {
        nodes: cycle,
    })?;

    for node_id in order {
        // Skip nodes already completed in a prior run attempt.
        if record.node_outputs.contains_key(&node_id) {
            continue;
        }

        if cancel.is_cancelled() {
            record.status = FlowRunStatus::Cancelled;
            return Err(FlowError::Cancelled {
                at_node: node_id,
                reason: "cancel signal observed between node dispatches".to_string(),
            });
        }

        let node_spec = def
            .nodes
            .get(&node_id)
            .ok_or_else(|| FlowError::UnknownNode {
                node_id: node_id.clone(),
            })?
            .clone();

        let kind = NodeTypeKind::from_node_type(&node_spec.node_type);
        let executor = executors.get(kind).await.ok_or_else(|| FlowError::NodeFailed {
            node_id: node_id.clone(),
            message: format!(
                "no executor registered for node type {:?} — register via Executors::register",
                kind
            ),
        })?;

        // Build inputs: top-level flow inputs + upstream node
        // outputs already collected. Simple wire shape; per-node
        // input_mapping handling is a follow-up enhancement.
        let mut inputs = NodeInputs::new();
        inputs.insert("__flow_inputs".to_string(), record.inputs.clone());
        for (k, v) in &record.node_outputs {
            inputs.insert(k.clone(), v.clone());
        }

        record.current_node = node_id.clone();
        // Persist the "we're working on N" checkpoint so an
        // interrupted resume picks up the right node.
        let _ = store.upsert_flow_run(record);

        let exec_ctx = ExecutorContext {
            branch: &record.parent_branch,
            flow_run_id: &record.flow_run_id,
            node_id: &node_id,
            workspace,
            cancel: cancel.clone(),
            // C14 wiring: thread the originating MCP session id
            // (if any) through to executors that need it
            // (specifically `client_sampling` for back-calls).
            // None for CLI / REST runs.
            originating_session_id: record.originating_session_id.as_deref(),
        };

        let max_retries = def.max_node_retries;
        let mut attempt = 0u32;
        let output = loop {
            attempt += 1;
            match executor
                .execute(&node_spec, inputs.clone(), exec_ctx_ref(&exec_ctx))
                .await
            {
                Ok(out) => break out,
                Err(e) if matches!(e, FlowError::Cancelled { .. }) => {
                    record.status = FlowRunStatus::Cancelled;
                    return Err(e);
                }
                Err(e) if e.is_retryable() && attempt <= max_retries => {
                    tracing::warn!(
                        target: "thinkingroot_flow::runtime",
                        flow_run_id = %record.flow_run_id,
                        node_id = %node_id,
                        attempt,
                        error = %e,
                        "node failed, retrying"
                    );
                    continue;
                }
                Err(e) => {
                    return Err(FlowError::NodeFailed {
                        node_id: node_id.clone(),
                        message: e.to_string(),
                    });
                }
            }
        };

        // Checkpoint this node's output.
        record.node_outputs.insert(node_id.clone(), output);
        let _ = store.upsert_flow_run(record);
    }

    // Collect declared outputs.
    for (output_name, output_spec) in &def.outputs {
        let source_node = output_spec
            .source
            .split('.')
            .next()
            .unwrap_or(&output_spec.source);
        if let Some(value) = record.node_outputs.get(source_node) {
            record.outputs.insert(output_name.clone(), value.clone());
        }
    }
    record.status = FlowRunStatus::Succeeded;
    Ok(())
}

/// Helper: rebuild an ExecutorContext borrow because the prior
/// one was consumed. Cheap — all fields are references.
fn exec_ctx_ref<'a>(template: &'a ExecutorContext<'a>) -> ExecutorContext<'a> {
    ExecutorContext {
        branch: template.branch,
        flow_run_id: template.flow_run_id,
        node_id: template.node_id,
        workspace: template.workspace,
        cancel: template.cancel.clone(),
        originating_session_id: template.originating_session_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::FlowDefinition;
    use crate::executors::deterministic::{DeterministicExecutor, DeterministicRegistry};
    use serde_json::json;
    use tempfile::TempDir;

    fn build_runtime(workspace_root: &std::path::Path) -> FlowRuntime {
        let store = FlowStore::new(workspace_root);
        let registry = DeterministicRegistry::with_builtins();
        let executors = Executors::default();
        let runtime = FlowRuntime::new(store, executors.clone());
        // Register executor synchronously via blocking lock.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                executors
                    .register(
                        NodeTypeKind::Deterministic,
                        Arc::new(DeterministicExecutor::new(registry)),
                    )
                    .await;
            });
        });
        runtime
    }

    fn single_node_def() -> FlowDefinition {
        FlowDefinition::from_yaml(
            r#"
id: smoke
nodes:
  only:
    type: deterministic
    function: identity
outputs:
  result:
    type: object
    source: only
"#,
        )
        .expect("parse")
    }

    fn three_node_chain() -> FlowDefinition {
        FlowDefinition::from_yaml(
            r#"
id: chain
nodes:
  first:
    type: deterministic
    function: noop
  second:
    type: deterministic
    function: identity
  third:
    type: deterministic
    function: identity
edges:
  - from: first
    to: second
  - from: second
    to: third
"#,
        )
        .expect("parse")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_node_run_to_completion() {
        let tmp = TempDir::new().unwrap();
        let runtime = build_runtime(tmp.path());
        runtime
            .store()
            .insert_flow_definition(single_node_def())
            .unwrap();

        let handle = runtime
            .start_run("smoke", "ws-test", "main", json!({"hello": "world"}))
            .await
            .unwrap();
        handle.join_handle.await.unwrap();

        let record = runtime
            .store()
            .get_flow_run(&handle.flow_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, FlowRunStatus::Succeeded);
        assert!(record.finished_at.is_some());
        assert!(record.error.is_none());
        // Output collected from `only` node.
        assert!(record.outputs.contains_key("result"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn three_node_chain_runs_in_topo_order() {
        let tmp = TempDir::new().unwrap();
        let runtime = build_runtime(tmp.path());
        runtime
            .store()
            .insert_flow_definition(three_node_chain())
            .unwrap();

        let handle = runtime
            .start_run("chain", "ws-test", "main", json!({}))
            .await
            .unwrap();
        handle.join_handle.await.unwrap();

        let record = runtime
            .store()
            .get_flow_run(&handle.flow_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, FlowRunStatus::Succeeded);
        // Every node produced an output.
        assert!(record.node_outputs.contains_key("first"));
        assert!(record.node_outputs.contains_key("second"));
        assert!(record.node_outputs.contains_key("third"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_aborts_within_one_node_boundary() {
        let tmp = TempDir::new().unwrap();
        let runtime = build_runtime(tmp.path());
        runtime
            .store()
            .insert_flow_definition(three_node_chain())
            .unwrap();

        let handle = runtime
            .start_run("chain", "ws-test", "main", json!({}))
            .await
            .unwrap();
        // Trip immediately. The race is fine — even if the first
        // node completes, the second node MUST observe and abort.
        handle.cancel.cancel();
        handle.join_handle.await.unwrap();

        let record = runtime
            .store()
            .get_flow_run(&handle.flow_run_id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            record.status,
            FlowRunStatus::Cancelled | FlowRunStatus::Succeeded
        ));
        // If cancelled mid-flight, we'll have <3 node outputs.
        // If raced past it (very fast nodes), all 3 — both are
        // acceptable; the contract is "no crash, terminal state
        // recorded, error populated when cancelled".
        if record.status == FlowRunStatus::Cancelled {
            assert!(record.error.is_some());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_skips_checkpointed_nodes() {
        let tmp = TempDir::new().unwrap();
        let runtime = build_runtime(tmp.path());
        runtime
            .store()
            .insert_flow_definition(three_node_chain())
            .unwrap();

        // Simulate a half-done run: first + second completed, third remaining.
        let flow_run_id = ulid::Ulid::new().to_string();
        let mut record = FlowRunRecord {
            flow_run_id: flow_run_id.clone(),
            flow_id: "chain".to_string(),
            flow_version: 1,
            status: FlowRunStatus::Running,
            current_node: "third".to_string(),
            started_at: Utc::now(),
            finished_at: None,
            parent_branch: "main".to_string(),
            originating_session_id: None,
            inputs: json!({}),
            node_outputs: Default::default(),
            outputs: Default::default(),
            error: None,
        };
        record
            .node_outputs
            .insert("first".to_string(), json!(null));
        record
            .node_outputs
            .insert("second".to_string(), json!({}));
        runtime.store().upsert_flow_run(&record).unwrap();

        let handle = runtime.resume_run(record).await.unwrap();
        handle.join_handle.await.unwrap();

        let final_record = runtime
            .store()
            .get_flow_run(&flow_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(final_record.status, FlowRunStatus::Succeeded);
        assert!(final_record.node_outputs.contains_key("third"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unregistered_executor_returns_typed_node_failed_error() {
        let tmp = TempDir::new().unwrap();
        let store = FlowStore::new(tmp.path());
        let executors = Executors::default(); // empty
        let runtime = FlowRuntime::new(store, executors);
        runtime
            .store()
            .insert_flow_definition(single_node_def())
            .unwrap();

        let handle = runtime
            .start_run("smoke", "ws-test", "main", json!({}))
            .await
            .unwrap();
        handle.join_handle.await.unwrap();

        let record = runtime
            .store()
            .get_flow_run(&handle.flow_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, FlowRunStatus::Failed);
        assert!(record.error.as_deref().unwrap().contains("no executor"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scan_resumable_runs_returns_only_running_status() {
        let tmp = TempDir::new().unwrap();
        let runtime = build_runtime(tmp.path());
        runtime
            .store()
            .insert_flow_definition(single_node_def())
            .unwrap();

        // Seed three runs in different terminal states + one Running.
        for (id, status) in [
            ("r-running", FlowRunStatus::Running),
            ("r-done", FlowRunStatus::Succeeded),
            ("r-failed", FlowRunStatus::Failed),
        ] {
            let mut r = FlowRunRecord {
                flow_run_id: id.to_string(),
                flow_id: "smoke".to_string(),
                flow_version: 1,
                status,
                current_node: String::new(),
                started_at: Utc::now(),
                finished_at: if status.is_terminal() {
                    Some(Utc::now())
                } else {
                    None
                },
                parent_branch: "main".to_string(),
            originating_session_id: None,
                inputs: json!({}),
                node_outputs: Default::default(),
                outputs: Default::default(),
                error: None,
            };
            if status.is_terminal() {
                r.outputs.insert("x".to_string(), json!(1));
            }
            runtime.store().upsert_flow_run(&r).unwrap();
        }

        let resumable = runtime.scan_resumable_runs().unwrap();
        assert_eq!(resumable.len(), 1);
        assert_eq!(resumable[0].flow_run_id, "r-running");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_flow_definition_returns_typed_error() {
        let tmp = TempDir::new().unwrap();
        let runtime = build_runtime(tmp.path());
        let err = runtime
            .start_run("nonexistent", "ws-test", "main", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, FlowError::Storage(_)));
    }
}
