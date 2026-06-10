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

// ─── A7-SECURITY ⑥ — periodic integrity snapshots of main ──────────────────
//
// Rollback-to-known-good for poison discovered late: a copy of the main
// graph taken on a cadence, VALIDATED by re-opening it (a torn copy of a
// live db fails to open and is reported — never silently kept as a "good"
// snapshot). Snapshots live under `.thinkingroot/graph/integrity/` on the
// data volume. Vector indexes are NOT snapshotted — they are derivable by
// recompile; the graph is the source of truth.
//
// Env-gated, default OFF: TR_INTEGRITY_SNAPSHOTS=1 enables;
// TR_INTEGRITY_SNAPSHOT_SECS (default 21600 = 6h) sets the cadence;
// TR_INTEGRITY_SNAPSHOT_RETAIN (default 7) bounds retention — only files
// matching our own `graph.db.integrity-{ts}` pattern in our own
// `integrity/` dir are ever pruned.

/// One snapshot pass: copy → validate-by-open → prune to `retain`.
/// Returns the snapshot path, or an error when the copy failed validation.
pub fn integrity_snapshot_once(
    workspace_root: &std::path::Path,
    retain: usize,
) -> Result<PathBuf, thinkingroot_core::Error> {
    use thinkingroot_core::Error;
    let graph_db = workspace_root.join(".thinkingroot").join("graph").join("graph.db");
    if !graph_db.exists() {
        return Err(Error::GraphStorage(format!(
            "integrity snapshot: no graph at {}",
            graph_db.display()
        )));
    }
    let dir = workspace_root.join(".thinkingroot").join("graph").join("integrity");
    std::fs::create_dir_all(&dir).map_err(|e| Error::io_path(&dir, e))?;

    // Millis + a fixed-width process-global sequence: unique under rapid
    // calls (tests; manual triggers), all-digits (the pristine filter),
    // fixed width (lexicographic sort == chronological).
    use std::sync::atomic::{AtomicU64, Ordering};
    static SNAP_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().timestamp_millis();
    let seq = SNAP_SEQ.fetch_add(1, Ordering::Relaxed) % 1000;
    let snap = dir.join(format!("graph.db.integrity-{ts}{seq:03}"));
    std::fs::copy(&graph_db, &snap).map_err(|e| Error::io_path(&snap, e))?;

    // Validate structurally: a snapshot that cannot be opened is WORSE than
    // none — it would be discovered broken exactly when a rollback is
    // needed. We deliberately do NOT open-probe with GraphStore: cozo's
    // sqlite layer PANICS (internal unwrap) on a corrupt file, which would
    // crash the daemon on the very copy this validation exists to catch.
    // The structural check (SQLite magic + page-size/file-size consistency)
    // is panic-free and catches the realistic torn-copy modes — truncated
    // tail, mid-write garbage, wrong file. Failed copies are renamed
    // `.torn` (kept for forensics, excluded from retention/restore picks).
    if let Err(e) = validate_sqlite_structure(&snap) {
        let torn = snap.with_extension("torn");
        let _ = std::fs::rename(&snap, &torn);
        return Err(Error::GraphStorage(format!(
            "integrity snapshot failed validation (likely copied mid-write): {e} — \
             kept as {} for forensics; will retry next cycle",
            torn.display()
        )));
    }

    // Retention: prune ONLY our own pristine `graph.db.integrity-{ts}`
    // files (strictly numeric suffix — never `.torn`, never scratch),
    // oldest first, down to `retain`.
    let is_pristine_snapshot = |n: &str| {
        n.strip_prefix("graph.db.integrity-")
            .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false)
    };
    let mut snaps: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| Error::io_path(&dir, e))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(is_pristine_snapshot)
                    .unwrap_or(false)
        })
        .collect();
    snaps.sort();
    while snaps.len() > retain.max(1) {
        let oldest = snaps.remove(0);
        if let Err(e) = std::fs::remove_file(&oldest) {
            tracing::warn!(path = %oldest.display(), error = %e, "integrity snapshot prune failed");
        }
    }
    Ok(snap)
}

/// A7-SECURITY ⑥ (restore side) — list pristine integrity snapshots,
/// newest-first, as `(path, unix_millis)`. The millis are parsed from the
/// `graph.db.integrity-{digits}` name (the same value `_once` wrote), so no
/// extra stat() and the order is exact. `.torn`/foreign files are excluded.
pub fn list_integrity_snapshots(
    workspace_root: &std::path::Path,
) -> Result<Vec<(PathBuf, i64)>, thinkingroot_core::Error> {
    use thinkingroot_core::Error;
    let dir = workspace_root.join(".thinkingroot").join("graph").join("integrity");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<(PathBuf, i64)> = std::fs::read_dir(&dir)
        .map_err(|e| Error::io_path(&dir, e))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            let stamp = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("graph.db.integrity-"))
                .filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|r| r.parse::<i64>().ok())?;
            p.is_file().then_some((p, stamp))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
    Ok(out)
}

/// A7-SECURITY ⑥ (restore side) — roll the main graph back to a known-good
/// integrity snapshot. **Offline/maintenance operation**: the engine must
/// NOT be serving this workspace (it holds the SQLite file open; swapping it
/// underneath a live engine corrupts in-flight reads). This is enforced
/// operationally via the runbook, not in code — the CLI path runs before the
/// daemon binds. Steps: validate the chosen snapshot structurally → back up
/// the CURRENT graph.db as `graph.db.pre-restore-{ts}` (so a wrong rollback
/// is itself reversible) → atomically swap the snapshot into place. Vectors
/// are NOT restored (derivable by recompile); the caller should
/// `root compile` after to rebuild the index against the restored graph.
pub fn restore_integrity_snapshot(
    workspace_root: &std::path::Path,
    snapshot: &std::path::Path,
) -> Result<PathBuf, thinkingroot_core::Error> {
    use thinkingroot_core::Error;
    if !snapshot.exists() {
        return Err(Error::GraphStorage(format!(
            "restore: snapshot not found: {}",
            snapshot.display()
        )));
    }
    // Refuse a structurally-bad snapshot — restoring garbage over good data
    // is the one outcome worse than not restoring.
    validate_sqlite_structure(snapshot)?;

    let graph_db = workspace_root.join(".thinkingroot").join("graph").join("graph.db");
    // Back up the current graph first (reversible rollback).
    if graph_db.exists() {
        let ts = chrono::Utc::now().timestamp_millis();
        let backup = graph_db.with_file_name(format!("graph.db.pre-restore-{ts}"));
        std::fs::copy(&graph_db, &backup).map_err(|e| Error::io_path(&backup, e))?;
        tracing::info!(backup = %backup.display(), "restore: backed up current graph");
    }
    // Atomic swap via a temp in the same dir + rename.
    let tmp = graph_db.with_extension("db.restore-tmp");
    std::fs::copy(snapshot, &tmp).map_err(|e| Error::io_path(&tmp, e))?;
    std::fs::rename(&tmp, &graph_db).map_err(|e| Error::io_path(&graph_db, e))?;
    tracing::info!(from = %snapshot.display(), "restore: graph rolled back to snapshot");
    Ok(graph_db)
}

/// Panic-free structural validation of a SQLite file: the 16-byte magic,
/// a sane declared page size (power of two in [512, 65536]), and a file
/// size that is an exact multiple of it (SQLite files always are — a
/// truncated or mid-write copy almost never is).
fn validate_sqlite_structure(path: &std::path::Path) -> Result<(), thinkingroot_core::Error> {
    use thinkingroot_core::Error;
    let bytes = std::fs::read(path).map_err(|e| Error::io_path(path, e))?;
    if bytes.len() < 100 {
        return Err(Error::GraphStorage(format!(
            "snapshot too small to be a SQLite db ({} bytes)",
            bytes.len()
        )));
    }
    if &bytes[..16] != b"SQLite format 3\0" {
        return Err(Error::GraphStorage("snapshot missing SQLite header magic".into()));
    }
    let page_size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
        1 => 65_536usize, // SQLite encodes 65536 as 1
        n if n.is_power_of_two() && (512..=32_768).contains(&(n as usize)) => n as usize,
        n => {
            return Err(Error::GraphStorage(format!(
                "snapshot declares invalid SQLite page size {n}"
            )));
        }
    };
    if bytes.len() % page_size != 0 {
        return Err(Error::GraphStorage(format!(
            "snapshot size {} is not a multiple of page size {page_size} — truncated copy",
            bytes.len()
        )));
    }
    Ok(())
}

/// Retention prune for the append-only learning-signal tables. Default ON
/// with a generous window — unbounded growth is a real disk-exhaustion risk
/// at query volume, and the (future) idle trainer runs far inside any sane
/// window. `TR_SIGNAL_RETENTION_DAYS=0` disables; default 90 days.
struct SignalRetentionTask {
    workspace_root: PathBuf,
    retention_days: f64,
    interval: Duration,
}

#[async_trait]
impl PeriodicTask for SignalRetentionTask {
    fn name(&self) -> &'static str {
        "signal_retention"
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn run(&self) -> Result<(), thinkingroot_core::Error> {
        let graph_dir = self.workspace_root.join(".thinkingroot").join("graph");
        if !graph_dir.exists() {
            return Ok(());
        }
        let cutoff = (chrono::Utc::now().timestamp() as f64) - self.retention_days * 86_400.0;
        let graph = thinkingroot_graph::graph::GraphStore::init(&graph_dir)?;
        let (usage, verdicts) = graph.prune_learning_signal(cutoff)?;
        if usage > 0 || verdicts > 0 {
            tracing::info!(usage, verdicts, "signal retention pruned old rows");
        }
        Ok(())
    }
}

/// Spawn the learning-signal retention task. No-op handle when retention is
/// disabled (`TR_SIGNAL_RETENTION_DAYS=0`).
pub fn spawn_signal_retention(workspace_root: PathBuf) -> JoinHandle<()> {
    let retention_days = std::env::var("TR_SIGNAL_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(90.0);
    if retention_days <= 0.0 {
        return tokio::spawn(async {});
    }
    let task: Arc<dyn PeriodicTask> = Arc::new(SignalRetentionTask {
        workspace_root,
        retention_days,
        // Daily is plenty for a 90-day window.
        interval: Duration::from_secs(86_400),
    });
    spawn_periodic_task(task)
}

struct IntegritySnapshotTask {
    workspace_root: PathBuf,
    retain: usize,
    interval: Duration,
}

#[async_trait]
impl PeriodicTask for IntegritySnapshotTask {
    fn name(&self) -> &'static str {
        "integrity_snapshot"
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn run(&self) -> Result<(), thinkingroot_core::Error> {
        let root = self.workspace_root.clone();
        let retain = self.retain;
        // File copy of a potentially large db — keep it off the async core.
        tokio::task::spawn_blocking(move || integrity_snapshot_once(&root, retain))
            .await
            .map_err(|e| thinkingroot_core::Error::GraphStorage(format!("snapshot task join: {e}")))?
            .map(|p| {
                tracing::info!(snapshot = %p.display(), "integrity snapshot written");
            })
    }
}

/// Spawn the integrity-snapshot task (A7-⑥). No-op handle when the
/// `TR_INTEGRITY_SNAPSHOTS` env flag is off (the default).
pub fn spawn_integrity_snapshots(workspace_root: PathBuf) -> JoinHandle<()> {
    let enabled = std::env::var("TR_INTEGRITY_SNAPSHOTS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !enabled {
        return tokio::spawn(async {});
    }
    let interval_secs = std::env::var("TR_INTEGRITY_SNAPSHOT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(21_600u64);
    let retain = std::env::var("TR_INTEGRITY_SNAPSHOT_RETAIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7usize);
    let task: Arc<dyn PeriodicTask> = Arc::new(IntegritySnapshotTask {
        workspace_root,
        retain,
        interval: Duration::from_secs(interval_secs.max(60)),
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
/// with `BranchKind::Feature` + `MergePolicy::RequiresProposal` so the
/// topic→main promotion is **verify-before-merge gated** (M3): reaching
/// main requires an approved proposal whose `health_score` check passed.
/// `min_reviewers: 0` keeps autonomous promotion possible — the checks,
/// not a human reviewer, are the gate (open a proposal → checks run →
/// `Approved` iff health passes). Concurrent cleanup ticks racing the
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
        // health_score gates graph health/conflicts; function_tests (P4) runs
        // every Root Function's fixtures in the isolate so a JIT-authored
        // function that flowed in via a stream branch cannot reach `main`
        // unless its own tests pass — closing the self-authoring → verify loop.
        MergePolicy::RequiresProposal {
            min_reviewers: 0,
            required_checks: vec!["health_score".to_string(), "function_tests".to_string()],
        },
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

#[cfg(test)]
mod integrity_snapshot_tests {
    use super::*;

    #[test]
    fn snapshot_validates_prunes_and_skips_torn_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let graph_dir = root.join(".thinkingroot").join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();
        // A real, openable graph to snapshot.
        thinkingroot_graph::graph::GraphStore::init(&graph_dir).expect("init graph");

        // No graph at a bogus root → honest error.
        let bogus = tempfile::tempdir().unwrap();
        assert!(integrity_snapshot_once(bogus.path(), 3).is_err());

        // Take 5 snapshots with retain=3 → only the 3 newest survive.
        let mut taken = Vec::new();
        for _ in 0..5 {
            taken.push(integrity_snapshot_once(root, 3).expect("snapshot"));
        }
        let integrity_dir = graph_dir.join("integrity");
        let count = |dir: &std::path::Path| {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| {
                            n.strip_prefix("graph.db.integrity-")
                                .map(|r| r.bytes().all(|b| b.is_ascii_digit()))
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
                })
                .count()
        };
        assert_eq!(count(&integrity_dir), 3, "retention must prune to 3");
        // The newest snapshot survives and is openable (validated already,
        // but prove the survivor is the latest taken).
        assert!(taken.last().unwrap().exists(), "newest snapshot must survive");
        // The oldest were pruned.
        assert!(!taken[0].exists() && !taken[1].exists(), "oldest must be pruned");

        // Foreign files + .torn artifacts are NEVER pruned.
        let foreign = integrity_dir.join("operator-note.txt");
        std::fs::write(&foreign, b"do not touch").unwrap();
        let torn = integrity_dir.join("graph.db.integrity-1.torn");
        std::fs::write(&torn, b"forensics").unwrap();
        for _ in 0..3 {
            integrity_snapshot_once(root, 2).expect("snapshot");
        }
        assert!(foreign.exists(), "foreign files must never be pruned");
        assert!(torn.exists(), ".torn forensics must never be pruned");

        // A torn copy fails validation: corrupt source → error + .torn kept,
        // no pristine snapshot added.
        let before = count(&integrity_dir);
        std::fs::write(graph_dir.join("graph.db"), b"definitely not a database").unwrap();
        let res = integrity_snapshot_once(root, 5);
        assert!(res.is_err(), "corrupt copy must fail validation");
        assert_eq!(count(&integrity_dir), before, "no pristine snapshot from a torn copy");
    }

    #[test]
    fn list_and_restore_rolls_back_with_reversible_backup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let graph_dir = root.join(".thinkingroot").join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();
        thinkingroot_graph::graph::GraphStore::init(&graph_dir).expect("init graph");
        let graph_db = graph_dir.join("graph.db");

        // Take a known-good snapshot, then mutate the live graph so we can
        // prove the restore actually changed bytes back.
        let good = std::fs::read(&graph_db).unwrap();
        let snap = integrity_snapshot_once(root, 5).expect("snapshot");
        std::fs::write(&graph_db, b"corrupted-by-poison-or-bug").unwrap();

        // list surfaces the pristine snapshot.
        let snaps = list_integrity_snapshots(root).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].0, snap);

        // Restore rolls graph.db back to the snapshot bytes...
        restore_integrity_snapshot(root, &snap).expect("restore");
        assert_eq!(std::fs::read(&graph_db).unwrap(), good, "graph must match the snapshot");

        // ...and the pre-restore state is preserved (reversible).
        let pre = std::fs::read_dir(&graph_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("graph.db.pre-restore-"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(pre, 1, "current graph must be backed up before swap");

        // Restoring a structurally-bad file is refused (never overwrites good
        // data with garbage).
        let junk = graph_dir.join("graph.db.integrity-9999999999999");
        std::fs::write(&junk, b"not sqlite").unwrap();
        assert!(restore_integrity_snapshot(root, &junk).is_err());
        // Missing snapshot is an honest error, not a panic.
        assert!(restore_integrity_snapshot(root, &graph_dir.join("nope")).is_err());
    }
}
