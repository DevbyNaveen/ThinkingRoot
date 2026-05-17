//! Windows backend for [`crate::Sandbox`].
//!
//! Phase D Wave 1 (2026-05-17) — `shell_exec` is intentionally
//! HARD-BLOCKED on Windows. The reasoning:
//!
//! Windows Job Objects (via `windows-rs`) can bound CPU, memory,
//! and process tree but do NOT enforce path or registry isolation.
//! A sandboxed shell using only Job Objects could still write to
//! `HKEY_CURRENT_USER`, read browser cookies, exfiltrate any path
//! the user's process can reach.  AppContainer is the real
//! Windows sandbox primitive but requires either an MSIX package
//! manifest or signed appx — neither of which `cargo install
//! thinkingroot-cli` can provide today.
//!
//! Returning [`crate::SandboxError::UnsupportedPlatform`] is the
//! honest path. The error string directs the user to WSL2 (where
//! the Linux sandbox works natively). The other 9 Phase D Wave 1
//! tools — file_read, file_write, file_edit, glob, grep,
//! clipboard_read, clipboard_write, open_in_default, trash — all
//! work natively on Windows because the underlying crates
//! (`arboard`, `opener`, `trash`, `ignore`) handle Windows
//! cleanly.

use crate::{Sandbox, SandboxError, SandboxOutput, SandboxPolicy};

pub struct WindowsSandbox;

impl WindowsSandbox {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Sandbox for WindowsSandbox {
    async fn spawn(
        &self,
        _command: &str,
        _args: &[String],
        _policy: &SandboxPolicy,
    ) -> Result<SandboxOutput, SandboxError> {
        Err(SandboxError::UnsupportedPlatform {
            os: "windows".to_string(),
            hint: "shell_exec is supported only on macOS and Linux at v1. \
                   To run shell commands from ThinkingRoot on Windows, install \
                   WSL2 (Windows Subsystem for Linux) and run thinkingroot inside \
                   it: https://learn.microsoft.com/en-us/windows/wsl/install. \
                   The other 9 system-power tools (file_read, file_write, \
                   file_edit, glob, grep, clipboard_read, clipboard_write, \
                   open_in_default, trash) work natively on Windows without \
                   WSL2."
                .to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn windows_sandbox_returns_unsupported_with_wsl2_hint() {
        let s = WindowsSandbox::new();
        let p = SandboxPolicy::deny_default();
        let err = s.spawn("echo", &["hi".into()], &p).await.unwrap_err();
        match err {
            SandboxError::UnsupportedPlatform { os, hint } => {
                assert_eq!(os, "windows");
                assert!(
                    hint.contains("WSL2"),
                    "hint must mention WSL2 so the user knows the workaround: {hint}"
                );
                assert!(
                    hint.contains("file_read")
                        && hint.contains("clipboard_read")
                        && hint.contains("open_in_default"),
                    "hint must enumerate the other 9 tools that DO work on Windows: {hint}"
                );
            }
            other => panic!("expected UnsupportedPlatform, got {other:?}"),
        }
    }
}
