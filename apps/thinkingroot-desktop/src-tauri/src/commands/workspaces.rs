//! Workspace commands — lifecycle for the **Satellites** surface.
//!
//! A *workspace* is a folder ThinkingRoot has compiled into a queryable
//! knowledge graph. The CLI manages these via `tr workspace add` /
//! `tr compile`; the desktop app exposes the same operations through
//! Tauri commands so non-CLI users can:
//!
//! 1. Register an existing compiled folder ([`workspace_add`])
//! 2. Compile a fresh folder from a file picker ([`workspace_compile`])
//! 3. List, set-active, or remove registered workspaces
//!
//! Compilation streams live progress as `workspace_compile_progress`
//! events — driven by `thinkingroot_serve::pipeline::ProgressEvent`,
//! which is what the CLI's indicatif bars consume. The webview can
//! render the same progression with no schema change.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};
use thinkingroot_serve::pipeline::{ProgressEvent, run_pipeline_with_cancel};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::state::{AppState, CompileHandle};

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
pub fn workspace_add(args: WorkspaceAddArgs) -> Result<WorkspaceView, String> {
    let abs = std::fs::canonicalize(&args.path)
        .map_err(|e| format!("path not found: {} ({e})", args.path))?;
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
pub fn workspace_remove(args: WorkspaceRemoveArgs) -> Result<bool, String> {
    let mut registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let removed = registry.remove(&args.name);
    if removed {
        registry.save().map_err(|e| e.to_string())?;
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
#[tauri::command]
pub fn workspace_set_active(args: WorkspaceSetActiveArgs) -> Result<String, String> {
    let mut registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let abs = registry
        .workspaces
        .iter()
        .find(|w| w.name == args.name)
        .map(|e| e.path.display().to_string())
        .ok_or_else(|| format!("workspace `{}` not found", args.name))?;
    registry
        .set_active(&args.name)
        .map_err(|e| e.to_string())?;
    registry.save().map_err(|e| e.to_string())?;
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
    GroundingProgress {
        done: usize,
        total: usize,
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
#[tauri::command]
pub async fn workspace_compile(
    app: AppHandle,
    args: WorkspaceCompileArgs,
) -> Result<String, String> {
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let path: PathBuf = match registry.workspaces.iter().find(|w| w.name == args.target) {
        Some(e) => e.path.clone(),
        None => std::fs::canonicalize(&args.target)
            .map_err(|e| format!("not a registered workspace and not a path: {} ({e})", args.target))?,
    };
    let workspace_label = path.display().to_string();
    let branch = args.branch;

    // Refuse to start a second compile on top of an active one — the
    // pipeline doesn't itself coordinate two runs against the same
    // workspace, so racing CozoDB writes would just confuse the user.
    {
        let state = app.state::<AppState>();
        let guard = state.active_compile.lock().await;
        if let Some(handle) = guard.as_ref() {
            return Err(format!(
                "compile already in progress for {}; call workspace_compile_stop first",
                handle.workspace_label
            ));
        }
    }

    // Register the compile so workspace_compile_stop can find the
    // cancellation token.  Must be in place BEFORE we spawn the task
    // so a fast UI Stop click can't miss the registration.
    let cancel = CancellationToken::new();
    {
        let state = app.state::<AppState>();
        let mut guard = state.active_compile.lock().await;
        *guard = Some(CompileHandle {
            workspace_label: workspace_label.clone(),
            cancel: cancel.clone(),
        });
    }

    let app_for_task = app.clone();
    let path_for_task = path.clone();
    let label_for_task = workspace_label.clone();
    let cancel_for_task = cancel.clone();

    tokio::spawn(async move {
        let _ = app_for_task.emit(
            "workspace_compile_progress",
            CompileProgress::Started {
                workspace: label_for_task.clone(),
            },
        );

        let (tx, mut rx) = mpsc::unbounded_channel::<ProgressEvent>();
        let app_for_progress = app_for_task.clone();
        let label_for_progress = label_for_task.clone();
        let pump = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let mapped = map_progress(&label_for_progress, event);
                if let Some(payload) = mapped {
                    let _ = app_for_progress.emit("workspace_compile_progress", payload);
                }
            }
        });

        let outcome = run_pipeline_with_cancel(
            &path_for_task,
            branch.as_deref(),
            Some(tx),
            cancel_for_task,
        )
        .await;

        // Drop the sender side by waiting for the pump to drain — we
        // already moved `tx` into `run_pipeline`, so the channel closes
        // when run_pipeline returns. The pump task exits its loop when
        // recv() yields None.
        let _ = pump.await;

        match outcome {
            Ok(result) => {
                let _ = app_for_task.emit(
                    "workspace_compile_progress",
                    CompileProgress::Done {
                        files_parsed: result.files_parsed,
                        claims: result.claims_count,
                        entities: result.entities_count,
                        relations: result.relations_count,
                        contradictions: result.contradictions_count,
                        artifacts: result.artifacts_count,
                        health_score: result.health_score,
                        cache_dirty: result.cache_dirty,
                        failed_batches: result.failed_batches,
                        failed_chunk_ranges: result.failed_chunk_ranges.clone(),
                    },
                );
            }
            Err(e) if e.is_cancelled() => {
                let _ = app_for_task
                    .emit("workspace_compile_progress", CompileProgress::Cancelled);
            }
            Err(e) => {
                let _ = app_for_task.emit(
                    "workspace_compile_progress",
                    CompileProgress::Failed {
                        error: e.to_string(),
                    },
                );
            }
        }

        // Compile is over — clear the active-compile slot so a
        // subsequent click of "Compile" can start fresh.  Done in a
        // separate scope so the lock is held only while taking the
        // handle.
        let state = app_for_task.state::<AppState>();
        let mut guard = state.active_compile.lock().await;
        *guard = None;
    });

    Ok(workspace_label)
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
/// compiled right now.  React polls this from a long-lived "Compile"
/// modal so the Stop button can stay disabled when nothing is running
/// without depending on event ordering.
#[derive(Debug, Serialize)]
pub struct CompileStatus {
    pub active: bool,
    pub workspace: Option<String>,
}

#[tauri::command]
pub async fn workspace_compile_status(app: AppHandle) -> Result<CompileStatus, String> {
    let state = app.state::<AppState>();
    let guard = state.active_compile.lock().await;
    Ok(match guard.as_ref() {
        Some(h) => CompileStatus {
            active: true,
            workspace: Some(h.workspace_label.clone()),
        },
        None => CompileStatus {
            active: false,
            workspace: None,
        },
    })
}

fn map_progress(_workspace: &str, event: ProgressEvent) -> Option<CompileProgress> {
    match event {
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
        ProgressEvent::GroundingProgress { done, total } => {
            Some(CompileProgress::GroundingProgress { done, total })
        }
        ProgressEvent::LinkingStart { total_entities } => {
            Some(CompileProgress::LinkingStart { total_entities })
        }
        ProgressEvent::EntityResolved { done, total } => {
            Some(CompileProgress::LinkingProgress { done, total })
        }
        ProgressEvent::VectorProgress { done, total } => {
            Some(CompileProgress::VectorProgress { done, total })
        }
        // Pipeline emits a richer event set than we surface to the UI
        // (GroundingStart, ExtractionBatchStart, CompilationProgress,
        // VerificationDone, RootingProgress, …). We drop them silently
        // rather than spamming the webview; the `Done` event is
        // constructed in `workspace_compile` from `PipelineResult`.
        _ => None,
    }
}

