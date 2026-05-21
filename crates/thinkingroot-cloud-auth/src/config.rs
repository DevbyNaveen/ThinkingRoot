//! auth.json reader / writer with atomic write + sentinel lockfile.
//!
//! Spec: `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md`
//! §5.5 + §8.1.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::CloudError;

/// Schema version this client writes + reads. Reader-bumped: a v2
/// reader refuses v3+ with `CloudError::IncompatibleSchema`.
pub const SCHEMA_VERSION: u16 = 2;

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,

    #[serde(default)]
    pub token: Option<String>,

    pub server: String,

    #[serde(default)]
    pub handle: Option<String>,

    #[serde(default)]
    pub display_name: Option<String>,

    #[serde(default)]
    pub user_id: Option<String>,

    #[serde(default)]
    pub tier: Option<String>,

    #[serde(default)]
    pub credits_remaining: Option<u64>,

    #[serde(default)]
    pub credits_total: Option<u64>,

    #[serde(default)]
    pub credit_period_end: Option<DateTime<Utc>>,

    #[serde(default)]
    pub token_expires_at: Option<DateTime<Utc>>,

    #[serde(default)]
    pub me_refreshed_at: Option<DateTime<Utc>>,

    #[serde(default)]
    pub model_catalogue_cached: Option<ModelCatalogue>,

    /// Per-user Azure OpenAI provider vended by the hub at signin.
    /// `None` when the hub doesn't have APIM configured (dev) or when
    /// the user is signed out. Distinct from any BYOK Azure entry
    /// in the engine's own `ProvidersConfig` — the `managed: true`
    /// semantics here are: cloud-auth owns the lifecycle (mint at
    /// login, delete at logout), the engine just reads the values.
    #[serde(default)]
    pub managed_azure: Option<ManagedAzureProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedAzureProvider {
    /// OpenAI-compatible endpoint exposed by the hub's Azure APIM
    /// product (e.g. `https://tr-prod.azure-api.net/openai`).
    pub endpoint: String,
    /// Azure REST API version (e.g. `2024-12-01-preview`).
    pub api_version: String,
    /// Default deployment / model id (e.g. `gpt-4o-mini`).
    pub default_deployment: String,
    /// `Ocp-Apim-Subscription-Key` value. Sensitive — never log.
    pub api_key: String,
    /// When the hub minted this key. Used for `root doctor` display
    /// and for "rotate older than 90 days" hygiene.
    pub provisioned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCatalogue {
    pub fetched_at: DateTime<Utc>,
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    pub owned_by: String,
    pub credits_per_1k_input_tokens: u64,
    pub credits_per_1k_output_tokens: u64,
    pub context_window: u64,
}

/// Hub URL. landing-v2 (Next.js on Vercel) owns identity + the Azure
/// APIM gateway, so the same host serves `/auth/cli` (browser-login
/// bridge), `/api/auth/*` (signup/signin/signout), `/api/me`, and
/// `/api/v1/models`. Override via `Config::server` per-install or
/// at test time.
const DEFAULT_SERVER: &str = "https://thinkingroot.com";

fn default_schema_version() -> u16 {
    SCHEMA_VERSION
}

impl Config {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            token: None,
            server: DEFAULT_SERVER.to_string(),
            handle: None,
            display_name: None,
            user_id: None,
            tier: None,
            credits_remaining: None,
            credits_total: None,
            credit_period_end: None,
            token_expires_at: None,
            me_refreshed_at: None,
            model_catalogue_cached: None,
            managed_azure: None,
        }
    }

    pub fn is_signed_in(&self) -> bool {
        self.token.as_deref().map(|t| !t.is_empty()).unwrap_or(false)
    }
}

/// Redacted Display — never log the token. Honesty rule §8.6.
impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Config {{ schema_version: {}, server: {}, handle: {:?}, tier: {:?}, token: {} }}",
            self.schema_version,
            self.server,
            self.handle,
            self.tier,
            redacted_token(&self.token),
        )
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delegate to Display, which redacts the token. This makes
        // tracing::debug!(?config), dbg!(cfg), and {:?} formatting
        // all token-safe.
        std::fmt::Display::fmt(self, f)
    }
}

fn redacted_token(token: &Option<String>) -> String {
    match token.as_deref() {
        None => "<none>".to_string(),
        Some(t) if t.len() <= 8 => "<redacted>".to_string(),
        Some(t) => {
            let tail = &t[t.len() - 4..];
            format!("<redacted…{tail}>")
        }
    }
}

#[cfg(unix)]
fn chmod_0600(path: &std::path::Path) -> Result<(), CloudError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).map_err(CloudError::Io)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms).map_err(CloudError::Io)?;
    Ok(())
}

#[cfg(not(unix))]
fn chmod_0600(_path: &std::path::Path) -> Result<(), CloudError> {
    Ok(())
}

pub fn config_path() -> Result<PathBuf, CloudError> {
    let base = dirs::config_dir()
        .ok_or_else(|| CloudError::Io(std::io::Error::other("no config dir")))?;
    Ok(base.join("thinkingroot").join("auth.json"))
}

pub fn lockfile_path() -> Result<PathBuf, CloudError> {
    let p = config_path()?;
    Ok(p.with_extension("json.write"))
}

/// Load auth.json from disk. Returns `Ok(None)` if the file is absent.
/// Returns `Err(IncompatibleSchema)` if the file's schema_version exceeds
/// our `SCHEMA_VERSION`.
pub fn load() -> Result<Option<Config>, CloudError> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(CloudError::Io)?;
    let cfg: Config = serde_json::from_slice(&bytes).map_err(CloudError::JsonParse)?;
    if cfg.schema_version > SCHEMA_VERSION {
        return Err(CloudError::IncompatibleSchema {
            found: cfg.schema_version,
            max_supported: SCHEMA_VERSION,
        });
    }
    Ok(Some(cfg))
}

/// Atomically save the config. Acquires the sentinel lockfile.
///
/// **Do NOT call from within `update()`** — `update` holds the same
/// sentinel lock; calling `save` from inside the closure would
/// deadlock on the second `lock_exclusive` attempt. Use the internal
/// `save_locked` helper from inside the locked region.
///
/// Holds an exclusive lock on the sentinel lockfile for the duration;
/// this serialises concurrent writers (CLI + desktop running in the
/// same user account both bump credit balance).
pub fn save(cfg: &Config) -> Result<(), CloudError> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(CloudError::Io)?;
    }
    let lock_path = lockfile_path()?;
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(CloudError::Io)?;
    lock_file.lock_exclusive().map_err(CloudError::Io)?;

    let tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(CloudError::Io)?;
    serde_json::to_writer_pretty(tmp.as_file(), cfg).map_err(CloudError::JsonParse)?;
    tmp.as_file().sync_all().map_err(CloudError::Io)?;
    tmp.persist(&path)
        .map_err(|e| CloudError::Io(e.error))?;

    chmod_0600(&path)?;

    let _ = fs2::FileExt::unlock(&lock_file);
    Ok(())
}

/// Apply a closure to the loaded config, then save. Serialises all
/// readers + writers via the sentinel lockfile.
pub fn update<F>(f: F) -> Result<Config, CloudError>
where
    F: FnOnce(&mut Config),
{
    let lock_path = lockfile_path()?;
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(CloudError::Io)?;
    }
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(CloudError::Io)?;
    lock_file.lock_exclusive().map_err(CloudError::Io)?;

    let mut cfg = match read_locked()? {
        Some(c) => c,
        None => Config::empty(),
    };
    f(&mut cfg);
    save_locked(&cfg)?;
    let _ = fs2::FileExt::unlock(&lock_file);
    Ok(cfg)
}

fn read_locked() -> Result<Option<Config>, CloudError> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let mut s = String::new();
    fs::File::open(&path)
        .map_err(CloudError::Io)?
        .read_to_string(&mut s)
        .map_err(CloudError::Io)?;
    let cfg: Config = serde_json::from_str(&s).map_err(CloudError::JsonParse)?;
    if cfg.schema_version > SCHEMA_VERSION {
        return Err(CloudError::IncompatibleSchema {
            found: cfg.schema_version,
            max_supported: SCHEMA_VERSION,
        });
    }
    Ok(Some(cfg))
}

fn save_locked(cfg: &Config) -> Result<(), CloudError> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(CloudError::Io)?;
    }
    let tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(CloudError::Io)?;
    serde_json::to_writer_pretty(tmp.as_file(), cfg).map_err(CloudError::JsonParse)?;
    tmp.as_file().sync_all().map_err(CloudError::Io)?;
    tmp.persist(&path).map_err(|e| CloudError::Io(e.error))?;

    chmod_0600(&path)?;

    Ok(())
}

/// Wipe auth.json — used by `root logout`.
pub fn clear() -> Result<(), CloudError> {
    let path = config_path()?;
    if path.exists() {
        fs::remove_file(&path).map_err(CloudError::Io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Override the config dir for tests via env var. Per-test
    /// isolation: each test calls this and binds the result to a
    /// `let _tmp` so the tempdir lives for the test's scope.
    ///
    /// `std::env::set_var` is a process-wide mutation, so we acquire
    /// the workspace-wide `ENV_GUARD` from `thinkingroot-core` for
    /// the test's scope; this serialises against install_manifest,
    /// cortex, recovery_log tests in the same binary AND any other
    /// cloud-auth test that mutates env. The guard is held until the
    /// returned `TempHome` drops at end-of-test.
    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_xdg: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        prev_appdata: Option<std::ffi::OsString>,
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            // SAFETY: we still hold `_guard` for the duration of this
            // Drop body; no other test in this binary can observe the
            // intermediate env state.
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

    fn use_temp_home() -> TempHome {
        let guard = thinkingroot_core::test_util::ENV_GUARD
            .lock()
            .expect("env guard poisoned");
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_appdata = std::env::var_os("APPDATA");
        // SAFETY: ENV_GUARD held above serialises this mutation with
        // every other env-mutating test in the workspace.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("APPDATA", tmp.path());
        }
        TempHome {
            _guard: guard,
            _tmp: tmp,
            prev_xdg,
            prev_home,
            prev_appdata,
        }
    }

    #[test]
    fn redacted_token_hides_all_but_last_4() {
        let token = Some("tr_live_supersecret123sf3d".to_string());
        let rendered = redacted_token(&token);
        assert!(rendered.contains("sf3d"));
        assert!(!rendered.contains("supersecret"));
    }

    #[test]
    fn display_redacts_token_completely() {
        let mut cfg = Config::empty();
        cfg.token = Some("tr_live_topsecret_abcd".to_string());
        cfg.server = "https://api.example.com".into();
        let rendered = format!("{cfg}");
        assert!(rendered.contains("api.example.com"));
        assert!(!rendered.contains("topsecret"));
    }

    #[test]
    fn debug_redacts_token_same_as_display() {
        let mut cfg = Config::empty();
        cfg.token = Some("tr_live_topsecret_abcd".to_string());
        let debug_rendered = format!("{cfg:?}");
        let display_rendered = format!("{cfg}");
        assert_eq!(debug_rendered, display_rendered, "Debug must delegate to Display");
        assert!(!debug_rendered.contains("topsecret"), "Debug leaked token");
    }

    #[test]
    fn load_returns_none_when_file_absent() {
        let _tmp = use_temp_home();
        let result = load().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn round_trip_save_and_load() {
        let _tmp = use_temp_home();
        let mut cfg = Config::empty();
        cfg.token = Some("tr_live_xxx".into());
        cfg.handle = Some("naveen".into());
        cfg.tier = Some("pro".into());
        cfg.credits_remaining = Some(48153);
        cfg.credits_total = Some(50000);
        save(&cfg).unwrap();

        let loaded = load().unwrap().expect("loaded");
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn save_sets_mode_0600_on_unix() {
        let _tmp = use_temp_home();
        save(&Config::empty()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(config_path().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn schema_v99_load_returns_incompatible_schema() {
        let _tmp = use_temp_home();
        let path = config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"schema_version":99,"server":"https://x.com"}"#,
        )
        .unwrap();
        match load() {
            Err(CloudError::IncompatibleSchema {
                found: 99,
                max_supported: 2,
            }) => {}
            other => panic!("expected IncompatibleSchema, got {other:?}"),
        }
    }

    #[test]
    fn clear_removes_the_file() {
        let _tmp = use_temp_home();
        save(&Config::empty()).unwrap();
        assert!(config_path().unwrap().exists());
        clear().unwrap();
        assert!(!config_path().unwrap().exists());
    }

    #[test]
    fn update_atomically_mutates_balance() {
        let _tmp = use_temp_home();
        save(&Config::empty()).unwrap();
        let updated = update(|c| {
            c.credits_remaining = Some(1234);
            c.credits_total = Some(50000);
        })
        .unwrap();
        assert_eq!(updated.credits_remaining, Some(1234));
        let loaded = load().unwrap().expect("loaded");
        assert_eq!(loaded.credits_remaining, Some(1234));
    }
}
