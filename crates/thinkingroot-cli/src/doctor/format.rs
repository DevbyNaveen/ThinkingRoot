//! Doctor report renderers — terminal pretty-print and JSON.

use crate::doctor::{CheckStatus, DoctorReport, FixAction};

/// Pretty-print a doctor report for terminal output. Glyphs + plain
/// English status words side by side so output stays grep-friendly.
pub fn to_terminal(report: &DoctorReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "root doctor — schema v{} — {} ok / {} warn / {} fail / {} skipped\n\n",
        report.schema_version,
        report.summary.ok,
        report.summary.warn,
        report.summary.fail,
        report.summary.skipped,
    ));
    for c in &report.checks {
        let (glyph, word) = match c.status {
            CheckStatus::Ok => ("✓", "ok"),
            CheckStatus::Warn => ("⚠", "warn"),
            CheckStatus::Fail => ("✗", "fail"),
            CheckStatus::Skipped => ("·", "skipped"),
        };
        out.push_str(&format!(
            "  {} [{}] {}  ({})\n     {}\n",
            glyph, word, c.label, c.id.0, c.detail
        ));
        if let Some(fix) = &c.fix {
            out.push_str(&format!("     fix: {}\n", format_fix(fix)));
        }
    }
    out
}

fn format_fix(fix: &FixAction) -> String {
    match fix {
        FixAction::ShellHint { command } => format!("run: {command}"),
        FixAction::RunCommand { command } => format!("auto: {command}"),
        FixAction::FillIn { prompt, .. } => format!("prompt: {prompt}"),
    }
}

/// Pretty-printed JSON suitable for the Desktop blocking panel
/// (Slice D) and any external scripting consumers.
pub fn to_json(report: &DoctorReport) -> String {
    serde_json::to_string_pretty(report).expect("DoctorReport always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{check::CheckId, CheckResult, Summary};

    fn fixture_report() -> DoctorReport {
        let checks = vec![
            CheckResult {
                id: CheckId::from_static("binary.cli.installed"),
                label: "ThinkingRoot CLI binary".into(),
                status: CheckStatus::Ok,
                detail: "/usr/local/bin/root".into(),
                fix: None,
            },
            CheckResult {
                id: CheckId::from_static("credentials.any_provider"),
                label: "At least one LLM provider key".into(),
                status: CheckStatus::Fail,
                detail: "no provider keys configured".into(),
                fix: None,
            },
        ];
        DoctorReport {
            schema_version: 1,
            checks: checks.clone(),
            summary: Summary::from_checks(&checks),
        }
    }

    #[test]
    fn to_terminal_includes_status_glyphs_and_labels() {
        let report = fixture_report();
        let out = to_terminal(&report);
        assert!(out.contains("ThinkingRoot CLI binary"), "got: {out}");
        assert!(out.contains("At least one LLM provider key"), "got: {out}");
        assert!(out.contains("ok"));
        assert!(out.contains("fail"));
        assert!(out.contains("✓"));
        assert!(out.contains("✗"));
    }

    #[test]
    fn to_json_produces_well_formed_output() {
        let report = fixture_report();
        let json = to_json(&report);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["checks"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["summary"]["ok"], 1);
        assert_eq!(parsed["summary"]["fail"], 1);
    }
}
