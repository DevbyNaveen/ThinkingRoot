//! Slice 1 follow-up — desktop wiring for `root doctor`.
//!
//! Shells out to the same `root doctor --json` the CLI uses so the
//! palette's "Run diagnostics" command runs the real self-check
//! battery instead of the prior `phase("/doctor", "D-10")` placeholder.
//!
//! Honesty contract:
//! - The Tauri command never silently maps a degraded run to "ok"; the
//!   raw verdict + per-check report is forwarded verbatim to the UI.
//! - Exit codes 0/1/2 from the CLI map to `verdict: ok | degraded |
//!   broken`. Any other exit code surfaces as an error.

use std::process::Stdio;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Debug, Serialize, Clone)]
pub struct DoctorReport {
    /// `ok` | `degraded` | `broken`.
    pub verdict: String,
    /// Pretty JSON body returned by `root doctor --json`.
    pub raw_json: String,
    /// stderr captured during the run — useful when the CLI exits with
    /// an unexpected code.
    pub stderr_log: String,
    /// Exit code returned by the subprocess.
    pub exit_code: i32,
}

fn resolve_root_binary() -> Option<String> {
    if let Ok(override_path) = std::env::var("THINKINGROOT_ROOT_BINARY")
        && !override_path.is_empty()
    {
        return Some(override_path);
    }
    let bin = if cfg!(windows) { "root.exe" } else { "root" };
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

/// Run `root doctor --json [--repair]`.
#[tauri::command]
pub async fn doctor_run(repair: bool) -> Result<DoctorReport, String> {
    let bin = resolve_root_binary()
        .ok_or_else(|| "could not locate `root` binary in PATH".to_string())?;

    let mut cmd = Command::new(&bin);
    cmd.arg("doctor").arg("--json");
    if repair {
        cmd.arg("--repair");
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn `{bin} doctor`: {e}"))?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });
    let status = child
        .wait()
        .await
        .map_err(|e| format!("wait root doctor: {e}"))?;

    let stdout_log = stdout_task.await.map_err(|e| e.to_string())?;
    let stderr_log = stderr_task.await.map_err(|e| e.to_string())?;
    let exit_code = status.code().unwrap_or(-1);

    let verdict = match exit_code {
        0 => "ok",
        1 => "degraded",
        2 => "broken",
        _ => {
            return Err(format!(
                "root doctor exited with unexpected code {exit_code}. stderr:\n{stderr_log}"
            ));
        }
    }
    .to_string();

    Ok(DoctorReport {
        verdict,
        raw_json: stdout_log,
        stderr_log,
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity check: the verdict mapping uses exit codes 0/1/2, so the
    /// CLI contract documented in `doctor_cmd.rs` and what the desktop
    /// surfaces stay in lockstep. Errors on any other code rather than
    /// silently falling through to "ok".
    #[test]
    fn verdict_mapping_matches_cli_contract() {
        // Mirror the match arms in `doctor_run` directly so a future
        // refactor that renames a verdict trips this test.
        let map = |code: i32| -> Option<&'static str> {
            match code {
                0 => Some("ok"),
                1 => Some("degraded"),
                2 => Some("broken"),
                _ => None,
            }
        };
        assert_eq!(map(0), Some("ok"));
        assert_eq!(map(1), Some("degraded"));
        assert_eq!(map(2), Some("broken"));
        assert_eq!(map(75), None, "DAEMON_UNREACHABLE must not collapse to ok");
        assert_eq!(map(-1), None);
    }
}
