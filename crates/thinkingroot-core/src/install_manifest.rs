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
    /// Track 32 (2026-05-16). Model bundle staged at install time
    /// by `install.sh` / `install.ps1`. `None` on pre-Track-32
    /// installs and on `TR_SKIP_MODELS=1` opt-outs — `root doctor`'s
    /// `models.bundle_present` check surfaces this state so the
    /// user can `root doctor --fix` to fetch it.
    ///
    /// `#[serde(default)]` for back-compat with existing v1 manifests
    /// in the wild — readers on a daemon that hasn't been upgraded
    /// to Track 32 silently get `None` here instead of failing
    /// IncompatibleSchema (a soft addition, not a schema bump).
    #[serde(default)]
    pub model_bundle: Option<ModelBundle>,
}

/// One model file pair (ONNX + tokenizer) with BLAKE3 anchors for
/// tamper-evidence. Verified at every `root doctor` run and at
/// daemon boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFile {
    pub onnx_path: PathBuf,
    pub tokenizer_path: PathBuf,
    pub onnx_blake3: String,
    pub tokenizer_blake3: String,
}

impl ModelFile {
    /// Stream-hash both files and check against the recorded
    /// `*_blake3` digests. Returns `ChecksumMismatch` on the first
    /// divergence — caller can route this into `Decision::RepairNeeded`
    /// or surface the doctor `models.bundle_present` failure.
    pub fn verify(&self) -> Result<(), ManifestError> {
        verify_file_blake3(&self.onnx_path, &self.onnx_blake3)?;
        verify_file_blake3(&self.tokenizer_path, &self.tokenizer_blake3)?;
        Ok(())
    }

    /// Existence-only check (no BLAKE3). Cheaper than `verify` —
    /// used by `root doctor`'s presence check before optionally
    /// running the full hash audit.
    pub fn files_exist(&self) -> bool {
        self.onnx_path.exists() && self.tokenizer_path.exists()
    }
}

/// Track 32 model bundle. Versioned independently of the engine
/// release so a 0.9.x → 0.9.y bugfix doesn't force re-downloading
/// the ~340 MB model files. `version` matches the GitHub release
/// tag (`models-v1`, `models-v2`, ...) the installer pulled from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelBundle {
    /// e.g. `"v1"`. The installer stamps this from the tag it
    /// downloaded; `root doctor` compares it against the daemon's
    /// expected version (a future minimum-version requirement
    /// surfaces as a `models.bundle_version` doctor warning).
    pub version: String,
    /// Embedding model (AllMiniLM-L6-v2 INT8 ONNX, ~30 MB).
    pub embed: ModelFile,
    /// Cross-encoder reranker (gte-reranker-modernbert-base FP16
    /// ONNX, ~300 MB).
    pub rerank: ModelFile,
    /// When the installer registered this bundle.
    pub registered_at: DateTime<Utc>,
}

impl ModelBundle {
    /// Verify every file's BLAKE3 anchor. Order is `embed` then
    /// `rerank` so failures surface on the smaller download first
    /// (fast feedback for the common "corrupt download" case).
    pub fn verify(&self) -> Result<(), ManifestError> {
        self.embed.verify()?;
        self.rerank.verify()?;
        Ok(())
    }

    /// Existence-only check across both model files. Cheap pre-flight
    /// before the full `verify`.
    pub fn files_exist(&self) -> bool {
        self.embed.files_exist() && self.rerank.files_exist()
    }
}

fn verify_file_blake3(
    path: &std::path::Path,
    expected_hex: &str,
) -> Result<(), ManifestError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    let actual = hasher.finalize().to_hex().to_string();
    if actual == expected_hex {
        Ok(())
    } else {
        Err(ManifestError::ChecksumMismatch {
            path: path.to_path_buf(),
            expected: expected_hex.to_string(),
            actual,
        })
    }
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

        // Advisory write lock (sibling sentinel). The sentinel MUST
        // NOT be truncated on open. POSIX `flock` is per-open-file-
        // description: a second opener that calls `open()` with
        // `truncate(true)` gets a fresh fd that is NOT blocked by an
        // existing lock on the prior fd, defeating mutual exclusion
        // completely. `truncate(false)` keeps every opener pointing
        // at the same inode so `lock_exclusive` actually serialises.
        // Matches the cortex.lock write-sentinel discipline in
        // `crates/thinkingroot-core/src/cortex.rs::write_lock_inner`.
        let sentinel_path = parent.join("install-manifest.json.write");
        let sentinel = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
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
        Self::load_unlocked(&path)
    }

    /// Internal — load without acquiring the write lock. Used by
    /// `register_or_update` which already holds it.
    fn load_unlocked(path: &std::path::Path) -> Result<Option<Self>, ManifestError> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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

    /// Atomically register or update one binary entry. Holds the
    /// sibling `install-manifest.json.write` lock for the full
    /// read-modify-write cycle so two writers do not lose updates.
    ///
    /// If the manifest doesn't exist yet, creates a fresh one with the
    /// new entry and `preferred = Some(entry.id)`.
    ///
    /// If an entry with the same `id` already exists, replaces it in
    /// place. `preferred` is left unchanged on update — that's a user
    /// preference, not an install-side concern.
    pub fn register_or_update(entry: BinaryEntry) -> Result<(), ManifestError> {
        let path = Self::path()?;
        let parent = path
            .parent()
            .expect("manifest path always has a parent");
        std::fs::create_dir_all(parent)?;

        let sentinel_path = parent.join("install-manifest.json.write");
        let sentinel = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&sentinel_path)?;
        fs2::FileExt::lock_exclusive(&sentinel)?;

        let mut manifest = match Self::load_unlocked(&path)? {
            Some(m) => m,
            None => Self {
                schema_version: SCHEMA_VERSION,
                binaries: Vec::new(),
                preferred: Some(entry.id),
                setup_complete_at: None,
                model_bundle: None,
            },
        };

        if let Some(existing) = manifest.binaries.iter_mut().find(|e| e.id == entry.id) {
            *existing = entry;
        } else {
            manifest.binaries.push(entry);
        }

        // Inline atomic write under the held sentinel lock (do not
        // call `save()` — it would re-acquire the same lock).
        let tmp = tempfile::NamedTempFile::new_in(parent)?;
        let json = serde_json::to_string_pretty(&manifest)?;
        {
            use std::io::Write;
            let mut handle = tmp.as_file().try_clone()?;
            handle.write_all(json.as_bytes())?;
            handle.sync_all()?;
        }
        tmp.persist(&path)
            .map_err(|e| ManifestError::Io(e.error))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }

        let _ = fs2::FileExt::unlock(&sentinel);
        Ok(())
    }

    /// Stamp `setup_complete_at = now()` atomically under the sentinel
    /// lock. Used by the desktop's `mark_setup_complete` Tauri command
    /// when the EngineGate flips to healthy for the first time.
    ///
    /// The desktop layer used to do `load() → mutate → save()` from
    /// inside a Tauri command, leaving a window where two concurrent
    /// callers (`register_or_update` from the install bridge, an
    /// EngineGate React-Strict-Mode double-fire) could read the same
    /// manifest, each mutate locally, and write back — clobbering
    /// each other. Holding the sentinel lock across the whole
    /// load-modify-save closes the window.
    pub fn mark_setup_complete() -> Result<(), ManifestError> {
        let path = Self::path()?;
        let parent = path
            .parent()
            .expect("manifest path always has a parent");
        std::fs::create_dir_all(parent)?;

        let sentinel_path = parent.join("install-manifest.json.write");
        let sentinel = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&sentinel_path)?;
        fs2::FileExt::lock_exclusive(&sentinel)?;

        let mut manifest = match Self::load_unlocked(&path)? {
            Some(m) => m,
            None => Self {
                schema_version: SCHEMA_VERSION,
                binaries: Vec::new(),
                preferred: None,
                setup_complete_at: None,
                model_bundle: None,
            },
        };
        manifest.setup_complete_at = Some(chrono::Utc::now());

        // Inline atomic write under the held sentinel (mirrors the
        // pattern in `register_or_update` — do NOT delegate to
        // `save()` here, which would re-acquire this same lock and
        // block forever).
        let tmp = tempfile::NamedTempFile::new_in(parent)?;
        let json = serde_json::to_string_pretty(&manifest)?;
        {
            use std::io::Write;
            let mut handle = tmp.as_file().try_clone()?;
            handle.write_all(json.as_bytes())?;
            handle.sync_all()?;
        }
        tmp.persist(&path).map_err(|e| ManifestError::Io(e.error))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }

        let _ = fs2::FileExt::unlock(&sentinel);
        Ok(())
    }
}

impl BinaryEntry {
    /// Verify the on-disk file at `self.path` matches
    /// `self.checksum_blake3`. Streams the file — handles any
    /// binary size without loading it into memory.
    ///
    /// Used by `root doctor` (Slice B) and by Slice F's
    /// binary-corruption auto-repair.
    pub fn verify_checksum(&self) -> Result<(), ManifestError> {
        let mut file = std::fs::File::open(&self.path)?;
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut file, &mut hasher)?;
        let actual = hasher.finalize().to_hex().to_string();
        if actual == self.checksum_blake3 {
            Ok(())
        } else {
            Err(ManifestError::ChecksumMismatch {
                path: self.path.clone(),
                expected: self.checksum_blake3.clone(),
                actual,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::ENV_GUARD;

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
            model_bundle: None,
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
            model_bundle: None,
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
            model_bundle: None,
        };
        m.save().unwrap();

        let mode = std::fs::metadata(InstallManifest::path().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[test]
    fn load_refuses_future_schema_version() {
        let _cfg = ConfigDirOverride::new();

        // `InstallManifest::path()` is the cross-platform truth — on
        // macOS `dirs::config_dir()` resolves to
        // `$HOME/Library/Application Support`, not `$HOME` directly,
        // so we route through the resolver instead of joining
        // `tmp_path()` by hand.
        let manifest_path = InstallManifest::path().unwrap();
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest_path,
            format!(
                r#"{{
                    "schema_version": {},
                    "binaries": [],
                    "preferred": null,
                    "setup_complete_at": null
                }}"#,
                SCHEMA_VERSION + 7,
            ),
        )
        .unwrap();

        let result = InstallManifest::load();
        match result {
            Err(ManifestError::IncompatibleSchema { found, supported }) => {
                assert_eq!(found, SCHEMA_VERSION + 7);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            other => panic!("expected IncompatibleSchema, got {other:?}"),
        }
    }

    #[test]
    fn load_treats_corrupt_file_as_absent() {
        let _cfg = ConfigDirOverride::new();

        let manifest_path = InstallManifest::path().unwrap();
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, b"this is not json").unwrap();

        let result = InstallManifest::load().expect("load() returns Ok for corrupt file");
        assert!(result.is_none(), "corrupt file treated as absent (Slice F rebuilds)");
    }

    #[test]
    fn register_or_update_upserts_by_id() {
        let _cfg = ConfigDirOverride::new();

        // First registration → creates a new entry and sets preferred.
        let entry = BinaryEntry {
            id: BinaryId::CliScript,
            path: std::path::PathBuf::from("/Users/x/.local/bin/root"),
            version: "0.9.0".into(),
            installed_at: chrono::Utc::now(),
            checksum_blake3: "aa".repeat(32),
        };
        InstallManifest::register_or_update(entry.clone()).expect("first register");
        let m = InstallManifest::load().unwrap().unwrap();
        assert_eq!(m.binaries.len(), 1);
        assert_eq!(m.binaries[0].version, "0.9.0");
        assert_eq!(
            m.preferred,
            Some(BinaryId::CliScript),
            "preferred defaults to first-registered entry's id"
        );

        // Re-registration with same id → replaces in place. No duplicate row.
        let updated = BinaryEntry {
            version: "0.9.1".into(),
            installed_at: chrono::Utc::now(),
            checksum_blake3: "bb".repeat(32),
            ..entry.clone()
        };
        InstallManifest::register_or_update(updated).expect("upgrade register");
        let m = InstallManifest::load().unwrap().unwrap();
        assert_eq!(m.binaries.len(), 1, "no duplicate entry by id");
        assert_eq!(m.binaries[0].version, "0.9.1");

        // Different id → adds entry, preferred unchanged.
        let desktop = BinaryEntry {
            id: BinaryId::DesktopBundle,
            path: std::path::PathBuf::from(
                "/Applications/ThinkingRoot.app/Contents/Resources/binaries/thinkingroot-agent-runtime-aarch64-apple-darwin",
            ),
            version: "0.9.1".into(),
            installed_at: chrono::Utc::now(),
            checksum_blake3: "cc".repeat(32),
        };
        InstallManifest::register_or_update(desktop).expect("desktop register");
        let m = InstallManifest::load().unwrap().unwrap();
        assert_eq!(m.binaries.len(), 2);
        assert_eq!(
            m.preferred,
            Some(BinaryId::CliScript),
            "preferred sticky across new registrations"
        );
    }

    #[test]
    fn verify_checksum_matches_recorded_blake3() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_path = tmp.path().join("fake-root");
        let payload = b"#!/bin/sh\necho v0.9.1\n";
        std::fs::write(&bin_path, payload).unwrap();

        let expected = {
            let mut h = blake3::Hasher::new();
            h.update(payload);
            h.finalize().to_hex().to_string()
        };

        let entry = BinaryEntry {
            id: BinaryId::CliScript,
            path: bin_path.clone(),
            version: "0.9.1".into(),
            installed_at: chrono::Utc::now(),
            checksum_blake3: expected.clone(),
        };
        entry.verify_checksum().expect("checksum matches");

        // Mutate the file → mismatch.
        std::fs::write(&bin_path, b"#!/bin/sh\necho tampered\n").unwrap();
        let err = entry.verify_checksum().expect_err("mismatch caught");
        match err {
            ManifestError::ChecksumMismatch { expected: e, .. } => {
                assert_eq!(e, expected);
            }
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
    }
}
