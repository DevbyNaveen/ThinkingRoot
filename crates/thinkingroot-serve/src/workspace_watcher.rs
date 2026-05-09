//! Slice 3 — workspace-root file-system watcher.
//!
//! Spawns a dedicated tokio task that keeps a [`notify::Watcher`]
//! attached to the daemon's currently-active workspace root. When the
//! root flips (mount / unmount / re-mount) the watcher tears down the
//! old subscription and re-attaches to the new one. When the active
//! workspace's `.thinkingroot/` is deleted underneath the daemon, the
//! watcher emits [`WorkspaceEvent::DotThinkingrootDeleted`] on the
//! shared broadcast channel and flips the daemon's
//! [`WorkspaceState`] flag to `OrphanedSubstrate` so subsequent
//! compile/query handlers refuse with `Error::WorkspaceOrphaned`.
//!
//! # Why this exists
//!
//! Pre-Slice 3, `rm -rf .thinkingroot/` underneath a running daemon
//! produced opaque 500-class errors on the next CozoDB read — the
//! daemon kept routing requests against a vanished substrate until it
//! happened to fail.  CLAUDE.md §honesty rule §1 forbids that silent
//! degradation.  The watcher surfaces the deletion as a real-time
//! event so subscribed UIs (desktop, future MCP clients) can react,
//! and arms the typed `Error::WorkspaceOrphaned` so handlers refuse
//! loudly instead of returning corrupt-looking data.
//!
//! # Safety
//!
//! - The watcher never auto-recreates `.thinkingroot/` — that would be
//!   silent recovery (a `rm` could indicate the user wanted to start
//!   over, not have the daemon restore stale bytes).
//! - The watcher polls `current_workspace_root` instead of being
//!   pushed updates, so it can ride the existing `RwLock<Option<PathBuf>>`
//!   contract on `AppState` without forcing every mount handler to
//!   notify the watcher explicitly.
//! - Heartbeats fire every `heartbeat_secs` so SSE consumers can tell
//!   "no events" from "watcher dead" without reading server-side
//!   telemetry.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{broadcast, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use thinkingroot_core::types::{WorkspaceEvent, WorkspaceState};

/// Default polling cadence for re-reading `current_workspace_root`.
/// 500ms keeps the response to a mount-flip user-imperceptibly fast
/// while staying gentle on the runtime.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Default heartbeat cadence. 30s mirrors the SSE keep-alive we already
/// emit on `/branches/{branch}/events/stream`.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(30);

/// Tunables for [`spawn_workspace_watcher`]. `Default::default()` is
/// the production setting; tests override the intervals.
#[derive(Debug, Clone, Copy)]
pub struct WatcherConfig {
    /// How often to re-read `current_workspace_root` and re-attach if
    /// the active root changed.
    pub poll_interval: Duration,
    /// How often to emit a `Heartbeat` event when the watcher is
    /// healthy.
    pub heartbeat: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            heartbeat: DEFAULT_HEARTBEAT,
        }
    }
}

/// Live handle returned by [`spawn_workspace_watcher`]. The caller
/// keeps it alive for the lifetime of the daemon and aborts via
/// [`Self::cancel`] on shutdown.
pub struct WatcherHandle {
    /// Broadcast sender; subscribe with `tx.subscribe()` from the SSE
    /// handler to fan-out events to clients.
    pub tx: broadcast::Sender<WorkspaceEvent>,
    /// Live workspace state, mutated by the watcher when
    /// `.thinkingroot/` disappears.
    pub state: Arc<RwLock<WorkspaceState>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl WatcherHandle {
    /// Trip the cancellation token and join the underlying task.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
    /// Cancel without awaiting (caller is responsible for joining if
    /// they care).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// Spawn the workspace watcher.
///
/// `current_root` is a closure handed in by the caller (typically
/// `|| state.current_workspace_root()`) so this module stays free of a
/// cyclic dep on `AppState`.
pub fn spawn_workspace_watcher<F>(
    mut current_root: F,
    cfg: WatcherConfig,
) -> WatcherHandle
where
    F: FnMut() -> Option<PathBuf> + Send + 'static,
{
    let (tx, _rx) = broadcast::channel::<WorkspaceEvent>(64);
    let state = Arc::new(RwLock::new(WorkspaceState::Active));
    let cancel = CancellationToken::new();

    let tx_task = tx.clone();
    let state_task = state.clone();
    let cancel_task = cancel.clone();

    let task = tokio::spawn(async move {
        // ── Watcher state captured between ticks. ──────────────────
        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();

        let mut watcher: Option<RecommendedWatcher> = None;
        let mut current: Option<PathBuf> = None;
        let mut last_heartbeat = std::time::Instant::now();

        loop {
            if cancel_task.is_cancelled() {
                break;
            }

            // ── 1. Reconcile the active root. ──────────────────────
            let observed = current_root();
            if observed != current {
                if let (Some(_), Some(w)) = (current.as_ref(), watcher.as_mut()) {
                    // Best-effort unwatch of previous root; ignore the
                    // error — the path may already be gone.
                    if let Some(prev) = current.as_ref() {
                        let _ = w.unwatch(prev.as_path());
                    }
                }
                if let Some(new_root) = observed.as_ref() {
                    let new_engine = new_root.join(".thinkingroot");
                    let need_new_watcher = watcher.is_none();
                    if need_new_watcher {
                        let inner_tx = notify_tx.clone();
                        let built = notify::recommended_watcher(
                            move |res: notify::Result<notify::Event>| {
                                let _ = inner_tx.send(res);
                            },
                        );
                        match built {
                            Ok(w) => watcher = Some(w),
                            Err(e) => {
                                tracing::warn!(target: "fs_watch", "build watcher: {e}");
                                // Fall through; we'll retry next tick.
                            }
                        }
                    }
                    if let Some(w) = watcher.as_mut()
                        && new_engine.exists()
                    {
                        match w.watch(&new_engine, RecursiveMode::Recursive) {
                            Ok(()) => {
                                tracing::info!(target: "fs_watch", "attached to {}", new_engine.display());
                                {
                                    let mut s = state_task.write().await;
                                    *s = WorkspaceState::Active;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(target: "fs_watch", "watch {}: {e}", new_engine.display());
                            }
                        }
                    }
                }
                current = observed.clone();
            }

            // ── 2. Drain notify events. ───────────────────────────
            while let Ok(res) = notify_rx.try_recv() {
                match res {
                    Ok(event) => publish_event(&tx_task, &state_task, &current, event).await,
                    Err(e) => tracing::warn!(target: "fs_watch", "notify error: {e}"),
                }
            }

            // ── 3. Detect whole-dir deletion. notify-rs on macOS
            //      sometimes drops the parent-deletion event when the
            //      watched dir vanishes; cross-check by stat.
            if let Some(root) = current.as_ref() {
                let engine = root.join(".thinkingroot");
                if !engine.exists() {
                    let already_orphan = matches!(
                        *state_task.read().await,
                        WorkspaceState::OrphanedSubstrate
                    );
                    if !already_orphan {
                        let mut s = state_task.write().await;
                        *s = WorkspaceState::OrphanedSubstrate;
                        let _ = tx_task.send(WorkspaceEvent::DotThinkingrootDeleted {
                            workspace_root: root.clone(),
                        });
                        tracing::warn!(
                            target: "fs_watch",
                            workspace_root = %root.display(),
                            "workspace orphaned: .thinkingroot/ missing"
                        );
                    }
                }
            }

            // ── 4. Periodic heartbeat. ────────────────────────────
            if last_heartbeat.elapsed() >= cfg.heartbeat {
                let _ = tx_task.send(WorkspaceEvent::Heartbeat);
                last_heartbeat = std::time::Instant::now();
            }

            tokio::time::sleep(cfg.poll_interval).await;
        }
        tracing::info!(target: "fs_watch", "watcher shutting down");
    });

    WatcherHandle {
        tx,
        state,
        cancel,
        task,
    }
}

/// Translate a raw `notify::Event` into the typed `WorkspaceEvent`
/// variants the API surfaces.
async fn publish_event(
    tx: &broadcast::Sender<WorkspaceEvent>,
    state: &Arc<RwLock<WorkspaceState>>,
    current: &Option<PathBuf>,
    event: notify::Event,
) {
    let Some(root) = current.as_ref() else {
        return;
    };
    let engine = root.join(".thinkingroot");
    let graph_db = engine.join("graph").join("graph.db");
    let config_toml = engine.join("config.toml");

    let removal = matches!(event.kind, EventKind::Remove(_));
    for path in &event.paths {
        if removal && (path == &engine || path.starts_with(&engine) && path == &engine) {
            // Whole-dir removal — handled also by the stat fallback.
            continue;
        }
        if removal && path == &graph_db {
            let _ = tx.send(WorkspaceEvent::GraphFileMissing { path: path.clone() });
            continue;
        }
        if path == &config_toml && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
        {
            let _ = tx.send(WorkspaceEvent::ConfigModified { path: path.clone() });
            continue;
        }
    }
    // If the engine dir itself was removed during this batch, mark
    // orphan inline so we don't wait a poll cycle.
    if removal && !engine.exists() {
        let mut s = state.write().await;
        if *s != WorkspaceState::OrphanedSubstrate {
            *s = WorkspaceState::OrphanedSubstrate;
            let _ = tx.send(WorkspaceEvent::DotThinkingrootDeleted {
                workspace_root: root.clone(),
            });
        }
    }
}

/// Helper exported for tests + handlers: returns true when the typed
/// state forbids further stateful operations.
#[inline]
pub fn is_orphaned(state: WorkspaceState) -> bool {
    matches!(state, WorkspaceState::OrphanedSubstrate)
}

/// Build the typed [`Error::WorkspaceOrphaned`] for handlers to return
/// when [`is_orphaned`] flips. Centralised so the message stays
/// consistent across REST + MCP surfaces.
pub fn orphaned_error(workspace_root: &Path) -> thinkingroot_core::Error {
    thinkingroot_core::Error::WorkspaceOrphaned {
        workspace_root: workspace_root.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn fast_cfg() -> WatcherConfig {
        WatcherConfig {
            poll_interval: Duration::from_millis(50),
            heartbeat: Duration::from_millis(120),
        }
    }

    #[tokio::test]
    async fn delete_dot_thinkingroot_emits_event_and_flips_state() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        let engine = ws.join(".thinkingroot");
        std::fs::create_dir_all(engine.join("graph")).unwrap();

        let ws_copy = ws.clone();
        let handle = spawn_workspace_watcher(move || Some(ws_copy.clone()), fast_cfg());
        let mut rx = handle.tx.subscribe();

        // Give the watcher a couple of ticks to attach.
        tokio::time::sleep(Duration::from_millis(120)).await;
        std::fs::remove_dir_all(&engine).unwrap();

        let mut got_orphan = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                ev = rx.recv() => {
                    if matches!(ev, Ok(WorkspaceEvent::DotThinkingrootDeleted { .. })) {
                        got_orphan = true;
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        assert!(got_orphan, "expected DotThinkingrootDeleted event");
        let st = *handle.state.read().await;
        assert_eq!(st, WorkspaceState::OrphanedSubstrate);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn unmount_workspace_stops_watching() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        std::fs::create_dir_all(ws.join(".thinkingroot")).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_cb = counter.clone();
        let ws_copy = ws.clone();
        let handle = spawn_workspace_watcher(
            move || {
                let n = counter_cb.fetch_add(1, Ordering::SeqCst);
                if n < 4 {
                    Some(ws_copy.clone())
                } else {
                    None
                }
            },
            fast_cfg(),
        );

        // Give it time to flip to None and then to drop the watcher.
        tokio::time::sleep(Duration::from_millis(400)).await;
        std::fs::remove_dir_all(ws.join(".thinkingroot")).unwrap();
        // No assertion on rx here — the watcher should NOT emit a deletion
        // event because it has unmounted the workspace.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let st = *handle.state.read().await;
        // State stays Active after unmount (the daemon doesn't conflate
        // unmount with orphan).
        assert_eq!(st, WorkspaceState::Active);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeat_arrives_periodically() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        std::fs::create_dir_all(ws.join(".thinkingroot")).unwrap();

        let ws_copy = ws.clone();
        let handle = spawn_workspace_watcher(move || Some(ws_copy.clone()), fast_cfg());
        let mut rx = handle.tx.subscribe();

        let mut beats = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                ev = rx.recv() => {
                    if matches!(ev, Ok(WorkspaceEvent::Heartbeat)) {
                        beats += 1;
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if beats >= 2 {
                break;
            }
        }
        assert!(beats >= 2, "expected ≥2 heartbeats in 1s, got {beats}");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn is_orphaned_helper() {
        assert!(is_orphaned(WorkspaceState::OrphanedSubstrate));
        assert!(!is_orphaned(WorkspaceState::Active));
    }

    #[tokio::test]
    async fn orphaned_error_carries_workspace_root() {
        let err = orphaned_error(Path::new("/abs/ws"));
        let msg = err.to_string();
        assert!(msg.contains("/abs/ws"));
        assert!(msg.contains("orphaned"));
    }
}
