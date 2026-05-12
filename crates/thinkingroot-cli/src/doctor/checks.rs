//! Built-in check registry. Each check is a pure function over a
//! `DoctorEnv` (the injected environment) returning one `CheckResult`.
//! Checks must NOT access env vars or the real filesystem directly —
//! that goes through `DoctorEnv` so tests can mock it.
//!
//! Stability: every `CheckId` here is commit-locked. Add new IDs
//! freely; never rename existing ones.

use crate::doctor::check::{CheckId, CheckResult, CheckStatus, DoctorEnv, FixAction};

/// Sync portion of the check matrix. Doesn't probe network IO.
/// Useful for `--quiet` and dry-run scenarios.
pub fn run_all_sync(env: &DoctorEnv) -> Vec<CheckResult> {
    vec![
        binary_cli_installed(env),
        binary_cli_on_path(env),
        config_dir_writable(env),
        credentials_any_provider(env),
        daemon_lockfile_parseable(env),
        workspace_registry_parseable(env),
        workspace_active_exists(env),
    ]
}

/// Full check matrix including async probes. Used by `run_doctor`.
/// Order is deterministic (matches the order checks appear in this
/// file) so the terminal output is stable.
pub async fn run_all(env: &DoctorEnv) -> Vec<CheckResult> {
    let mut checks = run_all_sync(env);
    checks.push(daemon_reachable(env).await);
    checks
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

/// Can we create and write to a file under `env.config_dir`? Uses
/// a tempfile sentinel to probe without polluting state.
pub fn config_dir_writable(env: &DoctorEnv) -> CheckResult {
    let dir = &env.config_dir;
    if !dir.exists() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return CheckResult {
                id: CheckId::from_static("config.dir.writable"),
                label: "Config directory writable".into(),
                status: CheckStatus::Fail,
                detail: format!("cannot create {}: {e}", dir.display()),
                fix: Some(FixAction::ShellHint {
                    command: format!("mkdir -p {}", dir.display()),
                }),
            };
        }
    }
    let sentinel = dir.join(".tr-doctor-probe");
    match std::fs::write(&sentinel, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&sentinel);
            CheckResult {
                id: CheckId::from_static("config.dir.writable"),
                label: "Config directory writable".into(),
                status: CheckStatus::Ok,
                detail: format!("{}", dir.display()),
                fix: None,
            }
        }
        Err(e) => CheckResult {
            id: CheckId::from_static("config.dir.writable"),
            label: "Config directory writable".into(),
            status: CheckStatus::Fail,
            detail: format!("cannot write under {}: {e}", dir.display()),
            fix: Some(FixAction::ShellHint {
                command: format!("chmod u+w {}", dir.display()),
            }),
        },
    }
}

/// Does any LLM provider have a key configured (env var OR file)?
pub fn credentials_any_provider(env: &DoctorEnv) -> CheckResult {
    if env.has_any_provider_key() {
        CheckResult {
            id: CheckId::from_static("credentials.any_provider"),
            label: "At least one LLM provider key".into(),
            status: CheckStatus::Ok,
            detail: "credential present".into(),
            fix: None,
        }
    } else {
        CheckResult {
            id: CheckId::from_static("credentials.any_provider"),
            label: "At least one LLM provider key".into(),
            status: CheckStatus::Fail,
            detail: "no provider keys configured".into(),
            fix: Some(FixAction::RunCommand {
                command: "root provider add".into(),
            }),
        }
    }
}

/// Is `cortex.lock` either absent or well-formed JSON?
/// Status: `Ok` (absent OR valid) / `Warn` (corrupt; Slice F self-heals).
pub fn daemon_lockfile_parseable(env: &DoctorEnv) -> CheckResult {
    let lock_path = env.config_dir.join("cortex.lock");
    if !lock_path.exists() {
        return CheckResult {
            id: CheckId::from_static("daemon.lockfile.parseable"),
            label: "Daemon lockfile state".into(),
            status: CheckStatus::Ok,
            detail: "no daemon running (lockfile absent)".into(),
            fix: None,
        };
    }
    let bytes = match std::fs::read(&lock_path) {
        Ok(b) => b,
        Err(e) => {
            return CheckResult {
                id: CheckId::from_static("daemon.lockfile.parseable"),
                label: "Daemon lockfile state".into(),
                status: CheckStatus::Warn,
                detail: format!("lockfile read failed: {e}"),
                fix: Some(FixAction::ShellHint {
                    command: format!("rm {}", lock_path.display()),
                }),
            };
        }
    };
    if bytes.is_empty() {
        return CheckResult {
            id: CheckId::from_static("daemon.lockfile.parseable"),
            label: "Daemon lockfile state".into(),
            status: CheckStatus::Warn,
            detail: "lockfile empty".into(),
            fix: Some(FixAction::ShellHint {
                command: format!("rm {}", lock_path.display()),
            }),
        };
    }
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(_) => CheckResult {
            id: CheckId::from_static("daemon.lockfile.parseable"),
            label: "Daemon lockfile state".into(),
            status: CheckStatus::Ok,
            detail: "lockfile present and parseable".into(),
            fix: None,
        },
        Err(_) => CheckResult {
            id: CheckId::from_static("daemon.lockfile.parseable"),
            label: "Daemon lockfile state".into(),
            status: CheckStatus::Warn,
            detail: "lockfile is not valid JSON".into(),
            fix: Some(FixAction::ShellHint {
                command: format!("rm {}", lock_path.display()),
            }),
        },
    }
}

/// Is `workspaces.toml` either absent or well-formed TOML?
/// Parses the registry file at `env.config_dir.join("workspaces.toml")`
/// directly (not via `WorkspaceRegistry::load`) so the DoctorEnv injection
/// is honored — `load()` resolves through process-global `dirs::config_dir()`.
pub fn workspace_registry_parseable(env: &DoctorEnv) -> CheckResult {
    let path = env.config_dir.join("workspaces.toml");
    if !path.exists() {
        return CheckResult {
            id: CheckId::from_static("workspace.registry.parseable"),
            label: "Workspace registry".into(),
            status: CheckStatus::Ok,
            detail: "no workspaces registered".into(),
            fix: None,
        };
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return CheckResult {
                id: CheckId::from_static("workspace.registry.parseable"),
                label: "Workspace registry".into(),
                status: CheckStatus::Warn,
                detail: format!("unreadable: {e}"),
                fix: None,
            };
        }
    };
    match toml::from_str::<thinkingroot_core::WorkspaceRegistry>(&content) {
        Ok(reg) => CheckResult {
            id: CheckId::from_static("workspace.registry.parseable"),
            label: "Workspace registry".into(),
            status: CheckStatus::Ok,
            detail: format!("{} workspace(s) registered", reg.workspaces.len()),
            fix: None,
        },
        Err(e) => CheckResult {
            id: CheckId::from_static("workspace.registry.parseable"),
            label: "Workspace registry".into(),
            status: CheckStatus::Warn,
            detail: format!("parse error: {e}"),
            fix: Some(FixAction::ShellHint {
                command: format!("mv {} {}.broken-$(date +%s)", path.display(), path.display()),
            }),
        },
    }
}

/// If a workspace is marked active, does its directory still exist?
/// Skipped if no registry or no active workspace.
pub fn workspace_active_exists(env: &DoctorEnv) -> CheckResult {
    let path = env.config_dir.join("workspaces.toml");
    if !path.exists() {
        return CheckResult {
            id: CheckId::from_static("workspace.active.exists"),
            label: "Active workspace directory exists".into(),
            status: CheckStatus::Skipped,
            detail: "no registry".into(),
            fix: None,
        };
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            return CheckResult {
                id: CheckId::from_static("workspace.active.exists"),
                label: "Active workspace directory exists".into(),
                status: CheckStatus::Skipped,
                detail: "registry unreadable".into(),
                fix: None,
            };
        }
    };
    let reg = match toml::from_str::<thinkingroot_core::WorkspaceRegistry>(&content) {
        Ok(r) => r,
        Err(_) => {
            return CheckResult {
                id: CheckId::from_static("workspace.active.exists"),
                label: "Active workspace directory exists".into(),
                status: CheckStatus::Skipped,
                detail: "registry unparseable".into(),
                fix: None,
            };
        }
    };
    let Some(active) = reg.active.as_ref() else {
        return CheckResult {
            id: CheckId::from_static("workspace.active.exists"),
            label: "Active workspace directory exists".into(),
            status: CheckStatus::Skipped,
            detail: "no active workspace set".into(),
            fix: None,
        };
    };
    let Some(entry) = reg.workspaces.iter().find(|w| &w.name == active) else {
        return CheckResult {
            id: CheckId::from_static("workspace.active.exists"),
            label: "Active workspace directory exists".into(),
            status: CheckStatus::Fail,
            detail: format!("active workspace '{}' not in registry", active),
            fix: Some(FixAction::RunCommand {
                command: format!("root workspace remove {}", active),
            }),
        };
    };
    if entry.path.exists() {
        CheckResult {
            id: CheckId::from_static("workspace.active.exists"),
            label: "Active workspace directory exists".into(),
            status: CheckStatus::Ok,
            detail: format!("{} -> {}", entry.name, entry.path.display()),
            fix: None,
        }
    } else {
        CheckResult {
            id: CheckId::from_static("workspace.active.exists"),
            label: "Active workspace directory exists".into(),
            status: CheckStatus::Fail,
            detail: format!(
                "active workspace '{}' points to {} which no longer exists",
                entry.name,
                entry.path.display()
            ),
            fix: Some(FixAction::RunCommand {
                command: format!("root workspace remove {}", entry.name),
            }),
        }
    }
}

/// Is the daemon described in `cortex.lock` actually serving HTTP?
/// Status:
/// - `Skipped` when lockfile absent (no daemon expected)
/// - `Ok` when /livez returns 2xx within 1s
/// - `Warn` when lockfile exists but probe fails (stale lock)
pub async fn daemon_reachable(env: &DoctorEnv) -> CheckResult {
    let lock_path = env.config_dir.join("cortex.lock");
    if !lock_path.exists() {
        return CheckResult {
            id: CheckId::from_static("daemon.reachable"),
            label: "Daemon /livez".into(),
            status: CheckStatus::Skipped,
            detail: "no daemon running".into(),
            fix: None,
        };
    }
    let bytes = match std::fs::read(&lock_path) {
        Ok(b) => b,
        Err(_) => {
            return CheckResult {
                id: CheckId::from_static("daemon.reachable"),
                label: "Daemon /livez".into(),
                status: CheckStatus::Skipped,
                detail: "lockfile unreadable".into(),
                fix: None,
            };
        }
    };
    #[derive(serde::Deserialize)]
    struct LockShape {
        host: String,
        port: u16,
    }
    let lock: LockShape = match serde_json::from_slice(&bytes) {
        Ok(l) => l,
        Err(_) => {
            return CheckResult {
                id: CheckId::from_static("daemon.reachable"),
                label: "Daemon /livez".into(),
                status: CheckStatus::Skipped,
                detail: "lockfile not parseable".into(),
                fix: None,
            };
        }
    };
    let url = format!("http://{}:{}/livez", lock.host, lock.port);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                id: CheckId::from_static("daemon.reachable"),
                label: "Daemon /livez".into(),
                status: CheckStatus::Warn,
                detail: format!("could not build HTTP client: {e}"),
                fix: None,
            };
        }
    };
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => CheckResult {
            id: CheckId::from_static("daemon.reachable"),
            label: "Daemon /livez".into(),
            status: CheckStatus::Ok,
            detail: format!("{} ok", url),
            fix: None,
        },
        Ok(resp) => CheckResult {
            id: CheckId::from_static("daemon.reachable"),
            label: "Daemon /livez".into(),
            status: CheckStatus::Warn,
            detail: format!("{} returned {}", url, resp.status()),
            fix: Some(FixAction::ShellHint {
                command: format!("rm {}", lock_path.display()),
            }),
        },
        Err(e) => CheckResult {
            id: CheckId::from_static("daemon.reachable"),
            label: "Daemon /livez".into(),
            status: CheckStatus::Warn,
            detail: format!("probe failed: {e}"),
            fix: Some(FixAction::ShellHint {
                command: format!("rm {}", lock_path.display()),
            }),
        },
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

    #[test]
    fn config_dir_writable_ok_when_writable() {
        let tmp = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = config_dir_writable(&env);
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[cfg(unix)]
    #[test]
    fn config_dir_writable_fail_when_readonly() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let readonly = tmp.path().join("ro");
        std::fs::create_dir(&readonly).unwrap();
        let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(&readonly, perms).unwrap();

        let env = DoctorEnv {
            config_dir: readonly.clone(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = config_dir_writable(&env);
        // Restore perms before tempdir's drop tries to clean up.
        let mut p = std::fs::metadata(&readonly).unwrap().permissions();
        p.set_mode(0o700);
        std::fs::set_permissions(&readonly, p).unwrap();

        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn credentials_any_provider_ok_when_creds_file_has_key() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("credentials.toml"),
            b"openai_api_key = \"sk-real-key\"\n",
        )
        .unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = credentials_any_provider(&env);
        assert_eq!(r.status, CheckStatus::Ok);
        assert!(!r.detail.contains("sk-real-key"), "must not leak key into detail");
    }

    #[test]
    fn credentials_any_provider_fail_when_no_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        // Defensive: ensure no provider env var is set in the test process.
        for k in crate::doctor::check::CREDENTIAL_VARS {
            // SAFETY: tests in this binary serialize via the per-crate
            // ENV_GUARD in tokio's test harness; the install_manifest
            // tests show the pattern. This check ONLY reads env vars
            // we know about; the test pollution risk is low because
            // CREDENTIAL_VARS aren't typically set in CI.
            unsafe { std::env::remove_var(k); }
        }
        let r = credentials_any_provider(&env);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(matches!(
            r.fix,
            Some(FixAction::RunCommand { ref command }) if command.contains("provider add")
        ));
    }

    #[test]
    fn daemon_lockfile_parseable_ok_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = daemon_lockfile_parseable(&env);
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[test]
    fn daemon_lockfile_parseable_warn_when_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("cortex.lock"), b"not json").unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = daemon_lockfile_parseable(&env);
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[tokio::test]
    async fn daemon_reachable_skipped_when_no_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = daemon_reachable(&env).await;
        assert_eq!(r.status, CheckStatus::Skipped);
    }

    #[test]
    fn workspace_registry_parseable_ok_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = workspace_registry_parseable(&env);
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[test]
    fn workspace_registry_parseable_warn_when_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("workspaces.toml"), b"!!! not toml").unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = workspace_registry_parseable(&env);
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn workspace_active_exists_skipped_when_no_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = workspace_active_exists(&env);
        assert_eq!(r.status, CheckStatus::Skipped);
    }
}
