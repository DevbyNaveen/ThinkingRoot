//! Doctor check substrate — types only.  Pure-data so the registry
//! in `checks.rs` and the renderers in `format.rs` can each evolve
//! independently.
//!
//! Spec: `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md` §2.
//!
//! Coexists with `crate::doctor_cmd` until the migration in Task 12
//! deletes the legacy implementation.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Stable string identifier for a check. The Desktop UI (Slice D)
/// hard-codes these IDs to map checks to UI rows.  Adding a new ID
/// is non-breaking; renaming one IS breaking.  Treat IDs as
/// commit-locked once shipped.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckId(pub String);

impl CheckId {
    pub fn from_static(s: &'static str) -> Self {
        Self(s.to_string())
    }
}

/// One row in a doctor report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub id: CheckId,
    pub label: String,
    pub status: CheckStatus,
    pub detail: String,
    pub fix: Option<FixAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FixAction {
    /// Print the command for the user to copy.  No automatic execution.
    ShellHint { command: String },
    /// Execute the named `root` subcommand.
    RunCommand { command: String },
    /// Interactively prompt for a credential value and write to credentials.toml.
    FillIn { prompt: String, credential_key: String },
}

/// Injected environment.  Production code uses
/// `DoctorEnv::from_real_filesystem()`; tests construct fakes inline.
#[derive(Debug, Clone)]
pub struct DoctorEnv {
    pub config_dir: PathBuf,
    pub install_dir_candidates: Vec<PathBuf>,
    pub path_entries: Vec<PathBuf>,
}

impl DoctorEnv {
    pub fn from_real_filesystem() -> Result<Self, anyhow::Error> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("HOME / config dir unavailable"))?
            .join("thinkingroot");
        let install_dir_candidates = vec![
            PathBuf::from("/usr/local/bin/root"),
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join(".local/bin/root"),
        ];
        let path_entries = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        Ok(Self {
            config_dir,
            install_dir_candidates,
            path_entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_result_serializes_with_stable_field_names() {
        let r = CheckResult {
            id: CheckId::from_static("binary.cli.installed"),
            label: "ThinkingRoot CLI binary".to_string(),
            status: CheckStatus::Ok,
            detail: "/Users/x/.local/bin/root v0.9.1".to_string(),
            fix: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"id\":\"binary.cli.installed\""), "got: {json}");
        assert!(json.contains("\"status\":\"ok\""), "got: {json}");
        assert!(json.contains("\"fix\":null"), "got: {json}");
    }

    #[test]
    fn check_status_serializes_kebab_lowercase() {
        for (s, expected) in [
            (CheckStatus::Ok, "\"ok\""),
            (CheckStatus::Warn, "\"warn\""),
            (CheckStatus::Fail, "\"fail\""),
            (CheckStatus::Skipped, "\"skipped\""),
        ] {
            assert_eq!(serde_json::to_string(&s).unwrap(), expected);
        }
    }

    #[test]
    fn fix_action_kinds_round_trip() {
        let actions = vec![
            FixAction::ShellHint { command: "export PATH=...".into() },
            FixAction::RunCommand { command: "root provider add".into() },
            FixAction::FillIn { prompt: "API key:".into(), credential_key: "OPENAI_API_KEY".into() },
        ];
        for a in actions {
            let json = serde_json::to_string(&a).unwrap();
            let back: FixAction = serde_json::from_str(&json).unwrap();
            assert_eq!(a, back);
        }
    }
}
