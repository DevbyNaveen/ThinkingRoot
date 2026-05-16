//! Workspace commands — lifecycle for the **Satellites** surface.
//!
//! A *workspace* is a folder ThinkingRoot has compiled into a queryable
//! knowledge graph. The CLI manages these via `tr workspace add` /
//! `tr compile`; the desktop app exposes the same operations through
//! Tauri commands so non-CLI users can:
//!
//! 1. Register a folder ([`workspace_add`]) — creates `.thinkingroot/`
//!    when missing (same as `root init`) so the engine can mount immediately
//! 2. Compile a folder ([`workspace_compile`])
//! 3. List, set-active, or remove registered workspaces
//!
//! Compilation streams live progress as `workspace_compile_progress`
//! events — driven by `thinkingroot_serve::pipeline::ProgressEvent`,
//! which is what the CLI's indicatif bars consume. The webview can
//! render the same progression with no schema change.

use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};
use thinkingroot_serve::pipeline::{PipelineResult, ProgressEvent};
use tokio_util::sync::CancellationToken;

use crate::state::{AppState, CompileHandle};

/// Notify the webview that `workspaces.toml` changed so the sidebar can
/// re-fetch `workspace_list` (compiled badges, ordering, etc.).
fn emit_workspaces_changed(app: &AppHandle) {
    let _ = app.emit("workspaces-changed", true);
}

/// Create `<root>/.thinkingroot/` when absent — matches `root init` (directory
/// only; config inherits from global). Idempotent.
fn ensure_thinkingroot_data_dir(workspace_root: &std::path::Path) -> Result<(), String> {
    let dir = workspace_root.join(".thinkingroot");
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))
}

/// One workspace as the UI sees it. Mirrors [`WorkspaceEntry`] plus
/// derived fields the surface uses to colour the row (compiled badge,
/// active marker).
#[derive(Debug, Serialize, Clone)]
pub struct WorkspaceView {
    pub name: String,
    pub path: String,
    pub port: u16,
    /// `true` when `<path>/.thinkingroot/graph.db` exists. The same
    /// check the CLI's `workspace list` uses.
    pub compiled: bool,
    /// `true` when this workspace is the one bound to
    /// `THINKINGROOT_WORKSPACE` (and thus what `chat_send` recalls from).
    pub active: bool,
}

#[tauri::command]
pub fn workspace_list() -> Result<Vec<WorkspaceView>, String> {
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let active_name = registry.active.as_deref();
    Ok(registry
        .workspaces
        .iter()
        .map(|w| WorkspaceView {
            name: w.name.clone(),
            path: w.path.display().to_string(),
            port: w.port,
            compiled: w.path.join(".thinkingroot").join("graph.db").exists(),
            active: active_name == Some(w.name.as_str()),
        })
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceAddArgs {
    pub path: String,
    pub name: Option<String>,
    pub port: Option<u16>,
}

#[tauri::command]
pub fn workspace_add(app: AppHandle, args: WorkspaceAddArgs) -> Result<WorkspaceView, String> {
    let abs = std::fs::canonicalize(&args.path)
        .map_err(|e| format!("path not found: {} ({e})", args.path))?;
    ensure_thinkingroot_data_dir(&abs)?;
    let mut registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let name = args.name.unwrap_or_else(|| {
        abs.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string())
    });
    let port = args.port.unwrap_or_else(|| registry.next_available_port());
    registry.add(WorkspaceEntry {
        name: name.clone(),
        path: abs.clone(),
        port,
    });
    registry.save().map_err(|e| e.to_string())?;
    emit_workspaces_changed(&app);

    let active = registry.active.as_deref() == Some(name.as_str());
    Ok(WorkspaceView {
        name,
        path: abs.display().to_string(),
        port,
        compiled: abs.join(".thinkingroot").join("graph.db").exists(),
        active,
    })
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceRemoveArgs {
    pub name: String,
}

#[tauri::command]
pub fn workspace_remove(app: AppHandle, args: WorkspaceRemoveArgs) -> Result<bool, String> {
    let mut registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let removed_active = registry.active.as_deref() == Some(args.name.as_str());
    let removed = registry.remove(&args.name);
    if removed {
        if removed_active {
            registry.clear_active();
        }
        registry.save().map_err(|e| e.to_string())?;
        emit_workspaces_changed(&app);
    }
    Ok(removed)
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceSetActiveArgs {
    pub name: String,
}

/// Mark a registered workspace as the one chat / brain / privacy commands
/// resolve to. Persists into the shared `WorkspaceRegistry.active` pointer
/// — single source of truth.
///
/// Stream A — the next call into `SidecarClient::ensure_active` will
/// pick up the new active workspace from the registry and remount it
/// on the daemon side via `POST /api/v1/workspaces`.  No desktop-side
/// cache to invalidate.
#[tauri::command]
pub async fn workspace_set_active(
    app: AppHandle,
    args: WorkspaceSetActiveArgs,
) -> Result<String, String> {
    let mut registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let root_path = registry
        .workspaces
        .iter()
        .find(|w| w.name == args.name)
        .map(|e| e.path.clone())
        .ok_or_else(|| format!("workspace `{}` not found", args.name))?;
    ensure_thinkingroot_data_dir(&root_path)?;
    let abs = root_path.display().to_string();
    registry.set_active(&args.name).map_err(|e| e.to_string())?;
    registry.save().map_err(|e| e.to_string())?;
    emit_workspaces_changed(&app);
    Ok(abs)
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceCompileArgs {
    /// Either an already-registered workspace name or an absolute path.
    /// The latter form is what the file-picker flow uses; the former
    /// is what the Satellites row's "recompile" button uses.
    pub target: String,
    pub branch: Option<String>,
}

/// Streamed when compilation progresses. Curated subset of
/// [`thinkingroot_serve::pipeline::ProgressEvent`] mapped to a stable
/// wire vocabulary the React layer can render as discrete bars/phases.
/// We deliberately do not surface every internal pipeline event — the UI
/// shows phase transitions plus per-phase progress, not every batch tick.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum CompileProgress {
    Started {
        workspace: String,
    },
    /// Unified compile-progress snapshot — emitted every 250 ms by the
    /// daemon ticker while a compile is live. **This is the single
    /// canonical progress event** new UI surfaces should render. The
    /// per-phase variants below (`ParseComplete`, `ExtractionProgress`,
    /// …) are kept for back-compat with the existing React multi-bar
    /// component until that migrates to a single-bar rendering of
    /// `Tick`.
    Tick {
        /// User-facing step. One of: `reading`, `extracting`,
        /// `linking`, `persisting`, `packing`. Steps can repeat
        /// across a single compile (e.g. Linking → Persisting →
        /// Linking) — render the current step, never gate on
        /// "have we passed step N".
        step: String,
        /// Human-readable step label (e.g. `"Linking"`).
        step_label: String,
        /// Step-local counter. 0 when total is also 0.
        done: u64,
        /// Step-local total. 0 means "indeterminate; show spinner".
        total: u64,
        /// Wall-clock since the current step started (ms).
        step_elapsed_ms: u64,
        /// Wall-clock since the compile started (ms).
        total_elapsed_ms: u64,
        /// Daemon-computed ETA for the current step (ms). `None` when
        /// total is unknown or done is 0.
        eta_ms: Option<u64>,
        /// Short sub-phase caption (e.g. "removing changed sources",
        /// "synthesizing paper"). `None` when the engine has nothing
        /// more specific to say than the step label. The UI renders
        /// this in the indeterminate-spinner caption — pre-fix the
        /// caption was hard-coded to "counting sources…" regardless
        /// of what the engine was actually doing.
        #[serde(default)]
        detail: Option<String>,
    },
    /// Emitted while the compile task is waiting for the bundled
    /// `root` sidecar to finish booting (livez probe + child-process
    /// liveness).  Pre-fix the user's "Compile" click sat silent for
    /// up to 120 s with no UI signal that anything was happening,
    /// indistinguishable from a hung app.  Now React renders a
    /// "Waiting for engine…" indicator that resolves into the
    /// ParseComplete/ExtractionStart events the user expects.
    Booting {
        workspace: String,
    },
    /// Sidecar is up; we've sent `POST /compile/stream` and are
    /// waiting for the server to reach its first ticker emission
    /// (~500ms typical, up to ~2s on cold first-mount). Pre-fix the
    /// bar sat at "Starting compilation · 5%" with no movement during
    /// this window. The fix paints a brief "Connecting to engine…"
    /// caption that the first Tick overwrites within a second on a
    /// warm sidecar.
    Connecting {
        workspace: String,
    },
    /// First attempt failed; auto-retry is scheduled. Carries the
    /// retry attempt index (1-based — the user's click was attempt 0),
    /// the backoff delay before the retry fires, and the first-attempt
    /// error so the UI can surface "Retrying after: <error>" honestly
    /// instead of pretending nothing went wrong. Pre-fix the bar sat
    /// frozen at the last tick percent during the backoff with no UI
    /// signal that a retry was happening, then snapped back to 5%
    /// when the retry pipeline emitted its first Reading tick — which
    /// looked like an unexplained reset.
    Retrying {
        workspace: String,
        attempt: u32,
        after_ms: u64,
        first_error: String,
    },
    DiffStart,
    DiffComplete {
        changed: usize,
        unchanged: usize,
        deleted: usize,
    },
    ParseComplete {
        files: usize,
    },
    ExtractionStart {
        total_chunks: usize,
        total_batches: usize,
    },
    ExtractionProgress {
        done: usize,
        total: usize,
    },
    ExtractionComplete {
        claims: usize,
        entities: usize,
    },
    /// Emitted when one or more LLM batches failed permanently during
    /// extraction.  React renders a non-fatal toast — the compile is
    /// still moving forward but the chunks in `failed_chunk_ranges`
    /// have no claims.  Pre-fix these failures were silently dropped.
    ExtractionPartial {
        failed_batches: usize,
        failed_chunk_ranges: Vec<(usize, usize)>,
    },
    GroundingStart {
        llm_claims: usize,
        structural_claims: usize,
    },
    GroundingProgress {
        done: usize,
        total: usize,
    },
    GroundingDone {
        accepted: usize,
        rejected: usize,
    },
    FingerprintDone {
        truly_changed: usize,
        cutoffs: usize,
    },
    RootingStart {
        candidates: usize,
    },
    RootingProgress {
        done: usize,
        total: usize,
    },
    RootingDone {
        rooted: usize,
        attested: usize,
        quarantined: usize,
        rejected: usize,
    },
    LinkingStart {
        total_entities: usize,
    },
    LinkingProgress {
        done: usize,
        total: usize,
    },
    VectorProgress {
        done: usize,
        total: usize,
    },
    VectorUpdateDone {
        entities_indexed: usize,
        claims_indexed: usize,
    },
    CompilationProgress {
        done: usize,
        total: usize,
    },
    CompilationDone {
        artifacts: usize,
    },
    VerificationDone {
        health: u8,
    },
    PhaseDone {
        name: String,
        elapsed_ms: u64,
    },
    Done {
        files_parsed: usize,
        claims: usize,
        entities: usize,
        relations: usize,
        contradictions: usize,
        artifacts: usize,
        health_score: u8,
        cache_dirty: bool,
        /// Carried through from PipelineResult so the React side can
        /// render a "compile finished but N batches failed" warning
        /// without listening to a separate ExtractionPartial event.
        #[serde(default)]
        failed_batches: usize,
        #[serde(default)]
        failed_chunk_ranges: Vec<(usize, usize)>,
        /// Full incremental delta (source/claim/structural counts +
        /// per-phase timings) so the React side can render a summary
        /// panel without listening to a separate IncrementalDone event.
        /// Optional because pre-T8 daemons lack the field; the React
        /// side renders an empty panel when None.
        #[serde(default)]
        incremental_summary: Option<thinkingroot_core::IncrementalSummary>,
    },
    /// Caller-initiated stop via `workspace_compile_stop`.  Distinct
    /// from `Failed` so the UI can render a neutral "stopped" state
    /// instead of a red error toast.  Per-source state already
    /// persisted by Phase 4 / per-batch checkpoint flushes is
    /// preserved on disk.
    Cancelled,
    Failed {
        error: String,
    },
}

/// Kick off a compile in a background tokio task. Returns immediately;
/// progress flows via `workspace_compile_progress` Tauri events keyed
/// to the workspace path so the UI can correlate when multiple compiles
/// run concurrently.
///
/// The compile is cancellable via `workspace_compile_stop` — the
/// `CancellationToken` lives in `AppState.active_compile` for the
/// lifetime of the run.  Pre-fix the only way to stop a compile was
/// to kill the desktop process, which discarded all extraction work.
/// Sidecar-boot wait: 20 polls × 500 ms = **10 s**. Pre-Witness-Mesh
/// the cap was 60 s with stale justification of "large NLI ONNX model
/// + first-run fastembed download". Both deps are gone (verified via
/// `crates/thinkingroot-ground/Cargo.toml:17` and
/// `crates/thinkingroot-extract/src/extractor.rs:76`), so a healthy
/// sidecar boots in 2–4 s — 10 s gives 4–5× headroom while feeling
/// instant on the click that just sat through 60 s of dead spinner.
const SIDECAR_BOOT_MAX_ATTEMPTS: u32 = 20;

/// Maximum supersede-clear wait. Beyond this the prior task is
/// considered wedged and we **abort the JoinHandle** (force-kill the
/// tokio task) before registering the new one. Without abort, the
/// previous task could keep running and emit a `Cancelled` Tauri
/// event after the new compile had already started — racing the new
/// `Started` event into the UI.
const SLOT_CLEAR_MAX_WAIT: Duration = Duration::from_secs(5);

/// Resolution of `WorkspaceCompileArgs.target` into the pair the
/// helper task actually needs: a workspace URL alias the sidecar's
/// `/api/v1/ws/{ws}/compile/stream` endpoint understands, and the
/// canonical filesystem root path.
struct ResolvedCompileTarget {
    /// The registered workspace name when `args.target` matched a
    /// `workspaces.toml` entry; otherwise `"_"` — the placeholder
    /// the server-side `compile_stream` handler accepts for
    /// path-based workspace lookup. Pre-fix this was hardcoded to
    /// `"desktop"`, which only happened to work because the engine
    /// canonicalises by `root_path` regardless of alias — but it
    /// produced misleading status-actor keys and broke any future
    /// per-alias routing.
    url_alias: String,
    /// Canonical filesystem path of the workspace root. Passed in
    /// the request body so the sidecar locates the right
    /// `.thinkingroot/` directory.
    path: PathBuf,
}

fn resolve_compile_target(target: &str) -> Result<ResolvedCompileTarget, String> {
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    if let Some(entry) = registry.workspaces.iter().find(|w| w.name == target) {
        return Ok(ResolvedCompileTarget {
            url_alias: entry.name.clone(),
            path: entry.path.clone(),
        });
    }
    let canonical = std::fs::canonicalize(target).map_err(|e| {
        format!("not a registered workspace and not a path: {target} ({e})")
    })?;
    // When the user picks a folder via the file dialog (not via the
    // registry), the registry's canonicalized paths sometimes still
    // match — handle that case too so we don't surface a spurious
    // `_` alias when the path is actually known.
    if let Some(entry) = registry
        .workspaces
        .iter()
        .find(|w| w.path == canonical)
    {
        return Ok(ResolvedCompileTarget {
            url_alias: entry.name.clone(),
            path: entry.path.clone(),
        });
    }
    Ok(ResolvedCompileTarget {
        url_alias: "_".to_string(),
        path: canonical,
    })
}

#[tauri::command]
pub async fn workspace_compile(
    app: AppHandle,
    args: WorkspaceCompileArgs,
) -> Result<String, String> {
    let target = resolve_compile_target(&args.target)?;
    let workspace_label = target.path.display().to_string();
    let branch = args.branch;

    // ── Compile-scoped breaker pre-check ───────────────────────────
    // The self-heal substrate (`thinkingroot_core::restart_state`)
    // tracks compile failures separately from process crashes. If 3
    // failures landed in the last 5 minutes, the breaker is tripped
    // for 10 minutes — refuse the click loud-and-honest rather than
    // letting the user hammer a deterministically broken provider.
    // The auto-retry path inside the spawned task ALSO consults this
    // breaker so a single user-initiated compile can't kick off
    // retry storms.
    {
        let mut rs = thinkingroot_core::restart_state::RestartState::load().unwrap_or_default();
        rs.prune_compile_attempts();
        if rs.compile_breaker_active() {
            let until = rs
                .compile_breaker_until
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "<unknown>".into());
            let count = rs.recent_compile_failure_count(&target.url_alias);
            return Err(format!(
                "Compile is paused after repeated failures (last 5 min: {count}). \
                 Auto-clears at {until}. Open Help → Recovery Log to inspect; \
                 the breaker can be reset manually via `root doctor --fix`."
            ));
        }
    }

    // ── Slot supersede / force-abort ───────────────────────────────
    // One in-flight compile slot per desktop process. A second
    // workspace returns a hard error; the same workspace cancels the
    // prior run + waits up to 5 s for the slot to clear, then aborts
    // the wedged task's JoinHandle and force-clears.
    {
        let state = app.state::<AppState>();
        let guard = state.active_compile.lock().await;
        if let Some(handle) = guard.as_ref() {
            if handle.workspace_label != workspace_label {
                return Err(format!(
                    "compile already in progress for {}; call workspace_compile_stop \
                     first or wait for it to finish",
                    handle.workspace_label
                ));
            }
            tracing::warn!(
                workspace = %workspace_label,
                "workspace_compile superseding prior run for same workspace — cancelling tracked compile"
            );
            handle.cancel.cancel();
        }
    }
    let wait_started = Instant::now();
    loop {
        let slot_free = {
            let state = app.state::<AppState>();
            let guard = state.active_compile.lock().await;
            guard.is_none()
        };
        if slot_free {
            break;
        }
        if wait_started.elapsed() >= SLOT_CLEAR_MAX_WAIT {
            // Force-clear path: take the old CompileHandle out of the
            // slot, drop the guard, and abort the JoinHandle. Pre-fix
            // we only set `*guard = None`, which left the old tokio
            // task running in the background — it could still emit a
            // `Cancelled` Tauri event AFTER the new task had emitted
            // `Started`, producing the UI's "compile briefly works
            // then says it stopped" glitch.
            let zombie: Option<CompileHandle> = {
                let state = app.state::<AppState>();
                let mut guard = state.active_compile.lock().await;
                guard.take()
            };
            if let Some(handle) = zombie {
                tracing::error!(
                    workspace = %workspace_label,
                    "active_compile still set after supersede cancel + {}s wait — \
                     aborting task JoinHandle (zombie compile)",
                    SLOT_CLEAR_MAX_WAIT.as_secs()
                );
                // Trip once more for good measure (no-op if already tripped).
                handle.cancel.cancel();
                handle.task.abort();
            }
            break;
        }
        // Cancel-aware wait: if a Stop click trips a fresh token in
        // the meantime, exit the wait immediately rather than burn
        // the full 5 s window. We don't have OUR cancel yet (this
        // is pre-task), so we just sleep — but interleave with a
        // tokio::task::yield to keep the runtime responsive.
        tokio::time::sleep(Duration::from_millis(80)).await;
        tokio::task::yield_now().await;
    }

    // ── Spawn the compile task ─────────────────────────────────────
    let cancel = CancellationToken::new();
    let app_for_task = app.clone();
    let path_for_task = target.path.clone();
    let label_for_task = workspace_label.clone();
    let url_alias_for_task = target.url_alias.clone();
    let cancel_for_task = cancel.clone();

    let task = tokio::spawn(async move {
        run_desktop_compile_task(
            app_for_task,
            url_alias_for_task,
            path_for_task,
            label_for_task,
            branch,
            cancel_for_task,
        )
        .await;
    });

    // Register the handle (with JoinHandle) so:
    //   - workspace_compile_stop can trip the cancel
    //   - the next force-clear can abort() the task
    //   - the task itself can clear the slot on completion
    {
        let state = app.state::<AppState>();
        let mut guard = state.active_compile.lock().await;
        *guard = Some(CompileHandle {
            workspace_label: workspace_label.clone(),
            cancel,
            task,
        });
    }

    Ok(workspace_label)
}

/// Limit for auto-retry on a transient compile failure. The first
/// attempt is the user's click; we add at most one quiet retry when
/// the failure looks transient (and the breaker hasn't tripped).
const COMPILE_AUTO_RETRY_LIMIT: u32 = 1;

/// Body of the spawned compile task. Owns:
///
/// - Sidecar boot wait (10 s cap, cancel-aware).
/// - First attempt: SSE compile via the sidecar.
/// - Auto-retry: on `Failed` and no active breaker, sleep
///   `compile_backoff_for_attempt(1)` and try once more — typically
///   recovers from transient `ECONNRESET` during sidecar workspace
///   mount, or a flaky provider's 502.
/// - Self-heal accounting: every outcome records into `RestartState`
///   (`record_compile_failure` / `record_compile_success` /
///   `record_compile_cancellation`) and writes a
///   `recovery_log::RecoveryEvent` so the doctor + recovery log
///   surface both reflect compile health.
/// - Tauri event emission (`Started` / `Booting` / `Done` / `Failed`
///   / `Cancelled`) plus the `workspaces-changed` notification.
/// - Final slot clear so the next `workspace_compile` can start
///   fresh.
async fn run_desktop_compile_task(
    app: AppHandle,
    url_alias: String,
    path: PathBuf,
    workspace_label: String,
    branch: Option<String>,
    cancel: CancellationToken,
) {
    use thinkingroot_core::recovery_log::{self, RecoveryEvent};
    use thinkingroot_core::restart_state::{self, RestartState};

    let _ = app.emit(
        "workspace_compile_progress",
        CompileProgress::Started {
            workspace: workspace_label.clone(),
        },
    );

    // Sidecar boot wait — emits a single Booting event on first miss
    // so the UI shows "Waiting for engine…" instead of a dead spinner.
    let sidecar = await_sidecar_handle(&app, &workspace_label, &cancel).await;

    let first_attempt = match sidecar {
        Some((host, port)) => {
            // Bridge the Started→first-tick gap with a "Connecting"
            // caption so the bar shows movement during the HTTP POST
            // + server-side pipeline setup window (~500ms warm, up to
            // ~2s cold). The first Tick from the server overwrites
            // this caption within a second on typical workloads.
            let _ = app.emit(
                "workspace_compile_progress",
                CompileProgress::Connecting {
                    workspace: workspace_label.clone(),
                },
            );
            drive_compile_via_sidecar(
                app.clone(),
                host,
                port,
                path.clone(),
                workspace_label.clone(),
                url_alias.clone(),
                branch.clone(),
                cancel.clone(),
            )
            .await
        }
        None if cancel.is_cancelled() => Err(CompileDriveError::Cancelled),
        None => Err(CompileDriveError::Failed(
            "Engine sidecar did not finish booting within 10 seconds. \
             Check the logs (Help → Open Logs) — the bundled `root` binary \
             may be missing or the data directory may be unwritable."
                .to_string(),
        )),
    };

    let outcome = match first_attempt {
        Ok(result) => Ok(result),
        Err(CompileDriveError::Cancelled) => Err(CompileDriveError::Cancelled),
        Err(CompileDriveError::Failed(first_err)) => {
            // ── Auto-retry once when the breaker hasn't tripped ────
            tracing::warn!(
                workspace = %workspace_label,
                error = %first_err,
                "compile failed; checking auto-retry eligibility"
            );
            let _ = recovery_log::append(&RecoveryEvent::compile_failed(
                &url_alias,
                &first_err,
                0,
            ));

            let should_retry = {
                let mut rs = RestartState::load().unwrap_or_default();
                rs.prune_compile_attempts();
                rs.record_compile_failure(&url_alias, &first_err);
                if rs.compile_should_trip(&url_alias) {
                    let until = rs.trip_compile_breaker();
                    let count = rs.recent_compile_failure_count(&url_alias);
                    tracing::error!(
                        workspace = %url_alias,
                        attempts = count,
                        ?until,
                        "compile breaker tripped — disabling auto-retry until breaker clears"
                    );
                    let _ = recovery_log::append(&RecoveryEvent::compile_breaker_tripped(
                        &url_alias,
                        count as u32,
                        until,
                    ));
                    let _ = rs.save();
                    false
                } else {
                    let active = rs.compile_breaker_active();
                    let _ = rs.save();
                    !active && !cancel.is_cancelled()
                }
            };

            if !should_retry {
                Err(CompileDriveError::Failed(first_err))
            } else {
                let backoff = restart_state::compile_backoff_for_attempt(1);
                let _ = recovery_log::append(&RecoveryEvent::compile_retry_scheduled(
                    &url_alias,
                    1,
                    backoff,
                ));
                tracing::info!(
                    workspace = %url_alias,
                    backoff_ms = backoff.as_millis() as u64,
                    "compile auto-retry scheduled (attempt 2/{})",
                    COMPILE_AUTO_RETRY_LIMIT + 1
                );

                // Signal the retry to the UI so React can: (a) show
                // a "Retrying after: <error>" caption instead of
                // pretending nothing happened, and (b) reset its
                // monotonic-max bar tracker so the retry's fresh
                // Reading-5% tick doesn't look like an unexplained
                // backward jump.
                let _ = app.emit(
                    "workspace_compile_progress",
                    CompileProgress::Retrying {
                        workspace: workspace_label.clone(),
                        attempt: 1,
                        after_ms: backoff.as_millis() as u64,
                        first_error: first_err.clone(),
                    },
                );

                // Cancel-aware sleep — if user clicks Stop during
                // the backoff, bail immediately with Cancelled.
                tokio::select! {
                    _ = cancel.cancelled() => {
                        Err(CompileDriveError::Cancelled)
                    }
                    _ = tokio::time::sleep(backoff) => {
                        // Re-await sidecar handle in case it crashed and
                        // restarted between the two attempts.
                        let sidecar2 = await_sidecar_handle(&app, &workspace_label, &cancel).await;
                        match sidecar2 {
                            Some((host, port)) => {
                                // Same Connecting caption as the first
                                // attempt — the retry's HTTP POST +
                                // server setup window deserves the
                                // same honest signal.
                                let _ = app.emit(
                                    "workspace_compile_progress",
                                    CompileProgress::Connecting {
                                        workspace: workspace_label.clone(),
                                    },
                                );
                                let retry_outcome = drive_compile_via_sidecar(
                                    app.clone(),
                                    host,
                                    port,
                                    path.clone(),
                                    workspace_label.clone(),
                                    url_alias.clone(),
                                    branch.clone(),
                                    cancel.clone(),
                                )
                                .await;
                                if let Ok(ref _result) = retry_outcome {
                                    let _ = recovery_log::append(
                                        &RecoveryEvent::compile_recovered(&url_alias, 1),
                                    );
                                    tracing::info!(
                                        workspace = %url_alias,
                                        "compile auto-retry succeeded"
                                    );
                                }
                                retry_outcome
                            }
                            None if cancel.is_cancelled() => {
                                Err(CompileDriveError::Cancelled)
                            }
                            None => Err(CompileDriveError::Failed(
                                "Engine sidecar did not finish booting within 10 seconds (retry). \
                                 Check Help → Open Logs."
                                    .to_string(),
                            )),
                        }
                    }
                }
            }
        }
    };

    // ── Emit the terminal Tauri event + record outcome ─────────────
    match outcome {
        Ok(result) => {
            // Record success — clears the workspace's failure history
            // so a previously-flaky provider doesn't keep counting
            // toward the next breaker trip.
            {
                let mut rs = RestartState::load().unwrap_or_default();
                rs.prune_compile_attempts();
                rs.record_compile_success(&url_alias);
                let _ = rs.save();
            }
            let cache_dirty = result.cache_dirty;
            let _ = app.emit(
                "workspace_compile_progress",
                CompileProgress::Done {
                    files_parsed: result.files_parsed,
                    claims: result.claims_count,
                    entities: result.entities_count,
                    relations: result.relations_count,
                    contradictions: result.contradictions_count,
                    artifacts: result.artifacts_count,
                    health_score: result.health_score,
                    cache_dirty,
                    failed_batches: result.failed_batches,
                    failed_chunk_ranges: result.failed_chunk_ranges.clone(),
                    incremental_summary: Some(result.incremental_summary.clone()),
                },
            );
            emit_workspaces_changed(&app);
        }
        Err(CompileDriveError::Cancelled) => {
            // Cancellations are recorded but DO NOT contribute to
            // the breaker — they're user-initiated, not a sign of a
            // deterministically broken pipeline.
            {
                let mut rs = RestartState::load().unwrap_or_default();
                rs.prune_compile_attempts();
                rs.record_compile_cancellation(&url_alias);
                let _ = rs.save();
            }
            let _ = app.emit("workspace_compile_progress", CompileProgress::Cancelled);
            emit_workspaces_changed(&app);
        }
        Err(CompileDriveError::Failed(msg)) => {
            // The failure has already been recorded into
            // RestartState above (in the auto-retry decision block),
            // so we don't double-record here. We just emit the
            // failure event for the UI.
            let _ = app.emit(
                "workspace_compile_progress",
                CompileProgress::Failed { error: msg },
            );
            emit_workspaces_changed(&app);
        }
    }

    // Compile is over — clear the active-compile slot.
    let state = app.state::<AppState>();
    let mut guard = state.active_compile.lock().await;
    *guard = None;
}

/// Wait for the sidecar's `host:port` to appear in `AppState.sidecar`.
/// Bounded by `SIDECAR_BOOT_MAX_ATTEMPTS * 500ms` (10 s). Emits a
/// single `Booting` Tauri event on first miss. Cancel-aware: a Stop
/// click during the wait bails immediately.
async fn await_sidecar_handle(
    app: &AppHandle,
    workspace_label: &str,
    cancel: &CancellationToken,
) -> Option<(String, u16)> {
    let state = app.state::<AppState>();
    let mut emitted_booting = false;
    for _attempt in 0u32..SIDECAR_BOOT_MAX_ATTEMPTS {
        {
            let guard = state.sidecar.lock().await;
            if let Some(h) = guard.as_ref() {
                return Some((h.host.clone(), h.port));
            }
        }
        if !emitted_booting {
            emitted_booting = true;
            tracing::info!(
                workspace = %workspace_label,
                "sidecar handle not yet available — waiting for sidecar to finish booting"
            );
            let _ = app.emit(
                "workspace_compile_progress",
                CompileProgress::Booting {
                    workspace: workspace_label.to_string(),
                },
            );
        }
        if cancel.is_cancelled() {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    None
}

/// Outcome distinct from a raw `Result<PipelineResult, String>` so the
/// dispatcher can differentiate user-initiated cancellation (mapped to
/// the neutral `Cancelled` UI state) from a real pipeline failure.
enum CompileDriveError {
    Cancelled,
    Failed(String),
}

// Stream A — `drive_compile_in_process` removed. Cortex Protocol
// rule "Compile runs in the sidecar, not in the desktop process"
// forbids the in-process fallback. When the sidecar is unavailable
// the dispatcher emits a clear "Engine sidecar did not finish booting"
// error rather than silently falling back to writing graph.db from
// the desktop process.

/// Sidecar pipeline driver — POST `/api/v1/ws/{ws}/compile/stream`,
/// parse the SSE stream, fan progress out to the UI, and yield the
/// final `PipelineResult`.  The same `CancellationToken` plumbed
/// through `AppState.active_compile` is honoured here: when tripped,
/// we drop the response body (which trips the server-side DropGuard
/// that owns the pipeline's cancel token) and surface
/// `CompileDriveError::Cancelled` to the dispatcher.
/// How many times to retry the initial `POST /compile/stream` on
/// `reqwest` connection error before declaring the sidecar
/// unreachable. Each retry sleeps 2 s, **cancel-aware** so a Stop
/// click during the retry window doesn't block the UI.
const MAX_COMPILE_RETRIES: u8 = 5;

/// Stall watchdog window — if no SSE event arrives in this duration
/// (no `progress` tick, no `done`/`failed`/`cancelled` terminator,
/// not even the server's `keep-alive` comment), declare the stream
/// stalled. The server emits a `keep-alive` every 15 s
/// (`rest.rs::compile_stream`), so 60 s is 4× headroom on the
/// quietest expected interval; a stream that's been silent that
/// long has wedged on either the server or the network and the user
/// shouldn't sit forever staring at a frozen progress bar.
const SSE_STALL_WATCHDOG: Duration = Duration::from_secs(60);

async fn drive_compile_via_sidecar(
    app: AppHandle,
    host: String,
    port: u16,
    path: PathBuf,
    label: String,
    url_alias: String,
    branch: Option<String>,
    cancel: CancellationToken,
) -> Result<PipelineResult, CompileDriveError> {
    use eventsource_stream::Eventsource;
    use futures::StreamExt;

    // Dynamic workspace alias — pre-fix this was hardcoded `"desktop"`.
    // When the sidecar didn't mount a workspace under that name,
    // every compile request failed with the engine's
    // `WorkspaceNotMounted` error envelope. Now we route through the
    // registered workspace name (or `"_"` placeholder if the target
    // wasn't in the registry — the engine then matches by
    // `root_path` from the body).
    let url = format!("http://{host}:{port}/api/v1/ws/{url_alias}/compile/stream");
    let body = serde_json::json!({
        "root_path": path.display().to_string(),
        "branch": branch,
        "no_rooting": false,
    });

    let client = reqwest::Client::new();
    // Per-request timeout is intentionally absent on the streaming
    // body — compiles legitimately run for minutes on first-time
    // large workspaces. The stall watchdog below bounds the gap
    // BETWEEN events instead, which is the honest property we
    // actually want.
    //
    // Initial-connect retry: the sidecar's readiness probe in
    // `spawn()` returns OK as soon as axum starts — before all
    // workspace mounts complete. Transient `ECONNREFUSED` /
    // `ECONNRESET` errors land here and we retry with a fixed 2 s
    // backoff. **Cancel-aware**: pre-fix a Stop click during this
    // retry loop was silently ignored for up to 10 s
    // (`MAX_COMPILE_RETRIES * 2 s`).
    let resp = {
        let mut last_err: Option<reqwest::Error> = None;
        let mut result = None;
        for attempt in 0u8..MAX_COMPILE_RETRIES {
            if attempt > 0 {
                tracing::warn!(
                    attempt,
                    url = %url,
                    error = %last_err.as_ref().unwrap(),
                    "sidecar compile request failed, retrying in 2s"
                );
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(CompileDriveError::Cancelled);
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }
            }
            // The request itself races against cancellation too —
            // pre-fix a wedged sidecar (slow TLS handshake, slow
            // workspace mount) could swallow up to ~30 s of "Stop
            // does nothing" per attempt.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(CompileDriveError::Cancelled);
                }
                req = client.post(&url).json(&body).send() => {
                    match req {
                        Ok(r) => {
                            result = Some(r);
                            break;
                        }
                        Err(e) => {
                            last_err = Some(e);
                        }
                    }
                }
            }
        }
        match result {
            Some(r) => r,
            None => {
                return Err(CompileDriveError::Failed(format!(
                    "sidecar compile request failed after {MAX_COMPILE_RETRIES} attempts: {}",
                    last_err.unwrap()
                )));
            }
        }
    };

    if !resp.status().is_success() {
        // Pull the body so the caller sees the engine's own error
        // envelope (`{"ok":false,"error":{"code":...}}`) rather than
        // the bare HTTP status code.
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(CompileDriveError::Failed(format!(
            "sidecar compile returned HTTP {status}: {body}"
        )));
    }

    let mut stream = resp.bytes_stream().eventsource();
    let mut final_result: Option<PipelineResult> = None;
    let mut error: Option<String> = None;
    let mut cancelled = false;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Drop the stream → reqwest closes the body → axum
                // detects the disconnect → server-side DropGuard
                // fires → pipeline exits. We don't wait for the
                // `cancelled` SSE terminator here because the user
                // wants the UI to react immediately.
                cancelled = true;
                break;
            }
            // Stall watchdog: bound the gap BETWEEN events. The
            // server's `KeepAlive::new().interval(15s).text("keep-alive")`
            // (rest.rs::compile_stream) emits SSE comments that the
            // eventsource decoder surfaces as `Ok(event)` with empty
            // `event` field — so 60 s of silence means BOTH the
            // server stopped emitting progress AND the keep-alive
            // timer somehow lapsed. That's a wedged stream; fail
            // loud rather than block forever.
            ev = tokio::time::timeout(SSE_STALL_WATCHDOG, stream.next()) => {
                match ev {
                    Err(_) => {
                        error = Some(format!(
                            "sidecar SSE stream stalled — no event for {}s",
                            SSE_STALL_WATCHDOG.as_secs()
                        ));
                        break;
                    }
                    Ok(None) => break,
                    Ok(Some(Ok(event))) => {
                        match event.event.as_str() {
                            "progress" => {
                                match serde_json::from_str::<ProgressEvent>(&event.data) {
                                    Ok(pe) => {
                                        if let Some(payload) = map_progress(&label, pe) {
                                            let _ = app.emit(
                                                "workspace_compile_progress",
                                                payload,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            data = %event.data,
                                            "failed to deserialise progress event from sidecar — skipping"
                                        );
                                    }
                                }
                            }
                            "done" => {
                                match serde_json::from_str::<PipelineResult>(&event.data) {
                                    Ok(pr) => final_result = Some(pr),
                                    Err(e) => {
                                        error = Some(format!(
                                            "sidecar emitted malformed done payload: {e}"
                                        ));
                                    }
                                }
                            }
                            "cancelled" => {
                                cancelled = true;
                            }
                            "failed" => {
                                let msg = serde_json::from_str::<serde_json::Value>(
                                    &event.data,
                                )
                                .ok()
                                .and_then(|v| {
                                    v.get("error")
                                        .and_then(|e| e.as_str())
                                        .map(|s| s.to_string())
                                })
                                .unwrap_or_else(|| {
                                    "sidecar compile failed (no error message)".to_string()
                                });
                                error = Some(msg);
                            }
                            // Keep-alive comments and unknown event
                            // types are ignored — keeps the stream
                            // forward-compatible if the engine ever
                            // adds new event variants.
                            _ => {}
                        }
                    }
                    Ok(Some(Err(e))) => {
                        error = Some(format!("SSE stream error: {e}"));
                        break;
                    }
                }
            }
        }
    }

    if cancelled {
        return Err(CompileDriveError::Cancelled);
    }
    if let Some(err) = error {
        return Err(CompileDriveError::Failed(err));
    }
    final_result.ok_or_else(|| {
        CompileDriveError::Failed("sidecar SSE stream ended without `done` event".to_string())
    })
}

/// Stop an in-progress compile.  Returns `true` when a compile was
/// active and the cancellation token was tripped; `false` when no
/// compile is running.  The actual abort happens at the next phase
/// boundary in the pipeline (typically <1 s) — the spawned task then
/// emits `CompileProgress::Cancelled` and clears `active_compile`.
#[tauri::command]
pub async fn workspace_compile_stop(app: AppHandle) -> Result<bool, String> {
    let state = app.state::<AppState>();
    let guard = state.active_compile.lock().await;
    match guard.as_ref() {
        Some(handle) => {
            tracing::info!(
                workspace = %handle.workspace_label,
                "user requested compile stop — tripping cancellation token"
            );
            handle.cancel.cancel();
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Lightweight read-only status: which workspace (if any) is being
/// compiled right now.  React reads this from the Right-Rail Compile
/// button's onClick handler so a stale-slot mismatch surfaces as an
/// inline warning instead of a hard-error toast.
///
/// Field name `running` matches the TypeScript `CompileStatus`
/// binding in `apps/thinkingroot-desktop/ui/src/lib/tauri.ts:489`.
/// Pre-fix this struct shipped `active: bool` (server) vs
/// `running: bool` (TS), so every UI poll returned `running:
/// undefined` and the pre-flight check silently no-op'd.
#[derive(Debug, Serialize)]
pub struct CompileStatus {
    pub running: bool,
    pub workspace: Option<String>,
}

#[tauri::command]
pub async fn workspace_compile_status(app: AppHandle) -> Result<CompileStatus, String> {
    let state = app.state::<AppState>();
    let guard = state.active_compile.lock().await;
    Ok(match guard.as_ref() {
        Some(h) => CompileStatus {
            running: true,
            workspace: Some(h.workspace_label.clone()),
        },
        None => CompileStatus {
            running: false,
            workspace: None,
        },
    })
}

/// `GET /api/v1/ws/{ws}/readme` proxy. Returns the engine-canonical
/// workspace README markdown (auto-synthesised by Phase 10 of the
/// compile pipeline). Backs the desktop's right-rail Readme tab.
/// Empty string when the workspace has not been compiled yet — the
/// view renders an "no README yet" empty state, never a fabricated
/// placeholder (CLAUDE.md honesty rule §1).
#[derive(Debug, serde::Deserialize)]
struct ReadmeEnvelope {
    readme: String,
}

#[tauri::command]
pub async fn workspace_readme(app: AppHandle) -> Result<String, String> {
    use crate::commands::sidecar_client::SidecarClient;
    let sc = SidecarClient::ensure_active(&app).await?;
    let path = format!("/api/v1/ws/{}/readme", sc.workspace);
    let env: ReadmeEnvelope = sc.get(&path).await?;
    Ok(env.readme)
}

fn map_progress(_workspace: &str, event: ProgressEvent) -> Option<CompileProgress> {
    match event {
        // The unified ticker event — single canonical path. Mapped
        // first so it's the cheapest match in the hot loop (one tick
        // every 250 ms, vs the per-row legacy events below).
        ProgressEvent::CompileTick(tick) => Some(CompileProgress::Tick {
            step: match tick.step {
                thinkingroot_core::CompileStep::Reading => "reading".into(),
                thinkingroot_core::CompileStep::Extracting => "extracting".into(),
                thinkingroot_core::CompileStep::Linking => "linking".into(),
                thinkingroot_core::CompileStep::Persisting => "persisting".into(),
                thinkingroot_core::CompileStep::Packing => "packing".into(),
            },
            step_label: tick.step.label().to_string(),
            done: tick.done,
            total: tick.total,
            step_elapsed_ms: tick.step_elapsed_ms,
            total_elapsed_ms: tick.total_elapsed_ms,
            eta_ms: tick.eta_ms,
            detail: tick.detail,
        }),
        ProgressEvent::DiffStart => Some(CompileProgress::DiffStart),
        ProgressEvent::DiffComplete {
            changed,
            unchanged,
            deleted,
        } => Some(CompileProgress::DiffComplete {
            changed,
            unchanged,
            deleted,
        }),
        ProgressEvent::ParseComplete { files } => Some(CompileProgress::ParseComplete { files }),
        ProgressEvent::ExtractionStart {
            total_chunks,
            total_batches,
            ..
        } => Some(CompileProgress::ExtractionStart {
            total_chunks,
            total_batches,
        }),
        ProgressEvent::ChunkDone { done, total, .. } => {
            Some(CompileProgress::ExtractionProgress { done, total })
        }
        ProgressEvent::ExtractionComplete {
            claims, entities, ..
        } => Some(CompileProgress::ExtractionComplete { claims, entities }),
        ProgressEvent::ExtractionPartial {
            failed_batches,
            failed_chunk_ranges,
        } => Some(CompileProgress::ExtractionPartial {
            failed_batches,
            failed_chunk_ranges,
        }),
        ProgressEvent::GroundingStart {
            llm_claims,
            structural_claims,
        } => Some(CompileProgress::GroundingStart {
            llm_claims,
            structural_claims,
        }),
        ProgressEvent::GroundingProgress { done, total } => {
            Some(CompileProgress::GroundingProgress { done, total })
        }
        ProgressEvent::GroundingDone { accepted, rejected } => {
            Some(CompileProgress::GroundingDone { accepted, rejected })
        }
        ProgressEvent::FingerprintDone {
            truly_changed,
            cutoffs,
        } => Some(CompileProgress::FingerprintDone {
            truly_changed,
            cutoffs,
        }),
        ProgressEvent::RootingStart { candidates } => {
            Some(CompileProgress::RootingStart { candidates })
        }
        ProgressEvent::RootingProgress { done, total } => {
            Some(CompileProgress::RootingProgress { done, total })
        }
        ProgressEvent::RootingDone {
            rooted,
            attested,
            quarantined,
            rejected,
        } => Some(CompileProgress::RootingDone {
            rooted,
            attested,
            quarantined,
            rejected,
        }),
        ProgressEvent::LinkingStart { total_entities } => {
            Some(CompileProgress::LinkingStart { total_entities })
        }
        ProgressEvent::EntityResolved { done, total } => {
            Some(CompileProgress::LinkingProgress { done, total })
        }
        ProgressEvent::VectorProgress { done, total } => {
            Some(CompileProgress::VectorProgress { done, total })
        }
        ProgressEvent::VectorUpdateDone {
            entities_indexed,
            claims_indexed,
        } => Some(CompileProgress::VectorUpdateDone {
            entities_indexed,
            claims_indexed,
        }),
        ProgressEvent::CompilationProgress { done, total } => {
            Some(CompileProgress::CompilationProgress { done, total })
        }
        ProgressEvent::CompilationDone { artifacts } => {
            Some(CompileProgress::CompilationDone { artifacts })
        }
        ProgressEvent::VerificationDone { health } => {
            Some(CompileProgress::VerificationDone { health })
        }
        ProgressEvent::PhaseDone { name, elapsed_ms } => {
            Some(CompileProgress::PhaseDone { name, elapsed_ms })
        }
        // Remaining events are either internal timing markers or high-frequency
        // signals that don't add user-facing value in the desktop progress UI.
        _ => None,
    }
}
