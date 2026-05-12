//! `--fix` runner — walks fail rows and applies their FixActions.
//!
//! Three FixAction kinds:
//! - ShellHint: print only, never auto-execute.
//! - RunCommand: re-invoke this same `root` binary as a child.
//! - FillIn: prompt for credentials (Slice E lands the full wizard).

use crate::doctor::{CheckResult, CheckStatus, FixAction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixOutcome {
    Printed,
    Executed,
    Skipped,
    NotImplemented,
}

/// Walk every fail check and apply its fix. Returns one entry per
/// `(check, outcome)` for checks with `status == Fail` AND a fix
/// action.
pub fn apply_all(checks: &[CheckResult], interactive: bool) -> Vec<(CheckResult, FixOutcome)> {
    checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .filter_map(|c| {
            c.fix.as_ref().map(|fix| {
                let outcome = apply_one(fix, interactive, &c.label);
                (c.clone(), outcome)
            })
        })
        .collect()
}

fn apply_one(fix: &FixAction, interactive: bool, label: &str) -> FixOutcome {
    match fix {
        FixAction::ShellHint { command } => {
            eprintln!("\n  fix for \"{label}\":\n    {command}\n");
            FixOutcome::Printed
        }
        FixAction::RunCommand { command } => {
            if interactive {
                eprint!("apply fix for \"{label}\"?  run: {command}  [y/N] ");
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).is_err() {
                    return FixOutcome::Skipped;
                }
                if !line.trim().eq_ignore_ascii_case("y") {
                    return FixOutcome::Skipped;
                }
            }
            let parts: Vec<&str> = command.split_whitespace().collect();
            if parts.is_empty() {
                return FixOutcome::Skipped;
            }
            // Skip leading "root " in commands like "root provider add".
            let arg_slice = if parts[0] == "root" {
                &parts[1..]
            } else {
                &parts[..]
            };
            let me = std::env::current_exe()
                .unwrap_or_else(|_| std::path::PathBuf::from("root"));
            let status = std::process::Command::new(&me).args(arg_slice).status();
            match status {
                Ok(s) if s.success() => FixOutcome::Executed,
                _ => FixOutcome::Skipped,
            }
        }
        FixAction::FillIn { .. } => {
            eprintln!(
                "\n  fix for \"{label}\":\n    run: root provider add\n  (interactive credential wizard ships in Slice E)\n"
            );
            FixOutcome::NotImplemented
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::check::CheckId;

    #[test]
    fn apply_all_filters_to_fail_status_with_fix() {
        let checks = vec![
            CheckResult {
                id: CheckId::from_static("x.ok"),
                label: "ok one".into(),
                status: CheckStatus::Ok,
                detail: "".into(),
                fix: Some(FixAction::ShellHint { command: "noop".into() }),
            },
            CheckResult {
                id: CheckId::from_static("x.fail.with.fix"),
                label: "fail with fix".into(),
                status: CheckStatus::Fail,
                detail: "".into(),
                fix: Some(FixAction::ShellHint { command: "real".into() }),
            },
            CheckResult {
                id: CheckId::from_static("x.fail.no.fix"),
                label: "fail no fix".into(),
                status: CheckStatus::Fail,
                detail: "".into(),
                fix: None,
            },
        ];
        let out = apply_all(&checks, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.id, CheckId::from_static("x.fail.with.fix"));
        assert_eq!(out[0].1, FixOutcome::Printed);
    }
}
