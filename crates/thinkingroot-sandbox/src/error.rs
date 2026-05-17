//! Typed error variants returned by [`crate::Sandbox::spawn`].
//!
//! Every variant carries enough context that the caller (typically
//! the `shell_exec` MCP tool handler) can surface an actionable
//! error to the agent + the user. We never wrap into generic
//! `String`s — the tool layer needs the variant to decide whether
//! to retry, prompt the user, or fail fast.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    /// The sandbox attempted to spawn the command but the OS
    /// refused (binary not on `PATH`, permission denied, ENOENT,
    /// etc.). `reason` carries the OS error string verbatim.
    #[error("sandbox failed to spawn `{command}`: {reason}")]
    SpawnFailed { command: String, reason: String },

    /// The command ran but exceeded its wall-clock timeout. The
    /// child has been signal-killed; any partial stdout/stderr is
    /// discarded.
    #[error("sandbox killed `{command}` after {secs}s (timeout)")]
    Timeout { command: String, secs: u32 },

    /// The requested working directory is not inside any path
    /// declared in `allowed_paths` or `readonly_paths`. A sandboxed
    /// process cannot have its CWD outside its filesystem
    /// allowlist — that would let the child open paths transitively
    /// through the CWD.
    #[error("sandbox cwd `{cwd}` is not inside the policy allowlist")]
    CwdNotAllowed { cwd: String },

    /// macOS: `/usr/bin/sandbox-exec` not found, or Seatbelt rejected
    /// the inline policy. The latter is a programming error
    /// (malformed policy from our generator) and should fail loudly.
    #[error("macOS Seatbelt rejected policy or sandbox-exec missing: {reason}")]
    SeatbeltError { reason: String },

    /// Linux: `bwrap` not on PATH AND degraded-mode fallback also
    /// failed (the `nix` rlimit/prctl setup errored). In practice
    /// this is rare — degraded mode degrades to "no sandbox, just
    /// rlimits" which itself should rarely fail. When it does, the
    /// caller should NOT silently fall through to unsandboxed
    /// spawn; surface the error.
    #[error("Linux sandbox failed: bwrap unavailable AND degraded fallback errored: {reason}")]
    LinuxBothBackendsFailed { reason: String },

    /// `shell_exec` is intentionally not supported on this platform.
    /// Today: Windows. The error carries a hint pointing at WSL2.
    /// The other 9 Phase D Wave 1 tools work natively on Windows
    /// (clipboard, file ops, glob, grep, open_in_default, trash).
    #[error("shell_exec is not supported on {os}: {hint}")]
    UnsupportedPlatform { os: String, hint: String },

    /// The caller's policy declared a host pattern that the
    /// sandbox can't enforce (e.g. macOS Seatbelt doesn't support
    /// per-host filtering; we either allow all network or none).
    /// Today: Linux bubblewrap is binary on/off; if the caller
    /// supplies `allowed_hosts` we treat that as "share net" and
    /// emit a warning via `tracing`.
    #[error("sandbox cannot enforce per-host network allowlist on this backend")]
    PerHostNetworkUnsupported,
}
