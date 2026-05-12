//! Snapshot tests for the doctor renderer. Catches drift in
//! human-readable output and serde shape changes.
//!
//! Uses substring assertions rather than full-output snapshots
//! because the terminal renderer interpolates tempdir paths
//! that vary per run; full-string snapshots would be noisy
//! without a path-redaction filter. Substring checks pin the
//! load-bearing labels + status words + the schema-version
//! JSON shape — enough to catch real regressions without
//! drowning future format tweaks in churn.

use std::path::PathBuf;

use thinkingroot_cli::doctor::{
    check::DoctorEnv, checks, format, DoctorReport, Summary, REPORT_SCHEMA_VERSION,
};

fn render_report(env: DoctorEnv) -> DoctorReport {
    let checks = checks::run_all_sync(&env);
    let summary = Summary::from_checks(&checks);
    DoctorReport {
        schema_version: REPORT_SCHEMA_VERSION,
        checks,
        summary,
    }
}

/// Defensive: provider env vars contaminate `credentials_any_provider`.
/// Strip every variable in the canonical list before each test so the
/// outcome is deterministic regardless of the developer's shell.
fn scrub_provider_env() {
    for k in thinkingroot_cli::doctor::check::CREDENTIAL_VARS {
        // SAFETY: tests in this crate already remove env vars unsafely
        // (see `credentials_any_provider_fail_when_no_keys` in
        // `src/doctor/checks.rs`). The CREDENTIAL_VARS list is the
        // closed set of variables doctor checks recognise.
        unsafe { std::env::remove_var(k); }
    }
}

#[test]
fn empty_env_renders_with_clear_failures() {
    scrub_provider_env();
    let tmp = tempfile::tempdir().unwrap();
    let env = DoctorEnv {
        config_dir: tmp.path().to_path_buf(),
        install_dir_candidates: vec![PathBuf::from("/nonexistent/root")],
        path_entries: vec![],
    };
    let report = render_report(env);
    let terminal = format::to_terminal(&report);

    // Substring assertions so the test is stable across small
    // formatting tweaks. Full-string snapshots are noisy.
    assert!(
        terminal.contains("[fail] ThinkingRoot CLI binary"),
        "got: {terminal}"
    );
    assert!(
        terminal.contains("[skipped] `root` on PATH"),
        "got: {terminal}"
    );
    assert!(
        terminal.contains("[ok] Config directory writable"),
        "got: {terminal}"
    );
    assert!(
        terminal.contains("[fail] At least one LLM provider key"),
        "got: {terminal}"
    );
    assert!(
        terminal.contains("[ok] Daemon lockfile state"),
        "got: {terminal}"
    );
    assert!(
        terminal.contains("[ok] Workspace registry"),
        "got: {terminal}"
    );
    assert!(
        terminal.contains("[skipped] Active workspace directory exists"),
        "got: {terminal}"
    );
    assert!(
        terminal.contains("[skipped] Install manifest in sync with disk"),
        "got: {terminal}"
    );
}

#[test]
fn empty_env_json_shape_is_well_formed() {
    scrub_provider_env();
    let tmp = tempfile::tempdir().unwrap();
    let env = DoctorEnv {
        config_dir: tmp.path().to_path_buf(),
        install_dir_candidates: vec![PathBuf::from("/nonexistent/root")],
        path_entries: vec![],
    };
    let report = render_report(env);
    let json = format::to_json(&report);

    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["schema_version"], 1);
    // Sync checks: binary.cli.{installed,on_path,checksum},
    // config.dir.writable, credentials.any_provider,
    // daemon.{lockfile.parseable,restart.exhausted},
    // workspace.{registry.parseable,active.exists},
    // install.manifest.consistent → 10 total. Updated by Slice F
    // (added binary.cli.checksum + daemon.restart.exhausted).
    let checks = parsed["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 10, "sync check count drifted; got: {checks:?}");
    // Verify the Slice F additions are present so future drift
    // points at the new IDs rather than just count.
    let ids: Vec<&str> = checks.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"binary.cli.checksum"), "got: {ids:?}");
    assert!(ids.contains(&"daemon.restart.exhausted"), "got: {ids:?}");
    // At least one fail (binary or credentials)
    assert!(parsed["summary"]["fail"].as_u64().unwrap() >= 1);
}
