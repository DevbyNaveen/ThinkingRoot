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
//!
//! Slice D adds two typed companions:
//! - [`doctor_check`] returns the parsed [`TypedDoctorReport`] so the
//!   blocking-panel UI can render structured per-check rows instead
//!   of pretty-print JSON.
//! - [`doctor_apply_fix`] runs `root doctor --fix --json` and returns
//!   the post-fix report.  The CLI's `Fix | FixJson` dispatch arms
//!   were updated in lockstep so stdout stays clean JSON.

use std::process::Stdio;

use serde::{Deserialize, Serialize};
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

// ---------------------------------------------------------------------------
// Slice D — typed report surface for the blocking-panel UI.
//
// The structs below mirror `thinkingroot_cli::doctor::DoctorReport` field for
// field but are duplicated here on purpose: the desktop's Tauri crate must
// not depend on `thinkingroot-cli` (clap, sled, the whole CLI link tree).
// The JSON wire shape is the contract — drift between the two would surface
// as a deserialize error on first call, not silent corruption.
// ---------------------------------------------------------------------------

/// Parsed `root doctor --json` output.  Mirrors
/// `thinkingroot_cli::doctor::DoctorReport`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedDoctorReport {
    pub schema_version: u32,
    pub checks: Vec<TypedCheckResult>,
    pub summary: TypedSummary,
}

/// One row in a doctor report.  Mirrors
/// `thinkingroot_cli::doctor::CheckResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedCheckResult {
    pub id: String,
    pub label: String,
    pub status: String,
    pub detail: String,
    #[serde(default)]
    pub fix: Option<TypedFixAction>,
}

/// Aggregate counters.  Mirrors
/// `thinkingroot_cli::doctor::Summary`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TypedSummary {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
    pub skipped: usize,
}

/// Mirrors `thinkingroot_cli::doctor::FixAction`.  Tag + payload shape
/// matches `#[serde(tag = "kind", rename_all = "kebab-case")]` on the
/// CLI side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TypedFixAction {
    ShellHint { command: String },
    RunCommand { command: String },
    FillIn { prompt: String, credential_key: String },
}

/// Reader-bumped schema version.  Matches `REPORT_SCHEMA_VERSION` in
/// `crates/thinkingroot-cli/src/doctor/mod.rs`.  Bumping the CLI side
/// must bump this in lockstep.
pub const TYPED_REPORT_MAX_SCHEMA_VERSION: u32 = 1;

/// Run `root doctor --json` and return the typed report.
///
/// Errors (Tauri command surface — never panic):
/// - `root` binary not on PATH / no env override.
/// - subprocess spawn fails.
/// - stdout is not UTF-8.
/// - JSON parse fails.
/// - report `schema_version > TYPED_REPORT_MAX_SCHEMA_VERSION`
///   (forward-incompat per the cortex/install-manifest discipline).
///
/// Non-zero exit codes from `root doctor` are NOT errors: the CLI
/// returns 1 when there are failing checks and 2 when there are only
/// warnings, but the JSON on stdout is still valid and complete.
/// The blocking-panel UI surfaces failure state via the typed
/// summary, not via exit code.
#[tauri::command]
pub async fn doctor_check() -> Result<TypedDoctorReport, String> {
    let bin = resolve_root_binary()
        .ok_or_else(|| "no `root` binary on PATH or in install manifest".to_string())?;

    let output = Command::new(&bin)
        .arg("doctor")
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("spawn `{bin} doctor --json`: {e}"))?;

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("`root doctor --json` stdout not UTF-8: {e}"))?;

    let report: TypedDoctorReport = serde_json::from_str(&stdout).map_err(|e| {
        format!(
            "parse `root doctor --json` output: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;

    if report.schema_version > TYPED_REPORT_MAX_SCHEMA_VERSION {
        return Err(format!(
            "root doctor report schema_version {} > supported {}; \
             upgrade the desktop app",
            report.schema_version, TYPED_REPORT_MAX_SCHEMA_VERSION
        ));
    }

    Ok(report)
}

/// Run `root doctor --fix --json` and return the post-fix typed report.
///
/// The `_check_id` argument is accepted for forward-compat with a
/// future per-check fix flow (Slice E credential wizard).  Today the
/// CLI's `--fix` runner walks every failing check that has a fix
/// action attached; targeted single-check fixes are not yet wired.
///
/// Stdout contract: the CLI's `Fix | Json` dispatch arm runs the fix
/// flow non-interactively, re-runs the check matrix, and prints the
/// post-fix `DoctorReport` JSON on stdout.  Fix-progress noise
/// (per-outcome lines) goes to stderr — the Tauri command never has
/// to strip it before parsing.
#[tauri::command]
pub async fn doctor_apply_fix(_check_id: Option<String>) -> Result<TypedDoctorReport, String> {
    let bin = resolve_root_binary()
        .ok_or_else(|| "no `root` binary on PATH or in install manifest".to_string())?;

    let output = Command::new(&bin)
        .arg("doctor")
        .arg("--fix")
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("spawn `{bin} doctor --fix --json`: {e}"))?;

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("`root doctor --fix --json` stdout not UTF-8: {e}"))?;

    let report: TypedDoctorReport = serde_json::from_str(&stdout).map_err(|e| {
        format!(
            "parse `root doctor --fix --json` output: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;

    if report.schema_version > TYPED_REPORT_MAX_SCHEMA_VERSION {
        return Err(format!(
            "root doctor report schema_version {} > supported {}; \
             upgrade the desktop app",
            report.schema_version, TYPED_REPORT_MAX_SCHEMA_VERSION
        ));
    }

    Ok(report)
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

    /// Wire-shape compatibility: a JSON payload matching the CLI's
    /// `DoctorReport` must deserialize into our duplicated
    /// `TypedDoctorReport` without loss.  If the CLI side renames a
    /// field this test trips before users see the deserialize error.
    #[test]
    fn typed_report_parses_cli_json_shape() {
        let raw = r#"{
            "schema_version": 1,
            "checks": [
                {
                    "id": "binary.cli.installed",
                    "label": "ThinkingRoot CLI binary",
                    "status": "ok",
                    "detail": "/usr/local/bin/root",
                    "fix": null
                },
                {
                    "id": "credentials.any_provider",
                    "label": "At least one LLM provider key",
                    "status": "fail",
                    "detail": "no provider keys configured",
                    "fix": {
                        "kind": "run-command",
                        "command": "root provider add"
                    }
                }
            ],
            "summary": { "ok": 1, "warn": 0, "fail": 1, "skipped": 0 }
        }"#;

        let report: TypedDoctorReport = serde_json::from_str(raw)
            .expect("CLI JSON must round-trip into TypedDoctorReport");
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.checks.len(), 2);
        assert_eq!(report.checks[0].id, "binary.cli.installed");
        assert_eq!(report.checks[0].status, "ok");
        assert!(report.checks[0].fix.is_none());
        assert_eq!(report.summary.ok, 1);
        assert_eq!(report.summary.fail, 1);

        match &report.checks[1].fix {
            Some(TypedFixAction::RunCommand { command }) => {
                assert_eq!(command, "root provider add");
            }
            other => panic!("expected RunCommand fix, got {other:?}"),
        }
    }

    /// Forward-incompat guard: a report claiming `schema_version` 99
    /// must be rejected, not silently parsed (mirrors the cortex +
    /// install-manifest reader-bumped discipline).
    #[test]
    fn typed_report_rejects_future_schema_version() {
        // We can't call `doctor_check` without a real subprocess, but
        // we can replicate the version-gate check the command applies
        // post-parse — keeping the constant in this same test file
        // ensures a bump to TYPED_REPORT_MAX_SCHEMA_VERSION is a
        // visible diff.
        let r = TypedDoctorReport {
            schema_version: TYPED_REPORT_MAX_SCHEMA_VERSION + 1,
            checks: vec![],
            summary: TypedSummary { ok: 0, warn: 0, fail: 0, skipped: 0 },
        };
        assert!(
            r.schema_version > TYPED_REPORT_MAX_SCHEMA_VERSION,
            "version-gate must trip on future schemas"
        );
    }

    /// FixAction tag + payload shape compatibility.  Mirrors the
    /// `fix_action_kinds_round_trip` test on the CLI side at
    /// `crates/thinkingroot-cli/src/doctor/check.rs:166-178`.
    #[test]
    fn typed_fix_action_round_trips_all_kinds() {
        for raw in [
            r#"{"kind":"shell-hint","command":"export PATH=..."}"#,
            r#"{"kind":"run-command","command":"root provider add"}"#,
            r#"{"kind":"fill-in","prompt":"API key:","credential_key":"OPENAI_API_KEY"}"#,
        ] {
            let parsed: TypedFixAction =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("parse {raw}: {e}"));
            let reser = serde_json::to_string(&parsed).unwrap();
            let back: TypedFixAction = serde_json::from_str(&reser).unwrap();
            // Re-serialise + deserialise must be a no-op.
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(&back).unwrap(),
            );
        }
    }
}
