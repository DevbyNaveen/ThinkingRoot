//! Per-workspace live sync — debounced auto-compile when sources drift.
//!
//! Wired from the source-tree → [`WorkspaceStatusMsg::FsChanged`] bridge.
//! Policy lives in each workspace's `[compilation]` config (`auto_sync`,
//! default **on**). Compiles always go through [`crate::rest::run_unified_compile`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use thinkingroot_core::config::{CompilationConfig, Config};
use thinkingroot_core::restart_state::RestartState;
use thinkingroot_core::types::{SourcesState, WorkspaceStatus};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::rest::{AppState, UnifiedCompileRequest, run_unified_compile};
use crate::workspace_state::Msg as WorkspaceStatusMsg;
use crate::workspace_watcher::DEFAULT_SOURCE_DEBOUNCE_MS;

/// Background compile scheduler keyed by workspace name.
pub struct LiveSyncScheduler {
    state: Weak<AppState>,
    slots: Mutex<HashMap<String, WorkspaceSlot>>,
}

struct WorkspaceSlot {
    root_path: PathBuf,
    debounce: Option<JoinHandle<()>>,
    in_flight: Option<InFlightCompile>,
}

struct InFlightCompile {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl LiveSyncScheduler {
    pub fn new(state: Weak<AppState>) -> Arc<Self> {
        Arc::new(Self {
            state,
            slots: Mutex::new(HashMap::new()),
        })
    }

    fn app_state(&self) -> Arc<AppState> {
        self.state
            .upgrade()
            .expect("AppState must outlive LiveSyncScheduler")
    }

    /// Track a mounted workspace (idempotent).
    pub async fn register_workspace(&self, name: &str, root_path: PathBuf) {
        let mut slots = self.slots.lock().await;
        slots
            .entry(name.to_string())
            .and_modify(|s| s.root_path = root_path.clone())
            .or_insert_with(|| WorkspaceSlot {
                root_path,
                debounce: None,
                in_flight: None,
            });
    }

    /// Drop scheduler state for an unmounted workspace and cancel tasks.
    pub async fn unregister_workspace(&self, name: &str) {
        let mut slots = self.slots.lock().await;
        if let Some(mut slot) = slots.remove(name) {
            if let Some(d) = slot.debounce.take() {
                d.abort();
            }
            if let Some(f) = slot.in_flight.take() {
                f.cancel.cancel();
                f.task.abort();
            }
        }
    }

    /// Called after `FsChanged` has refreshed `sources` on the status actor.
    pub async fn on_sources_changed(self: &Arc<Self>, workspace: &str, root_path: PathBuf) {
        self.register_workspace(workspace, root_path.clone()).await;

        let state = self.app_state();
        let auto_sync = compilation_config_from_disk(&root_path).auto_sync;
        if !auto_sync {
            return;
        }

        if self.compile_breaker_blocks(workspace).await {
            tracing::info!(
                target: "live_sync",
                workspace,
                "auto-sync skipped — compile circuit breaker active"
            );
            return;
        }

        let stale = self.sources_stale(&state, workspace).await;
        if !stale {
            return;
        }

        let debounce_ms =
            compilation_config_from_disk(&root_path).effective_auto_sync_debounce_ms(
                DEFAULT_SOURCE_DEBOUNCE_MS,
            );

        Arc::clone(self)
            .schedule_debounced_compile(state, workspace, root_path, debounce_ms)
            .await;
    }

    async fn sources_stale(&self, state: &AppState, workspace: &str) -> bool {
        let Some(actor) = state.workspace_status.get(workspace).await else {
            return false;
        };
        let status: WorkspaceStatus = actor.current().await;
        match status.sources {
            SourcesState::Some {
                fingerprint_match: false,
                ..
            } => true,
            SourcesState::Some { fingerprint_match: true, .. } => false,
            SourcesState::None => true,
        }
    }

    async fn compile_breaker_blocks(&self, _workspace: &str) -> bool {
        let rs = RestartState::load().unwrap_or_default();
        rs.compile_breaker_active()
    }

    async fn schedule_debounced_compile(
        self: Arc<Self>,
        state: Arc<AppState>,
        workspace: &str,
        root_path: PathBuf,
        debounce_ms: u64,
    ) {
        let scheduler = Arc::clone(&self);
        let ws = workspace.to_string();

        let mut slots = self.slots.lock().await;
        let slot = slots
            .entry(ws.clone())
            .or_insert_with(|| WorkspaceSlot {
                root_path: root_path.clone(),
                debounce: None,
                in_flight: None,
            });
        slot.root_path = root_path;

        if let Some(d) = slot.debounce.take() {
            d.abort();
        }

        if let Some(mut flight) = slot.in_flight.take() {
            flight.cancel.cancel();
            flight.task.abort();
        }

        state
            .workspace_status
            .dispatch(
                &ws,
                slot.root_path.clone(),
                WorkspaceStatusMsg::CompileQueued {
                    reason: "auto_sync".into(),
                },
            )
            .await;

        let debounce = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
            scheduler.run_compile_now(&ws).await;
        });
        slot.debounce = Some(debounce);
    }

    async fn run_compile_now(self: Arc<Self>, workspace: &str) {
        let state = self.app_state();
        {
            let mut slots = self.slots.lock().await;
            let Some(slot) = slots.get_mut(workspace) else {
                return;
            };
            slot.debounce = None;
            if slot.in_flight.is_some() {
                return;
            }
        }

        if self.compile_breaker_blocks(workspace).await {
            return;
        }
        if !self.sources_stale(&state, workspace).await {
            return;
        }

        let root_path = {
            let slots = self.slots.lock().await;
            let from_slot = slots.get(workspace).map(|s| s.root_path.clone());
            if from_slot.is_some() {
                from_slot
            } else {
                let engine = state.engine.read().await;
                engine.workspace_root_path(workspace)
            }
        };
        let Some(root_path) = root_path else {
            return;
        };

        let cancel = CancellationToken::new();
        let state = Arc::clone(&state);
        let ws = workspace.to_string();
        let cancel_for_task = cancel.clone();

        let scheduler = Arc::clone(&self);
        let task = tokio::spawn(async move {
            let req = UnifiedCompileRequest {
                ws_url_alias: ws.clone(),
                root_path: root_path.clone(),
                branch: None,
                no_rooting: false,
            };
            let (_name, outcome) =
                run_unified_compile(state, req, None, cancel_for_task).await;
            tracing::debug!(
                target: "live_sync",
                workspace = %ws,
                ?outcome,
                "auto-sync compile finished"
            );
            scheduler.clear_in_flight(&ws).await;
        });

        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(workspace) {
            slot.in_flight = Some(InFlightCompile { cancel, task });
        }
    }

    async fn clear_in_flight(&self, workspace: &str) {
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(workspace) {
            slot.in_flight = None;
        }
    }

    /// Poll in-flight compiles and clear slots when tasks complete.
    pub async fn reap_completed(&self) {
        let mut slots = self.slots.lock().await;
        for slot in slots.values_mut() {
            if let Some(flight) = slot.in_flight.as_ref()
                && flight.task.is_finished()
            {
                if let Some(mut f) = slot.in_flight.take() {
                    let _ = (&mut f.task).await;
                }
            }
        }
    }
}

/// Subscribe to watcher `SourceChanged` events and drive live sync.
pub fn spawn_live_sync_bridge(state: Arc<AppState>, scheduler: Arc<LiveSyncScheduler>) {
    let watcher = state.workspace_watcher.clone();
    tokio::spawn(async move {
        let mut rx = loop {
            let guard = watcher.read().await;
            let Some(handle) = guard.as_ref() else {
                drop(guard);
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            break handle.tx.subscribe();
        };

        let mut reap = tokio::time::interval(Duration::from_secs(2));
        reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reap.tick().await;

        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Ok(thinkingroot_core::types::WorkspaceEvent::SourceChanged {
                            workspace_root,
                            ..
                        }) => {
                            let snapshots = state.workspace_status.snapshot_all().await;
                            let matches: Vec<(String, PathBuf)> = snapshots
                                .into_iter()
                                .filter(|s| s.path == workspace_root)
                                .map(|s| (s.name, s.path))
                                .collect();
                            for (name, path) in matches {
                                state
                                    .workspace_status
                                    .dispatch(&name, path.clone(), WorkspaceStatusMsg::FsChanged)
                                    .await;
                                scheduler.clone().on_sources_changed(&name, path).await;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                target: "live_sync",
                                skipped = n,
                                "live-sync bridge lagged on source events"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = reap.tick() => {
                    scheduler.reap_completed().await;
                }
            }
        }
        tracing::info!(target: "live_sync", "live-sync bridge shutting down");
    });
}

fn compilation_config_from_disk(root: &std::path::Path) -> CompilationConfig {
    Config::load_merged(root)
        .map(|c| c.compilation)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::config::CompilationConfig;

    #[test]
    fn compilation_config_defaults_auto_sync_off() {
        // Auto-sync defaults OFF — it self-loops in the cloud (compile writes
        // into the watched workspace → watcher recompiles → ∞). Opt in explicitly.
        let c = CompilationConfig::default();
        assert!(!c.auto_sync);
        assert_eq!(
            c.effective_auto_sync_debounce_ms(200),
            200,
            "zero debounce uses daemon default"
        );
    }

    #[test]
    fn compilation_config_honors_custom_debounce() {
        let c = CompilationConfig {
            auto_sync_debounce_ms: 500,
            ..CompilationConfig::default()
        };
        assert_eq!(c.effective_auto_sync_debounce_ms(200), 500);
    }
}
