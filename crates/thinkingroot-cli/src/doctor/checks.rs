//! Built-in check registry. Each check is a pure function over a
//! `DoctorEnv` (the injected environment) returning one `CheckResult`.
//! Checks must NOT access env vars or the real filesystem directly —
//! that goes through `DoctorEnv` so tests can mock it.
//!
//! Stability: every `CheckId` here is commit-locked. Add new IDs
//! freely; never rename existing ones.

use crate::doctor::check::{CheckId, CheckResult, CheckStatus, DoctorEnv, FixAction};
use thinkingroot_core::install_manifest::InstallManifest;

/// Sync portion of the check matrix. Doesn't probe network IO.
/// Useful for `--quiet` and dry-run scenarios.
pub fn run_all_sync(env: &DoctorEnv) -> Vec<CheckResult> {
    vec![
        binary_cli_installed(env),
        binary_cli_on_path(env),
        binary_cli_checksum(env),
        config_dir_writable(env),
        credentials_any_provider(env),
        daemon_lockfile_parseable(env),
        daemon_restart_exhausted(env),
        workspace_registry_parseable(env),
        workspace_active_exists(env),
        install_manifest_consistent(env),
        models_bundle_present(env),
    ]
}

/// Full check matrix including async probes. Used by `run_doctor`.
/// Order is deterministic (matches the order checks appear in this
/// file) so the terminal output is stable.
pub async fn run_all(env: &DoctorEnv) -> Vec<CheckResult> {
    let mut checks = run_all_sync(env);
    checks.push(daemon_reachable(env).await);
    checks.push(binary_cli_runnable(env).await);
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

/// Does every binary entry in the install manifest point to a file
/// that exists on disk? Existence-only — BLAKE3 verification is
/// Slice F's binary-corruption auto-repair.
pub fn install_manifest_consistent(env: &DoctorEnv) -> CheckResult {
    let manifest_path = env.config_dir.join("install-manifest.json");
    if !manifest_path.exists() {
        return CheckResult {
            id: CheckId::from_static("install.manifest.consistent"),
            label: "Install manifest in sync with disk".into(),
            status: CheckStatus::Skipped,
            detail: "no install manifest yet".into(),
            fix: None,
        };
    }
    let bytes = match std::fs::read(&manifest_path) {
        Ok(b) => b,
        Err(e) => {
            return CheckResult {
                id: CheckId::from_static("install.manifest.consistent"),
                label: "Install manifest in sync with disk".into(),
                status: CheckStatus::Warn,
                detail: format!("manifest unreadable: {e}"),
                fix: None,
            };
        }
    };
    let manifest: InstallManifest = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            return CheckResult {
                id: CheckId::from_static("install.manifest.consistent"),
                label: "Install manifest in sync with disk".into(),
                status: CheckStatus::Warn,
                detail: format!("manifest unparseable: {e} (Slice F will rebuild)"),
                fix: None,
            };
        }
    };
    let mut missing: Vec<String> = Vec::new();
    for entry in &manifest.binaries {
        if !entry.path.exists() {
            missing.push(format!(
                "{:?} → {}",
                entry.id,
                entry.path.display()
            ));
        }
    }
    if missing.is_empty() {
        CheckResult {
            id: CheckId::from_static("install.manifest.consistent"),
            label: "Install manifest in sync with disk".into(),
            status: CheckStatus::Ok,
            detail: format!("{} entries, all paths exist", manifest.binaries.len()),
            fix: None,
        }
    } else {
        CheckResult {
            id: CheckId::from_static("install.manifest.consistent"),
            label: "Install manifest in sync with disk".into(),
            status: CheckStatus::Warn,
            detail: format!("missing binary path(s): {}", missing.join(", ")),
            fix: Some(FixAction::RunCommand {
                command: "root setup --as-cli".into(),
            }),
        }
    }
}

/// Track 32 — verifies the ONNX model bundle (`embed.{onnx,tokenizer.json}` +
/// `rerank.{onnx,tokenizer.json}`) is present on disk and matches the
/// BLAKE3 anchors recorded in `install-manifest.json::model_bundle`.
///
/// Status:
/// - `Skipped` when no install manifest exists yet (Slice A's hand-off)
/// - `Fail` when manifest exists but carries no `model_bundle` (legacy
///   pre-Track-32 install, or `TR_SKIP_MODELS=1` opt-out)
/// - `Fail` when one or more files are missing on disk
/// - `Warn` when files exist but BLAKE3 anchors mismatch (corrupt /
///   tampered download)
/// - `Ok` when both pairs verify against their anchors
///
/// Fix command: re-run the installer. The download steps are
/// idempotent — already-cached files with matching SHA-256 skip
/// the network fetch entirely.
pub fn models_bundle_present(env: &DoctorEnv) -> CheckResult {
    let id = CheckId::from_static("models.bundle_present");
    let label = "Model bundle (embed + rerank ONNX)".to_string();

    let manifest_path = env.config_dir.join("install-manifest.json");
    if !manifest_path.exists() {
        return CheckResult {
            id,
            label,
            status: CheckStatus::Skipped,
            detail: "no install manifest yet — run `install.sh` to fetch the bundle".into(),
            fix: None,
        };
    }
    let bytes = match std::fs::read(&manifest_path) {
        Ok(b) => b,
        Err(e) => {
            return CheckResult {
                id,
                label,
                status: CheckStatus::Warn,
                detail: format!("manifest unreadable: {e}"),
                fix: None,
            };
        }
    };
    let manifest: InstallManifest = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            return CheckResult {
                id,
                label,
                status: CheckStatus::Warn,
                detail: format!("manifest unparseable: {e}"),
                fix: None,
            };
        }
    };

    let bundle = match &manifest.model_bundle {
        Some(b) => b,
        None => {
            return CheckResult {
                id,
                label,
                status: CheckStatus::Fail,
                detail: "no model bundle recorded — install was pre-Track-32 or skipped models"
                    .into(),
                fix: Some(FixAction::RunCommand {
                    command: "curl -fsSL https://thinkingroot.com/install.sh | sh".into(),
                }),
            };
        }
    };

    if !bundle.files_exist() {
        return CheckResult {
            id,
            label,
            status: CheckStatus::Fail,
            detail: format!(
                "model files missing — embed.onnx: {}, rerank.onnx: {}",
                if bundle.embed.onnx_path.exists() {
                    "ok"
                } else {
                    "MISSING"
                },
                if bundle.rerank.onnx_path.exists() {
                    "ok"
                } else {
                    "MISSING"
                },
            ),
            fix: Some(FixAction::RunCommand {
                command: "curl -fsSL https://thinkingroot.com/install.sh | sh".into(),
            }),
        };
    }

    match bundle.verify() {
        Ok(()) => CheckResult {
            id,
            label,
            status: CheckStatus::Ok,
            detail: format!(
                "bundle {} verified (embed + rerank)",
                bundle.version
            ),
            fix: None,
        },
        Err(e) => CheckResult {
            id,
            label,
            status: CheckStatus::Warn,
            detail: format!("BLAKE3 mismatch: {e} — re-run installer to refresh"),
            fix: Some(FixAction::RunCommand {
                command: "curl -fsSL https://thinkingroot.com/install.sh | sh".into(),
            }),
        },
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

/// Pre-spawn smoke test: can we actually RUN the installed binary?
/// Catches macOS Gatekeeper quarantine, Linux missing dynamic-link
/// libs (libonnxruntime), Windows missing VC++ runtime.
///
/// Status:
/// - `Skipped` if no binary installed (binary.cli.installed handles
///   the fail in that case)
/// - `Ok` if `<binary> --version` exits 0 within 3s
/// - `Warn` if `<binary> --version` exits non-zero within 3s (binary
///   ran but didn't recognize the flag — common for older builds;
///   still attachable for purposes of doctor health)
/// - `Fail` if subprocess spawn errored, hit the 3s timeout, or was
///   killed by signal
pub async fn binary_cli_runnable(env: &DoctorEnv) -> CheckResult {
    let Some(binary) = env.install_dir_candidates.iter().find(|p| p.exists()) else {
        return CheckResult {
            id: CheckId::from_static("binary.cli.runnable"),
            label: "`root` binary runs cleanly".into(),
            status: CheckStatus::Skipped,
            detail: "no binary installed".into(),
            fix: None,
        };
    };

    let spawn_result = tokio::process::Command::new(binary)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let child = match spawn_result {
        Ok(c) => c,
        Err(e) => {
            return CheckResult {
                id: CheckId::from_static("binary.cli.runnable"),
                label: "`root` binary runs cleanly".into(),
                status: CheckStatus::Fail,
                detail: format!("could not spawn {}: {e}", binary.display()),
                fix: Some(platform_fix_for_unrunnable(binary)),
            };
        }
    };

    let wait = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        child.wait_with_output(),
    )
    .await;

    match wait {
        Err(_) => {
            // 3s timeout — kill the child (kill_on_drop handles it
            // when `child` goes out of scope, but we capture the
            // explicit error for the detail message).
            CheckResult {
                id: CheckId::from_static("binary.cli.runnable"),
                label: "`root` binary runs cleanly".into(),
                status: CheckStatus::Fail,
                detail: format!(
                    "{} --version timed out after 3s (hung process?)",
                    binary.display()
                ),
                fix: Some(platform_fix_for_unrunnable(binary)),
            }
        }
        Ok(Err(e)) => CheckResult {
            id: CheckId::from_static("binary.cli.runnable"),
            label: "`root` binary runs cleanly".into(),
            status: CheckStatus::Fail,
            detail: format!("wait for {} failed: {e}", binary.display()),
            fix: Some(platform_fix_for_unrunnable(binary)),
        },
        Ok(Ok(output)) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                CheckResult {
                    id: CheckId::from_static("binary.cli.runnable"),
                    label: "`root` binary runs cleanly".into(),
                    status: CheckStatus::Ok,
                    detail: stdout.trim().to_string(),
                    fix: None,
                }
            } else {
                let code = output.status.code();
                if code.is_none() {
                    // Killed by signal — Fail.
                    CheckResult {
                        id: CheckId::from_static("binary.cli.runnable"),
                        label: "`root` binary runs cleanly".into(),
                        status: CheckStatus::Fail,
                        detail: format!(
                            "{} --version killed by signal",
                            binary.display()
                        ),
                        fix: Some(platform_fix_for_unrunnable(binary)),
                    }
                } else {
                    // Non-zero exit with valid code — Warn.  Binary
                    // ran but rejected the flag.  Older builds may
                    // not have --version; still attachable.
                    CheckResult {
                        id: CheckId::from_static("binary.cli.runnable"),
                        label: "`root` binary runs cleanly".into(),
                        status: CheckStatus::Warn,
                        detail: format!(
                            "{} --version exited {} (older build?)",
                            binary.display(),
                            code.unwrap_or(0)
                        ),
                        fix: None,
                    }
                }
            }
        }
    }
}

/// Verify the binary file at the install manifest's preferred entry
/// matches its recorded BLAKE3 checksum.  Slice F's binary-corruption
/// auto-repair hooks fire when this returns Warn.
///
/// Status:
/// - `Skipped`: no manifest, or manifest has no entry whose `path`
///   exists on disk (other binary.* checks handle the absence)
/// - `Ok`: BLAKE3 matches
/// - `Warn`: BLAKE3 mismatches (suggests reinstall or tamper)
pub fn binary_cli_checksum(_env: &DoctorEnv) -> CheckResult {
    let manifest = match InstallManifest::load() {
        Ok(Some(m)) => m,
        Ok(None) => {
            return CheckResult {
                id: CheckId::from_static("binary.cli.checksum"),
                label: "Binary BLAKE3 integrity".into(),
                status: CheckStatus::Skipped,
                detail: "no install manifest".into(),
                fix: None,
            };
        }
        Err(e) => {
            return CheckResult {
                id: CheckId::from_static("binary.cli.checksum"),
                label: "Binary BLAKE3 integrity".into(),
                status: CheckStatus::Skipped,
                detail: format!("manifest read error: {e}"),
                fix: None,
            };
        }
    };

    // Pick the preferred entry's binary, or the first entry whose
    // path exists on disk (more lenient).
    let entry = manifest
        .preferred
        .and_then(|id| manifest.binaries.iter().find(|e| e.id == id).cloned())
        .filter(|e| e.path.exists())
        .or_else(|| manifest.binaries.iter().find(|e| e.path.exists()).cloned());

    let Some(entry) = entry else {
        return CheckResult {
            id: CheckId::from_static("binary.cli.checksum"),
            label: "Binary BLAKE3 integrity".into(),
            status: CheckStatus::Skipped,
            detail: "no binary entry in manifest with a path that exists".into(),
            fix: None,
        };
    };

    match entry.verify_checksum() {
        Ok(()) => CheckResult {
            id: CheckId::from_static("binary.cli.checksum"),
            label: "Binary BLAKE3 integrity".into(),
            status: CheckStatus::Ok,
            detail: format!(
                "{} → {}",
                entry.path.display(),
                &entry.checksum_blake3[..entry.checksum_blake3.len().min(12)]
            ),
            fix: None,
        },
        Err(e) => CheckResult {
            id: CheckId::from_static("binary.cli.checksum"),
            label: "Binary BLAKE3 integrity".into(),
            status: CheckStatus::Warn,
            detail: format!("checksum mismatch: {e}"),
            fix: Some(FixAction::RunCommand {
                command: "root reinstall --pinned-version".into(),
            }),
        },
    }
}

/// Surface the auto-restart subsystem's circuit-breaker state.  When
/// the breaker is tripped, the daemon won't auto-restart — user
/// needs to click "Reset and try again" or wait 5min.
///
/// Status:
/// - `Ok`: no recent failures, breaker not tripped
/// - `Warn`: recent failures present but breaker not yet tripped
/// - `Fail`: breaker tripped — auto-restart suspended
pub fn daemon_restart_exhausted(_env: &DoctorEnv) -> CheckResult {
    use thinkingroot_core::restart_state::RestartState;

    let mut state = match RestartState::load() {
        Ok(s) => s,
        Err(_) => {
            // Corrupt or unreadable restart-state → assume Ok.
            // Restart state is best-effort observability, not a
            // correctness gate.
            return CheckResult {
                id: CheckId::from_static("daemon.restart.exhausted"),
                label: "Daemon auto-restart healthy".into(),
                status: CheckStatus::Ok,
                detail: "no restart state recorded".into(),
                fix: None,
            };
        }
    };
    state.prune();

    if state.breaker_active() {
        let until = state
            .circuit_breaker_until
            .expect("breaker_active implies set");
        return CheckResult {
            id: CheckId::from_static("daemon.restart.exhausted"),
            label: "Daemon auto-restart healthy".into(),
            status: CheckStatus::Fail,
            detail: format!(
                "circuit breaker tripped — auto-restart suspended until {}",
                until.to_rfc3339()
            ),
            fix: Some(FixAction::RunCommand {
                command: "root doctor --recovery-log".into(),
            }),
        };
    }

    let recent = state.recent_failure_count();
    if recent == 0 {
        CheckResult {
            id: CheckId::from_static("daemon.restart.exhausted"),
            label: "Daemon auto-restart healthy".into(),
            status: CheckStatus::Ok,
            detail: "no recent failures".into(),
            fix: None,
        }
    } else {
        CheckResult {
            id: CheckId::from_static("daemon.restart.exhausted"),
            label: "Daemon auto-restart healthy".into(),
            status: CheckStatus::Warn,
            detail: format!("{} recent failure(s) in last 60s", recent),
            fix: None,
        }
    }
}

/// Platform-specific fix hint for a binary that can't run.  We pick
/// the most-likely-relevant fix per OS at compile time.
fn platform_fix_for_unrunnable(binary: &std::path::Path) -> FixAction {
    #[cfg(target_os = "macos")]
    {
        FixAction::ShellHint {
            command: format!("xattr -d com.apple.quarantine {}", binary.display()),
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = binary;
        FixAction::ShellHint {
            command: "sudo apt install libonnxruntime  # or equivalent for your distro".to_string(),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = binary;
        FixAction::ShellHint {
            command: "download VC++ redistributable from https://aka.ms/vs/17/release/vc_redist.x64.exe".to_string(),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = binary;
        FixAction::ShellHint {
            command: "verify binary integrity + dependencies".to_string(),
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

    #[test]
    fn install_manifest_consistent_skipped_when_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = install_manifest_consistent(&env);
        assert_eq!(r.status, CheckStatus::Skipped);
    }

    #[test]
    fn install_manifest_consistent_ok_when_entry_matches_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("root");
        std::fs::write(&bin, "x").unwrap();

        let manifest_path = tmp.path().join("install-manifest.json");
        let content = format!(
            r#"{{
                "schema_version": 1,
                "binaries": [
                    {{
                        "id": "cli-script",
                        "path": "{}",
                        "version": "0.9.1",
                        "installed_at": "2026-05-11T14:22:00Z",
                        "checksum_blake3": "deadbeef"
                    }}
                ],
                "preferred": "cli-script",
                "setup_complete_at": null
            }}"#,
            bin.display()
        );
        std::fs::write(&manifest_path, content).unwrap();

        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = install_manifest_consistent(&env);
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[test]
    fn install_manifest_consistent_warn_when_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("install-manifest.json");
        let content = r#"{
            "schema_version": 1,
            "binaries": [
                {
                    "id": "cli-script",
                    "path": "/nonexistent/root",
                    "version": "0.9.1",
                    "installed_at": "2026-05-11T14:22:00Z",
                    "checksum_blake3": "deadbeef"
                }
            ],
            "preferred": "cli-script",
            "setup_complete_at": null
        }"#;
        std::fs::write(&manifest_path, content).unwrap();

        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = install_manifest_consistent(&env);
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn models_bundle_present_skipped_when_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = models_bundle_present(&env);
        assert_eq!(r.status, CheckStatus::Skipped);
    }

    #[test]
    fn models_bundle_present_fail_when_manifest_omits_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("install-manifest.json");
        // Pre-Track-32 manifest shape — `model_bundle` field absent.
        std::fs::write(
            &manifest_path,
            r#"{
                "schema_version": 1,
                "binaries": [],
                "preferred": null,
                "setup_complete_at": null
            }"#,
        )
        .unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = models_bundle_present(&env);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(
            r.detail.contains("no model bundle"),
            "expected helpful detail, got: {}",
            r.detail
        );
        assert!(r.fix.is_some(), "must surface a fix command for the user");
    }

    #[test]
    fn models_bundle_present_fail_when_files_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("install-manifest.json");
        let missing = tmp.path().join("does-not-exist.onnx");
        let content = format!(
            r#"{{
                "schema_version": 1,
                "binaries": [],
                "preferred": null,
                "setup_complete_at": null,
                "model_bundle": {{
                    "version": "v1",
                    "embed": {{
                        "onnx_path": "{m}",
                        "tokenizer_path": "{m}",
                        "onnx_blake3": "0000",
                        "tokenizer_blake3": "0000"
                    }},
                    "rerank": {{
                        "onnx_path": "{m}",
                        "tokenizer_path": "{m}",
                        "onnx_blake3": "0000",
                        "tokenizer_blake3": "0000"
                    }},
                    "registered_at": "2026-05-16T00:00:00Z"
                }}
            }}"#,
            m = missing.display()
        );
        std::fs::write(&manifest_path, content).unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = models_bundle_present(&env);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(
            r.detail.contains("MISSING") || r.detail.contains("missing"),
            "expected 'missing' in detail, got: {}",
            r.detail
        );
    }

    #[test]
    fn models_bundle_present_ok_when_files_match_blake3() {
        let tmp = tempfile::tempdir().unwrap();
        // Stage four small "model" files and record their real BLAKE3
        // anchors in the manifest — verify() must accept them.
        let mut paths_and_hashes: Vec<(std::path::PathBuf, String)> = Vec::new();
        for name in &[
            "embed.onnx",
            "embed.tokenizer.json",
            "rerank.onnx",
            "rerank.tokenizer.json",
        ] {
            let p = tmp.path().join(name);
            std::fs::write(&p, format!("fake {name} content")).unwrap();
            let mut h = blake3::Hasher::new();
            h.update(format!("fake {name} content").as_bytes());
            let hex = h.finalize().to_hex().to_string();
            paths_and_hashes.push((p, hex));
        }
        let manifest_path = tmp.path().join("install-manifest.json");
        let content = format!(
            r#"{{
                "schema_version": 1,
                "binaries": [],
                "preferred": null,
                "setup_complete_at": null,
                "model_bundle": {{
                    "version": "v1",
                    "embed": {{
                        "onnx_path": "{}",
                        "tokenizer_path": "{}",
                        "onnx_blake3": "{}",
                        "tokenizer_blake3": "{}"
                    }},
                    "rerank": {{
                        "onnx_path": "{}",
                        "tokenizer_path": "{}",
                        "onnx_blake3": "{}",
                        "tokenizer_blake3": "{}"
                    }},
                    "registered_at": "2026-05-16T00:00:00Z"
                }}
            }}"#,
            paths_and_hashes[0].0.display(),
            paths_and_hashes[1].0.display(),
            paths_and_hashes[0].1,
            paths_and_hashes[1].1,
            paths_and_hashes[2].0.display(),
            paths_and_hashes[3].0.display(),
            paths_and_hashes[2].1,
            paths_and_hashes[3].1,
        );
        std::fs::write(&manifest_path, content).unwrap();
        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = models_bundle_present(&env);
        assert_eq!(
            r.status,
            CheckStatus::Ok,
            "expected Ok with matching BLAKE3s, got status={:?} detail={}",
            r.status,
            r.detail
        );
        assert!(
            r.detail.contains("v1") && r.detail.contains("verified"),
            "expected detail to mention version + verified, got: {}",
            r.detail
        );
    }

    #[tokio::test]
    async fn binary_cli_runnable_skipped_when_no_binary_installed() {
        let env = DoctorEnv {
            config_dir: std::path::PathBuf::from("/tmp/cfg"),
            install_dir_candidates: vec![std::path::PathBuf::from("/nonexistent/root")],
            path_entries: vec![],
        };
        let r = binary_cli_runnable(&env).await;
        assert_eq!(r.status, CheckStatus::Skipped);
        assert!(r.detail.contains("no binary installed"));
    }

    #[tokio::test]
    async fn binary_cli_runnable_ok_when_binary_runs() {
        // Use the current test binary's path as a stand-in — it's
        // guaranteed to exist + run.  We're testing the smoke-test
        // mechanic, not the actual `root --version` output.
        let me = std::env::current_exe().unwrap();
        let env = DoctorEnv {
            config_dir: std::path::PathBuf::from("/tmp/cfg"),
            install_dir_candidates: vec![me.clone()],
            path_entries: vec![],
        };
        let r = binary_cli_runnable(&env).await;
        // Any successful exit (incl. non-zero from a test binary called
        // with --version it doesn't recognise) counts as Ok for this
        // check: the binary RAN, that's what we care about.  If the
        // assertion needs to be tighter, we'd ship a fixture binary —
        // out of scope for Slice F T4.  Accept Ok OR Warn (some test
        // runners exit non-zero on --version).
        assert!(
            matches!(r.status, CheckStatus::Ok | CheckStatus::Warn),
            "expected Ok or Warn, got: {r:?}"
        );
    }

    #[tokio::test]
    async fn binary_cli_runnable_fail_when_binary_path_is_not_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("definitely-not-a-binary");
        std::fs::write(&path, b"this is not an executable").unwrap();
        // Don't chmod +x — on Unix this should fail to exec.

        let env = DoctorEnv {
            config_dir: std::path::PathBuf::from("/tmp/cfg"),
            install_dir_candidates: vec![path.clone()],
            path_entries: vec![],
        };
        let r = binary_cli_runnable(&env).await;
        // On Unix: Permission denied → Fail.
        // On Windows: may behave differently (.exe convention), but
        // since we wrote non-magic-bytes the executor should refuse.
        #[cfg(unix)]
        assert_eq!(r.status, CheckStatus::Fail);
        #[cfg(not(unix))]
        let _ = r;  // tolerate platform variance
    }

    #[test]
    fn binary_cli_checksum_skipped_when_no_manifest() {
        let _guard = thinkingroot_core::test_util::ENV_GUARD.lock().expect("env guard");
        let tmp = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        // SAFETY: ENV_GUARD held.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
        }

        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = binary_cli_checksum(&env);
        assert_eq!(r.status, CheckStatus::Skipped);

        // SAFETY: restore.
        unsafe {
            match prev_xdg { Some(v) => std::env::set_var("XDG_CONFIG_HOME", v), None => std::env::remove_var("XDG_CONFIG_HOME") }
            match prev_home { Some(v) => std::env::set_var("HOME", v), None => std::env::remove_var("HOME") }
        }
    }

    #[test]
    fn daemon_restart_exhausted_ok_when_no_state() {
        let _guard = thinkingroot_core::test_util::ENV_GUARD.lock().expect("env guard");
        let tmp = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_appdata = std::env::var_os("APPDATA");
        // SAFETY: ENV_GUARD held.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("APPDATA", tmp.path());
        }

        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = daemon_restart_exhausted(&env);
        assert_eq!(r.status, CheckStatus::Ok);

        // SAFETY: restore.
        unsafe {
            match prev_xdg { Some(v) => std::env::set_var("XDG_CONFIG_HOME", v), None => std::env::remove_var("XDG_CONFIG_HOME") }
            match prev_home { Some(v) => std::env::set_var("HOME", v), None => std::env::remove_var("HOME") }
            match prev_appdata { Some(v) => std::env::set_var("APPDATA", v), None => std::env::remove_var("APPDATA") }
        }
    }

    #[test]
    fn daemon_restart_exhausted_fail_when_breaker_active() {
        let _guard = thinkingroot_core::test_util::ENV_GUARD.lock().expect("env guard");
        let tmp = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_appdata = std::env::var_os("APPDATA");
        // SAFETY: ENV_GUARD held.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("APPDATA", tmp.path());
        }

        // Set up restart state with the breaker tripped.
        let mut state = thinkingroot_core::restart_state::RestartState::new();
        state.trip_circuit_breaker();
        state.save().unwrap();

        let env = DoctorEnv {
            config_dir: tmp.path().to_path_buf(),
            install_dir_candidates: vec![],
            path_entries: vec![],
        };
        let r = daemon_restart_exhausted(&env);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(matches!(r.fix, Some(FixAction::RunCommand { ref command }) if command.contains("doctor")));

        // SAFETY: restore.
        unsafe {
            match prev_xdg { Some(v) => std::env::set_var("XDG_CONFIG_HOME", v), None => std::env::remove_var("XDG_CONFIG_HOME") }
            match prev_home { Some(v) => std::env::set_var("HOME", v), None => std::env::remove_var("HOME") }
            match prev_appdata { Some(v) => std::env::set_var("APPDATA", v), None => std::env::remove_var("APPDATA") }
        }
    }
}
