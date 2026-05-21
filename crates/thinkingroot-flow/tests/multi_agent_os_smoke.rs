//! C19 end-to-end smoke (2026-05-22).
//!
//! Exercises the multi-agent OS substrate end-to-end without
//! requiring a running daemon, real LLM credentials, or browser
//! UI:
//!
//! 1. Define a 3-node flow (deterministic-only so the smoke is
//!    hermetic).
//! 2. Persist via FlowStore.
//! 3. Validate against the deterministic registry.
//! 4. Start a run; wait for completion.
//! 5. Verify status, node_outputs, final outputs all populate.
//! 6. Kill the runtime, re-construct from disk, verify
//!    auto-resume scan picks up running runs (simulated crash).
//!
//! The smoke runs in <1s on a developer laptop. It's the
//! production confidence pin for the runtime + storage + executor
//! integration — a regression here means the multi-agent OS is
//! broken at its core.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;
use tempfile::TempDir;
use thinkingroot_flow::definition::FlowDefinition;
use thinkingroot_flow::executors::deterministic::{
    DeterministicExecutor, DeterministicRegistry,
};
use thinkingroot_flow::runtime::{Executors, FlowRuntime, NodeTypeKind};
use thinkingroot_flow::storage::{FlowRunStatus, FlowStore};
use thinkingroot_flow::validator::{validate, ValidatorContext};

const SMOKE_FLOW_YAML: &str = r#"
id: smoke-multi-agent-os-v1
version: 1
description: |
  Three-node deterministic flow exercising the full substrate:
  storage + validator + runtime + executors + checkpoint + outputs.
inputs:
  message:
    type: string
outputs:
  echoed:
    type: string
    source: third
nodes:
  first:
    type: deterministic
    function: identity
  second:
    type: deterministic
    function: noop
  third:
    type: deterministic
    function: identity
edges:
  - from: first
    to: second
  - from: second
    to: third
final_merge:
  policy: manual
  target: main
max_node_retries: 1
"#;

async fn build_runtime(ws_root: &std::path::Path) -> FlowRuntime {
    let store = FlowStore::new(ws_root);
    let executors = Executors::default();
    executors
        .register(
            NodeTypeKind::Deterministic,
            Arc::new(DeterministicExecutor::new(
                DeterministicRegistry::with_builtins(),
            )),
        )
        .await;
    FlowRuntime::new(store, executors)
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_agent_os_smoke_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let ws_root = tmp.path();

    // ── Phase 1: Parse + validate ────────────────────────────
    let def = FlowDefinition::from_yaml(SMOKE_FLOW_YAML).expect("parse smoke YAML");
    assert_eq!(def.id, "smoke-multi-agent-os-v1");
    assert_eq!(def.nodes.len(), 3);
    assert_eq!(def.edges.len(), 2);

    // Validate against a registry that has the functions we
    // reference (identity, noop). Empty tools set — we don't use
    // any MCP tools in this hermetic smoke.
    let tools: HashSet<String> = HashSet::new();
    let functions: HashSet<String> = ["identity", "noop", "concat", "select_first"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let ctx = ValidatorContext::new(&tools, &functions);
    validate(&def, &ctx).expect("validator must accept smoke flow");

    // ── Phase 2: Persist ─────────────────────────────────────
    let runtime = build_runtime(ws_root).await;
    let stored_record = runtime
        .store()
        .insert_flow_definition(def.clone())
        .expect("insert flow definition");
    assert_eq!(stored_record.definition.id, def.id);
    assert_eq!(stored_record.content_blake3.len(), 64);

    // Round-trip: list shows it.
    let listed = runtime
        .store()
        .list_flow_definitions()
        .expect("list flows");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].definition.id, "smoke-multi-agent-os-v1");

    // ── Phase 3: Run ─────────────────────────────────────────
    let handle = runtime
        .start_run(
            "smoke-multi-agent-os-v1",
            "smoke-ws",
            "main",
            json!({"message": "hello multi-agent OS"}),
        )
        .await
        .expect("start_run");
    let flow_run_id = handle.flow_run_id.clone();
    handle.join_handle.await.expect("run completes");

    // ── Phase 4: Verify completion ───────────────────────────
    let final_record = runtime
        .store()
        .get_flow_run(&flow_run_id)
        .expect("get flow run")
        .expect("record present");
    assert_eq!(
        final_record.status,
        FlowRunStatus::Succeeded,
        "smoke flow must succeed — got error: {:?}",
        final_record.error
    );
    assert!(final_record.finished_at.is_some(), "finished_at populated");
    assert!(final_record.error.is_none(), "no error on success path");

    // All three nodes produced outputs (per-node checkpoint).
    assert_eq!(final_record.node_outputs.len(), 3, "all 3 nodes checkpointed");
    assert!(final_record.node_outputs.contains_key("first"));
    assert!(final_record.node_outputs.contains_key("second"));
    assert!(final_record.node_outputs.contains_key("third"));

    // The declared output (`echoed` ← third) is populated.
    assert!(
        final_record.outputs.contains_key("echoed"),
        "declared output should be in record.outputs"
    );

    // ── Phase 5: Simulated daemon restart + auto-resume ──────
    // The runtime is gone (we drop it). A fresh runtime
    // constructed against the same workspace dir must see no
    // resumable runs (this one already completed).
    drop(runtime);
    let restarted_runtime = build_runtime(ws_root).await;
    let resumable = restarted_runtime
        .scan_resumable_runs()
        .expect("scan resumable");
    assert!(
        resumable.is_empty(),
        "no runs should be auto-resumable post-completion; got {:?}",
        resumable
            .iter()
            .map(|r| (&r.flow_run_id, &r.status))
            .collect::<Vec<_>>()
    );

    // ── Phase 6: Simulated mid-flight crash + auto-resume ────
    // Manually write a half-done run record + verify
    // scan_resumable_runs picks it up.
    let half_done_id = "smoke-half-done-test".to_string();
    let mut half_done = thinkingroot_flow::storage::FlowRunRecord {
        flow_run_id: half_done_id.clone(),
        flow_id: "smoke-multi-agent-os-v1".to_string(),
        flow_version: 1,
        status: FlowRunStatus::Running,
        current_node: "second".to_string(),
        started_at: chrono::Utc::now(),
        finished_at: None,
        parent_branch: "main".to_string(),
        originating_session_id: None,
        inputs: json!({"message": "interrupted"}),
        node_outputs: Default::default(),
        outputs: Default::default(),
        error: None,
    };
    half_done.node_outputs.insert("first".to_string(), json!({}));
    restarted_runtime
        .store()
        .upsert_flow_run(&half_done)
        .expect("write half-done run");

    let resumable = restarted_runtime
        .scan_resumable_runs()
        .expect("scan resumable post-checkpoint");
    assert_eq!(
        resumable.len(),
        1,
        "half-done run must appear in resumable scan"
    );
    assert_eq!(resumable[0].flow_run_id, half_done_id);
    assert_eq!(resumable[0].node_outputs.len(), 1); // only `first` done

    // Resume it — should pick up at `second`, skip `first`.
    let resume_handle = restarted_runtime
        .resume_run(half_done)
        .await
        .expect("resume_run");
    resume_handle.join_handle.await.expect("resume completes");

    let final_resumed = restarted_runtime
        .store()
        .get_flow_run(&half_done_id)
        .expect("get resumed run")
        .expect("present");
    assert_eq!(
        final_resumed.status,
        FlowRunStatus::Succeeded,
        "resumed run must succeed"
    );
    assert_eq!(
        final_resumed.node_outputs.len(),
        3,
        "all nodes done after resume (first preserved + second+third executed)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_agent_os_smoke_handles_node_failure_gracefully() {
    let tmp = TempDir::new().unwrap();
    let ws_root = tmp.path();
    let runtime = build_runtime(ws_root).await;

    // Define a flow referencing an unregistered function — the
    // runtime should fail the run with a typed UnknownFunction
    // error, not panic.
    let def = FlowDefinition::from_yaml(
        r#"
id: smoke-bad-fn
nodes:
  bad:
    type: deterministic
    function: function_that_does_not_exist
"#,
    )
    .expect("parse");
    runtime
        .store()
        .insert_flow_definition(def)
        .expect("insert");

    let handle = runtime
        .start_run("smoke-bad-fn", "smoke-ws", "main", json!({}))
        .await
        .expect("start");
    handle.join_handle.await.expect("run completes");

    let record = runtime
        .store()
        .get_flow_run(&handle.flow_run_id)
        .expect("get")
        .expect("present");
    assert_eq!(record.status, FlowRunStatus::Failed);
    assert!(record.error.is_some());
    assert!(
        record
            .error
            .as_deref()
            .unwrap()
            .contains("function_that_does_not_exist"),
        "error message must mention the offending function"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_agent_os_smoke_cancellation_aborts_cleanly() {
    let tmp = TempDir::new().unwrap();
    let ws_root = tmp.path();
    let runtime = build_runtime(ws_root).await;

    let def = FlowDefinition::from_yaml(
        r#"
id: smoke-cancel
nodes:
  a:
    type: deterministic
    function: noop
  b:
    type: deterministic
    function: noop
edges:
  - from: a
    to: b
"#,
    )
    .expect("parse");
    runtime
        .store()
        .insert_flow_definition(def)
        .expect("insert");

    let handle = runtime
        .start_run("smoke-cancel", "smoke-ws", "main", json!({}))
        .await
        .expect("start");
    // Trip immediately. Race is fine — either we cancel before
    // execution starts (status=Cancelled) or we race past (status=Succeeded);
    // either way, no panic, terminal state recorded.
    handle.cancel.cancel();
    handle.join_handle.await.expect("completes");

    let record = runtime
        .store()
        .get_flow_run(&handle.flow_run_id)
        .expect("get")
        .expect("present");
    assert!(record.status.is_terminal(), "run must reach terminal state");
}
