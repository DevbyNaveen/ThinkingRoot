//! Cortex Protocol — singleton-engine discovery primitives.
//!
//! Spec: `docs/2026-05-02-unified-singleton-runtime.md`.
//!
//! This module provides the **sync**, **runtime-free** half of the
//! Cortex Protocol: types, lockfile I/O, and PID liveness. The async
//! `resolve_engine()` that performs the HTTP `/livez` health check
//! and spawns the daemon lives in each consumer crate (CLI, Desktop)
//! so `thinkingroot-core` stays free of `tokio` and `reqwest`.
//!
//! Three load-bearing invariants:
//!
//! - **Atomic lockfile writes.** Every write goes through
//!   `tempfile::NamedTempFile::persist`, which uses `rename(2)` on
//!   POSIX (atomic) and `ReplaceFileW` on Windows (atomic). A reader
//!   can never observe a half-written or zero-byte lockfile.
//!
//! - **Advisory writer serialisation.** Two surfaces racing to write
//!   the lock both acquire an exclusive `fs2` lock on the sibling
//!   `cortex.lock.write` sentinel for the duration of the write. The
//!   second one observes the first's lock on its read-after-acquire
//!   and falls through to attach.
//!
//! - **`schema_version` is reader-bumped.** A reader on version N
//!   refuses to parse a lockfile with `schema_version > N`. This
//!   prevents an old reader from misinterpreting a future field
//!   layout — the wrong call here is to silently treat it as
//!   compatible, which is exactly the silent-corruption class
//!   Honesty Rule #1 forbids.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current schema version of the on-disk `cortex.lock`. Bumping this
/// breaks compatibility with older readers; only do so when adding a
/// field that older readers must not silently ignore.
pub const SCHEMA_VERSION: u32 = 1;

/// Canonical loopback port for the singleton engine. Chosen to avoid
/// the engine's legacy 3000 default (collides with Next.js, Rails,
/// Flask) and the cloud's 3100-grid; settable via env in tests.
pub const DEFAULT_PORT: u16 = 31760;

/// Loopback bind address. Cortex never binds to a non-loopback host;
/// enterprise on-host installs that do are out of scope (they need
/// their own auth surface, not just discovery).
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// HTTP path that the cortex health check probes.
pub const LIVENESS_PATH: &str = "/livez";

/// Provenance of the daemon — useful in diagnostics and in
/// `cortex_status` UIs. Surfaces append their own variant.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartedBy {
    /// Spawned by `root serve` (or auto-spawned by a stateful CLI
    /// command via `resolve_engine`).
    Cli,
    /// Spawned by the Tauri desktop's sidecar manager.
    Desktop,
    /// Spawned by an OS-level service manager (`launchd` /
    /// `systemd` / Windows Service).
    Service,
    /// Spawned by the Python SDK.
    PythonSdk,
    /// Spawned by the TypeScript SDK.
    TsSdk,
}

impl StartedBy {
    /// Stable string label suitable for log fields and CLI output.
    pub fn as_str(&self) -> &'static str {
        match self {
            StartedBy::Cli => "cli",
            StartedBy::Desktop => "desktop",
            StartedBy::Service => "service",
            StartedBy::PythonSdk => "python_sdk",
            StartedBy::TsSdk => "ts_sdk",
        }
    }
}

/// Why we are calling `resolve_engine`. Drives the spawn-vs-attach
/// vs error decision when no daemon is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineIntent {
    /// "I want to BE the daemon" — caller is `root serve`. If a
    /// daemon already exists, resolve_engine returns
    /// `EngineConnection::Remote` so the caller can print "engine
    /// already running" and exit cleanly. If none exists, returns
    /// `InProcess` and the caller proceeds to bind + write the lock.
    Serve,
    /// "I need an engine to talk to" — caller is `root compile`,
    /// `root query`, etc. If no daemon exists, resolve_engine
    /// auto-spawns one in a detached process group.
    Command,
    /// Same as `Command` but the caller (the desktop sidecar
    /// manager) also stores the `Child` handle so it can drive a
    /// graceful stop on app exit.
    DesktopBoot,
    /// MCP stdio mode — the caller is `root serve --mcp-stdio`,
    /// invoked over stdin/stdout by an editor. No HTTP, no lock,
    /// no daemon coordination.
    McpStdio,
}

/// What `resolve_engine` returns. `Remote` means attach; `InProcess`
/// means the caller is the daemon now; `Stdio` means bypass cortex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineConnection {
    /// A healthy daemon is running. The caller should issue HTTP
    /// requests against `host:port` and not open CozoDB locally.
    Remote {
        host: String,
        port: u16,
        started_by: StartedBy,
        pid: u32,
    },
    /// No daemon found and `intent == Serve`. Caller should bind the
    /// listener and call `write_lock` before serving traffic.
    InProcess,
    /// `intent == McpStdio`. Cortex bypassed entirely.
    Stdio,
}

/// Outcome of probing the daemon's `/livez` endpoint. Used by
/// `decide()` to disambiguate "lock says daemon is here but it's
/// actually dead" from "lock says daemon is here and it's serving."
///
/// Caller fills this in by running an async probe (e.g. via
/// `thinkingroot-cortex-async::probe_livez`) BEFORE calling
/// `decide()` — the decision function itself is sync and never
/// touches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// `/livez` returned 2xx within the timeout. `version` is the
    /// daemon's reported version (from `/api/v1/version` or
    /// `/livez` if it carries it).
    Healthy { version: String },
    /// `/livez` returned but daemon is degraded (e.g. workspace
    /// mount errors). `warnings` carries the daemon's structured
    /// degradation reasons. Still treated as "attach" by
    /// `decide()` — degraded is not stale.
    Degraded { version: String, warnings: Vec<String> },
    /// `/livez` failed to respond, returned non-2xx, or hit the
    /// timeout. The lockfile-claimed daemon is dead or wedged.
    Unhealthy,
    /// Caller did not probe (e.g. no lockfile exists, so there's
    /// nothing to probe). Distinct from `Unhealthy` so `decide()`
    /// can correctly choose "Spawn" vs "RepairNeeded".
    NotProbed,
}

/// All facts `decide()` consumes. Caller assembles this struct
/// from sync filesystem reads (`read_lock`, `process_alive`,
/// `InstallManifest::load`) plus one async probe.
///
/// Everything in here is owned by the caller — `decide` takes
/// `&self` semantics conceptually but the function consumes by
/// value to keep ownership clean across the async/sync boundary.
#[derive(Debug, Clone)]
pub struct DecisionInputs {
    /// Why the caller wants an engine.
    pub intent: EngineIntent,
    /// Current `cortex.lock` contents, or `None` if file is
    /// absent / corrupt (corrupt = treated as absent per
    /// `read_lock`'s contract).
    pub lock: Option<CortexLock>,
    /// `sysinfo::process_alive(lock.pid)` if lock is present.
    /// `false` when lock is None.
    pub lock_pid_alive: bool,
    /// Outcome of probing the lockfile's host:port/livez. Caller
    /// MUST set this to `ProbeResult::NotProbed` when no lock
    /// exists.
    pub probe_result: ProbeResult,
    /// Path to the preferred install-manifest binary, if the
    /// manifest exists AND its preferred entry exists on disk.
    /// `None` triggers `Decision::RepairNeeded` for spawn intents.
    pub manifest_preferred_binary: Option<std::path::PathBuf>,
    /// True if `--in-process` global flag was passed. Forces
    /// `Decision::InProcess` regardless of other inputs (escape
    /// hatch for hermetic CI / air-gapped scenarios).
    pub in_process_flag: bool,
}

/// What `decide()` says to do. Caller maps to its own connection
/// type (`EngineConnection` for CLI, or with `SpawnRequired` for
/// desktop's attached-spawn flow added in T3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Healthy daemon found — caller should attach via HTTP.
    Attach {
        host: String,
        port: u16,
        /// Version reported by `/livez`. Caller validates against
        /// its own `CARGO_PKG_VERSION` and refuses on skew (desktop
        /// case). CLI may attach across versions in practice but
        /// the desktop is stricter.
        version: String,
    },
    /// No healthy daemon. Caller should spawn the given binary on
    /// the given host:port, then re-probe. CLI spawns detached;
    /// desktop re-routes via `SpawnRequired` to keep Child handle.
    Spawn {
        binary_path: std::path::PathBuf,
        port: u16,
        host: String,
    },
    /// Caller is `root serve` and no daemon exists — caller
    /// becomes the daemon. Also reachable via `--in-process`.
    InProcess,
    /// `intent == McpStdio`. Cortex bypassed.
    Stdio,
    /// Cannot proceed — install-side prerequisites are missing.
    /// `failing_check_ids` carries `root doctor` check IDs that
    /// the caller surfaces (CLI: exit non-zero with these in the
    /// error message; desktop: render blocking panel).
    RepairNeeded { failing_check_ids: Vec<String> },
}

/// On-disk lockfile shape. JSON-encoded for human inspectability and
/// compatibility with non-Rust tooling (a future Python SDK reader
/// only needs `json.load`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CortexLock {
    pub schema_version: u32,
    pub pid: u32,
    pub port: u16,
    pub host: String,
    pub version: String,
    pub started_by: StartedBy,
    pub started_at: DateTime<Utc>,
    pub binary_path: PathBuf,
}

impl CortexLock {
    /// Construct a lock for the current process. Caller fills in the
    /// version + binary_path from `env!("CARGO_PKG_VERSION")` and
    /// `std::env::current_exe()`.
    pub fn new(
        port: u16,
        started_by: StartedBy,
        version: impl Into<String>,
        binary_path: PathBuf,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            pid: std::process::id(),
            port,
            host: DEFAULT_HOST.to_string(),
            version: version.into(),
            started_by,
            started_at: Utc::now(),
            binary_path,
        }
    }
}

/// Errors from cortex lockfile and PID operations.
#[derive(Debug, thiserror::Error)]
pub enum CortexError {
    /// `dirs::config_dir()` returned `None`. On a standard Linux /
    /// macOS / Windows install this can't happen; surfaces in
    /// stripped-down container images that lack `$HOME`.
    #[error("config dir unavailable (set XDG_CONFIG_HOME on Linux/macOS or APPDATA on Windows)")]
    NoConfigDir,

    /// On-disk schema is newer than this binary supports. Refusing
    /// to attach is correct here — silently mis-interpreting future
    /// fields is a Honesty Rule #1 violation.
    #[error(
        "incompatible cortex.lock schema_version {found} (this binary supports up to {max}). \
         Upgrade `root` (`root update`) or restart the newer daemon manually."
    )]
    IncompatibleLockSchema { found: u32, max: u32 },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("cortex writer-lock unavailable (another spawner is in flight)")]
    WriterLockBusy,
}

/// Filesystem path where the lockfile lives. Honours
/// `dirs::config_dir()` which respects `XDG_CONFIG_HOME` on
/// Linux/macOS and `APPDATA` on Windows — integration tests override
/// these to isolate per-test.
pub fn lock_path() -> Result<PathBuf, CortexError> {
    let dir = dirs::config_dir().ok_or(CortexError::NoConfigDir)?;
    Ok(dir.join("thinkingroot").join("cortex.lock"))
}

/// Path of the writer-sentinel file used to serialise concurrent
/// spawns. Lives next to `cortex.lock` to share filesystem semantics
/// (no cross-mount torn-rename surprises).
pub fn writer_sentinel_path() -> Result<PathBuf, CortexError> {
    let mut p = lock_path()?;
    p.set_extension("lock.write");
    Ok(p)
}

/// Read the current lockfile if present and parseable.
///
/// Returns:
/// - `Ok(Some(lock))` — file present, valid JSON, schema in range.
/// - `Ok(None)` — file absent OR present-but-corrupt. Corrupt files
///   are logged at WARN and removed by the next `write_lock` call;
///   the data is recoverable from process state so silent recovery
///   is correct here.
/// - `Err(IncompatibleLockSchema)` — file present and well-formed
///   but `schema_version` exceeds `SCHEMA_VERSION`. Attach refused.
pub fn read_lock() -> Result<Option<CortexLock>, CortexError> {
    let path = lock_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    // Empty file (caller observed mid-write between truncate + write,
    // or another process raced). Treat as absent — the writer will
    // overwrite.
    if bytes.is_empty() {
        return Ok(None);
    }

    let lock: CortexLock = match serde_json::from_slice(&bytes) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "cortex.lock unparseable; treating as stale and ignoring"
            );
            return Ok(None);
        }
    };

    if lock.schema_version > SCHEMA_VERSION {
        return Err(CortexError::IncompatibleLockSchema {
            found: lock.schema_version,
            max: SCHEMA_VERSION,
        });
    }

    Ok(Some(lock))
}

/// Atomic write of the lockfile.
///
/// Algorithm:
/// 1. Ensure parent dir exists.
/// 2. Acquire exclusive advisory lock on the writer-sentinel
///    (creates it if absent). Two surfaces racing to write both
///    serialise here; the second observes the first's lock on its
///    next `read_lock` and falls through to attach.
/// 3. Write to a tempfile in the same directory.
/// 4. `persist()` — atomic rename (POSIX) or `ReplaceFileW` (Windows).
/// 5. Release the writer-sentinel lock by dropping the file handle.
///
/// `WriterLockBusy` is returned only when the caller used
/// `try_lock_exclusive` and another spawner held the sentinel.
/// Production callers use the blocking `lock_exclusive` (via
/// `write_lock_blocking`); this `try_` variant exists for the
/// `serve` startup path that wants to fail fast rather than block on
/// an apparently wedged peer spawner.
pub fn write_lock(lock: &CortexLock) -> Result<(), CortexError> {
    write_lock_inner(lock, /*blocking=*/ true)
}

/// Try-variant — returns `WriterLockBusy` instead of blocking when
/// another spawner holds the sentinel. Used by `root serve` startup
/// to surface racing daemons quickly.
pub fn try_write_lock(lock: &CortexLock) -> Result<(), CortexError> {
    write_lock_inner(lock, /*blocking=*/ false)
}

fn write_lock_inner(lock: &CortexLock, blocking: bool) -> Result<(), CortexError> {
    use fs2::FileExt;

    let path = lock_path()?;
    let parent = path
        .parent()
        .expect("lock_path always has a thinkingroot/ parent");
    std::fs::create_dir_all(parent)?;

    let sentinel_path = writer_sentinel_path()?;
    let sentinel = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&sentinel_path)?;

    if blocking {
        // Blocks until acquired; OS releases on process death so a
        // crashed spawner cannot deadlock the next one.
        #[allow(clippy::incompatible_msrv)]
        sentinel.lock_exclusive()?;
    } else {
        #[allow(clippy::incompatible_msrv)]
        match sentinel.try_lock_exclusive() {
            Ok(()) => {}
            Err(_) => return Err(CortexError::WriterLockBusy),
        }
    }

    // RAII guard so the lock releases even if json/persist panics.
    struct SentinelGuard(std::fs::File);
    impl Drop for SentinelGuard {
        fn drop(&mut self) {
            #[allow(clippy::incompatible_msrv)]
            let _ = fs2::FileExt::unlock(&self.0);
        }
    }
    let _guard = SentinelGuard(sentinel);

    let json = serde_json::to_vec_pretty(lock)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        use std::io::Write as _;
        tmp.write_all(&json)?;
        tmp.as_file_mut().sync_data()?;
    }
    tmp.persist(&path).map_err(|e| e.error)?;

    tracing::debug!(
        port = lock.port,
        pid = lock.pid,
        started_by = lock.started_by.as_str(),
        "cortex.lock written"
    );

    Ok(())
}

/// Remove the lockfile. Idempotent — `NotFound` is not an error.
/// The sibling writer-sentinel file is left in place; it's empty,
/// 4 bytes of inode metadata, and the next spawner will reuse it.
pub fn remove_lock() -> Result<(), CortexError> {
    let path = lock_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => {
            tracing::debug!(path = %path.display(), "cortex.lock removed");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Cross-platform PID liveness. Returns `true` only if a process with
/// the given PID exists AND is not a zombie (zombies are dead
/// processes whose exit status hasn't been reaped — they no longer
/// own the listener).
///
/// Backed by `sysinfo` so the same code works on Linux, macOS, and
/// Windows without per-OS branches.
pub fn process_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};

    let mut sys = sysinfo::System::new();
    let pid = Pid::from_u32(pid);
    // ProcessRefreshKind::new() creates the lightest-weight refresh
    // descriptor (no CPU, no memory, no disk usage) — we only need
    // existence + status, so we don't pay for any sub-detail.
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::new(),
    );
    match sys.process(pid) {
        Some(p) => !matches!(p.status(), sysinfo::ProcessStatus::Zombie),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::ENV_GUARD;

    /// Override the config dir to a tempdir for the duration of the
    /// test. Restores the original on drop. Acquires the env guard
    /// so concurrent tests don't see overlapping overrides.
    struct ConfigDirOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_xdg: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        prev_appdata: Option<std::ffi::OsString>,
    }

    impl ConfigDirOverride {
        fn new() -> Self {
            let guard = ENV_GUARD.lock().expect("env guard poisoned");
            let tmp = tempfile::TempDir::new().expect("mktempdir");
            let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
            let prev_home = std::env::var_os("HOME");
            let prev_appdata = std::env::var_os("APPDATA");
            // Cover Linux (XDG), macOS (HOME), Windows (APPDATA).
            // dirs::config_dir consults each according to OS rules.
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", tmp.path());
                std::env::set_var("HOME", tmp.path());
                std::env::set_var("APPDATA", tmp.path());
            }
            Self {
                _guard: guard,
                _tmp: tmp,
                prev_xdg,
                prev_home,
                prev_appdata,
            }
        }
    }

    impl Drop for ConfigDirOverride {
        fn drop(&mut self) {
            unsafe {
                match self.prev_xdg.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
                match self.prev_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match self.prev_appdata.take() {
                    Some(v) => std::env::set_var("APPDATA", v),
                    None => std::env::remove_var("APPDATA"),
                }
            }
        }
    }

    fn sample_lock() -> CortexLock {
        CortexLock {
            schema_version: SCHEMA_VERSION,
            pid: std::process::id(),
            port: 31760,
            host: DEFAULT_HOST.to_string(),
            version: "0.9.1".to_string(),
            started_by: StartedBy::Cli,
            started_at: Utc::now(),
            binary_path: PathBuf::from("/usr/local/bin/root"),
        }
    }

    #[test]
    fn read_returns_none_when_lock_absent() {
        let _g = ConfigDirOverride::new();
        let result = read_lock().expect("read_lock errored");
        assert!(
            result.is_none(),
            "expected None for absent lock, got {result:?}"
        );
    }

    #[test]
    fn write_then_read_roundtrips() {
        let _g = ConfigDirOverride::new();
        let lock = sample_lock();
        write_lock(&lock).expect("write_lock");
        let read = read_lock().expect("read_lock").expect("lock present");
        // chrono microseconds precision survives JSON roundtrip; no
        // need to munge timestamps.
        assert_eq!(read, lock);
    }

    #[test]
    fn write_is_atomic_via_rename() {
        // Verifies that the temp file persistence path used by
        // write_lock leaves no stray `.tmp` files on success.
        let _g = ConfigDirOverride::new();
        let lock = sample_lock();
        write_lock(&lock).expect("write_lock");
        let parent = lock_path().unwrap().parent().unwrap().to_path_buf();
        let strays: Vec<_> = std::fs::read_dir(&parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.starts_with(".tmp") || (s.contains("tmp") && s != "cortex.lock.write")
            })
            .collect();
        assert!(
            strays.is_empty(),
            "stray temp files left after write: {strays:?}"
        );
    }

    #[test]
    fn remove_is_idempotent_for_absent_lock() {
        let _g = ConfigDirOverride::new();
        // Calling remove twice on an absent lock must not error.
        remove_lock().expect("remove on absent");
        remove_lock().expect("remove on absent (second call)");
    }

    #[test]
    fn corrupt_lock_reads_as_none() {
        let _g = ConfigDirOverride::new();
        let path = lock_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not even close to json").unwrap();
        let result = read_lock().expect("corrupt lock should read as Ok(None)");
        assert!(
            result.is_none(),
            "corrupt lock should be silently treated as absent"
        );
    }

    #[test]
    fn empty_lock_reads_as_none() {
        let _g = ConfigDirOverride::new();
        let path = lock_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let result = read_lock().expect("empty lock should read as Ok(None)");
        assert!(result.is_none());
    }

    #[test]
    fn future_schema_version_refuses_attach() {
        let _g = ConfigDirOverride::new();
        let mut lock = sample_lock();
        lock.schema_version = SCHEMA_VERSION + 1;
        let path = lock_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&lock).unwrap()).unwrap();
        let err = read_lock().expect_err("future schema must error");
        assert!(
            matches!(err, CortexError::IncompatibleLockSchema { .. }),
            "wrong error variant: {err:?}"
        );
    }

    #[test]
    fn process_alive_for_self_is_true() {
        let me = std::process::id();
        assert!(process_alive(me), "this process must report as alive");
    }

    #[test]
    fn process_alive_for_unlikely_pid_is_false() {
        // A PID at the upper end of the u32 space is overwhelmingly
        // unlikely to be in use; OSes assign low PIDs.
        assert!(!process_alive(u32::MAX - 1));
    }

    #[test]
    fn started_by_str_round_trips() {
        for sb in [
            StartedBy::Cli,
            StartedBy::Desktop,
            StartedBy::Service,
            StartedBy::PythonSdk,
            StartedBy::TsSdk,
        ] {
            let s = serde_json::to_string(&sb).unwrap();
            let back: StartedBy = serde_json::from_str(&s).unwrap();
            assert_eq!(sb, back, "round-trip failed for {sb:?}");
            assert!(!sb.as_str().is_empty());
        }
    }

    #[test]
    fn writer_sentinel_path_is_sibling_of_lock() {
        let _g = ConfigDirOverride::new();
        let lock = lock_path().unwrap();
        let sentinel = writer_sentinel_path().unwrap();
        assert_eq!(lock.parent(), sentinel.parent());
        assert_eq!(sentinel.file_name().unwrap(), "cortex.lock.write");
    }

    #[test]
    fn try_write_lock_yields_when_sentinel_held() {
        // Acquire the sentinel manually, then verify try_write_lock
        // returns WriterLockBusy without blocking.
        use fs2::FileExt;
        let _g = ConfigDirOverride::new();
        let sentinel_path = writer_sentinel_path().unwrap();
        std::fs::create_dir_all(sentinel_path.parent().unwrap()).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&sentinel_path)
            .unwrap();
        #[allow(clippy::incompatible_msrv)]
        held.lock_exclusive().unwrap();

        let err = try_write_lock(&sample_lock()).expect_err("must yield");
        assert!(matches!(err, CortexError::WriterLockBusy));

        #[allow(clippy::incompatible_msrv)]
        let _ = FileExt::unlock(&held);
    }

    #[test]
    fn write_overwrites_corrupt_lock_cleanly() {
        let _g = ConfigDirOverride::new();
        let path = lock_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"corrupt").unwrap();
        let lock = sample_lock();
        write_lock(&lock).expect("write_lock should overwrite corrupt file");
        let read = read_lock().unwrap().unwrap();
        assert_eq!(read, lock);
    }

    #[test]
    fn engine_intent_is_copy() {
        let i = EngineIntent::Command;
        let _ = i;
        // Statically asserts EngineIntent: Copy. If this fails to
        // compile, downstream callers that want to pass the intent
        // by value into multiple branches break.
        let _: EngineIntent = i;
    }

    #[test]
    fn lock_serialises_with_schema_version_first() {
        // Pretty-print order matters for human inspection — readers
        // should see schema_version on line 2 (after the opening
        // brace) so a `head -3 cortex.lock` immediately reveals
        // version compat.
        let lock = sample_lock();
        let json = serde_json::to_string_pretty(&lock).unwrap();
        let mut lines = json.lines();
        assert_eq!(lines.next(), Some("{"));
        let second = lines.next().unwrap();
        assert!(
            second.contains("\"schema_version\""),
            "expected schema_version on second line, got: {second}"
        );
    }

    #[test]
    fn decision_serializable_roundtrip() {
        // Sanity check the Decision enum can serialize for debug logs.
        // (Decision is NOT a wire type — but Debug must work for tracing.)
        let d = Decision::Attach {
            host: "127.0.0.1".to_string(),
            port: 31760,
            version: "0.9.1".to_string(),
        };
        let repr = format!("{:?}", d);
        assert!(repr.contains("Attach"), "got: {repr}");
    }

    #[test]
    fn decision_repair_needed_carries_failing_checks() {
        let d = Decision::RepairNeeded {
            failing_check_ids: vec![
                "binary.cli.installed".to_string(),
                "config.dir.writable".to_string(),
            ],
        };
        let repr = format!("{:?}", d);
        assert!(repr.contains("binary.cli.installed"), "got: {repr}");
    }

    #[test]
    fn probe_result_default_is_not_probed() {
        let p = ProbeResult::NotProbed;
        assert!(matches!(p, ProbeResult::NotProbed));
    }

    #[test]
    fn decision_inputs_can_be_constructed() {
        let _inputs = DecisionInputs {
            intent: EngineIntent::Command,
            lock: None,
            lock_pid_alive: false,
            probe_result: ProbeResult::NotProbed,
            manifest_preferred_binary: None,
            in_process_flag: false,
        };
    }
}
