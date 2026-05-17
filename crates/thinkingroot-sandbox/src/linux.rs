//! Linux backend for [`crate::Sandbox`].
//!
//! Phase D Wave 1 (2026-05-17). Prefers `bwrap` (bubblewrap) when
//! available + unprivileged user namespaces work. Falls back to a
//! degraded mode using `prctl(PR_SET_NO_NEW_PRIVS)` plus
//! `setrlimit(CPU/AS/NOFILE/NPROC)` when bwrap is missing or
//! userns is disabled.
//!
//! Why fallback instead of hard-fail: Linux distributions vary
//! widely in whether `bwrap` is installed by default and whether
//! unprivileged userns is enabled. RHEL 8/9 with the default
//! `kernel.unprivileged_userns_clone=0` disables userns for
//! non-root; Debian 12 enables it; Arch ships bubblewrap in `base-
//! devel`. Hard-rejecting `shell_exec` on the missing-bwrap case
//! would block too many users at install time. Degraded mode
//! emits a witness audit-log entry and a one-time UI toast so the
//! user is informed that sandboxing is reduced — never silent.
//!
//! Why we don't ship our own `bwrap`: bubblewrap needs `setuid` or
//! kernel userns capability. Shipping a setuid binary makes
//! ThinkingRoot a privilege-escalation surface and gets us flagged
//! by every distro security scanner. The Doctor surface prints
//! the exact install one-liner per distro instead.

use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Instant;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{Sandbox, SandboxBackend, SandboxError, SandboxOutput, SandboxPolicy, clamp_timeout};

pub struct LinuxSandbox;

impl LinuxSandbox {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Sandbox for LinuxSandbox {
    async fn spawn(
        &self,
        command: &str,
        args: &[String],
        policy: &SandboxPolicy,
    ) -> Result<SandboxOutput, SandboxError> {
        validate_cwd_in_allowlist(policy)?;

        if bwrap_available() {
            spawn_with_bwrap(command, args, policy).await
        } else {
            spawn_degraded(command, args, policy).await
        }
    }
}

/// Cached probe: is `bwrap` on PATH AND does unprivileged userns
/// work? Probed once per process. The Doctor surface re-runs the
/// same probe at install time so the user sees the verdict before
/// hitting `shell_exec`.
fn bwrap_available() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        // 1. bwrap on PATH
        let bwrap = std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let bwrap_ok = matches!(bwrap, Ok(s) if s.success());
        if !bwrap_ok {
            tracing::warn!(
                "Linux sandbox: `bwrap` not found on PATH. \
                 Install via `apt install bubblewrap` (Debian/Ubuntu), \
                 `dnf install bubblewrap` (Fedora/RHEL), or \
                 `pacman -S bubblewrap` (Arch). \
                 Falling back to degraded sandbox (setrlimit + prctl)."
            );
            return false;
        }
        // 2. unprivileged userns works
        let userns = std::process::Command::new("unshare")
            .args(["--user", "--pid", "echo", "ok"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let userns_ok = matches!(userns, Ok(s) if s.success());
        if !userns_ok {
            tracing::warn!(
                "Linux sandbox: unprivileged user namespaces are disabled \
                 (sysctl kernel.unprivileged_userns_clone=0?). \
                 Falling back to degraded sandbox (setrlimit + prctl)."
            );
        }
        userns_ok
    })
}

async fn spawn_with_bwrap(
    command: &str,
    args: &[String],
    policy: &SandboxPolicy,
) -> Result<SandboxOutput, SandboxError> {
    let timeout = clamp_timeout(policy.timeout_secs);
    let started = Instant::now();

    let mut bwrap = Command::new("bwrap");
    bwrap.arg("--unshare-all");
    if !policy.allowed_hosts.is_empty() {
        // bwrap network is binary on/off; per-host filtering is not
        // expressible. We log the unenforced bit so callers know.
        tracing::warn!(
            "Linux sandbox (bwrap): allowed_hosts list is unenforced; \
             enabling --share-net for any-host access. \
             Use a network proxy or a future fine-grained backend for \
             per-host filtering."
        );
        bwrap.arg("--share-net");
    }
    bwrap.arg("--die-with-parent").arg("--new-session");
    // Read-only root and a tmpfs over /tmp for scratch state.
    bwrap.args(["--ro-bind", "/", "/", "--tmpfs", "/tmp"]);
    bwrap.args(["--proc", "/proc", "--dev", "/dev"]);
    // Caller-declared paths.
    for p in &policy.readonly_paths {
        let s = p.to_string_lossy().into_owned();
        bwrap.args(["--ro-bind-try", &s, &s]);
    }
    for p in &policy.allowed_paths {
        let s = p.to_string_lossy().into_owned();
        bwrap.args(["--bind-try", &s, &s]);
    }
    bwrap.arg("--");
    bwrap.arg(command).args(args);
    if let Some(cwd) = &policy.cwd {
        bwrap.current_dir(cwd);
    }
    bwrap
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = bwrap.spawn().map_err(|e| SandboxError::SpawnFailed {
        command: command.to_string(),
        reason: format!("bwrap spawn: {e}"),
    })?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let cap = policy.max_output_bytes;
    let stdout_task = tokio::spawn(async move { read_capped(stdout_pipe, cap).await });
    let stderr_task = tokio::spawn(async move { read_capped(stderr_pipe, cap).await });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(SandboxError::SpawnFailed {
                command: command.to_string(),
                reason: format!("waitpid failed: {e}"),
            });
        }
        Err(_) => {
            let _ = child.kill().await;
            return Err(SandboxError::Timeout {
                command: command.to_string(),
                secs: policy.timeout_secs,
            });
        }
    };

    let (stdout, truncated_stdout) = stdout_task.await.unwrap_or((Vec::new(), false));
    let (stderr, truncated_stderr) = stderr_task.await.unwrap_or((Vec::new(), false));

    Ok(SandboxOutput {
        exit_code: status.code().unwrap_or(-1),
        signal: signal_from_status(&status),
        stdout,
        stderr,
        duration_ms: started.elapsed().as_millis() as u64,
        truncated_stdout,
        truncated_stderr,
        backend: SandboxBackend::LinuxBubblewrap,
    })
}

/// Degraded fallback. Spawns the command directly, applies
/// `prctl(PR_SET_NO_NEW_PRIVS)` so the child cannot gain
/// capabilities via setuid binaries, and sets resource limits via
/// `setrlimit`. Path enforcement happens at the tool-handler layer
/// (defense in depth) — this fallback alone does NOT enforce the
/// caller's path allowlist.
async fn spawn_degraded(
    command: &str,
    args: &[String],
    policy: &SandboxPolicy,
) -> Result<SandboxOutput, SandboxError> {
    use nix::sys::prctl;
    use nix::sys::resource::{Resource, setrlimit};

    let timeout = clamp_timeout(policy.timeout_secs);
    let started = Instant::now();

    let mut cmd = Command::new(command);
    cmd.args(args);
    if let Some(cwd) = &policy.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Apply rlimits + PR_SET_NO_NEW_PRIVS in the pre_exec hook so
    // they affect the child only, not our agent process.
    let cpu_secs = policy.timeout_secs.saturating_mul(2) as u64;
    let max_mem_bytes: u64 = 1024 * 1024 * 1024; // 1 GiB
    let max_fds: u64 = 1024;
    let max_procs: u64 = 64;
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            // Order matters: NO_NEW_PRIVS first so subsequent
            // setrlimit failures still leave us safer than nothing.
            if let Err(e) = prctl::set_no_new_privs() {
                return Err(std::io::Error::other(format!(
                    "PR_SET_NO_NEW_PRIVS failed: {e}"
                )));
            }
            let _ = setrlimit(Resource::RLIMIT_CPU, cpu_secs, cpu_secs);
            let _ = setrlimit(Resource::RLIMIT_AS, max_mem_bytes, max_mem_bytes);
            let _ = setrlimit(Resource::RLIMIT_NOFILE, max_fds, max_fds);
            let _ = setrlimit(Resource::RLIMIT_NPROC, max_procs, max_procs);
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(|e| SandboxError::SpawnFailed {
        command: command.to_string(),
        reason: format!("degraded spawn: {e}"),
    })?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let cap = policy.max_output_bytes;
    let stdout_task = tokio::spawn(async move { read_capped(stdout_pipe, cap).await });
    let stderr_task = tokio::spawn(async move { read_capped(stderr_pipe, cap).await });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(SandboxError::SpawnFailed {
                command: command.to_string(),
                reason: format!("waitpid failed: {e}"),
            });
        }
        Err(_) => {
            let _ = child.kill().await;
            return Err(SandboxError::Timeout {
                command: command.to_string(),
                secs: policy.timeout_secs,
            });
        }
    };

    let (stdout, truncated_stdout) = stdout_task.await.unwrap_or((Vec::new(), false));
    let (stderr, truncated_stderr) = stderr_task.await.unwrap_or((Vec::new(), false));

    Ok(SandboxOutput {
        exit_code: status.code().unwrap_or(-1),
        signal: signal_from_status(&status),
        stdout,
        stderr,
        duration_ms: started.elapsed().as_millis() as u64,
        truncated_stdout,
        truncated_stderr,
        backend: SandboxBackend::LinuxDegraded,
    })
}

fn validate_cwd_in_allowlist(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    let Some(cwd) = &policy.cwd else {
        return Ok(());
    };
    let ok = policy
        .allowed_paths
        .iter()
        .chain(policy.readonly_paths.iter())
        .any(|allowed| cwd.starts_with(allowed));
    if ok {
        Ok(())
    } else {
        Err(SandboxError::CwdNotAllowed {
            cwd: cwd.display().to_string(),
        })
    }
}

async fn read_capped<R: AsyncReadExt + Unpin>(pipe: Option<R>, cap: usize) -> (Vec<u8>, bool) {
    let Some(mut pipe) = pipe else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::with_capacity(cap.min(8 * 1024));
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

fn signal_from_status(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn allow_tmp_policy() -> SandboxPolicy {
        SandboxPolicy {
            allowed_paths: vec![PathBuf::from("/tmp")],
            readonly_paths: vec![PathBuf::from("/usr")],
            allowed_hosts: Vec::new(),
            timeout_secs: 5,
            cwd: None,
            max_output_bytes: 64 * 1024,
        }
    }

    #[tokio::test]
    async fn echo_succeeds_via_some_backend() {
        // Test asserts the call succeeds; which backend was used
        // depends on the test environment. Both bwrap and degraded
        // must return exit_code = 0 for /bin/echo.
        let s = LinuxSandbox::new();
        let p = allow_tmp_policy();
        let out = s.spawn("/bin/echo", &["hello".into()], &p).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.starts_with(b"hello"));
        assert!(matches!(
            out.backend,
            SandboxBackend::LinuxBubblewrap | SandboxBackend::LinuxDegraded
        ));
    }

    #[tokio::test]
    async fn timeout_kills_long_running_child() {
        let s = LinuxSandbox::new();
        let mut p = allow_tmp_policy();
        p.timeout_secs = 1;
        let err = s
            .spawn("/bin/sleep", &["10".into()], &p)
            .await
            .unwrap_err();
        match err {
            SandboxError::Timeout { command, secs } => {
                assert!(command.contains("sleep"));
                assert_eq!(secs, 1);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cwd_outside_allowlist_is_typed_error() {
        let s = LinuxSandbox::new();
        let mut p = allow_tmp_policy();
        p.cwd = Some(PathBuf::from("/var/log"));
        let err = s.spawn("/bin/echo", &["hi".into()], &p).await.unwrap_err();
        assert!(matches!(err, SandboxError::CwdNotAllowed { .. }));
    }
}
