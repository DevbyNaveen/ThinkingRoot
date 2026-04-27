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
use tauri::{AppHandle, Emitter};
use thinkingroot_core::{WorkspaceEntry, WorkspaceRegistry};
use thinkingroot_serve::pipeline::{ProgressEvent, run_pipeline};
use tokio::sync::mpsc;

use crate::config::AppConfig;

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
    let cfg = AppConfig::load().map_err(|e| e.to_string())?;
    let active_path = cfg.env_or("THINKINGROOT_WORKSPACE");
    Ok(registry
        .workspaces
        .iter()
        .map(|w| WorkspaceView {
            name: w.name.clone(),
            path: w.path.display().to_string(),
            port: w.port,
            compiled: w.path.join(".thinkingroot").join("graph.db").exists(),
            active: active_path
                .as_ref()
                .map(|p| PathBuf::from(p) == w.path)
                .unwrap_or(false),
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

    let cfg = AppConfig::load().map_err(|e| e.to_string())?;
    let active_path = cfg.env_or("THINKINGROOT_WORKSPACE");
    Ok(WorkspaceView {
        name,
        path: abs.display().to_string(),
        port,
        compiled: abs.join(".thinkingroot").join("graph.db").exists(),
        active: active_path
            .as_ref()
            .map(|p| PathBuf::from(p) == abs)
            .unwrap_or(false),
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

/// Mark a registered workspace as the one `chat_send` recalls from by
/// writing `THINKINGROOT_WORKSPACE` + `THINKINGROOT_WORKSPACE_NAME` into the
/// config file. Returns the resolved absolute path.
#[tauri::command]
pub fn workspace_set_active(args: WorkspaceSetActiveArgs) -> Result<String, String> {
    let registry = WorkspaceRegistry::load().map_err(|e| e.to_string())?;
    let entry = registry
        .workspaces
        .iter()
        .find(|w| w.name == args.name)
        .ok_or_else(|| format!("workspace `{}` not found", args.name))?;
    let abs = entry.path.display().to_string();

    write_config_keys(&[
        ("THINKINGROOT_WORKSPACE", &abs),
        ("THINKINGROOT_WORKSPACE_NAME", &entry.name),
    ])?;
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
    },
    Failed {
        error: String,
    },
}

/// Kick off a compile in a background tokio task. Returns immediately;
/// progress flows via `workspace_compile_progress` Tauri events keyed
/// to the workspace path so the UI can correlate when multiple compiles
/// run concurrently.
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

    let app_for_task = app.clone();
    let path_for_task = path.clone();
    let label_for_task = workspace_label.clone();

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

        let outcome = run_pipeline(&path_for_task, branch.as_deref(), Some(tx)).await;

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
                    },
                );
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
    });

    Ok(workspace_label)
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

/// Append/replace keys in the config file via the same path
/// `commands::settings::config_write` uses, but without going through
/// the Tauri IPC boundary. The settings module owns the canonical
/// implementation; we duplicate the minimal write logic here only to
/// avoid a circular module dep when settings::config_write itself
/// would call back into us in the future.
fn write_config_keys(pairs: &[(&str, &str)]) -> Result<(), String> {
    use std::fs;
    let path = resolve_config_path().ok_or_else(|| "no HOME directory set".to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create config dir: {e}"))?;
    }
    let mut entries: std::collections::BTreeMap<String, toml::Value> = if path.exists() {
        let raw = fs::read_to_string(&path).map_err(|e| format!("read config: {e}"))?;
        toml::from_str(&raw).map_err(|e| format!("parse config: {e}"))?
    } else {
        Default::default()
    };
    for (k, v) in pairs {
        entries.insert((*k).to_string(), toml::Value::String((*v).to_string()));
    }
    let serialized = toml::to_string_pretty(&entries).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, serialized).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("THINKINGROOT_DESKTOP_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("thinkingroot").join("desktop.toml"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("thinkingroot")
            .join("desktop.toml"),
    )
}
