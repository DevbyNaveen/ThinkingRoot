//! Restart-state tracker for the daemon auto-restart subsystem.
//! Persisted at `<config_dir>/thinkingroot/restart-state.json`.
//!
//! Spec: `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md` §7.
//!
//! The desktop's sidecar watchdog records each daemon crash here and
//! consults `cap_reached()` before deciding whether to auto-restart.
//! Slice F T2 wires the crash + respawn recording paths; Slice F T3
//! will wire the circuit breaker (`circuit_breaker_until`) trip + the
//! auto-clear-after-5m logic. The field is reserved here so both ships
//! read the same on-disk schema.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

/// Window for "consecutive failures" — only failures within this
/// window contribute to the restart cap.  Spec §7.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// Maximum number of restart attempts within FAILURE_WINDOW before
/// we surface RepairNeeded.
pub const MAX_ATTEMPTS: usize = 4;

/// Backoff schedule for the Nth restart attempt (1-indexed).
/// Spec §7: 0ms, 500ms, 2s, 5s; subsequent calls cap at 5s.
pub fn backoff_for_attempt(attempt: usize) -> Duration {
    match attempt {
        0 | 1 => Duration::from_millis(0),
        2 => Duration::from_millis(500),
        3 => Duration::from_secs(2),
        _ => Duration::from_secs(5),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartAttempt {
    pub ts: DateTime<Utc>,
    pub outcome: AttemptOutcome,
    /// Set on `Respawned` outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Set on `Crash` outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// Daemon crashed (non-zero exit or terminating signal).
    Crash,
    /// Daemon respawn succeeded — sidecar manager observed /livez.
    Respawned,
    /// Respawn attempt failed (Command::spawn failed or /livez never
    /// came up within the readiness window).
    SpawnFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartState {
    pub schema_version: u32,
    pub attempts: Vec<RestartAttempt>,
    /// When set, no further auto-restart until this timestamp passes.
    /// Slice F T3 wires the trip + auto-clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_breaker_until: Option<DateTime<Utc>>,
}

impl Default for RestartState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RestartStateError {
    #[error("config directory unavailable (HOME unset?)")]
    NoConfigDir,
    #[error("restart-state I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("restart-state parse error: {0}")]
    Parse(#[from] serde_json::Error),
}

pub fn path() -> Result<PathBuf, RestartStateError> {
    let config_dir = dirs::config_dir().ok_or(RestartStateError::NoConfigDir)?;
    Ok(config_dir.join("thinkingroot").join("restart-state.json"))
}

impl RestartState {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            attempts: Vec::new(),
            circuit_breaker_until: None,
        }
    }

    /// Load from disk.  Corrupt file → fresh state (best-effort).
    /// Missing file → fresh state.
    pub fn load() -> Result<Self, RestartStateError> {
        let path = path()?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(e.into()),
        };
        if bytes.is_empty() {
            return Ok(Self::new());
        }
        match serde_json::from_slice(&bytes) {
            Ok(state) => Ok(state),
            Err(e) => {
                tracing::warn!(error = %e, "restart-state corrupt; resetting");
                Ok(Self::new())
            }
        }
    }

    /// Save atomically via tempfile+persist.
    pub fn save(&self) -> Result<(), RestartStateError> {
        let path = path()?;
        let parent = path
            .parent()
            .expect("restart-state always has a parent dir");
        std::fs::create_dir_all(parent)?;

        let tmp = tempfile::NamedTempFile::new_in(parent)?;
        let json = serde_json::to_string_pretty(self)?;
        {
            use std::io::Write;
            let mut handle = tmp.as_file().try_clone()?;
            handle.write_all(json.as_bytes())?;
            handle.sync_all()?;
        }
        tmp.persist(&path)
            .map_err(|e| RestartStateError::Io(e.error))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }
        Ok(())
    }

    /// Prune attempts older than FAILURE_WINDOW.  Call before every
    /// read of `recent_failure_count`.
    pub fn prune(&mut self) {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(FAILURE_WINDOW)
                .expect("FAILURE_WINDOW fits chrono::Duration");
        self.attempts.retain(|a| a.ts >= cutoff);
    }

    /// Count of `Crash` + `SpawnFailed` outcomes in the recent window.
    /// Call `prune()` first.
    pub fn recent_failure_count(&self) -> usize {
        self.attempts
            .iter()
            .filter(|a| {
                matches!(
                    a.outcome,
                    AttemptOutcome::Crash | AttemptOutcome::SpawnFailed
                )
            })
            .count()
    }

    /// True if we've hit the cap.  Caller surfaces RepairNeeded.
    pub fn cap_reached(&self) -> bool {
        self.recent_failure_count() >= MAX_ATTEMPTS
    }

    pub fn record_crash(&mut self, exit_code: Option<i32>) {
        self.attempts.push(RestartAttempt {
            ts: Utc::now(),
            outcome: AttemptOutcome::Crash,
            pid: None,
            exit_code,
        });
    }

    pub fn record_respawn(&mut self, pid: u32) {
        self.attempts.push(RestartAttempt {
            ts: Utc::now(),
            outcome: AttemptOutcome::Respawned,
            pid: Some(pid),
            exit_code: None,
        });
    }

    pub fn record_spawn_failed(&mut self) {
        self.attempts.push(RestartAttempt {
            ts: Utc::now(),
            outcome: AttemptOutcome::SpawnFailed,
            pid: None,
            exit_code: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_matches_spec() {
        assert_eq!(backoff_for_attempt(1), Duration::from_millis(0));
        assert_eq!(backoff_for_attempt(2), Duration::from_millis(500));
        assert_eq!(backoff_for_attempt(3), Duration::from_secs(2));
        assert_eq!(backoff_for_attempt(4), Duration::from_secs(5));
        assert_eq!(backoff_for_attempt(99), Duration::from_secs(5));
    }

    #[test]
    fn prune_removes_old_attempts() {
        let mut state = RestartState::new();
        let old_ts = Utc::now() - chrono::Duration::seconds(120);
        state.attempts.push(RestartAttempt {
            ts: old_ts,
            outcome: AttemptOutcome::Crash,
            pid: None,
            exit_code: Some(1),
        });
        state.attempts.push(RestartAttempt {
            ts: Utc::now(),
            outcome: AttemptOutcome::Crash,
            pid: None,
            exit_code: Some(1),
        });
        state.prune();
        assert_eq!(state.attempts.len(), 1);
    }

    #[test]
    fn cap_reached_at_max_attempts() {
        let mut state = RestartState::new();
        for _ in 0..MAX_ATTEMPTS {
            state.record_crash(Some(139));
        }
        assert!(state.cap_reached());
    }

    #[test]
    fn record_respawn_does_not_count_toward_cap() {
        let mut state = RestartState::new();
        state.record_crash(Some(139));
        state.record_respawn(12345);
        state.record_crash(Some(139));
        state.prune();
        // 2 crashes + 1 respawn = 2 failures, under MAX_ATTEMPTS.
        assert_eq!(state.recent_failure_count(), 2);
        assert!(!state.cap_reached());
    }
}
