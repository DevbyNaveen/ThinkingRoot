use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::graph::serve_graph;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast};
use axum::http::HeaderValue;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::engine::{ClaimFilter, QueryEngine};
use crate::workspace_state::{Msg as WorkspaceStatusMsg, WorkspaceStateRegistry};
use crate::workspace_watcher::WatcherHandle;
use thinkingroot_core::BranchEvent;
use thinkingroot_core::types::{WorkspaceEvent, WorkspaceState, WorkspaceStatusEvent};

// ─── App State ───────────────────────────────────────────────

pub struct AppState {
    /// Shared engine handle. Wrapped in `Arc<RwLock<…>>` (rather than the
    /// older bare `RwLock<…>`) so the agent loop's `ToolContext` can
    /// clone the same handle into multiple tool handlers without
    /// hopping through `Arc<AppState>`. All existing call sites that
    /// did `state.engine.read().await` keep working unchanged because
    /// `Arc<RwLock<T>>` derefs to `RwLock<T>`.
    ///
    /// **Locking discipline (Tier 3 M, 2026-05-19):** Brain reads and
    /// concurrent compiles are non-blocking by design. The contract:
    ///
    /// 1. `compile_stream` / `run_unified_compile` run the pipeline
    ///    via `run_pipeline_with_options`, which opens its OWN
    ///    `StorageEngine` against `root_path`. The daemon's
    ///    `state.engine` RwLock is **not** held during the pipeline
    ///    body — readers (Brain view, search, AEP probes) flow
    ///    concurrently the entire time the pipeline runs.
    /// 2. The only `state.engine.write().await` acquisition during a
    ///    compile is the post-compile remount inside
    ///    `finalize_successful_compile` — held for the duration of
    ///    `engine.mount(...)` (~100 ms typical). Brain reads briefly
    ///    queue, then immediately see fresh substrate state.
    /// 3. The vector reconcile (post-compile) runs in a `tokio::spawn`
    ///    that takes only `state.engine.read().await` — multiple
    ///    reconciles can run concurrently with Brain reads.
    ///
    /// MVCC-style "readers never block" is therefore already provided
    /// without a substrate refactor; the earlier Brain-freeze symptom
    /// pre-2026-05-18 came from in-process compile holding the engine
    /// write lock for the full pipeline duration. Both root causes
    /// (in-process compile + foreground vector rebuild) shipped fixes
    /// across Tiers 1-3.
    pub engine: Arc<RwLock<QueryEngine>>,
    pub api_key: Option<String>,
    pub mcp_sessions: crate::mcp::sse::SseSessionMap,
    /// Per-agent session state for the intelligent serve layer.
    pub sessions: crate::intelligence::session::SessionStore,
    /// Workspace root path for branch operations.
    ///
    /// Wrapped in `RwLock` so the desktop's mount handler can update it
    /// when the user switches workspaces — branch operations always
    /// target the most-recently-mounted workspace. Read via
    /// `current_workspace_root()`; written via `set_workspace_root()`.
    pub workspace_root: tokio::sync::RwLock<Option<PathBuf>>,
    /// Pending agent-tool approvals, keyed by `tool_use_id`. The
    /// streaming `/ask/stream` handler inserts one entry per write
    /// tool the agent proposes; the `/ask/approval/{id}` POST handler
    /// looks up and fires the matching `oneshot::Sender` so the
    /// agent's `ChannelApprovalGate` unblocks. Both sides bound this
    /// shared map; nothing else writes to it.
    pub pending_approvals: crate::intelligence::approval::PendingApprovalMap,
    /// RARP / Active Engram Protocol manager — owns per-session
    /// materialised Engrams and serves the 4 new MCP tools.
    pub engram_manager: Arc<crate::intelligence::engram::EngramManager>,
    /// T1.6 — per-branch broadcast channels for live SSE event streams.
    /// Keyed by branch name within `workspace_root`.  Each channel is
    /// created lazily on first subscriber, has capacity 64 (branch
    /// events are infrequent — one per merge / abandon /
    /// redaction-update — so the small buffer keeps slow consumers from
    /// blocking the writer), and is reused across reconnects.  The
    /// previously-shipped `BranchEvent` log on `BranchRef` remains the
    /// source of truth; this hub is a fan-out for clients that prefer
    /// live updates over polling `/branches/{branch}/events`.
    pub branch_event_hub: Arc<RwLock<HashMap<String, broadcast::Sender<BranchEvent>>>>,
    /// Phase δ.2 — per-workspace substrate-bus schedulers. Lazy: an
    /// entry is inserted the first time the workspace's substrate
    /// bus is started (via `POST /substrate-bus/start` or the
    /// equivalent Tauri command). Lookup by workspace name; the
    /// `Arc<SubAgentScheduler>` is cheap to clone across handlers
    /// reading the report ring or initiating a `run_now`. Per-
    /// workspace ownership (not process-global) so a multi-workspace
    /// daemon doesn't conflate reports across workspaces.
    pub substrate_bus:
        Arc<RwLock<HashMap<String, Arc<crate::intelligence::substrate_bus::SubAgentScheduler>>>>,
    /// Task 15 — single broadcast channel that fans every branch
    /// event into one aggregate stream. The desktop's left-rail
    /// branch tree subscribes here once and gets create / merge /
    /// abandon events for ALL branches without N per-branch
    /// connections. Capacity 256 because the aggregate sees every
    /// branch's traffic; slow consumers still surface as `lagged`
    /// SSE events. Per-branch fan-out at `branch_event_hub` is
    /// preserved for clients that want a focused stream.
    pub branch_event_aggregate: broadcast::Sender<(String, BranchEvent)>,
    /// HEAD-only updates (`POST /branches/{name}/checkout`) — not a
    /// `BranchEvent` on any `BranchRef`, but UIs must refetch
    /// `/head` + `/branches`. Merged into `/branch-events/stream` as
    /// `event: head_changed` alongside `branch_event`.
    pub head_change_tx: broadcast::Sender<String>,
    /// Unified activity bus. Every subsystem publishes an
    /// `ActivityEvent` here; `/api/v1/ws/{ws}/activity/stream`
    /// subscribes. Capacity 512: the aggregate sees every subsystem's
    /// traffic; slow consumers surface as `lagged` SSE events rather
    /// than blocking publishers. Mirrors `branch_event_aggregate`.
    pub activity_tx: broadcast::Sender<crate::activity::ActivityEvent>,
    /// T1.5 — in-flight merge `CancellationToken`s keyed by merge id
    /// (a ULID generated at handler entry).  `POST /merges/{id}/cancel`
    /// looks up and trips the matching token; the merge phase-boundary
    /// check inside `execute_merge_into_cancellable` returns
    /// `Error::Cancelled` at the next safe point.  Tokens are removed
    /// from the map on every exit path (success, failure, cancellation)
    /// by the merge handler so a long-cancelled merge never leaks.
    pub active_merges: Arc<RwLock<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// C3 (2026-05-22) — in-flight MCP `tools/call`
    /// `CancellationToken`s keyed by the JSON-RPC request id (as a
    /// string). `notifications/cancelled` looks up the matching id
    /// and trips its token; long tool handlers observe the token
    /// at phase boundaries and return `Error::Cancelled` cleanly.
    /// The SSE transport's `handle_post` is the sole writer + reaper.
    /// Tokens are removed on every exit path so a long-cancelled
    /// tool never leaks.
    pub mcp_pending_calls:
        Arc<RwLock<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// C13 (2026-05-22) — in-flight outbound
    /// `sampling/createMessage` calls keyed by the JSON-RPC
    /// request id we minted. When the MCP client POSTs back a
    /// response with the matching id, `mcp::sse::handle_post`
    /// detects it as a response (no `method` field) and routes
    /// to the matching oneshot via
    /// `mcp::sampling::route_incoming_response`. The receiver
    /// awakens + returns the result to the original caller (a
    /// flow `client_sampling` executor, etc.). Entries are
    /// reaped on every exit path (success, timeout, refusal,
    /// session-drop) so a late response never leaks.
    pub mcp_pending_sampling:
        Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>>>,
    /// Slice 3 — optional file-system watcher handle.  When `Some`,
    /// `/api/v1/ws/{ws}/events/stream` subscribes to its broadcast
    /// channel and stateful handlers consult [`WatcherHandle::state`]
    /// to refuse with `Error::WorkspaceOrphaned` once the substrate
    /// disappears.  `None` in the in-process / legacy code paths that
    /// don't run a daemon (CLI `--in-process`, MCP stdio).
    pub workspace_watcher: Arc<RwLock<Option<Arc<WatcherHandle>>>>,
    /// Slice 0 — per-workspace state-machine actor registry. Mount /
    /// unmount / compile / fs-watcher push [`WorkspaceStatusMsg`]s into
    /// the matching actor; the `/api/v1/workspaces/{name}/status` and
    /// `/status/stream` endpoints read from it. The five contradicting
    /// per-view probes (`pack_estimate`, `llm_health`, `mcp_list_connected`,
    /// `workspace_compile_state`, right-rail substrate poll) all collapse
    /// to a single subscriber on this registry.
    pub workspace_status: Arc<WorkspaceStateRegistry>,
    /// Canonical mount table for multi-root source-tree watching and
    /// live sync (name → filesystem root).
    pub mounted_workspace_roots: Arc<RwLock<HashMap<String, PathBuf>>>,
    /// Debounced auto-compile when `compilation.auto_sync` is enabled.
    pub live_sync: Arc<crate::live_sync::LiveSyncScheduler>,
    /// Phase D Wave 1 (2026-05-17) — identity-level permission rules
    /// for the system-power tools. Loaded from
    /// `<config_dir>/thinkingroot/permissions.toml` at startup (or
    /// empty store on first run); shared with the
    /// [`crate::intelligence::permissions_gate::PermissionsGate`]
    /// that wraps the `ToolApprovalRouter` for every agent
    /// invocation. The approval handler at
    /// `POST /api/v1/ws/{ws}/ask/approval/{tool_use_id}` mutates
    /// this when the user clicks "Allow always" / "Deny always" in
    /// the desktop permission prompt.
    pub permission_store: Arc<RwLock<thinkingroot_core::permissions::PermissionStore>>,

    /// Phase 2 central-AI-plan (2026-05-18) — per-tool agentmemory
    /// tokens. Loaded from `<config>/thinkingroot/agentmemory-tokens.json`
    /// at startup. Mutated by `POST /agentmemory/connect` (issue) and
    /// by `agentmemory_auth_check` (last_seen bump). The agentmemory
    /// auth path consults this store BEFORE the legacy
    /// `THINKINGROOT_AGENTMEMORY_SECRET` env var fallback — per-tool
    /// scoping wins when both are present.
    pub agentmemory_tokens: Arc<RwLock<crate::agentmemory::tokens::AgentmemoryTokenStore>>,

    /// Phase 3 central-AI-plan (2026-05-18) — per-MCP-session
    /// telemetry. Parallel to `mcp_sessions` (which holds the SSE
    /// channel for dispatch); this map carries
    /// User-Agent / counters / errors for the "AI Tools" dashboard.
    /// Populated at `handle_sse` open, bumped on each `handle_post`
    /// dispatch, persisted to `<config>/thinkingroot/mcp-sessions.jsonl`
    /// on disconnect.
    pub mcp_session_telemetry: crate::mcp::telemetry::SessionTelemetryMap,
}

impl AppState {
    /// Create a new `AppState` wrapped in `Arc`, initialising a fresh session map.
    /// Backward-compatible — workspace_root defaults to None.
    pub fn new(engine: QueryEngine, api_key: Option<String>) -> Arc<Self> {
        Self::new_with_root(engine, api_key, None)
    }

    /// Create a new `AppState` with an explicit workspace root path for branch operations.
    pub fn new_with_root(
        engine: QueryEngine,
        api_key: Option<String>,
        workspace_root: Option<PathBuf>,
    ) -> Arc<Self> {
        // Phase E.3 (2026-05-17) — register memory_tree MCP tools
        // (export_memory_tree, import_memory_tree). Idempotent —
        // duplicate entries collapse in the tool_trait registry.
        crate::memory_tree::register_memory_tree_tools();
        // Phase 1 of the central-AI-plan (2026-05-18) — register
        // 16 self-heal operator tools so the in-app agent can run
        // doctor / read the recovery log / reset breakers / migrate
        // schemas without bouncing through Tauri commands.
        crate::operator_tools::register_all();
        // JIT capability-acquisition tools (mcp_server_install +
        // skill_define). Same idempotent tool_trait registry; lets the
        // agent acquire tools/skills at runtime.
        crate::acquisition_tools::register_all();
        // Wire the restart-request broadcast channel + an internal
        // subscriber that performs the actual graceful self-exit.
        //
        // The flow on `restart_engine_request`:
        //   1. MCP handler calls `tx.send(AgentInitiated { reason })`.
        //   2. The tool returns immediately so the SSE response can flush.
        //   3. This subscriber wakes, logs the reason, sleeps briefly
        //      (so the in-flight HTTP/SSE responses drain), and calls
        //      `std::process::exit(0)`.
        //   4. The OS service manager (`dev.thinkingroot` launchd /
        //      systemd / Task Scheduler from Phase universal-install)
        //      or the desktop's sidecar watchdog (which sees the
        //      child exit cleanly, NOT as a crash signal) respawns
        //      the daemon.
        //
        // The 500ms sleep is the smallest interval that reliably
        // lets a 1-frame SSE response complete on the loopback
        // socket on every platform — measured during Phase F
        // bring-up.
        let (restart_tx, mut restart_rx) = broadcast::channel(8);
        crate::operator_tools::install_restart_channel(restart_tx);
        tokio::spawn(async move {
            // `broadcast::Receiver::recv` returns Err on channel
            // closure (sender dropped) or on lag. Lag is impossible
            // here — capacity 8, single-producer-per-call — so any
            // error means the channel is dead and we exit the task.
            while let Ok(reason) = restart_rx.recv().await {
                tracing::warn!(
                    target = "operator_tools",
                    ?reason,
                    "restart_engine_request received — initiating graceful self-exit in 500ms"
                );
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                std::process::exit(0);
            }
        });
        Arc::new_cyclic(|weak| {
            let state = Self {
                engine: Arc::new(RwLock::new(engine)),
                api_key,
                mcp_sessions: {
                    // Install the live session map as the process-global
                    // notify target so `mcp_server_install` can broadcast
                    // `tools/list_changed` after a live remount. Mirrors
                    // the `mcp_session_telemetry` install-global pattern
                    // a few fields below.
                    let map = crate::mcp::sse::new_session_map();
                    crate::mcp::sse::install_notify_sessions(map.clone());
                    map
                },
                sessions: crate::intelligence::session::new_session_store(),
                workspace_root: tokio::sync::RwLock::new(workspace_root),
                pending_approvals: crate::intelligence::approval::new_pending_approval_map(),
                engram_manager: crate::intelligence::engram::EngramManager::new(
                    crate::intelligence::engram::EngramConfig::default(),
                ),
                branch_event_hub: Arc::new(RwLock::new(HashMap::new())),
                branch_event_aggregate: broadcast::channel(256).0,
                head_change_tx: broadcast::channel(64).0,
                activity_tx: broadcast::channel(512).0,
                active_merges: Arc::new(RwLock::new(HashMap::new())),
                mcp_pending_calls: Arc::new(RwLock::new(HashMap::new())),
                mcp_pending_sampling: Arc::new(RwLock::new(HashMap::new())),
                workspace_watcher: Arc::new(RwLock::new(None)),
                workspace_status: Arc::new(WorkspaceStateRegistry::new()),
                mounted_workspace_roots: Arc::new(RwLock::new(HashMap::new())),
                live_sync: crate::live_sync::LiveSyncScheduler::new(weak.clone()),
                substrate_bus: Arc::new(RwLock::new(HashMap::new())),
                permission_store: Arc::new(RwLock::new(load_permission_store_or_empty())),
                agentmemory_tokens: Arc::new(RwLock::new(
                    match crate::agentmemory::tokens::AgentmemoryTokenStore::load() {
                        Ok(store) => store,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "agentmemory-tokens.json failed to load — starting with an empty store"
                            );
                            crate::agentmemory::tokens::AgentmemoryTokenStore::empty()
                        }
                    },
                )),
                mcp_session_telemetry: {
                    let map = crate::mcp::telemetry::new_telemetry_map();
                    crate::mcp::telemetry::install_global_map(map.clone());
                    map
                },
            };
            state
        })
    }

    /// Phase δ.2 — Start the substrate-bus scheduler for a workspace.
    /// Idempotent: a second call for the same workspace name is a
    /// no-op (returns the existing scheduler). Returns the scheduler
    /// so callers can immediately invoke `run_now` on a registered
    /// agent if the user requests an on-demand observation.
    pub async fn ensure_substrate_bus(
        self: &Arc<Self>,
        workspace: &str,
    ) -> Arc<crate::intelligence::substrate_bus::SubAgentScheduler> {
        {
            let map = self.substrate_bus.read().await;
            if let Some(existing) = map.get(workspace) {
                return Arc::clone(existing);
            }
        }
        // Slow path: build a fresh scheduler + start it. Done under
        // a fresh write guard so a concurrent caller waiting on the
        // read sees the populated entry.
        let scheduler = Arc::new(
            crate::intelligence::substrate_bus::default_scheduler(),
        );
        let ctx = crate::intelligence::substrate_bus::SubAgentContext {
            engine: Arc::clone(&self.engine),
            workspace: workspace.to_string(),
        };
        scheduler.start(ctx);
        let mut map = self.substrate_bus.write().await;
        map.entry(workspace.to_string())
            .or_insert_with(|| Arc::clone(&scheduler));
        scheduler
    }

    /// Phase δ.2 — Shut down the substrate-bus scheduler for a
    /// workspace (e.g. on unmount). Drops every registered sub-agent
    /// task via the scheduler's shared `CancellationToken`.
    /// Idempotent: dropping a workspace not registered is a no-op.
    pub async fn stop_substrate_bus(&self, workspace: &str) {
        let removed = {
            let mut map = self.substrate_bus.write().await;
            map.remove(workspace)
        };
        if let Some(s) = removed {
            s.shutdown();
        }
    }

    /// Phase δ.2 — Snapshot of recent substrate-bus reports for a
    /// workspace. Returns `Vec::new()` when the bus isn't running.
    /// Honest empty state — the caller (UI) renders "bus not started"
    /// instead of pretending observations exist.
    pub async fn substrate_bus_reports(
        &self,
        workspace: &str,
    ) -> Vec<crate::intelligence::substrate_bus::SubAgentReport> {
        let map = self.substrate_bus.read().await;
        match map.get(workspace) {
            Some(s) => s.recent_reports().await,
            None => Vec::new(),
        }
    }

    /// Slice 3 — install the workspace watcher handle and arm
    /// [`AppState::workspace_state`] reads. Called once by the serve
    /// binary right after `AppState::new_with_root` returns.
    pub async fn attach_workspace_watcher(self: &Arc<Self>, handle: Arc<WatcherHandle>) {
        *self.workspace_watcher.write().await = Some(handle);
    }

    /// Returns the current [`WorkspaceState`]; `Active` when no watcher
    /// is installed (the contract preserved across in-process /
    /// MCP-stdio paths that never spawn one).
    pub async fn workspace_state(&self) -> WorkspaceState {
        let guard = self.workspace_watcher.read().await;
        match guard.as_ref() {
            Some(handle) => *handle.state.read().await,
            None => WorkspaceState::Active,
        }
    }

    /// Subscribe to the workspace event channel. Returns `None` when no
    /// watcher is installed.
    pub async fn subscribe_workspace_events(
        &self,
    ) -> Option<broadcast::Receiver<WorkspaceEvent>> {
        let guard = self.workspace_watcher.read().await;
        guard.as_ref().map(|h| h.tx.subscribe())
    }

    /// Read the current workspace_root path.
    ///
    /// Stream A — replaces direct `&state.workspace_root` reads. Returns
    /// an owned `Option<PathBuf>` so the read lock is released before
    /// the caller does anything with the result.
    pub async fn current_workspace_root(&self) -> Option<PathBuf> {
        self.workspace_root.read().await.clone()
    }

    /// Best-effort display name for the active workspace (its mounted
    /// directory name), or "main" when unknown. Used to stamp activity
    /// events for the Console log.
    pub async fn current_workspace_name(&self) -> String {
        self.current_workspace_root()
            .await
            .and_then(|r| r.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "main".into())
    }

    /// Update the active workspace root.
    ///
    /// Stream A — called by `mount_workspace_handler` after a successful
    /// mount so branch operations target the most-recently-mounted
    /// workspace. The desktop calls `POST /api/v1/workspaces` with the
    /// active workspace path on every `workspace_set_active`, which
    /// transitively flips this pointer.
    pub async fn set_workspace_root(&self, root: Option<PathBuf>) {
        *self.workspace_root.write().await = root;
    }

    /// T1.6 — get-or-create the broadcast channel for a branch.  The
    /// returned `Sender` is cloneable; subscribers call `subscribe()`
    /// on it to obtain a `Receiver`.  Capacity is fixed at 64; on
    /// overflow the oldest events are dropped and slow subscribers see
    /// `RecvError::Lagged` — surfaced to the SSE client as a `lagged`
    /// event so they can refetch via the polling endpoint.
    pub async fn branch_event_sender(&self, branch: &str) -> broadcast::Sender<BranchEvent> {
        if let Some(tx) = self.branch_event_hub.read().await.get(branch).cloned() {
            return tx;
        }
        let mut map = self.branch_event_hub.write().await;
        map.entry(branch.to_string())
            .or_insert_with(|| broadcast::channel(64).0)
            .clone()
    }

    /// Publish an activity event to the live stream and durably append
    /// it to the workspace volume. `send()` errors only when there are
    /// zero subscribers — harmless. The durable append runs on the
    /// blocking pool (best-effort, fire-and-forget) so publishers never
    /// block on disk; it writes under the workspace root (the mounted
    /// volume), NOT config_dir, so history survives container respawns.
    pub async fn publish_activity(&self, event: crate::activity::ActivityEvent) {
        let _ = self.activity_tx.send(event.clone());
        if let Some(root) = self.current_workspace_root().await {
            tokio::task::spawn_blocking(move || {
                let _ = crate::activity::append_event(&root, &event);
            });
        }
    }
}

/// T1.6 — read the latest event for a branch from the on-disk
/// registry and broadcast it on the corresponding channel.
///
/// Called after every successful branch mutation handler.  No-op when
/// the branch has no events (defensive — registries written before
/// the audit log shipped have an empty `events` vector and round-trip
/// via `#[serde(default)]`).
///
/// `broadcast::Sender::send` only fails when the channel has zero
/// receivers; we ignore that error because subscribers may attach at
/// any time and the polling endpoint serves them the full history.
///
/// `pub` so integration tests can drive it directly without wiring
/// every mutation handler into the test setup.  Production code
/// already calls it from inside the rest crate after every successful
/// branch mutation.
/// Invalidate the engram cache for the workspace whose root matches
/// `root`. Best-effort: no-op when the root isn't registered in
/// `mounted_workspace_roots` (e.g. legacy single-workspace boot).
///
/// Mirrors `finalize_successful_compile`'s post-write reconciliation:
/// after main's graph mutates (compile, merge, …) any AEP probe
/// cached against the prior claim ids would resolve to GC'd rows.
/// Same hygiene, applied at the second main-graph-mutating surface
/// (the first being compile).
async fn invalidate_engrams_for_root(state: &AppState, root: &PathBuf) {
    let name_opt = {
        let map = state.mounted_workspace_roots.read().await;
        map.iter()
            .find(|(_, p)| *p == root)
            .map(|(n, _)| n.clone())
    };
    if let Some(name) = name_opt {
        state.engram_manager.invalidate_workspace(&name).await;
    }
}

pub async fn publish_latest_branch_event(state: &AppState, branch: &str) {
    let Some(root) = state.current_workspace_root().await else {
        return;
    };
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::branch::BranchRegistry;
    let Ok(registry) = BranchRegistry::load_or_create(&refs_dir) else {
        return;
    };
    let Some(event) = registry
        .all()
        .into_iter()
        .find(|b| b.name == branch)
        .and_then(|b| b.events.last().cloned())
    else {
        return;
    };
    let tx = state.branch_event_sender(branch).await;
    let _ = tx.send(event.clone());
    // Mirror onto the unified activity bus so the Console Activity tab
    // shows branch ops alongside everything else. Read by reference
    // before the aggregate send below moves `event`.
    {
        use thinkingroot_core::BranchEvent::*;
        let (kind, actor) = match &event {
            Created { actor, .. } => ("branch.created", actor.clone()),
            Merged { actor, .. } => ("branch.merged", actor.clone()),
            Abandoned { actor, .. } => ("branch.abandoned", actor.clone()),
            RedactionUpdated { actor, .. } => ("branch.redaction", actor.clone()),
            PermissionsUpdated { actor, .. } => ("branch.permissions", actor.clone()),
            ContributeBulk { actor, count, .. } => {
                let _ = count;
                ("branch.contribute", actor.clone())
            }
        };
        let ws_name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "main".into());
        state
            .publish_activity(
                crate::activity::ActivityEvent::new(
                    ws_name,
                    crate::activity::ActivityClass::Branch,
                    kind,
                    format!("{branch}"),
                )
                .with_principal(Some(actor))
                .with_detail(serde_json::json!({ "branch": branch })),
            )
            .await;
    }
    // Task 15: also fan into the aggregate channel so the
    // `/branch-events/stream` subscriber sees every branch's events
    // without N per-branch connections. send() returns Err only when
    // there are zero subscribers — harmless to ignore.
    let _ = state
        .branch_event_aggregate
        .send((branch.to_string(), event));
}

// ─── Response Envelope ───────────────────────────────────────

#[derive(Serialize)]
struct ApiResponse<T: Serialize> {
    ok: bool,
    data: Option<T>,
    error: Option<ApiError>,
}

#[derive(Serialize)]
struct ApiError {
    code: String,
    message: String,
}

fn ok_response<T: Serialize>(data: T) -> Json<ApiResponse<T>> {
    Json(ApiResponse {
        ok: true,
        data: Some(data),
        error: None,
    })
}

fn err_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = ApiResponse::<()> {
        ok: false,
        data: None,
        error: Some(ApiError {
            code: code.to_string(),
            message: message.to_string(),
        }),
    };
    (status, Json(body)).into_response()
}

fn request_user(headers: &HeaderMap) -> Option<String> {
    ["x-thinkingroot-user", "x-user"]
        .into_iter()
        .find_map(|name| headers.get(name))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ─── Query Params ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ClaimQueryParams {
    #[serde(rename = "type")]
    pub claim_type: Option<String>,
    pub entity: Option<String>,
    pub min_confidence: Option<f64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Deserialize)]
pub struct SearchQueryParams {
    pub q: String,
    pub top_k: Option<usize>,
}

// ─── Router ──────────────────────────────────────────────────

pub fn build_router(state: Arc<AppState>) -> Router {
    build_router_opts(state, true, true)
}

pub fn build_router_opts(state: Arc<AppState>, enable_rest: bool, enable_mcp: bool) -> Router {
    // CORS is locked to loopback origins. The daemon's threat model
    // assumes loopback binding; if the user explicitly binds to a
    // non-loopback interface they accept that local-only browser
    // SDKs (desktop, CLI tools, local dev pages) keep working while
    // arbitrary third-party origins cannot read API responses via a
    // browser. `Any` would let any page on the web exfiltrate
    // workspace data through a visiting user's session.
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            origin
                .to_str()
                .map(is_loopback_origin)
                .unwrap_or(false)
        }))
        .allow_methods(Any)
        .allow_headers(Any);

    let mut router = Router::new();

    if enable_rest {
        router = router.route("/graph", get(serve_graph));
    }

    if enable_rest {
        let api_routes = Router::new()
            .route(
                "/workspaces",
                get(list_workspaces).post(mount_workspace_handler),
            )
            .route(
                "/workspaces/{name}",
                delete(unmount_workspace_handler),
            )
            // Slice 0 — unified workspace status surface. One source of
            // truth for substrate / sources / mount / llm / compile /
            // branch axes plus pure-derived readiness flags. All five
            // pre-Slice-0 view-side probes collapse to a single
            // subscriber on `/status/stream`.
            .route(
                "/workspaces/{name}/status",
                get(workspace_status_handler),
            )
            .route(
                "/workspaces/{name}/status/stream",
                get(workspace_status_stream_handler),
            )
            .route(
                "/workspaces/{name}/refresh",
                post(workspace_status_refresh_handler),
            )
            .route("/ws/{ws}/entities", get(list_entities))
            .route("/ws/{ws}/entities/{name}", get(get_entity))
            .route("/ws/{ws}/code/search-entity", get(code_search_entity_handler))
            .route("/ws/{ws}/code/entity/{id}", get(code_retrieve_entity_handler))
            .route("/ws/{ws}/code/traverse", post(code_traverse_handler))
            .route("/ws/{ws}/repo-map", post(repo_map_handler))
            .route("/ws/{ws}/summaries", get(summaries_handler))
            .route("/ws/{ws}/summaries/build", post(summaries_build_handler))
            .route("/ws/{ws}/claims", get(list_claims))
            .route("/ws/{ws}/claims/rooted", get(list_rooted_claims_handler))
            // Witness Mesh — new substrate read endpoints. Lives
            // alongside `/claims` during the additive scaffold phase;
            // the Commit-2 reader cutover routes legacy `/claims`
            // consumers here too.
            .route("/ws/{ws}/witnesses", get(list_witnesses_handler))
            .route("/ws/{ws}/witnesses/count", get(witnesses_count_handler))
            .route(
                "/ws/{ws}/witnesses/by-source",
                get(witnesses_by_source_handler),
            )
            .route("/ws/{ws}/witnesses/{id}", get(get_witness_handler))
            .route("/ws/{ws}/witnesses/{id}/walk", get(walk_mesh_handler))
            // Workspace filesystem operations — shared with the
            // desktop FileManager (which has its own Tauri command
            // surface that calls the same `fs_ops` module). The MCP
            // tools `list_directory` / `create_folder` /
            // `rename_path` / `move_paths` dispatch through the same
            // primitives, so AI agents can reorganise a workspace
            // exactly the way a human can.
            .route("/ws/{ws}/fs/list", get(fs_list_handler))
            .route("/ws/{ws}/fs/read", get(fs_read_handler))
            .route("/ws/{ws}/fs/create-folder", post(fs_create_folder_handler))
            .route("/ws/{ws}/fs/rename", post(fs_rename_handler))
            .route("/ws/{ws}/fs/move", post(fs_move_handler))
            // Playground v1 — Living Paper + gaps surface. Both
            // delegate to existing QueryEngine methods (see
            // `engine.rs::regenerate_paper` + `list_gaps_branched`).
            .route("/ws/{ws}/paper/regenerate", post(paper_regenerate_handler))
            // ── Phase β.1 — Cognition Commits ──────────────────────
            // Three endpoints over the new cognition_commits table.
            // Same workspace-scoping pattern as the witness endpoints;
            // each delegates to QueryEngine methods which own the
            // citation/parent verification (so a 400-level bad-request
            // is surfaced as a typed engine error before the table is
            // touched).
            .route(
                "/ws/{ws}/commits",
                get(list_cognition_commits_handler).post(record_cognition_commit_handler),
            )
            // Phase γ.1 — merge-plan endpoint. Pure read; computes the
            // deterministic divergence between two cognition-commit
            // branches. Lives at `/commits/merge-plan` so the
            // `/commits/{id}` route below doesn't shadow it (Axum
            // matches static segments before dynamic placeholders, so
            // routing order is correct without reorder).
            .route(
                "/ws/{ws}/commits/merge-plan",
                get(merge_plan_handler),
            )
            // Phase γ.2 — LLM-driven synthesis on top of a γ.1 plan.
            .route(
                "/ws/{ws}/commits/synthesize-merge",
                post(synthesize_merge_handler),
            )
            .route("/ws/{ws}/commits/{id}", get(get_cognition_commit_handler))
            // Phase δ.2 — Substrate Bus surfaces. Per-workspace.
            .route(
                "/ws/{ws}/substrate-bus/start",
                post(substrate_bus_start_handler),
            )
            .route(
                "/ws/{ws}/substrate-bus/stop",
                post(substrate_bus_stop_handler),
            )
            .route(
                "/ws/{ws}/substrate-bus/reports",
                get(substrate_bus_reports_handler),
            )
            .route(
                "/ws/{ws}/substrate-bus/run-now",
                post(substrate_bus_run_now_handler),
            )
            .route("/ws/{ws}/gaps", get(gaps_handler))
            .route("/ws/{ws}/sources", get(list_sources_handler))
            // ─── Compiled Prompt substrate ───────────────────────────
            .route(
                "/ws/{ws}/prompts",
                get(list_prompts_handler).put(put_prompt_handler),
            )
            .route("/ws/{ws}/prompts/{name}", get(get_prompt_handler))
            .route("/ws/{ws}/prompts/{name}/versions", get(prompt_versions_handler))
            .route("/ws/{ws}/prompts/{name}/assemble", post(assemble_prompt_handler))
            // ─── Compiled capsule (low-token, grounded context payload) ─
            .route("/ws/{ws}/capsule", post(compile_capsule_handler))
            // ─── Capability router: rank tools/functions for an intent ──
            .route("/ws/{ws}/route", post(route_handler))
            .route("/ws/{ws}/route-tools", post(route_tools_handler))
            .route("/ws/{ws}/sleep", post(sleep_handler))
            .route("/ws/{ws}/dream", post(dream_handler))
            .route("/ws/{ws}/predict", post(predict_handler))
            .route("/ws/{ws}/age", get(age_handler))
            .route("/ws/{ws}/drives", get(drives_handler))
            .route("/ws/{ws}/bequeath", post(bequeath_handler))
            .route("/ws/{ws}/inherit", post(inherit_handler))
            // ─── Capability routing report (experience view for Console) ─
            .route("/ws/{ws}/capability-routing", get(capability_routing_handler))
            // ─── Operating-layer artifact nodes (prompts/functions/flows/MCP) ─
            .route("/ws/{ws}/artifact-nodes", get(list_artifact_nodes_handler))
            .route("/ws/{ws}/branch-nodes", get(list_branch_nodes_handler))
            .route("/ws/{ws}/agents/spawn", post(agent_spawn_handler))
            .route("/ws/{ws}/agents/finish", post(agent_finish_handler))
            // ─── Per-user-namespaced store (path-guardable; auto-mounts u_*) ─
            .route("/ws/{ws}/contribute-bulk", post(ws_contribute_handler))
            // ─── Root Functions ──────────────────────────────────────
            .route(
                "/ws/{ws}/functions",
                get(list_functions_handler).put(put_function_handler),
            )
            .route("/ws/{ws}/functions/{name}", get(get_function_handler))
            .route(
                "/ws/{ws}/functions/{name}/caps",
                get(get_function_caps_handler).put(set_function_caps_handler),
            )
            .route(
                "/ws/{ws}/functions/{name}/verdict",
                post(function_verdict_handler),
            )
            .route(
                "/ws/{ws}/functions/{name}/verdicts",
                get(function_verdicts_handler),
            )
            .route("/ws/{ws}/functions/{name}/invoke", post(invoke_function_handler))
            .route("/ws/{ws}/functions/{name}/runs", get(function_runs_handler))
            // Learned retrieval prior (item 10) — read-only learning window.
            .route(
                "/ws/{ws}/learning/retrieval-prior",
                get(retrieval_prior_handler),
            )
            .route(
                "/ws/{ws}/cognition/{token}/answer",
                post(answer_cognition_handler),
            )
            // ─── Flow triggers (headless) ────────────────────────────
            .route("/ws/{ws}/flows", get(list_flows_handler))
            .route("/ws/{ws}/flows/{flow_id}/run", post(run_flow_handler))
            .route("/ws/{ws}/flow-runs/{run_id}", get(flow_run_status_handler))
            // ─── MCP connectors ──────────────────────────────────────
            .route(
                "/ws/{ws}/mcp-servers",
                get(list_mcp_servers_handler).post(install_mcp_server_handler),
            )
            .route(
                "/ws/{ws}/mcp-servers/{name}",
                delete(remove_mcp_server_handler),
            )
            .route("/ws/{ws}/sources/forget", post(forget_source_handler))
            .route("/ws/{ws}/sources/ingest", post(ingest_sources_handler))
            .route("/ws/{ws}/readme", get(workspace_readme_handler))
            .route("/ws/{ws}/relations", get(get_all_relations))
            .route("/ws/{ws}/relations/{entity}", get(get_entity_relations))
            .route("/ws/{ws}/artifacts", get(list_artifacts))
            .route("/ws/{ws}/artifacts/{artifact_type}", get(get_artifact))
            .route("/ws/{ws}/health", get(get_health))
            .route("/ws/{ws}/llm/health", get(llm_health_handler))
            .route("/ws/{ws}/search", get(search))
            .route("/ws/{ws}/search/hybrid", post(hybrid_search_handler))
            // T3.2 — cross-branch reflect.  Body is `{ branches: [...] }`;
            // returns a `CrossBranchReflectResult` JSON with per-branch
            // outcomes + divergent-pattern rows.
            .route(
                "/ws/{ws}/reflect/across-branches",
                post(reflect_across_branches_handler),
            )
            // T2.4 — bitemporal "as-of" claim list.  Query parameter
            // `as_of` carries an ISO-8601 timestamp; the optional
            // `branch` query parameter scopes to a non-main branch.
            // Returns the claims that existed at or before that
            // moment (i.e. their `created_at <= as_of`).
            .route("/ws/{ws}/claims/as-of", get(claims_as_of_handler))
            // Brain probes (parity with MCP `brief` / `investigate`).
            // CLI + Tauri consumers reach the intelligent serve layer over
            // HTTP without needing the MCP SSE transport.  Focus is
            // intentionally not exposed — it mutates per-session
            // `SessionContext.focus_entity`, which is meaningful only to
            // the LLM-mediated MCP loop, never to a stateless caller.
            .route("/ws/{ws}/brain/brief", post(brain_brief_handler))
            .route(
                "/ws/{ws}/brain/investigate",
                post(brain_investigate_handler),
            )
            // RARP / Active Engram Protocol — engram lifecycle endpoints
            // mirror the 4 MCP tools (`materialize_engram`, `probe_engram`,
            // `list_engrams`, `expire_engram`) so HTTP-only consumers
            // (Python/TS SDKs, CLI scripts) reach AEP without an MCP
            // transport. Session id is required and passed via
            // `X-TR-Session-Id` header — matches the SSE-MCP pattern.
            .route(
                "/ws/{ws}/engrams",
                get(list_engrams_handler).post(materialize_engram_handler),
            )
            .route(
                "/ws/{ws}/engrams/{ptr}",
                delete(expire_engram_handler),
            )
            .route(
                "/ws/{ws}/engrams/{ptr}/probe",
                post(probe_engram_handler),
            )
            .route("/ws/{ws}/ask", post(ask_handler))
            .route("/ws/{ws}/ask/stream", post(ask_stream_handler))
            .route(
                "/ws/{ws}/ask/approval/{tool_use_id}",
                post(ask_approval_handler),
            )
            .route("/ws/{ws}/galaxy", get(get_galaxy))
            // Slice 3 — live SSE feed of workspace lifecycle events
            // (FS deletion, graph.db missing, config.toml modified,
            // heartbeats). Subscribers attach to the per-process
            // broadcast channel hosted by the daemon's
            // `workspace_watcher`.
            .route(
                "/ws/{ws}/events/stream",
                get(stream_workspace_events_handler),
            )
            .route("/ws/{ws}/compile", post(compile))
            .route("/ws/{ws}/compile/stream", post(compile_stream))
            // Unified project activity log: live SSE tail + durable history
            // + connected-MCP roster. Powers the Console "Activity" tab.
            .route("/ws/{ws}/activity/stream", get(stream_activity_handler))
            .route("/ws/{ws}/activity", get(list_activity_handler))
            .route("/mcp/sessions", get(list_mcp_sessions_handler))
            // A2 — compiled-not-raw capture: extract atomic claims from a
            // conversation turn/transcript and contribute them (vs storing
            // verbatim "User said: …"). Branch goes in the body so the live
            // session captures into its `stream/{id}` quarantine.
            .route("/ws/{ws}/extract-contribute", post(extract_contribute_handler))
            .route("/ws/{ws}/caption-image", post(caption_image_handler))
            .route("/ws/{ws}/ingest-transcript", post(ingest_transcript_handler))
            // C1 — on-demand consolidation: detect + apply supersessions within
            // each entity's claim cluster (keeps the self-evolving graph clean).
            .route("/ws/{ws}/consolidate", post(consolidate_claims_handler))
            .route("/ws/{ws}/verify", post(verify_ws))
            // Branch endpoints
            .route(
                "/branches",
                get(list_branches_handler).post(create_branch_handler),
            )
            .route("/branches/{branch}/diff", get(diff_branch_handler))
            .route("/branches/{branch}/merge", post(merge_branch_handler))
            .route(
                "/branches/{source}/merge-into/{target}",
                post(merge_into_branch_handler),
            )
            // T1.5 — cancel an in-flight merge by id.  The id is the
            // ULID returned in the merge response; cancellation flips
            // the token so the merge exits with `Error::Cancelled` at
            // the next phase boundary.
            .route("/merges/{id}/cancel", post(cancel_merge_handler))
            .route("/branches/{branch}/rebase", post(rebase_branch_handler))
            .route("/branches/{branch}/rollback", post(rollback_merge_handler))
            .route("/branches/{branch}/checkout", post(checkout_branch_handler))
            .route("/branches/{branch}", delete(delete_branch_handler))
            // T0.7 — connector-attributed bulk contribute with idempotency.
            .route(
                "/branches/{branch}/contribute-bulk",
                post(contribute_bulk_handler),
            )
            // T2.6 — per-branch outbound redaction policy.
            .route(
                "/branches/{branch}/redaction",
                post(set_branch_redaction_handler),
            )
            // T1.3 — branch audit log. Every state-changing mutation
            // appends a `BranchEvent` to the BranchRef's events vec;
            // this route exposes that log read-only.
            .route("/branches/{branch}/events", get(list_branch_events_handler))
            // T1.6 — live SSE stream of branch events.  Subscribers
            // attach to a per-branch broadcast channel; mutations in
            // the registry publish to it after they commit.  Pairs
            // with `/branches/{branch}/events` (history) so a client
            // can backfill on connect, then follow the live stream.
            .route(
                "/branches/{branch}/events/stream",
                get(stream_branch_events_handler),
            )
            // Task 15 — aggregate SSE stream across ALL branches.
            // Pairs with the per-branch stream above; named
            // `/branch-events/stream` (not `/branches/events/stream`)
            // to avoid Axum routing the literal segment "events"
            // into the {branch} path param of the per-branch route.
            .route(
                "/branch-events/stream",
                get(stream_all_branch_events_handler),
            )
            // T1.2 — fast per-branch stats (claims/entities/sources)
            // without running a full diff.
            .route("/branches/{branch}/stats", get(branch_stats_handler))
            // T1.7 — lineage DAG aggregating fork/merge edges across
            // every branch in the registry (active + merged +
            // abandoned).
            .route("/branches/lineage", get(branch_lineage_handler))
            // T2.5 — tag create + list. Writes are rejected by the
            // immutability gate at `engine::ensure_branch_permission`
            // (lives since T0.6); this surface is what gives the gate
            // live data to gate against.
            .route("/tags", get(list_tags_handler).post(create_tag_handler))
            .route("/tags/{name}", get(get_tag_handler))
            // T3.7 — branch templates.  CRUD for the
            // workspace-scoped `branch_templates.toml`; consumed by
            // `POST /branches { template: "..." }` to materialise a
            // pre-baked merge policy / kind / TTL bundle.
            .route(
                "/branch-templates",
                get(list_branch_templates_handler).post(upsert_branch_template_handler),
            )
            .route(
                "/branch-templates/{name}",
                get(get_branch_template_handler).delete(delete_branch_template_handler),
            )
            // T0.4 — Knowledge Proposal lifecycle. The
            // `RequiresProposal` merge gate (`merge.rs:336`) consults
            // `find_approved_proposal` on these files; routes here are
            // the only way to advance a proposal through the
            // open→review→approve states.
            .route(
                "/branches/{branch}/proposals",
                get(list_branch_proposals_handler).post(open_proposal_handler),
            )
            .route("/proposals", get(list_all_proposals_handler))
            .route("/proposals/{id}", get(get_proposal_handler))
            .route(
                "/proposals/{id}/reviews",
                post(review_proposal_handler),
            )
            .route("/proposals/{id}/run-checks", post(run_checks_handler))
            .route("/consolidate", post(consolidate_handler))
            .route("/proposals/{id}/close", post(close_proposal_handler))
            .route("/head", get(get_head_handler));
        router = router.nest("/api/v1", api_routes);
    }

    if enable_mcp {
        let mcp_routes = crate::mcp::sse::build_router(state.clone());
        router = router.nest("/mcp", mcp_routes);
    }

    // Apply the daemon's X-API-Key auth ONLY to routes registered up
    // to this point. `tower::Layer::layer` is order-sensitive: routes
    // added afterwards are not wrapped. The remaining surfaces
    // (`/agentmemory/*` which has its own bearer scheme, the public
    // ops endpoints, and the discovery endpoints) merge in AFTER the
    // auth wrap but BEFORE the CORS wrap so all routes share CORS
    // semantics.
    let auth_routed = router.layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ));

    // Public + self-authenticated routes. `/livez`, `/readyz`,
    // `/metrics`, `/api/v1/version`, `/.well-known/mcp` are
    // unauthenticated discovery / monitoring surfaces. `/agentmemory`
    // carries its own env-gated bearer scheme — see
    // `crate::agentmemory::agentmemory_auth_check`.
    let public_or_self_auth = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/readyz", get(readyz_handler))
        .route("/livez", get(livez_handler))
        .route("/api/v1/version", get(version_handler))
        .route("/.well-known/mcp", get(well_known_mcp_handler))
        .nest("/agentmemory", crate::agentmemory::router(state.clone()));

    // CORS is applied as the OUTERMOST layer so every route
    // (auth-gated, self-authenticated, or fully public) honours the
    // loopback-only origin policy. Pre-fix `/agentmemory/*` was
    // nested after `.layer(cors)` and got no CORS headers at all,
    // breaking legitimate browser SDK calls.
    auth_routed
        .merge(public_or_self_auth)
        .layer(cors)
        .with_state(state)
}

/// Is this `Origin` header value a loopback URL? Used by the CORS
/// `AllowOrigin::predicate` so only same-machine browser surfaces
/// (Tauri webview at `tauri://localhost`, dev tools at
/// `http://localhost:*`, CLI fetches from `http://127.0.0.1:*`)
/// receive `Access-Control-Allow-Origin` headers. Any other origin
/// (an attacker-controlled page on the public web) gets no CORS
/// response and the browser refuses the read.
fn is_loopback_origin(origin: &str) -> bool {
    let after_scheme = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .or_else(|| origin.strip_prefix("tauri://"));
    let Some(authority) = after_scheme else {
        return false;
    };
    // Strip optional port. `[::1]:8080` keeps the bracketed v6
    // literal as the host.
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        // IPv6 literal — find closing bracket.
        match stripped.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        authority.split_once(':').map(|(h, _)| h).unwrap_or(authority)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod cors_origin_tests {
    use super::is_loopback_origin;

    #[test]
    fn loopback_origins_are_allowed() {
        assert!(is_loopback_origin("http://localhost"));
        assert!(is_loopback_origin("http://localhost:31760"));
        assert!(is_loopback_origin("http://127.0.0.1:8080"));
        assert!(is_loopback_origin("http://[::1]:31760"));
        assert!(is_loopback_origin("https://localhost:443"));
        assert!(is_loopback_origin("tauri://localhost"));
    }

    #[test]
    fn non_loopback_origins_are_refused() {
        assert!(!is_loopback_origin("http://evil.example.com"));
        assert!(!is_loopback_origin("http://192.168.1.42"));
        assert!(!is_loopback_origin("https://anthropic.com"));
        assert!(!is_loopback_origin("file:///etc/passwd"));
        assert!(!is_loopback_origin(""));
        assert!(!is_loopback_origin("[malformed"));
    }
}

// ─── Ops endpoints (unauthenticated) ─────────────────────────

async fn livez_handler() -> Response {
    // If this handler runs, the tokio reactor is alive enough to accept
    // requests. No deeper check — that's what /readyz is for.
    (StatusCode::OK, "ok\n").into_response()
}

/// GET `/api/v1/version`
///
/// Identity probe for cortex discovery. Lets a desktop / CLI client
/// detect when the running daemon is a stale binary (different
/// `CARGO_PKG_VERSION` than the bundled one) and decide to respawn
/// rather than attach to a daemon whose handlers might have been
/// fixed in a newer source revision.
///
/// Unauthenticated on purpose — discovery must work before the
/// client knows whether to send credentials.
async fn version_handler() -> Response {
    let body = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
    });
    ok_response(body).into_response()
}

/// GET `/.well-known/mcp`
///
/// JSON manifest for UIs and integrators. The `tools` array is exactly the
/// MCP `tools/list` catalog (names + descriptions + input schemas) so
/// clients like ThinkingRoot Desktop can show the real tool surface without
/// opening an SSE session.
async fn well_known_mcp_handler() -> Response {
    let rpc = crate::mcp::tools::handle_list(None).await;
    let tools = rpc
        .result
        .as_ref()
        .and_then(|r| r.get("tools"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(vec![]));

    let body = serde_json::json!({
        "schema_version": 1,
        "description": "ThinkingRoot MCP catalog (mirrors JSON-RPC tools/list). Client transport: GET /mcp/sse then POST /mcp?sessionId=…",
        "servers": [],
        "tools": tools,
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// Process-global "embed + rerank ONNX models are resident" flag, for an
/// honest `/readyz`. Set true by warm-on-boot (`serve.rs`) once the models
/// are loaded. When `TR_WARM_ON_BOOT=1`, `/readyz` stays 503 until this
/// flips — so the cloud provisioner only checkpoints an engine whose models
/// are already in RAM (warm-snapshot cold-start SOTA). When warm-on-boot is
/// off, readiness keeps its original workspace-only meaning.
static MODELS_WARM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark the embed + rerank models as loaded. Called by warm-on-boot after
/// the models are resident; flips `/readyz` to ready.
pub fn set_models_warm() {
    MODELS_WARM.store(true, std::sync::atomic::Ordering::Release);
}

async fn readyz_handler(State(state): State<Arc<AppState>>) -> Response {
    // Readiness = engine's workspace registry can be read without error,
    // AND (when warm-on-boot is required) the ONNX models are resident.
    // Distinguishes "warming up" from "serving traffic". Cheap; suitable
    // for a 1-second probe cadence.
    let warm_required = std::env::var("TR_WARM_ON_BOOT").as_deref() == Ok("1");
    if warm_required && !MODELS_WARM.load(std::sync::atomic::Ordering::Acquire) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "not-ready: models warming\n",
        )
            .into_response();
    }
    let engine = state.engine.read().await;
    match engine.list_workspaces().await {
        Ok(_) => (StatusCode::OK, "ready\n").into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("not-ready: {e}\n")).into_response(),
    }
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> Response {
    // Prometheus text format 0.0.4. Minimal surface for v0.1 — extended
    // once we wire a histogram backend. HelloRoot's watchdog (spec O-11)
    // is the primary consumer.
    let mut out = String::new();
    out.push_str("# HELP thinkingroot_up Process uptime indicator (always 1 while serving).\n");
    out.push_str("# TYPE thinkingroot_up gauge\n");
    out.push_str("thinkingroot_up 1\n");

    out.push_str("# HELP thinkingroot_build_info Static build information as labels.\n");
    out.push_str("# TYPE thinkingroot_build_info gauge\n");
    out.push_str(&format!(
        "thinkingroot_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION"),
    ));

    // Workspace count — cheap read; bounded by the number of mounted
    // workspaces. Does not iterate entities/claims.
    let engine = state.engine.read().await;
    let ws_count = engine.list_workspaces().await.map(|v| v.len()).unwrap_or(0);
    out.push_str("# HELP thinkingroot_workspaces_total Number of mounted workspaces.\n");
    out.push_str("# TYPE thinkingroot_workspaces_total gauge\n");
    out.push_str(&format!("thinkingroot_workspaces_total {ws_count}\n"));

    // MCP active SSE sessions (ops signal for agent concurrency).
    // `SseSessionMap` is `Arc<Mutex<HashMap<..>>>` — use lock(), not read().
    let mcp_sessions = state.mcp_sessions.lock().await.len();
    out.push_str("# HELP thinkingroot_mcp_sessions_active Live MCP SSE sessions.\n");
    out.push_str("# TYPE thinkingroot_mcp_sessions_active gauge\n");
    out.push_str(&format!(
        "thinkingroot_mcp_sessions_active {mcp_sessions}\n"
    ));

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        out,
    )
        .into_response()
}

// ─── Auth Middleware ──────────────────────────────────────────

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if let Some(ref expected_key) = state.api_key {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        // Constant-time bearer comparison. A naive `==` short-circuits
        // on the first mismatching byte, leaking the expected secret
        // one character at a time over the network. `ConstantTimeEq`
        // compares the full length unconditionally.
        let authorized = match provided {
            Some(key) => {
                use subtle::ConstantTimeEq;
                key.as_bytes().ct_eq(expected_key.as_bytes()).into()
            }
            None => false,
        };
        if !authorized {
            return err_response(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Invalid or missing API key",
            );
        }
    }
    next.run(request).await
}

// ─── Handlers ────────────────────────────────────────────────

async fn list_workspaces(State(state): State<Arc<AppState>>) -> Response {
    let engine = state.engine.read().await;
    match engine.list_workspaces().await {
        Ok(ws) => ok_response(ws).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            &e.to_string(),
        ),
    }
}

// ─── Workspace mount/unmount (cortex-aware tr-mount target) ─────
//
// `POST /api/v1/workspaces` accepts `{ name, root_path }` and mounts
// the workspace into the running daemon's engine. This is the seam
// the `root mount` CLI subcommand uses after unpacking a `.tr` pack
// — the unpacked `<dir>/.thinkingroot/` becomes a workspace the
// cortex daemon can serve to MCP clients (Claude Desktop, Cursor,
// etc.) without restart.
//
// `DELETE /api/v1/workspaces/{name}` is the symmetric unmount. Both
// honour the cortex contract: they mutate `engine.workspaces` under
// the engine write-lock, which serialises with the read paths used
// by every other handler (search, claims, AEP) so a concurrent
// query never observes a half-mounted workspace.

#[derive(Debug, Deserialize)]
struct MountWorkspaceRequest {
    name: String,
    root_path: String,
    /// Optional explicit data directory (defaults to
    /// `<root_path>/.thinkingroot/`). Set this when the data dir
    /// lives outside the workspace root — for example, the tr-mount
    /// flow stages `XDG_DATA_HOME/thinkingroot/mounts/<hash>/` and
    /// passes both as separate paths.
    #[serde(default)]
    data_dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct MountWorkspaceResponse {
    name: String,
    root_path: String,
    entity_count: usize,
    claim_count: usize,
    source_count: usize,
    /// Public REST root for this workspace — clients append entity /
    /// claim / engram paths under this prefix.
    rest_url: String,
    /// MCP SSE endpoint (clients connect over SSE for the standard
    /// RARP/Hybrid tool surface).
    mcp_url: String,
}

async fn mount_workspace_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MountWorkspaceRequest>,
) -> Response {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "BAD_REQUEST", "name is required");
    }
    let root_path = PathBuf::from(&body.root_path);
    if !root_path.is_absolute() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "root_path must be absolute",
        );
    }
    if !root_path.is_dir() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "ROOT_PATH_NOT_DIR",
            &format!("root_path is not a directory: {}", root_path.display()),
        );
    }

    // World-class fix (2026-05-19) — auto-create `<root>/.thinkingroot/`
    // when absent. The desktop's `workspace_add` already does this via
    // `ensure_thinkingroot_data_dir` (apps/.../commands/workspaces.rs);
    // pre-fix the REST mount endpoint rejected bare folders with
    // `NOT_FOUND: no .thinkingroot directory found`, forcing every SDK
    // caller (Python `Brain.mount`, TS `Brain.mount`, raw curl) to
    // shell out to `root init` or hand-create the dir. Mirrors `root
    // init` (directory only; config inherits from global). Idempotent.
    let tr_dir = root_path.join(".thinkingroot");
    if !tr_dir.exists() {
        if let Err(e) = tokio::fs::create_dir_all(&tr_dir).await {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "MOUNT_INIT_FAILED",
                &format!("could not create {}: {e}", tr_dir.display()),
            );
        }
    }

    // Slice 0 — flag the actor that mount is in flight so subscribers
    // see a `Mounting` snapshot instead of jumping NotMounted → Mounted.
    state
        .workspace_status
        .dispatch(&name, root_path.clone(), WorkspaceStatusMsg::MountAttempt)
        .await;

    let mut engine = state.engine.write().await;
    let mount_result = match body.data_dir.as_deref() {
        Some(dd) => {
            let data_dir = PathBuf::from(dd);
            engine
                .mount_with_data_dir(name.clone(), root_path.clone(), data_dir)
                .await
        }
        None => engine.mount(name.clone(), root_path.clone()).await,
    };
    if let Err(e) = mount_result {
        // Slice 0 — propagate the failure to the actor before
        // converting the error into the HTTP response so the desktop's
        // status stream surfaces `MountState::Failed` immediately.
        let reason = format!("{e}");
        state
            .workspace_status
            .dispatch(
                &name,
                root_path.clone(),
                WorkspaceStatusMsg::MountFailed { reason },
            )
            .await;
        return match_engine_error(e);
    }

    // Pull a fresh count so the response carries the substrate size
    // the SDK can show in its connection summary.
    let info = match engine.list_workspaces().await {
        Ok(list) => list.into_iter().find(|w| w.name == name),
        Err(_) => None,
    };
    let (entity_count, claim_count, source_count) = info
        .map(|w| (w.entity_count, w.claim_count, w.source_count))
        .unwrap_or((0, 0, 0));

    drop(engine);

    // Warm-on-mount (cloud cold-start SOTA): the cloud daemon boots with NO
    // workspaces and the provisioner mounts over REST here, so the startup
    // warm-on-boot loop had nothing to warm. Trigger model warm-up now, off
    // the response path, so the first real query is fast and an idle
    // checkpoint captures already-resident ONNX models. Opt-in via
    // TR_WARM_ON_BOOT=1; flips `/readyz` to ready once the models are loaded.
    if std::env::var("TR_WARM_ON_BOOT").as_deref() == Ok("1") {
        let engine_arc = state.engine.clone();
        let ws = name.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            match engine_arc.read().await.warm_models(&ws).await {
                Ok(()) => {
                    set_models_warm();
                    tracing::info!(
                        workspace = %ws,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "warm-on-mount: models loaded"
                    );
                }
                Err(e) => {
                    tracing::warn!(workspace = %ws, error = %e, "warm-on-mount failed")
                }
            }
        });
    }

    // Slice 0 — push live counts into the actor. The state machine
    // moves to `MountState::Mounted` and `SubstrateState::Populated`
    // (or `Empty` when claim_count == 0). All views read from the
    // resulting snapshot — no more per-view probes.
    let graph_db_bytes = match tokio::fs::metadata(
        root_path.join(".thinkingroot").join("graph").join("graph.db"),
    )
    .await
    {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    state
        .workspace_status
        .dispatch(
            &name,
            root_path.clone(),
            WorkspaceStatusMsg::MountSucceeded {
                claim_count: claim_count as u64,
                entity_count: entity_count as u64,
                source_count_at_last_compile: source_count as u64,
                graph_db_bytes,
            },
        )
        .await;

    // Bugfix 2026-05-10 — also push an LLM probe state so the actor's
    // `llm` axis stops reading `Unconfigured` once mount has produced
    // a usable LlmClient. Pre-fix the actor stayed at the initial
    // `Unconfigured` forever; daemon restart silently broke the chat
    // gates on every previously-compiled workspace. Honest decision:
    // we have credentials in shape (LlmClient was constructible) but
    // no fresh probe in this session, so `Configured` is the truthful
    // axis state — the periodic reconcile tick at `workspace_state.rs:321`
    // would decay any `Healthy` we tried to fabricate here back to
    // `Configured` within `LLM_HEALTH_WINDOW`, so we save the flicker
    // and dispatch the durable answer directly.
    let llm_summary = {
        let engine = state.engine.read().await;
        engine.workspace_llm_summary(&name)
    };
    if let Some((provider, model)) = llm_summary {
        state
            .workspace_status
            .dispatch(
                &name,
                root_path.clone(),
                WorkspaceStatusMsg::LlmProbed {
                    state: thinkingroot_core::types::LlmState::Configured {
                        provider,
                        model: Some(model),
                    },
                },
            )
            .await;
    }

    // Emit RARP-aware invalidation so any pre-existing engrams pinned
    // to a same-named workspace are dropped — defends against the
    // "remount under the same name returns stale claim ids" case.
    state.engram_manager.invalidate_workspace(&name).await;

    // Stream A — flip the daemon's workspace_root pointer so branch
    // operations target the most-recently-mounted workspace. The desktop
    // calls this on every workspace_set_active so the daemon and the
    // desktop's idea of "active workspace" stay in lockstep without
    // requiring a daemon restart.
    state.set_workspace_root(Some(root_path.clone())).await;

    state
        .mounted_workspace_roots
        .write()
        .await
        .insert(name.clone(), root_path.clone());
    state
        .live_sync
        .register_workspace(&name, root_path.clone())
        .await;

    // Ship 3B (2026-05-20) — auto-start the substrate-bus scheduler
    // on mount so the Reconciler / GapHunter / Curator / Watcher
    // sub-agents actually run in the background. Pre-fix the bus was
    // opt-in via a separate `/substrate_bus/start` HTTP endpoint that
    // no UI surface ever called, so the sub-agents emitted zero
    // reports in normal use and the chat agent's
    // `<sub_agent_digest>` reminder block stayed silent regardless of
    // workspace activity. `ensure_substrate_bus` is idempotent — a
    // second call on the same workspace returns the existing
    // scheduler without spawning a duplicate.
    let _scheduler = state.ensure_substrate_bus(&name).await;

    let rest_url = format!("/api/v1/ws/{name}/");
    let mcp_url = "/mcp/sse".to_string();
    ok_response(MountWorkspaceResponse {
        name,
        root_path: root_path.display().to_string(),
        entity_count,
        claim_count,
        source_count,
        rest_url,
        mcp_url,
    })
    .into_response()
}

async fn unmount_workspace_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let mut engine = state.engine.write().await;
    if let Err(e) = engine.unmount(&name) {
        return match_engine_error(e);
    }
    drop(engine);

    state.engram_manager.invalidate_workspace(&name).await;

    state.mounted_workspace_roots.write().await.remove(&name);
    state.live_sync.unregister_workspace(&name).await;

    // Slice 0 — flip the status actor to `NotMounted`. The actor
    // remains live so subscribers keep their stream until the
    // workspace is genuinely removed (the registry remove is the
    // disposal path).
    if let Some(actor) = state.workspace_status.get(&name).await {
        let _ = actor.send(WorkspaceStatusMsg::Unmounted).await;
    }

    ok_response(serde_json::json!({ "unmounted": name })).into_response()
}

// ─── RARP / Active Engram Protocol REST endpoints ───────────────
//
// These mirror the 4 MCP tools (`materialize_engram`, `probe_engram`,
// `list_engrams`, `expire_engram`) so HTTP-only consumers can reach
// the AEP read path. Session id is mandatory and travels in the
// `X-TR-Session-Id` header — same lifetime contract as the SSE-MCP
// session: idle TTL eviction, cache-dirty invalidation, max engrams
// per session enforced by `EngramManager`.

const SESSION_HEADER: &str = "X-TR-Session-Id";

fn require_session_id(headers: &HeaderMap) -> Result<String, Response> {
    match headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(err_response(
            StatusCode::BAD_REQUEST,
            "MISSING_SESSION",
            &format!("{SESSION_HEADER} header is required"),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct MaterializeEngramRequest {
    topic: String,
    /// Optional explicit seed entity ids; falls back to a vector
    /// search against the workspace if absent. Mirrors the MCP
    /// behaviour at mcp/tools.rs::handle_materialize_engram.
    #[serde(default)]
    seed_entity_ids: Option<Vec<String>>,
    /// Optional engram scope (depth_hops, event_window_days,
    /// clearance, seed_claim_ids, score_with_hybrid).
    #[serde(default)]
    scope: Option<serde_json::Value>,
}

async fn materialize_engram_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MaterializeEngramRequest>,
) -> Response {
    let session_id = match require_session_id(&headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let topic = body.topic.trim().to_string();
    if topic.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "BAD_REQUEST", "topic is required");
    }

    let engine = state.engine.read().await;
    let seed_entity_ids = match body.seed_entity_ids {
        Some(ids) => ids,
        None => match engine.search(&ws, &topic, 10).await {
            Ok(result) => result.entities.into_iter().map(|e| e.id).collect(),
            Err(e) => return match_engine_error(e),
        },
    };
    if seed_entity_ids.is_empty() {
        return err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "NO_ANCHORS",
            &format!("no semantic anchors for topic '{topic}'"),
        );
    }

    let scope = crate::mcp::tools::parse_scope(body.scope.as_ref());

    let graph = match engine.graph_store(&ws).await {
        Some(g) => g,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "WORKSPACE_NOT_MOUNTED",
                &format!("workspace '{ws}' not mounted"),
            );
        }
    };

    match state
        .engram_manager
        .materialize_engram(&session_id, &ws, &topic, &graph, seed_entity_ids, scope, None)
        .await
    {
        Ok((pointer, summary)) => ok_response(serde_json::json!({
            "pointer": pointer,
            "summary": &*summary,
        }))
        .into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Debug, Deserialize)]
struct ProbeEngramRequest {
    question: String,
    #[serde(default)]
    clearance: Option<Vec<String>>,
    #[serde(default)]
    probe_kind: Option<String>,
    /// AEP × Hybrid composition flag. When `true`, the probe answer's
    /// rows are reordered by `hybrid_retrieve` before being returned.
    /// Spec: docs/2026-05-02-hybrid-retrieval-spec.md §11.
    #[serde(default)]
    score_with_hybrid: bool,
}

async fn probe_engram_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, ptr)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ProbeEngramRequest>,
) -> Response {
    let session_id = match require_session_id(&headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if body.question.trim().is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "BAD_REQUEST", "question is required");
    }

    let engine = state.engine.read().await;
    let graph = match engine.graph_store(&ws).await {
        Some(g) => g,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "WORKSPACE_NOT_MOUNTED",
                &format!("workspace '{ws}' not mounted"),
            );
        }
    };
    let byte_store = match engine.byte_store(&ws) {
        Some(b) => b,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "WORKSPACE_NO_BYTE_STORE",
                &format!("workspace '{ws}' has no byte store"),
            );
        }
    };

    let clearance: Option<Vec<thinkingroot_core::types::Sensitivity>> = body
        .clearance
        .as_ref()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| crate::mcp::tools::parse_sensitivity_str(s))
                .collect()
        });
    let probe_kind = body
        .probe_kind
        .as_deref()
        .and_then(crate::mcp::tools::parse_probe_kind_str);

    let probe_clearance = clearance.clone();
    let mut answer = match state
        .engram_manager
        .probe_engram(
            &session_id,
            &ptr,
            &body.question,
            clearance,
            &graph,
            byte_store.as_ref(),
            probe_kind,
        )
        .await
    {
        Ok(a) => a,
        Err(e) => return match_engine_error(e),
    };

    if body.score_with_hybrid && !answer.claim_ids.is_empty() {
        let req = crate::engine::RetrievalRequest {
            query_text: body.question.clone(),
            typed_predicates: vec![],
            session_id: session_id.clone(),
            clearance: probe_clearance
                .unwrap_or_else(|| vec![thinkingroot_core::types::Sensitivity::Public]),
            top_k: answer.claim_ids.len(),
            time_window: None,
            scoring_profile: crate::engine::ScoringProfile::default(),
            require_certificate: false,
            include_test_origin: true,
            include_quarantined: false,
            require_provenance_verified: false,
            now: None,
            scoped_claim_ids: Some(answer.claim_ids.clone()),
            branch: None,
        };
        match engine.hybrid_retrieve(&ws, req, None).await {
            Ok(resp) => {
                let new_order: Vec<String> =
                    resp.hits.iter().map(|h| h.claim_id.clone()).collect();
                crate::mcp::tools::reorder_probe_answer_in_place(&mut answer, &new_order);
            }
            Err(e) => {
                // Fall back to Datalog order rather than failing the
                // probe — matches the MCP path's tolerant behaviour.
                tracing::warn!("hybrid composition fallback: {e}");
            }
        }
    }

    ok_response(answer).into_response()
}

async fn list_engrams_handler(
    State(state): State<Arc<AppState>>,
    Path(_ws): Path<String>,
    headers: HeaderMap,
) -> Response {
    let session_id = match require_session_id(&headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let refs = state.engram_manager.list_engrams(&session_id).await;
    ok_response(refs).into_response()
}

async fn expire_engram_handler(
    State(state): State<Arc<AppState>>,
    Path((_ws, ptr)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let session_id = match require_session_id(&headers) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let expired = state.engram_manager.expire_engram(&session_id, &ptr).await;
    ok_response(serde_json::json!({ "expired": expired, "pointer": ptr })).into_response()
}

async fn list_entities(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.list_entities(&ws).await {
        Ok(entities) => ok_response(entities).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// Stream A — `GET /api/v1/ws/{ws}/sources`. Lists every source row in
/// the workspace (id, uri, source_type). Backs the desktop's privacy
/// dashboard and any consumer that needs to enumerate sources without
/// loading their claims.
async fn list_sources_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.list_sources(&ws).await {
        Ok(sources) => ok_response(sources).into_response(),
        Err(e) => match_engine_error(e),
    }
}

// ─── Compiled Prompt handlers ────────────────────────────────────────────────

async fn list_prompts_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.prompt_list_latest(&ws).await {
        Ok(v) => ok_response(v).into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct PutPromptBody {
    name: String,
    template_text: String,
}

async fn put_prompt_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<PutPromptBody>,
) -> Response {
    let engine = state.engine.read().await;
    match engine
        .prompt_put_template(&ws, &body.name, &body.template_text)
        .await
    {
        Ok(t) => ok_response(t).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_prompt_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.prompt_get_latest(&ws, &name).await {
        Ok(Some(t)) => ok_response(t).into_response(),
        Ok(None) => err_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("prompt template '{name}' not found"),
        ),
        Err(e) => match_engine_error(e),
    }
}

async fn prompt_versions_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.prompt_list_versions(&ws, &name).await {
        Ok(v) => ok_response(v).into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct AssemblePromptBody {
    #[serde(default)]
    vars: std::collections::BTreeMap<String, String>,
}

async fn assemble_prompt_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
    Json(body): Json<AssemblePromptBody>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.assemble_prompt(&ws, &name, &body.vars).await {
        Ok(s) => ok_response(serde_json::json!({ "prompt": s })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/capsule` — compile (or serve from cache) a
/// witness-grounded context capsule for one turn. Body is a `CapsuleSpec`
/// (`prompt_name`, `query`, optional `vars`/`branch`/`top_k`/`max_tools`/
/// `session_id`). Returns a `CompiledCapsule` whose `token_estimate`
/// shows the payload is a fraction of raw context and whose `cache_hit`
/// flags the warm path.
async fn compile_capsule_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(spec): Json<crate::engine::CapsuleSpec>,
) -> Response {
    if let Err(resp) = ensure_user_ws(&state, &ws).await {
        return resp;
    }
    let engine = state.engine.read().await;
    // M4 — when a session_id is supplied this is a live streaming-branch
    // turn: reuse the session's warm frame so only retrieval runs. Without
    // one, fall back to the shared query-keyed cache path (M1).
    let result = match spec.session_id.clone() {
        Some(session_id) => {
            engine
                .compile_capsule_session(&ws, &state.sessions, &session_id, spec)
                .await
        }
        None => engine.compile_capsule(&ws, spec).await,
    };
    match result {
        Ok(capsule) => ok_response(capsule).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `GET /api/v1/ws/{ws}/artifact-nodes` — the operating-layer nodes (prompts,
/// functions, flows, MCP servers/tools) the brain runs on, with their edges,
/// read straight from the cognition graph. (Distinct from `/artifacts`, which
/// lists on-disk compile-artifact types.)
async fn list_artifact_nodes_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.list_operating_artifacts(&ws).await {
        Ok(arts) => ok_response(arts).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `GET /api/v1/ws/{ws}/branch-nodes` — the brain's view of its own durable
/// branch topology: typed branch nodes (status/parent/kind/timestamps) synced
/// from the branch registry. Ephemeral `stream/*` branches are absent by design.
async fn list_branch_nodes_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.list_branch_nodes(&ws).await {
        Ok(nodes) => ok_response(serde_json::json!({ "branches": nodes })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

// ─── Multi-agent branch-brain lifecycle ───────────────────────────────────

#[derive(Deserialize)]
struct AgentSpawnRequest {
    agent_id: String,
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /api/v1/ws/{ws}/agents/spawn` — spawn an agent = fork its own
/// `agent/{id}` branch-brain (RequiresProposal-gated). Returns the branch name.
async fn agent_spawn_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<AgentSpawnRequest>,
) -> Response {
    let engine = state.engine.read().await;
    match engine
        .spawn_agent_branch(&ws, &body.agent_id, body.parent.as_deref())
        .await
    {
        Ok(branch) => ok_response(serde_json::json!({ "branch": branch })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct AgentFinishRequest {
    agent_id: String,
    #[serde(default)]
    min_reviewers: Option<u8>,
    #[serde(default)]
    auto_merge: Option<bool>,
}

/// `POST /api/v1/ws/{ws}/agents/finish` — finish an agent = gated merge-back of
/// its branch-brain into the shared brain (verify-before-merge). Returns the
/// honest merge report.
async fn agent_finish_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<AgentFinishRequest>,
) -> Response {
    let engine = state.engine.read().await;
    match engine
        .finish_agent_branch(
            &ws,
            &body.agent_id,
            body.min_reviewers.unwrap_or(0),
            body.auto_merge.unwrap_or(true),
        )
        .await
    {
        Ok(report) => ok_response(serde_json::json!({ "report": report })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

// ─── Root Function handlers ───────────────────────────────────────────────

async fn list_functions_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.list_functions(&ws).await {
        Ok(v) => ok_response(v).into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct PutFunctionBody {
    name: String,
    body: String,
    #[serde(default)]
    language: Option<String>,
}

async fn put_function_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(payload): Json<PutFunctionBody>,
) -> Response {
    let engine = state.engine.read().await;
    let lang = payload.language.as_deref().unwrap_or("js");
    match engine.put_function(&ws, &payload.name, &payload.body, lang).await {
        Ok(f) => ok_response(f).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_function_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_function(&ws, &name).await {
        Ok(Some(f)) => ok_response(f).into_response(),
        Ok(None) => err_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("root function '{name}' not found"),
        ),
        Err(e) => match_engine_error(e),
    }
}

/// A1 — read the effective capability grants for a function. `explicit` is
/// `true` when a stored grant exists (else the unrestricted default is shown).
async fn get_function_caps_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_function_caps(&ws, &name).await {
        Ok((caps, explicit)) => {
            ok_response(serde_json::json!({ "caps": caps, "explicit": explicit })).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

/// A1 — set the capability grants for a function. The body is the full
/// CapSet document; any capability omitted deserialises to `false` (a grant
/// can only narrow, never silently widen — see CapSet's serde contract).
async fn set_function_caps_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
    Json(caps): Json<crate::engine::CapSet>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.set_function_caps(&ws, &name, caps).await {
        Ok(()) => ok_response(serde_json::json!({ "caps": caps, "explicit": true })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct VerdictBody {
    #[serde(default)]
    input: serde_json::Value,
    passed: bool,
    #[serde(default)]
    detail: String,
}

/// A6 — read recent verification verdicts for a function (newest-first).
/// Powers the Console learning view: shows where a function was caught
/// answering wrong even though the run completed.
async fn function_verdicts_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.function_verdicts(&ws, &name, 50).await {
        Ok(v) => ok_response(v).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// GET /ws/{ws}/learning/retrieval-prior — the per-tenant learn-to-rank
/// window: whether the learned prior is enabled, how many claims have an
/// accumulated usefulness, and the top ones by Wilson-scored citation rate.
async fn retrieval_prior_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.retrieval_prior_summary(&ws, 20).await {
        Ok(v) => ok_response(v).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// A6 — record a verification verdict (forge test-case outcome) for a
/// function. Corrects the router's learned experience: a run that completed
/// with the WRONG answer was over-credited by the invoke path; the failed
/// verdict applies the missing negative evidence. Verdicts persist durably
/// (the idle trainer + Console read them).
async fn function_verdict_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
    Json(body): Json<VerdictBody>,
) -> Response {
    let engine = state.engine.read().await;
    match engine
        .record_function_verdict(&ws, &name, &body.input, body.passed, &body.detail)
        .await
    {
        Ok(()) => ok_response(serde_json::json!({ "recorded": true, "passed": body.passed }))
            .into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct InvokeFunctionBody {
    #[serde(default)]
    input: serde_json::Value,
    /// A2 — branch-scoped invoke: route this run's `memory.remember`
    /// writes to this branch (forked from main if absent) instead of main.
    #[serde(default)]
    target_branch: Option<String>,
    /// A2 — run on a fresh ephemeral branch that is abandoned after the run
    /// (a true dry run: side effects happen in isolation, then vanish).
    #[serde(default)]
    dry_run: bool,
}

async fn invoke_function_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
    Json(payload): Json<InvokeFunctionBody>,
) -> Response {
    let engine = state.engine.read().await;
    state
        .publish_activity(
            crate::activity::ActivityEvent::new(
                ws.clone(),
                crate::activity::ActivityClass::Function,
                "fn.invoked",
                format!("invoke {name}"),
            )
            .with_detail(serde_json::json!({ "function": name })),
        )
        .await;
    let invoke_result = if payload.target_branch.is_some() || payload.dry_run {
        let opts = crate::engine::InvokeBranchOpts {
            target_branch: payload.target_branch.clone(),
            dry_run: payload.dry_run,
        };
        engine.invoke_function_with_opts(&ws, &name, &payload.input, opts).await
    } else {
        engine.invoke_function(&ws, &name, &payload.input).await
    };
    match invoke_result {
        Ok(v) => {
            state
                .publish_activity(
                    crate::activity::ActivityEvent::new(
                        ws.clone(),
                        crate::activity::ActivityClass::Function,
                        "fn.result",
                        format!("{name} · ok"),
                    )
                    .with_detail(serde_json::json!({ "function": name })),
                )
                .await;
            ok_response(serde_json::json!({ "result": v })).into_response()
        }
        Err(e) => {
            state
                .publish_activity(
                    crate::activity::ActivityEvent::new(
                        ws.clone(),
                        crate::activity::ActivityClass::Error,
                        "fn.error",
                        format!("{name} · {e}"),
                    )
                    .with_detail(serde_json::json!({ "function": name })),
                )
                .await;
            match_engine_error(e)
        }
    }
}

async fn function_runs_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.list_function_runs(&ws, &name).await {
        Ok(v) => ok_response(v).into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct AnswerCognitionBody {
    #[serde(default)]
    answer: serde_json::Value,
}

/// Answer a suspended Root Function's pending `ctx.cognition.ask` (by
/// token) and resume the run. Returns the resumed result (a value, or
/// another `{ _suspended, token, question }` marker if it asks again).
async fn answer_cognition_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, token)): Path<(String, String)>,
    Json(payload): Json<AnswerCognitionBody>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.answer_cognition(&ws, &token, &payload.answer).await {
        Ok(v) => ok_response(serde_json::json!({ "result": v })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

// ─── Flow triggers (headless / cloud) ─────────────────────────────────────

/// Build the executor registry for a **headless** flow run — the set that works
/// without a connected AI client: deterministic, local LLM, MCP tool, auto-
/// approved human, and Root Function. `ClientSampling` is intentionally absent
/// (those nodes sample the connected client, which a REST/cron/webhook trigger
/// has none of) — a flow that uses one fails with a clear "no executor"
/// error instead of hanging. Triggering such flows still works over MCP.
pub(crate) async fn build_headless_executors(
    state: &Arc<AppState>,
) -> thinkingroot_flow::runtime::Executors {
    use thinkingroot_flow::runtime::{Executors, NodeTypeKind};
    let exec_registry =
        thinkingroot_flow::executors::deterministic::DeterministicRegistry::with_builtins();
    let executors = Executors::default();
    executors
        .register(
            NodeTypeKind::Deterministic,
            std::sync::Arc::new(
                thinkingroot_flow::executors::deterministic::DeterministicExecutor::new(
                    exec_registry,
                ),
            ),
        )
        .await;
    executors
        .register(
            NodeTypeKind::LocalLlm,
            std::sync::Arc::new(crate::flow_executors::local_llm::LocalLlmExecutor::new(
                state.engine.clone(),
            )),
        )
        .await;
    executors
        .register(
            NodeTypeKind::McpTool,
            std::sync::Arc::new(crate::flow_executors::mcp_tool::McpToolExecutor::new(
                state.engine.clone(),
                state.sessions.clone(),
                state.engram_manager.clone(),
                state.clone(),
            )),
        )
        .await;
    executors
        .register(
            NodeTypeKind::Human,
            std::sync::Arc::new(crate::flow_executors::human::HumanExecutor::new(
                std::sync::Arc::new(crate::intelligence::approval::AutoApprove),
            )),
        )
        .await;
    executors
        .register(
            NodeTypeKind::RootFunction,
            std::sync::Arc::new(
                crate::flow_executors::root_function::RootFunctionExecutor::new(
                    state.engine.clone(),
                ),
            ),
        )
        .await;
    executors
}

fn flow_store_for(state_root: std::path::PathBuf) -> thinkingroot_flow::storage::FlowStore {
    thinkingroot_flow::storage::FlowStore::new(state_root)
}

#[derive(Debug, Deserialize)]
struct RunFlowBody {
    /// Inputs validated against the flow definition's `inputs` schema at start.
    #[serde(default)]
    inputs: serde_json::Value,
}

/// `GET /api/v1/ws/{ws}/flows` — list flow definitions.
async fn list_flows_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let root = {
        let engine = state.engine.read().await;
        match engine.workspace_root_path(&ws) {
            Some(p) => p,
            None => {
                return err_response(
                    StatusCode::NOT_FOUND,
                    "ws_not_mounted",
                    &format!("workspace '{ws}' not mounted"),
                )
            }
        }
    };
    match flow_store_for(root).list_flow_definitions() {
        Ok(defs) => ok_response(defs).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, "list_flows_failed", &e.to_string()),
    }
}

/// `POST /api/v1/ws/{ws}/flows/{flow_id}/run` — start a headless run. Returns
/// the flow_run_id immediately; the run executes asynchronously. Poll
/// `GET /flow-runs/{id}` for status.
async fn run_flow_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, flow_id)): Path<(String, String)>,
    Json(body): Json<RunFlowBody>,
) -> Response {
    let root = {
        let engine = state.engine.read().await;
        match engine.workspace_root_path(&ws) {
            Some(p) => p,
            None => {
                return err_response(
                    StatusCode::NOT_FOUND,
                    "ws_not_mounted",
                    &format!("workspace '{ws}' not mounted"),
                )
            }
        }
    };
    let store = flow_store_for(root);
    let executors = build_headless_executors(&state).await;
    let runtime = thinkingroot_flow::runtime::FlowRuntime::new(store, executors);
    match runtime
        .start_run_for_session(&flow_id, &ws, "main", body.inputs, None)
        .await
    {
        Ok(handle) => ok_response(serde_json::json!({
            "flow_run_id": handle.flow_run_id,
            "status": "running",
            "started_at": handle.started_at.to_rfc3339(),
        }))
        .into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, "flow_run_failed", &e.to_string()),
    }
}

/// `GET /api/v1/ws/{ws}/flow-runs/{run_id}` — a run's current status/outputs.
async fn flow_run_status_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, run_id)): Path<(String, String)>,
) -> Response {
    let root = {
        let engine = state.engine.read().await;
        match engine.workspace_root_path(&ws) {
            Some(p) => p,
            None => {
                return err_response(
                    StatusCode::NOT_FOUND,
                    "ws_not_mounted",
                    &format!("workspace '{ws}' not mounted"),
                )
            }
        }
    };
    match flow_store_for(root).get_flow_run(&run_id) {
        Ok(Some(r)) => ok_response(r).into_response(),
        Ok(None) => err_response(
            StatusCode::NOT_FOUND,
            "flow_run_not_found",
            &format!("flow run '{run_id}' not found"),
        ),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "flow_run_status_failed",
            &e.to_string(),
        ),
    }
}

// ─── MCP connector handlers ───────────────────────────────────────────────

async fn list_mcp_servers_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.list_mcp_servers(&ws).await {
        Ok(v) => ok_response(v).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn install_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // Validate + parse the connector spec with the same checks the MCP
    // `mcp_server_install` tool uses.
    let entry = match crate::acquisition_tools::parse_and_validate(&body) {
        Ok(e) => e,
        Err(msg) => return err_response(StatusCode::BAD_REQUEST, "invalid_spec", &msg),
    };
    let engine = state.engine.read().await;
    match engine.install_mcp_server(&ws, entry).await {
        Ok(count) => ok_response(serde_json::json!({ "server_count": count })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn remove_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.remove_mcp_server(&ws, &name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("connector '{name}' not installed"),
        ),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Serialize)]
struct ReadmeResponse {
    /// Engine-canonical workspace README markdown (the contents of
    /// `<workspace_root>/.thinkingroot/README.md`). Empty string when
    /// the file does not exist — honest empty state, never a 404 (the
    /// workspace itself exists; a missing README is just a no-op
    /// surface, not a not-found condition).
    readme: String,
}

/// `GET /api/v1/ws/{ws}/readme`. Returns the engine-canonical workspace
/// README synthesised by Phase 10 of the compile pipeline. Backs the
/// desktop's right-rail Readme tab and any consumer that wants to render
/// a workspace overview without reissuing the per-substrate aggregate
/// queries on every request.
///
/// Workspace must be mounted (otherwise `404 NOT_FOUND`). README file
/// missing returns `200 { readme: "" }` — the workspace is healthy, the
/// README is just stale or never compiled.
async fn workspace_readme_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let root = {
        let engine = state.engine.read().await;
        match engine.workspace_root_path(&ws) {
            Some(p) => p,
            None => {
                return err_response(
                    StatusCode::NOT_FOUND,
                    "WORKSPACE_NOT_MOUNTED",
                    &format!("workspace '{ws}' not mounted"),
                );
            }
        }
    };
    let path = root.join(".thinkingroot").join("README.md");
    let body = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            // Permission / corruption errors are real failures — log
            // and surface as 500 rather than silently masquerading as
            // "no README" (CLAUDE.md §honesty rule §6).
            tracing::error!(
                target: "readme",
                error = %e,
                path = %path.display(),
                "workspace_readme: read_to_string failed"
            );
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "READ_FAILED",
                &format!("could not read README: {e}"),
            );
        }
    };
    ok_response(ReadmeResponse { readme: body }).into_response()
}

#[derive(Deserialize)]
struct ForgetSourceRequest {
    source_uri: String,
}

/// Stream A — `POST /api/v1/ws/{ws}/sources/forget`. Removes every
/// claim/edge/vector descended from `source_uri` and atomically rebuilds
/// the in-memory cache. Returns `{ "removed": usize }` (0 when the URI
/// did not match any source). Idempotent — second call is a no-op.
async fn forget_source_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<ForgetSourceRequest>,
) -> Response {
    if body.source_uri.trim().is_empty() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "source_uri is required",
        );
    }
    let engine = state.engine.read().await;
    match engine.forget_source(&ws, &body.source_uri).await {
        Ok(removed) => {
            ok_response(serde_json::json!({ "removed": removed })).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

/// One uploaded source file: a workspace-relative path + its UTF-8 text.
#[derive(Deserialize)]
struct IngestFile {
    /// Workspace-relative path, e.g. `src/index.ts`. MUST stay inside the
    /// workspace root — absolute paths and `..` components are rejected.
    path: String,
    /// File contents (UTF-8 text). Binary blobs have no place in a code
    /// compile; the pipeline's `is_probably_text` gate drops junk anyway.
    content: String,
}

#[derive(Deserialize)]
struct IngestSourcesRequest {
    files: Vec<IngestFile>,
    /// When true, wipe the workspace root's tracked source tree first so a
    /// re-ingest of a moved/renamed tree doesn't leave orphan files behind.
    /// `.thinkingroot/` (the data dir) is always preserved.
    #[serde(default)]
    clear: bool,
}

/// Hard caps — a code workspace, not a file host. A caller exceeding these
/// is rejected loudly rather than silently truncated (no-silent-failure).
const INGEST_MAX_FILES: usize = 20_000;
const INGEST_MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const INGEST_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;

/// Resolve a client-supplied relative path to an absolute path **guaranteed
/// to live under `root`**. Returns `None` for anything that would escape:
/// empty, absolute, a Windows drive/prefix, or any `..`/`.` component. This
/// is the path-traversal guard for the ingest endpoint — the only place the
/// engine writes attacker-influenced filenames to disk.
fn safe_join_under(root: &std::path::Path, rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    let rel_path = std::path::Path::new(rel);
    let mut out = root.to_path_buf();
    let mut pushed = false;
    for comp in rel_path.components() {
        match comp {
            Component::Normal(part) => {
                // Reject embedded NUL and any residual separators a single
                // component should never contain.
                let s = part.to_str()?;
                if s.is_empty() || s.contains('\0') {
                    return None;
                }
                out.push(part);
                pushed = true;
            }
            // Prefix (C:\), RootDir (/), ParentDir (..), CurDir (.) all
            // either escape the root or are absolute — reject the whole path.
            _ => return None,
        }
    }
    if !pushed {
        return None;
    }
    Some(out)
}

/// `POST /api/v1/ws/{ws}/sources/ingest` — receive a set of source files
/// from an external client (which has the project's working tree but the
/// engine does not — e.g. the MrGuy plugin compiling a developer's local
/// folder against the managed cloud) and write them under the workspace's
/// `root_path` so a subsequent `/compile` parses them. Pairs with
/// `/compile`: ingest puts files on the engine's disk, compile builds the
/// graph from them. Returns `{ written, skipped, root_path }`.
async fn ingest_sources_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<IngestSourcesRequest>,
) -> Response {
    if body.files.len() > INGEST_MAX_FILES {
        return err_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "TOO_MANY_FILES",
            &format!("at most {INGEST_MAX_FILES} files per ingest"),
        );
    }
    let mut total: usize = 0;
    for f in &body.files {
        total = total.saturating_add(f.content.len());
        if f.content.len() > INGEST_MAX_FILE_BYTES {
            return err_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "FILE_TOO_LARGE",
                &format!("'{}' exceeds {INGEST_MAX_FILE_BYTES} bytes", f.path),
            );
        }
    }
    if total > INGEST_MAX_TOTAL_BYTES {
        return err_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            &format!("total upload exceeds {INGEST_MAX_TOTAL_BYTES} bytes"),
        );
    }

    // Resolve the workspace root under a short read-lock, then release it —
    // the file I/O below holds no engine lock (compile is a separate call).
    let root_path = {
        let engine = state.engine.read().await;
        match engine.workspace_root_path(&ws) {
            Some(p) => p,
            None => {
                return err_response(
                    StatusCode::NOT_FOUND,
                    "WS_NOT_MOUNTED",
                    &format!("workspace '{ws}' not mounted"),
                );
            }
        }
    };

    // Optional clean slate: remove every entry under root EXCEPT the
    // `.thinkingroot/` data dir (graph.db lives there — wiping it would
    // destroy the workspace, not refresh its sources).
    if body.clear {
        match tokio::fs::read_dir(&root_path).await {
            Ok(mut rd) => {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    if entry.file_name() == std::ffi::OsStr::new(".thinkingroot") {
                        continue;
                    }
                    let p = entry.path();
                    let res = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        tokio::fs::remove_dir_all(&p).await
                    } else {
                        tokio::fs::remove_file(&p).await
                    };
                    if let Err(e) = res {
                        return err_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "CLEAR_FAILED",
                            &format!("could not clear {}: {e}", p.display()),
                        );
                    }
                }
            }
            Err(e) => {
                return err_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CLEAR_FAILED",
                    &format!("could not read workspace root: {e}"),
                );
            }
        }
    }

    let mut written = 0usize;
    let mut skipped = 0usize;
    for f in &body.files {
        let Some(dest) = safe_join_under(&root_path, &f.path) else {
            // A path that tries to escape the workspace is skipped, not
            // fatal — one bad name shouldn't abort a 10k-file upload.
            skipped += 1;
            tracing::warn!("ingest: rejected unsafe path '{}'", f.path);
            continue;
        };
        if let Some(parent) = dest.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return err_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "WRITE_FAILED",
                    &format!("could not create {}: {e}", parent.display()),
                );
            }
        }
        if let Err(e) = tokio::fs::write(&dest, f.content.as_bytes()).await {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "WRITE_FAILED",
                &format!("could not write {}: {e}", dest.display()),
            );
        }
        written += 1;
    }

    ok_response(serde_json::json!({
        "written": written,
        "skipped": skipped,
        "root_path": root_path.display().to_string(),
    }))
    .into_response()
}

/// Stream A — `GET /api/v1/ws/{ws}/claims/rooted`. Returns the rooted-
/// tier claims (Phase 6.5 admission gate passed) for the workspace.
/// Backs the Brain view's tier badging without forcing a second
/// round-trip through `list_claims` + a separate rooted-id lookup.
async fn list_rooted_claims_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.list_rooted_claims(&ws, None, None, None).await {
        Ok(claims) => ok_response(claims).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_entity(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_entity(&ws, &name).await {
        Ok(entity) => ok_response(entity).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn list_claims(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<ClaimQueryParams>,
) -> Response {
    let engine = state.engine.read().await;
    let filter = ClaimFilter {
        claim_type: params.claim_type,
        entity_name: params.entity,
        min_confidence: params.min_confidence,
        limit: params.limit,
        offset: params.offset,
    };
    match engine.list_claims(&ws, filter).await {
        Ok(claims) => ok_response(claims).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// Query parameters accepted by `GET /api/v1/ws/{ws}/witnesses`.
/// `limit` caps the row count; `rule` filters to a specific catalog
/// rule; `source_id` scopes to one source row's witnesses (used by
/// the Playground SourceLibrary click-through). All optional —
/// passing none lists every Witness in the workspace.
#[derive(serde::Deserialize)]
struct WitnessListParams {
    limit: Option<usize>,
    rule: Option<String>,
    source_id: Option<String>,
}

async fn list_witnesses_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<WitnessListParams>,
) -> Response {
    let engine = state.engine.read().await;
    let result = if let Some(sid) = params.source_id.as_deref() {
        engine.list_witnesses_by_source(&ws, sid).await
    } else {
        engine.list_witnesses(&ws, params.limit).await
    };
    match result {
        Ok(mut witnesses) => {
            if let Some(rule_filter) = &params.rule {
                witnesses.retain(|w| &w.rule == rule_filter);
            }
            // `source_id` already scoped server-side; apply `limit`
            // post-hoc when both source_id + limit are supplied so
            // the caller still gets predictable truncation.
            if params.source_id.is_some() {
                if let Some(limit) = params.limit {
                    witnesses.truncate(limit);
                }
            }
            ok_response(witnesses).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

// ── E2: code-graph traversal API ─────────────────────────────────

#[derive(Deserialize)]
struct CodeSearchEntityParams {
    keyword: String,
}

async fn code_search_entity_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<CodeSearchEntityParams>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.search_entity(&ws, &params.keyword).await {
        Ok(entities) => ok_response(serde_json::json!({ "entities": entities })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn code_retrieve_entity_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, id)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.retrieve_entity(&ws, &id).await {
        // 404=empty, honesty rule: unknown id returns {"entity": null}, not 500.
        Ok(Some(detail)) => ok_response(detail).into_response(),
        Ok(None) => ok_response(serde_json::json!({ "entity": null })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

fn default_traverse_direction() -> thinkingroot_graph::codegraph::TraversalDirection {
    thinkingroot_graph::codegraph::TraversalDirection::Out
}
fn default_traverse_hops() -> u32 {
    3
}
fn default_traverse_edge_kinds() -> Vec<thinkingroot_graph::codegraph::EdgeKind> {
    vec![thinkingroot_graph::codegraph::EdgeKind::Calls]
}

#[derive(Deserialize)]
struct CodeTraverseBody {
    /// Start by symbol name (resolved to its best-matching claim id) …
    #[serde(default)]
    symbol: Option<String>,
    /// … or directly by claim id (takes precedence over `symbol`).
    #[serde(default)]
    claim_id: Option<String>,
    #[serde(default = "default_traverse_direction")]
    direction: thinkingroot_graph::codegraph::TraversalDirection,
    #[serde(default = "default_traverse_hops")]
    hops: u32,
    #[serde(default = "default_traverse_edge_kinds")]
    edge_kinds: Vec<thinkingroot_graph::codegraph::EdgeKind>,
}

async fn code_traverse_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<CodeTraverseBody>,
) -> Response {
    let engine = state.engine.read().await;
    // Resolve the start node: explicit claim_id wins; otherwise resolve the
    // symbol to its best match. Unknown symbol → empty result (honesty).
    let start_id = match body.claim_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            let Some(symbol) = body.symbol.as_ref().filter(|s| !s.is_empty()) else {
                return ok_response(serde_json::json!({ "nodes": [], "start": null })).into_response();
            };
            match engine.search_entity(&ws, symbol).await {
                Ok(hits) => match hits.into_iter().next() {
                    Some(h) => h.claim_id,
                    None => {
                        return ok_response(serde_json::json!({ "nodes": [], "start": null }))
                            .into_response();
                    }
                },
                Err(e) => return match_engine_error(e),
            }
        }
    };
    let hops = body.hops.min(16); // bound the walk
    match engine
        .traverse_graph(&ws, &start_id, body.direction, hops, &body.edge_kinds)
        .await
    {
        Ok(nodes) => {
            ok_response(serde_json::json!({ "nodes": nodes, "start": start_id })).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

fn default_repo_budget() -> usize {
    1024
}

#[derive(Deserialize)]
struct RepoMapQuery {
    #[serde(default = "default_repo_budget")]
    budget: usize,
}

#[derive(Deserialize, Default)]
struct RepoMapBody {
    /// Overrides `?budget=` when present.
    #[serde(default)]
    budget_tokens: Option<usize>,
    /// Optional query — biases PageRank personalization toward matches.
    #[serde(default)]
    query: Option<String>,
}

async fn repo_map_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(q): Query<RepoMapQuery>,
    Json(body): Json<RepoMapBody>,
) -> Response {
    let engine = state.engine.read().await;
    let req = crate::intelligence::repo_map::RepoMapRequest {
        budget_tokens: body.budget_tokens.unwrap_or(q.budget),
        query: body.query,
    };
    match engine.repo_map(&ws, &req).await {
        Ok(map) => ok_response(map).into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize, Default)]
struct SummariesParams {
    /// Optional altitude filter: function | file | repo.
    #[serde(default)]
    altitude: Option<String>,
}

async fn summaries_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<SummariesParams>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_summaries(&ws, params.altitude.as_deref()).await {
        Ok(nodes) => ok_response(serde_json::json!({ "summaries": nodes })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn summaries_build_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.build_summaries(&ws).await {
        Ok(count) => ok_response(serde_json::json!({ "summary_nodes": count })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_witness_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, id)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_witness(&ws, &id).await {
        Ok(Some(w)) => ok_response(w).into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            ok_response(serde_json::json!({
                "error": "witness not found",
                "witness_id": id,
            })),
        )
            .into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn witnesses_count_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.count_witnesses(&ws).await {
        Ok(count) => ok_response(serde_json::json!({ "count": count })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

// ── workspace filesystem ops ─────────────────────────────────────

#[derive(Deserialize, Default)]
struct FsListParams {
    /// Optional sub-folder rel-path. Omit / empty = workspace root.
    rel: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FsReadParams {
    /// Workspace-relative path of the file to read.
    rel: String,
}

#[derive(Deserialize)]
struct FsCreateFolderBody {
    parent_rel: String,
    name: String,
}

#[derive(Deserialize)]
struct FsRenameBody {
    rel: String,
    new_name: String,
}

#[derive(Deserialize)]
struct FsMoveBody {
    sources: Vec<String>,
    dest_folder: String,
}

fn resolve_workspace_root_for_fs(
    engine: &QueryEngine,
    ws: &str,
) -> Result<PathBuf, Response> {
    engine.workspace_root_path(ws).ok_or_else(|| {
        err_response(
            StatusCode::NOT_FOUND,
            "workspace_not_mounted",
            &format!("workspace `{ws}` is not mounted"),
        )
    })
}

async fn fs_list_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<FsListParams>,
) -> Response {
    let engine = state.engine.read().await;
    let root = match resolve_workspace_root_for_fs(&engine, &ws) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let rel = params.rel.unwrap_or_default();
    match crate::fs_ops::list_directory(&root, &ws, &rel) {
        Ok(listing) => ok_response(listing).into_response(),
        Err(msg) => err_response(StatusCode::BAD_REQUEST, "fs_list_failed", &msg),
    }
}

async fn fs_read_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<FsReadParams>,
) -> Response {
    let engine = state.engine.read().await;
    let root = match resolve_workspace_root_for_fs(&engine, &ws) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match crate::fs_ops::read_file(&root, &params.rel) {
        Ok(content) => {
            ok_response(serde_json::json!({ "rel_path": params.rel, "content": content }))
                .into_response()
        }
        Err(msg) => err_response(StatusCode::BAD_REQUEST, "fs_read_failed", &msg),
    }
}

async fn fs_create_folder_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<FsCreateFolderBody>,
) -> Response {
    let engine = state.engine.read().await;
    let root = match resolve_workspace_root_for_fs(&engine, &ws) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match crate::fs_ops::create_folder(&root, &body.parent_rel, &body.name) {
        Ok(rel) => ok_response(serde_json::json!({ "rel_path": rel })).into_response(),
        Err(msg) => err_response(StatusCode::BAD_REQUEST, "fs_create_folder_failed", &msg),
    }
}

async fn fs_rename_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<FsRenameBody>,
) -> Response {
    let engine = state.engine.read().await;
    let root = match resolve_workspace_root_for_fs(&engine, &ws) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match crate::fs_ops::rename_path(&root, &body.rel, &body.new_name) {
        Ok(rel) => ok_response(serde_json::json!({ "rel_path": rel })).into_response(),
        Err(msg) => err_response(StatusCode::BAD_REQUEST, "fs_rename_failed", &msg),
    }
}

async fn fs_move_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<FsMoveBody>,
) -> Response {
    let engine = state.engine.read().await;
    let root = match resolve_workspace_root_for_fs(&engine, &ws) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match crate::fs_ops::move_paths(&root, body.sources, &body.dest_folder) {
        Ok(outcome) => ok_response(outcome).into_response(),
        Err(msg) => err_response(StatusCode::BAD_REQUEST, "fs_move_failed", &msg),
    }
}

/// `GET /api/v1/ws/{ws}/witnesses/by-source` — witness count per
/// source row. Used by the Playground SourceLibrary to badge each
/// source with its witness count. Returns
/// `[{ "source_id": "...", "count": N }, ...]` so JS consumers
/// don't have to handle Vec<(String, u64)>'s tuple-encoding
/// surprises across runtimes.
async fn witnesses_by_source_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.count_witnesses_by_source(&ws).await {
        Ok(rows) => {
            let body: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(source_id, count)| {
                    serde_json::json!({ "source_id": source_id, "count": count })
                })
                .collect();
            ok_response(body).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/paper/regenerate` — rerun the Living Paper
/// synthesiser against the workspace's current Witness Mesh state
/// without driving a full compile. Returns the rendered paper bytes
/// (the same content written to `<root>/.thinkingroot/paper.md`).
async fn paper_regenerate_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.regenerate_paper(&ws).await {
        Ok(output) => ok_response(serde_json::json!({
            "byte_length": output.byte_length,
            "sections": output.frontmatter.sections.len(),
            "markdown": output.markdown,
        }))
        .into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// Query params for `GET /api/v1/ws/{ws}/commits`.
#[derive(serde::Deserialize)]
struct CommitListParams {
    /// Branch to list. Defaults to `main` server-side when omitted so
    /// the typical chat-UI path (one commit DAG per branch, branch
    /// often unspecified at first paint) just works.
    branch: Option<String>,
    /// Max commits to return. Omit for everything.
    limit: Option<usize>,
}

/// Request body for `POST /api/v1/ws/{ws}/commits`. Mirrors the MCP
/// `commit_cognition` tool's argument shape so external agents can
/// drive the REST endpoint with identical JSON.
#[derive(serde::Deserialize)]
struct RecordCommitRequest {
    branch: String,
    parent_id: Option<String>,
    author_kind: String,
    author_id: String,
    #[serde(default)]
    author_model: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    witnesses_added: Vec<String>,
    #[serde(default)]
    citations: Vec<String>,
    #[serde(default)]
    gaps_surfaced: Vec<String>,
}

/// `GET /api/v1/ws/{ws}/commits?branch=&limit=` — list commits on a
/// branch newest-first. Returns the full `CognitionCommit` shape so
/// the chat-UI can render the citation chips inline without follow-up
/// fetches.
async fn list_cognition_commits_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<CommitListParams>,
) -> Response {
    let engine = state.engine.read().await;
    let branch = params.branch.as_deref().unwrap_or("main");
    match engine.list_cognition_commits(&ws, branch, params.limit).await {
        Ok(commits) => ok_response(commits).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// Query params for `GET /api/v1/ws/{ws}/commits/merge-plan`. Both
/// branch names are required — there's no sensible default-side for
/// merging.
#[derive(serde::Deserialize)]
struct MergePlanParams {
    left: String,
    right: String,
}

/// Phase γ.1 — `GET /api/v1/ws/{ws}/commits/merge-plan?left=&right=`.
///
/// Compute a deterministic merge plan between two cognition-commit
/// branches. Pure read — no commit recorded. Returns the full
/// `MergePlan` JSON; the React conflict-resolution view (γ.3, not
/// yet shipped) will be the primary consumer. Today the response
/// also drives the in-app `merge_cognition` tool's outcome.
async fn merge_plan_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<MergePlanParams>,
) -> Response {
    if params.left.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            ok_response(serde_json::json!({
                "error": "merge-plan: `left` query param is required",
            })),
        )
            .into_response();
    }
    if params.right.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            ok_response(serde_json::json!({
                "error": "merge-plan: `right` query param is required",
            })),
        )
            .into_response();
    }
    let engine = state.engine.read().await;
    match engine
        .compute_merge_plan(&ws, &params.left, &params.right)
        .await
    {
        Ok(plan) => ok_response(plan).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// Body for `POST /api/v1/ws/{ws}/commits/synthesize-merge`. Mirrors
/// the MCP `synthesize_merge` tool's argument shape.
#[derive(serde::Deserialize)]
struct SynthesizeMergeRequest {
    left_branch: String,
    right_branch: String,
}

/// Phase γ.2 — `POST /api/v1/ws/{ws}/commits/synthesize-merge`.
async fn synthesize_merge_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    axum::Json(body): axum::Json<SynthesizeMergeRequest>,
) -> Response {
    if body.left_branch.is_empty() || body.right_branch.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            ok_response(serde_json::json!({
                "error": "synthesize_merge: `left_branch` and `right_branch` both required",
            })),
        )
            .into_response();
    }
    let engine = state.engine.read().await;
    match engine
        .synthesize_merge(&ws, &body.left_branch, &body.right_branch)
        .await
    {
        Ok(synthesis) => ok_response(synthesis).into_response(),
        Err(e) => match_engine_error(e),
    }
}

// ─── Phase δ.2 — Substrate Bus REST handlers ─────────────────────────

/// `POST /api/v1/ws/{ws}/substrate-bus/start` — idempotent: starts
/// the bus for `ws` if not already running, returns the registered
/// agent names.
async fn substrate_bus_start_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let scheduler = state.ensure_substrate_bus(&ws).await;
    let names: Vec<String> = scheduler
        .agent_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    ok_response(serde_json::json!({
        "workspace": ws,
        "running": true,
        "agents": names,
    }))
    .into_response()
}

/// `POST /api/v1/ws/{ws}/substrate-bus/stop` — idempotent shutdown.
async fn substrate_bus_stop_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    state.stop_substrate_bus(&ws).await;
    ok_response(serde_json::json!({ "workspace": ws, "running": false }))
        .into_response()
}

/// `GET /api/v1/ws/{ws}/substrate-bus/reports` — snapshot of the
/// per-agent report ring. Empty when the bus isn't running.
async fn substrate_bus_reports_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let reports = state.substrate_bus_reports(&ws).await;
    ok_response(reports).into_response()
}

/// Body for `POST /api/v1/ws/{ws}/substrate-bus/run-now`.
#[derive(serde::Deserialize)]
struct SubstrateBusRunNowRequest {
    agent: String,
}

/// `POST /api/v1/ws/{ws}/substrate-bus/run-now` — manually trigger
/// one tick of an agent (without waiting for its interval). Useful
/// for the desktop "Run now" affordance.
async fn substrate_bus_run_now_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    axum::Json(body): axum::Json<SubstrateBusRunNowRequest>,
) -> Response {
    let scheduler = state.ensure_substrate_bus(&ws).await;
    let ctx = crate::intelligence::substrate_bus::SubAgentContext {
        engine: Arc::clone(&state.engine),
        workspace: ws.clone(),
    };
    match scheduler.run_now(&body.agent, &ctx).await {
        Some(report) => ok_response(report).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            ok_response(serde_json::json!({
                "error": format!("unknown agent `{}`", body.agent),
            })),
        )
            .into_response(),
    }
}

/// `GET /api/v1/ws/{ws}/commits/{id}` — fetch a single commit by id.
/// Returns 404 when the id is unknown so the chat-UI can render a
/// "this commit was pruned" empty state honestly.
async fn get_cognition_commit_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, id)): Path<(String, String)>,
) -> Response {
    let parsed = match thinkingroot_core::types::CommitId::from_hex(&id) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                ok_response(serde_json::json!({
                    "error": format!("invalid commit id `{id}`: {e}"),
                })),
            )
                .into_response();
        }
    };
    let engine = state.engine.read().await;
    match engine.get_cognition_commit(&ws, &parsed).await {
        Ok(Some(c)) => ok_response(c).into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            ok_response(serde_json::json!({
                "error": "commit not found",
                "commit_id": id,
            })),
        )
            .into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/commits` — record one cognition commit.
/// Citations + parent are verified by `QueryEngine::commit_cognition`
/// before the row lands; a 400-shaped error surfaces fabricated
/// citations + dangling parents to the caller.
async fn record_cognition_commit_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<RecordCommitRequest>,
) -> Response {
    use thinkingroot_core::types::{CognitionCommit, CommitAuthor, CommitId};

    if body.branch.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            ok_response(serde_json::json!({
                "error": "branch must not be empty",
            })),
        )
            .into_response();
    }

    let parent = match body.parent_id.as_deref() {
        Some(s) if !s.is_empty() => match CommitId::from_hex(s) {
            Ok(p) => Some(p),
            Err(e) => {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    ok_response(serde_json::json!({
                        "error": format!("invalid parent_id `{s}`: {e}"),
                    })),
                )
                    .into_response();
            }
        },
        _ => None,
    };

    let author = match body.author_kind.as_str() {
        "user" => CommitAuthor::User { id: body.author_id },
        "agent" => CommitAuthor::Agent {
            model: body.author_model,
            principal: body.author_id,
        },
        other => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                ok_response(serde_json::json!({
                    "error": format!("author_kind must be 'user' or 'agent', got `{other}`"),
                })),
            )
                .into_response();
        }
    };

    let witnesses_added = match collect_witness_ids(&body.witnesses_added) {
        Ok(v) => v,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                ok_response(serde_json::json!({
                    "error": format!("witnesses_added: {e}"),
                })),
            )
                .into_response();
        }
    };
    let citations = match collect_witness_ids(&body.citations) {
        Ok(v) => v,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                ok_response(serde_json::json!({
                    "error": format!("citations: {e}"),
                })),
            )
                .into_response();
        }
    };

    let commit = CognitionCommit::new(
        parent,
        body.branch,
        author,
        body.prompt,
        body.reasoning,
        witnesses_added,
        citations,
        body.gaps_surfaced,
        chrono::Utc::now(),
    );

    let engine = state.engine.read().await;
    match engine.commit_cognition(&ws, &commit).await {
        Ok(()) => ok_response(commit).into_response(),
        Err(e) => match_engine_error(e),
    }
}

fn collect_witness_ids(
    hex_ids: &[String],
) -> std::result::Result<Vec<thinkingroot_core::types::WitnessId>, String> {
    use thinkingroot_core::types::WitnessId;
    let mut out: Vec<WitnessId> = Vec::with_capacity(hex_ids.len());
    for s in hex_ids {
        let id = WitnessId::from_hex(s)
            .map_err(|e| format!("invalid witness id `{s}`: {e}"))?;
        out.push(id);
    }
    Ok(out)
}

/// Query parameters for `GET /api/v1/ws/{ws}/gaps`. All optional —
/// callers can list every gap by omitting them. Mirrors the MCP `gaps`
/// tool's argument shape.
#[derive(serde::Deserialize)]
struct GapsParams {
    entity: Option<String>,
    min_confidence: Option<f64>,
    branch: Option<String>,
}

/// `GET /api/v1/ws/{ws}/gaps` — list Phase 9 known-unknowns inferred
/// from structural co-occurrence patterns. Same shape the MCP `gaps`
/// tool returns; the Playground "Find gaps" panel renders these
/// inline.
async fn gaps_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<GapsParams>,
) -> Response {
    let engine = state.engine.read().await;
    let min_conf = params.min_confidence.unwrap_or(0.5);
    let entity = params.entity.as_deref();
    let branch = params.branch.as_deref();
    match engine
        .list_gaps_branched(&ws, entity, min_conf, branch)
        .await
    {
        Ok(rows) => ok_response(rows).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// Query parameters for `GET /api/v1/ws/{ws}/witnesses/{id}/walk`.
/// Both are optional; the handler applies the same clamps as the
/// MCP `walk_mesh` tool (depth ≤ 10, fanout 1..=200).
#[derive(serde::Deserialize)]
struct WalkMeshParams {
    max_depth: Option<usize>,
    max_fanout: Option<usize>,
}

async fn walk_mesh_handler(
    State(state): State<Arc<AppState>>,
    Path((ws, id)): Path<(String, String)>,
    Query(params): Query<WalkMeshParams>,
) -> Response {
    let engine = state.engine.read().await;
    let raw_depth = params.max_depth.unwrap_or(4);
    let max_depth = raw_depth.min(10);
    let raw_fanout = params.max_fanout.unwrap_or(50);
    let max_fanout = raw_fanout.clamp(1, 200);
    match engine.walk_witness_mesh(&ws, &id, max_depth, max_fanout).await {
        Ok((witnesses, edges)) => ok_response(serde_json::json!({
            "witnesses": witnesses,
            "edges": edges.iter().map(|(p, c)| {
                serde_json::json!({ "parent": p, "child": c })
            }).collect::<Vec<_>>(),
            "max_depth": max_depth,
            "max_fanout": max_fanout,
            "depth_clamped": raw_depth > max_depth,
            "fanout_clamped": raw_fanout != max_fanout,
        }))
        .into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_galaxy(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.get_galaxy_map(&ws).await {
        Ok(map) => ok_response(map).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_all_relations(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.get_all_relations(&ws).await {
        Ok(rels) => {
            let data: Vec<serde_json::Value> = rels
                .into_iter()
                .map(|(from, to, rtype, strength)| {
                    serde_json::json!({
                        "from": from,
                        "to": to,
                        // Normalize the legacy TitleCase storage form
                        // (`format!("{:?}")` at graph.rs) to the wire
                        // snake_case `RelationType` declares via serde.
                        "relation_type":
                            thinkingroot_core::types::RelationType::normalize_storage(&rtype),
                        "strength": strength,
                    })
                })
                .collect();
            ok_response(data).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

async fn get_entity_relations(
    State(state): State<Arc<AppState>>,
    Path((ws, entity)): Path<(String, String)>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_relations(&ws, &entity).await {
        Ok(rels) => ok_response(rels).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn list_artifacts(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.list_artifacts(&ws).await {
        Ok(artifacts) => ok_response(artifacts).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path((ws, artifact_type)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let engine = state.engine.read().await;
    match engine.get_artifact(&ws, &artifact_type).await {
        Ok(artifact) => {
            let wants_markdown = headers
                .get("accept")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.contains("text/markdown"))
                .unwrap_or(false);

            if wants_markdown {
                (
                    StatusCode::OK,
                    [("content-type", "text/markdown")],
                    artifact.content,
                )
                    .into_response()
            } else {
                ok_response(artifact).into_response()
            }
        }
        Err(e) => match_engine_error(e),
    }
}

async fn get_health(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.health(&ws).await {
        Ok(result) => ok_response(result).into_response(),
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct RouteRequest {
    #[serde(default)]
    query: String,
    #[serde(default = "default_route_k")]
    top_k: usize,
    #[serde(default)]
    branch: Option<String>,
}
fn default_route_k() -> usize {
    10
}

/// `POST /api/v1/ws/{ws}/route` — capability router. Ranks deployed Root
/// Functions + external MCP tools for an INTENT by fusing semantic similarity
/// (embedded capability nodes) with the learned Wilson experience score.
/// Powers the meta-tool discovery surface, the Console routing view, and
/// `route-eval` (recall@k). Returns `{query, ranked: [tool_name, …]}`.
async fn route_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(req): Json<RouteRequest>,
) -> Response {
    let engine = state.engine.read().await;
    match engine
        .route_capabilities(&ws, req.branch.as_deref(), &req.query, req.top_k)
        .await
    {
        Ok(ranked) => {
            ok_response(serde_json::json!({ "query": req.query, "ranked": ranked })).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

#[derive(Deserialize)]
struct RouteToolsRequest {
    #[serde(default)]
    query: String,
    #[serde(default)]
    tools: Vec<RouteToolDesc>,
    #[serde(default = "default_route_k")]
    top_k: usize,
}
#[derive(Deserialize)]
struct RouteToolDesc {
    name: String,
    #[serde(default)]
    description: String,
}

/// `POST /api/v1/ws/{ws}/route-tools` — A2 tool-list gating primitive. Ranks an
/// ARBITRARY caller-supplied tool catalog (`tools: [{name, description}]`) by
/// semantic relevance to `query`, returning the top-k tool names. The host (e.g.
/// MrGuy) uses this to shrink the model's VISIBLE tool list per turn — the token
/// win. Fail-open in the engine (returns the first k if the embedder is down).
async fn route_tools_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(req): Json<RouteToolsRequest>,
) -> Response {
    let tools: Vec<(String, String)> =
        req.tools.into_iter().map(|t| (t.name, t.description)).collect();
    let engine = state.engine.read().await;
    match engine.rank_tool_catalog(&ws, &req.query, tools, req.top_k).await {
        Ok(ranked) => ok_response(serde_json::json!({ "ranked": ranked })).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/sleep` — B5 consolidation ("the being rests, wakes wiser"):
/// resolve unresolved contradictions by superseding the older/less-confident claim,
/// so recall returns the surviving truth. Returns `{ contradictions_resolved,
/// claims_superseded }`. Idempotent; safe to schedule nightly.
#[derive(Deserialize, Default)]
struct SleepRequest {
    /// Expire active claims created before this epoch-seconds whose confidence is
    /// below `confidence_floor` (old low-value noise). Omit to skip stale-expiry.
    #[serde(default)]
    stale_before_epoch: Option<f64>,
    /// Confidence floor for stale-expiry (default 0.5).
    #[serde(default)]
    confidence_floor: Option<f64>,
}

async fn sleep_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    body: Option<Json<SleepRequest>>,
) -> Response {
    let req = body.map(|b| b.0).unwrap_or_default();
    let conf_floor = req.confidence_floor.unwrap_or(0.5);
    let engine = state.engine.read().await;
    match engine
        .sleep_consolidate(&ws, req.stale_before_epoch, conf_floor)
        .await
    {
        Ok(report) => ok_response(serde_json::json!(report)).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/dream` — §11 #26 Night Shift dreaming: synthesize
/// higher-level insights/playbooks from existing claims via the workspace LLM,
/// quarantined to a dream branch, verify-before-merge. `auto_merge` merges kept
/// insights into main; otherwise they stay on the branch for review.
#[derive(Deserialize, Default)]
struct DreamRequest {
    #[serde(default = "default_dream_max_claims")]
    max_claims: usize,
    #[serde(default = "default_dream_max_insights")]
    max_insights: usize,
    #[serde(default)]
    auto_merge: bool,
}
fn default_dream_max_claims() -> usize {
    50
}
fn default_dream_max_insights() -> usize {
    5
}

async fn dream_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    body: Option<Json<DreamRequest>>,
) -> Response {
    let req = body.map(|b| b.0).unwrap_or_default();
    let engine = state.engine.read().await;
    match engine
        .dream(&ws, req.max_claims, req.max_insights, req.auto_merge, &state.sessions)
        .await
    {
        Ok(report) => ok_response(serde_json::json!(report)).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/predict` — §1 the `predict` verb: "what happens next",
/// grounded ONLY in recalled claims via the workspace LLM, falsifier-gated
/// (verified-or-silent; refuses when there's no basis or no grounded citation).
#[derive(Deserialize)]
struct PredictRequest {
    question: String,
    #[serde(default = "default_predict_top_k")]
    top_k: usize,
}
fn default_predict_top_k() -> usize {
    12
}

async fn predict_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<PredictRequest>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.predict(&ws, &body.question, body.top_k).await {
        Ok(report) => ok_response(serde_json::json!(report)).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `GET /api/v1/ws/{ws}/age` — P2 honest developmental age: verified capability
/// mass + knowledge + reconciliations → a coarse life stage.
async fn age_handler(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.developmental_age(&ws).await {
        Ok(report) => ok_response(serde_json::json!(report)).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `GET /api/v1/ws/{ws}/drives` — P3 behavioral drives: curiosity/exploration vs
/// frontier-focus, derived from measured maturity.
async fn drives_handler(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.drives(&ws).await {
        Ok(report) => ok_response(serde_json::json!(report)).into_response(),
        Err(e) => match_engine_error(e),
    }
}

fn default_legacy_min_conf() -> f64 {
    0.7
}
fn default_true_legacy() -> bool {
    true
}
#[derive(Deserialize)]
struct BequeathRequest {
    #[serde(default = "default_legacy_min_conf")]
    min_confidence: f64,
    #[serde(default = "default_true_legacy")]
    only_verified: bool,
}

/// `POST /api/v1/ws/{ws}/bequeath` — P4: produce the being's verified inheritance
/// (genome + high-confidence knowledge).
async fn bequeath_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    body: Option<Json<BequeathRequest>>,
) -> Response {
    let req = body.map(|b| b.0).unwrap_or(BequeathRequest {
        min_confidence: 0.7,
        only_verified: true,
    });
    let engine = state.engine.read().await;
    match engine
        .bequeath(&ws, req.min_confidence, req.only_verified)
        .await
    {
        Ok(bundle) => ok_response(serde_json::json!(bundle)).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/inherit` — P4: inherit a legacy bundle into this successor.
async fn inherit_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(bundle): Json<crate::engine::LegacyBundle>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.inherit(&ws, bundle).await {
        Ok(report) => ok_response(serde_json::json!(report)).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `GET /api/v1/ws/{ws}/capability-routing` — the routing/experience report:
/// every deployed function + its learned experience (n_success/n_fail + Wilson
/// score) grouped by input_class. Powers the Console routing view (P5).
async fn capability_routing_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.capability_routing_report(&ws).await {
        Ok(report) => ok_response(report).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn search(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(params): Query<SearchQueryParams>,
) -> Response {
    let engine = state.engine.read().await;
    let top_k = params.top_k.unwrap_or(10);
    match engine.search(&ws, &params.q, top_k).await {
        Ok(results) => ok_response(results).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/search/hybrid` — Hybrid Retrieval (vector × Datalog
/// × BLAKE3 × 11-component score fusion). Single-shot JSON response, not
/// SSE; the <25ms p95 budget makes streaming overhead net-negative. Cancel
/// on client disconnect via the same `CancellationToken + DropGuard`
/// pattern as the SSE compile route.
async fn hybrid_search_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(req): Json<crate::engine::RetrievalRequest>,
) -> Response {
    if let Err(resp) = ensure_user_ws(&state, &ws).await {
        return resp;
    }
    let cancel = tokio_util::sync::CancellationToken::new();
    let _drop_guard = cancel.clone().drop_guard();
    let engine = state.engine.read().await;
    match engine.hybrid_retrieve(&ws, req, Some(cancel)).await {
        Ok(resp) => ok_response(resp).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// T2.4 — `GET /api/v1/ws/{ws}/claims/as-of?as_of=2026-04-15T00:00:00Z[&branch=feat/x]`.
/// Returns claims whose `created_at` ≤ the supplied timestamp.
#[derive(Deserialize)]
struct AsOfQuery {
    as_of: String,
    #[serde(default)]
    branch: Option<String>,
}

async fn claims_as_of_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Query(query): Query<AsOfQuery>,
) -> Response {
    let parsed: chrono::DateTime<chrono::Utc> = match query.as_of.parse() {
        Ok(ts) => ts,
        Err(e) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "INVALID_AS_OF",
                &format!(
                    "as_of parameter must be ISO-8601 (e.g. 2026-04-15T00:00:00Z): {e}"
                ),
            );
        }
    };
    let engine = state.engine.read().await;
    match engine
        .list_claims_as_of_branched(&ws, query.branch.as_deref(), parsed)
        .await
    {
        Ok(claims) => ok_response(serde_json::json!({
            "workspace": ws,
            "branch": query.branch.unwrap_or_else(|| "main".to_string()),
            "as_of": query.as_of,
            "claim_count": claims.len(),
            "claims": claims,
        }))
        .into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/brain/brief` — workspace-level orientation.
/// Stateless equivalent of the MCP `brief` tool: returns the raw
/// `WorkspaceSummary` (counts + top entities + recent decisions +
/// contradiction count) so a CLI / Tauri caller can format it locally.
/// The MCP path additionally resets `SessionContext.token_budget` —
/// that is meaningless without an LLM session, so we omit it here.
#[derive(Debug, Default, Deserialize)]
struct BrainBriefRequest {
    #[serde(default)]
    branch: Option<String>,
}

async fn brain_brief_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    body: Option<Json<BrainBriefRequest>>,
) -> Response {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let engine = state.engine.read().await;
    match engine
        .get_workspace_brief_branched(&ws, req.branch.as_deref())
        .await
    {
        Ok(summary) => ok_response(summary).into_response(),
        Err(e) => match_engine_error(e),
    }
}

/// `POST /api/v1/ws/{ws}/brain/investigate` — full graph context for
/// one entity, optionally scoped to a branch. Returns the raw
/// `EntityContext` (relations, claims, contradictions). The MCP
/// counterpart additionally compresses against a session's
/// already-delivered claim budget; that compression is intentionally
/// not exposed over REST — the caller has the structured data to
/// project however it wants.
#[derive(Debug, Deserialize)]
struct BrainInvestigateRequest {
    entity: String,
    #[serde(default)]
    branch: Option<String>,
}

async fn brain_investigate_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(req): Json<BrainInvestigateRequest>,
) -> Response {
    let entity = req.entity.trim();
    if entity.is_empty() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "entity is required",
        );
    }
    let engine = state.engine.read().await;
    match engine
        .get_entity_context_branched(&ws, entity, req.branch.as_deref())
        .await
    {
        Ok(Some(ctx)) => ok_response(ctx).into_response(),
        Ok(None) => err_response(
            StatusCode::NOT_FOUND,
            "ENTITY_NOT_FOUND",
            &format!("entity '{entity}' not found in workspace '{ws}'"),
        ),
        Err(e) => match_engine_error(e),
    }
}

/// T3.2 — `POST /api/v1/ws/{ws}/reflect/across-branches`.  Body:
/// `{ "branches": ["main", "feature/foo", ...] }`.  Runs reflect
/// against each named branch and returns the union of per-branch
/// results plus divergent patterns.
#[derive(Deserialize)]
struct ReflectAcrossBranchesRequest {
    branches: Vec<String>,
}

async fn reflect_across_branches_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(req): Json<ReflectAcrossBranchesRequest>,
) -> Response {
    let engine = state.engine.read().await;
    match engine.reflect_across_branches(&ws, &req.branches).await {
        Ok(result) => ok_response(result).into_response(),
        Err(e) => match_engine_error(e),
    }
}

async fn compile(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    // The audit flagged that this read guard is held for the entire
    // compile (multi-minute).  Concurrent *readers* (search,
    // brain_load, etc.) are unaffected — `RwLock::read` is shared.
    // The only callers blocked are *writers* of the engine itself,
    // which are `mount`/`unmount` on the workspace map.  Those run
    // exactly once per workspace add/remove and are never on a UI hot
    // path, so the practical contention surface is empty.
    //
    // Releasing the guard mid-compile would require changing
    // `QueryEngine::compile`'s signature to `compile(Arc<Self>)`
    // because the returned Future captures `&self` from the guard;
    // that's a public API break we're not pulling forward without
    // observed contention.
    let engine = state.engine.read().await;
    match engine.compile(&ws).await {
        Ok(result) => {
            // Plan §3.10: when compile dirties the cache, drop every Engram
            // pointing at this workspace. Without this hook a probe after a
            // writing compile can return rows whose claim ids were just GC'd.
            // Mirrors the existing `branch_engines.invalidate_workspace`
            // call inside `QueryEngine::compile` (engine.rs:2987).
            if result.cache_dirty {
                state.engram_manager.invalidate_workspace(&ws).await;
            }
            ok_response(result).into_response()
        }
        Err(e) => match_engine_error(e),
    }
}

// ─── Streaming compile (P4 / H5) ─────────────────────────────
//
// `POST /api/v1/ws/{ws}/compile/stream` runs the v3 pipeline in this
// process and streams every `ProgressEvent` to the client as an SSE
// frame, plus a single `done`/`failed`/`cancelled` terminator.  Used
// by the desktop to route compile through the managed sidecar so the
// desktop process is never the writer of `graph.db` — pre-fix the
// in-process compile froze the desktop's Brain view because both the
// pipeline (writer) and `MountedMemory` (reader) shared the desktop's
// CozoDB instance.
//
// The handler doesn't go through `QueryEngine::compile` — that path
// requires the workspace to be mounted in this server's engine and
// does its own cache reload.  The desktop's sidecar is launched
// without `--path` args (workspaces are managed via the registry,
// not CLI flags), so we run `run_pipeline_with_options` directly
// against the explicit `root_path` from the request body.  This
// keeps the contract simple: the client tells the server what to
// compile; the server doesn't need its own mount table.
//
// Cancellation is wired via a `CancellationToken` whose
// `DropGuard` lives inside the SSE stream.  When the client
// disconnects, axum drops the response body future, the guard
// drops, the token trips, and the running pipeline exits at the
// next phase boundary with `Error::Cancelled` (which we surface
// as a `cancelled` SSE event for callers that race the disconnect).
#[derive(Debug, Deserialize)]
struct CompileStreamRequest {
    /// Absolute path to the workspace root.  Required when this
    /// server was started without `--path`; defaults to
    /// `state.workspace_root` otherwise.
    root_path: Option<String>,
    /// Optional branch — `None` resolves to the active head.
    branch: Option<String>,
    /// Skip Phase 6.5 (Rooting admission gate).  Mirrors
    /// `PipelineOptions::no_rooting` and the CLI's `--no-rooting`
    /// flag.
    #[serde(default)]
    no_rooting: bool,
}

/// Request shape for [`run_unified_compile`]. Cleanly separates the
/// SSE wire body from the helper's internal contract — callers that
/// don't speak SSE (the MCP `compile` tool) construct the same shape
/// without depending on the axum body extractor.
#[derive(Debug, Clone)]
pub(crate) struct UnifiedCompileRequest {
    /// The `{ws}` URL path component, or `"_"` when the caller is the
    /// CLI placeholder. The helper resolves it to the canonical
    /// workspace name via `engine.list_workspaces()` when it's `"_"`
    /// (matched by `root_path`) or the registered name when it's a
    /// real alias.
    pub ws_url_alias: String,
    /// Already-canonicalized workspace root.
    pub root_path: PathBuf,
    pub branch: Option<String>,
    pub no_rooting: bool,
}

/// Outcome of [`run_unified_compile`]. The first field is the
/// canonical workspace name (resolved by the helper); the second is
/// the typed outcome. Both SSE and MCP callers branch on the outcome
/// to emit their wire-format terminator.
#[derive(Debug)]
pub(crate) enum UnifiedCompileOutcome {
    Done(crate::pipeline::PipelineResult),
    Cancelled,
    Failed(String),
}

/// Maps a [`crate::pipeline::ProgressEvent`] to the canonical
/// user-facing phase token. The token matches the snake_case form of
/// [`thinkingroot_core::types::CompileStep`] so the diagnostic line
/// rendered by `workspace_status.rs::derive_diagnostics`
/// (`"Compile in progress (phase: {phase})"`) and the desktop's
/// progress-bar step label show the **same** word — pre-fix the badge
/// was frozen at `"starting"` for the whole compile because
/// `WorkspaceStatusMsg::CompilePhase` had no emitter anywhere in the
/// daemon.
///
/// Returns `None` for terminal / informational variants
/// (`PhaseDone`, `IncrementalDone`, `PipelineFailed`, the deleted
/// legacy `Grounding*` family) where dispatching a phase update would
/// either be redundant or actively wrong.
fn phase_token_from_event(ev: &crate::pipeline::ProgressEvent) -> Option<&'static str> {
    use crate::pipeline::ProgressEvent::*;
    use thinkingroot_core::types::CompileStep;
    match ev {
        ParseStart | ParseComplete { .. } | DiffStart | DiffComplete { .. } => Some("reading"),
        ExtractionStart { .. }
        | ExtractionBatchStart { .. }
        | ChunkDone { .. }
        | ExtractionComplete { .. }
        | ExtractionPartial { .. } => Some("extracting"),
        // Witness Mesh persist + fingerprint sit under the "persisting"
        // umbrella — they write to the substrate, not the LLM-driven
        // extract pass.
        WitnessMeshStart { .. } | WitnessMeshDone { .. } | FingerprintDone { .. } => {
            Some("persisting")
        }
        RootingStart { .. } | RootingProgress { .. } | RootingDone { .. } => Some("linking"),
        LinkingStart { .. } | EntityResolved { .. } | LinkComplete { .. } => Some("linking"),
        VectorProgress { .. } | VectorUpdateDone { .. } => Some("persisting"),
        CompilationProgress { .. } | CompilationDone { .. } | VerificationDone { .. } => {
            Some("packing")
        }
        // Authoritative ticker — already carries the canonical step;
        // serialize as snake_case to match the other arms above.
        CompileTick(t) => Some(match t.step {
            CompileStep::Reading => "reading",
            CompileStep::Extracting => "extracting",
            CompileStep::Linking => "linking",
            CompileStep::Persisting => "persisting",
            CompileStep::Packing => "packing",
        }),
        // Legacy grounding tribunal family — removed in the Witness
        // Mesh cutover but variants retained for SSE deserializer
        // back-compat. They never fire post-cutover; map to None so a
        // misbehaving consumer doesn't see a grounding phase that
        // doesn't exist anymore.
        GroundingStart { .. } | GroundingModelReady | GroundingProgress { .. }
        | GroundingDone { .. } => None,
        // Informational / terminal — don't move the badge.
        PhaseDone { .. } | IncrementalDone { .. } | PipelineFailed { .. } => None,
    }
}

/// Shared compile workflow used by the SSE `/compile/stream` endpoint
/// AND the MCP `compile` tool dispatch. Owns the **complete** post-
/// compile reconciliation contract: workspace remount, vector-index
/// rebuild, LLM-probe stamping, mount-success dispatch, terminal
/// `CompileFinished` actor message, **and** EngramManager cache
/// invalidation when `cache_dirty` (which the legacy streaming path
/// silently skipped — every agent-driven compile prior to this ship
/// could return AEP probes against GC'd claim ids).
///
/// Cancellation is end-to-end: the caller owns the [`CancellationToken`]
/// (typically via a [`tokio_util::sync::CancellationToken::drop_guard`]
/// in its scope) and trips it on client disconnect or agent-turn
/// abort. The pipeline observes the same token via `PipelineOptions`
/// and bails at the next phase boundary with `Error::Cancelled`.
///
/// `progress_tx` is forwarded straight to the pipeline; the SSE
/// caller passes its sibling-task channel sender so events stream as
/// they fire, and the MCP caller passes `None` so events are dropped
/// (the agent waits for the final result, not the wire stream).
pub(crate) async fn run_unified_compile(
    state: Arc<AppState>,
    req: UnifiedCompileRequest,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::pipeline::ProgressEvent>>,
    cancel: tokio_util::sync::CancellationToken,
) -> (String, UnifiedCompileOutcome) {
    use crate::pipeline::{PipelineOptions, ProgressEvent, run_pipeline_with_options};

    let compile_started = std::time::Instant::now();

    // Resolve the canonical workspace name. When the CLI POSTs to
    // `/api/v1/ws/_/compile/stream` (the `_` placeholder used by
    // `cortex_remote::run_compile_remote`), `ws` is literally the
    // string `_` — match by `root_path` against the engine's
    // mounted-workspace registry so `workspace_status` dispatches
    // land on the right actor key. Mirrors the bugfix 2026-05-10
    // logic from the original compile_stream body.
    let status_name = if req.ws_url_alias == "_" {
        let engine = state.engine.read().await;
        match engine.list_workspaces().await {
            Ok(list) => list
                .into_iter()
                .find(|w| std::path::PathBuf::from(&w.path) == req.root_path)
                .map(|w| w.name)
                .unwrap_or_else(|| req.ws_url_alias.clone()),
            Err(_) => req.ws_url_alias.clone(),
        }
    } else {
        req.ws_url_alias.clone()
    };

    state
        .workspace_status
        .dispatch(
            &status_name,
            req.root_path.clone(),
            WorkspaceStatusMsg::CompileStarted,
        )
        .await;

    state
        .publish_activity(
            crate::activity::ActivityEvent::new(
                status_name.clone(),
                crate::activity::ActivityClass::Ingest,
                "compile.started",
                "compile started",
            ),
        )
        .await;

    // Sidecar forwarder: intercepts pipeline events so the
    // `workspace_status` actor sees live `CompilePhase` updates AND
    // the caller's `progress_tx` keeps receiving the unchanged event
    // stream. Pre-fix, the actor only saw `CompileStarted` and
    // `CompileFinished` — so `CompileState::Running.phase` stayed
    // frozen at `"starting"` for the whole run, and the
    // `Diagnostic` text in workspace_status.rs:772 displayed
    // `(phase: starting)` even at 99% extracting.
    //
    // **Phase-change-only** dispatch. The pipeline emits one
    // `CompileTick` every 250 ms; the workspace_status actor
    // broadcasts a fresh `WorkspaceStatus` snapshot on every
    // meaningful change; four UI surfaces (ChatView, BuildersPanel,
    // RightRail, PackExportSheet) re-render on every broadcast. A
    // per-tick dispatch produced 4 broadcasts/sec → 16 component
    // re-renders/sec → visible flicker across the entire shell while
    // a compile ran. Dispatching only when the phase token changes
    // collapses this to ~5 dispatches per compile (one per CompileStep
    // boundary). `WorkspaceStatus.compile.progress` is intentionally
    // left as `None` because no UI surface reads it — the live
    // per-source counter flows over the separate Tauri-event channel
    // (`workspace_compile_progress`) consumed by `RightRail`'s
    // progress meter.
    //
    // The forwarder task lives until the pipeline drops its sender
    // clone, then receives `None` and exits naturally — no abort, no
    // explicit cancellation token. We hold the `JoinHandle` to await
    // its drain after the pipeline returns so the final phase
    // dispatch is observed before `CompileFinished` lands (the actor
    // is single-writer so ordering inside it is preserved by message
    // arrival order on the inbox).
    let (forwarder_tx, mut forwarder_rx) =
        tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let workspace_status = state.workspace_status.clone();
    let status_name_for_fwd = status_name.clone();
    let root_path_for_fwd = req.root_path.clone();
    let caller_tx = progress_tx;
    let state_for_fwd = state.clone();
    let ws_name_for_fwd = status_name.clone();
    let forwarder = tokio::spawn(async move {
        let mut last_phase: Option<&'static str> = None;
        while let Some(ev) = forwarder_rx.recv().await {
            // Mirror meaningful ingest milestones onto the activity bus
            // (not every per-chunk tick — only the summary events).
            match &ev {
                ProgressEvent::ExtractionComplete { claims, entities, .. } => {
                    state_for_fwd
                        .publish_activity(
                            crate::activity::ActivityEvent::new(
                                ws_name_for_fwd.clone(),
                                crate::activity::ActivityClass::Ingest,
                                "claims.added",
                                format!("+{claims} claims · {entities} entities"),
                            )
                            .with_detail(
                                serde_json::json!({ "claims": claims, "entities": entities }),
                            ),
                        )
                        .await;
                }
                ProgressEvent::IncrementalDone { .. } => {
                    state_for_fwd
                        .publish_activity(crate::activity::ActivityEvent::new(
                            ws_name_for_fwd.clone(),
                            crate::activity::ActivityClass::Ingest,
                            "compile.done",
                            "compile finished",
                        ))
                        .await;
                }
                _ => {}
            }
            if let Some(phase) = phase_token_from_event(&ev) {
                if last_phase != Some(phase) {
                    workspace_status
                        .dispatch(
                            &status_name_for_fwd,
                            root_path_for_fwd.clone(),
                            WorkspaceStatusMsg::CompilePhase {
                                phase: phase.into(),
                                progress: None,
                            },
                        )
                        .await;
                    last_phase = Some(phase);
                }
            }
            if let Some(tx) = &caller_tx {
                // Caller's receiver may have already dropped (chat
                // turn cancel, SSE disconnect). Suppress the send
                // error — pipeline cancellation is handled via the
                // `cancel` token, not via channel close.
                let _ = tx.send(ev);
            }
        }
    });

    // Run the pipeline. Events flow into `forwarder_tx` (the only
    // pipeline-visible sender); the forwarder task fans them out to
    // the workspace_status actor + the caller's optional channel.
    let pipeline_result = run_pipeline_with_options(
        &req.root_path,
        req.branch.as_deref(),
        Some(forwarder_tx),
        PipelineOptions {
            cancel,
            no_rooting: req.no_rooting,
            skip_byte_audit: false,
            no_incremental: false,
            emit_summaries: false,
        },
    )
    .await;

    // Pipeline returned → its sender clone is dropped → forwarder's
    // `recv()` will return `None` on the next iteration. Awaiting
    // the JoinHandle guarantees the final `CompilePhase` dispatch
    // lands before `CompileFinished` so observers never see the
    // terminal-state badge with a stale "(phase: extracting)" line.
    let _ = forwarder.await;

    let duration_ms = compile_started.elapsed().as_millis() as u64;

    match pipeline_result {
        Ok(result) => {
            finalize_successful_compile(
                state.as_ref(),
                &status_name,
                &req.root_path,
                &result,
                duration_ms,
            )
            .await;
            (status_name, UnifiedCompileOutcome::Done(result))
        }
        Err(e) if e.is_cancelled() => {
            state
                .workspace_status
                .dispatch(
                    &status_name,
                    req.root_path.clone(),
                    WorkspaceStatusMsg::CompileFinished {
                        outcome: thinkingroot_core::types::CompileOutcome::Cancelled {
                            phase: "unknown".into(),
                        },
                        duration_ms,
                        claim_count: 0,
                        entity_count: 0,
                        graph_db_bytes: 0,
                    },
                )
                .await;
            (status_name, UnifiedCompileOutcome::Cancelled)
        }
        Err(e) => {
            state
                .workspace_status
                .dispatch(
                    &status_name,
                    req.root_path.clone(),
                    WorkspaceStatusMsg::CompileFinished {
                        outcome: thinkingroot_core::types::CompileOutcome::Failed {
                            phase: "unknown".into(),
                            reason: e.to_string(),
                        },
                        duration_ms,
                        claim_count: 0,
                        entity_count: 0,
                        graph_db_bytes: 0,
                    },
                )
                .await;
            (status_name, UnifiedCompileOutcome::Failed(e.to_string()))
        }
    }
}

/// Post-compile reconciliation extracted from the legacy
/// `compile_stream` body. Runs on the `Ok(PipelineResult)` branch of
/// [`run_unified_compile`]. Single owner of:
///
/// - Daemon in-memory cache reload (`engine.mount`) so a subsequent
///   `/search` / `/claims` against the fresh graph doesn't return the
///   pre-compile empty view.
/// - Vector index rebuild so `/search/hybrid` and AEP probes work
///   immediately after compile (the v3 pipeline deliberately does
///   not embed — consumer's responsibility).
/// - `LlmProbed { Healthy }` stamp so `readiness.for_query` /
///   `readiness.for_chat` flip true on the post-compile status snapshot
///   (the just-finished compile is empirical evidence the LLM is
///   reachable; the heartbeat decays this back to `Configured` if no
///   fresh probe lands within `LLM_HEALTH_WINDOW`).
/// - `MountSucceeded` dispatch so the mount kind moves
///   `not_mounted → mounted`.
/// - Final `CompileFinished` actor message carrying the success
///   outcome + counts.
/// - `EngramManager.invalidate_workspace` when `cache_dirty` so AEP
///   probes after a writing compile don't return GC'd claim ids.
///   This matches the MCP `compile` handler's pre-refactor behaviour
///   and silently closes the same gap on the streaming path.
async fn finalize_successful_compile(
    state: &AppState,
    status_name: &str,
    root_path: &std::path::Path,
    result: &crate::pipeline::PipelineResult,
    duration_ms: u64,
) {
    let outcome = if result.failed_batches > 0 {
        thinkingroot_core::types::CompileOutcome::Partial {
            extracted_claims: result.claims_count as u64,
            failed_batches: result.failed_batches as u64,
            summary: format!("{} LLM batches", result.failed_batches),
        }
    } else {
        thinkingroot_core::types::CompileOutcome::Success {
            extracted_claims: result.claims_count as u64,
            sources_processed: result.files_parsed as u64,
        }
    };

    let graph_db_bytes = match tokio::fs::metadata(
        root_path
            .join(".thinkingroot")
            .join("graph")
            .join("graph.db"),
    )
    .await
    {
        Ok(m) => m.len(),
        Err(_) => 0,
    };

    // Daemon in-memory cache reload — see doc comment above.
    let mut remount_ok = false;
    {
        let mut engine = state.engine.write().await;
        match engine
            .mount(status_name.to_string(), root_path.to_path_buf())
            .await
        {
            Ok(()) => {
                remount_ok = true;
            }
            Err(e) => {
                tracing::warn!(
                    workspace = %status_name,
                    "post-compile cache reload failed: {e} — \
                     substrate is on disk but daemon's in-memory \
                     view is stale; restart the daemon or POST \
                     /api/v1/workspaces to remount"
                );
            }
        }
    }

    if remount_ok {
        // Defer the vector-index reconcile to a background task
        // (originally backgrounded 2026-05-18; switched from full
        // rebuild to delta reconcile on the same date). Pre-fix this
        // `await` blocked `finalize_*` for 20–30 s on a 600-claim
        // workspace because `rebuild_vector_index` re-embedded every
        // claim + entity through ONNX inference. Backgrounding moved
        // the wait off the critical path; `reconcile_vector_index`
        // then collapses the work itself — a typical 1-file edit
        // re-embeds ~10 claims instead of all 600.
        //
        // Backgrounding is safe because:
        //   - The vector store is per-workspace and self-locked
        //     (`storage_arc.blocking_lock()` inside the spawn_blocking
        //     pool — see `engine.rs::reconcile_vector_index`). A
        //     search that races the reconcile blocks on that mutex
        //     until it completes; never reads a torn intermediate.
        //   - Compile slot serialisation (compile-resilience.md, single
        //     `CompileHandle`) means two concurrent compiles for the
        //     same workspace cannot both spawn this — a second compile
        //     can only start after the first's `finalize_*` has
        //     returned, by which time the previous `tokio::spawn`'s
        //     own task is the only contender for the storage lock.
        //   - Failures still surface via `tracing::warn!` — daemon
        //     operators see the warning in `serve.log`; the next
        //     compile's `finalize_*` schedules a fresh attempt.
        //
        // What this trades: a search that lands inside the reconcile
        // window pays its share of the reconcile latency (mutex wait)
        // instead of returning empty. That is the honest behaviour —
        // CLAUDE.md §honesty rule §1 forbids "fake data, ever", so we
        // never surface a half-built index as if it were complete.
        let engine = state.engine.clone();
        let ws = status_name.to_string();
        // The reconcile inherits a fresh cancel token. Today there is
        // no producer-side path that fires it — the background task
        // runs to completion or fails. Future work (Tier 2: yield to
        // a fresh compile or unmount) will arm this token at those
        // points. The plumbing lands now so the contract is in place.
        let reconcile_cancel = tokio_util::sync::CancellationToken::new();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let result = engine
                .read()
                .await
                .reconcile_vector_index(&ws, reconcile_cancel)
                .await;
            let elapsed_ms = started.elapsed().as_millis();
            match result {
                Ok(stats) => tracing::info!(
                    workspace = %ws,
                    elapsed_ms,
                    existing = stats.existing,
                    current = stats.current,
                    removed = stats.removed,
                    added = stats.added,
                    "background vector index reconciled (delta path)"
                ),
                Err(e) => tracing::warn!(
                    workspace = %ws,
                    elapsed_ms,
                    "background vector index reconcile failed: {e} — \
                     semantic search and AEP probes will return empty \
                     results until the next compile re-attempts"
                ),
            }
        });
    }

    if remount_ok {
        let (counts, llm_summary) = {
            let engine = state.engine.read().await;
            let counts = engine.list_workspaces().await.ok().and_then(|list| {
                list.into_iter()
                    .find(|w| w.name == status_name)
                    .map(|w| {
                        (
                            w.claim_count as u64,
                            w.entity_count as u64,
                            w.source_count as u64,
                        )
                    })
            });
            let llm = engine.workspace_llm_summary(status_name);
            (counts, llm)
        };
        if let Some((provider, model)) = llm_summary {
            state
                .workspace_status
                .dispatch(
                    status_name,
                    root_path.to_path_buf(),
                    WorkspaceStatusMsg::LlmProbed {
                        state: thinkingroot_core::types::LlmState::Healthy {
                            provider,
                            model: Some(model),
                            last_probed_at: chrono::Utc::now(),
                        },
                    },
                )
                .await;
        }
        if let Some((claim_count, entity_count, source_count)) = counts {
            state
                .workspace_status
                .dispatch(
                    status_name,
                    root_path.to_path_buf(),
                    WorkspaceStatusMsg::MountSucceeded {
                        claim_count,
                        entity_count,
                        source_count_at_last_compile: source_count,
                        graph_db_bytes,
                    },
                )
                .await;
        }
    }

    state
        .workspace_status
        .dispatch(
            status_name,
            root_path.to_path_buf(),
            WorkspaceStatusMsg::CompileFinished {
                outcome,
                duration_ms,
                claim_count: result.claims_count as u64,
                entity_count: result.entities_count as u64,
                graph_db_bytes,
            },
        )
        .await;

    // Engram cache invalidation — matches the legacy `engine.compile`
    // path's contract. Without this, a probe after a writing compile
    // can return rows whose claim ids were just GC'd by the new
    // substrate write. The legacy `compile_stream` body skipped this
    // because it didn't go through `QueryEngine::compile`; folding it
    // into the helper closes the gap for every caller in one place.
    if result.cache_dirty {
        state.engram_manager.invalidate_workspace(status_name).await;
    }
}

async fn compile_stream(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<CompileStreamRequest>,
) -> Response {
    use tokio_util::sync::CancellationToken;

    let root_path = match (body.root_path.as_deref(), state.current_workspace_root().await) {
        (Some(p), _) => PathBuf::from(p),
        (None, Some(r)) => r,
        (None, None) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "MISSING_ROOT_PATH",
                "request body must include root_path when the server has no --path arg",
            );
        }
    };

    // Bugfix 2026-05-10 — canonicalize the path so workspace-registry
    // matching works when the CLI sends a relative path like "." (which
    // is what `root compile .` produces). Without canonicalization we'd
    // compare `PathBuf::from(".")` against the registry's absolute paths,
    // miss every match, and leak a phantom workspace named "_" with
    // path "." into the engine's mount table.
    let root_path = std::fs::canonicalize(&root_path).unwrap_or(root_path);

    if !root_path.is_dir() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "ROOT_PATH_NOT_DIR",
            &format!("root_path is not a directory: {}", root_path.display()),
        );
    }

    // Build the helper request. All path / `_`-alias / canonical-
    // name resolution + workspace_status dispatching + post-compile
    // remount/vector-rebuild lives inside `run_unified_compile` so
    // the MCP `compile` tool gets the exact same behaviour.
    let req = UnifiedCompileRequest {
        ws_url_alias: ws.clone(),
        root_path: root_path.clone(),
        branch: body.branch.clone(),
        no_rooting: body.no_rooting,
    };

    // Channel that the helper writes to and the SSE stream below
    // pumps events from.
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::pipeline::ProgressEvent>();

    // The DropGuard fires the cancel token when the SSE stream is
    // dropped (client disconnect, axum body cancellation, etc.).
    // The pipeline observes the same token via `PipelineOptions` and
    // bails at the next phase boundary — the engine-pipeline.md
    // cancellation contract (every stateful REST handler binds a
    // CancellationToken + DropGuard inside its response body).
    let cancel = CancellationToken::new();
    let cancel_for_helper = cancel.clone();
    let drop_guard = cancel.drop_guard();

    // Spawn the helper as a sibling task so the SSE stream below
    // pumps `progress_rx` concurrently with the pipeline running.
    // The helper owns ALL the post-compile reconciliation (remount,
    // vector-index rebuild, LLM-probe stamp, mount-success dispatch,
    // terminal CompileFinished, engram cache invalidation); when it
    // returns we just yield the wire terminator.
    let helper_state = state.clone();
    let helper_handle = tokio::spawn(async move {
        run_unified_compile(helper_state, req, Some(progress_tx), cancel_for_helper).await
    });

    let stream = async_stream::stream! {
        let _guard = drop_guard;

        while let Some(event) = progress_rx.recv().await {
            let payload = match serde_json::to_string(&event) {
                Ok(s) => s,
                Err(e) => {
                    // Should not happen — every ProgressEvent variant
                    // is composed of primitives. If it does, surface
                    // the error rather than silently swallowing the
                    // event so the desktop can show a real failure
                    // instead of an incomplete progress stream.
                    let payload = serde_json::json!({
                        "error": format!("progress event encode failed: {e}"),
                    })
                    .to_string();
                    yield Ok::<Event, std::convert::Infallible>(
                        Event::default().event("failed").data(payload),
                    );
                    return;
                }
            };
            yield Ok(Event::default().event("progress").data(payload));
        }

        // Channel closed → the helper task has finished. Yield the
        // single terminator event that matches its outcome. The
        // helper has already stamped the workspace_status actor +
        // remounted the workspace + invalidated engrams, so the
        // terminator is the *only* thing the SSE stream still owes
        // the client.
        match helper_handle.await {
            Ok((_status_name, UnifiedCompileOutcome::Done(result))) => {
                let payload = serde_json::to_string(&result)
                    .unwrap_or_else(|_| "{}".to_string());
                yield Ok(Event::default().event("done").data(payload));
            }
            Ok((_, UnifiedCompileOutcome::Cancelled)) => {
                yield Ok(Event::default().event("cancelled").data("{}"));
            }
            Ok((_, UnifiedCompileOutcome::Failed(msg))) => {
                let payload = serde_json::json!({ "error": msg }).to_string();
                yield Ok(Event::default().event("failed").data(payload));
            }
            Err(e) => {
                let payload = serde_json::json!({
                    "error": format!("compile task panicked: {e}"),
                })
                .to_string();
                yield Ok(Event::default().event("failed").data(payload));
            }
        }
    };

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

async fn verify_ws(State(state): State<Arc<AppState>>, Path(ws): Path<String>) -> Response {
    let engine = state.engine.read().await;
    match engine.verify(&ws).await {
        Ok(result) => ok_response(result).into_response(),
        Err(e) => match_engine_error(e),
    }
}

// ─── Branch Handlers ─────────────────────────────────────────

async fn list_branches_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            // No workspace root set — return empty list (server started without --path)
            let empty: Vec<serde_json::Value> = vec![];
            return ok_response(serde_json::json!({ "branches": empty })).into_response();
        }
    };
    match thinkingroot_branch::list_branches(&root) {
        Ok(branches) => ok_response(serde_json::json!({ "branches": branches })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

async fn get_head_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return ok_response(serde_json::json!({ "head": "main" })).into_response();
        }
    };
    match thinkingroot_branch::read_head_branch(&root) {
        Ok(head) => ok_response(serde_json::json!({ "head": head })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct CreateBranchRequest {
    name: String,
    parent: Option<String>,
    description: Option<String>,
    /// T0.6 — optional explicit BranchKind. Defaults to Feature.
    #[serde(default)]
    kind: Option<thinkingroot_core::BranchKind>,
    /// T0.6 — optional explicit MergePolicy. Defaults to Manual.
    #[serde(default)]
    merge_policy: Option<thinkingroot_core::MergePolicy>,
    /// T2.6 — optional redaction policy. Defaults to no redaction.
    #[serde(default)]
    redaction: Option<thinkingroot_core::RedactionPolicy>,
    /// T3.7 — apply the named template's defaults to any field on
    /// this request that the caller did not explicitly set.  Explicit
    /// fields always win — the template never overrides a value the
    /// caller asked for.
    #[serde(default)]
    template: Option<String>,
}

async fn create_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateBranchRequest>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let parent = body.parent.as_deref().unwrap_or("main");

    // T3.7 — apply template defaults to any field the caller left
    // unset.  An invalid template name returns 400 rather than
    // silently materialising the branch with engine defaults — the
    // caller asked for a template, give them a clear error.
    let mut kind = body.kind;
    let mut merge_policy = body.merge_policy;
    let mut redaction = body.redaction;
    let mut permissions: Option<thinkingroot_core::BranchPermissions> = None;
    let mut max_age_secs: Option<u64> = None;
    if let Some(template_name) = body.template.as_deref() {
        use thinkingroot_branch::templates::TemplateRegistry;
        let refs_dir = root.join(".thinkingroot-refs");
        let registry = match TemplateRegistry::load_or_seed(&refs_dir) {
            Ok(r) => r,
            Err(e) => {
                return err_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "TEMPLATE_LOAD_FAILED",
                    &e.to_string(),
                );
            }
        };
        let Some(template) = registry.get(template_name) else {
            return err_response(
                StatusCode::NOT_FOUND,
                "TEMPLATE_NOT_FOUND",
                &format!("no branch template named '{template_name}'"),
            );
        };
        kind.get_or_insert_with(|| template.kind.clone());
        if merge_policy.is_none() {
            merge_policy = Some(template.merge_policy.clone());
        }
        if redaction.is_none() {
            redaction = template.redaction.clone();
        }
        permissions = template.permissions.clone();
        max_age_secs = template.max_age_secs;
    }

    match thinkingroot_branch::create_branch_full(
        &root,
        &body.name,
        parent,
        body.description,
        request_user(&headers),
        permissions.unwrap_or_default(),
        kind.unwrap_or_default(),
        merge_policy.unwrap_or_default(),
        redaction,
    )
    .await
    {
        Ok(branch) => {
            // T3.7 — when the template specified a TTL, apply it now
            // via the post-create setter.  The TTL is the only
            // template field that can't be passed to
            // `create_branch_full` directly because the registry path
            // for it predates templates.
            if let Some(ttl) = max_age_secs {
                if let Err(e) = thinkingroot_branch::set_branch_max_age_secs(
                    &root,
                    &branch.name,
                    Some(ttl),
                ) {
                    return err_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "TEMPLATE_TTL_APPLY_FAILED",
                        &e.to_string(),
                    );
                }
            }
            // T1.6 — publish the `Created` event the registry just
            // appended on the new broadcast channel so any client
            // already subscribed to `/branches/{name}/events/stream`
            // picks it up live.
            publish_latest_branch_event(&state, &branch.name).await;
            // Sync the new durable branch as a graph node so the brain can
            // describe its own topology (no-op for ephemeral `stream/*`).
            // Best-effort — a node-sync failure never fails branch creation.
            {
                let kind_label = format!("{:?}", branch.kind);
                let kind_label = kind_label
                    .split(|c: char| c == ' ' || c == '{')
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                let created = chrono::Utc::now().timestamp() as f64;
                let engine = state.engine.read().await;
                let _ = engine
                    .sync_branch_created(
                        &root,
                        &branch.name,
                        Some(parent),
                        Some(&kind_label),
                        created,
                    )
                    .await;
            }
            ok_response(serde_json::json!({ "branch": branch })).into_response()
        }
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

// ─── T0.7: contribute-bulk ────────────────────────────────────────────

#[derive(Deserialize)]
struct ContributeBulkRequest {
    /// Workspace name (matches the mounted workspace identifier).
    workspace: String,
    /// Optional session id for turn-calendar attribution. When absent,
    /// the synthetic session id derived from the connector identity is
    /// used.
    #[serde(default)]
    session_id: Option<String>,
    /// Connector identifier (e.g. `"github"`, `"slack"`, `"notion"`).
    connector_id: String,
    /// Per-install identifier (`"alice-acme-prod"`).
    install_id: String,
    /// Idempotency key (the connector picks this; typically the
    /// webhook delivery id or the upstream event id).
    idempotency_key: String,
    /// When `true`, skip per-claim rooting (deferred to end of batch).
    #[serde(default)]
    backfill: bool,
    /// The batch of claims being contributed.
    claims: Vec<crate::engine::AgentClaim>,
}

async fn contribute_bulk_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
    Json(body): Json<ContributeBulkRequest>,
) -> impl IntoResponse {
    let principal = crate::engine::Principal::Connector {
        connector_id: body.connector_id.clone(),
        install_id: body.install_id.clone(),
    };
    let session_id = body.session_id.unwrap_or_else(|| {
        format!(
            "connector:{}:{}:{}",
            body.connector_id, body.install_id, body.idempotency_key
        )
    });
    let branch_arg = if branch == "main" {
        None
    } else {
        Some(branch.as_str())
    };

    let engine = state.engine.read().await;
    match engine
        .contribute_bulk(
            &body.workspace,
            &session_id,
            branch_arg,
            body.claims,
            &state.sessions,
            principal,
            &body.idempotency_key,
            body.backfill,
        )
        .await
    {
        Ok(result) => {
            // T1.6 — `contribute_bulk` appends a `ContributeBulk`
            // BranchEvent on success.  Broadcast it for live
            // subscribers (only meaningful when contributing to a
            // named branch — `main` has no per-branch broadcast key,
            // but we publish anyway for symmetry).
            drop(engine);
            publish_latest_branch_event(&state, &branch).await;
            ok_response(serde_json::json!(result)).into_response()
        }
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "CONTRIBUTE_BULK_FAILED",
            &e.to_string(),
        ),
    }
}

/// A2 — request body for `POST /ws/{ws}/extract-contribute`. `text` is the raw
/// conversation turn (or transcript); the engine runs the LLM extractor over it
/// and contributes the resulting atomic claims to `branch` (or main).
#[derive(Deserialize)]
struct ExtractContributeRequest {
    text: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    connector_id: String,
    install_id: String,
    idempotency_key: String,
}

async fn extract_contribute_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<ExtractContributeRequest>,
) -> impl IntoResponse {
    if let Err(resp) = ensure_user_ws(&state, &ws).await {
        return resp;
    }
    let principal = crate::engine::Principal::Connector {
        connector_id: body.connector_id.clone(),
        install_id: body.install_id.clone(),
    };
    let session_id = body.session_id.unwrap_or_else(|| {
        format!(
            "connector:{}:{}:{}",
            body.connector_id, body.install_id, body.idempotency_key
        )
    });
    let branch_arg = match body.branch.as_deref() {
        Some("main") | None => None,
        Some(b) => Some(b),
    };
    let engine = state.engine.read().await;
    match engine
        .extract_and_contribute(
            &ws,
            &body.text,
            branch_arg,
            &session_id,
            &state.sessions,
            principal,
            &body.idempotency_key,
        )
        .await
    {
        Ok(result) => {
            drop(engine);
            if let Some(b) = body.branch.as_deref() {
                publish_latest_branch_event(&state, b).await;
            }
            ok_response(serde_json::json!(result)).into_response()
        }
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "EXTRACT_CONTRIBUTE_FAILED",
            &e.to_string(),
        ),
    }
}

/// §6 — request body for `POST /ws/{ws}/caption-image`. The image rides as
/// base64 (a `data:…;base64,` prefix is tolerated); `media_type` is its MIME
/// type. `connector_id`/`install_id`/`idempotency_key` scope the contribute
/// (same idempotency model as extract-contribute; default key = image hash).
#[derive(Deserialize)]
struct CaptionImageRequest {
    image_base64: String,
    #[serde(default = "default_image_media_type")]
    media_type: String,
    #[serde(default)]
    instruction: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    connector_id: Option<String>,
    #[serde(default)]
    install_id: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}
fn default_image_media_type() -> String {
    "image/jpeg".to_string()
}

/// §6 multimodal — caption an image with the workspace vision LLM and
/// contribute the resulting claims (caption-then-extract). Honest on failure:
/// bad base64, no LLM, or a non-vision provider returns an error, never a
/// fabricated claim.
async fn caption_image_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<CaptionImageRequest>,
) -> impl IntoResponse {
    if let Err(resp) = ensure_user_ws(&state, &ws).await {
        return resp;
    }
    // Tolerate a data-URL prefix; decode to raw bytes.
    let raw_b64 = body
        .image_base64
        .split_once(";base64,")
        .map(|(_, b)| b)
        .unwrap_or(&body.image_base64);
    let bytes = {
        use base64::Engine;
        match base64::engine::general_purpose::STANDARD.decode(raw_b64.trim().as_bytes()) {
            Ok(b) => b,
            Err(e) => {
                return err_response(StatusCode::BAD_REQUEST, "BAD_IMAGE_BASE64", &e.to_string());
            }
        }
    };
    let principal = crate::engine::Principal::Connector {
        connector_id: body.connector_id.clone().unwrap_or_else(|| "vision".to_string()),
        install_id: body.install_id.clone().unwrap_or_else(|| "default".to_string()),
    };
    let branch_arg = match body.branch.as_deref() {
        Some("main") | None => None,
        Some(b) => Some(b),
    };
    let idem = body.idempotency_key.clone().unwrap_or_default();
    let engine = state.engine.read().await;
    match engine
        .caption_and_contribute(
            &ws,
            &bytes,
            &body.media_type,
            body.instruction.as_deref(),
            branch_arg,
            &state.sessions,
            principal,
            &idem,
        )
        .await
    {
        Ok((caption, result, sha)) => {
            drop(engine);
            if let Some(b) = body.branch.as_deref() {
                publish_latest_branch_event(&state, b).await;
            }
            ok_response(serde_json::json!({
                "caption": caption,
                "image_sha256": sha,
                "contribute": result,
            }))
            .into_response()
        }
        Err(e) => {
            err_response(StatusCode::BAD_REQUEST, "CAPTION_IMAGE_FAILED", &e.to_string())
        }
    }
}

/// §6 P2 — request body for `POST /ws/{ws}/ingest-transcript`. Audio "claims
/// with ears": a (speaker/time-segmented) transcript becomes speaker-stamped
/// claims through the existing extraction pipeline, with audio provenance. ASR
/// is the caller's (their Whisper / meeting tool); we own the cognition.
/// `audio_sha256` (the caller's audio blob hash) anchors provenance; absent →
/// derived from the transcript text.
#[derive(Deserialize)]
struct IngestTranscriptRequest {
    segments: Vec<crate::intelligence::transcript::TranscriptSegment>,
    #[serde(default)]
    audio_sha256: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    connector_id: Option<String>,
    #[serde(default)]
    install_id: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

async fn ingest_transcript_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<IngestTranscriptRequest>,
) -> Response {
    if let Err(resp) = ensure_user_ws(&state, &ws).await {
        return resp;
    }
    let doc = crate::intelligence::transcript::format_transcript(&body.segments);
    if doc.trim().is_empty() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "EMPTY_TRANSCRIPT",
            "no non-empty transcript segments",
        );
    }
    // Provenance: claims trace to audio://<sha>. Caller's audio hash if given,
    // else the transcript's own hash (still a stable, queryable anchor).
    let sha = body
        .audio_sha256
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| blake3::hash(doc.as_bytes()).to_hex().to_string());
    let principal = crate::engine::Principal::Connector {
        connector_id: body.connector_id.clone().unwrap_or_else(|| "audio".to_string()),
        install_id: body.install_id.clone().unwrap_or_else(|| "default".to_string()),
    };
    let branch_arg = match body.branch.as_deref() {
        Some("main") | None => None,
        Some(b) => Some(b),
    };
    let session_id = format!("audio:{sha}");
    let idem = body
        .idempotency_key
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("audio:{sha}"));
    let engine = state.engine.read().await;
    // Structured ingest: each segment's claims get a per-utterance
    // `audio://<sha>?t_start=..&t_end=..&speaker=..` provenance URI (queryable
    // span + speaker), not just inline tags in one flattened doc. `doc` above
    // still gates the empty-transcript check + the fallback content hash.
    match engine
        .ingest_transcript_structured(
            &ws,
            &body.segments,
            &sha,
            branch_arg,
            &session_id,
            &state.sessions,
            principal,
            &idem,
        )
        .await
    {
        Ok(result) => {
            drop(engine);
            if let Some(b) = body.branch.as_deref() {
                publish_latest_branch_event(&state, b).await;
            }
            ok_response(serde_json::json!({ "audio_sha256": sha, "contribute": result }))
                .into_response()
        }
        Err(e) => err_response(StatusCode::BAD_REQUEST, "INGEST_TRANSCRIPT_FAILED", &e.to_string()),
    }
}

/// C1 — request body for `POST /ws/{ws}/consolidate`. `max_entities` bounds the
/// blast radius + LLM cost of a single pass (newest entities are cheap to skip).
#[derive(Deserialize)]
struct ConsolidateRequest {
    #[serde(default = "default_max_entities")]
    max_entities: usize,
}
fn default_max_entities() -> usize {
    100
}

async fn consolidate_claims_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    body: Option<Json<ConsolidateRequest>>,
) -> impl IntoResponse {
    if let Err(resp) = ensure_user_ws(&state, &ws).await {
        return resp;
    }
    let max_entities = body.map(|b| b.max_entities).unwrap_or_else(default_max_entities);
    let engine = state.engine.read().await;
    match engine.consolidate(&ws, max_entities).await {
        Ok(report) => ok_response(serde_json::json!(report)).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, "CONSOLIDATE_FAILED", &e.to_string()),
    }
}

/// #1 — ensure a per-user workspace is mounted before a scoped engine call.
/// Cheap read-check first; only takes the write lock to auto-mount a `u_*`
/// namespace on first reference. Non-`u_` names are left to the normal
/// not-mounted error path. Returns an error `Response` on mount failure.
async fn ensure_user_ws(state: &AppState, ws: &str) -> std::result::Result<(), Response> {
    if !ws.starts_with("u_") {
        return Ok(());
    }
    let mounted = { state.engine.read().await.is_mounted(ws) };
    if mounted {
        return Ok(());
    }
    let mut engine = state.engine.write().await;
    engine.get_or_mount_user_ws(ws).await.map_err(|e| {
        err_response(StatusCode::INTERNAL_SERVER_ERROR, "WS_MOUNT_FAILED", &e.to_string())
    })
}

/// `POST /api/v1/ws/{ws}/contribute-bulk` — #1 — the per-user-namespaced
/// store path. Writes claims into `ws`'s main (auto-mounting a per-user
/// workspace on first use), so the gateway can confine a scoped key by the
/// path segment (contribute-bulk's body-only workspace can't be guarded).
#[derive(Deserialize)]
struct WsContributeRequest {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default = "default_sdk_connector")]
    connector_id: String,
    install_id: String,
    idempotency_key: String,
    #[serde(default)]
    backfill: bool,
    claims: Vec<crate::engine::AgentClaim>,
}

fn default_sdk_connector() -> String {
    "sdk".to_string()
}

async fn ws_contribute_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<WsContributeRequest>,
) -> Response {
    if let Err(resp) = ensure_user_ws(&state, &ws).await {
        return resp;
    }
    let principal = crate::engine::Principal::Connector {
        connector_id: body.connector_id.clone(),
        install_id: body.install_id.clone(),
    };
    let session_id = body.session_id.unwrap_or_else(|| {
        format!(
            "connector:{}:{}:{}",
            body.connector_id, body.install_id, body.idempotency_key
        )
    });
    let engine = state.engine.read().await;
    match engine
        .contribute_bulk(
            &ws,
            &session_id,
            None, // write to the per-user workspace's main
            body.claims,
            &state.sessions,
            principal,
            &body.idempotency_key,
            body.backfill,
        )
        .await
    {
        Ok(result) => ok_response(serde_json::json!(result)).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, "CONTRIBUTE_FAILED", &e.to_string()),
    }
}

// ─── T2.6: per-branch redaction policy ────────────────────────────────

#[derive(Deserialize)]
struct SetRedactionRequest {
    /// `null` clears the policy; an object sets it.
    policy: Option<thinkingroot_core::RedactionPolicy>,
}

async fn set_branch_redaction_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
    Json(body): Json<SetRedactionRequest>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    match thinkingroot_branch::set_branch_redaction(&root, &branch, body.policy) {
        Ok(updated) => {
            // T1.6 — `set_branch_redaction` appends `RedactionUpdated`.
            publish_latest_branch_event(&state, &updated.name).await;
            ok_response(serde_json::json!({ "branch": updated })).into_response()
        }
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "REDACTION_UPDATE_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── T1.3: Branch audit log ──────────────────────────────────────────
//
// Returns the append-only `events` vec on a `BranchRef`, oldest-first.
// Useful for "who changed this branch when?" UX and as the source of
// truth for the lineage DAG (T1.7) and SSE stream (T1.6).

async fn list_branch_events_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::branch::BranchRegistry;
    let registry = match BranchRegistry::load_or_create(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BRANCH_ERROR",
                &e.to_string(),
            );
        }
    };
    // Look across both active + abandoned + merged so a merged branch
    // still answers its history (lineage UI walks merged branches).
    let events = registry
        .all()
        .into_iter()
        .find(|b| b.name == branch)
        .map(|b| b.events.clone())
        .unwrap_or_default();
    ok_response(serde_json::json!({
        "branch": branch,
        "events": events,
    }))
    .into_response()
}

// ─── T1.6: Live SSE branch events ────────────────────────────────────
//
// Subscribers connect to `GET /branches/{branch}/events/stream`,
// receive the per-branch broadcast channel, and forward every
// `BranchEvent` published after a successful mutation as one SSE
// `branch_event` frame.  No backfill — the polling endpoint
// `/branches/{branch}/events` is the source of historical events.
// Channel-lag (a slow consumer that fell more than 64 events behind)
// is surfaced as a `lagged` event so the client can refetch via the
// polling endpoint and resume.

async fn stream_branch_events_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> Response {
    use tokio_stream::StreamExt as _;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

    let tx = state.branch_event_sender(&branch).await;
    let rx = tx.subscribe();
    let stream = BroadcastStream::new(rx).map(move |res| match res {
        Ok(event) => {
            let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            Ok::<Event, std::convert::Infallible>(
                Event::default().event("branch_event").data(payload),
            )
        }
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            let payload = serde_json::json!({ "missed": n }).to_string();
            Ok(Event::default().event("lagged").data(payload))
        }
    });

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// Task 15 — aggregate SSE stream of every branch event. The
/// desktop's left-rail branch tree subscribes once here and sees
/// `Created` / `Merged` / `Abandoned` / `RedactionUpdated` /
/// `ContributeBulk` events for every branch in the workspace
/// without holding N per-branch connections.
///
/// Wire format mirrors the per-branch stream — `event:
/// branch_event` data is `{branch: "...", event: <BranchEvent
/// JSON>}`. Slow consumers see `event: lagged` with a `missed`
/// counter so they can refetch via `/branches/{branch}/events`.
///
/// Lifecycle: `branch_event_aggregate` is a single broadcast
/// channel created at AppState init (capacity 256) — every
/// successful branch mutation publishes here in addition to the
/// per-branch hub, so the aggregate stream is always live.
async fn stream_all_branch_events_handler(
    State(state): State<Arc<AppState>>,
) -> Response {
    use tokio_stream::StreamExt as _;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

    let rx_br = state.branch_event_aggregate.subscribe();
    let rx_hd = state.head_change_tx.subscribe();

    let s1 = BroadcastStream::new(rx_br).map(move |res| match res {
        Ok((branch, event)) => {
            let payload = serde_json::json!({
                "branch": branch,
                "event": event,
            })
            .to_string();
            Ok::<Event, std::convert::Infallible>(
                Event::default().event("branch_event").data(payload),
            )
        }
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            let payload = serde_json::json!({ "missed": n }).to_string();
            Ok(Event::default().event("lagged").data(payload))
        }
    });

    let s2 = BroadcastStream::new(rx_hd).map(move |res| match res {
        Ok(head) => {
            let payload = serde_json::json!({ "head": head }).to_string();
            Ok::<Event, std::convert::Infallible>(
                Event::default().event("head_changed").data(payload),
            )
        }
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            let payload = serde_json::json!({ "missed": n }).to_string();
            Ok(Event::default().event("lagged").data(payload))
        }
    });

    let stream = s1.merge(s2);

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ─── Unified activity log ────────────────────────────────────────────

/// Live SSE tail of activity events. Pairs with the `/activity` history
/// endpoint (client backfills, then follows live). A cloud engine serves
/// one project, so we stream ALL events rather than filtering by the
/// `{ws}` path segment — the segment is kept for API symmetry, and each
/// event carries its own `ws` for display. (Filtering here would
/// silently drop everything if the caller's workspace name didn't match
/// the engine's mounted directory name — a honesty-rule trap.)
async fn stream_activity_handler(
    State(state): State<Arc<AppState>>,
    Path(_ws): Path<String>,
) -> Response {
    use tokio_stream::StreamExt as _;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

    let rx = state.activity_tx.subscribe();
    let stream = BroadcastStream::new(rx).map(move |res| match res {
        Ok(ev) => {
            let payload = serde_json::to_string(&ev).unwrap_or_default();
            Ok::<Event, std::convert::Infallible>(
                Event::default().event("activity").data(payload),
            )
        }
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            let payload = serde_json::json!({ "missed": n }).to_string();
            Ok(Event::default().event("lagged").data(payload))
        }
    });

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

#[derive(serde::Deserialize)]
struct ActivityQuery {
    limit: Option<usize>,
    before: Option<chrono::DateTime<chrono::Utc>>,
}

/// Durable activity history for one workspace, newest-window last.
/// Honest empty array when there is no log yet (not a 500/404 surface).
async fn list_activity_handler(
    State(state): State<Arc<AppState>>,
    Path(_ws): Path<String>,
    Query(q): Query<ActivityQuery>,
) -> Response {
    let Some(root) = state.current_workspace_root().await else {
        return ok_response(serde_json::json!([])).into_response();
    };
    let limit = q.limit.unwrap_or(200).min(1000);
    match crate::activity::read_recent(&root, limit, q.before) {
        Ok(evs) => ok_response(serde_json::json!(evs)).into_response(),
        Err(_) => ok_response(serde_json::json!([])).into_response(),
    }
}

/// Live roster of connected MCP sessions, read from in-memory telemetry.
/// Empty array when nothing is connected (honest empty state).
async fn list_mcp_sessions_handler(State(state): State<Arc<AppState>>) -> Response {
    let map = state.mcp_session_telemetry.read().await;
    let sessions: Vec<_> = map.values().cloned().collect();
    ok_response(serde_json::json!({ "sessions": sessions })).into_response()
}

// ─── T1.2: Branch stats ──────────────────────────────────────────────
//
// Cheap per-branch probe — claim / entity / source counts — without
// running a full `compute_diff`.  Reads the branch's own GraphStore;
// avoids the AEP path so the substrate cost is bounded by table
// scans, not Datalog.

#[derive(Serialize)]
struct BranchStatsResponse {
    branch: String,
    /// Number of claims in the branch's graph.db (post any merges).
    claim_count: usize,
    /// Number of entities.
    entity_count: usize,
    /// Number of source rows.
    source_count: usize,
    /// Number of audit-log entries currently retained on this branch
    /// (capped by `MAX_EVENTS`).
    event_count: usize,
    /// Lifecycle state (active / merged / abandoned) for the row.
    status: String,
}

async fn branch_stats_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    use thinkingroot_branch::branch::BranchRegistry;
    use thinkingroot_branch::snapshot::resolve_data_dir;
    use thinkingroot_graph::graph::GraphStore;

    let refs_dir = root.join(".thinkingroot-refs");
    let registry = match BranchRegistry::load_or_create(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BRANCH_ERROR",
                &e.to_string(),
            );
        }
    };
    let branch_ref = match registry.all().into_iter().find(|b| b.name == branch) {
        Some(b) => b.clone(),
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "BRANCH_NOT_FOUND",
                &format!("branch '{branch}' not found"),
            );
        }
    };

    let branch_arg = if branch == "main" { None } else { Some(branch.as_str()) };
    let data_dir = resolve_data_dir(&root, branch_arg);
    if !data_dir.exists() {
        // Branch entry exists but data dir is gone (abandoned branches
        // have their dir removed by gc).  Report what we know about
        // the audit-log without lying about substrate counts.
        return ok_response(BranchStatsResponse {
            branch: branch.clone(),
            claim_count: 0,
            entity_count: 0,
            source_count: 0,
            event_count: branch_ref.events.len(),
            status: branch_status_label(&branch_ref.status),
        })
        .into_response();
    }

    let graph = match GraphStore::init(&data_dir.join("graph")) {
        Ok(g) => g,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &e.to_string(),
            );
        }
    };
    // Stats queries must propagate graph errors — returning 0s on a query
    // failure would lie to the caller (a stats response with all zeros is
    // indistinguishable from an empty branch).
    let claims = match graph.get_all_claims_with_sources() {
        Ok(rows) => rows,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &format!("branch_stats: failed to read claims for branch '{branch}': {e}"),
            );
        }
    };
    let entities = match graph.get_all_entities() {
        Ok(rows) => rows,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &format!("branch_stats: failed to read entities for branch '{branch}': {e}"),
            );
        }
    };
    let sources = match graph.get_all_sources() {
        Ok(rows) => rows,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &format!("branch_stats: failed to read sources for branch '{branch}': {e}"),
            );
        }
    };

    ok_response(BranchStatsResponse {
        branch: branch.clone(),
        claim_count: claims.len(),
        entity_count: entities.len(),
        source_count: sources.len(),
        event_count: branch_ref.events.len(),
        status: branch_status_label(&branch_ref.status),
    })
    .into_response()
}

fn branch_status_label(status: &thinkingroot_core::BranchStatus) -> String {
    use thinkingroot_core::BranchStatus;
    match status {
        BranchStatus::Active => "active".into(),
        BranchStatus::Merged { .. } => "merged".into(),
        BranchStatus::Abandoned { .. } => "abandoned".into(),
    }
}

// ─── T2.5: Tag create / list / get ───────────────────────────────────
//
// `POST /api/v1/tags` registers an immutable [`BranchKind::Tag`].
// `GET /api/v1/tags` lists every active tag.
// `GET /api/v1/tags/{name}` returns one.

#[derive(Deserialize)]
struct CreateTagRequest {
    /// User-visible tag name (e.g. `"v1.0.0"`, `"q1-snapshot"`).
    name: String,
    /// Internal ref pointer (e.g. `"refs/tags/v1.0.0"`); free-form.
    ref_name: String,
    /// Pinned target — typically a BLAKE3 commit hash matching
    /// `BranchRef::parent_commit_hash`.
    target: String,
    #[serde(default)]
    description: Option<String>,
}

async fn create_tag_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateTagRequest>,
) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    match thinkingroot_branch::create_tag(
        &root,
        &body.name,
        &body.ref_name,
        &body.target,
        request_user(&headers),
        body.description,
    ) {
        Ok(tag) => ok_response(serde_json::json!({ "tag": tag })).into_response(),
        Err(thinkingroot_core::Error::BranchAlreadyExists(_)) => err_response(
            StatusCode::CONFLICT,
            "TAG_ALREADY_EXISTS",
            &format!("tag '{}' already exists", body.name),
        ),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "TAG_CREATE_FAILED",
            &e.to_string(),
        ),
    }
}

async fn list_tags_handler(State(state): State<Arc<AppState>>) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            // Empty list rather than error — matches list_branches
            // behaviour and lets unconfigured daemons stay
            // 200-OK-with-empty for monitoring scrapers.
            return ok_response(serde_json::json!({ "tags": Vec::<()>::new() }))
                .into_response();
        }
    };
    match thinkingroot_branch::list_tags(&root) {
        Ok(tags) => ok_response(serde_json::json!({ "tags": tags })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TAG_LIST_FAILED",
            &e.to_string(),
        ),
    }
}

async fn get_tag_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    match thinkingroot_branch::list_tags(&root) {
        Ok(tags) => match tags.into_iter().find(|t| t.name == name) {
            Some(t) => ok_response(serde_json::json!({ "tag": t })).into_response(),
            None => err_response(
                StatusCode::NOT_FOUND,
                "TAG_NOT_FOUND",
                &format!("tag '{name}' not found"),
            ),
        },
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TAG_LOOKUP_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── T3.7: Branch templates ──────────────────────────────────────────
//
// Read/write surface for `branch_templates.toml`.  POST creates or
// overwrites a template by name; GET (collection) lists; GET (item)
// fetches one; DELETE removes one.  Consumers wire `template: "..."`
// on `POST /branches` to materialise the bundled defaults.

async fn list_branch_templates_handler(State(state): State<Arc<AppState>>) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::templates::TemplateRegistry;
    match TemplateRegistry::load_or_seed(&refs_dir) {
        Ok(r) => ok_response(serde_json::json!({ "templates": r.list() })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TEMPLATE_LOAD_FAILED",
            &e.to_string(),
        ),
    }
}

async fn upsert_branch_template_handler(
    State(state): State<Arc<AppState>>,
    Json(template): Json<thinkingroot_branch::templates::BranchTemplate>,
) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::templates::TemplateRegistry;
    let mut registry = match TemplateRegistry::load_or_seed(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "TEMPLATE_LOAD_FAILED",
                &e.to_string(),
            );
        }
    };
    let name = template.name.clone();
    match registry.upsert(template) {
        Ok(existed) => {
            let status_code = if existed {
                "updated"
            } else {
                "created"
            };
            ok_response(serde_json::json!({
                "name": name,
                "status": status_code,
            }))
            .into_response()
        }
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "TEMPLATE_INVALID",
            &e.to_string(),
        ),
    }
}

async fn get_branch_template_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::templates::TemplateRegistry;
    match TemplateRegistry::load_or_seed(&refs_dir) {
        Ok(r) => match r.get(&name) {
            Some(t) => ok_response(serde_json::json!({ "template": t })).into_response(),
            None => err_response(
                StatusCode::NOT_FOUND,
                "TEMPLATE_NOT_FOUND",
                &format!("no branch template named '{name}'"),
            ),
        },
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TEMPLATE_LOAD_FAILED",
            &e.to_string(),
        ),
    }
}

async fn delete_branch_template_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let refs_dir = root.join(".thinkingroot-refs");
    use thinkingroot_branch::templates::TemplateRegistry;
    let mut registry = match TemplateRegistry::load_or_seed(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "TEMPLATE_LOAD_FAILED",
                &e.to_string(),
            );
        }
    };
    match registry.remove(&name) {
        Ok(true) => ok_response(serde_json::json!({ "deleted": name })).into_response(),
        Ok(false) => err_response(
            StatusCode::NOT_FOUND,
            "TEMPLATE_NOT_FOUND",
            &format!("no branch template named '{name}'"),
        ),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "TEMPLATE_DELETE_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── T1.7: Branch lineage DAG ────────────────────────────────────────
//
// Aggregates `(parent, child)` fork edges + `(child, into)` merge
// edges across every branch in the registry.  Consumers (Brain
// surface, dashboards) render this as a DAG; the layout is theirs to
// pick — we just hand back the edge list with timestamps so they can
// time-order siblings.

#[derive(Serialize)]
struct LineageEdge {
    /// `"fork"` or `"merge"`.
    kind: &'static str,
    from: String,
    to: String,
    at: chrono::DateTime<chrono::Utc>,
    /// For merge edges: the proposal id (when the merge was gated).
    #[serde(skip_serializing_if = "Option::is_none")]
    authorising_proposal_id: Option<String>,
}

#[derive(Serialize)]
struct LineageNode {
    name: String,
    status: String,
    kind: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize)]
struct LineageResponse {
    nodes: Vec<LineageNode>,
    edges: Vec<LineageEdge>,
}

async fn branch_lineage_handler(State(state): State<Arc<AppState>>) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    use thinkingroot_branch::branch::BranchRegistry;
    use thinkingroot_core::BranchEvent;

    let refs_dir = root.join(".thinkingroot-refs");
    let registry = match BranchRegistry::load_or_create(&refs_dir) {
        Ok(r) => r,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BRANCH_ERROR",
                &e.to_string(),
            );
        }
    };

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for branch in registry.all() {
        nodes.push(LineageNode {
            name: branch.name.clone(),
            status: branch_status_label(&branch.status),
            kind: kind_label(&branch.kind),
            created_at: branch.created_at,
        });
        // Fork edge: parent → branch, timestamped at the Created event
        // when present (T1.3 wires this on every new branch); falls
        // back to `created_at` when reading a pre-T1.3 registry.
        let forked_at = branch
            .events
            .iter()
            .find_map(|e| match e {
                BranchEvent::Created { at, .. } => Some(*at),
                _ => None,
            })
            .unwrap_or(branch.created_at);
        edges.push(LineageEdge {
            kind: "fork",
            from: branch.parent.clone(),
            to: branch.name.clone(),
            at: forked_at,
            authorising_proposal_id: None,
        });
        // Merge edges: one per Merged event (typically only one, but
        // a branch could in theory be merged-then-reopened in a future
        // workflow; loop covers the general case).
        for ev in &branch.events {
            if let BranchEvent::Merged {
                at,
                into,
                authorising_proposal_id,
                ..
            } = ev
            {
                edges.push(LineageEdge {
                    kind: "merge",
                    from: branch.name.clone(),
                    to: into.clone(),
                    at: *at,
                    authorising_proposal_id: authorising_proposal_id.clone(),
                });
            }
        }
    }

    ok_response(LineageResponse { nodes, edges }).into_response()
}

fn kind_label(kind: &thinkingroot_core::BranchKind) -> String {
    use thinkingroot_core::BranchKind;
    match kind {
        BranchKind::Main => "main".into(),
        BranchKind::Feature => "feature".into(),
        BranchKind::Stream { .. } => "stream".into(),
        BranchKind::Sandbox { .. } => "sandbox".into(),
        BranchKind::Tag { .. } => "tag".into(),
    }
}

// ─── T0.4: Knowledge Proposal handlers ────────────────────────────────
//
// These five routes wire the `thinkingroot-pr` crate (the proposal
// lifecycle layer) into HTTP. A workspace's proposals all live under
// `<workspace>/.thinkingroot-refs/proposals/`; routes that need a
// `refs_dir` derive it from `state.workspace_root` and bail early if
// the daemon was started without `--path`.

#[derive(Deserialize)]
struct OpenProposalRequest {
    /// Optional explicit target branch; `None` (or omitted) means main.
    #[serde(default)]
    target_branch: Option<String>,
    /// Free-form description supplied by the proposing principal.
    #[serde(default)]
    description: Option<String>,
    /// Distinct approving reviewers required. Reads from the source
    /// branch's `MergePolicy::RequiresProposal { min_reviewers }` when
    /// omitted (`None`); falls back to `1` if no policy is set so this
    /// route stays usable for branches that haven't opted into proposal
    /// gating yet.
    #[serde(default)]
    min_reviewers: Option<u8>,
    /// Required-checks list to freeze on the proposal at open time.
    /// When omitted, copied from the source branch's policy if set,
    /// otherwise empty.
    #[serde(default)]
    required_checks: Option<Vec<String>>,
}

async fn refs_dir_from_state(state: &AppState) -> std::result::Result<PathBuf, Response> {
    let root = state.current_workspace_root().await.ok_or_else(|| {
        err_response(
            StatusCode::BAD_REQUEST,
            "NOT_CONFIGURED",
            "workspace_root not set",
        )
    })?;
    Ok(root.join(".thinkingroot-refs"))
}

/// Look up the source branch's `RequiresProposal` policy values so
/// callers don't have to mirror them on every open request. Returns
/// `(min_reviewers, required_checks)`. Defaults to `(1, vec![])` when
/// the branch has any other policy or when it can't be loaded — the
/// proposal still gets created, the merge gate just won't honour it
/// unless the policy is also `RequiresProposal`.
fn proposal_policy_defaults(
    refs_dir: &std::path::Path,
    source_branch: &str,
) -> (u8, Vec<String>) {
    use thinkingroot_branch::branch::BranchRegistry;
    use thinkingroot_core::MergePolicy;
    if let Ok(registry) = BranchRegistry::load_or_create(refs_dir)
        && let Some(branch) = registry.get(source_branch)
        && let MergePolicy::RequiresProposal {
            min_reviewers,
            required_checks,
        } = &branch.merge_policy
    {
        return (*min_reviewers, required_checks.clone());
    }
    (1, Vec::new())
}

async fn open_proposal_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(branch): Path<String>,
    Json(body): Json<OpenProposalRequest>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    // Author principal. Prefer the per-end-user `X-TR-User` header when present;
    // otherwise fall back to a stable machine principal ("sdk") so SDK/agent-driven
    // proposals on a SHARED (main) branch work. The cloud gateway's per-user
    // leak-guard confines ANY `X-TR-User` request to that user's `u_*` namespace,
    // so a proposal targeting a main-workspace branch must NOT depend on the header.
    // Safe for verify-before-merge: the author is excluded from the approval count
    // regardless (verifier-outside-write-scope), so a defaulted author cannot
    // self-approve, and objective `required_checks` still gate the merge.
    let author = request_user(&headers).unwrap_or_else(|| "sdk".to_string());

    let (default_min, default_checks) =
        proposal_policy_defaults(&refs_dir, &branch);
    let min_reviewers = body.min_reviewers.unwrap_or(default_min);
    let required_checks = body.required_checks.unwrap_or(default_checks);

    match thinkingroot_pr::open_proposal(
        &refs_dir,
        &branch,
        body.target_branch.as_deref(),
        &author,
        body.description,
        min_reviewers,
        required_checks,
    ) {
        Ok(p) => {
            // M3 — run the proposal's required checks immediately so it can
            // reach `Approved` without a manual run-checks call. Best-effort:
            // a check-run error leaves the proposal Open (gated), not failed-open.
            let proposal = if let Some(root) = state.current_workspace_root().await {
                let engine = state.engine.read().await;
                engine.run_proposal_checks(&root, &p.id).await.unwrap_or(p)
            } else {
                p
            };
            ok_response(serde_json::json!({ "proposal": proposal })).into_response()
        }
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "PROPOSAL_OPEN_FAILED",
            &e.to_string(),
        ),
    }
}

/// `POST /api/v1/proposals/{id}/run-checks` — run the proposal's required
/// checks daemon-side and record each result. Returns the updated proposal;
/// `status` advances to `Approved` once all required checks pass (and the
/// reviewer count is met). This is the wiring that makes `required_checks`
/// a real merge gate.
async fn run_checks_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let engine = state.engine.read().await;
    match engine.run_proposal_checks(&root, &id).await {
        Ok(p) => ok_response(serde_json::json!({ "proposal": p })).into_response(),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "RUN_CHECKS_FAILED",
            &e.to_string(),
        ),
    }
}

/// `POST /api/v1/consolidate` — #2 — run a promotion consolidation pass: mine
/// quorum'd, de-identified patterns from this project's per-user brains and
/// stage them for verify-before-merge promotion into the shared brain. The
/// request body is a [`crate::consolidation::ConsolidationSpec`] (all fields
/// optional → conservative defaults). This is a privileged op: the cloud only
/// reaches it for projects with `promotion_enabled`, and it ranges solely over
/// this daemon's own `u_*` workspaces (one daemon = one project).
async fn consolidate_handler(
    State(state): State<Arc<AppState>>,
    body: Option<Json<crate::consolidation::ConsolidationSpec>>,
) -> Response {
    let spec = body
        .map(|Json(s)| s)
        .unwrap_or_default();
    let engine = state.engine.read().await;
    match engine.consolidate_to_shared(spec, &state.sessions).await {
        Ok(report) => ok_response(serde_json::json!({ "report": report })).into_response(),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "CONSOLIDATE_FAILED",
            &e.to_string(),
        ),
    }
}

async fn list_branch_proposals_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    match thinkingroot_pr::list_proposals(&refs_dir) {
        Ok(all) => {
            let filtered: Vec<_> = all
                .into_iter()
                .filter(|p| p.source_branch == branch)
                .collect();
            ok_response(serde_json::json!({ "proposals": filtered })).into_response()
        }
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "PROPOSAL_LIST_FAILED",
            &e.to_string(),
        ),
    }
}

async fn list_all_proposals_handler(State(state): State<Arc<AppState>>) -> Response {
    let refs_dir = match refs_dir_from_state(&state).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    match thinkingroot_pr::list_proposals(&refs_dir) {
        Ok(all) => ok_response(serde_json::json!({ "proposals": all })).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "PROPOSAL_LIST_FAILED",
            &e.to_string(),
        ),
    }
}

async fn get_proposal_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    match thinkingroot_pr::read_proposal(&refs_dir, &id) {
        Ok(Some(p)) => ok_response(serde_json::json!({ "proposal": p })).into_response(),
        Ok(None) => err_response(
            StatusCode::NOT_FOUND,
            "PROPOSAL_NOT_FOUND",
            &format!("proposal `{id}` not found"),
        ),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "PROPOSAL_READ_FAILED",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct ReviewProposalRequest {
    /// `"approve"`, `"request_changes"`, or `"comment"`.
    decision: String,
    #[serde(default)]
    comment: Option<String>,
}

async fn review_proposal_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ReviewProposalRequest>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let reviewer = match request_user(&headers) {
        Some(u) => u,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "MISSING_PRINCIPAL",
                "X-TR-User header is required to review a proposal",
            );
        }
    };
    let decision = match body.decision.to_ascii_lowercase().as_str() {
        "approve" => thinkingroot_pr::ReviewDecision::Approve,
        "request_changes" | "request-changes" | "changes_requested" => {
            thinkingroot_pr::ReviewDecision::RequestChanges
        }
        "comment" => thinkingroot_pr::ReviewDecision::Comment,
        other => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "BAD_DECISION",
                &format!(
                    "decision must be one of approve|request_changes|comment, got `{other}`"
                ),
            );
        }
    };
    match thinkingroot_pr::review_proposal(&refs_dir, &id, &reviewer, decision, body.comment) {
        Ok(p) => ok_response(serde_json::json!({ "proposal": p })).into_response(),
        Err(e) => err_response(
            StatusCode::BAD_REQUEST,
            "PROPOSAL_REVIEW_FAILED",
            &e.to_string(),
        ),
    }
}

async fn close_proposal_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let refs_dir = match refs_dir_from_state(&state).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let closer = match request_user(&headers) {
        Some(u) => u,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "MISSING_PRINCIPAL",
                "X-TR-User header is required to close a proposal",
            );
        }
    };
    match thinkingroot_pr::close_proposal(&refs_dir, &id, &closer) {
        Ok(p) => ok_response(serde_json::json!({ "proposal": p })).into_response(),
        Err(e) => err_response(
            StatusCode::FORBIDDEN,
            "PROPOSAL_CLOSE_FAILED",
            &e.to_string(),
        ),
    }
}

async fn delete_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    let engine = state.engine.read().await;
    let actor = request_user(&headers)
        .map(crate::engine::BranchActor::User)
        .unwrap_or(crate::engine::BranchActor::System);
    match engine.delete_branch_as(&root, &branch, actor).await {
        Ok(_) => {
            // T1.6 — `delete_branch_as` calls `abandon_branch` which
            // appended an `Abandoned` event; broadcast it before
            // dropping the engine read-lock.
            drop(engine);
            publish_latest_branch_event(&state, &branch).await;
            ok_response(serde_json::json!({ "deleted": branch })).into_response()
        }
        Err(e) => err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &e.to_string()),
    }
}

async fn checkout_branch_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    match thinkingroot_branch::write_head_branch(&root, &branch) {
        Ok(_) => {
            let _ = state.head_change_tx.send(branch.clone());
            ok_response(serde_json::json!({ "head": branch })).into_response()
        }
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BRANCH_ERROR",
            &e.to_string(),
        ),
    }
}

async fn diff_branch_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    use thinkingroot_branch::diff::compute_diff;
    use thinkingroot_branch::snapshot::resolve_data_dir;
    use thinkingroot_core::config::Config;
    use thinkingroot_graph::graph::GraphStore;

    let config = match Config::load_merged(&root) {
        Ok(c) => c,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONFIG_ERROR",
                &e.to_string(),
            );
        }
    };
    let mc = &config.merge;
    let main_data_dir = resolve_data_dir(&root, None);
    let branch_data_dir = resolve_data_dir(&root, Some(&branch));

    if !branch_data_dir.exists() {
        return err_response(
            StatusCode::NOT_FOUND,
            "BRANCH_NOT_FOUND",
            &format!("branch '{}' not found", branch),
        );
    }

    let main_graph = match GraphStore::init(&main_data_dir.join("graph")) {
        Ok(g) => g,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &e.to_string(),
            );
        }
    };
    let branch_graph = match GraphStore::init(&branch_data_dir.join("graph")) {
        Ok(g) => g,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRAPH_ERROR",
                &e.to_string(),
            );
        }
    };

    match compute_diff(
        &main_graph,
        &branch_graph,
        &branch,
        mc.auto_resolve_threshold,
        mc.max_health_drop,
        mc.block_on_contradictions,
    ) {
        Ok(diff) => ok_response(diff).into_response(),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DIFF_ERROR",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct MergeBranchRequest {
    force: Option<bool>,
    propagate_deletions: Option<bool>,
}

#[derive(Deserialize, Default)]
struct MergeQuery {
    /// T1.5 — when true, compute the diff that would land in target
    /// without mutating anything.  Returns the same shape as the
    /// committing path plus the diff body so callers can preview.
    #[serde(default)]
    dry_run: bool,
}

async fn merge_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(branch): Path<String>,
    Query(query): Query<MergeQuery>,
    body: Option<Json<MergeBranchRequest>>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    use thinkingroot_core::MergedBy;

    let force = body.as_ref().and_then(|b| b.force).unwrap_or(false);
    let propagate_deletions = body
        .as_ref()
        .and_then(|b| b.propagate_deletions)
        .unwrap_or(false);

    // T1.5 — dry-run path.  `dry_run_merge_into` runs the same
    // diff-computation chain (two-way + vector pass, three-way when
    // an LCA is present) the committing merge would, but never
    // touches the target graph or the registry.  Default `target`
    // matches the committing path's `None → "main"`.
    if query.dry_run {
        match thinkingroot_branch::dry_run_merge_into(&root, &branch, "main", force).await {
            Ok(diff) => return ok_response(serde_json::json!({
                "dry_run": true,
                "diff": diff,
                "merge_allowed": diff.merge_allowed,
                "blocking_reasons": diff.blocking_reasons,
                "new_claims": diff.new_claims.len(),
                "new_entities": diff.new_entities.len(),
                "auto_resolved": diff.auto_resolved.len(),
                "needs_review": diff.needs_review.len(),
            }))
            .into_response(),
            Err(e) => {
                return err_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "DRY_RUN_FAILED",
                    &e.to_string(),
                );
            }
        }
    }

    // T1.5 — register a CancellationToken so `POST /merges/{id}/cancel`
    // can trip it.  The id is returned to the caller in the success
    // response and surfaced in error responses too so a hung merge can
    // be aborted by an out-of-band client.
    let merge_id = format!(
        "merge_{}",
        chrono::Utc::now().timestamp_micros().to_string()
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    state
        .active_merges
        .write()
        .await
        .insert(merge_id.clone(), cancel.clone());

    let engine = state.engine.read().await;
    let result = engine
        .merge_into_branch_cancellable(
            &root,
            &branch,
            None,
            force,
            propagate_deletions,
            MergedBy::Human {
                user: request_user(&headers).unwrap_or_else(|| "api".to_string()),
            },
            Some(cancel.clone()),
        )
        .await;
    drop(engine);

    // Always remove the token so the map doesn't grow unbounded — we
    // do this before deciding response shape so a slow publish below
    // can't leak the entry.
    state.active_merges.write().await.remove(&merge_id);

    match result {
        Ok(diff) => {
            // merge_branch_handler always targets `main` (the default
            // when `target = None` upstream), so this branch always
            // mutates main's graph — invalidate engrams unconditionally.
            invalidate_engrams_for_root(&state, &root).await;
            publish_latest_branch_event(&state, &branch).await;
            ok_response(serde_json::json!({
                "merged": branch,
                "merge_id": merge_id,
                "new_claims": diff.new_claims.len(),
                "new_entities": diff.new_entities.len(),
                "auto_resolved": diff.auto_resolved.len(),
            }))
            .into_response()
        }
        Err(thinkingroot_core::Error::EntityNotFound(msg)) => {
            err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &msg)
        }
        Err(e) if e.is_cancelled() => err_response(
            StatusCode::GONE,
            "MERGE_CANCELLED",
            &format!("merge {merge_id} cancelled before completion"),
        ),
        Err(e) => err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "MERGE_BLOCKED",
            &e.to_string(),
        ),
    }
}

async fn merge_into_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((source, target)): Path<(String, String)>,
    Query(query): Query<MergeQuery>,
    body: Option<Json<MergeBranchRequest>>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };
    use thinkingroot_core::MergedBy;

    let force = body.as_ref().and_then(|b| b.force).unwrap_or(false);
    let propagate_deletions = body
        .as_ref()
        .and_then(|b| b.propagate_deletions)
        .unwrap_or(false);

    // T1.5 — dry-run path mirrors `merge_branch_handler`.
    if query.dry_run {
        match thinkingroot_branch::dry_run_merge_into(&root, &source, &target, force).await {
            Ok(diff) => return ok_response(serde_json::json!({
                "dry_run": true,
                "source": source,
                "target": target,
                "diff": diff,
                "merge_allowed": diff.merge_allowed,
                "blocking_reasons": diff.blocking_reasons,
                "new_claims": diff.new_claims.len(),
                "new_entities": diff.new_entities.len(),
                "auto_resolved": diff.auto_resolved.len(),
                "needs_review": diff.needs_review.len(),
            }))
            .into_response(),
            Err(e) => {
                return err_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "DRY_RUN_FAILED",
                    &e.to_string(),
                );
            }
        }
    }

    let merge_id = format!(
        "merge_{}",
        chrono::Utc::now().timestamp_micros().to_string()
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    state
        .active_merges
        .write()
        .await
        .insert(merge_id.clone(), cancel.clone());

    let engine = state.engine.read().await;
    let result = engine
        .merge_into_branch_cancellable(
            &root,
            &source,
            Some(&target),
            force,
            propagate_deletions,
            MergedBy::Human {
                user: request_user(&headers).unwrap_or_else(|| "api".to_string()),
            },
            Some(cancel.clone()),
        )
        .await;
    drop(engine);
    state.active_merges.write().await.remove(&merge_id);

    match result {
        Ok(diff) => {
            // Cross-branch merges that don't touch main don't change
            // what BrainView reads, but per-workspace engrams stay
            // honest only when invalidated on every main-touching
            // mutation. So gate on target == "main" here.
            if target == "main" {
                invalidate_engrams_for_root(&state, &root).await;
            }
            publish_latest_branch_event(&state, &source).await;
            ok_response(serde_json::json!({
                "merged": source,
                "target": target,
                "merge_id": merge_id,
                "new_claims": diff.new_claims.len(),
                "new_entities": diff.new_entities.len(),
                "auto_resolved": diff.auto_resolved.len(),
            }))
            .into_response()
        }
        Err(thinkingroot_core::Error::EntityNotFound(msg)) => {
            err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &msg)
        }
        Err(e) if e.is_cancelled() => err_response(
            StatusCode::GONE,
            "MERGE_CANCELLED",
            &format!("merge {merge_id} cancelled before completion"),
        ),
        Err(e) => err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "MERGE_BLOCKED",
            &e.to_string(),
        ),
    }
}

// ─── T1.5 — cancel an in-flight merge by id ──────────────────────────

async fn cancel_merge_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if let Some(token) = state.active_merges.read().await.get(&id).cloned() {
        token.cancel();
        ok_response(serde_json::json!({ "cancelled": id })).into_response()
    } else {
        err_response(
            StatusCode::NOT_FOUND,
            "MERGE_NOT_ACTIVE",
            &format!("no in-flight merge with id '{id}' (already finished or never started)"),
        )
    }
}

async fn rebase_branch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };

    let actor = request_user(&headers)
        .map(crate::engine::BranchActor::User)
        .unwrap_or(crate::engine::BranchActor::System);
    let engine = state.engine.read().await;
    match engine.rebase_branch(&root, &branch, actor).await {
        Ok(diff) => ok_response(serde_json::json!({
            "rebased": branch,
            "from_branch": diff.from_branch,
            "new_claims": diff.new_claims.len(),
            "new_entities": diff.new_entities.len(),
            "auto_resolved": diff.auto_resolved.len(),
        }))
        .into_response(),
        Err(thinkingroot_core::Error::EntityNotFound(msg)) => {
            err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &msg)
        }
        Err(e) => err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "REBASE_BLOCKED",
            &e.to_string(),
        ),
    }
}

async fn rollback_merge_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match state.current_workspace_root().await {
        Some(r) => r,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "NOT_CONFIGURED",
                "workspace_root not set",
            );
        }
    };

    let engine = state.engine.read().await;
    match engine.rollback_merge(&root, &branch).await {
        Ok(()) => ok_response(serde_json::json!({
            "rolled_back": branch,
        }))
        .into_response(),
        Err(thinkingroot_core::Error::EntityNotFound(msg)) => {
            err_response(StatusCode::NOT_FOUND, "BRANCH_NOT_FOUND", &msg)
        }
        Err(e) => err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ROLLBACK_FAILED",
            &e.to_string(),
        ),
    }
}

// ─── Intelligence Ask Endpoint ───────────────────────────────

/// POST /api/v1/ws/{workspace}/ask
///
/// Runs the full hybrid retrieval + synthesis pipeline proven at 91.2% on
/// LongMemEval-500. Returns a synthesized natural-language answer with source
/// attribution.
///
/// Body:
/// ```json
/// {
///   "question": "What time did I reach the clinic on Monday?",
///   "session_scope": ["session_001", "session_002"],  // optional
///   "question_date": "2023/05/30 (Tue) 22:10",        // optional, for temporal
///   "category_hint": "temporal-reasoning"              // optional
/// }
/// ```

#[derive(Deserialize)]
struct AskRequest {
    question: String,
    #[serde(default)]
    session_scope: Vec<String>,
    #[serde(default)]
    question_date: String,
    #[serde(default)]
    category_hint: String,
    /// Recent conversation turns (oldest-first) the synthesizer should
    /// treat as memory. Empty = single-shot mode and the wire prompt is
    /// byte-identical to v0.9.0. The desktop chat surface pins the last
    /// 6-8 turns here once Sprint S5 wires it through; the LongMemEval
    /// bench harness leaves it empty so the contract holds.
    #[serde(default)]
    history: Vec<ChatTurnPayload>,
    /// When `true`, route the chat through the multi-turn tool-using
    /// agent (S3) instead of one-shot retrieval-and-synthesise. Only
    /// honoured by `/ask/stream` and only when the workspace has a
    /// `Conversational` persona resolved. Defaults to `false` so
    /// existing CLI / API clients keep their byte-stable behaviour;
    /// the desktop chat surface flips it to `true` once the UI is
    /// wired to render `tool_call_*` SSE events.
    #[serde(default)]
    use_agent: bool,
    /// Stable identifier for this conversation. Used by the agent
    /// path as the MCP session id (which scopes
    /// `contribute_claim`'s active branch and provenance). When
    /// missing, the streaming handler synthesises a fresh UUID per
    /// request, which means each turn looks like a brand-new
    /// session — fine for stateless flows, breaks per-conversation
    /// active-branch tracking, so callers that want continuity
    /// must pass this.
    #[serde(default)]
    conversation_id: Option<String>,
}

/// Wire-format conversation turn. Mirrors the OpenAI Chat Completions /
/// Anthropic Messages role string so the JSON travels through any
/// front-end without translation. Unknown roles (i.e. `tool`, `system`)
/// are silently dropped — the synthesizer is a strict 2-role consumer.
#[derive(Deserialize)]
struct ChatTurnPayload {
    role: String,
    content: String,
}

/// Translate the wire-format `[{role, content}, ...]` history into the
/// synthesizer's internal `Vec<ChatTurn>`. Unknown roles are skipped
/// (rather than failing the request) so a misbehaving client cannot
/// take down the chat surface — the worst case is the synthesizer sees
/// fewer turns than the client thought it sent. Empty `content` strings
/// are also dropped to keep the prompt tight.
fn decode_history(payload: &[ChatTurnPayload]) -> Vec<crate::intelligence::synthesizer::ChatTurn> {
    use crate::intelligence::synthesizer::{ChatRole, ChatTurn};
    payload
        .iter()
        .filter_map(|t| {
            let role = match t.role.as_str() {
                "user" => ChatRole::User,
                "assistant" => ChatRole::Assistant,
                _ => return None,
            };
            let content = t.content.trim();
            if content.is_empty() {
                return None;
            }
            Some(ChatTurn {
                role,
                content: content.to_string(),
            })
        })
        .collect()
}

#[derive(Serialize, Deserialize, Clone)]
struct AskResponseBody {
    answer: String,
    claims_used: usize,
    category: String,
    /// Verified, byte-anchored citations (mechanical citation gate).
    citations: Vec<crate::intelligence::citations::Citation>,
    /// Fraction of emitted `[claim:<id>]` markers that verified, in [0,1].
    answer_confidence: f32,
    /// True when the answer was refused (all citations were fabricated).
    refused: bool,
    /// §5 output stack #1 — output tokens the model did NOT generate because
    /// the engine hydrated `[claim:id]` markers from the graph. 0 when
    /// hydration is off or nothing was rendered. Feeds the Savings Meter.
    #[serde(default)]
    hydrated_output_tokens_saved: usize,
    /// §5 input #4 — true when this whole answer was served from the
    /// provenance-aware answer cache (no LLM call). Feeds the Savings Meter.
    #[serde(default)]
    from_cache: bool,
}

/// §3 #6 retry flag — give a refused answer one more synthesis pass before
/// abstaining. OFF until eval measures answered-vs-refused; read per request.
fn answer_retry_on() -> bool {
    std::env::var("TR_ANSWER_RETRY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// §5 output-stack #1 flag — render `[claim:id]` skeletons into verbatim cited
/// prose engine-side. OFF by default; pairs with a skeleton-emitting prompt.
fn hydrate_answers_on() -> bool {
    std::env::var("TR_HYDRATE_ANSWERS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// §5 input #4 flag — serve repeated questions from the provenance-aware
/// answer cache. OFF by default (it changes answered behaviour + carries a
/// bounded staleness window). `TR_ANSWER_CACHE_TTL_SECS` (default 3600) caps
/// how old a cached answer may be before it's treated as a miss.
fn answer_cache_on() -> bool {
    std::env::var("TR_ANSWER_CACHE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}
fn answer_cache_ttl_secs() -> f64 {
    std::env::var("TR_ANSWER_CACHE_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|t| *t > 0.0)
        .unwrap_or(3600.0)
}

async fn ask_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<AskRequest>,
) -> Response {
    use crate::intelligence::identity::build_workspace_identity;
    use crate::intelligence::synthesizer::{AskRequest as SynthAskRequest, ask};
    use std::collections::HashMap;
    use std::collections::HashSet;

    // ── Cortex Protocol cancellation contract ──────────────────────
    // The ask path is single-task today: dropping the response
    // future also drops the LLM synthesis call inside `ask()`, so
    // client disconnect IS cancellation. The explicit
    // CancellationToken + DropGuard pair below documents the
    // invariant and provides a hookpoint for the day `ask()` grows
    // its own `Option<CancellationToken>` argument (mirroring
    // `hybrid_retrieve`). Until then it is a no-op guard — the
    // important property is that the pattern matches every other
    // stateful endpoint so a future audit can grep for it
    // uniformly.
    let _cancel = tokio_util::sync::CancellationToken::new();
    let _drop_guard = _cancel.clone().drop_guard();

    let engine = state.engine.read().await;

    // Resolve workspace root for sessions directory.
    // Prefer AppState.workspace_root (set by --path), fall back to engine's per-workspace root.
    let sessions_dir = state
        .current_workspace_root()
        .await
        .or_else(|| engine.workspace_root_path(&ws))
        .map(|p| p.join("sessions"))
        .unwrap_or_else(|| std::path::PathBuf::from("sessions"));

    // If no session_scope provided, use an empty set (no scoping — all claims allowed)
    let allowed_sources: HashSet<String> = body.session_scope.iter().cloned().collect();

    // Infer category from hint or router
    let category = if !body.category_hint.is_empty() {
        body.category_hint.clone()
    } else {
        // Use the query router to infer category
        let tmp_session = crate::intelligence::session::SessionContext::new("_ask", &ws);
        match crate::intelligence::router::classify_query(&body.question, &tmp_session) {
            crate::intelligence::router::QueryPath::Agentic => {
                let q = body.question.to_lowercase();
                if q.contains("when")
                    || q.contains(" ago")
                    || q.contains("last ")
                    || q.contains("how many days")
                {
                    "temporal-reasoning".to_string()
                } else {
                    "multi-session".to_string()
                }
            }
            crate::intelligence::router::QueryPath::Fast => "single-session-user".to_string(),
        }
    };

    // Retrieve the LLM client from the engine's workspace config
    let llm = engine.workspace_llm(&ws);

    // Workspace identity / persona — the chat-time prompt structure that
    // anchors the model to *this* workspace. Falls back to the
    // Memory/Terse default (identity=None) when the workspace isn't
    // mounted, preserving the v0.9.0 LongMemEval-91.2% wire prompt
    // for tests / harnesses.
    let snapshot = engine.workspace_chat_snapshot(&ws).await;
    let chat = snapshot
        .as_ref()
        .map(|s| s.config.chat.resolve(&s.source_kinds))
        .unwrap_or_else(SynthAskRequest::default_chat);
    let identity_owned = snapshot
        .as_ref()
        .map(|s| build_workspace_identity(s, &s.config.chat));
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let history = decode_history(&body.history);

    let req = SynthAskRequest {
        workspace: &ws,
        question: &body.question,
        category: &category,
        allowed_sources: &allowed_sources,
        question_date: &body.question_date,
        session_dates: &HashMap::new(),
        answer_sids: &body.session_scope,
        sessions_dir: &sessions_dir,
        excluded_claim_ids: &HashSet::new(),
        chat,
        identity: identity_owned.as_ref(),
        today: Some(&today),
        history: &history,
    };

    // ── §5 input #4 — provenance-aware answer cache (read) ──────────
    // Serve a repeated question from cache when fresh within TTL. Causal
    // invalidation (claim supersession/removal evicts the answer) means a hit
    // can only be stale within the TTL window, never built on a changed fact.
    let answer_key =
        thinkingroot_graph::answer_cache::answer_cache_key(None, &body.question);
    if answer_cache_on() {
        if let Some(graph) = engine.graph_store(&ws).await {
            if let Ok(Some(row)) = graph.answer_cache_get(&answer_key) {
                let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
                if now - row.created_at <= answer_cache_ttl_secs() {
                    if let Ok(mut cached) =
                        serde_json::from_str::<AskResponseBody>(&row.answer_json)
                    {
                        cached.from_cache = true;
                        tracing::debug!("answer cache hit");
                        return ok_response(cached).into_response();
                    }
                }
            }
        }
    }

    // Keep a handle for the §3 #6 retry pass (Arc clone is cheap).
    let llm_retry = llm.clone();
    let mut result = ask(&engine, llm, &req).await;

    // ── Mechanical citation gate ────────────────────────────────────
    // Verify every `[claim:<id>]` marker against the grounding set the
    // model was actually shown; strip fabricated provenance; refuse the
    // answer outright if EVERY citation was fabricated. Inert (no-op)
    // when the answer carries no markers — preserving the Memory /
    // LongMemEval persona wire behaviour.
    let mut gate =
        crate::intelligence::citations::verify_citations(&engine, &ws, &result.answer, &result.grounding)
            .await;

    // ── §3 #6 — verification + abstention with one retry ─────────────
    // "verified or silent." When the first synthesis failed the gate (every
    // citation fabricated) but we DID retrieve grounding and have an LLM, give
    // the model exactly ONE more pass before abstaining — a re-synthesis over
    // the same grounding usually cites correctly. Keep the retry only if it
    // actually verifies; otherwise we fall through to the honest non-answer.
    // Gated TR_ANSWER_RETRY (default OFF) so the LongMemEval library path is
    // untouched.
    if gate.refused
        && answer_retry_on()
        && llm_retry.is_some()
        && !result.grounding.is_empty()
    {
        let retry = ask(&engine, llm_retry, &req).await;
        let retry_gate = crate::intelligence::citations::verify_citations(
            &engine,
            &ws,
            &retry.answer,
            &retry.grounding,
        )
        .await;
        if !retry_gate.refused {
            tracing::info!("answer retry recovered a verified answer after first-pass refusal");
            result = retry;
            gate = retry_gate;
        }
    }

    // ── Usage signal (NOW item 4) ───────────────────────────────────
    // Persist shown-vs-cited per grounding hit — the citation gate
    // computed this and previously discarded it. Feeds the per-project
    // learn-to-rank trainer + restated-fraction metric. Non-fatal:
    // losing a training row must never fail an answer.
    if !result.grounding.is_empty() {
        let cited_ids: HashSet<&str> =
            gate.citations.iter().map(|c| c.claim_id.as_str()).collect();
        let hits: Vec<(String, usize, f32, bool)> = result
            .grounding
            .iter()
            .enumerate()
            .map(|(rank, h)| {
                (h.id.clone(), rank, h.relevance, cited_ids.contains(h.id.as_str()))
            })
            .collect();
        if let Some(graph) = engine.graph_store(&ws).await {
            let session = body.conversation_id.clone().unwrap_or_default();
            if let Err(e) =
                graph.record_retrieval_usage(&session, &body.question, gate.refused, &hits)
            {
                tracing::warn!("usage-signal record failed (non-fatal): {e}");
            }
            // Living Engram (Build 1) — reinforce associative edges from this
            // verified-good answer's co-citations (same gate as the streaming
            // path). TR_LIVING_EDGES; NOT refused; ≥2 distinct cited claims.
            let living_edges = std::env::var("TR_LIVING_EDGES")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            if living_edges && !gate.refused {
                let mut cited: Vec<String> = cited_ids.iter().map(|s| s.to_string()).collect();
                cited.sort();
                cited.dedup();
                if cited.len() >= 2 {
                    let mut pairs: Vec<(String, String)> = Vec::new();
                    for i in 0..cited.len() {
                        for j in (i + 1)..cited.len() {
                            pairs.push((cited[i].clone(), cited[j].clone()));
                        }
                    }
                    let q = {
                        let c = gate.answer_confidence as f64;
                        if c > 0.0 { c.min(1.0) } else { 1.0 }
                    };
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    match graph.record_co_citation(&pairs, q, ts) {
                        Ok(n) => {
                            tracing::info!(pairs = n, "living-edges: reinforced co-citation edges")
                        }
                        Err(e) => {
                            tracing::warn!("living-edges record failed (non-fatal): {e}")
                        }
                    }
                }
            }
        }
    }

    let category = result.category.clone();
    let (mut answer, claims_used) = if gate.refused {
        (
            "I don't have enough verified information to answer that.".to_string(),
            0,
        )
    } else {
        (result.answer, result.claims_used)
    };

    // ── §5 output stack #1 — Hydrated Answers ───────────────────────
    // Render `[claim:id]` skeleton markers into verbatim cited prose from the
    // grounding set (text the model didn't have to generate). Double-render-
    // safe (a statement already present is never re-inserted), so it is inert
    // on a normal verbose answer. Gated TR_HYDRATE_ANSWERS.
    let mut hydrated_output_tokens_saved = 0usize;
    if !gate.refused && hydrate_answers_on() && !result.grounding.is_empty() {
        let pairs: Vec<(String, String)> = result
            .grounding
            .iter()
            .map(|h| (h.id.clone(), h.statement.clone()))
            .collect();
        let h = crate::intelligence::hydration::hydrate_answer(&answer, &pairs);
        answer = h.text;
        hydrated_output_tokens_saved = h.output_tokens_saved;
    }

    state
        .publish_activity(
            crate::activity::ActivityEvent::new(
                ws.clone(),
                crate::activity::ActivityClass::Retrieval,
                "ask.grounded",
                format!(
                    "\"{}\" → {} claims, {} cited",
                    crate::activity::truncate(&body.question, 48),
                    claims_used,
                    gate.citations.len()
                ),
            )
            .with_detail(serde_json::json!({
                "claims": claims_used,
                "category": category,
                "cited": gate.citations.len(),
                "stripped": gate.stripped.len(),
                "refused": gate.refused,
            })),
        )
        .await;

    let resp_body = AskResponseBody {
        answer,
        claims_used,
        category,
        citations: gate.citations,
        answer_confidence: gate.answer_confidence,
        refused: gate.refused,
        hydrated_output_tokens_saved,
        from_cache: false,
    };

    // ── §5 input #4 — answer cache (write) ──────────────────────────
    // Store verified answers keyed by the question, with the grounding claim
    // ids as the provenance set so a later claim change evicts exactly this
    // answer. Never cache a refusal. Best-effort: a cache write must not fail
    // the response.
    if answer_cache_on() && !resp_body.refused && !result.grounding.is_empty() {
        if let Some(graph) = engine.graph_store(&ws).await {
            if let Ok(json) = serde_json::to_string(&resp_body) {
                let deps: Vec<(String, String)> = result
                    .grounding
                    .iter()
                    .map(|h| (h.id.clone(), "claim".to_string()))
                    .collect();
                let row = thinkingroot_graph::answer_cache::AnswerCacheRow {
                    key: answer_key,
                    answer_json: json,
                    query: body.question.clone(),
                    branch: String::new(),
                    created_at: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
                };
                if let Err(e) = graph.answer_cache_put(&row, &deps) {
                    tracing::warn!(error = %e, "answer cache write failed (non-fatal)");
                }
            }
        }
    }

    ok_response(resp_body).into_response()
}

// ─── Streaming Ask (SSE) ─────────────────────────────────────

/// POST /api/v1/ws/{workspace}/ask/stream
///
/// Server-Sent-Events variant of `/ask`. Same retrieval pipeline,
/// same prompt — but the LLM call goes through `chat_stream` and
/// chunks are forwarded incrementally so the desktop chat surface
/// renders tokens as they arrive instead of after the full
/// synthesis finishes.
///
/// Event sequence on the wire (all `data:` is JSON):
///
/// ```text
/// event: meta
/// data: {"claims_used":12,"category":"single-session-user"}
///
/// event: token
/// data: {"text":"The"}
///
/// event: token
/// data: {"text":" answer"}
///
/// event: final
/// data: {"claims_used":12,"category":"single-session-user","truncated":false}
///
/// event: error
/// data: {"message":"connect: ..."}     # only on failure
/// ```
///
/// Static branch (no claims OR no LLM): emits one `meta` event
/// then a single `token` carrying the full fallback text plus a
/// `final` — so the desktop never has to special-case "static
/// vs streamed" on its end.
async fn ask_stream_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
    Json(body): Json<AskRequest>,
) -> impl IntoResponse {
    use crate::intelligence::identity::build_workspace_identity;
    use crate::intelligence::synthesizer::{
        AskRequest as SynthAskRequest, StreamingAnswer, ask_streaming,
    };
    use futures::StreamExt;
    use std::collections::{HashMap, HashSet};

    // Agent path branches off as early as possible: it has its own
    // event stream (tool_call_* + token + final + error) and reuses
    // none of the one-shot retrieval scaffolding below.
    if body.use_agent {
        return agent_stream_response(state.clone(), ws, body).await;
    }

    let engine = state.engine.read().await;

    let sessions_dir = state
        .current_workspace_root()
        .await
        .or_else(|| engine.workspace_root_path(&ws))
        .map(|p| p.join("sessions"))
        .unwrap_or_else(|| std::path::PathBuf::from("sessions"));

    let allowed_sources: HashSet<String> = body.session_scope.iter().cloned().collect();

    let category = if !body.category_hint.is_empty() {
        body.category_hint.clone()
    } else {
        let tmp_session = crate::intelligence::session::SessionContext::new("_ask", &ws);
        match crate::intelligence::router::classify_query(&body.question, &tmp_session) {
            crate::intelligence::router::QueryPath::Agentic => {
                let q = body.question.to_lowercase();
                if q.contains("when")
                    || q.contains(" ago")
                    || q.contains("last ")
                    || q.contains("how many days")
                {
                    "temporal-reasoning".to_string()
                } else {
                    "multi-session".to_string()
                }
            }
            crate::intelligence::router::QueryPath::Fast => "single-session-user".to_string(),
        }
    };

    let llm = engine.workspace_llm(&ws);
    let answer_sids = body.session_scope.clone();

    let snapshot = engine.workspace_chat_snapshot(&ws).await;
    let chat = snapshot
        .as_ref()
        .map(|s| s.config.chat.resolve(&s.source_kinds))
        .unwrap_or_else(SynthAskRequest::default_chat);
    let identity_owned = snapshot
        .as_ref()
        .map(|s| build_workspace_identity(s, &s.config.chat));
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let history = decode_history(&body.history);

    let req = SynthAskRequest {
        workspace: &ws,
        question: &body.question,
        category: &category,
        allowed_sources: &allowed_sources,
        question_date: &body.question_date,
        session_dates: &HashMap::new(),
        answer_sids: &answer_sids,
        sessions_dir: &sessions_dir,
        excluded_claim_ids: &HashSet::new(),
        chat,
        identity: identity_owned.as_ref(),
        today: Some(&today),
        history: &history,
    };

    let outcome = ask_streaming(&engine, llm, &req).await;
    drop(engine);

    // Move-captured by the SSE generator for usage-signal logging after the
    // citation gate runs (the engine read-guard above is already released;
    // the generator re-acquires it briefly post-stream).
    let usage_state = state.clone();
    let usage_ws = ws.clone();
    let usage_question = body.question.clone();
    let usage_session = body.conversation_id.clone().unwrap_or_default();

    let stream = async_stream::stream! {
        match outcome {
            StreamingAnswer::Static { answer, claims_used, category } => {
                let meta = serde_json::json!({
                    "claims_used": claims_used,
                    "category": category,
                });
                yield Ok::<Event, std::convert::Infallible>(
                    Event::default().event("meta").data(meta.to_string())
                );
                if !answer.is_empty() {
                    let payload = serde_json::json!({ "text": answer });
                    yield Ok(
                        Event::default().event("token").data(payload.to_string())
                    );
                }
                let final_payload = serde_json::json!({
                    "claims_used": claims_used,
                    "category": category,
                    "truncated": false,
                });
                yield Ok(
                    Event::default().event("final").data(final_payload.to_string())
                );
            }
            StreamingAnswer::Stream { mut stream, claims_used, category, grounding } => {
                let meta = serde_json::json!({
                    "claims_used": claims_used,
                    "category": category,
                });
                yield Ok(
                    Event::default().event("meta").data(meta.to_string())
                );
                let mut truncated = false;
                // Accumulate the streamed answer so the citation gate can
                // verify its `[claim:<id>]` markers once the full text is
                // assembled (markers only resolve over the complete reply).
                let mut answer_acc = String::new();
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(chunk) => {
                            if !chunk.text.is_empty() {
                                answer_acc.push_str(&chunk.text);
                                let payload =
                                    serde_json::json!({ "text": chunk.text });
                                yield Ok(
                                    Event::default()
                                        .event("token")
                                        .data(payload.to_string())
                                );
                            }
                            if let Some(finish) = chunk.finish {
                                truncated = finish.truncated;
                            }
                        }
                        Err(e) => {
                            let payload =
                                serde_json::json!({ "message": e.to_string() });
                            yield Ok(
                                Event::default()
                                    .event("error")
                                    .data(payload.to_string())
                            );
                            return;
                        }
                    }
                }
                // Mechanical citation gate (sync core — the engine handle
                // was released before the stream opened). Emitted as a
                // terminal `citation` event before `final`; the wire order
                // is meta → token* → citation → final.
                let gate = crate::intelligence::citations::verify_citations_sync(
                    &answer_acc,
                    &grounding,
                );
                // Usage signal (NOW item 4) — same shown-vs-cited logging as
                // the one-shot path. The tokens already streamed; this runs
                // between the last token and the `citation` frame and is
                // non-fatal (a lost training row never breaks the stream).
                if !grounding.is_empty() {
                    let cited_ids: HashSet<&str> =
                        gate.citations.iter().map(|c| c.claim_id.as_str()).collect();
                    let hits: Vec<(String, usize, f32, bool)> = grounding
                        .iter()
                        .enumerate()
                        .map(|(rank, h)| {
                            (h.id.clone(), rank, h.relevance, cited_ids.contains(h.id.as_str()))
                        })
                        .collect();
                    let engine = usage_state.engine.read().await;
                    if let Some(graph) = engine.graph_store(&usage_ws).await {
                        if let Err(e) = graph.record_retrieval_usage(
                            &usage_session,
                            &usage_question,
                            gate.refused,
                            &hits,
                        ) {
                            tracing::warn!("usage-signal record failed (non-fatal): {e}");
                        }
                        // Living Engram (Build 1) — reinforce associative edges
                        // from this verified-good answer's co-citations. Gated
                        // TR_LIVING_EDGES; only when NOT refused and ≥2 distinct
                        // claims were cited (an edge needs two ends). Non-fatal.
                        let living_edges = std::env::var("TR_LIVING_EDGES")
                            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                            .unwrap_or(false);
                        if living_edges && !gate.refused {
                            let mut cited: Vec<String> =
                                cited_ids.iter().map(|s| s.to_string()).collect();
                            cited.sort();
                            cited.dedup();
                            if cited.len() >= 2 {
                                let mut pairs: Vec<(String, String)> = Vec::new();
                                for i in 0..cited.len() {
                                    for j in (i + 1)..cited.len() {
                                        pairs.push((cited[i].clone(), cited[j].clone()));
                                    }
                                }
                                let q = {
                                    let c = gate.answer_confidence as f64;
                                    if c > 0.0 { c.min(1.0) } else { 1.0 }
                                };
                                let ts = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs_f64())
                                    .unwrap_or(0.0);
                                match graph.record_co_citation(&pairs, q, ts) {
                                    Ok(n) => tracing::info!(
                                        pairs = n,
                                        "living-edges: reinforced co-citation edges"
                                    ),
                                    Err(e) => tracing::warn!(
                                        "living-edges record failed (non-fatal): {e}"
                                    ),
                                }
                            }
                        }
                    }
                }
                let citation_payload = serde_json::json!({
                    "citations": gate.citations,
                    "stripped": gate.stripped,
                    "answer_confidence": gate.answer_confidence,
                    "refused": gate.refused,
                });
                yield Ok(
                    Event::default().event("citation").data(citation_payload.to_string())
                );
                let final_payload = serde_json::json!({
                    "claims_used": claims_used,
                    "category": category,
                    "truncated": truncated,
                });
                yield Ok(
                    Event::default().event("final").data(final_payload.to_string())
                );
            }
        }
    };

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ─── Agent streaming response (S5) ───────────────────────────
//
// When the request body sets `use_agent: true`, the streaming
// handler routes here instead of the one-shot retrieve-and-synthesise
// path above. The agent (S3) drives a multi-turn loop calling tools,
// gating writes through `ToolApprovalRouter` (which suspends on a
// oneshot until `/ask/approval/{id}` resolves it), and emitting
// `AgentEvent`s through an mpsc channel.
//
// Wire shape on the SSE stream — every `AgentEvent` becomes one
// `event:` line:
//
//   event: token                  # AgentEvent::Text
//   event: tool_call_proposed     # incl. {id, name, input, is_write}
//   event: tool_call_executing    # incl. {id, name}
//   event: tool_call_finished     # incl. {id, name, content, is_error}
//   event: tool_call_rejected     # incl. {id, name, reason}
//   event: final                  # AgentEvent::Done
//   event: error                  # AgentEvent::Error
//
// In addition, when the agent emits `tool_call_proposed` with
// `is_write: true`, the handler registers a oneshot in
// `state.pending_approvals` keyed by the tool_use_id and emits an
// `approval_requested` SSE event so the desktop UI can render its
// claim card. The UI then POSTs the decision to
// `/ask/approval/{tool_use_id}`.


/// Ship 3F (2026-05-20) — mid-turn system-prompt refresher.
///
/// The agent loop calls `refresh()` at the top of every iteration so
/// the LLM's `system` parameter carries fresh volatile signals
/// (substrate freshness, recent sub-agent reports, fresh recovery
/// events) across long multi-iteration turns. The base system prompt
/// is the static identity + workflow + skill manifest captured at
/// request entry; the refresher appends a small `<system-reminder>`
/// suffix with whatever has changed since.
///
/// **Design decision: append-only suffix, not full re-render.** The
/// full reminder bus runs once on the user-message side at request
/// entry. The refresher only injects signals that mutate WITHIN the
/// turn — typically these three. Identity / today / branch / skills
/// don't change between iterations of the same turn, so re-rendering
/// them would burn LLM tokens for no signal. Aligns with Anthropic
/// "right context at right time" — and minimises prompt-cache churn.
struct RestStreamSystemRefresher {
    state: Arc<AppState>,
    workspace: String,
    base_system_prompt: String,
}

#[async_trait::async_trait]
impl crate::intelligence::agent::SystemPromptRefresher for RestStreamSystemRefresher {
    async fn refresh(&self, _iteration: usize) -> String {
        use crate::intelligence::reminder_assembly::build_mid_turn;
        use crate::intelligence::reminder_bus::render_reactive_reminders;

        // Mid-turn volatile-signals gather lifted into
        // `reminder_assembly::build_mid_turn` (C1, 2026-05-22). Both
        // this REST agent-loop refresher and the new MCP
        // `get_reminder_context` tool (C2) walk the same helper —
        // mid-turn refresh stays cheap (~2 ms) and drift-free.
        let refresh = build_mid_turn(&self.state, &self.workspace).await;
        let ctx = refresh.as_context();
        let suffix = render_reactive_reminders(&ctx);
        if suffix.is_empty() {
            self.base_system_prompt.clone()
        } else {
            format!("{}\n\n{}", self.base_system_prompt, suffix)
        }
    }
}

async fn agent_stream_response(state: Arc<AppState>, ws: String, body: AskRequest) -> Response {
    use crate::intelligence::agent::AgentEvent;
    use crate::intelligence::agent_streaming::{
        StreamAgentDeps, StreamAgentRequest, agent_event_to_sse, spawn_agent_run,
    };
    use crate::intelligence::identity::build_workspace_identity;
    use crate::intelligence::skills::SkillRegistry;
    use crate::intelligence::synthesizer::{
        AskRequest as SynthAskRequest, ChatRole, ChatTurn, build_system_prompt,
        compose_full_system_prompt,
    };

    // Chat-turn timeline instrumentation (ship 2026-05-20). Every
    // stage logs an `elapsed_ms` so a `RUST_LOG=thinkingroot_serve::rest=info`
    // capture answers the "why does chat feel slow?" question
    // without guessing. The user-facing complaint that drove this
    // ("60s watchdog kills chat", which turns out to be misdiagnosed
    // because the watchdog only wraps compile) gets a real
    // measurement surface: prompt-build vs LLM-call vs first-token
    // vs total-turn elapsed are all visible. No new code paths, no
    // wire-format changes — just `tracing::info!` events on the
    // existing path so an operator running `journalctl -u thinkingroot
    // -f` or the desktop's sidecar log sees the timeline.
    let chat_started_at = std::time::Instant::now();
    tracing::info!(
        target: "chat_turn",
        workspace = %ws,
        question_len = body.question.len(),
        history_turns = body.history.len(),
        "received"
    );

    // Snapshot engine state we need before releasing the read lock —
    // the agent path goes async via spawn() and can't hold a guard
    // across .await without serialising every concurrent agent.
    let engine = state.engine.read().await;
    let llm = match engine.workspace_llm(&ws) {
        Some(c) => c,
        None => {
            let payload = serde_json::json!({
                "message": format!("workspace '{ws}' has no LLM configured")
            });
            let stream = async_stream::stream! {
                yield Ok::<Event, std::convert::Infallible>(
                    Event::default().event("error").data(payload.to_string())
                );
            };
            return Sse::new(stream)
                .keep_alive(KeepAlive::new().text("keep-alive"))
                .into_response();
        }
    };
    let workspace_root = state
        .current_workspace_root()
        .await
        .or_else(|| engine.workspace_root_path(&ws));
    let snapshot = engine.workspace_chat_snapshot(&ws).await;
    let chat = snapshot
        .as_ref()
        .map(|s| s.config.chat.resolve(&s.source_kinds))
        .unwrap_or_else(SynthAskRequest::default_chat);
    // C2 (Task 5, plan 2026-05-09): build the workspace identity here
    // so the agent's first user message can carry the same
    // <system-reminder> ambient-context block the non-agent path has
    // shipped since v0.9.0. Prior to this fix the agent literally did
    // not know which workspace it was answering about, how many claims
    // were indexed, or today's date — the audit's C2 critical bug.
    let identity = snapshot
        .as_ref()
        .map(|s| build_workspace_identity(s, &s.config.chat));
    drop(engine);

    let Some(workspace_root) = workspace_root else {
        let payload = serde_json::json!({
            "message": format!("workspace '{ws}' has no on-disk root mounted; agent path requires one")
        });
        let stream = async_stream::stream! {
            yield Ok::<Event, std::convert::Infallible>(
                Event::default().event("error").data(payload.to_string())
            );
        };
        return Sse::new(stream)
            .keep_alive(KeepAlive::new().text("keep-alive"))
            .into_response();
    };

    // Skills live at <workspace_root>/.thinkingroot/skills/. Missing
    // dir is benign (Ok(empty) at `SkillRegistry::load_from_dir`);
    // an Err here means the dir IS present but malformed — a parse
    // or IO failure that silently degrades the agent's skill manifest
    // to empty. Phase C.3 (2026-05-17) upgraded the log level from
    // WARN → ERROR with structured fields so the failure surfaces in
    // the doctor log and the operator can actually find it; the chat
    // continues so the user still gets an answer.
    let skill_dir = workspace_root.join(".thinkingroot/skills");
    let skills = match SkillRegistry::load_from_dir(&skill_dir) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            tracing::error!(
                skill_dir = %skill_dir.display(),
                error = %e,
                "skill registry failed to load — chat continuing without skills. \
                 Fix the broken skill file(s) and the next turn will reload them."
            );
            Arc::new(SkillRegistry::empty())
        }
    };

    // Compose the full system prompt: persona + (no style — styles
    // are resolved server-side from `[chat]` config in a future
    // sprint) + skill manifest.
    let mut system_prompt = compose_full_system_prompt(chat, None, Some(&skills));
    let _ = build_system_prompt; // re-export for callers that want raw

    // Ship 3C (2026-05-20) — classify intent BEFORE the agent runs
    // and splice the mode-specific workflow appendix onto the system
    // prompt. The classifier is deterministic + sub-µs (keyword +
    // length heuristics) so the path is cache-friendly per-mode (4
    // possible suffixes, not N-questions-many). Recovery signals
    // surface from the workspace status snapshot.
    // Lightweight recovery-event probe for intent classification
    // (the full `RecoveryEventBrief` list is built later for the
    // reminder bus). Only checks "did anything land in the last 5
    // minutes" — sub-ms — and doesn't need the per-event projection.
    let has_recent_recovery = match thinkingroot_core::recovery_log::tail(50) {
        Ok(events) => {
            let now = chrono::Utc::now();
            let window = chrono::Duration::minutes(5);
            events
                .into_iter()
                .any(|ev| now.signed_duration_since(ev.ts) <= window)
        }
        Err(_) => false,
    };
    let intent_inputs = crate::intelligence::intent::ClassifyInputs {
        question: &body.question,
        has_recent_recovery_events: has_recent_recovery,
        last_compile_failed: {
            let snapshots = state.workspace_status.snapshot_all().await;
            snapshots
                .into_iter()
                .find(|s| s.name == ws)
                .map(|s| matches!(
                    s.compile,
                    thinkingroot_core::types::CompileState::Idle {
                        last_outcome: Some(thinkingroot_core::types::CompileOutcome::Failed { .. }),
                        ..
                    }
                ))
                .unwrap_or(false)
        },
    };
    let intent = crate::intelligence::intent::classify_intent(&intent_inputs);
    let workflow_appendix = crate::intelligence::intent::workflow_appendix(intent);
    system_prompt.push_str(workflow_appendix);
    tracing::info!(
        target: "chat_turn",
        workspace = %ws,
        intent = intent.slug(),
        "intent_classified"
    );

    // Translate wire-format history into ChatTurn → ChatMessage.
    let chat_history: Vec<ChatTurn> = body
        .history
        .iter()
        .filter_map(|t| {
            let role = match t.role.as_str() {
                "user" => ChatRole::User,
                "assistant" => ChatRole::Assistant,
                _ => return None,
            };
            let content = t.content.trim();
            if content.is_empty() {
                return None;
            }
            Some(ChatTurn {
                role,
                content: content.to_string(),
            })
        })
        .collect();
    let agent_messages = crate::intelligence::agent::chat_turns_to_messages(&chat_history);

    let conversation_id = body
        .conversation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // River v1.0 — symmetric stream-branch auto-create on the REST chat
    // path. The MCP `tools/call` path has called this since T0.6; the
    // REST path previously did NOT, so agent contributions from desktop
    // chat landed on `main` even when `streams.auto_session_branch =
    // true`. The shared helper at `mcp::auto_create_session_branch` is
    // idempotent (skips when session.active_branch is already set) and
    // creates the SessionContext lazily if absent — safe to call before
    // `spawn_agent_run` mints the session via tools.
    {
        let engine_guard = state.engine.read().await;
        crate::mcp::auto_create_session_branch(
            &ws,
            &engine_guard,
            &conversation_id,
            &state.sessions,
        )
        .await;
    }

    // Phase B.1 (2026-05-17) — record the user's first message of
    // this session and persist it onto the stream branch's
    // `description` so `maintenance::cleanup_once` can propagate
    // that title to the auto-created topic branch at merge time.
    // Idempotent: only the first turn of each session pays the (one)
    // registry write. Persistence is best-effort — a registry I/O
    // failure here MUST NOT block the user's chat turn from running.
    {
        let (stored_msg, branch_name) = {
            let mut sessions_guard = state.sessions.lock().await;
            let session = sessions_guard
                .entry(conversation_id.clone())
                .or_insert_with(|| {
                    crate::intelligence::session::SessionContext::new(
                        conversation_id.clone(),
                        ws.clone(),
                    )
                });
            let stored = session.set_first_user_message_if_unset(&body.question);
            let branch_name = session.active_branch.clone();
            (stored, branch_name)
        };
        if let (Some(stored), Some(branch_name)) = (stored_msg, branch_name) {
            // Single TOML write under a registry file lock; measured
            // in single-digit ms. Cheap enough to inline on the async
            // runtime for the one-shot-per-session it represents.
            match thinkingroot_branch::set_branch_description(
                &workspace_root,
                &branch_name,
                Some(stored),
            ) {
                Ok(_) => {
                    tracing::debug!(
                        branch = %branch_name,
                        "B.1: persisted first_user_message on stream branch description"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        branch = %branch_name,
                        "B.1: failed to persist first_user_message on stream branch description (chat continues): {e}"
                    );
                }
            }
        }
    }

    // C1 (2026-05-22): the full 17-block gather (formerly inline,
    // ~300 LOC) is lifted into `intelligence::reminder_assembly::build`.
    // Both this REST chat path and the new MCP `get_reminder_context`
    // tool (C2) walk through the same helper — drift between the
    // two surfaces is structurally impossible from here on.
    use crate::intelligence::reminder_assembly::build as build_reminder_context;
    use crate::intelligence::reminder_bus::render_reactive_reminders;

    let reminder_build =
        build_reminder_context(&state, &ws, &conversation_id, &body.question, Some(&skills))
            .await;
    let bus_ctx = reminder_build.as_context(identity.as_ref());
    let bus_prefix = render_reactive_reminders(&bus_ctx);

    let user_question = if bus_prefix.is_empty() {
        body.question.clone()
    } else {
        format!("{}{}", bus_prefix, body.question)
    };

    // Phase B.2 (2026-05-17): captures threaded into the stream block
    // for post-Done auto-distill. Taken BEFORE the StreamAgentRequest
    // is constructed because that construction moves `conversation_id`
    // and `workspace_root` by value.
    let workspace_root_for_persist = workspace_root.clone();
    let conversation_id_for_persist = conversation_id.clone();
    let user_question_for_persist = body.question.clone();
    let sessions_for_persist = state.sessions.clone();
    // Shared branch-engine cache so the post-Done turn persist writes through the
    // SAME resident handle the retrieval path uses (no second cozo instance →
    // no branch-graph corruption). Captured up-front: cheap Arc clone.
    let branch_cache_for_persist = state.engine.read().await.branch_engines_arc();

    tracing::info!(
        target: "chat_turn",
        workspace = %ws,
        elapsed_ms = chat_started_at.elapsed().as_millis() as u64,
        prompt_bytes = system_prompt.len(),
        bus_bytes = bus_prefix.len(),
        history_turns = agent_messages.len(),
        recalls = reminder_build.agentmemory_recalls.len(),
        engrams = reminder_build.engram_handles.len(),
        skill_picked = reminder_build.relevant_skill_name.is_some(),
        "prompt_built"
    );

    // Ship 3F (2026-05-20) — wire the mid-turn refresher so long
    // agent runs (up to 25 iterations) re-inject fresh volatile
    // signals into the `system` parameter at the top of every LLM
    // call. `state.clone()` is cheap (Arc bump); `system_prompt` is
    // captured by value as the baseline the refresher appends to.
    let refresher = Arc::new(RestStreamSystemRefresher {
        state: state.clone(),
        workspace: ws.clone(),
        base_system_prompt: system_prompt.clone(),
    })
        as Arc<dyn crate::intelligence::agent::SystemPromptRefresher>;

    let req = StreamAgentRequest {
        workspace: ws.clone(),
        workspace_root,
        session_id: conversation_id,
        agent_id: "thinkingroot".to_string(),
        system_prompt,
        user_question,
        history: agent_messages,
        skills,
        system_refresher: Some(refresher),
    };
    let deps = StreamAgentDeps {
        engine: state.engine.clone(),
        llm,
        sessions: state.sessions.clone(),
        pending_approvals: state.pending_approvals.clone(),
        trace: None,
        engram_manager: state.engram_manager.clone(),
        permission_store: state.permission_store.clone(),
    };

    // Cancellation = client disconnect, end-to-end (same contract
    // `compile_stream` enforces at rest.rs:2911). The token is owned
    // by the SSE response body's `async_stream::stream!` block below
    // via `drop_guard`; when the body is dropped (Stop button, modal
    // close, network drop) the guard fires the token and the agent
    // task observes it at its next safe checkpoint. Pre-2026-05-17
    // the agent had no cancellation observation; tokens kept burning
    // until natural completion.
    let cancel = tokio_util::sync::CancellationToken::new();
    let drop_guard = cancel.clone().drop_guard();
    tracing::info!(
        target: "chat_turn",
        workspace = %ws,
        elapsed_ms = chat_started_at.elapsed().as_millis() as u64,
        "agent_spawning"
    );
    let (mut rx, router) = spawn_agent_run(req, deps, cancel);

    // The streaming task watches the event channel. For every
    // `tool_call_proposed` with `is_write: true`, it (1) tells the
    // ToolApprovalRouter to register a pending oneshot under the
    // tool_use_id, and (2) emits an `approval_requested` SSE event
    // so the desktop UI can render its claim card. The matching
    // POST to `/ask/approval/{id}` resolves the oneshot and the
    // agent unblocks.
    // State that survives across the stream loop so the post-`Done`
    // verifier has the inputs it needs:
    //   * `capture` — folds every search_claims / hybrid_retrieve
    //     result into RetrievalHits (intelligence/retrieval_capture.rs)
    //   * `last_rejection` — flips on when the agent's last action
    //     before Done was a rejected write tool. Drives
    //     VerifyKind::SkippedRejection so the trust receipt renders
    //     "no claim made" instead of trying to ground "user declined".
    //   * `final_text_for_verify` — the agent's final answer text,
    //     captured from `Done`. The substantive path can't run
    //     without it.
    let engine_for_verify = state.engine.clone();
    let workspace_for_verify = ws.clone();
    // Hand the timeline marker + workspace label into the stream block
    // so first-token + done log lines carry the same correlation
    // identity as the pre-spawn events. No clone — `chat_started_at`
    // is `Copy`.
    let chat_started_at_for_stream = chat_started_at;
    let workspace_for_timeline = ws.clone();
    let stream = async_stream::stream! {
        use crate::intelligence::retrieval_capture::{HashSetSubstrate, RetrievalCapture};
        use crate::intelligence::verifier::{
            DEFAULT_AUTO_CITE_THRESHOLD, Verdict, VerifyInput, VerifyKind, verify,
        };

        // Move the DropGuard inside the stream block. When the SSE
        // response body is dropped (Stop button, client disconnect,
        // modal close) this guard fires the matching cancellation
        // token and the spawned agent task observes it at its next
        // safe checkpoint.
        let _agent_drop_guard = drop_guard;

        // First-token watermark for the chat-turn timeline. Flips
        // exactly once on the first Text/ToolCallProposed/
        // ToolCallExecuting event so a `RUST_LOG=chat_turn=info`
        // capture pinpoints whether the wait is in retrieval setup
        // (long pre-first-token gap) or in tool execution (short
        // first-token, long total).
        let mut first_event_logged = false;

        // Surface a cheap meta event up front so UIs that show a
        // "category" header have something to render before tokens
        // start flowing. claims_used is unknown from the agent
        // (tools may produce many results), so we report 0 for now.
        let meta = serde_json::json!({
            "claims_used": 0,
            "category": "agentic",
        });
        yield Ok::<Event, std::convert::Infallible>(
            Event::default().event("meta").data(meta.to_string())
        );

        let mut capture = RetrievalCapture::new();
        let mut last_was_rejection = false;
        let mut final_text_for_verify: Option<String> = None;

        // Ship 3E (2026-05-20) — retrieval outcome snoop. Correlates
        // ToolCallProposed id+query with ToolCallFinished id+hit-count
        // so the post-Done hook can write back the LAST retrieval's
        // query + hits onto the session for the next turn's
        // `<search_was_shallow>` reminder. Map cleared on each new
        // proposal so a flurry of retrievals only retains the most
        // recent. Names matched are the canonical retrieval-class
        // tools — keep in sync with `RetrievalCapture::observe_tool_finished`.
        const RETRIEVAL_TOOLS: &[&str] = &[
            "hybrid_retrieve",
            "search",
            "search_claims",
            "query_claims",
        ];
        let mut pending_retrieval_query: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        let mut last_retrieval_outcome: Option<(String, u32)> = None;

        // `router` is kept alive for the lifetime of the stream (the
        // agent task holds its own Arc via the PermissionsGate wrap);
        // SSE no longer needs to call `set_pending_id` because the
        // agent supplies `tool_use_id` directly to
        // `ApprovalGate::check`. The router registers its pending
        // oneshot under that id, the `/ask/approval/{id}` POST
        // resolves it — no race between the SSE relay and the agent
        // dispatch task.
        let _router_anchor = router;
        while let Some(event) = rx.recv().await {
            // Timeline: log the first downstream event so an
            // operator can see whether the user-visible wait is
            // pre-first-token (LLM warm-up + reminder bus emission)
            // or post-first-token (tool fan-out, retrieval ranking).
            if !first_event_logged {
                first_event_logged = true;
                let kind_tag = match &event {
                    AgentEvent::Text { .. } => "text",
                    AgentEvent::ToolCallProposed { .. } => "tool_call_proposed",
                    AgentEvent::ToolCallExecuting { .. } => "tool_call_executing",
                    AgentEvent::ToolCallFinished { .. } => "tool_call_finished",
                    AgentEvent::ToolCallRejected { .. } => "tool_call_rejected",
                    AgentEvent::ToolCallProgress { .. } => "tool_call_progress",
                    AgentEvent::ContinuationOffered { .. } => "continuation_offered",
                    AgentEvent::Done { .. } => "done",
                    _ => "other",
                };
                tracing::info!(
                    target: "chat_turn",
                    workspace = %workspace_for_timeline,
                    elapsed_ms = chat_started_at_for_stream.elapsed().as_millis() as u64,
                    first_event = kind_tag,
                    "first_token"
                );
            }

            // Surface the approval prompt to the UI only when the
            // agent's `PermissionsGate` will actually delegate to
            // the inner UI-prompt path. Pre-fix (2026-05-18) the SSE
            // emitted `approval_requested` for **every** write-tool
            // proposal — but the agent's gate may auto-decide via
            // policy (Allow rule or DEFAULT_DENY hit) without ever
            // registering a `pending_approvals` entry. The user's
            // subsequent click then hit a 404 `NO_PENDING_APPROVAL`
            // ("Permission decision failed" toast in the 2026-05-18
            // bug report).
            //
            // `PermissionsGate::predict` runs the same
            // canonicalisation + policy evaluation as the gate's
            // `check`, returning a tri-state outcome. Emit ONLY
            // when the prediction is `Ask`. The agent's gate may
            // observe a different store snapshot if a rule edit
            // races between predict + check — that TOCTOU is caught
            // defensively by `commands/chat.rs::chat_approve`
            // treating `NO_PENDING_APPROVAL` as a silent dismiss.
            if let AgentEvent::ToolCallProposed { id, is_write, name, input } = &event {
                // Ship 3E — record proposed retrieval queries so the
                // matching ToolCallFinished can correlate hits → query.
                if RETRIEVAL_TOOLS.contains(&name.as_str()) {
                    let query = input
                        .get("query")
                        .and_then(|v| v.as_str())
                        .or_else(|| input.get("q").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_string();
                    if !query.is_empty() {
                        pending_retrieval_query
                            .insert(id.clone(), (name.clone(), query));
                    }
                }
                if *is_write {
                    use crate::intelligence::permissions_gate::{PermissionsGate, PolicyOutcome};
                    let outcome = PermissionsGate::predict(
                        &state.permission_store,
                        name,
                        input,
                    )
                    .await;
                    if matches!(outcome, PolicyOutcome::Ask) {
                        // Phase D Wave 1 — when this is a Phase D
                        // system-power tool, attach a `permission_context`
                        // so the UI can render a permission-aware prompt
                        // (Allow once / Allow always for pattern X /
                        // Deny once / Deny always; deny-only when the
                        // canonical path matches DEFAULT_DENY).
                        let permission_context =
                            build_permission_context_for_tool(name, input);
                        let mut payload = serde_json::json!({
                            "id": id,
                            "name": name,
                            "input": input,
                        });
                        if let Some(ctx) = permission_context {
                            payload["permission_context"] = ctx;
                        }
                        yield Ok(
                            Event::default()
                                .event("approval_requested")
                                .data(payload.to_string())
                        );
                    }
                    // Allow / Deny outcomes: agent auto-decides; the
                    // ToolCallExecuting / ToolCallRejected event that
                    // follows below carries the actual outcome to the
                    // UI.
                }
            }

            // Side effect: fold retrieval results into the capture
            // BEFORE we yield the SSE event. Tools that aren't
            // retrieval-shaped no-op inside observe_tool_finished.
            //
            // Engram-tool side effect: when the agent calls
            // `materialize_engram` or `probe_engram` we additionally
            // emit an `engram_activated` SSE event so the desktop's
            // EngramTimeline scrubber can render the per-turn
            // activation footprint without re-parsing every tool
            // result on the UI side. This is a strictly additive
            // event — clients that don't recognise the type ignore
            // it (per SSE spec).
            let mut engram_activation: Option<serde_json::Value> = None;
            let mut gap_surfacing: Option<serde_json::Value> = None;
            match &event {
                AgentEvent::ToolCallFinished { id, name, content, is_error, .. } => {
                    capture.observe_tool_finished(name, content, *is_error);
                    last_was_rejection = false;
                    // Ship 3E — correlate retrieval finishes with the
                    // earlier proposal to record query + hit count.
                    // Hit count is parsed from common substrate-tool
                    // wire shapes: top-level `"hits"` / `"claims"` /
                    // `"results"` array length. When the shape doesn't
                    // match we honestly record 0 hits — the agent saw
                    // an empty / error result and the next turn's
                    // shallow-search warning should fire.
                    if !*is_error && RETRIEVAL_TOOLS.contains(&name.as_str()) {
                        if let Some((_, query)) = pending_retrieval_query.remove(id) {
                            let parsed: serde_json::Value =
                                serde_json::from_str(content).unwrap_or(serde_json::Value::Null);
                            let hit_count: u32 = ["hits", "claims", "results", "items"]
                                .iter()
                                .find_map(|k| parsed.get(*k).and_then(|v| v.as_array()).map(|a| a.len()))
                                .unwrap_or(0) as u32;
                            last_retrieval_outcome = Some((query, hit_count));
                        }
                    }
                    // 2026-05-17 — shape-driven side-event detection.
                    // Pre-fix the dispatch keyed on the literal tool
                    // name; new aggregate/wrapper tools that produce
                    // the same wire shape were silently invisible to
                    // the EngramTimeline + GapCards. The parsers now
                    // self-identify by JSON shape and return None when
                    // the shape doesn't match — drop in only the ones
                    // that pass.
                    if !*is_error {
                        engram_activation = parse_engram_activation(name, content);
                        gap_surfacing = parse_gaps_surfacing(content);
                    }
                }
                AgentEvent::ToolCallRejected { .. } => {
                    last_was_rejection = true;
                }
                AgentEvent::Text { .. } | AgentEvent::ToolCallExecuting { .. } => {
                    // These don't change rejection state; only a
                    // ToolCallFinished can clear it.
                }
                AgentEvent::Done { final_text, .. } => {
                    final_text_for_verify = Some(final_text.clone());
                }
                _ => {}
            }

            let (kind, payload) = agent_event_to_sse(&event);
            yield Ok(
                Event::default().event(kind).data(payload.to_string())
            );
            if let Some(activation) = engram_activation {
                yield Ok(
                    Event::default().event("engram_activated").data(activation.to_string())
                );
            }
            if let Some(gaps) = gap_surfacing {
                yield Ok(
                    Event::default().event("gaps_surfaced").data(gaps.to_string())
                );
            }

            // Terminal events end the stream after we emit the
            // trust-receipt follow-up.
            if matches!(event, AgentEvent::Done { .. }) {
                tracing::info!(
                    target: "chat_turn",
                    workspace = %workspace_for_timeline,
                    elapsed_ms = chat_started_at_for_stream.elapsed().as_millis() as u64,
                    tools_called = capture.claim_ids().count(),
                    last_was_rejection,
                    final_text_bytes = final_text_for_verify.as_ref().map(|s| s.len()).unwrap_or(0),
                    "done"
                );
                // Build the Substrate by batching claim_exists across
                // every captured retrieval hit. Cheap (one DbInstance
                // clone + per-id Cozo lookups; bounded by retrieval
                // top-K). Skipped when capture is empty — the
                // VerifyKind::SkippedRejection / SkippedChitchat paths
                // don't need a substrate at all.
                let kind_for_verify = if last_was_rejection {
                    VerifyKind::SkippedRejection
                } else {
                    VerifyKind::Substantive
                };
                let candidate_ids: Vec<String> =
                    capture.claim_ids().cloned().collect();
                let existing = if candidate_ids.is_empty() {
                    std::collections::HashSet::new()
                } else {
                    let eng = engine_for_verify.read().await;
                    eng.claim_exists_batch(&workspace_for_verify, &candidate_ids).await
                };
                let substrate = HashSetSubstrate::new(existing);
                let final_text =
                    final_text_for_verify.clone().unwrap_or_default();
                let top_k = capture.into_hits();
                let verdict = verify(&VerifyInput {
                    kind: kind_for_verify,
                    text: &final_text,
                    agent_citations: &[],
                    top_k: &top_k,
                    substrate: &substrate,
                    auto_cite_threshold: DEFAULT_AUTO_CITE_THRESHOLD,
                });

                // Ship 3D (2026-05-20) — Reflexion writeback. Capture
                // the verdict's critique into the session so the NEXT
                // turn's `<previous_verify>` reminder block surfaces it
                // and the LLM self-corrects when grounding was weak.
                // Only writes when the verdict carries actionable
                // signal — benign verdicts (FullyGrounded / Skipped*)
                // store empty placeholders so a stale prior critique
                // doesn't linger across turns.
                {
                    let (verdict_slug, verified, unverified, reason) = match &verdict {
                        Verdict::FullyGrounded { claims_used, .. } => (
                            "high_grounding".to_string(),
                            claims_used.len() as u32,
                            0u32,
                            String::new(),
                        ),
                        Verdict::PartiallyGrounded { claims_used, related_count } => (
                            "low_grounding".to_string(),
                            claims_used.len() as u32,
                            *related_count as u32,
                            format!(
                                "{} claim(s) cited, but {} only matched by surface vocabulary — \
                                 the prior answer leaned on related context, not strict evidence",
                                claims_used.len(),
                                related_count
                            ),
                        ),
                        Verdict::UnverifiedCitations { bad_claim_ids, claims_used } => (
                            "ungrounded".to_string(),
                            claims_used.len() as u32,
                            bad_claim_ids.len() as u32,
                            format!(
                                "{} [claim:…] marker(s) referenced ids that don't exist in the substrate",
                                bad_claim_ids.len()
                            ),
                        ),
                        Verdict::SkippedChitchat => (
                            "chitchat".to_string(),
                            0,
                            0,
                            String::new(),
                        ),
                        Verdict::SkippedRejection => (
                            "skipped_rejection".to_string(),
                            0,
                            0,
                            String::new(),
                        ),
                        Verdict::SkippedBenchHarness => (
                            "skipped_bench".to_string(),
                            0,
                            0,
                            String::new(),
                        ),
                    };
                    let mut store = sessions_for_persist.lock().await;
                    if let Some(s) = store.get_mut(&conversation_id_for_persist) {
                        s.record_verify_critique(verdict_slug, verified, unverified, reason);
                        // Ship 3E — record this turn's last retrieval
                        // outcome so the NEXT turn's
                        // `<search_was_shallow>` reminder block can
                        // surface it. Honest: when no retrieval ran
                        // this turn, leave the prior turn's outcome
                        // untouched — that's an accurate "the last
                        // retrieval was the previous turn's" signal.
                        if let Some((query, hits)) = last_retrieval_outcome.take() {
                            s.record_search_outcome(query, hits);
                        }
                    }
                }

                let payload = verdict.to_sse_payload();
                yield Ok(
                    Event::default()
                        .event("trust_receipt")
                        .data(payload.to_string())
                );

                // Phase B.2 (2026-05-17) — auto-distill this turn
                // onto the session's stream branch graph so the
                // NEXT turn's `hybrid_retrieve` / `search` can pull
                // it as context. Runs AFTER trust_receipt is on the
                // wire so a persistence stall never delays the
                // user-visible completion signal. Skipped silently
                // when:
                //   * there is no active stream branch on the
                //     session (e.g. `auto_session_branch = false`),
                //   * both user_question and the agent's final text
                //     are empty,
                //   * the chat_turn_count allocator hasn't yet
                //     incremented (defensive — should never happen
                //     since `next_chat_turn` runs in the agent loop
                //     before Done).
                let final_text_owned =
                    final_text_for_verify.clone().unwrap_or_default();
                let (active_branch_opt, turn_n) = {
                    let store = sessions_for_persist.lock().await;
                    match store.get(&conversation_id_for_persist) {
                        Some(s) => (s.active_branch.clone(), s.chat_turn_count),
                        None => (None, 0),
                    }
                };
                if let Some(active_branch) = active_branch_opt {
                    if turn_n > 0 {
                        match crate::intelligence::turn_persistence::persist_chat_turn(
                            &branch_cache_for_persist,
                            &workspace_root_for_persist,
                            &active_branch,
                            &conversation_id_for_persist,
                            turn_n,
                            &user_question_for_persist,
                            &final_text_owned,
                        )
                        .await
                        {
                            Ok(persisted) => {
                                tracing::debug!(
                                    branch = %active_branch,
                                    session_id = %conversation_id_for_persist,
                                    turn_number = persisted.turn_number,
                                    "B.2: persisted chat turn onto stream branch"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    branch = %active_branch,
                                    session_id = %conversation_id_for_persist,
                                    turn_number = turn_n,
                                    "B.2: chat-turn persistence failed (trust_receipt already on wire, chat completes normally): {e}"
                                );
                            }
                        }
                    }
                }

                break;
            }
        }
    };

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ─── Approval POST handler (S5) ──────────────────────────────
//
// POST /api/v1/ws/{ws}/ask/approval/{tool_use_id}
// Body: {"decision": "approve" | "reject", "reason": "..."}
//
// Resolves the matching pending oneshot in `state.pending_approvals`,
// unblocking the agent's `ToolApprovalRouter::check`. The `ws` path
// param is currently unused (every tool_use_id is globally unique
// across workspaces) but kept in the URL so future per-workspace
// scoping is a non-breaking change.

#[derive(Deserialize)]
struct ApprovalRequestBody {
    /// Either "approve" or "reject". Anything else is treated as
    /// rejection so a malformed client can't sneak through.
    decision: String,
    /// Optional human-readable reason. Surfaced to the LLM via the
    /// `tool_call_rejected` event when the decision is "reject".
    #[serde(default)]
    reason: Option<String>,
    /// Phase D Wave 1 (2026-05-17) — when present, the user clicked
    /// "Allow always" or "Deny always" in the permission prompt, and
    /// this rule is persisted to `permissions.toml` BEFORE the
    /// oneshot is resolved. If insert fails (e.g. the rule pattern
    /// overlaps with `DEFAULT_DENY`), the approval is still resolved
    /// (so the agent doesn't hang) but with a Rejected decision and
    /// the `ProtectedPath` error as the reason.
    #[serde(default)]
    persist_rule: Option<PersistRuleBody>,
}

#[derive(Deserialize)]
struct PersistRuleBody {
    /// `"path"` or `"command"`.
    kind: String,
    /// Glob pattern (path rules) or shell-command pattern.
    pattern: String,
    /// `"allow"` or `"deny"`.
    decision: String,
}

async fn ask_approval_handler(
    State(state): State<Arc<AppState>>,
    Path((_ws, tool_use_id)): Path<(String, String)>,
    Json(body): Json<ApprovalRequestBody>,
) -> Response {
    use crate::intelligence::approval::{ApprovalDecision, ToolApprovalRouter};
    use thinkingroot_core::permissions::{Decision as PermDecision, Rule, RuleKind};

    // Phase D Wave 1 — persist the rule BEFORE resolving the
    // oneshot, so the next turn's PermissionsGate sees the new rule
    // immediately. Failures here (e.g. DEFAULT_DENY conflict) coerce
    // the decision to Rejected with the protect-path error as the
    // reason — we never silently drop a rule write but we also never
    // hang the agent waiting on a decision we couldn't persist.
    let mut decision_override_reason: Option<String> = None;
    if let Some(rule_body) = &body.persist_rule {
        let kind = match rule_body.kind.as_str() {
            "path" => RuleKind::Path,
            "command" => RuleKind::Command,
            other => {
                return err_response(
                    StatusCode::BAD_REQUEST,
                    "INVALID_RULE_KIND",
                    &format!("persist_rule.kind must be `path` or `command`, got `{other}`"),
                );
            }
        };
        let perm_decision = match rule_body.decision.as_str() {
            "allow" => PermDecision::Allow,
            "deny" => PermDecision::Deny,
            other => {
                return err_response(
                    StatusCode::BAD_REQUEST,
                    "INVALID_RULE_DECISION",
                    &format!("persist_rule.decision must be `allow` or `deny`, got `{other}`"),
                );
            }
        };
        let new_rule = Rule {
            kind,
            pattern: rule_body.pattern.clone(),
            decision: perm_decision,
            created_at: chrono::Utc::now(),
            created_by: "user-decision".to_string(),
        };
        let mut store = state.permission_store.write().await;
        match store.insert_rule(new_rule) {
            Ok(_) => {
                // Persist to disk (best-effort; failure logged but
                // doesn't block the in-memory decision).
                if let Some(path) = permissions_toml_path() {
                    if let Err(e) = store.save(&path) {
                        tracing::warn!(
                            "permissions.toml write failed; rule held in memory only: {e}"
                        );
                    }
                }
            }
            Err(e) => {
                // DEFAULT_DENY conflict or invalid pattern → coerce
                // decision to Rejected so the agent doesn't think
                // the user enabled access that's hardcoded-blocked.
                decision_override_reason = Some(format!(
                    "permission policy refused rule persistence: {e}"
                ));
            }
        }
    }

    let decision = if let Some(reason) = decision_override_reason {
        ApprovalDecision::Rejected { reason }
    } else {
        match body.decision.as_str() {
            "approve" | "approved" => ApprovalDecision::Approved,
            _ => ApprovalDecision::Rejected {
                reason: body.reason.unwrap_or_else(|| "user declined".to_string()),
            },
        }
    };

    let resolved =
        ToolApprovalRouter::resolve(&state.pending_approvals, &tool_use_id, decision).await;

    if resolved {
        ok_response(serde_json::json!({"resolved": true})).into_response()
    } else {
        err_response(
            StatusCode::NOT_FOUND,
            "NO_PENDING_APPROVAL",
            &format!("no pending approval for tool_use_id '{tool_use_id}'"),
        )
    }
}

/// Phase D Wave 1 — build a `permission_context` JSON object for
/// an approval_requested event, based on the tool name + input.
///
/// Returns `None` when the tool isn't a Phase D system-power tool
/// (so the UI renders the standard approval prompt). Returns
/// `Some(json)` for the 10 system-power tools, with a structure
/// the UI consumes to render the permission-aware prompt:
///
/// ```json
/// {
///   "tool": "file_read",
///   "canonical_path": "/Users/me/.ssh/id_rsa",   // present for path-typed tools
///   "command": "git status",                      // present for shell_exec
///   "suggested_pattern": "~/.ssh/**",             // pattern the user could persist
///   "default_deny_matched": true                  // when true, UI hides Allow buttons
/// }
/// ```
fn build_permission_context_for_tool(
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<serde_json::Value> {
    use thinkingroot_core::permissions::PermissionStore;
    use thinkingroot_core::safe_path::canonicalize_for_policy;

    let is_phase_d = matches!(
        tool_name,
        "file_read"
            | "file_write"
            | "file_edit"
            | "glob"
            | "grep"
            | "shell_exec"
            | "clipboard_read"
            | "clipboard_write"
            | "open_in_default"
            | "trash"
    );
    if !is_phase_d {
        return None;
    }

    let mut ctx = serde_json::json!({
        "tool": tool_name,
    });

    // Extract the primary path (or command for shell_exec).
    let primary_path: Option<String> = match tool_name {
        "file_read" | "file_edit" | "open_in_default" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(String::from),
        "file_write" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(String::from),
        "glob" | "grep" => input
            .get("base")
            .or_else(|| input.get("path"))
            .and_then(|v| v.as_str())
            .map(String::from),
        "shell_exec" => input.get("cwd").and_then(|v| v.as_str()).map(String::from),
        "trash" => input
            .get("paths")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(String::from),
        _ => None,
    };

    // Surface the shell command separately so the UI can show it.
    if tool_name == "shell_exec" {
        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            ctx["command"] = serde_json::Value::String(cmd.to_string());
        }
    }

    if let Some(raw_path) = primary_path {
        let canonical = canonicalize_for_policy(std::path::Path::new(&raw_path));
        match canonical {
            Ok(c) => {
                ctx["canonical_path"] = serde_json::Value::String(c.display().to_string());
                // Check DEFAULT_DENY by evaluating against an empty store.
                let empty = PermissionStore::empty();
                let decision = empty.evaluate_path(&c);
                if matches!(
                    decision,
                    thinkingroot_core::permissions::Decision::Deny
                ) {
                    ctx["default_deny_matched"] =
                        serde_json::Value::Bool(true);
                }
                ctx["suggested_pattern"] =
                    serde_json::Value::String(suggested_pattern_for_canonical_path(&c));
            }
            Err(_e) => {
                // Path doesn't exist yet (file_write to new path)
                // or canonicalize failed. Still surface the raw
                // path so the UI can render context.
                ctx["raw_path"] = serde_json::Value::String(raw_path.clone());
                ctx["suggested_pattern"] = serde_json::Value::String(
                    suggested_pattern_for_raw_path(&raw_path),
                );
            }
        }
    }

    Some(ctx)
}

/// Produce a glob pattern the UI can offer as "Allow always for X".
/// Maps `/Users/me/Code/myproj/foo.rs` → `~/Code/**` when the path
/// is inside the user's home. For paths outside `~`, returns the
/// parent directory with `/**` appended so the rule covers the
/// directory (not the specific file).
fn suggested_pattern_for_canonical_path(canonical: &std::path::Path) -> String {
    let canonical_str = canonical.to_string_lossy();
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = canonical_str.strip_prefix(home_str.as_ref())
            && let Some(rest) = rest.strip_prefix('/')
        {
            // Take the first segment under home for the pattern.
            let first = rest.split('/').next().unwrap_or(rest);
            if !first.is_empty() {
                return format!("~/{first}/**");
            }
        }
    }
    // Outside home — pattern over the parent dir.
    canonical
        .parent()
        .map(|p| format!("{}/**", p.display()))
        .unwrap_or_else(|| canonical_str.into_owned())
}

/// Best-effort suggestion when the path can't be canonicalized
/// (typically file_write to a new file). Mirrors the canonical
/// version but operates on the literal string.
fn suggested_pattern_for_raw_path(raw: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = raw.strip_prefix(home_str.as_ref())
            && let Some(rest) = rest.strip_prefix('/')
        {
            let first = rest.split('/').next().unwrap_or(rest);
            if !first.is_empty() {
                return format!("~/{first}/**");
            }
        }
    }
    std::path::Path::new(raw)
        .parent()
        .map(|p| format!("{}/**", p.display()))
        .unwrap_or_else(|| raw.to_string())
}

/// Phase D Wave 1 — canonical on-disk location of the permission
/// store: `<dirs::config_dir()>/thinkingroot/permissions.toml`.
/// Same parent dir as `cortex.lock` and `credentials.toml`. Returns
/// `None` only when `dirs::config_dir()` fails (extremely rare —
/// no home directory on a non-XDG system).
pub fn permissions_toml_path() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("thinkingroot").join("permissions.toml"))
}

/// Load the permission store from disk at startup. Falls through to
/// an empty store on any error (first-run, corruption, schema
/// mismatch is logged) — chat continues, only the rules layer is
/// empty until the user re-creates them via the UI prompts.
pub fn load_permission_store_or_empty() -> thinkingroot_core::permissions::PermissionStore {
    let Some(path) = permissions_toml_path() else {
        return thinkingroot_core::permissions::PermissionStore::empty();
    };
    match thinkingroot_core::permissions::PermissionStore::load(&path) {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(
                "permissions.toml load failed; starting with empty store: {e}"
            );
            thinkingroot_core::permissions::PermissionStore::empty()
        }
    }
}

// ─── LLM Health (pre-flight) ─────────────────────────────────

/// GET /api/v1/ws/{ws}/llm/health
///
/// Cheap pre-flight the desktop calls on workspace switch. Tells the user
/// up-front whether `ask` will produce a real LLM-synthesised answer or fall
/// back to the top-claim statement, so the chat UI never spins for 120 s on a
/// silently-unconfigured workspace.
#[derive(Serialize)]
struct LlmHealthBody {
    /// True iff a provider+key resolved at workspace mount time.
    configured: bool,
    /// Provider name (e.g. "anthropic", "azure"). `None` when unconfigured.
    provider: Option<String>,
    /// Display model name. `None` when unconfigured.
    model: Option<String>,
    /// Number of claims compiled into this workspace — `0` means the engine
    /// will return the "not enough information" fallback regardless of LLM.
    claim_count: usize,
    /// Whether the workspace is mounted at all. `false` → 404-equivalent;
    /// the desktop should refuse to chat against a non-existent workspace.
    mounted: bool,
}

async fn llm_health_handler(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Response {
    let engine = state.engine.read().await;

    // Use the engine's existing workspace-info call: it returns the claim
    // count alongside identity, so one call covers `mounted` + `claim_count`.
    let info = engine
        .list_workspaces()
        .await
        .ok()
        .and_then(|list| list.into_iter().find(|w| w.name == ws));
    let Some(info) = info else {
        return ok_response(LlmHealthBody {
            configured: false,
            provider: None,
            model: None,
            claim_count: 0,
            mounted: false,
        })
        .into_response();
    };

    let llm = engine.workspace_llm(&ws);
    let configured = llm.is_some();
    let (provider, model) = match llm.as_deref() {
        Some(c) => (
            Some(c.provider_name().to_string()),
            Some(c.model_name().to_string()),
        ),
        None => (None, None),
    };

    ok_response(LlmHealthBody {
        configured,
        provider,
        model,
        claim_count: info.claim_count,
        mounted: true,
    })
    .into_response()
}

// ─── Slice 3: workspace event SSE stream ────────────────────────────

async fn stream_workspace_events_handler(
    State(state): State<Arc<AppState>>,
    Path(_ws): Path<String>,
) -> Response {
    use tokio_stream::StreamExt as _;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

    let Some(rx) = state.subscribe_workspace_events().await else {
        return err_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "WATCHER_NOT_RUNNING",
            "workspace watcher is not attached to this daemon",
        );
    };
    let stream = BroadcastStream::new(rx).map(|res| match res {
        Ok(event) => {
            let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            Ok::<Event, std::convert::Infallible>(
                Event::default().event("workspace_event").data(payload),
            )
        }
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            let payload = serde_json::json!({ "missed": n }).to_string();
            Ok(Event::default().event("lagged").data(payload))
        }
    });
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ─── Slice 0: unified workspace status ───────────────────────

/// `GET /api/v1/workspaces/{name}/status` — current full snapshot.
///
/// One-shot read; for live updates connect to
/// [`workspace_status_stream_handler`]. The body is a
/// [`WorkspaceStatus`] (the same shape served on the SSE stream's
/// `Snapshot` events) so consumers can use one decoder for both routes.
async fn workspace_status_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let path = match resolve_workspace_path(&state, &name).await {
        Some(p) => p,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("workspace `{name}` is not registered"),
            );
        }
    };
    let actor = state.workspace_status.ensure(&name, path).await;
    let snap = actor.current().await;
    Json(snap).into_response()
}

/// `GET /api/v1/workspaces/{name}/status/stream` — SSE stream of
/// [`WorkspaceStatusEvent`]s. The first event a fresh subscriber
/// receives is a `snapshot` carrying the actor's current state, so
/// connect-then-render is a one-roundtrip operation; subsequent
/// `snapshot` and `heartbeat` events follow.
///
/// Mirrors the broadcast → SSE pattern of
/// [`stream_branch_events_handler`] (T1.6) so the two surfaces share
/// the same lagged-event semantics and keep-alive cadence.
async fn workspace_status_stream_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    use tokio_stream::StreamExt as _;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

    let path = match resolve_workspace_path(&state, &name).await {
        Some(p) => p,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("workspace `{name}` is not registered"),
            );
        }
    };
    let actor = state.workspace_status.ensure(&name, path).await;

    // Capture the current snapshot so the very first event the client
    // sees is a complete `Snapshot` — never an empty connect followed
    // by a wait for the next state change.
    let initial = actor.current().await;
    let initial_event = WorkspaceStatusEvent::Snapshot(initial);
    let initial_payload =
        serde_json::to_string(&initial_event).unwrap_or_else(|_| "{}".to_string());

    let rx = actor.subscribe();
    let live = BroadcastStream::new(rx).map(|res| match res {
        Ok(event) => {
            let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            let kind = match &event {
                WorkspaceStatusEvent::Snapshot(_) => "snapshot",
                WorkspaceStatusEvent::Heartbeat { .. } => "heartbeat",
            };
            Ok::<Event, std::convert::Infallible>(Event::default().event(kind).data(payload))
        }
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            let payload = serde_json::json!({ "missed": n }).to_string();
            Ok(Event::default().event("lagged").data(payload))
        }
    });
    let initial_stream = tokio_stream::once(Ok::<Event, std::convert::Infallible>(
        Event::default().event("snapshot").data(initial_payload),
    ));
    let stream = initial_stream.chain(live);

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// `POST /api/v1/workspaces/{name}/refresh` — force the actor to
/// re-probe the on-disk substrate + sources axes. Used by the desktop
/// "Refresh" command palette entry and by long-idle clients that
/// suspect the watcher missed an event.
async fn workspace_status_refresh_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let path = match resolve_workspace_path(&state, &name).await {
        Some(p) => p,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("workspace `{name}` is not registered"),
            );
        }
    };
    let actor = state.workspace_status.ensure(&name, path).await;
    if let Err(e) = actor.send(WorkspaceStatusMsg::Refresh).await {
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "ACTOR_DOWN",
            &format!("status actor inbox closed: {e}"),
        );
    }
    ok_response(serde_json::json!({ "refreshed": name })).into_response()
}

/// Resolve `<name> → <root_path>` from the engine's mounted workspaces
/// list. Returns `None` for unregistered names so the three status
/// endpoints can refuse with a 404 rather than spawning a phantom actor.
async fn resolve_workspace_path(state: &AppState, name: &str) -> Option<PathBuf> {
    let engine = state.engine.read().await;
    let list = engine.list_workspaces().await.ok()?;
    list.into_iter()
        .find(|w| w.name == name)
        .map(|w| PathBuf::from(w.path))
}

// ─── Engram-activation SSE shim ──────────────────────────────────────
//
// Parses the JSON-string output of the `materialize_engram` and
// `probe_engram` MCP tools into a flat wire shape the desktop's
// EngramTimeline scrubber can consume directly:
//
//   materialize_engram → { tool: "materialize_engram", pointer: "0x7F9A",
//                          summary: { ... },
//                          source_count: N, ts_ms: <epoch> }
//   probe_engram      → { tool: "probe_engram", pointer: "0x7F9A",
//                         answer_count: M, ts_ms: <epoch> }
//
// `ts_ms` is the wall-clock time at which we forwarded the event —
// the engine doesn't yet thread a timestamp through the agent's
// ToolCallFinished payload. Honest scope: the timeline shows when the
// SSE relay observed the activation, not when the EngramManager
// internally cached the row.
//
// `source_count` for materialize_engram is best-effort: we read it
// from `summary.source_count` if present, else `summary.sources.len()`,
// else 0 (the wire shape is owned by `intelligence/engram.rs::EngramSummary`
// and may evolve). The UI treats 0 as "unknown" rather than "empty".
// Shape-driven, NOT name-driven (2026-05-17). Pre-fix this matched on
// the literal `name == "materialize_engram"` / `name == "probe_engram"`
// strings — any future tool that wrapped or aliased those (e.g. a
// composite `search_engrams` that internally materialises + ranks)
// would silently fail to surface the EngramTimeline event. The shape
// discriminator is specific: probe results have a `pointer` string +
// `answers` array; materialise results have a `pointer` string + a
// `summary` object. Two shapes, two events, no name dependency. The
// `tool` field in the emitted payload still carries `name` as a
// diagnostic so downstream consumers can observe which tool
// produced the activation.
fn parse_engram_activation(name: &str, content: &str) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
    let pointer = parsed.get("pointer").and_then(|v| v.as_str())?;
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Probe shape: `pointer + answers (array)`. Check this BEFORE the
    // materialise shape so a result that somehow carries both (e.g.
    // a future composite tool) classifies as the more informative
    // probe shape.
    if let Some(answers) = parsed.get("answers").and_then(|v| v.as_array()) {
        return Some(serde_json::json!({
            "tool": name,
            "pointer": pointer.to_string(),
            "answer_count": answers.len() as u64,
            "ts_ms": now_ms,
        }));
    }

    // Materialise shape: `pointer + summary (object)`. `summary` may
    // be an empty object on cold pointers; that still counts as the
    // materialise shape — the UI renders an honest "0 sources" cell
    // rather than swallowing the event.
    if let Some(summary) = parsed.get("summary").and_then(|v| v.as_object()) {
        let source_count = summary
            .get("source_count")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                summary
                    .get("sources")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len() as u64)
            })
            .unwrap_or(0);
        return Some(serde_json::json!({
            "tool": name,
            "pointer": pointer.to_string(),
            "summary": serde_json::Value::Object(summary.clone()),
            "source_count": source_count,
            "ts_ms": now_ms,
        }));
    }

    None
}

// ─── Gap-surfacing SSE shim ──────────────────────────────────────────
//
// Parses the JSON-string output of the `gaps` MCP tool into a flat
// per-gap wire shape the desktop's GapCards component renders inline:
//
//   { gaps: [
//       { entity_name, entity_type, expected_claim_type,
//         confidence, sample_size, reason },
//       ...
//     ],
//     ts_ms: <epoch> }
//
// Honest scope: the daemon's `gaps` MCP arm already filters by the
// caller's `min_confidence`. We trust that filter — no further
// confidence pruning here. Empty-gap responses are dropped so the UI
// never renders a "no gaps found" toast (the chat surface is the
// wrong place for null-result feedback).
fn parse_gaps_surfacing(content: &str) -> Option<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
    let gaps_arr = parsed.get("gaps").and_then(|v| v.as_array())?;
    if gaps_arr.is_empty() {
        return None;
    }
    // Shape discriminator: every gap row is an object carrying at
    // least `entity_name` AND `expected_claim_type` (the gap MCP
    // tool's stable wire shape per `mcp/tools.rs::list_gaps`). Pre-
    // 2026-05-17 the literal `name == "gaps"` filter was enough; the
    // shape-driven dispatch this commit moves to needs a tighter
    // discriminator so a coincidental `{gaps: [...]}` shape on an
    // unrelated tool doesn't trip a false GapCards render.
    let first = gaps_arr.first().and_then(|v| v.as_object())?;
    if !first.contains_key("entity_name") || !first.contains_key("expected_claim_type") {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    Some(serde_json::json!({
        "gaps": gaps_arr,
        "ts_ms": now_ms,
    }))
}

#[cfg(test)]
mod ingest_path_guard_tests {
    use super::safe_join_under;
    use std::path::Path;

    #[test]
    fn accepts_nested_relative_paths_under_root() {
        let root = Path::new("/srv/ws");
        let p = safe_join_under(root, "src/cli/index.ts").expect("clean relative path is allowed");
        assert_eq!(p, Path::new("/srv/ws/src/cli/index.ts"));
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let root = Path::new("/srv/ws");
        assert!(safe_join_under(root, "../etc/passwd").is_none());
        assert!(safe_join_under(root, "src/../../etc/passwd").is_none());
        assert!(safe_join_under(root, "a/b/../../../x").is_none());
    }

    #[test]
    fn rejects_absolute_paths() {
        let root = Path::new("/srv/ws");
        assert!(safe_join_under(root, "/etc/passwd").is_none());
    }

    #[test]
    fn rejects_empty_and_dot_only() {
        let root = Path::new("/srv/ws");
        assert!(safe_join_under(root, "").is_none());
        assert!(safe_join_under(root, ".").is_none());
        assert!(safe_join_under(root, "./").is_none());
    }

    #[test]
    fn rejects_embedded_nul() {
        let root = Path::new("/srv/ws");
        assert!(safe_join_under(root, "src/in\0dex.ts").is_none());
    }

    #[test]
    fn leading_current_dir_is_normalised_then_allowed() {
        // `./src/x` — the CurDir component is rejected outright (we require
        // every component be Normal), so this is None. Callers strip a
        // leading "./" before upload; this asserts the conservative default.
        let root = Path::new("/srv/ws");
        assert!(safe_join_under(root, "./src/x").is_none());
    }
}

#[cfg(test)]
mod sse_side_event_tests {
    use super::{parse_engram_activation, parse_gaps_surfacing};

    #[test]
    fn parse_engram_activation_recognises_probe_shape_independent_of_name() {
        // Probe shape: pointer + answers (array). The name argument
        // is purely diagnostic; passing a wrapper-tool name still
        // produces the event so future aliases inherit the
        // EngramTimeline integration for free.
        let body = r#"{"pointer":"0x7F9A","answers":[{"id":"c1"},{"id":"c2"}]}"#;
        let out = parse_engram_activation("future_search_engrams", body)
            .expect("probe shape must trigger event regardless of tool name");
        assert_eq!(out["pointer"], "0x7F9A");
        assert_eq!(out["answer_count"], 2);
        assert_eq!(out["tool"], "future_search_engrams");
    }

    #[test]
    fn parse_engram_activation_recognises_materialize_shape() {
        let body = r#"{"pointer":"0x7F9A","summary":{"source_count":3}}"#;
        let out = parse_engram_activation("materialize_engram", body)
            .expect("materialise shape must trigger event");
        assert_eq!(out["pointer"], "0x7F9A");
        assert_eq!(out["source_count"], 3);
    }

    #[test]
    fn parse_engram_activation_drops_unrelated_shapes() {
        // A tool result that happens to be valid JSON but doesn't
        // carry the probe or materialise discriminator must NOT
        // surface a spurious engram_activated event.
        let unrelated = r#"{"matches":["a","b"]}"#;
        assert!(parse_engram_activation("grep", unrelated).is_none());

        // Non-JSON text is also rejected cleanly.
        assert!(parse_engram_activation("anything", "raw text result").is_none());

        // `pointer` alone (no answers, no summary) is ambiguous —
        // not enough signal to classify, so we refuse to emit.
        let ambiguous = r#"{"pointer":"0xDEAD"}"#;
        assert!(parse_engram_activation("anything", ambiguous).is_none());
    }

    #[test]
    fn parse_gaps_surfacing_requires_typed_gap_rows() {
        // Real gaps tool shape — first row has the discriminator
        // fields.
        let body = r#"{"gaps":[{"entity_name":"foo","entity_type":"Function","expected_claim_type":"DocComment","confidence":0.8,"sample_size":3,"reason":"missing"}]}"#;
        let out = parse_gaps_surfacing(body).expect("typed gap rows must surface");
        assert_eq!(out["gaps"].as_array().unwrap().len(), 1);

        // Coincidental `gaps: [{...}]` from an unrelated tool with
        // a different row shape must NOT trip the side-event.
        let coincidence = r#"{"gaps":[{"key":"x","value":1}]}"#;
        assert!(
            parse_gaps_surfacing(coincidence).is_none(),
            "non-gap shape must not produce a gaps_surfaced event"
        );

        // Empty array is also dropped (gap tool conventionally
        // returns absence as no event, not a noisy "0 gaps" toast).
        let empty = r#"{"gaps":[]}"#;
        assert!(parse_gaps_surfacing(empty).is_none());
    }
}

// ─── Error Mapping ───────────────────────────────────────────

fn match_engine_error(e: thinkingroot_core::Error) -> Response {
    match &e {
        thinkingroot_core::Error::EntityNotFound(_) => {
            err_response(StatusCode::NOT_FOUND, "NOT_FOUND", &e.to_string())
        }
        thinkingroot_core::Error::Config(_) => {
            err_response(StatusCode::NOT_FOUND, "NOT_FOUND", &e.to_string())
        }
        thinkingroot_core::Error::WorkspaceOrphaned { .. } => err_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "WORKSPACE_ORPHANED",
            &e.to_string(),
        ),
        _ => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            &e.to_string(),
        ),
    }
}
