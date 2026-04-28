//! Privacy dashboard backend.
//!
//! Two commands back the dashboard:
//!
//! | Command           | Backend call                                        |
//! |-------------------|-----------------------------------------------------|
//! | `privacy_summary` | `QueryEngine::list_{sources,claims,entities}` ×3    |
//! | `privacy_forget`  | `QueryEngine::forget_source(ws, source_uri)`        |
//!
//! Both reuse the lazily-mounted [`QueryEngine`] held in [`AppState`]
//! (the same handle that backs the Brain view).
//!
//! Forgetting a source is a graph mutation: it removes the source row
//! plus every claim/entity-edge/vector/contradiction descendant, then
//! rebuilds the read cache so the dashboard immediately reflects the
//! redaction. There is no soft-delete tombstone — the row is gone.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use thinkingroot_serve::engine::{ClaimFilter, QueryEngine};
use tokio::sync::RwLock;

use crate::state::{AppState, MountedMemory};

/// One source listed in the dashboard. Mirrors
/// `thinkingroot_serve::engine::SourceInfo`.
#[derive(Debug, Serialize, Clone)]
pub struct PrivacySource {
    pub id: String,
    pub uri: String,
    pub source_type: String,
}

/// Aggregate counts shown above the table.
#[derive(Debug, Serialize, Clone)]
pub struct PrivacySummary {
    pub workspace: String,
    pub sources: Vec<PrivacySource>,
    pub source_count: usize,
    pub claim_count: usize,
    pub entity_count: usize,
}

/// Read counts + source list for the active workspace. Returns an
/// empty summary when no workspace is mounted (the dashboard renders
/// an honest "no workspace" state rather than fabricating numbers).
#[tauri::command]
pub async fn privacy_summary(app: AppHandle) -> Result<PrivacySummary, String> {
    let (engine, ws) = mount_engine(&app).await.map_err(|e| e.to_string())?;
    let guard = engine.read().await;

    let sources = guard
        .list_sources(&ws)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|s| PrivacySource {
            id: s.id,
            uri: s.uri,
            source_type: s.source_type,
        })
        .collect::<Vec<_>>();

    let claims = guard
        .list_claims(
            &ws,
            ClaimFilter {
                claim_type: None,
                entity_name: None,
                min_confidence: None,
                limit: None,
                offset: None,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let entities = guard
        .list_entities(&ws)
        .await
        .map_err(|e| e.to_string())?;

    Ok(PrivacySummary {
        workspace: ws,
        source_count: sources.len(),
        claim_count: claims.len(),
        entity_count: entities.len(),
        sources,
    })
}

/// Forget every claim/edge/vector descended from `source_uri`. Returns
/// the number of source rows removed (0 if no match).
#[tauri::command]
pub async fn privacy_forget(app: AppHandle, source_uri: String) -> Result<usize, String> {
    let (engine, ws) = mount_engine(&app).await.map_err(|e| e.to_string())?;
    let guard = engine.read().await;
    guard
        .forget_source(&ws, &source_uri)
        .await
        .map_err(|e| e.to_string())
}

async fn mount_engine(
    app: &AppHandle,
) -> anyhow::Result<(Arc<RwLock<QueryEngine>>, String)> {
    let registry = thinkingroot_core::WorkspaceRegistry::load()
        .map_err(|e| anyhow::anyhow!("load workspace registry: {e}"))?;
    let entry = registry
        .active_entry()
        .ok_or_else(|| anyhow::anyhow!("no active workspace selected"))?;
    let root_path: PathBuf = entry.path.clone();
    let workspace = entry.name.clone();

    let state = app.state::<AppState>();
    let mut guard = state.memory.lock().await;
    let needs_mount = match guard.as_ref() {
        Some(m) => m.root_path != root_path || m.workspace != workspace,
        None => true,
    };
    if needs_mount {
        let mut engine = QueryEngine::new();
        engine
            .mount(workspace.clone(), root_path.clone())
            .await
            .map_err(|e| anyhow::anyhow!("mount engine: {e}"))?;
        *guard = Some(MountedMemory {
            root_path: root_path.clone(),
            workspace: workspace.clone(),
            engine: Arc::new(RwLock::new(engine)),
        });
    }
    let engine = guard.as_ref().expect("just mounted").engine.clone();
    drop(guard);
    Ok((engine, workspace))
}
