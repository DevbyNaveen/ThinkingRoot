//! Memory commands.
//!
//! | Command       | Backend call                                      |
//! |---------------|---------------------------------------------------|
//! | `memory_list` | `QueryEngine::list_claims(ws, filter)`            |
//! | `brain_load`  | Single round-trip fanning out claims + entities + |
//! |               | relations + rooted-claim ids for the Brain view.  |
//!
//! Both reuse the lazily-mounted [`QueryEngine`] held in [`AppState`].
//! The first call in a fresh session pays the mount cost; subsequent
//! calls reuse the cached handle until the workspace pointer changes.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use thinkingroot_serve::engine::{ClaimFilter, ClaimInfo, EntityInfo, QueryEngine};
use tokio::sync::RwLock;

use crate::state::{AppState, MountedMemory};

#[derive(Debug, Serialize, Clone)]
pub struct ClaimRow {
    pub id: String,
    pub tier: String,
    pub confidence: f64,
    pub statement: String,
    pub source: String,
    pub claim_type: String,
}

impl From<ClaimInfo> for ClaimRow {
    fn from(c: ClaimInfo) -> Self {
        Self {
            id: c.id,
            tier: "unknown".to_string(),
            confidence: c.confidence,
            statement: c.statement,
            source: c.source_uri,
            claim_type: c.claim_type,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct EntityRow {
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
}

impl From<EntityInfo> for EntityRow {
    fn from(e: EntityInfo) -> Self {
        Self {
            name: e.name,
            entity_type: e.entity_type,
            claim_count: e.claim_count,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct RelationEdge {
    pub source: String,
    pub target: String,
    pub relation_type: String,
    pub strength: f64,
}

/// Combined payload for the Brain view — one round trip populates
/// both the d3 graph and the virtualized claim table.
#[derive(Debug, Serialize, Clone)]
pub struct BrainSnapshot {
    pub claims: Vec<ClaimRow>,
    pub entities: Vec<EntityRow>,
    pub relations: Vec<RelationEdge>,
    pub rooted_ids: Vec<String>,
}

#[tauri::command]
pub async fn memory_list(
    app: AppHandle,
    filter: Option<String>,
) -> Result<Vec<ClaimRow>, String> {
    let (engine, ws) = mount_engine(&app).await.map_err(|e| e.to_string())?;
    load_claims(&engine, &ws, filter.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn brain_load(app: AppHandle) -> Result<BrainSnapshot, String> {
    let (engine, ws) = mount_engine(&app).await.map_err(|e| e.to_string())?;

    let engine_guard = engine.read().await;
    let claims = engine_guard
        .list_claims(
            &ws,
            ClaimFilter {
                claim_type: None,
                entity_name: None,
                min_confidence: None,
                limit: Some(500),
                offset: None,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let entities = engine_guard
        .list_entities(&ws)
        .await
        .map_err(|e| e.to_string())?;

    let relations = engine_guard
        .get_all_relations(&ws)
        .await
        .map_err(|e| e.to_string())?;

    let rooted = engine_guard
        .list_rooted_claims(&ws, None, None, None)
        .await
        .map(|rr| rr.into_iter().map(|c| c.id).collect::<Vec<_>>())
        .unwrap_or_default();
    drop(engine_guard);

    let rooted_set: std::collections::HashSet<&str> =
        rooted.iter().map(String::as_str).collect();

    let mut claim_rows: Vec<ClaimRow> = claims.into_iter().map(ClaimRow::from).collect();
    for row in &mut claim_rows {
        if rooted_set.contains(row.id.as_str()) {
            row.tier = "rooted".to_string();
        } else if row.confidence >= 0.7 {
            row.tier = "attested".to_string();
        } else {
            row.tier = "unknown".to_string();
        }
    }

    Ok(BrainSnapshot {
        claims: claim_rows,
        entities: entities.into_iter().map(EntityRow::from).collect(),
        relations: relations
            .into_iter()
            .map(|(src, tgt, ty, strength)| RelationEdge {
                source: src,
                target: tgt,
                relation_type: ty,
                strength,
            })
            .collect(),
        rooted_ids: rooted,
    })
}

/// Mount or reuse a [`QueryEngine`] for the configured workspace and
/// return a shared handle plus the workspace name. The engine is
/// constructed directly via `thinkingroot-serve` — no runtime wrapper
/// sits between the desktop and the engine.
async fn mount_engine(
    app: &AppHandle,
) -> anyhow::Result<(Arc<RwLock<QueryEngine>>, String)> {
    // Resolve the active workspace via the shared registry — the same
    // pointer the workspaces sidebar's "active" tick is keyed on.
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

async fn load_claims(
    engine: &Arc<RwLock<QueryEngine>>,
    workspace: &str,
    filter_type: Option<&str>,
) -> anyhow::Result<Vec<ClaimRow>> {
    let guard = engine.read().await;
    let claims = guard
        .list_claims(
            workspace,
            ClaimFilter {
                claim_type: filter_type.map(ToString::to_string),
                entity_name: None,
                min_confidence: None,
                limit: Some(500),
                offset: None,
            },
        )
        .await?;
    Ok(claims.into_iter().map(ClaimRow::from).collect())
}
