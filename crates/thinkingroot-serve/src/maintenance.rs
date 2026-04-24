//! Background maintenance tasks for long-running thinkingroot servers.
//!
//! Stream-branch cleanup: when `streams.auto_session_branch = true`, each
//! MCP session spawns a `stream/{session_id}` branch (see mcp/mod.rs:131).
//! Without this cleanup task these accumulate forever — the session store
//! only lives in memory while the session is active, but the branch on disk
//! outlives the session.
//!
//! Safety contract: branches that have *agent contributes* (identified by
//! `mcp://agent/*` sources present in their graph) are never hard-deleted
//! from the cleanup path. The most `cleanup_action = "purge"` will do in
//! that case is downgrade to a soft abandon + WARN log. Losing agent work
//! silently would be a severe failure mode.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::branch_cache::BranchEngineCache;
use crate::intelligence::session::SessionStore;
use thinkingroot_core::BranchStatus;

/// Spawn the stream-branch cleanup task.
///
/// Returns a `JoinHandle` the caller must keep alive (and may abort at
/// shutdown). If `cleanup_enabled = false`, this returns a no-op handle.
pub fn spawn_stream_cleanup(
    sessions: SessionStore,
    workspace_root: PathBuf,
    cfg: thinkingroot_core::config::StreamsConfig,
    branch_engines: Option<Arc<BranchEngineCache>>,
) -> JoinHandle<()> {
    if !cfg.cleanup_enabled {
        return tokio::spawn(async {});
    }

    let interval = Duration::from_secs(cfg.cleanup_interval_secs.max(1));
    let idle_secs = cfg.cleanup_idle_secs;
    let action = cfg.cleanup_action.clone();

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the immediate first tick so we don't scan at t=0 before any
        // sessions exist; interval::tick() fires once immediately by default.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        loop {
            ticker.tick().await;
            if let Err(e) = cleanup_once(
                &sessions,
                &workspace_root,
                idle_secs,
                &action,
                branch_engines.as_deref(),
            )
            .await
            {
                tracing::warn!("stream cleanup tick failed (non-fatal): {e}");
            }
        }
    })
}

/// Single cleanup pass. Exposed for tests.
///
/// When `branch_engines` is provided, the cache is invalidated before each
/// disposition (soft-delete or purge). Tests that don't need cache
/// coordination can pass `None`.
pub async fn cleanup_once(
    sessions: &SessionStore,
    workspace_root: &std::path::Path,
    idle_secs: u64,
    action: &str,
    branch_engines: Option<&BranchEngineCache>,
) -> thinkingroot_core::Result<CleanupStats> {
    let mut stats = CleanupStats::default();

    let branches = thinkingroot_branch::list_branches(workspace_root)
        .map_err(|e| thinkingroot_core::Error::GraphStorage(format!("list_branches: {e}")))?;

    for branch in branches {
        // Only consider Active stream branches. Merged/Abandoned are skipped.
        if !matches!(branch.status, BranchStatus::Active) {
            continue;
        }
        let Some(session_id) = branch.name.strip_prefix("stream/") else {
            continue;
        };
        stats.branches_scanned += 1;

        // Active (in-memory) and not idle past threshold? Keep it.
        let session_active = {
            let store = sessions.lock().await;
            store
                .get(session_id)
                .map(|s| s.idle_secs() < idle_secs)
                .unwrap_or(false)
        };
        if session_active {
            stats.kept += 1;
            continue;
        }

        // Safety: if the branch holds agent contributes, never hard-purge.
        let has_contributes =
            branch_has_agent_contributes(workspace_root, &branch.name).unwrap_or(false); // on error, assume has contributes (safe default)

        let effective_action = if action == "purge" && has_contributes {
            tracing::warn!(
                branch = %branch.name,
                session_id,
                "stream_cleanup: downgrading purge → abandon (branch has agent contributes)"
            );
            "abandon"
        } else {
            action
        };

        // Drop the cached branch handle *before* disposing the branch on
        // disk. Holding the handle open during purge would leave the
        // GraphStore referencing a now-deleted file; new readers landing
        // between cleanup tick and their own invalidation would get a
        // broken handle until TTL expiry.
        if let Some(cache) = branch_engines {
            cache.invalidate(workspace_root, &branch.name).await;
        }
        // `branch_has_agent_contributes` above opened a fresh GraphStore
        // without going through the cache (it's a low-frequency cleanup
        // read path, not a hot agent path). That handle has already been
        // dropped by the time we reach the action below, so the purge
        // operation does not race a live DbInstance.

        match effective_action {
            "purge" => match thinkingroot_branch::purge_branch(workspace_root, &branch.name) {
                Ok(_) => {
                    stats.purged += 1;
                    tracing::info!(branch = %branch.name, session_id, "stream_cleanup: purged");
                }
                Err(e) => {
                    tracing::warn!(
                        branch = %branch.name,
                        "stream_cleanup: purge failed: {e}"
                    );
                }
            },
            _ => {
                // "abandon" (default) and any unknown value both soft-delete.
                match thinkingroot_branch::delete_branch(workspace_root, &branch.name) {
                    Ok(_) => {
                        stats.abandoned += 1;
                        tracing::info!(branch = %branch.name, session_id, "stream_cleanup: abandoned");
                    }
                    Err(e) => {
                        tracing::warn!(
                            branch = %branch.name,
                            "stream_cleanup: abandon failed: {e}"
                        );
                    }
                }
            }
        }
    }

    tracing::info!(
        target: "stream_cleanup",
        branches_scanned = stats.branches_scanned,
        abandoned = stats.abandoned,
        purged = stats.purged,
        kept = stats.kept,
        "stream cleanup tick complete"
    );
    Ok(stats)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CleanupStats {
    pub branches_scanned: usize,
    pub abandoned: usize,
    pub purged: usize,
    pub kept: usize,
}

/// Detect whether a branch has agent-contributed claims. Agent contributes
/// create a synthetic source with URI `mcp://agent/{session_id}` at
/// engine.rs:1493; their presence means the branch holds in-flight work.
fn branch_has_agent_contributes(
    workspace_root: &std::path::Path,
    branch_name: &str,
) -> thinkingroot_core::Result<bool> {
    use thinkingroot_branch::snapshot::resolve_data_dir;
    use thinkingroot_graph::graph::GraphStore;

    let dir = resolve_data_dir(workspace_root, Some(branch_name));
    let graph_dir = dir.join("graph");
    if !graph_dir.exists() {
        return Ok(false);
    }
    let graph = GraphStore::init(&graph_dir)
        .map_err(|e| thinkingroot_core::Error::GraphStorage(format!("open branch graph: {e}")))?;
    // Scan sources for any mcp://agent/ URI. `find_sources_by_uri` matches
    // exact URIs, so we look at get_all_sources and filter the prefix.
    let sources = graph.get_all_sources()?;
    Ok(sources
        .iter()
        .any(|(_, uri, _)| uri.starts_with("mcp://agent/")))
}
