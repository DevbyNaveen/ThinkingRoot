//! Cross-platform process sandbox for ThinkingRoot's `shell_exec` tool.
//!
//! Phase D Wave 1 (2026-05-17) — the foundation under every Phase D
//! system-power capability. `shell_exec` is the universal escape
//! hatch for the agent to invoke any installed binary (git, pip,
//! npm, cargo, brew, …) on the user's machine. Without a real
//! sandbox boundary, one prompt-injected blog post the agent
//! retrieves could run `rm -rf ~`. This crate is that boundary.
//!
//! ## Per-platform policy enforcement
//!
//! - **macOS** — shells out to `/usr/bin/sandbox-exec -p '<inline
//!   policy>' -- <command> <args>`. Policy is generated in-memory
//!   from [`SandboxPolicy`] and passed via the `-p` flag (NOT a
//!   tempfile — Seatbelt SIGKILLs the child if the policy tempfile
//!   gets `unlink`ed mid-execution). Seatbelt is documented as
//!   deprecated by Apple but ships on every macOS through 15.x and
//!   does not require codesigning, which keeps `cargo install
//!   thinkingroot-cli` working.
//!
//! - **Linux** — prefers `bwrap` (bubblewrap) when the host binary
//!   is present AND unprivileged user namespaces work. The Doctor
//!   surface checks both at install time and at each `shell_exec`
//!   call (cached). When `bwrap` is absent or userns is broken, the
//!   sandbox falls back to a **degraded mode**: spawn with
//!   `prctl(PR_SET_NO_NEW_PRIVS)` plus `setrlimit(CPU/AS/NOFILE/
//!   NPROC)` via the `nix` crate, plus path-allowlist enforcement
//!   at the tool-handler layer (defense in depth). The fallback is
//!   communicated to the user via a one-time UI toast and a witness
//!   audit-log entry — never silent.
//!
//!   We deliberately do NOT ship our own `bwrap` binary. `bwrap`
//!   typically needs `setuid` or kernel-level userns capability; a
//!   $10/mo product cannot eat the supply-chain blast radius of
//!   shipping setuid binaries. Users install via their distro's
//!   package manager (`apt`, `dnf`, `pacman`); the Doctor surface
//!   prints the exact one-liner per distro when bwrap is missing.
//!
//! - **Windows** — `shell_exec` is intentionally HARD-BLOCKED on
//!   Windows v1. Job Objects bound CPU/memory/process-tree but do
//!   not enforce path or registry isolation; AppContainer would
//!   require admin + appx packaging which `cargo install` can't
//!   provide. Returning [`SandboxError::UnsupportedPlatform`] with
//!   a WSL2 hint is honest; pretending Job Objects sandbox a shell
//!   would be a security regression. The other 9 Phase D Wave 1
//!   tools work natively on Windows.

use std::path::PathBuf;
use std::time::Duration;

mod error;
pub use error::SandboxError;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// Hard upper bound on `SandboxPolicy::timeout_secs`. A single shell
/// command running longer than 5 minutes is almost certainly a
/// runaway loop or a network hang; we kill it and surface
/// [`SandboxError::Timeout`] so the agent can decide whether to
/// re-issue with a higher (capped) timeout if the user agrees.
pub const MAX_TIMEOUT_SECS: u32 = 300;

/// Default per-call cap on captured stdout/stderr. Beyond this we
/// truncate and set the `truncated_*` flags on [`SandboxOutput`].
/// The cap protects the LLM's context window — a 100 MB compile log
/// fed back into the chat would blow the budget for the rest of the
/// session.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024; // 256 KiB

/// Policy that fully describes what a sandboxed process is allowed
/// to do. Every field is required; there is no "open by default"
/// shortcut. Callers (typically `system_power::shell_exec`) build
/// this from the caller's [`PermissionsGate`] decisions plus the
/// active workspace root.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Paths the sandboxed process may read AND write under.
    ///
    /// On macOS each entry becomes `(allow file-read* file-write*
    /// (subpath ...))`. On Linux (bwrap) each entry becomes
    /// `--bind <path> <path>` (read-write bind mount).
    pub allowed_paths: Vec<PathBuf>,

    /// Paths the sandboxed process may read but NOT write.
    ///
    /// On macOS each entry becomes `(allow file-read* (subpath
    /// ...))`. On Linux (bwrap) each entry becomes `--ro-bind`.
    pub readonly_paths: Vec<PathBuf>,

    /// Network hosts (or `*` for "any") the sandboxed process may
    /// reach.  Empty vec means "no network".
    ///
    /// macOS: each entry becomes `(allow network-outbound (remote
    /// ip "..."))`.  Linux bwrap: enables `--share-net` when
    /// non-empty (bubblewrap's net namespace is binary, not
    /// host-scoped — a future ship may layer a per-host filter).
    pub allowed_hosts: Vec<String>,

    /// Wall-clock timeout in seconds. The child is hard-killed on
    /// timeout. Clamped to [`MAX_TIMEOUT_SECS`] by [`Sandbox::spawn`].
    pub timeout_secs: u32,

    /// Working directory for the sandboxed process. If `None`, the
    /// process inherits the parent's CWD (which is typically the
    /// active workspace root). Must be inside one of
    /// `allowed_paths` or `readonly_paths` or the spawn returns
    /// [`SandboxError::CwdNotAllowed`].
    pub cwd: Option<PathBuf>,

    /// Per-call cap on captured stdout and stderr each. Set to
    /// [`DEFAULT_MAX_OUTPUT_BYTES`] for typical agent calls.
    pub max_output_bytes: usize,
}

impl SandboxPolicy {
    /// Construct a deny-everything policy.  Build from this when the
    /// caller has explicit allowlists to add.
    pub fn deny_default() -> Self {
        Self {
            allowed_paths: Vec::new(),
            readonly_paths: Vec::new(),
            allowed_hosts: Vec::new(),
            timeout_secs: 30,
            cwd: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// The result of a sandboxed process invocation.
#[derive(Debug, Clone)]
pub struct SandboxOutput {
    /// Process exit code. `0` is success per POSIX convention. On
    /// Unix, a signal-terminated process has `exit_code = -1` and
    /// `signal = Some(N)`.
    pub exit_code: i32,
    /// On Unix, set to `Some(N)` when the process was terminated
    /// by signal N (e.g. 9 for SIGKILL from our own timeout). On
    /// Windows always `None`.
    pub signal: Option<i32>,
    /// Captured stdout, truncated to `policy.max_output_bytes`.
    pub stdout: Vec<u8>,
    /// Captured stderr, truncated to `policy.max_output_bytes`.
    pub stderr: Vec<u8>,
    /// Wall-clock duration from spawn to wait-completion.
    pub duration_ms: u64,
    /// True when stdout was truncated.
    pub truncated_stdout: bool,
    /// True when stderr was truncated.
    pub truncated_stderr: bool,
    /// Which backend actually serviced the call.  Surfaced to the
    /// audit log so the user can see when a Linux call ran in
    /// degraded mode.
    pub backend: SandboxBackend,
}

/// Which underlying mechanism performed the sandboxing.  Recorded
/// in audit logs so it's visible after the fact that a Linux call
/// ran without `bwrap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    /// macOS `sandbox-exec` (Seatbelt) with inline policy.
    MacosSandboxExec,
    /// Linux `bubblewrap` (`bwrap`).
    LinuxBubblewrap,
    /// Linux fallback: setrlimit + prctl(PR_SET_NO_NEW_PRIVS).
    /// User has been notified.
    LinuxDegraded,
}

/// The cross-platform sandbox trait. One concrete impl per OS,
/// returned by [`default_sandbox`].
///
/// The trait is async because Linux's `bwrap` fallback path and the
/// timeout watchdog both rely on `tokio::process` + `tokio::time`.
/// Callers may invoke `spawn` from inside the agent loop without
/// blocking other concurrent agent tasks.
#[async_trait::async_trait]
pub trait Sandbox: Send + Sync + 'static {
    async fn spawn(
        &self,
        command: &str,
        args: &[String],
        policy: &SandboxPolicy,
    ) -> Result<SandboxOutput, SandboxError>;
}

// Internal helper: clamp the timeout to [`MAX_TIMEOUT_SECS`].
pub(crate) fn clamp_timeout(secs: u32) -> Duration {
    Duration::from_secs(secs.min(MAX_TIMEOUT_SECS) as u64)
}

/// Construct the default sandbox for the current platform.
///
/// Returns a boxed trait object so callers can store the sandbox in
/// `Arc<dyn Sandbox>` for shared use across the agent loop.
pub fn default_sandbox() -> Box<dyn Sandbox> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosSandbox::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxSandbox::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsSandbox::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        // BSD, illumos, etc.  We don't ship a sandbox for these; the
        // tool layer will hard-reject `shell_exec` the same way it
        // does on Windows, surfacing UnsupportedPlatform.
        Box::new(unsupported::UnsupportedSandbox)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod unsupported {
    use super::*;

    pub struct UnsupportedSandbox;

    #[async_trait::async_trait]
    impl Sandbox for UnsupportedSandbox {
        async fn spawn(
            &self,
            _command: &str,
            _args: &[String],
            _policy: &SandboxPolicy,
        ) -> Result<SandboxOutput, SandboxError> {
            Err(SandboxError::UnsupportedPlatform {
                os: std::env::consts::OS.to_string(),
                hint: "shell_exec is supported only on macOS and Linux. \
                       Other Unix-like platforms can use file_read, file_write, \
                       file_edit, glob, grep, and clipboard tools, which are \
                       cross-platform."
                    .to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_default_has_no_allowed_paths() {
        let p = SandboxPolicy::deny_default();
        assert!(p.allowed_paths.is_empty());
        assert!(p.readonly_paths.is_empty());
        assert!(p.allowed_hosts.is_empty());
        assert!(p.cwd.is_none());
        assert_eq!(p.timeout_secs, 30);
        assert_eq!(p.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn clamp_timeout_respects_max() {
        assert_eq!(clamp_timeout(10), Duration::from_secs(10));
        assert_eq!(clamp_timeout(MAX_TIMEOUT_SECS), Duration::from_secs(MAX_TIMEOUT_SECS as u64));
        assert_eq!(
            clamp_timeout(MAX_TIMEOUT_SECS + 999),
            Duration::from_secs(MAX_TIMEOUT_SECS as u64),
            "clamp must cap at MAX_TIMEOUT_SECS"
        );
    }
}
