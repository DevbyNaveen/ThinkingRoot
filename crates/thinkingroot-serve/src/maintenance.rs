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

use async_trait::async_trait;
use tokio::task::JoinHandle;

use crate::branch_cache::BranchEngineCache;
use crate::intelligence::session::SessionStore;
use crate::scheduler::{PeriodicTask, spawn_periodic_task};
use thinkingroot_core::{BranchKind, BranchPermissions, BranchStatus, MergePolicy, MergedBy};

/// Phase E.2 (2026-05-17) — `PeriodicTask` impl for stream-branch
/// cleanup. Encapsulates the data the worker loop needs so the
/// worker body itself can stay generic in `scheduler::spawn_periodic_task`.
///
/// The shape mirrors the legacy `spawn_stream_cleanup` closure
/// captures byte-for-byte — moving the loop into a trait impl is a
/// pure refactor.
struct StreamCleanupTask {
    sessions: SessionStore,
    workspace_root: PathBuf,
    idle_secs: u64,
    action: String,
    branch_engines: Option<Arc<BranchEngineCache>>,
    interval: Duration,
}

#[async_trait]
impl PeriodicTask for StreamCleanupTask {
    fn name(&self) -> &'static str {
        "stream_cleanup"
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn run(&self) -> Result<(), thinkingroot_core::Error> {
        cleanup_once(
            &self.sessions,
            &self.workspace_root,
            self.idle_secs,
            &self.action,
            self.branch_engines.as_deref(),
        )
        .await
        .map(|_stats| ())
    }
}

/// Spawn the stream-branch cleanup task.
///
/// Returns a `JoinHandle` the caller must keep alive (and may abort at
/// shutdown). If `cleanup_enabled = false`, this returns a no-op handle.
///
/// Phase E.2 (2026-05-17) — internally builds a `StreamCleanupTask`
/// and spawns it via `scheduler::spawn_periodic_task`. The signature
/// is unchanged for byte-equivalent CLI behaviour at
/// `thinkingroot-cli/src/serve.rs:{377,667}`.
pub fn spawn_stream_cleanup(
    sessions: SessionStore,
    workspace_root: PathBuf,
    cfg: thinkingroot_core::config::StreamsConfig,
    branch_engines: Option<Arc<BranchEngineCache>>,
) -> JoinHandle<()> {
    if !cfg.cleanup_enabled {
        return tokio::spawn(async {});
    }

    let task: Arc<dyn PeriodicTask> = Arc::new(StreamCleanupTask {
        sessions,
        workspace_root,
        idle_secs: cfg.cleanup_idle_secs,
        action: cfg.cleanup_action,
        branch_engines,
        interval: Duration::from_secs(cfg.cleanup_interval_secs.max(1)),
    });
    spawn_periodic_task(task)
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
        // Only consider Active branches. Merged/Abandoned are skipped.
        if !matches!(branch.status, BranchStatus::Active) {
            continue;
        }

        // T0.6: filter by typed `BranchKind::Stream { session_id }`
        // first; fall back to the historical `stream/` *name prefix*
        // for branches created before T0.6 added the discriminator.
        // The prefix-only path is the migration shim, not the
        // long-term contract — once every workspace has been remounted
        // post-T0.6 it can be removed.
        let session_id_owned: Option<String> = match &branch.kind {
            BranchKind::Stream { session_id } => Some(session_id.clone()),
            _ => branch
                .name
                .strip_prefix("stream/")
                .map(|s| s.to_string()),
        };
        let Some(session_id) = session_id_owned else {
            continue;
        };
        let session_id = session_id.as_str();
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

        // T0.6 — Ephemeral policy is always abandon. Never purge an
        // Ephemeral branch on the cleanup tick: the user opted into
        // "discard, don't merge" but didn't necessarily opt into
        // "delete the data dir." Purge stays an explicit gc_branches
        // call.
        //
        // Phase A (2026-05-17) — AutoOnSessionEnd with agent
        // contributes routes to `auto_merge_to_topic`: a topic
        // Feature branch is auto-created (or reused if already
        // present) and the stream is merged into it instead of being
        // abandoned. Main is intentionally NOT the target — only an
        // explicit user-initiated merge of topic → main promotes the
        // work. AutoOnSessionEnd with NO contributes falls through to
        // the default cleanup_action so empty streams don't leave
        // spurious topic branches behind.
        let effective_action = if branch.merge_policy.is_ephemeral() {
            "abandon"
        } else if matches!(branch.merge_policy, MergePolicy::AutoOnSessionEnd)
            && has_contributes
        {
            "auto_merge_to_topic"
        } else if action == "purge" && has_contributes {
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
            "auto_merge_to_topic" => {
                let topic_name =
                    topic_branch_name_for_session(session_id, &branch.created_at);
                match ensure_topic_branch(workspace_root, &topic_name).await {
                    Ok(()) => {
                        // Merge stream → topic. `force=false` keeps the
                        // health-score gate honest; `propagate_deletions=false`
                        // because stream-origin deletions are session-local
                        // intent and shouldn't propagate to a long-lived
                        // topic.
                        match thinkingroot_branch::merge_into(
                            workspace_root,
                            &branch.name,
                            &topic_name,
                            MergedBy::System,
                            /* force */ false,
                            /* propagate_deletions */ false,
                        )
                        .await
                        {
                            Ok(_diff) => {
                                stats.merged_to_topic += 1;

                                // Phase B.1 (2026-05-17) — propagate the
                                // stream branch's description (the user's
                                // first message of the session, written by
                                // the REST chat handler) onto the topic
                                // branch as its human-readable title.
                                // Best-effort: a failure here is logged but
                                // never rolls back the merge — the data is
                                // safely on the topic branch even if its
                                // title stays as the create-time placeholder.
                                if let Some(desc) = branch.description.as_deref() {
                                    let desc_trimmed = desc.trim();
                                    if !desc_trimmed.is_empty() {
                                        if let Err(e) =
                                            thinkingroot_branch::set_branch_description(
                                                workspace_root,
                                                &topic_name,
                                                Some(desc_trimmed.to_string()),
                                            )
                                        {
                                            tracing::warn!(
                                                topic = %topic_name,
                                                "B.1: failed to propagate stream description to topic (merge stays committed): {e}"
                                            );
                                        }
                                    }
                                }

                                tracing::info!(
                                    branch = %branch.name,
                                    session_id,
                                    target = %topic_name,
                                    "stream_cleanup: merged stream → topic (auto)"
                                );
                            }
                            Err(e) => {
                                // Leave stream Active; next tick retries.
                                // No data loss — the stream's data dir
                                // stays on disk until a successful merge
                                // (or an explicit abandon by the user).
                                tracing::warn!(
                                    branch = %branch.name,
                                    target = %topic_name,
                                    "stream_cleanup: auto-merge to topic failed \
                                     (stream stays Active for retry next tick): {e}"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            branch = %branch.name,
                            topic = %topic_name,
                            "stream_cleanup: ensure_topic_branch failed \
                             (stream stays Active for retry next tick): {e}"
                        );
                    }
                }
            }
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
        merged_to_topic = stats.merged_to_topic,
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
    /// Phase A (2026-05-17): count of stream branches whose
    /// `MergePolicy` is `AutoOnSessionEnd` and which carried agent
    /// contributes — these are merged into an auto-created `topic/*`
    /// Feature branch (default `MergePolicy::Manual`) instead of
    /// being abandoned. Main is left untouched; promoting topic →
    /// main remains an explicit user action.
    pub merged_to_topic: usize,
}

/// T2.3 — TTL cleanup pass.
///
/// Walks every Active branch in the workspace; abandons each one whose
/// `max_age_secs` opt-in TTL has expired (`now - created_at >
/// max_age_secs`).  Tag branches are skipped — they're immutable name
/// pins and should outlive arbitrary expiry windows.  Branches with
/// agent-contributed claims still abandon (data dir kept) rather than
/// purge — same safety as stream cleanup.
///
/// Returns counters for telemetry.  The pass is idempotent: a second
/// call after the first is a no-op (already-Abandoned branches are
/// skipped at the registry layer).
pub async fn ttl_cleanup_once(
    workspace_root: &std::path::Path,
    branch_engines: Option<&BranchEngineCache>,
) -> thinkingroot_core::Result<CleanupStats> {
    let mut stats = CleanupStats::default();

    let branches = thinkingroot_branch::list_branches(workspace_root)
        .map_err(|e| thinkingroot_core::Error::GraphStorage(format!("list_branches: {e}")))?;
    let now = chrono::Utc::now();

    for branch in branches {
        if !matches!(branch.status, BranchStatus::Active) {
            continue;
        }
        // Tags never auto-expire — they're name pins.
        if matches!(branch.kind, BranchKind::Tag { .. }) {
            continue;
        }
        let Some(max_age_secs) = branch.max_age_secs else {
            stats.kept += 1;
            continue;
        };
        stats.branches_scanned += 1;

        let age_secs = (now - branch.created_at).num_seconds().max(0) as u64;
        if age_secs <= max_age_secs {
            stats.kept += 1;
            continue;
        }

        // Drop cached engine handle BEFORE abandoning so the next
        // reader gets a fresh open and doesn't hold the disk open.
        if let Some(cache) = branch_engines {
            cache.invalidate(workspace_root, &branch.name).await;
        }
        match thinkingroot_branch::delete_branch(workspace_root, &branch.name) {
            Ok(_) => {
                stats.abandoned += 1;
                tracing::info!(
                    branch = %branch.name,
                    age_secs,
                    max_age_secs,
                    "ttl_cleanup: abandoned"
                );
            }
            Err(e) => {
                tracing::warn!(
                    branch = %branch.name,
                    "ttl_cleanup: abandon failed: {e}"
                );
            }
        }
    }

    tracing::info!(
        target: "ttl_cleanup",
        branches_scanned = stats.branches_scanned,
        abandoned = stats.abandoned,
        kept = stats.kept,
        "ttl cleanup tick complete"
    );
    Ok(stats)
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
        .any(|(_, uri, _, _)| uri.starts_with("mcp://agent/")))
}

/// Phase A (2026-05-17) — deterministic topic-branch name for a stream
/// session's auto-merge target.
///
/// Returns `topic/{YYYY-MM-DD}-{session_id_first_8_alphanum}`. Deterministic
/// so two cleanup ticks on the same session land on the same topic branch
/// (idempotent ensure + merge). Phase B replaces this with an AI-generated
/// slug derived from the session's conversation — callers MUST NOT depend
/// on the date-suffix shape.
fn topic_branch_name_for_session(
    session_id: &str,
    created_at: &chrono::DateTime<chrono::Utc>,
) -> String {
    let date = created_at.format("%Y-%m-%d");
    let short: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if short.is_empty() {
        format!("topic/{date}-anon")
    } else {
        format!("topic/{date}-{short}")
    }
}

/// Phase A (2026-05-17) — idempotent create of a topic Feature branch.
///
/// Returns Ok(()) when the branch is present-and-Active after the call,
/// whether we created it or it already existed. The branch is created
/// with `BranchKind::Feature` + `MergePolicy::Manual` so it never
/// auto-promotes to main — only an explicit user-initiated merge can
/// reach main from a topic branch. Concurrent cleanup ticks racing the
/// same name are tolerated: `Error::BranchAlreadyExists` is folded into
/// Ok because the post-state we wanted is already true.
async fn ensure_topic_branch(
    workspace_root: &std::path::Path,
    topic_name: &str,
) -> thinkingroot_core::Result<()> {
    // Cheap pre-check: skip the create call when the branch already
    // exists and is Active. Avoids a noisy "already exists" error in
    // the typical case (every cleanup tick after the first).
    let existing = thinkingroot_branch::list_branches(workspace_root)
        .map_err(|e| thinkingroot_core::Error::GraphStorage(format!("list_branches: {e}")))?;
    if existing
        .iter()
        .any(|b| b.name == topic_name && matches!(b.status, BranchStatus::Active))
    {
        return Ok(());
    }

    match thinkingroot_branch::create_branch_full(
        workspace_root,
        topic_name,
        "main",
        Some(
            "auto-created from stream cleanup (Phase A naming; AI titles in Phase B)"
                .to_string(),
        ),
        None, // owner: system
        BranchPermissions::default(),
        BranchKind::Feature,
        MergePolicy::Manual,
        None, // redaction
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) => match &e {
            // Racing cleanup ticks can both try to create; the
            // post-state is already what we wanted.
            thinkingroot_core::Error::BranchAlreadyExists(_) => Ok(()),
            _ => Err(e),
        },
    }
}
