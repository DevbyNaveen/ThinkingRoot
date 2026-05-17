//! macOS backend for [`crate::Sandbox`] via Apple's `sandbox-exec`
//! (Seatbelt).
//!
//! Phase D Wave 1 (2026-05-17). The implementation shells out to
//! `/usr/bin/sandbox-exec -p '<inline-policy>' -- <command> <args>`.
//! The inline policy is generated from [`SandboxPolicy`] at spawn
//! time and passed via the `-p` flag — NEVER as a tempfile.
//! Seatbelt SIGKILLs the child if its policy tempfile gets
//! `unlink`ed mid-execution, so we pass the policy directly on the
//! command line.
//!
//! Apple has marked `sandbox-exec` as deprecated since macOS 10.15
//! but the binary still ships and works through macOS 15.x as of
//! 2026-05. The alternative (App Sandbox + entitlements) would
//! require signed bundles, breaking `cargo install
//! thinkingroot-cli` distribution. Until App Sandbox can be
//! adopted without breaking that flow, `sandbox-exec` is the only
//! viable path.

use std::process::Stdio;
use std::time::Instant;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{Sandbox, SandboxBackend, SandboxError, SandboxOutput, SandboxPolicy, clamp_timeout};

pub struct MacosSandbox;

impl MacosSandbox {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Sandbox for MacosSandbox {
    async fn spawn(
        &self,
        command: &str,
        args: &[String],
        policy: &SandboxPolicy,
    ) -> Result<SandboxOutput, SandboxError> {
        validate_cwd_in_allowlist(policy)?;

        let policy_text = generate_seatbelt_policy(policy);
        let timeout = clamp_timeout(policy.timeout_secs);
        let started = Instant::now();

        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p").arg(&policy_text).arg("--");
        cmd.arg(command).args(args);
        if let Some(cwd) = &policy.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            // sandbox-exec missing → SeatbeltError (clearer for
            // doctor diagnostics); spawn-side OS errors of the
            // child command → SpawnFailed
            if e.kind() == std::io::ErrorKind::NotFound {
                SandboxError::SeatbeltError {
                    reason: format!("/usr/bin/sandbox-exec not found: {e}"),
                }
            } else {
                SandboxError::SpawnFailed {
                    command: command.to_string(),
                    reason: format!("sandbox-exec spawn: {e}"),
                }
            }
        })?;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let cap = policy.max_output_bytes;
        let stdout_task = tokio::spawn(async move { read_capped(stdout_pipe, cap).await });
        let stderr_task = tokio::spawn(async move { read_capped(stderr_pipe, cap).await });

        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                return Err(SandboxError::SpawnFailed {
                    command: command.to_string(),
                    reason: format!("waitpid failed: {e}"),
                });
            }
            Err(_) => {
                // Timeout: kill the child. kill_on_drop also fires
                // when we drop the Child, but we explicitly kill
                // here so the awaits below resolve quickly with
                // whatever output was already collected.
                let _ = child.kill().await;
                return Err(SandboxError::Timeout {
                    command: command.to_string(),
                    secs: policy.timeout_secs,
                });
            }
        };

        // Best-effort: gather captured output. JoinError or read
        // error → empty buffer; we never block the timeout path on
        // pipe drains.
        let (stdout, truncated_stdout) = stdout_task.await.unwrap_or((Vec::new(), false));
        let (stderr, truncated_stderr) = stderr_task.await.unwrap_or((Vec::new(), false));

        let exit_code = status.code().unwrap_or(-1);
        let signal = signal_from_status(&status);

        Ok(SandboxOutput {
            exit_code,
            signal,
            stdout,
            stderr,
            duration_ms: started.elapsed().as_millis() as u64,
            truncated_stdout,
            truncated_stderr,
            backend: SandboxBackend::MacosSandboxExec,
        })
    }
}

/// Build the Seatbelt `.sb` policy text for the given
/// [`SandboxPolicy`].
///
/// **v1 enforcement model — WRITE-restricted, not READ-restricted.**
///
/// macOS Seatbelt's `(deny default)` mode requires every operation
/// (mach lookups, sysctl reads, dyld syscalls, etc.) to be
/// explicitly allowed, and the surface area of "what a normal CLI
/// needs" is large and undocumented. Iterating to a tight
/// allowlist requires test cycles we can't fit in v1.
///
/// Instead, v1 takes the "(allow default) with explicit write
/// denies" approach: the sandboxed process can read most of the
/// filesystem (effectively the user's UID can already read those
/// paths via the calling process), but it can ONLY write to paths
/// explicitly listed in `policy.allowed_paths`. Writes elsewhere
/// abort the syscall. Network is denied unless `policy.allowed_hosts`
/// is non-empty.
///
/// This protects against the highest-impact attacks (a malicious
/// command writing to `~/.ssh/authorized_keys`, modifying
/// `~/.zshrc`, exfiltrating to disk) while remaining tight enough
/// to land in v1. Read-restriction is the next tightening pass —
/// it requires building a complete allow-list of the syscalls
/// every typical CLI tool needs, which is undocumented work.
fn generate_seatbelt_policy(policy: &SandboxPolicy) -> String {
    let mut buf = String::with_capacity(512);
    buf.push_str("(version 1)\n");
    buf.push_str("(allow default)\n");
    // Block writes by default; then explicitly allow caller-declared
    // writable paths.
    buf.push_str("(deny file-write*)\n");
    // Scratch space every CLI needs — without this, even `cargo
    // --version` writes to /private/tmp and would fail.
    buf.push_str("(allow file-write* (subpath \"/private/tmp\"))\n");
    buf.push_str("(allow file-write* (subpath \"/private/var/folders\"))\n");
    buf.push_str("(allow file-write* (subpath \"/dev\"))\n");
    // Caller-declared writable paths.
    for p in &policy.allowed_paths {
        buf.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sb_quote(&p.to_string_lossy())
        ));
    }
    // readonly_paths require no policy action — reads are already
    // allowed by `(allow default)`. They're carried in the policy
    // for parity with the Linux bwrap impl which DOES enforce read
    // bounds.
    // Network policy. Empty allowed_hosts = no network.
    if policy.allowed_hosts.is_empty() {
        buf.push_str("(deny network*)\n");
    }
    buf
}

/// Quote a path for inclusion in a Seatbelt scheme string. Doubles
/// any literal `"` and wraps the whole value in double quotes.
fn sb_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Validate that policy.cwd, if set, is inside the allowlist.
/// macOS Seatbelt enforces this transitively (anything the process
/// touches must be allowed), but we surface the violation as a
/// typed error before spawning so the caller gets a clear message.
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

/// Read up to `cap` bytes from `pipe`; return `(buf, truncated)`.
/// Excess bytes are drained silently so the writer side doesn't
/// block on a full pipe buffer.
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

#[cfg(unix)]
fn signal_from_status(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_from_status(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn allow_tmp_policy() -> SandboxPolicy {
        SandboxPolicy {
            allowed_paths: vec![PathBuf::from("/private/tmp")],
            readonly_paths: vec![PathBuf::from("/usr")],
            allowed_hosts: Vec::new(),
            timeout_secs: 5,
            cwd: None,
            max_output_bytes: 64 * 1024,
        }
    }

    #[tokio::test]
    async fn echo_succeeds_under_sandbox() {
        let s = MacosSandbox::new();
        let p = allow_tmp_policy();
        let out = s.spawn("/bin/echo", &["hello".into()], &p).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.backend, SandboxBackend::MacosSandboxExec);
        assert!(out.stdout.starts_with(b"hello"));
        assert!(!out.truncated_stdout);
    }

    #[tokio::test]
    async fn write_outside_allowlist_is_denied() {
        // v1 policy is write-restricted: writes outside the
        // allowed_paths list MUST be refused, even when the path
        // is otherwise readable. Use a path that's writable on
        // unsandboxed Unix but should be refused under our
        // sandbox (the user's home).
        let s = MacosSandbox::new();
        let p = allow_tmp_policy();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
        let target = format!("{home}/.thinkingroot-sandbox-write-test-DELETE-ME");
        // sh -c "echo x > <target>" exits 0 if the redirect succeeded.
        // Under our sandbox the write should fail (sh returns
        // non-zero because the redirect could not open the file).
        let out = s
            .spawn(
                "/bin/sh",
                &["-c".into(), format!("echo x > {target}")],
                &p,
            )
            .await
            .unwrap();
        // Best-effort cleanup in case the write somehow succeeded.
        let _ = std::fs::remove_file(&target);
        assert_ne!(
            out.exit_code, 0,
            "expected sandbox to refuse write outside allowlist; got exit_code=0 stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn timeout_kills_long_running_child() {
        let s = MacosSandbox::new();
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
        let s = MacosSandbox::new();
        let mut p = allow_tmp_policy();
        p.cwd = Some(PathBuf::from("/var/log"));
        let err = s.spawn("/bin/echo", &["hi".into()], &p).await.unwrap_err();
        assert!(matches!(err, SandboxError::CwdNotAllowed { .. }));
    }

    #[test]
    fn policy_generation_includes_caller_paths_and_quotes_them() {
        let mut p = SandboxPolicy::deny_default();
        p.allowed_paths
            .push(PathBuf::from("/Users/me/project with spaces"));
        p.readonly_paths.push(PathBuf::from("/etc/hosts"));
        let text = generate_seatbelt_policy(&p);
        // v1 write-restricted policy shape.
        assert!(text.contains("(allow default)"));
        assert!(text.contains("(deny file-write*)"));
        assert!(
            text.contains("\"/Users/me/project with spaces\""),
            "spaces in path must be quoted: {text}"
        );
        // readonly_paths are not surfaced in the policy text under the
        // v1 model — reads are allowed by `(allow default)`. They're
        // carried in the SandboxPolicy struct for parity with the
        // Linux backend which does enforce read bounds.
        // No network = explicit `(deny network*)` line.
        assert!(
            text.contains("(deny network*)"),
            "default policy must deny network when allowed_hosts is empty: {text}"
        );
    }

    #[test]
    fn policy_with_network_hosts_allows_outbound() {
        let mut p = SandboxPolicy::deny_default();
        p.allowed_hosts.push("api.anthropic.com".into());
        let text = generate_seatbelt_policy(&p);
        // v1: when allowed_hosts is non-empty, we omit the
        // `(deny network*)` line so the `(allow default)` baseline
        // permits outbound. Per-host filtering will land in a
        // follow-up tightening pass.
        assert!(
            !text.contains("(deny network*)"),
            "non-empty allowed_hosts must NOT emit a deny network rule: {text}"
        );
    }

    #[test]
    fn sb_quote_escapes_quotes_and_backslashes() {
        assert_eq!(sb_quote("plain"), "\"plain\"");
        assert_eq!(sb_quote(r#"has"quote"#), r#""has\"quote""#);
        assert_eq!(sb_quote(r"has\back"), r#""has\\back""#);
    }
}
