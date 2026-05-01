//! App-wide shared state managed by Tauri.
//!
//! ThinkingRoot Desktop's state is intentionally minimal: a single
//! optional [`MountedMemory`] handle pointing at the
//! [`thinkingroot_serve::engine::QueryEngine`] for the currently-
//! selected workspace. Chat sessions, agent orchestration, and signing
//! keys (load-bearing in helloroot) are out of scope here — the OSS
//! agent runtime sidecar (Step 10) owns those concerns.

use std::path::PathBuf;
use std::sync::Arc;

use thinkingroot_serve::engine::QueryEngine;
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tokio_util::sync::CancellationToken;

/// All process-wide state owned by Tauri's `app.manage(...)`.
#[derive(Default)]
pub struct AppState {
    /// Lazily-mounted query engine for the active workspace. `None`
    /// until a workspace command (`memory_list`, `brain_load`) needs
    /// to read the graph; remounted on workspace change.
    pub memory: AsyncMutex<Option<MountedMemory>>,
    /// Handle to the managed `root serve` sidecar, populated by
    /// [`crate::agent_runtime_subprocess::spawn`] during setup. The
    /// sidecar process itself runs on a detached tokio task that
    /// outlives this struct; what we keep here is the metadata the
    /// commands and shutdown hook need.
    pub sidecar: AsyncMutex<Option<SidecarHandle>>,
    /// Currently-running compile, if any.  Populated by
    /// `workspace_compile` and consumed by `workspace_compile_stop`
    /// or by the compile task itself on completion.  Holding the
    /// `CancellationToken` here is what makes the desktop "Stop"
    /// button possible — `cancel.cancel()` propagates through the
    /// pipeline orchestrator and aborts in-flight LLM batches.
    pub active_compile: AsyncMutex<Option<CompileHandle>>,
}

/// Live handle for an in-progress workspace compile.
#[derive(Debug, Clone)]
pub struct CompileHandle {
    /// Workspace path being compiled — surfaced by `compile_status`
    /// so the UI can render which workspace is busy.
    pub workspace_label: String,
    /// Tripping this aborts the pipeline at the next phase boundary
    /// (see `thinkingroot_serve::pipeline::run_pipeline_with_cancel`).
    pub cancel: CancellationToken,
}

/// Live metadata for the running sidecar.
///
/// Read by the Step 14 `mcp_status` Tauri command so the Settings
/// pane can show the loopback host/port plus the OS pid that owns
/// the process.  The `child` field carries the actual `tokio::process::Child`
/// so [`crate::agent_runtime_subprocess::shutdown`] can stage a graceful
/// stdin close + wait + SIGKILL escalation rather than relying on
/// `kill_on_drop`.  Wrapped in `Arc<Mutex<Option<...>>>` so the Clone
/// derive still works for the metadata-read paths (`mcp_status`,
/// `chat::*`) which only care about host/port/pid.
#[derive(Debug, Clone)]
pub struct SidecarHandle {
    pub host: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub child: Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>,
}

/// One mounted [`QueryEngine`] paired with the workspace it was
/// constructed against.
pub struct MountedMemory {
    /// Absolute path to the workspace root (the directory that
    /// contains `.thinkingroot/`).
    pub root_path: PathBuf,
    /// User-facing workspace name as registered in the engine.
    pub workspace: String,
    /// The engine itself — read-locked for queries, write-locked when
    /// the workspace pointer changes.
    pub engine: Arc<RwLock<QueryEngine>>,
}
