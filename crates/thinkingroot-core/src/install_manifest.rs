//! Install manifest — coordinating artifact for binary discovery
//! across the CLI install path, the desktop bundle, and future
//! self-heal recovery.
//!
//! Spec: `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current schema version of `install-manifest.json`. Bumping this
/// breaks compatibility with older readers; mirrors the
/// reader-bumped discipline in `cortex.rs::SCHEMA_VERSION`.
pub const SCHEMA_VERSION: u32 = 1;

/// Stable identifier for each install path. New variants land here
/// as new install surfaces ship (e.g. `BinaryId::Cargo` for
/// `cargo install thinkingroot-cli`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BinaryId {
    /// Installed by `install.sh` into `~/.local/bin/` or
    /// `/usr/local/bin/`.
    CliScript,
    /// Bundled inside the desktop `.app` / `.AppImage` / `.exe`
    /// at `<resource_dir>/binaries/thinkingroot-agent-runtime-<triple>`.
    DesktopBundle,
}

/// One discovered binary on the user's machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryEntry {
    /// Which install surface registered this entry.
    pub id: BinaryId,
    /// Absolute path to the binary file at registration time.
    pub path: PathBuf,
    /// `--version` string reported by the binary, or the version
    /// of the bundle that installed it.
    pub version: String,
    /// When this entry was registered. Used by self-heal to
    /// detect "binary disappeared after registration" cases.
    pub installed_at: DateTime<Utc>,
    /// BLAKE3 hex digest of the binary file at registration time.
    /// Used by Slice F's binary-corruption check.
    pub checksum_blake3: String,
}

/// The persisted manifest at `<config_dir>/thinkingroot/install-manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallManifest {
    /// Reader-bumped. A reader on version N refuses to parse
    /// `schema_version > N`.
    pub schema_version: u32,
    /// All registered binaries. Duplicates by `id` are merged on
    /// write (later registration wins).
    pub binaries: Vec<BinaryEntry>,
    /// Which entry CLI + desktop should prefer when multiple
    /// valid binaries exist. `None` means "no preference set yet"
    /// — the resolver in Slice C falls back to the first entry
    /// matching the caller's constraints.
    pub preferred: Option<BinaryId>,
    /// Set by the onboarding wizard when all setup-relevant
    /// `root doctor` checks pass. `None` means the user has not
    /// completed first-run setup.
    pub setup_complete_at: Option<DateTime<Utc>>,
}

/// All errors the manifest can produce. `#[non_exhaustive]` so we
/// can add variants in later slices without breaking match arms in
/// downstream crates.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    #[error("config directory unavailable (HOME unset?)")]
    NoConfigDir,
    #[error("manifest schema version {found} is newer than this binary supports ({supported})")]
    IncompatibleSchema { found: u32, supported: u32 },
    #[error("manifest I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: std::path::PathBuf,
        expected: String,
        actual: String,
    },
}

impl InstallManifest {
    /// Canonical on-disk path. Honours `XDG_CONFIG_HOME` on
    /// Linux/macOS and `APPDATA` on Windows via `dirs::config_dir`,
    /// matching `cortex::lock_path` exactly.
    pub fn path() -> Result<PathBuf, ManifestError> {
        let config_dir = dirs::config_dir().ok_or(ManifestError::NoConfigDir)?;
        Ok(config_dir.join("thinkingroot").join("install-manifest.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII helper that points `dirs::config_dir()` at a fresh
    /// tempdir on Linux/macOS/Windows by setting all three env vars
    /// (`XDG_CONFIG_HOME`, `HOME`, `APPDATA`) on construct and
    /// restoring them on drop. Mirrors `cortex.rs::ConfigDirOverride`.
    struct ConfigDirOverride {
        _tmp: tempfile::TempDir,
        prev_xdg: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        prev_appdata: Option<std::ffi::OsString>,
    }

    impl ConfigDirOverride {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
            let prev_home = std::env::var_os("HOME");
            let prev_appdata = std::env::var_os("APPDATA");
            // SAFETY: tests do not run in parallel within this binary
            // (cargo's default test isolation per-binary), and the
            // Drop impl below restores the previous values.
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", tmp.path());
                std::env::set_var("HOME", tmp.path());
                std::env::set_var("APPDATA", tmp.path());
            }
            Self {
                _tmp: tmp,
                prev_xdg,
                prev_home,
                prev_appdata,
            }
        }

        fn tmp_path(&self) -> &std::path::Path {
            self._tmp.path()
        }
    }

    impl Drop for ConfigDirOverride {
        fn drop(&mut self) {
            // SAFETY: same as new().
            unsafe {
                match self.prev_xdg.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
                match self.prev_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match self.prev_appdata.take() {
                    Some(v) => std::env::set_var("APPDATA", v),
                    None => std::env::remove_var("APPDATA"),
                }
            }
        }
    }

    #[test]
    fn round_trip_serializes_deterministically() {
        let manifest = InstallManifest {
            schema_version: SCHEMA_VERSION,
            binaries: vec![BinaryEntry {
                id: BinaryId::CliScript,
                path: std::path::PathBuf::from("/Users/x/.local/bin/root"),
                version: "0.9.1".to_string(),
                installed_at: chrono::DateTime::parse_from_rfc3339(
                    "2026-05-11T14:22:00Z",
                )
                .unwrap()
                .with_timezone(&chrono::Utc),
                checksum_blake3:
                    "f2ca1bb6c7e907d06dafe4687e579fce76b37e4e93b7605022da52e6ccc26fd2"
                        .to_string(),
            }],
            preferred: Some(BinaryId::CliScript),
            setup_complete_at: None,
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        assert!(
            json.contains("\"id\": \"cli-script\""),
            "BinaryId must serialize kebab-case; got: {json}"
        );
        assert!(
            json.contains("\"schema_version\": 1"),
            "SCHEMA_VERSION must serialize as integer 1; got: {json}"
        );

        let parsed: InstallManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, parsed);
    }

    #[test]
    fn path_honors_config_dir_override() {
        let cfg = ConfigDirOverride::new();
        let tmp_path = cfg.tmp_path().to_path_buf();

        let path = InstallManifest::path().expect("path resolved");
        assert!(
            path.starts_with(&tmp_path),
            "path={path:?}, tmp={tmp_path:?}"
        );
        assert_eq!(path.file_name().unwrap(), "install-manifest.json");
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            "thinkingroot"
        );
    }
}
