//! App-wide shared state managed by Tauri.
//!
//! Stream A — `MountedMemory` is gone. Every Tauri command that needs
//! to read or mutate the engine routes through the sidecar's REST
//! surface (`commands/sidecar_client.rs`). The desktop process never
//! opens `graph.db` itself; the daemon stays the single owner per the
//! Cortex Protocol invariant.

use std::collections::HashMap;
use std::sync::Arc;

use thinkingroot_core::types::WorkspaceStatus;
use tokio::sync::{oneshot, Mutex as AsyncMutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::commands::browser::BrowserSession;
use crate::commands::browser_save::ExtractCallbackPayload;
use crate::commands::terminal::TerminalSession;

/// All process-wide state owned by Tauri's `app.manage(...)`.
#[derive(Default)]
pub struct AppState {
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
    /// Slice 0 — cached unified workspace status, keyed by workspace
    /// name. Mirrors what the daemon's status-stream SSE has pushed.
    /// Every UI surface reads from here through the
    /// `workspace_status_get` Tauri command + the `workspace_status:{name}`
    /// Tauri events that fire on every cache update. Removes the five
    /// independent per-view probes the pre-Slice-0 desktop had.
    pub workspace_status: Arc<RwLock<HashMap<String, WorkspaceStatus>>>,
    /// Slice 0 — handle for the live SSE subscriber that mirrors the
    /// daemon's `/status/stream` into [`AppState::workspace_status`].
    /// The slot is `Some` while a subscriber is running; the
    /// `subscribe_workspace_status_stream` command swaps it for a
    /// fresh handle (cancelling the previous one) when the active
    /// workspace changes.
    pub workspace_status_subscriber: AsyncMutex<Option<WorkspaceStatusSubscriberHandle>>,
    /// Substrate Console — handle for the live SSE subscriber that
    /// mirrors the daemon's aggregate `/branch-events/stream` into
    /// the `branch-event` Tauri channel. The slot is `Some` while a
    /// subscriber is running; `branch_event_subscribe` is idempotent
    /// (re-entry returns the existing handle without spawning a
    /// duplicate). Cancelled on workspace unmount or app shutdown.
    pub branch_event_subscriber: AsyncMutex<Option<BranchEventSubscriberHandle>>,
    /// Terminal panel — open PTY sessions keyed by session id (uuid v4
    /// minted by the open command). The desktop's right-rail Terminal
    /// tab spawns one [`TerminalSession`] per UI tab; the entries are
    /// removed on `terminal_close` and on window destroy. Each session
    /// owns its own read thread (PTY → IPC) and a writer half guarded
    /// by a `std::sync::Mutex`. We lean on a sync `RwLock` here because
    /// every consumer is a sync IPC handler; an async lock would pull
    /// us through `block_on` for no benefit.
    pub terminals: Arc<RwLock<HashMap<String, Arc<TerminalSession>>>>,
    /// Browser panel — native child webviews keyed by tab id. These
    /// are real WebViews attached to the main Tauri window, not iframes
    /// inside the app webview, which means sites that block framing
    /// still work. The UI owns the chrome (address bar, tabs, history);
    /// this map owns the native surfaces and keeps them hidden/shown as
    /// the right-rail tab changes.
    pub browsers: Arc<RwLock<HashMap<String, Arc<BrowserSession>>>>,
    /// In-flight "Save Page" extraction requests, keyed by request id.
    /// The `browser_save_page` command inserts a oneshot sender here
    /// before injecting the Readability + Turndown extraction script
    /// into the captive webview; the captive JS calls back into the
    /// `browser_extract_callback` command which removes the sender
    /// and delivers the payload. Senders are removed on success OR
    /// on timeout (the command's `tokio::time::timeout` drops the
    /// receiver, but we also clear the map entry on the timeout path
    /// so subsequent attempts don't see stale state).
    pub pending_extracts:
        AsyncMutex<HashMap<String, oneshot::Sender<ExtractCallbackPayload>>>,
}

/// Live handle for an in-progress workspace compile.
///
/// Not `Clone` because `task: JoinHandle<()>` isn't `Clone`. The
/// handle is owned by `AppState.active_compile` and consumed in one
/// of two ways: the compile task itself takes it on completion to
/// clear the slot, or `workspace_compile_stop` / supersede / the
/// 5 s force-clear path takes it to fire `cancel.cancel()` (which
/// trips the server-side DropGuard via reqwest stream drop) and, on
/// the force-clear path only, calls `task.abort()` to kill a task
/// whose cancel propagation appears wedged. Aborting on the normal
/// stop path would race the clean-shutdown emitter that yields the
/// `Cancelled` Tauri event the UI needs.
#[derive(Debug)]
pub struct CompileHandle {
    /// Workspace path being compiled — surfaced by `compile_status`
    /// so the UI can render which workspace is busy.
    pub workspace_label: String,
    /// Tripping this aborts the pipeline at the next phase boundary
    /// (see `thinkingroot_serve::pipeline::run_pipeline_with_cancel`).
    pub cancel: CancellationToken,
    /// Join handle for the spawned compile task. Stored so the
    /// 5 s force-clear path in `workspace_compile` can abort a
    /// task whose cancel signal didn't propagate — without this
    /// the slot was freed but the task kept running, racing the
    /// next compile's events into the UI.
    pub task: tokio::task::JoinHandle<()>,
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

/// Slice 0 — handle for an active SSE subscriber to the daemon's
/// `/api/v1/workspaces/{name}/status/stream` endpoint. The subscriber
/// task runs on a tokio spawn; tripping `cancel` exits the next
/// receive loop and the task ends cleanly. The handle is recreated
/// on every workspace switch.
#[derive(Debug)]
pub struct WorkspaceStatusSubscriberHandle {
    /// Workspace name the subscriber is currently bound to.
    pub workspace: String,
    /// Cancellation token; cancelled to stop the subscriber.
    pub cancel: CancellationToken,
}

/// Substrate Console — handle for the live SSE subscriber to the
/// daemon's aggregate `/branch-events/stream` endpoint. Singleton
/// per process: every BranchTree / Branch chip in the UI listens to
/// the same `branch-event` Tauri channel; one underlying SSE
/// connection fans out to all of them.
#[derive(Debug)]
pub struct BranchEventSubscriberHandle {
    /// Cancellation token; cancelled to stop the subscriber.
    pub cancel: CancellationToken,
}
