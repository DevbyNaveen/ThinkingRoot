//! Recovery audit log — append-only JSONL record of every self-heal
//! action taken by the daemon, CLI, or desktop sidecar manager.
//!
//! Spec: `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md` §7.
//!
//! Path: `<dirs::config_dir()>/thinkingroot/recovery.log`. Mode 0600
//! on Unix.  10 MiB rotation cap — when the file exceeds 10 MiB it
//! is renamed to `recovery.log.1` (previous `.1` is overwritten) and
//! a fresh log starts.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Rotation threshold.  10 MiB matches the spec.
pub const ROTATION_THRESHOLD_BYTES: u64 = 10 * 1024 * 1024;

/// One self-heal action.  `action` discriminates the variant; each
/// variant carries action-specific fields flattened into the JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEvent {
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: RecoveryEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecoveryEventKind {
    /// Spawning the daemon — used for both clean spawn and restart.
    Respawn {
        attempt: u32,
        backoff_ms: u64,
        reason: String,
    },
    /// Respawn completed.
    RespawnOk { new_pid: u32 },
    /// Lockfile with dead PID was removed.
    StaleLockCleanup { dead_pid: u32 },
    /// Port `from` was held by a non-cortex process; advanced to `to`.
    PortAdvance {
        from: u16,
        to: u16,
        reason: String,
    },
    /// Install manifest was rebuilt from disk scan after corruption.
    ManifestRebuild { binaries_found: usize },
    /// Circuit breaker tripped — 5 failures in 60s OR 3 crash signals.
    CircuitBreakerTripped {
        consecutive_failures: u32,
        until_rfc3339: String,
    },
    /// Circuit breaker was reset (manually or auto after timeout).
    CircuitBreakerReset { reason: String },
    /// Binary corruption detected via BLAKE3 mismatch.
    BinaryChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    /// A workspace compile failed (pipeline error, not a process crash).
    CompileFailed {
        workspace: String,
        error: String,
        retry_attempt: u32,
    },
    /// A compile retry was scheduled after a failure.
    CompileRetryScheduled {
        workspace: String,
        attempt: u32,
        backoff_ms: u64,
    },
    /// Compile-scoped circuit breaker tripped — too many failures in window.
    CompileBreakerTripped {
        workspace: String,
        consecutive_failures: u32,
        until_rfc3339: String,
    },
    /// A retried compile completed successfully.
    CompileRecovered {
        workspace: String,
        retry_attempt: u32,
    },
}

impl RecoveryEvent {
    pub fn respawn_attempt(attempt: u32, backoff: std::time::Duration) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::Respawn {
                attempt,
                backoff_ms: backoff.as_millis() as u64,
                reason: "crash_detected".to_string(),
            },
        }
    }

    pub fn respawn_ok(new_pid: u32) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::RespawnOk { new_pid },
        }
    }

    pub fn stale_lock_cleanup(dead_pid: u32) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::StaleLockCleanup { dead_pid },
        }
    }

    pub fn port_advance(from: u16, to: u16, reason: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::PortAdvance {
                from,
                to,
                reason: reason.into(),
            },
        }
    }

    pub fn manifest_rebuild(binaries_found: usize) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::ManifestRebuild { binaries_found },
        }
    }

    pub fn circuit_breaker_tripped(consecutive_failures: u32, until: DateTime<Utc>) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::CircuitBreakerTripped {
                consecutive_failures,
                until_rfc3339: until.to_rfc3339(),
            },
        }
    }

    pub fn circuit_breaker_reset(reason: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::CircuitBreakerReset {
                reason: reason.into(),
            },
        }
    }

    pub fn binary_checksum_mismatch(
        path: PathBuf,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::BinaryChecksumMismatch {
                path,
                expected: expected.into(),
                actual: actual.into(),
            },
        }
    }

    pub fn compile_failed(
        workspace: impl Into<String>,
        error: impl Into<String>,
        retry_attempt: u32,
    ) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::CompileFailed {
                workspace: workspace.into(),
                error: error.into(),
                retry_attempt,
            },
        }
    }

    pub fn compile_retry_scheduled(
        workspace: impl Into<String>,
        attempt: u32,
        backoff: std::time::Duration,
    ) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::CompileRetryScheduled {
                workspace: workspace.into(),
                attempt,
                backoff_ms: backoff.as_millis() as u64,
            },
        }
    }

    pub fn compile_breaker_tripped(
        workspace: impl Into<String>,
        consecutive_failures: u32,
        until: DateTime<Utc>,
    ) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::CompileBreakerTripped {
                workspace: workspace.into(),
                consecutive_failures,
                until_rfc3339: until.to_rfc3339(),
            },
        }
    }

    pub fn compile_recovered(workspace: impl Into<String>, retry_attempt: u32) -> Self {
        Self {
            ts: Utc::now(),
            kind: RecoveryEventKind::CompileRecovered {
                workspace: workspace.into(),
                retry_attempt,
            },
        }
    }
}

/// Resolve the on-disk recovery log path.
pub fn log_path() -> Result<PathBuf, LogError> {
    let config_dir = dirs::config_dir().ok_or(LogError::NoConfigDir)?;
    Ok(config_dir.join("thinkingroot").join("recovery.log"))
}

/// All errors from log operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LogError {
    #[error("config directory unavailable (HOME unset?)")]
    NoConfigDir,
    #[error("log I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("log serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Append one event to the log.  Atomic at the line level: writes
/// the full JSON + newline in a single `write_all` call, with the
/// file opened in append mode (POSIX guarantees < PIPE_BUF writes
/// to append-mode files are atomic; our lines are well under that).
///
/// Rotates the log to `recovery.log.1` if it exceeds 10 MiB BEFORE
/// writing the new event.
pub fn append(event: &RecoveryEvent) -> Result<(), LogError> {
    let path = log_path()?;
    let parent = path
        .parent()
        .expect("recovery.log always has a parent dir");
    std::fs::create_dir_all(parent)?;

    // Rotate if oversized.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() >= ROTATION_THRESHOLD_BYTES {
            let rotated = parent.join("recovery.log.1");
            // Best-effort: ignore errors so we never fail to log
            // because we couldn't rotate.
            let _ = std::fs::rename(&path, &rotated);
        }
    }

    let line = serde_json::to_string(event)?;
    let mut bytes = line.into_bytes();
    bytes.push(b'\n');

    // POSIX guarantees `write(2)` on `O_APPEND` is atomic at the line
    // level when the line is < PIPE_BUF (typically 4096 bytes). A
    // single `write_all` therefore lands as one contiguous record
    // even under concurrent appenders — no flock needed. We
    // intentionally skip `sync_all()` (fsync): the recovery log is
    // observability infrastructure, not a correctness gate. The
    // doctor surface re-builds from `restart_state.json` on next
    // boot if the log is truncated by a crash. fsync adds 1-15 ms
    // of latency per append, which compounds on noisy debug paths.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(&bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }

    Ok(())
}

/// Read the last `n` events in chronological order (oldest first).
/// Used by `root doctor --recovery-log [N]` and the desktop's
/// blocking-panel "Open logs" action.
pub fn tail(n: usize) -> Result<Vec<RecoveryEvent>, LogError> {
    let path = log_path()?;
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut events: Vec<RecoveryEvent> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if events.len() > n {
        let drop = events.len() - n;
        events.drain(..drop);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_event_serializes_with_stable_field_names() {
        let ev = RecoveryEvent::respawn_attempt(2, std::time::Duration::from_millis(500));
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"action\":\"respawn\""), "got: {json}");
        assert!(json.contains("\"attempt\":2"), "got: {json}");
        assert!(json.contains("\"backoff_ms\":500"), "got: {json}");
    }

    #[test]
    fn compile_failed_serializes_with_workspace_and_retry() {
        let ev = RecoveryEvent::compile_failed("home-notes", "boom", 1);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"action\":\"compile_failed\""), "got: {json}");
        assert!(json.contains("\"workspace\":\"home-notes\""), "got: {json}");
        assert!(json.contains("\"retry_attempt\":1"), "got: {json}");
    }

    #[test]
    fn compile_retry_scheduled_carries_backoff() {
        let ev = RecoveryEvent::compile_retry_scheduled(
            "home-notes",
            2,
            std::time::Duration::from_millis(500),
        );
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("\"action\":\"compile_retry_scheduled\""),
            "got: {json}"
        );
        assert!(json.contains("\"backoff_ms\":500"), "got: {json}");
    }

    #[test]
    fn compile_breaker_tripped_carries_until() {
        let until = Utc::now() + chrono::Duration::seconds(600);
        let ev = RecoveryEvent::compile_breaker_tripped("home-notes", 3, until);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("\"action\":\"compile_breaker_tripped\""),
            "got: {json}"
        );
        assert!(
            json.contains("\"consecutive_failures\":3"),
            "got: {json}"
        );
    }

    #[test]
    fn compile_recovered_carries_workspace() {
        let ev = RecoveryEvent::compile_recovered("home-notes", 2);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("\"action\":\"compile_recovered\""),
            "got: {json}"
        );
        assert!(json.contains("\"retry_attempt\":2"), "got: {json}");
    }

    #[test]
    fn recovery_event_stale_lock_cleanup_carries_pid() {
        let ev = RecoveryEvent::stale_lock_cleanup(8421);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("\"action\":\"stale_lock_cleanup\""),
            "got: {json}"
        );
        assert!(json.contains("\"dead_pid\":8421"), "got: {json}");
    }

    #[test]
    fn append_then_tail_round_trips() {
        // Reuse the install_manifest test_util::ENV_GUARD pattern to
        // serialize env-mutation across modules in the same lib test
        // binary.
        let _guard = crate::test_util::ENV_GUARD
            .lock()
            .expect("env guard poisoned");
        let tmp = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_appdata = std::env::var_os("APPDATA");
        // SAFETY: ENV_GUARD serializes; Drop below restores.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("APPDATA", tmp.path());
        }

        let ev1 = RecoveryEvent::respawn_attempt(1, std::time::Duration::from_millis(0));
        let ev2 = RecoveryEvent::respawn_ok(12345);
        append(&ev1).expect("append 1");
        append(&ev2).expect("append 2");

        let tailed = tail(10).expect("tail");
        assert_eq!(tailed.len(), 2);
        assert!(matches!(
            tailed[0].kind,
            RecoveryEventKind::Respawn { attempt: 1, .. }
        ));
        assert!(matches!(
            tailed[1].kind,
            RecoveryEventKind::RespawnOk { new_pid: 12345 }
        ));

        // SAFETY: see above.
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_appdata {
                Some(v) => std::env::set_var("APPDATA", v),
                None => std::env::remove_var("APPDATA"),
            }
        }
    }
}
