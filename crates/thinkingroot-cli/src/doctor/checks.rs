//! Built-in check registry. Each check is a pure function over a
//! `DoctorEnv` (the injected environment) returning one `CheckResult`.
//! Checks must NOT access env vars or the real filesystem directly —
//! that goes through `DoctorEnv` so tests can mock it.
//!
//! Stability: every `CheckId` here is commit-locked. Add new IDs
//! freely; never rename existing ones.

use crate::doctor::check::{CheckId, CheckResult, CheckStatus, DoctorEnv, FixAction};

/// Run all built-in checks against `env`. Order is deterministic
/// (matches the order checks appear in this file) so the terminal
/// output is stable.
pub fn run_all(env: &DoctorEnv) -> Vec<CheckResult> {
    vec![
        binary_cli_installed(env),
        binary_cli_on_path(env),
    ]
}

/// Does at least one of the known install paths contain a `root` binary?
/// Status: `Ok` (some path matches), `Fail` (none).
pub fn binary_cli_installed(env: &DoctorEnv) -> CheckResult {
    let found = env
        .install_dir_candidates
        .iter()
        .find(|p| p.exists())
        .cloned();
    match found {
        Some(p) => CheckResult {
            id: CheckId::from_static("binary.cli.installed"),
            label: "ThinkingRoot CLI binary".to_string(),
            status: CheckStatus::Ok,
            detail: format!("{}", p.display()),
            fix: None,
        },
        None => CheckResult {
            id: CheckId::from_static("binary.cli.installed"),
            label: "ThinkingRoot CLI binary".to_string(),
            status: CheckStatus::Fail,
            detail: format!(
                "no `root` binary found in: {}",
                env.install_dir_candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            fix: Some(FixAction::ShellHint {
                command: "curl -fsSL https://raw.githubusercontent.com/DevbyNaveen/ThinkingRoot/main/install.sh | sh"
                    .into(),
            }),
        },
    }
}

/// Is the directory containing the installed `root` binary on `$PATH`?
/// Skipped if `binary.cli.installed` would fail.
pub fn binary_cli_on_path(env: &DoctorEnv) -> CheckResult {
    let Some(binary) = env.install_dir_candidates.iter().find(|p| p.exists()) else {
        return CheckResult {
            id: CheckId::from_static("binary.cli.on_path"),
            label: "`root` on PATH".into(),
            status: CheckStatus::Skipped,
            detail: "no binary installed".into(),
            fix: None,
        };
    };
    let Some(parent) = binary.parent() else {
        return CheckResult {
            id: CheckId::from_static("binary.cli.on_path"),
            label: "`root` on PATH".into(),
            status: CheckStatus::Fail,
            detail: format!("binary path has no parent: {}", binary.display()),
            fix: None,
        };
    };
    let on_path = env.path_entries.iter().any(|p| p == parent);
    if on_path {
        CheckResult {
            id: CheckId::from_static("binary.cli.on_path"),
            label: "`root` on PATH".into(),
            status: CheckStatus::Ok,
            detail: format!("{} present in $PATH", parent.display()),
            fix: None,
        }
    } else {
        CheckResult {
            id: CheckId::from_static("binary.cli.on_path"),
            label: "`root` on PATH".into(),
            status: CheckStatus::Fail,
            detail: format!(
                "binary exists at {} but $PATH does not include {}",
                binary.display(),
                parent.display()
            ),
            fix: Some(FixAction::ShellHint {
                command: format!("export PATH=\"{}:$PATH\"", parent.display()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env_with(install_paths: Vec<&str>, path_entries: Vec<&str>) -> DoctorEnv {
        DoctorEnv {
            config_dir: PathBuf::from("/tmp/cfg"),
            install_dir_candidates: install_paths.into_iter().map(PathBuf::from).collect(),
            path_entries: path_entries.into_iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn binary_installed_ok_when_one_path_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("root");
        std::fs::write(&bin, "x").unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![bin.clone(), PathBuf::from("/nonexistent/root")],
            path_entries: vec![],
        };
        let r = binary_cli_installed(&env);
        assert_eq!(r.status, CheckStatus::Ok);
        assert_eq!(r.id, CheckId::from_static("binary.cli.installed"));
    }

    #[test]
    fn binary_installed_fail_when_no_paths_exist() {
        let env = env_with(vec!["/nonexistent/a", "/nonexistent/b"], vec![]);
        let r = binary_cli_installed(&env);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(matches!(r.fix, Some(FixAction::ShellHint { .. })));
    }

    #[test]
    fn on_path_ok_when_parent_in_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("root");
        std::fs::write(&bin, "x").unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![bin.clone()],
            path_entries: vec![tmp.path().to_path_buf()],
        };
        let r = binary_cli_on_path(&env);
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[test]
    fn on_path_skipped_when_no_binary_installed() {
        let env = env_with(vec!["/nonexistent/x"], vec!["/usr/bin"]);
        let r = binary_cli_on_path(&env);
        assert_eq!(r.status, CheckStatus::Skipped);
    }

    #[test]
    fn on_path_fail_when_parent_not_in_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("root");
        std::fs::write(&bin, "x").unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![bin.clone()],
            path_entries: vec![PathBuf::from("/somewhere/else")],
        };
        let r = binary_cli_on_path(&env);
        assert_eq!(r.status, CheckStatus::Fail);
    }
}
