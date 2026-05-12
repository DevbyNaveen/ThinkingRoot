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

    /// Persist the manifest atomically. Uses `tempfile::NamedTempFile`
    /// + `persist`, which is `rename(2)` on POSIX (atomic) and
    /// `ReplaceFileW` on Windows (atomic). A concurrent reader can
    /// never observe a torn write.
    ///
    /// Serialises concurrent writers via an exclusive `fs2` lock on a
    /// sibling `install-manifest.json.write` sentinel — mirrors the
    /// cortex.lock write-serialisation discipline.
    pub fn save(&self) -> Result<(), ManifestError> {
        let final_path = Self::path()?;
        let parent = final_path
            .parent()
            .expect("manifest path always has a parent");
        std::fs::create_dir_all(parent)?;

        // Advisory write lock (sibling sentinel).
        let sentinel_path = parent.join("install-manifest.json.write");
        let sentinel = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&sentinel_path)?;
        fs2::FileExt::lock_exclusive(&sentinel)?;

        // Write to a temp file in the same dir, then atomic-persist.
        let tmp = tempfile::NamedTempFile::new_in(parent)?;
        let json = serde_json::to_string_pretty(self)?;
        {
            use std::io::Write;
            let mut handle = tmp.as_file().try_clone()?;
            handle.write_all(json.as_bytes())?;
            handle.sync_all()?;
        }
        tmp.persist(&final_path)
            .map_err(|e| ManifestError::Io(e.error))?;

        // Mode 0600 on Unix — manifest may carry path info worth
        // protecting from other local users.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&final_path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&final_path, perms)?;
        }

        // Lock drops with `sentinel` going out of scope, but call
        // unlock explicitly in case a future panic in Drop leaves it
        // held on platforms where Drop is best-effort.
        let _ = fs2::FileExt::unlock(&sentinel);
        Ok(())
    }

    /// Read the manifest. Returns:
    /// - `Ok(None)` when the file doesn't exist (clean state) or is
    ///   empty / unparseable (corrupt-but-recoverable — Slice F's
    ///   disk-scan recovery picks up from here).
    /// - `Err(IncompatibleSchema)` when on-disk `schema_version` is
    ///   newer than this binary supports — silent downgrade risks
    ///   silent corruption (Honesty Rule #1).
    pub fn load() -> Result<Option<Self>, ManifestError> {
        let path = Self::path()?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        // Peek schema_version before fully parsing — gives a typed
        // error for future-schema instead of a generic parse error.
        #[derive(Deserialize)]
        struct VersionPeek {
            schema_version: u32,
        }
        let peek: VersionPeek = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    ?path,
                    error = %e,
                    "install manifest corrupt; treating as absent"
                );
                return Ok(None);
            }
        };
        if peek.schema_version > SCHEMA_VERSION {
            return Err(ManifestError::IncompatibleSchema {
                found: peek.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        let manifest: Self = serde_json::from_slice(&bytes)?;
        Ok(Some(manifest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise env-mutating tests within this binary so they don't
    /// trample each other. Mirrors `cortex.rs::ENV_GUARD`. Without
    /// this, `cargo test`'s default parallel execution would let one
    /// test observe another's mid-flight env state.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    /// RAII helper that points `dirs::config_dir()` at a fresh
    /// tempdir on Linux/macOS/Windows by setting all three env vars
    /// (`XDG_CONFIG_HOME`, `HOME`, `APPDATA`) on construct and
    /// restoring them on drop. Mirrors `cortex.rs::ConfigDirOverride`.
    struct ConfigDirOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_xdg: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        prev_appdata: Option<std::ffi::OsString>,
    }

    impl ConfigDirOverride {
        fn new() -> Self {
            let guard = ENV_GUARD.lock().expect("env guard poisoned");
            let tmp = tempfile::tempdir().expect("tempdir");
            let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
            let prev_home = std::env::var_os("HOME");
            let prev_appdata = std::env::var_os("APPDATA");
            // SAFETY: ENV_GUARD serialises with any other test in
            // this binary that uses ConfigDirOverride; the Drop impl
            // body below restores the previous env values synchronously
            // before any field tears down, so the guard is still held
            // while env state is being restored.
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", tmp.path());
                std::env::set_var("HOME", tmp.path());
                std::env::set_var("APPDATA", tmp.path());
            }
            Self {
                _guard: guard,
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

    #[test]
    fn save_writes_atomically_and_load_round_trips() {
        let _cfg = ConfigDirOverride::new();

        let manifest = InstallManifest {
            schema_version: SCHEMA_VERSION,
            binaries: vec![BinaryEntry {
                id: BinaryId::CliScript,
                path: std::path::PathBuf::from("/Users/x/.local/bin/root"),
                version: "0.9.1".to_string(),
                installed_at: chrono::Utc::now(),
                checksum_blake3: "deadbeef".repeat(8),
            }],
            preferred: Some(BinaryId::CliScript),
            setup_complete_at: None,
        };

        manifest.save().expect("save succeeds");

        let on_disk = InstallManifest::path().unwrap();
        assert!(on_disk.exists(), "manifest file present at {on_disk:?}");

        let loaded = InstallManifest::load()
            .expect("load succeeds")
            .expect("manifest present");
        assert_eq!(loaded, manifest);
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_mode_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let _cfg = ConfigDirOverride::new();

        let m = InstallManifest {
            schema_version: SCHEMA_VERSION,
            binaries: vec![],
            preferred: None,
            setup_complete_at: None,
        };
        m.save().unwrap();

        let mode = std::fs::metadata(InstallManifest::path().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }
}
