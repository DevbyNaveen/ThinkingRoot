//! `root doctor` — single source of truth for setup + health
//! diagnosis.  Three skins (terminal pretty-print, --json,
//! --fix [--interactive]) on one check matrix.
//!
//! Spec: `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md` §2.
//!
//! Coexists with `crate::doctor_cmd` (the legacy implementation)
//! until Task 12 deletes it.

pub mod check;
pub mod checks;

pub use check::{CheckId, CheckResult, CheckStatus, DoctorEnv, FixAction};

use serde::{Deserialize, Serialize};

/// Reader-bumped schema version of `root doctor --json` output.
/// Matches cortex + install-manifest discipline: a reader on
/// version N refuses to parse `schema_version > N`. Bumping breaks
/// downstream consumers (Desktop blocking panel, scripts).
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// The full result of running `root doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub checks: Vec<CheckResult>,
    pub summary: Summary,
}

/// Counts derived from `checks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
    pub skipped: usize,
}

impl Summary {
    pub fn from_checks(checks: &[CheckResult]) -> Self {
        let mut s = Self { ok: 0, warn: 0, fail: 0, skipped: 0 };
        for c in checks {
            match c.status {
                CheckStatus::Ok => s.ok += 1,
                CheckStatus::Warn => s.warn += 1,
                CheckStatus::Fail => s.fail += 1,
                CheckStatus::Skipped => s.skipped += 1,
            }
        }
        s
    }

    /// Process exit code: 0 = all ok, 1 = any fail, 2 = any warn (no fail).
    /// Skipped does not contribute.
    pub fn exit_code(&self) -> i32 {
        if self.fail > 0 {
            1
        } else if self.warn > 0 {
            2
        } else {
            0
        }
    }
}

/// How `run_doctor` should behave. Maps 1:1 to CLI flag combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorMode {
    Default,
    Quiet,
    Json,
    Fix,
    FixInteractive,
}

/// Entry point. Runs all built-in checks against the real filesystem
/// (via DoctorEnv) and returns the report. Caller renders.
pub async fn run_doctor(_mode: DoctorMode) -> anyhow::Result<DoctorReport> {
    let env = check::DoctorEnv::from_real_filesystem()?;
    let checks = checks::run_all(&env).await;
    let summary = Summary::from_checks(&checks);
    Ok(DoctorReport {
        schema_version: REPORT_SCHEMA_VERSION,
        checks,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(id: &'static str, status: CheckStatus) -> CheckResult {
        CheckResult {
            id: CheckId::from_static(id),
            label: id.to_string(),
            status,
            detail: String::new(),
            fix: None,
        }
    }

    #[test]
    fn summary_counts_each_status() {
        let checks = vec![
            make_result("a", CheckStatus::Ok),
            make_result("b", CheckStatus::Ok),
            make_result("c", CheckStatus::Warn),
            make_result("d", CheckStatus::Fail),
            make_result("e", CheckStatus::Skipped),
        ];
        let s = Summary::from_checks(&checks);
        assert_eq!(s.ok, 2);
        assert_eq!(s.warn, 1);
        assert_eq!(s.fail, 1);
        assert_eq!(s.skipped, 1);
    }

    #[test]
    fn exit_code_zero_when_all_ok() {
        let s = Summary { ok: 5, warn: 0, fail: 0, skipped: 0 };
        assert_eq!(s.exit_code(), 0);
    }

    #[test]
    fn exit_code_two_when_only_warn() {
        let s = Summary { ok: 3, warn: 2, fail: 0, skipped: 1 };
        assert_eq!(s.exit_code(), 2);
    }

    #[test]
    fn exit_code_one_when_any_fail() {
        let s = Summary { ok: 1, warn: 2, fail: 3, skipped: 0 };
        assert_eq!(s.exit_code(), 1);
    }

    #[test]
    fn doctor_report_serializes_with_schema_version() {
        let checks = vec![make_result("x", CheckStatus::Ok)];
        let report = DoctorReport {
            schema_version: REPORT_SCHEMA_VERSION,
            checks: checks.clone(),
            summary: Summary::from_checks(&checks),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"schema_version\":1"), "got: {json}");
        assert!(json.contains("\"summary\""), "got: {json}");
    }
}
