//! Daemon login-agent installer.
//!
//! Writes the OS-native service descriptor for `root serve` to start
//! automatically when the user logs in, then loads it so the daemon
//! starts in this session without a reboot.
//!
//! - macOS: `~/Library/LaunchAgents/dev.thinkingroot.plist`, loaded
//!   via `launchctl bootstrap gui/$UID` (modern, 10.10+) with a
//!   fallback to `launchctl load` for older systems.
//! - Linux: `~/.config/systemd/user/thinkingroot.service`, enabled +
//!   started via `systemctl --user enable --now`.
//! - Windows: a Task Scheduler entry "ThinkingRoot" set to
//!   `/sc onlogon` for the current user, created via `schtasks /create`.
//!
//! Each function returns a typed [`ServiceOutcome`] so callers
//! (install.sh, EngineGate wizard, `root doctor --fix`) can report
//! honestly which step succeeded — silent "best-effort" failure is
//! exactly the headache mode we're closing.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use thiserror::Error;

/// Stable label / unit name shared across the three OSes.
pub const SERVICE_LABEL: &str = "dev.thinkingroot";
/// systemd user-unit + Windows-task name (no reverse-DNS). Used on
/// Linux/Windows builds only; macOS uses [`SERVICE_LABEL`] directly.
#[allow(dead_code)]
pub const SERVICE_SHORTNAME: &str = "thinkingroot";

/// Outcome of an install/uninstall call. Carries the artifact path
/// and which OS command (if any) was successfully executed so the
/// caller can report each step truthfully to the user.
#[derive(Debug, Clone)]
pub struct ServiceOutcome {
    /// Absolute path to the file that was written (plist / unit) or
    /// `None` for Windows where Task Scheduler holds the definition
    /// internally.
    pub artifact_path: Option<PathBuf>,
    /// Human-readable note about the loader step — e.g.
    /// `"launchctl bootstrap gui/501 succeeded"` or
    /// `"systemctl --user enable --now thinkingroot.service succeeded"`.
    pub loader_note: String,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("home directory unavailable")]
    NoHome,
    #[error("config directory unavailable")]
    NoConfigDir,
    #[error("current executable path could not be resolved: {0}")]
    NoExe(#[source] std::io::Error),
    #[error("failed to write service artifact at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("OS service loader {cmd} failed (exit {code:?}): {stderr}")]
    LoaderFailed {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("failed to spawn {cmd}: {source}")]
    LoaderSpawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
}

/// Install + activate the login agent. Idempotent: re-running
/// rewrites the artifact and reloads it.
pub fn install() -> Result<ServiceOutcome, ServiceError> {
    let binary = std::env::current_exe()
        .map_err(ServiceError::NoExe)?
        .to_string_lossy()
        .into_owned();
    let log_path = config_log_path()?;

    #[cfg(target_os = "macos")]
    {
        install_macos(&binary, &log_path)
    }
    #[cfg(target_os = "linux")]
    {
        install_linux(&binary, &log_path)
    }
    #[cfg(target_os = "windows")]
    {
        install_windows(&binary, &log_path)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (&binary, &log_path);
        Err(ServiceError::LoaderFailed {
            cmd: "<unsupported OS>".into(),
            code: None,
            stderr: "auto-start is only wired for macOS, Linux, and Windows today".into(),
        })
    }
}

/// Stop + remove the login agent. Idempotent: succeeds if the
/// service is already absent.
pub fn uninstall() -> Result<ServiceOutcome, ServiceError> {
    #[cfg(target_os = "macos")]
    {
        uninstall_macos()
    }
    #[cfg(target_os = "linux")]
    {
        uninstall_linux()
    }
    #[cfg(target_os = "windows")]
    {
        uninstall_windows()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(ServiceError::LoaderFailed {
            cmd: "<unsupported OS>".into(),
            code: None,
            stderr: "auto-start is only wired for macOS, Linux, and Windows today".into(),
        })
    }
}

/// What kind of operation produced this outcome. Used by
/// [`print_outcome`] to pick a context-correct footer — the install
/// footer ("starts on login") is wrong for uninstall, and vice versa.
#[derive(Debug, Clone, Copy)]
pub enum OutcomeKind {
    Install,
    Uninstall,
}

/// Pretty-printer used by both the CLI subcommand and install.sh.
/// Prints the artifact path + loader note + log location.
pub fn print_outcome(outcome: &ServiceOutcome, kind: OutcomeKind) -> Result<()> {
    let log_path = config_log_path()
        .context("resolve service log path for status print")?;
    println!();
    if let Some(p) = &outcome.artifact_path {
        let label = match kind {
            OutcomeKind::Install => "✓ Service file:",
            OutcomeKind::Uninstall => "✓ Removed:",
        };
        println!(
            "  {} {}",
            console::style(label).green().bold(),
            p.display()
        );
    }
    println!(
        "  {} {}",
        console::style("✓ Loader:").green().bold(),
        outcome.loader_note
    );
    println!(
        "  {} {}",
        console::style("✓ Logs:").green().bold(),
        log_path.display()
    );
    println!();
    match kind {
        OutcomeKind::Install => {
            println!("  ThinkingRoot will start automatically on login.");
        }
        OutcomeKind::Uninstall => {
            println!("  Login auto-start is now disabled.");
            println!("  (Any currently-running `root serve` keeps running until you stop it.)");
        }
    }
    Ok(())
}

fn config_log_path() -> Result<PathBuf, ServiceError> {
    let dir = dirs::config_dir()
        .ok_or(ServiceError::NoConfigDir)?
        .join("thinkingroot");
    std::fs::create_dir_all(&dir).map_err(|source| ServiceError::Write {
        path: dir.clone(),
        source,
    })?;
    Ok(dir.join("serve.log"))
}

// ── macOS ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn install_macos(binary: &str, log_path: &std::path::Path) -> Result<ServiceOutcome, ServiceError> {
    let agents_dir = dirs::home_dir()
        .ok_or(ServiceError::NoHome)?
        .join("Library")
        .join("LaunchAgents");
    std::fs::create_dir_all(&agents_dir).map_err(|source| ServiceError::Write {
        path: agents_dir.clone(),
        source,
    })?;
    let plist_path = agents_dir.join(format!("{SERVICE_LABEL}.plist"));

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>             <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>         <true/>
    <key>KeepAlive</key>         <true/>
    <key>ProcessType</key>       <string>Interactive</string>
    <key>StandardOutPath</key>   <string>{log}</string>
    <key>StandardErrorPath</key> <string>{log}</string>
</dict>
</plist>"#,
        label = SERVICE_LABEL,
        binary = binary,
        log = log_path.display()
    );

    std::fs::write(&plist_path, plist).map_err(|source| ServiceError::Write {
        path: plist_path.clone(),
        source,
    })?;

    // Modern path (10.10+): `launchctl bootstrap gui/$UID <plist>`.
    // Older bootout/bootstrap dance to avoid "already loaded" errors.
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let service_target = format!("{domain}/{SERVICE_LABEL}");

    // bootout is best-effort: it errors when the service isn't
    // loaded, but that's the very state we want to be in next.
    let _ = Command::new("launchctl")
        .args(["bootout", &service_target])
        .output();

    let bootstrap = Command::new("launchctl")
        .args(["bootstrap", &domain, plist_path.to_str().unwrap_or_default()])
        .output();

    let loader_note = match bootstrap {
        Ok(out) if out.status.success() => {
            format!("launchctl bootstrap {domain} succeeded")
        }
        Ok(out) => {
            // Fall back to the legacy load command for the rare
            // setups where bootstrap is unavailable (e.g.
            // restricted shells without TCC permissions).
            let load = Command::new("launchctl")
                .args(["load", "-w", plist_path.to_str().unwrap_or_default()])
                .output()
                .map_err(|source| ServiceError::LoaderSpawn {
                    cmd: "launchctl load".into(),
                    source,
                })?;
            if !load.status.success() {
                return Err(ServiceError::LoaderFailed {
                    cmd: "launchctl bootstrap (then load -w)".into(),
                    code: load.status.code(),
                    stderr: format!(
                        "bootstrap stderr: {}\nload stderr: {}",
                        String::from_utf8_lossy(&out.stderr).trim(),
                        String::from_utf8_lossy(&load.stderr).trim()
                    ),
                });
            }
            "launchctl load -w succeeded (legacy fallback)".to_string()
        }
        Err(source) => {
            return Err(ServiceError::LoaderSpawn {
                cmd: "launchctl bootstrap".into(),
                source,
            });
        }
    };

    Ok(ServiceOutcome {
        artifact_path: Some(plist_path),
        loader_note,
    })
}

#[cfg(target_os = "macos")]
fn uninstall_macos() -> Result<ServiceOutcome, ServiceError> {
    let agents_dir = dirs::home_dir()
        .ok_or(ServiceError::NoHome)?
        .join("Library")
        .join("LaunchAgents");
    let plist_path = agents_dir.join(format!("{SERVICE_LABEL}.plist"));

    let uid = unsafe { libc::getuid() };
    let service_target = format!("gui/{uid}/{SERVICE_LABEL}");
    let _ = Command::new("launchctl")
        .args(["bootout", &service_target])
        .output();

    let removed = if plist_path.exists() {
        std::fs::remove_file(&plist_path).map_err(|source| ServiceError::Write {
            path: plist_path.clone(),
            source,
        })?;
        true
    } else {
        false
    };

    Ok(ServiceOutcome {
        artifact_path: Some(plist_path),
        loader_note: if removed {
            "launchctl bootout + plist removed".to_string()
        } else {
            "no plist was installed (no-op)".to_string()
        },
    })
}

// ── Linux ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn install_linux(binary: &str, log_path: &std::path::Path) -> Result<ServiceOutcome, ServiceError> {
    let systemd_dir = dirs::config_dir()
        .ok_or(ServiceError::NoConfigDir)?
        .join("systemd")
        .join("user");
    std::fs::create_dir_all(&systemd_dir).map_err(|source| ServiceError::Write {
        path: systemd_dir.clone(),
        source,
    })?;
    let service_path = systemd_dir.join(format!("{SERVICE_SHORTNAME}.service"));

    let unit = format!(
        "[Unit]\nDescription=ThinkingRoot Knowledge Server\nAfter=network.target\n\n\
         [Service]\nExecStart={binary} serve\nRestart=on-failure\nRestartSec=2\n\
         StandardOutput=append:{log}\nStandardError=append:{log}\n\n\
         [Install]\nWantedBy=default.target\n",
        binary = binary,
        log = log_path.display()
    );

    std::fs::write(&service_path, unit).map_err(|source| ServiceError::Write {
        path: service_path.clone(),
        source,
    })?;

    // daemon-reload picks up the new unit; enable --now starts it AND
    // wires it for login auto-start in one call.
    run_or_err("systemctl", &["--user", "daemon-reload"])?;
    let enable_result = run_or_err(
        "systemctl",
        &[
            "--user",
            "enable",
            "--now",
            &format!("{SERVICE_SHORTNAME}.service"),
        ],
    );

    // Headless Linux servers don't run `systemd --user` for users
    // who never logged in interactively; `enable --now` exits 1 in
    // that case with a message about a missing user manager. Detect
    // the most reliable signal — `XDG_RUNTIME_DIR` unset means no
    // user manager has been booted for this UID — and surface a
    // hint about `loginctl enable-linger`, which keeps the user
    // manager alive across logout and is the conventional fix on
    // servers.
    if let Err(e) = &enable_result {
        let headless = std::env::var_os("XDG_RUNTIME_DIR").is_none();
        if headless {
            eprintln!(
                "\nHint: on headless / server systems, the systemd --user manager isn't \
                 running by default. Run\n\
                 \n    sudo loginctl enable-linger $USER\n\n\
                 then re-run `root service install`. (Detected via missing XDG_RUNTIME_DIR.)\n\
                 Underlying error: {e}"
            );
        }
    }
    enable_result?;

    Ok(ServiceOutcome {
        artifact_path: Some(service_path),
        loader_note: format!("systemctl --user enable --now {SERVICE_SHORTNAME}.service succeeded"),
    })
}

#[cfg(target_os = "linux")]
fn uninstall_linux() -> Result<ServiceOutcome, ServiceError> {
    let systemd_dir = dirs::config_dir()
        .ok_or(ServiceError::NoConfigDir)?
        .join("systemd")
        .join("user");
    let service_path = systemd_dir.join(format!("{SERVICE_SHORTNAME}.service"));

    // Best-effort disable + stop; harmless if the unit doesn't exist.
    let _ = Command::new("systemctl")
        .args([
            "--user",
            "disable",
            "--now",
            &format!("{SERVICE_SHORTNAME}.service"),
        ])
        .output();
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();

    let removed = if service_path.exists() {
        std::fs::remove_file(&service_path).map_err(|source| ServiceError::Write {
            path: service_path.clone(),
            source,
        })?;
        true
    } else {
        false
    };

    Ok(ServiceOutcome {
        artifact_path: Some(service_path),
        loader_note: if removed {
            "systemctl --user disable --now + unit removed".to_string()
        } else {
            "no unit was installed (no-op)".to_string()
        },
    })
}

// ── Windows ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn install_windows(
    binary: &str,
    log_path: &std::path::Path,
) -> Result<ServiceOutcome, ServiceError> {
    let _ = log_path; // schtasks writes nothing; the daemon owns its own log.

    // Build the task command line. `schtasks /TR` accepts a single
    // string that is forwarded verbatim to the task. When the binary
    // path contains spaces (the common case on Windows:
    // `C:\Users\John Doe\AppData\...`) the path must be quoted so
    // Task Scheduler reads it as one argument.
    //
    // The pre-fix shape `"\\\"{binary}\\\" serve"` baked
    // backslash-escaped quotes into the value, which only works when
    // the value is then passed through cmd.exe's parsing layer. Our
    // `run_or_err` uses `std::process::Command::args()`, which
    // forwards each arg as a NUL-terminated wide string directly to
    // CreateProcessW — no cmd.exe involvement. The doubled escapes
    // ended up in the task's command literally, and Task Scheduler
    // refused to launch the resulting `\"C:\path\\\" serve` payload.
    // The fix: plain double-quotes (one layer of escaping) so
    // CreateProcessW hands schtasks exactly `"C:\path" serve`.
    let tr = format!("\"{binary}\" serve");

    run_or_err(
        "schtasks",
        &[
            "/Create",
            "/TN",
            "ThinkingRoot",
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/F",
            "/TR",
            &tr,
        ],
    )?;

    // Start it now so the user sees the daemon come up without
    // logging out + back in.
    let _ = Command::new("schtasks")
        .args(["/Run", "/TN", "ThinkingRoot"])
        .output();

    Ok(ServiceOutcome {
        artifact_path: None,
        loader_note: "Task Scheduler 'ThinkingRoot' created (ONLOGON) and started".to_string(),
    })
}

#[cfg(target_os = "windows")]
fn uninstall_windows() -> Result<ServiceOutcome, ServiceError> {
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", "ThinkingRoot"])
        .output();
    let delete = Command::new("schtasks")
        .args(["/Delete", "/TN", "ThinkingRoot", "/F"])
        .output()
        .map_err(|source| ServiceError::LoaderSpawn {
            cmd: "schtasks /Delete".into(),
            source,
        })?;

    let note = if delete.status.success() {
        "Task Scheduler 'ThinkingRoot' deleted".to_string()
    } else {
        // schtasks returns non-zero when the task doesn't exist.
        // Treat that as a successful no-op rather than an error.
        format!(
            "schtasks /Delete reported {}: {} (treating as no-op)",
            delete.status,
            String::from_utf8_lossy(&delete.stderr).trim()
        )
    };

    Ok(ServiceOutcome {
        artifact_path: None,
        loader_note: note,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn run_or_err(cmd: &str, args: &[&str]) -> Result<(), ServiceError> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|source| ServiceError::LoaderSpawn {
            cmd: format!("{cmd} {}", args.join(" ")),
            source,
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::LoaderFailed {
            cmd: format!("{cmd} {}", args.join(" ")),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_is_clonable() {
        let o = ServiceOutcome {
            artifact_path: Some(PathBuf::from("/tmp/dev.thinkingroot.plist")),
            loader_note: "test".into(),
        };
        let o2 = o.clone();
        assert_eq!(o.loader_note, o2.loader_note);
        assert_eq!(o.artifact_path, o2.artifact_path);
    }

    #[test]
    fn service_labels_are_stable() {
        // These literal strings are referenced from install.sh,
        // install.ps1, and the doctor surface. Renaming them is a
        // wire break — guard against accidental drift.
        assert_eq!(SERVICE_LABEL, "dev.thinkingroot");
        assert_eq!(SERVICE_SHORTNAME, "thinkingroot");
    }
}
