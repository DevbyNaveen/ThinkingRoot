//! Slice 0 — workspace state machine actor.
//!
//! One [`WorkspaceStateActor`] per registered workspace. The actor
//! owns a [`WorkspaceStatus`] snapshot, accepts state-changing messages
//! on a [`tokio::sync::mpsc`] channel, and broadcasts a fresh snapshot
//! on a [`tokio::sync::broadcast`] channel after every transition that
//! actually changes the state.
//!
//! # Why an actor
//!
//! Every other approach drifted into races eventually:
//!
//! - **Per-handler RwLock<WorkspaceStatus>** — three handlers writing
//!   concurrently can interleave updates and emit a snapshot where
//!   `mount.state == Mounted` but `substrate.state == Absent`. The
//!   actor pattern guarantees a single writer.
//! - **Eager re-derivation in views** — the original bug. We're
//!   leaving that behind.
//!
//! The actor's loop is the only writer of [`WorkspaceStatus`] for a
//! given workspace; every other code path (mount handler, compile
//! handler, fs watcher, llm probe ticker) sends a [`Msg`] and reads the
//! resulting broadcast.
//!
//! # Honesty (CLAUDE.md §honesty rule §1)
//!
//! - The actor never **fabricates** transitions. A `MountSucceeded` is
//!   only emitted by the REST mount handler after the engine returns
//!   `Ok`. A `CompileFinished` is only emitted by the compile handler
//!   after the pipeline returns. The actor cannot decide on its own
//!   that mount succeeded.
//! - The substrate-from-disk probe is **best-effort**: if `graph.db`
//!   exists but we cannot determine claim count without opening it,
//!   the actor reports [`SubstrateState::Empty`] with the file size
//!   alone. The mount handler then pushes [`SubstrateState::Populated`]
//!   with the real claim/entity counts once the workspace is mounted.
//!   This is the most honest mapping we can make without violating
//!   the single-writer Cozo rule from `cortex-protocol.md`.
//! - The actor never silently drops messages. The mpsc channel is
//!   bounded (capacity 64); senders that hit the bound block — the
//!   pressure surfaces upstream rather than producing a "snapshot
//!   missed an event" silent failure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use thinkingroot_core::types::{
    BranchState, CompileOutcome, CompileProgress, CompileState, LLM_HEALTH_WINDOW, LlmState,
    MountState, SourcesState, SubstrateState, WorkspaceStatus, WorkspaceStatusEvent,
};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Capacity of each actor's inbound mpsc channel. Bounded — we'd
/// rather block a sender than silently drop a state-change message.
pub const ACTOR_INBOX_CAPACITY: usize = 64;

/// Capacity of each actor's outbound broadcast channel. Lossy under
/// extreme pressure (broadcast::Receiver returns `Lagged` for slow
/// consumers), but each message is a complete snapshot — a slow
/// consumer that catches up reads the latest, never an inconsistent
/// merge.
pub const ACTOR_BROADCAST_CAPACITY: usize = 64;

/// Cadence for the actor's internal "time-based" reconciliation:
/// re-emits the snapshot if `LlmState::Healthy` has aged past
/// [`LLM_HEALTH_WINDOW`] (degrades to `Configured`), and refreshes the
/// from-disk substrate + sources probes. 30s mirrors the existing
/// SSE keep-alive cadence.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Message accepted by [`WorkspaceStateActor`]. Every state-changing
/// edge in the daemon translates to exactly one of these — there is
/// no other writer.
#[derive(Debug, Clone)]
pub enum Msg {
    /// File-system watcher saw a change under `<root>/.thinkingroot/`
    /// or under the source tree. The actor re-runs the on-disk
    /// substrate + sources probes and broadcasts if anything changed.
    FsChanged,
    /// Workspace's `.thinkingroot/` was deleted while the daemon held
    /// it open. Distinct from [`Msg::FsChanged`] so the actor moves
    /// directly to [`SubstrateState::Orphaned`] without a probe.
    Orphaned {
        /// Workspace root the watcher captured.
        workspace_root: PathBuf,
    },
    /// REST mount handler is about to attempt mount.
    MountAttempt,
    /// Mount succeeded — engine is now holding the Cozo handle.
    /// Carries the live counts the mount handler read out of the
    /// engine.
    MountSucceeded {
        /// Live claim count from `?[count] := *claims[..]`.
        claim_count: u64,
        /// Live entity count.
        entity_count: u64,
        /// Source files seen by the most recent compile run that
        /// contributed to this substrate.
        source_count_at_last_compile: u64,
        /// Bytes on disk for `graph.db`.
        graph_db_bytes: u64,
    },
    /// Mount failed — engine refused to open the workspace.
    MountFailed {
        /// One-line reason from the engine.
        reason: String,
    },
    /// Workspace was unmounted via REST `DELETE /workspaces/{name}` or
    /// the daemon shut down.
    Unmounted,
    /// Live sync (or another scheduler) accepted a compile job; debounce
    /// window has not elapsed yet.
    CompileQueued {
        reason: String,
    },
    /// Compile pipeline started.
    CompileStarted,
    /// Compile pipeline phase tick.
    CompilePhase {
        /// Phase name (matches IncrementalSummary's `phase_timings`
        /// keys).
        phase: String,
        /// Optional progress fragment.
        progress: Option<CompileProgress>,
    },
    /// Compile finished (any outcome). Carries the resulting counts so
    /// the actor moves [`SubstrateState`] to `Populated` (or back to
    /// `Empty`) without an extra probe.
    CompileFinished {
        /// Outcome.
        outcome: CompileOutcome,
        /// Wall-clock duration of the run.
        duration_ms: u64,
        /// Resulting claim count.
        claim_count: u64,
        /// Resulting entity count.
        entity_count: u64,
        /// Resulting graph.db bytes.
        graph_db_bytes: u64,
    },
    /// LLM probe completed. The actor stamps the [`LlmState`] verbatim;
    /// staleness is enforced by the periodic reconcile tick rather than
    /// at probe time.
    LlmProbed {
        /// New state.
        state: LlmState,
    },
    /// Branch axis changed (mount switched branch, branch dirty flag
    /// flipped, etc.).
    BranchChanged {
        /// New branch state.
        state: BranchState,
    },
    /// Force a full re-probe of the on-disk axes. Wired to the
    /// `POST /workspaces/{name}/refresh` REST endpoint.
    Refresh,
}

/// Handle returned by [`spawn_workspace_state_actor`]. The caller keeps
/// this for the lifetime of the workspace's mount.
pub struct ActorHandle {
    /// Send messages to the actor.
    tx: mpsc::Sender<Msg>,
    /// Subscribe to broadcast snapshots. Each subscriber gets every
    /// snapshot from the time of subscription onward.
    events: broadcast::Sender<WorkspaceStatusEvent>,
    /// Latest snapshot, updated by the actor on every transition.
    /// Exposed so a fresh subscriber can synthesise an initial
    /// `Snapshot` event without waiting for the next state change.
    latest: Arc<RwLock<WorkspaceStatus>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl ActorHandle {
    /// Send a state-change message. Blocks (await) when the inbox is
    /// full; never drops.
    pub async fn send(&self, msg: Msg) -> Result<(), mpsc::error::SendError<Msg>> {
        self.tx.send(msg).await
    }

    /// Try to send without blocking. Returns
    /// [`mpsc::error::TrySendError::Full`] if the inbox is at capacity
    /// — caller decides whether to retry, await, or drop.
    pub fn try_send(&self, msg: Msg) -> Result<(), mpsc::error::TrySendError<Msg>> {
        self.tx.try_send(msg)
    }

    /// Subscribe to the broadcast channel. The first event a fresh
    /// subscriber should consume is the cached `latest` snapshot via
    /// [`ActorHandle::current`]; live changes follow.
    pub fn subscribe(&self) -> broadcast::Receiver<WorkspaceStatusEvent> {
        self.events.subscribe()
    }

    /// Snapshot the current [`WorkspaceStatus`] without waiting for an
    /// event. Used by the one-shot `/status` REST handler and by the
    /// SSE handler to send an initial `Snapshot` event on connect.
    pub async fn current(&self) -> WorkspaceStatus {
        self.latest.read().await.clone()
    }

    /// Trip the cancel token and join the actor task. Idempotent —
    /// subsequent calls are no-ops.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }

    /// Cancel without awaiting; caller is responsible for joining
    /// elsewhere if they care.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Test-only accessor for the broadcast sender. Real consumers use
    /// [`ActorHandle::subscribe`].
    #[cfg(test)]
    pub fn broadcast(&self) -> &broadcast::Sender<WorkspaceStatusEvent> {
        &self.events
    }
}

/// Actor configuration; defaults are production settings.
#[derive(Debug, Clone, Copy)]
pub struct ActorConfig {
    /// How often to fire the reconcile tick. Tests speed this up.
    pub reconcile_interval: Duration,
    /// Heartbeat cadence on the broadcast channel. 30s mirrors the
    /// SSE keep-alive on `/branches/{branch}/events/stream`.
    pub heartbeat_interval: Duration,
}

impl Default for ActorConfig {
    fn default() -> Self {
        Self {
            reconcile_interval: RECONCILE_INTERVAL,
            heartbeat_interval: Duration::from_secs(30),
        }
    }
}

/// Spawn a [`WorkspaceStateActor`] for the named workspace and return
/// its handle. The actor immediately runs an initial probe so the first
/// `latest` snapshot is fully populated.
pub async fn spawn_workspace_state_actor(
    name: String,
    path: PathBuf,
    cfg: ActorConfig,
) -> ActorHandle {
    let (tx, mut rx) = mpsc::channel::<Msg>(ACTOR_INBOX_CAPACITY);
    let (events, _) = broadcast::channel::<WorkspaceStatusEvent>(ACTOR_BROADCAST_CAPACITY);

    let initial = build_initial_snapshot(&name, &path).await;
    let latest = Arc::new(RwLock::new(initial.clone()));
    let cancel = CancellationToken::new();

    let events_task = events.clone();
    let latest_task = latest.clone();
    let cancel_task = cancel.clone();
    let name_task = name.clone();

    let task = tokio::spawn(async move {
        // ── Actor state held in the loop body. ────────────────────
        let mut path = initial.path.clone();
        let mut substrate = initial.substrate.clone();
        let mut sources = initial.sources.clone();
        let mut mount = initial.mount.clone();
        let mut llm = initial.llm.clone();
        let mut compile = initial.compile.clone();
        let mut branch = initial.branch.clone();

        let mut reconcile = tokio::time::interval(cfg.reconcile_interval);
        // First tick fires immediately by default; skip it so the
        // initial snapshot we just emitted is the first thing
        // subscribers see, not a duplicate.
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reconcile.tick().await;

        let mut heartbeat = tokio::time::interval(cfg.heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await;

        loop {
            tokio::select! {
                biased;
                _ = cancel_task.cancelled() => break,
                Some(msg) = rx.recv() => {
                    let prev_status = latest_task.read().await.clone();
                    apply_msg(
                        msg,
                        &name_task,
                        &mut path,
                        &mut substrate,
                        &mut sources,
                        &mut mount,
                        &mut llm,
                        &mut compile,
                        &mut branch,
                    )
                    .await;

                    let new_status = assemble(
                        &name_task,
                        &path,
                        &substrate,
                        &sources,
                        &mount,
                        &llm,
                        &compile,
                        &branch,
                    );

                    if status_meaningfully_differs(&prev_status, &new_status) {
                        *latest_task.write().await = new_status.clone();
                        let _ = events_task.send(WorkspaceStatusEvent::Snapshot(new_status));
                    }
                }
                _ = reconcile.tick() => {
                    // Time-based axes only:
                    //   - `LlmState::Healthy` decays to `Configured`
                    //     once `LLM_HEALTH_WINDOW` elapses.
                    //   - Sources are filesystem-only and re-probed
                    //     periodically to catch external file edits
                    //     the watcher missed.
                    //
                    // Substrate is NOT re-probed here. It's authoritatively
                    // owned by mount/compile/orphan push events; the disk
                    // probe runs only on `FsChanged` / `Refresh`. This is
                    // the rule that keeps the actor honest — we never
                    // override "you said claim_count=42" with a disk-probe
                    // best-guess that defaults claim count back to 0.
                    let prev_llm = llm.clone();
                    if let LlmState::Healthy { provider, model, last_probed_at } = &llm {
                        let age = Utc::now().signed_duration_since(*last_probed_at);
                        if age.to_std().map(|d| d > LLM_HEALTH_WINDOW).unwrap_or(false) {
                            llm = LlmState::Configured {
                                provider: provider.clone(),
                                model: model.clone(),
                            };
                        }
                    }

                    let new_sources = probe_sources(&path).await;

                    let llm_changed = llm != prev_llm;
                    let sources_changed = new_sources != sources;

                    if llm_changed || sources_changed {
                        sources = new_sources;
                        let new_status = assemble(
                            &name_task,
                            &path,
                            &substrate,
                            &sources,
                            &mount,
                            &llm,
                            &compile,
                            &branch,
                        );
                        *latest_task.write().await = new_status.clone();
                        let _ = events_task.send(WorkspaceStatusEvent::Snapshot(new_status));
                    }
                }
                _ = heartbeat.tick() => {
                    let _ = events_task.send(WorkspaceStatusEvent::Heartbeat {
                        name: name_task.clone(),
                        at: Utc::now(),
                    });
                }
            }
        }
        tracing::info!(target: "workspace_state", name = %name_task, "actor shutting down");
    });

    ActorHandle {
        tx,
        events,
        latest,
        cancel,
        task,
    }
}

/// Apply a [`Msg`] to the actor's local state. Pure transition logic;
/// I/O lives in `probe_*` helpers and is invoked separately. Mutates
/// the borrowed axes in place.
async fn apply_msg(
    msg: Msg,
    name: &str,
    path: &mut PathBuf,
    substrate: &mut SubstrateState,
    sources: &mut SourcesState,
    mount: &mut MountState,
    _llm: &mut LlmState,
    compile: &mut CompileState,
    branch: &mut BranchState,
) {
    match msg {
        Msg::FsChanged => {
            *substrate = probe_substrate(path, substrate, mount).await;
            *sources = probe_sources(path).await;
        }
        Msg::Orphaned { workspace_root } => {
            *substrate = SubstrateState::Orphaned { workspace_root };
            // Mount becomes Failed because the substrate is gone; the
            // engine handle, if any, references a deleted file.
            *mount = MountState::Failed {
                reason: ".thinkingroot/ deleted".into(),
                at: Utc::now(),
            };
        }
        Msg::MountAttempt => {
            *mount = MountState::Mounting;
        }
        Msg::MountSucceeded {
            claim_count,
            entity_count,
            source_count_at_last_compile,
            graph_db_bytes,
        } => {
            *mount = MountState::Mounted { since: Utc::now() };
            // Mount handler always pushes the live counts; substrate
            // updates from those — the only authoritative source.
            *substrate = if claim_count == 0 {
                SubstrateState::Empty { graph_db_bytes }
            } else {
                SubstrateState::Populated {
                    graph_db_bytes,
                    claim_count,
                    entity_count,
                    source_count_at_last_compile,
                }
            };
        }
        Msg::MountFailed { reason } => {
            *mount = MountState::Failed {
                reason,
                at: Utc::now(),
            };
        }
        Msg::Unmounted => {
            *mount = MountState::NotMounted;
        }
        Msg::CompileQueued { reason } => {
            *compile = CompileState::Queued {
                since: Utc::now(),
                reason,
            };
        }
        Msg::CompileStarted => {
            *compile = CompileState::Running {
                phase: "starting".into(),
                progress: None,
                started_at: Utc::now(),
            };
        }
        Msg::CompilePhase { phase, progress } => {
            // Preserve `started_at` if we're already running.
            let started_at = match compile {
                CompileState::Running { started_at, .. } => *started_at,
                _ => Utc::now(),
            };
            *compile = CompileState::Running {
                phase,
                progress,
                started_at,
            };
        }
        Msg::CompileFinished {
            outcome,
            duration_ms,
            claim_count,
            entity_count,
            graph_db_bytes,
        } => {
            *compile = CompileState::Idle {
                last_finished_at: Some(Utc::now()),
                last_duration_ms: Some(duration_ms),
                last_outcome: Some(outcome.clone()),
            };
            // Compile is the other authoritative count source. Update
            // substrate to reflect the post-compile reality. We respect
            // outcome — a `Failed` compile may not have produced a
            // populated substrate even if claim_count > 0 from before.
            let source_count_for_substrate = match &outcome {
                CompileOutcome::Success {
                    sources_processed, ..
                } => Some(*sources_processed),
                CompileOutcome::Partial { .. } => Some(0),
                CompileOutcome::Failed { .. } | CompileOutcome::Cancelled { .. } => None,
            };
            match source_count_for_substrate {
                Some(source_count) => {
                    *substrate = if claim_count == 0 {
                        SubstrateState::Empty { graph_db_bytes }
                    } else {
                        SubstrateState::Populated {
                            graph_db_bytes,
                            claim_count,
                            entity_count,
                            source_count_at_last_compile: source_count,
                        }
                    };
                }
                None => {
                    // Don't pretend the substrate updated; re-probe from
                    // disk so we report the truthful pre-compile state.
                    *substrate = probe_substrate(path, substrate, mount).await;
                }
            }
        }
        Msg::LlmProbed { state } => {
            *_llm = state;
        }
        Msg::BranchChanged { state } => {
            *branch = state;
        }
        Msg::Refresh => {
            *substrate = probe_substrate(path, substrate, mount).await;
            *sources = probe_sources(path).await;
            tracing::debug!(target: "workspace_state", %name, "forced refresh complete");
        }
    }
    let _ = path; // path is only mutable for future API symmetry
}

/// Probe the on-disk substrate. **Best-effort** — never opens Cozo
/// (single-writer rule). When the daemon is mounted, prefer
/// pushing [`SubstrateState`] via [`Msg::MountSucceeded`] /
/// [`Msg::CompileFinished`] which carry real counts.
async fn probe_substrate(
    path: &Path,
    prev: &SubstrateState,
    mount: &MountState,
) -> SubstrateState {
    let engine_dir = path.join(".thinkingroot");
    if !engine_dir.exists() {
        // If we previously thought the substrate was Populated and now
        // it's gone, the watcher will (separately) push `Orphaned`. For
        // now, surface the literal observation: Absent.
        return SubstrateState::Absent;
    }
    let graph_db = engine_dir.join("graph").join("graph.db");
    let bytes = match tokio::fs::metadata(&graph_db).await {
        Ok(m) => m.len(),
        Err(_) => return SubstrateState::Absent,
    };
    // If we're currently mounted and have live counts cached in
    // `prev`, preserve them — the disk-only probe can't decide
    // Populated-vs-Empty without opening Cozo. The mount handler will
    // push real counts on the next mount/compile event.
    if matches!(mount, MountState::Mounted { .. }) {
        if matches!(prev, SubstrateState::Populated { .. }) {
            // Refresh size on the existing Populated state.
            if let SubstrateState::Populated {
                claim_count,
                entity_count,
                source_count_at_last_compile,
                ..
            } = prev
            {
                return SubstrateState::Populated {
                    graph_db_bytes: bytes,
                    claim_count: *claim_count,
                    entity_count: *entity_count,
                    source_count_at_last_compile: *source_count_at_last_compile,
                };
            }
        }
        if matches!(prev, SubstrateState::Empty { .. }) {
            return SubstrateState::Empty {
                graph_db_bytes: bytes,
            };
        }
    }
    // Unmounted with substrate present — most honest report is
    // `Empty { bytes }`. If the file is empty (0 bytes), that's also
    // Absent-like; we still report `Empty { 0 }` to distinguish from
    // "directory entirely missing".
    SubstrateState::Empty {
        graph_db_bytes: bytes,
    }
}

/// Walk the workspace tree and count source files (excluding the same
/// dirs the pack writer excludes: `.thinkingroot/`, `cache/`, `target/`,
/// `node_modules/`, `.git/`, plus a handful of always-excluded files).
async fn probe_sources(path: &Path) -> SourcesState {
    if !path.exists() {
        return SourcesState::None;
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || walk_sources_blocking(&path))
        .await
        .unwrap_or(SourcesState::None)
}

fn walk_sources_blocking(root: &Path) -> SourcesState {
    use std::fs;
    use std::time::SystemTime;

    fn excluded(component: &str) -> bool {
        matches!(
            component,
            ".thinkingroot"
                | "cache"
                | "target"
                | "node_modules"
                | ".git"
                | ".DS_Store"
                | "fingerprints.json"
                | "config.toml"
        )
    }

    // ledger_mtime is the cutover point: anything on disk newer than
    // the last successful compile's fingerprints.json is unaccounted
    // for. Absent ledger ⇒ no compile has run ⇒ everything is "new".
    // The pipeline writes this file atomically via `FingerprintStore::save`
    // (tempfile + rename), so we never observe a torn mtime.
    let ledger_mtime: Option<SystemTime> = fs::metadata(root.join(".thinkingroot/fingerprints.json"))
        .ok()
        .and_then(|m| m.modified().ok());

    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut latest_file: Option<chrono::DateTime<Utc>> = None;
    // Track the newest *mtime* across both files AND directories. A
    // file add/remove updates the parent dir's mtime but not any
    // existing file's mtime, so dir-mtimes are load-bearing for
    // detecting newly-added or just-deleted sources.
    let mut newest_seen: Option<SystemTime> = None;
    let bump = |slot: &mut Option<SystemTime>, t: SystemTime| {
        *slot = Some(slot.map_or(t, |prev| prev.max(t)));
    };

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // Include the directory's own mtime in the freshness signal —
        // directory entries change when a child is added or removed,
        // and that's the only signal "the user just rm'd a source".
        if let Ok(dir_meta) = fs::metadata(&dir)
            && let Ok(mt) = dir_meta.modified()
        {
            bump(&mut newest_seen, mt);
        }

        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = match p.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if excluded(&name) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                file_count += 1;
                total_bytes = total_bytes.saturating_add(meta.len());
                if let Ok(modified) = meta.modified() {
                    let dt: chrono::DateTime<Utc> = modified.into();
                    latest_file = Some(latest_file.map_or(dt, |x| x.max(dt)));
                    bump(&mut newest_seen, modified);
                }
            }
        }
    }

    if file_count == 0 {
        SourcesState::None
    } else {
        // Fingerprint match honest derivation: the ledger is the
        // mtime of `.thinkingroot/fingerprints.json` (atomically
        // rewritten at the end of every successful compile). Anything
        // newer on disk — file edits, additions, deletions — proves
        // the substrate is behind. No ledger ⇒ no compile ever ran ⇒
        // we must report `false`; we never fabricate a "match" just
        // because the directory looks populated.
        let fingerprint_match = match (ledger_mtime, newest_seen) {
            (Some(ledger), Some(seen)) => seen <= ledger,
            (Some(_), None) => true, // ledger present, walker saw nothing newer
            (None, _) => false,      // no ledger ⇒ never compiled ⇒ stale by definition
        };
        SourcesState::Some {
            file_count,
            total_bytes,
            last_changed_at: latest_file,
            fingerprint_match,
        }
    }
}

fn assemble(
    name: &str,
    path: &Path,
    substrate: &SubstrateState,
    sources: &SourcesState,
    mount: &MountState,
    llm: &LlmState,
    compile: &CompileState,
    branch: &BranchState,
) -> WorkspaceStatus {
    WorkspaceStatus::assemble(
        name.to_string(),
        path.to_path_buf(),
        path.exists(),
        substrate.clone(),
        sources.clone(),
        mount.clone(),
        llm.clone(),
        compile.clone(),
        branch.clone(),
    )
}

/// Compare two snapshots for "any axis changed". Avoids broadcasting
/// no-op snapshots that just bump `as_of` — clients shouldn't see a
/// stream of identical events.
fn status_meaningfully_differs(a: &WorkspaceStatus, b: &WorkspaceStatus) -> bool {
    a.substrate != b.substrate
        || a.sources != b.sources
        || a.mount != b.mount
        || a.llm != b.llm
        || a.compile != b.compile
        || a.branch != b.branch
        || a.readiness != b.readiness
        || a.diagnostics != b.diagnostics
}

async fn build_initial_snapshot(name: &str, path: &Path) -> WorkspaceStatus {
    let substrate = probe_substrate(path, &SubstrateState::Absent, &MountState::NotMounted).await;
    let sources = probe_sources(path).await;
    assemble(
        name,
        path,
        &substrate,
        &sources,
        &MountState::NotMounted,
        &LlmState::Unconfigured,
        &CompileState::Idle {
            last_finished_at: None,
            last_duration_ms: None,
            last_outcome: None,
        },
        &BranchState::default(),
    )
}

/// Process-global registry of [`ActorHandle`]s, keyed by workspace
/// name. The daemon keeps one of these on [`AppState`]; mount/compile
/// handlers look up the actor by name and dispatch [`Msg`]s.
#[derive(Default)]
pub struct WorkspaceStateRegistry {
    inner: RwLock<HashMap<String, Arc<ActorHandle>>>,
}

impl WorkspaceStateRegistry {
    /// Construct a fresh, empty registry.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Get the handle for an existing workspace.
    pub async fn get(&self, name: &str) -> Option<Arc<ActorHandle>> {
        self.inner.read().await.get(name).cloned()
    }

    /// Spawn an actor for `name` if absent, returning the existing
    /// handle otherwise. Idempotent — safe to call from every mount
    /// site.
    pub async fn ensure(&self, name: &str, path: PathBuf) -> Arc<ActorHandle> {
        if let Some(h) = self.inner.read().await.get(name) {
            return h.clone();
        }
        let mut guard = self.inner.write().await;
        if let Some(h) = guard.get(name) {
            return h.clone();
        }
        let actor = spawn_workspace_state_actor(name.to_string(), path, ActorConfig::default()).await;
        let arc = Arc::new(actor);
        guard.insert(name.to_string(), arc.clone());
        arc
    }

    /// Dispatch a message to the named workspace's actor; spawn one
    /// if it doesn't exist yet (callers always know the path because
    /// the daemon's registry has it).
    pub async fn dispatch(&self, name: &str, path: PathBuf, msg: Msg) {
        let actor = self.ensure(name, path).await;
        if let Err(e) = actor.send(msg).await {
            tracing::warn!(
                target: "workspace_state",
                workspace = %name,
                "actor inbox dispatch failed: {e}"
            );
        }
    }

    /// Snapshot every known workspace's current status. Used by the
    /// `GET /workspaces/status` collection endpoint.
    pub async fn snapshot_all(&self) -> Vec<WorkspaceStatus> {
        let guard = self.inner.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for handle in guard.values() {
            out.push(handle.current().await);
        }
        out
    }

    /// Remove and shut down the named actor. Used by the unmount
    /// handler when the workspace is fully unregistered (vs just
    /// unmounted from the engine).
    pub async fn remove(&self, name: &str) {
        let removed = self.inner.write().await.remove(name);
        if let Some(handle) = removed {
            // Drop the Arc — but we still need to call shutdown on the
            // contained handle. Since shutdown takes ownership, we
            // can only call it if we're the last Arc holder. If we
            // aren't, callers still subscribed to events will keep the
            // actor alive; we cancel the token to terminate the loop.
            handle.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;
    use thinkingroot_core::types::{LlmState, SubstrateState};

    fn fast_cfg() -> ActorConfig {
        ActorConfig {
            reconcile_interval: Duration::from_millis(80),
            heartbeat_interval: Duration::from_millis(120),
        }
    }

    #[tokio::test]
    async fn fresh_actor_emits_initial_snapshot_via_current() {
        let tmp = TempDir::new().unwrap();
        let actor = spawn_workspace_state_actor(
            "demo".into(),
            tmp.path().to_path_buf(),
            fast_cfg(),
        )
        .await;
        let snap = actor.current().await;
        assert_eq!(snap.name, "demo");
        // Empty workspace: no .thinkingroot/, no source files.
        assert!(matches!(snap.substrate, SubstrateState::Absent));
        assert!(matches!(snap.sources, SourcesState::None));
        assert!(matches!(snap.mount, MountState::NotMounted));
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn mount_succeeded_msg_flips_substrate_to_populated() {
        let tmp = TempDir::new().unwrap();
        let engine = tmp.path().join(".thinkingroot").join("graph");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(engine.join("graph.db"), b"x".repeat(100_000)).unwrap();

        let actor =
            spawn_workspace_state_actor("demo".into(), tmp.path().to_path_buf(), fast_cfg())
                .await;
        let mut rx = actor.subscribe();

        actor
            .send(Msg::MountSucceeded {
                claim_count: 42,
                entity_count: 17,
                source_count_at_last_compile: 5,
                graph_db_bytes: 100_000,
            })
            .await
            .unwrap();

        let mut got_populated = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                ev = rx.recv() => {
                    if let Ok(WorkspaceStatusEvent::Snapshot(s)) = ev {
                        if matches!(s.substrate, SubstrateState::Populated { claim_count: 42, .. }) {
                            assert!(matches!(s.mount, MountState::Mounted { .. }));
                            assert!(s.readiness.for_export || !s.sources.has_sources());
                            got_populated = true;
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        assert!(got_populated, "expected Populated snapshot after mount");
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn mount_zero_claims_yields_empty_substrate() {
        // CipherVault scenario: graph.db exists, mount succeeds, but
        // claim count is 0. Right-rail must NOT see Populated.
        let tmp = TempDir::new().unwrap();
        let actor =
            spawn_workspace_state_actor("ws".into(), tmp.path().to_path_buf(), fast_cfg())
                .await;
        actor
            .send(Msg::MountSucceeded {
                claim_count: 0,
                entity_count: 0,
                source_count_at_last_compile: 0,
                graph_db_bytes: 12_288,
            })
            .await
            .unwrap();
        // Drain to the post-msg state.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let snap = actor.current().await;
        assert!(matches!(
            snap.substrate,
            SubstrateState::Empty { graph_db_bytes: 12_288 }
        ));
        assert!(!snap.readiness.for_query);
        assert!(!snap.readiness.for_chat);
        assert!(!snap.readiness.for_export);
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn orphaned_msg_blocks_compile_too() {
        let tmp = TempDir::new().unwrap();
        let actor =
            spawn_workspace_state_actor("ws".into(), tmp.path().to_path_buf(), fast_cfg())
                .await;
        actor
            .send(Msg::Orphaned {
                workspace_root: tmp.path().to_path_buf(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let snap = actor.current().await;
        assert!(matches!(snap.substrate, SubstrateState::Orphaned { .. }));
        assert!(!snap.readiness.for_compile);
        assert!(!snap.readiness.for_query);
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn compile_started_then_finished_round_trips() {
        let tmp = TempDir::new().unwrap();
        // Pre-populate so the post-compile state is Populated.
        std::fs::create_dir_all(tmp.path().join(".thinkingroot/graph")).unwrap();
        std::fs::write(
            tmp.path().join(".thinkingroot/graph/graph.db"),
            b"x".repeat(2048),
        )
        .unwrap();
        let actor =
            spawn_workspace_state_actor("ws".into(), tmp.path().to_path_buf(), fast_cfg())
                .await;
        actor.send(Msg::CompileStarted).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let mid = actor.current().await;
        assert!(matches!(mid.compile, CompileState::Running { .. }));
        assert!(!mid.readiness.for_compile);

        actor
            .send(Msg::CompileFinished {
                outcome: CompileOutcome::Success {
                    extracted_claims: 7,
                    sources_processed: 3,
                },
                duration_ms: 300,
                claim_count: 7,
                entity_count: 4,
                graph_db_bytes: 2048,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let end = actor.current().await;
        assert!(matches!(end.compile, CompileState::Idle { .. }));
        assert!(matches!(end.substrate, SubstrateState::Populated { claim_count: 7, .. }));
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn llm_healthy_decays_to_configured_after_window() {
        let tmp = TempDir::new().unwrap();
        // Use a fast reconcile so the test runs in <1s.
        let cfg = ActorConfig {
            reconcile_interval: Duration::from_millis(50),
            heartbeat_interval: Duration::from_millis(500),
        };
        let actor =
            spawn_workspace_state_actor("ws".into(), tmp.path().to_path_buf(), cfg).await;

        // Push a Healthy state with an already-stale `last_probed_at`.
        let stale = Utc::now() - chrono::Duration::seconds(LLM_HEALTH_WINDOW.as_secs() as i64 + 60);
        actor
            .send(Msg::LlmProbed {
                state: LlmState::Healthy {
                    provider: "anthropic".into(),
                    model: Some("opus".into()),
                    last_probed_at: stale,
                },
            })
            .await
            .unwrap();
        // Wait for the next reconcile.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let snap = actor.current().await;
        assert!(matches!(
            snap.llm,
            LlmState::Configured { .. }
        ), "stale Healthy must decay to Configured, got {:?}", snap.llm);
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn registry_ensure_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let reg = WorkspaceStateRegistry::new();
        let a = reg.ensure("ws", tmp.path().to_path_buf()).await;
        let b = reg.ensure("ws", tmp.path().to_path_buf()).await;
        assert!(Arc::ptr_eq(&a, &b));
        a.cancel();
    }

    #[tokio::test]
    async fn registry_snapshot_all_returns_known_workspaces() {
        let t1 = TempDir::new().unwrap();
        let t2 = TempDir::new().unwrap();
        let reg = WorkspaceStateRegistry::new();
        let _a = reg.ensure("alpha", t1.path().to_path_buf()).await;
        let _b = reg.ensure("beta", t2.path().to_path_buf()).await;
        let mut names: Vec<_> = reg
            .snapshot_all()
            .await
            .into_iter()
            .map(|s| s.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
        _a.cancel();
        _b.cancel();
    }

    #[tokio::test]
    async fn unmount_then_remount_emits_correct_states() {
        let tmp = TempDir::new().unwrap();
        let actor =
            spawn_workspace_state_actor("ws".into(), tmp.path().to_path_buf(), fast_cfg())
                .await;
        actor
            .send(Msg::MountSucceeded {
                claim_count: 1,
                entity_count: 1,
                source_count_at_last_compile: 1,
                graph_db_bytes: 1024,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(matches!(actor.current().await.mount, MountState::Mounted { .. }));

        actor.send(Msg::Unmounted).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(matches!(
            actor.current().await.mount,
            MountState::NotMounted
        ));

        actor.send(Msg::MountAttempt).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(actor.current().await.mount, MountState::Mounting));

        actor
            .send(Msg::MountFailed {
                reason: "boom".into(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(matches!(
            actor.current().await.mount,
            MountState::Failed { .. }
        ));

        actor.shutdown().await;
    }

    #[tokio::test]
    async fn refresh_msg_re_probes_disk() {
        let tmp = TempDir::new().unwrap();
        let actor =
            spawn_workspace_state_actor("ws".into(), tmp.path().to_path_buf(), fast_cfg())
                .await;
        // Initially Absent.
        assert!(matches!(
            actor.current().await.substrate,
            SubstrateState::Absent
        ));
        // Create graph.db and ask for refresh.
        std::fs::create_dir_all(tmp.path().join(".thinkingroot/graph")).unwrap();
        std::fs::write(
            tmp.path().join(".thinkingroot/graph/graph.db"),
            b"x".repeat(64),
        )
        .unwrap();
        actor.send(Msg::Refresh).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let snap = actor.current().await;
        assert!(matches!(snap.substrate, SubstrateState::Empty { .. }));
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn no_op_msg_does_not_emit_duplicate_snapshots() {
        let tmp = TempDir::new().unwrap();
        let actor =
            spawn_workspace_state_actor("ws".into(), tmp.path().to_path_buf(), fast_cfg())
                .await;
        let mut rx = actor.subscribe();
        // FsChanged with no actual change should not flip any axis.
        actor.send(Msg::FsChanged).await.unwrap();
        // Allow processing.
        let recv = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
        // Either Heartbeat (fine) or Lagged or no event — but never a
        // Snapshot, because nothing changed.
        if let Ok(Ok(WorkspaceStatusEvent::Snapshot(_))) = recv {
            panic!("no-op FsChanged must not emit a Snapshot event");
        }
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeat_arrives_periodically() {
        let tmp = TempDir::new().unwrap();
        let cfg = ActorConfig {
            reconcile_interval: Duration::from_secs(60), // suppress
            heartbeat_interval: Duration::from_millis(80),
        };
        let actor =
            spawn_workspace_state_actor("ws".into(), tmp.path().to_path_buf(), cfg).await;
        let mut rx = actor.subscribe();
        let mut beats = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline && beats < 2 {
            if let Ok(Ok(WorkspaceStatusEvent::Heartbeat { .. })) =
                tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
            {
                beats += 1;
            }
        }
        assert!(beats >= 2, "expected ≥2 heartbeats, got {beats}");
        actor.shutdown().await;
    }

    #[tokio::test]
    async fn fingerprint_match_false_when_ledger_absent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), b"fn main() {}").unwrap();
        let state = probe_sources(tmp.path()).await;
        let SourcesState::Some { fingerprint_match, file_count, .. } = state else {
            panic!("expected Some sources, got {state:?}");
        };
        assert_eq!(file_count, 1);
        // No `.thinkingroot/fingerprints.json` exists ⇒ no compile has
        // ever stamped these sources ⇒ honestly stale.
        assert!(!fingerprint_match, "absent ledger must report stale");
    }

    #[tokio::test]
    async fn fingerprint_match_true_when_ledger_newer_than_sources() {
        let tmp = TempDir::new().unwrap();
        // Source first, then ledger — so ledger mtime > source mtime.
        std::fs::write(tmp.path().join("a.rs"), b"fn main() {}").unwrap();
        // Tiny sleep so the ledger's mtime is strictly greater than the
        // source's mtime on filesystems with second-granularity mtime
        // (some Linux tmpfs configurations).
        tokio::time::sleep(Duration::from_millis(1100)).await;
        std::fs::create_dir_all(tmp.path().join(".thinkingroot")).unwrap();
        std::fs::write(
            tmp.path().join(".thinkingroot/fingerprints.json"),
            b"{}",
        )
        .unwrap();
        let state = probe_sources(tmp.path()).await;
        let SourcesState::Some { fingerprint_match, .. } = state else {
            panic!("expected Some sources, got {state:?}");
        };
        assert!(fingerprint_match, "ledger newer than sources must report fresh");
    }

    #[tokio::test]
    async fn fingerprint_match_false_after_source_edit() {
        let tmp = TempDir::new().unwrap();
        // Compile first (ledger), then edit a source after.
        std::fs::create_dir_all(tmp.path().join(".thinkingroot")).unwrap();
        std::fs::write(
            tmp.path().join(".thinkingroot/fingerprints.json"),
            b"{}",
        )
        .unwrap();
        std::fs::write(tmp.path().join("a.rs"), b"old").unwrap();
        // Wait past the second-granularity boundary so a fresh write
        // is strictly newer.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        std::fs::write(tmp.path().join("a.rs"), b"new content").unwrap();
        let state = probe_sources(tmp.path()).await;
        let SourcesState::Some { fingerprint_match, .. } = state else {
            panic!("expected Some sources, got {state:?}");
        };
        assert!(
            !fingerprint_match,
            "source edit after compile must report stale"
        );
    }

    #[tokio::test]
    async fn fingerprint_match_false_after_new_file_added() {
        let tmp = TempDir::new().unwrap();
        // Ledger + initial source.
        std::fs::create_dir_all(tmp.path().join(".thinkingroot")).unwrap();
        std::fs::write(tmp.path().join("a.rs"), b"fn main() {}").unwrap();
        std::fs::write(
            tmp.path().join(".thinkingroot/fingerprints.json"),
            b"{}",
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        // Add a brand-new file — captured via parent-dir mtime bump.
        std::fs::write(tmp.path().join("b.rs"), b"fn new() {}").unwrap();
        let state = probe_sources(tmp.path()).await;
        let SourcesState::Some { fingerprint_match, file_count, .. } = state else {
            panic!("expected Some sources, got {state:?}");
        };
        assert_eq!(file_count, 2);
        assert!(
            !fingerprint_match,
            "file add after compile must report stale via dir mtime"
        );
    }
}
