//! `root flow {run,list,show,cancel,validate}` CLI subcommand (C19).
//!
//! Operates directly against the local workspace's
//! `<workspace_root>/.thinkingroot/flows/` + `flow-runs/`
//! directories. No daemon needed — the file-backed FlowStore is
//! the single source of truth. For the agent-driven equivalent
//! (run a flow via MCP), use the `flow_run` MCP tool.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use serde_json::Value;
use thinkingroot_flow::definition::FlowDefinition;
use thinkingroot_flow::executors::deterministic::{
    DeterministicExecutor, DeterministicRegistry,
};
use thinkingroot_flow::runtime::{Executors, FlowRuntime, NodeTypeKind};
use thinkingroot_flow::storage::FlowStore;
use thinkingroot_flow::validator::{validate, ValidatorContext};

#[derive(Debug, Subcommand)]
pub enum FlowAction {
    /// Register a flow definition from a YAML or TOML file.
    Define {
        /// Path to the flow definition file (.yaml, .yml, or .toml).
        #[arg(long)]
        path: PathBuf,
        /// Workspace root (default: current dir).
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Start a flow run.
    Run {
        /// Flow id to execute.
        flow_id: String,
        /// JSON-encoded inputs object. Default: `{}`.
        #[arg(long, default_value = "{}")]
        inputs: String,
        /// Workspace root (default: current dir).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Parent branch the run writes to. Default: main.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Block until the run finishes. Default: detach.
        #[arg(long)]
        wait: bool,
    },
    /// List registered flow definitions + recent runs.
    List {
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Show flow runs instead of definitions.
        #[arg(long)]
        runs: bool,
    },
    /// Show details for one flow run.
    Show {
        flow_run_id: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Output JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Cancel an in-flight flow run.
    Cancel {
        flow_run_id: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Validate a flow definition file without persisting it.
    Validate {
        #[arg(long)]
        path: PathBuf,
    },
}

/// Entry point for `root flow ...`. Returns process exit code.
pub async fn run(action: FlowAction) -> anyhow::Result<i32> {
    match action {
        FlowAction::Define { path, workspace } => define(path, workspace).await,
        FlowAction::Run {
            flow_id,
            inputs,
            workspace,
            branch,
            wait,
        } => run_flow(flow_id, inputs, workspace, branch, wait).await,
        FlowAction::List { workspace, runs } => list(workspace, runs).await,
        FlowAction::Show {
            flow_run_id,
            workspace,
            json,
        } => show(flow_run_id, workspace, json).await,
        FlowAction::Cancel {
            flow_run_id,
            workspace,
        } => cancel(flow_run_id, workspace).await,
        FlowAction::Validate { path } => validate_cmd(path).await,
    }
}

fn resolve_workspace(workspace: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match workspace {
        Some(p) => Ok(p),
        None => Ok(std::env::current_dir()?),
    }
}

async fn define(path: PathBuf, workspace: Option<PathBuf>) -> anyhow::Result<i32> {
    let ws = resolve_workspace(workspace)?;
    let def = FlowDefinition::from_path(&path)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    let store = FlowStore::new(&ws);
    let record = store.insert_flow_definition(def)?;
    println!("flow defined:");
    println!("  id:             {}", record.definition.id);
    println!("  version:        {}", record.definition.version);
    println!("  nodes:          {}", record.definition.nodes.len());
    println!("  content_blake3: {}", record.content_blake3);
    println!("  workspace:      {}", ws.display());
    Ok(0)
}

async fn run_flow(
    flow_id: String,
    inputs_json: String,
    workspace: Option<PathBuf>,
    branch: String,
    wait: bool,
) -> anyhow::Result<i32> {
    let ws = resolve_workspace(workspace)?;
    let inputs: Value = serde_json::from_str(&inputs_json)
        .map_err(|e| anyhow::anyhow!("inputs must be valid JSON: {e}"))?;

    let store = FlowStore::new(&ws);
    let executors = Executors::default();
    // CLI runs only deterministic nodes natively. local_llm,
    // client_sampling, mcp_tool, human all need daemon
    // infrastructure (engine handle, MCP transport, approval
    // gate). The user-facing message + `root flow validate`
    // surface that limitation honestly when a flow references
    // executors that need the daemon.
    let registry = DeterministicRegistry::with_builtins();
    executors
        .register(
            NodeTypeKind::Deterministic,
            Arc::new(DeterministicExecutor::new(registry)),
        )
        .await;
    let runtime = FlowRuntime::new(store, executors);

    let handle = runtime
        .start_run(&flow_id, &ws.to_string_lossy(), &branch, inputs)
        .await?;
    println!("flow run started:");
    println!("  flow_run_id: {}", handle.flow_run_id);
    println!("  flow_id:     {}", flow_id);
    println!("  branch:      {}", branch);
    println!("  started_at:  {}", handle.started_at.to_rfc3339());

    if wait {
        let _ = handle.join_handle.await;
        let final_record = runtime.store().get_flow_run(&handle.flow_run_id)?;
        match final_record {
            Some(r) => {
                println!("\nfinal status:  {:?}", r.status);
                if let Some(err) = r.error {
                    println!("error:         {err}");
                }
                if !r.outputs.is_empty() {
                    println!("outputs:");
                    for (k, v) in &r.outputs {
                        println!("  {k} = {v}");
                    }
                }
            }
            None => println!("\nfinal status:  <run state file missing>"),
        }
    } else {
        println!(
            "\nrun detached; check status with `root flow show {}`",
            handle.flow_run_id
        );
    }
    Ok(0)
}

async fn list(workspace: Option<PathBuf>, runs: bool) -> anyhow::Result<i32> {
    let ws = resolve_workspace(workspace)?;
    let store = FlowStore::new(&ws);
    if runs {
        let records = store.list_flow_runs()?;
        if records.is_empty() {
            println!("no flow runs at {}/.thinkingroot/flow-runs/", ws.display());
        } else {
            println!("flow runs ({})", records.len());
            for r in records {
                println!(
                    "  {}  {:>10}  {}  ({})",
                    r.flow_run_id,
                    format!("{:?}", r.status).to_lowercase(),
                    r.flow_id,
                    r.started_at.to_rfc3339(),
                );
            }
        }
    } else {
        let records = store.list_flow_definitions()?;
        if records.is_empty() {
            println!("no flows at {}/.thinkingroot/flows/", ws.display());
        } else {
            println!("flow definitions ({})", records.len());
            for r in records {
                println!(
                    "  {:>20}  v{}  ({} nodes)  [{}]",
                    r.definition.id,
                    r.definition.version,
                    r.definition.nodes.len(),
                    &r.content_blake3[..12],
                );
            }
        }
    }
    Ok(0)
}

async fn show(
    flow_run_id: String,
    workspace: Option<PathBuf>,
    json: bool,
) -> anyhow::Result<i32> {
    let ws = resolve_workspace(workspace)?;
    let store = FlowStore::new(&ws);
    let record = store
        .get_flow_run(&flow_run_id)?
        .ok_or_else(|| anyhow::anyhow!("flow_run_id '{flow_run_id}' not found"))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&record)?);
    } else {
        println!("flow_run_id:    {}", record.flow_run_id);
        println!("flow_id:        {}", record.flow_id);
        println!("status:         {:?}", record.status);
        println!("current_node:   {}", record.current_node);
        println!("started_at:     {}", record.started_at.to_rfc3339());
        if let Some(f) = record.finished_at {
            println!("finished_at:    {}", f.to_rfc3339());
        }
        println!("parent_branch:  {}", record.parent_branch);
        if let Some(sid) = &record.originating_session_id {
            println!("session_id:     {sid}");
        }
        println!("nodes done:     {}", record.node_outputs.len());
        if !record.outputs.is_empty() {
            println!("outputs:");
            for (k, v) in &record.outputs {
                println!("  {k} = {}", serde_json::to_string(v).unwrap_or_default());
            }
        }
        if let Some(err) = &record.error {
            println!("error:          {err}");
        }
    }
    Ok(0)
}

async fn cancel(flow_run_id: String, workspace: Option<PathBuf>) -> anyhow::Result<i32> {
    let ws = resolve_workspace(workspace)?;
    let store = FlowStore::new(&ws);
    let record = store
        .get_flow_run(&flow_run_id)?
        .ok_or_else(|| anyhow::anyhow!("flow_run_id '{flow_run_id}' not found"))?;
    if record.status.is_terminal() {
        println!(
            "flow_run_id {} is already in terminal state {:?}",
            flow_run_id, record.status
        );
        return Ok(0);
    }
    let mut updated = record.clone();
    updated.status = thinkingroot_flow::storage::FlowRunStatus::Cancelled;
    updated.finished_at = Some(chrono::Utc::now());
    updated.error = Some("cancelled by CLI".to_string());
    store.upsert_flow_run(&updated)?;
    println!("flow_run_id {flow_run_id} marked cancelled");
    println!("note: CLI cancellation only marks the store record. Live");
    println!("      in-flight cancellation requires the run to be attached");
    println!("      to a daemon — use `flow_status` MCP tool against a");
    println!("      running daemon to trip the CancellationToken.");
    Ok(0)
}

async fn validate_cmd(path: PathBuf) -> anyhow::Result<i32> {
    let def = FlowDefinition::from_path(&path)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    // Validate against the minimal CLI runtime: deterministic
    // built-ins only. Flows referencing tools/functions outside
    // this set will fail validation here — that's honest, since
    // the CLI can't run those nodes natively anyway.
    let tools: HashSet<String> = HashSet::new();
    let functions: HashSet<String> = ["noop", "identity", "concat", "select_first"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let ctx = ValidatorContext::new(&tools, &functions);
    match validate(&def, &ctx) {
        Ok(()) => {
            println!("valid:  {} (v{})", def.id, def.version);
            println!("nodes:  {}", def.nodes.len());
            println!("edges:  {}", def.edges.len());
            Ok(0)
        }
        Err(errors) => {
            println!("INVALID: {} (v{}) — {} error(s)", def.id, def.version, errors.len());
            for e in errors {
                println!("  - {e}");
            }
            Ok(1)
        }
    }
}
