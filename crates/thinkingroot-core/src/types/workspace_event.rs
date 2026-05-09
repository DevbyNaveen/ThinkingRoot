//! Slice 3 — file-system events surfaced by the serve-side workspace
//! watcher.  Emitted as SSE frames on `GET /api/v1/ws/{ws}/events/stream`.
//!
//! These are intentionally lightweight: each variant carries the
//! information a UI needs to react ("the substrate vanished — show a
//! re-mount banner") without leaking the underlying `notify::Event`
//! shape.  Variants are `serde(tag = "kind", rename_all = "snake_case")`
//! so the wire format is stable across renames.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Lifecycle of a workspace as observed by the serve-side watcher.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    /// `.thinkingroot/` was removed at the given root. The daemon
    /// transitions to [`WorkspaceState::OrphanedSubstrate`] and refuses
    /// subsequent compile/query for that workspace until the user
    /// re-mounts.
    DotThinkingrootDeleted { workspace_root: PathBuf },
    /// `.thinkingroot/graph/graph.db` is missing under the active root
    /// even though `.thinkingroot/` itself still exists.  Distinct from
    /// [`Self::DotThinkingrootDeleted`] because the user-facing remedy
    /// differs ("run `root compile`" vs "re-mount the pack").
    GraphFileMissing { path: PathBuf },
    /// Workspace `config.toml` was modified — sidecar should reload
    /// provider settings on next request.  No immediate action is
    /// taken; the event is emitted as a hint.
    ConfigModified { path: PathBuf },
    /// Heartbeat emitted every `heartbeat_secs` while the watcher is
    /// healthy.  Lets clients distinguish "no events" from "watcher
    /// dead" without reading server-side telemetry.
    Heartbeat,
}

/// Coarse-grained workspace lifecycle state, derived from the watcher.
/// Read by the serve handlers via [`AppState::workspace_state`] (defined
/// in `thinkingroot-serve::rest`) to refuse compile/query when the
/// substrate is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// Workspace mounted and substrate present.
    Active,
    /// `.thinkingroot/` is missing — refuse stateful operations until
    /// re-mounted.
    OrphanedSubstrate,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::Active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn workspace_event_round_trip_through_json() {
        let ev = WorkspaceEvent::DotThinkingrootDeleted {
            workspace_root: PathBuf::from("/tmp/ws"),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("dot_thinkingroot_deleted"));
        let back: WorkspaceEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn workspace_event_heartbeat_serialises_without_payload() {
        let ev = WorkspaceEvent::Heartbeat;
        let s = serde_json::to_string(&ev).unwrap();
        assert_eq!(s, r#"{"kind":"heartbeat"}"#);
    }

    #[test]
    fn workspace_state_default_is_active() {
        assert_eq!(WorkspaceState::default(), WorkspaceState::Active);
    }
}
