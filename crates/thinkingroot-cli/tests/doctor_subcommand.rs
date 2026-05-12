//! `root doctor` subcommand end-to-end smoke.

use std::process::Command;

fn run_root(args: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_root");
    Command::new(bin).args(args).output().expect("spawn root")
}

#[test]
fn doctor_default_prints_header() {
    let out = run_root(&["doctor"]);
    assert!(
        out.status.code().is_some(),
        "doctor must exit cleanly, got: {:?}",
        out.status
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("root doctor"),
        "expected header in stdout, got: {stdout}"
    );
}

#[test]
fn doctor_json_emits_valid_json() {
    let out = run_root(&["doctor", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json output invalid: {e}\noutput: {stdout}"));
    assert!(parsed["schema_version"].is_number());
    assert!(parsed["checks"].is_array());
    assert!(parsed["summary"].is_object());
}

#[test]
fn doctor_quiet_emits_no_stdout() {
    let out = run_root(&["doctor", "--quiet"]);
    assert!(
        out.stdout.is_empty(),
        "doctor --quiet must produce no stdout, got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(out.status.code().is_some());
}
