//! `secrets.toml` reader / writer — the local credential store the
//! Root Function executor reads via `ctx.env`.
//!
//! Mirrors `config.rs`'s durability discipline verbatim: an exclusive
//! lock on a `.write` sentinel serialises concurrent writers, an atomic
//! `tempfile::persist` rename prevents torn files, and `chmod_0600`
//! keeps the file owner-only. Distinct file + lock from `auth.json` so
//! the two never contend.
//!
//! Resolution precedence (`resolve_secret`): a process env var wins over
//! the file. This is the single contract that lets the *same* secret
//! name work in two deployments — the cloud provisioner injects secrets
//! as env vars at container spawn, while a local/desktop user keeps them
//! in `secrets.toml`. The executor never needs to know which path it is.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::chmod_0600;
use crate::error::CloudError;

/// Schema version this client writes + reads. A reader refuses any file
/// whose `schema_version` exceeds this, matching `config::SCHEMA_VERSION`
/// semantics.
pub const SECRETS_SCHEMA_VERSION: u16 = 1;

/// The on-disk shape of `~/.config/thinkingroot/secrets.toml`.
///
/// Plaintext at rest by design — this is the *local* store, protected by
/// file mode 0600. The cloud never persists plaintext (it stores sealed
/// ciphertext and injects env vars at spawn); the two stores are
/// deliberately separate trust domains that share only the name→value
/// contract consumed by [`resolve_secret`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SecretsFile {
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,

    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

fn default_schema_version() -> u16 {
    SECRETS_SCHEMA_VERSION
}

impl SecretsFile {
    pub fn empty() -> Self {
        Self {
            schema_version: SECRETS_SCHEMA_VERSION,
            secrets: BTreeMap::new(),
        }
    }
}

/// Redacted Debug is not enough here — `SecretsFile` derives `Debug` for
/// tests, but production code must never `{:?}`-log a populated value.
/// Callers that need to surface contents use [`list_names`], which
/// returns names only.

pub fn secrets_path() -> Result<PathBuf, CloudError> {
    let base = dirs::config_dir()
        .ok_or_else(|| CloudError::Io(std::io::Error::other("no config dir")))?;
    Ok(base.join("thinkingroot").join("secrets.toml"))
}

pub fn secrets_lockfile_path() -> Result<PathBuf, CloudError> {
    let p = secrets_path()?;
    Ok(p.with_extension("toml.write"))
}

/// Load `secrets.toml`. Returns `Ok(None)` if the file is absent.
pub fn load() -> Result<Option<SecretsFile>, CloudError> {
    let path = secrets_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let mut s = String::new();
    fs::File::open(&path)
        .map_err(CloudError::Io)?
        .read_to_string(&mut s)
        .map_err(CloudError::Io)?;
    let file: SecretsFile = toml::from_str(&s).map_err(|e| CloudError::TomlParse(e.to_string()))?;
    if file.schema_version > SECRETS_SCHEMA_VERSION {
        return Err(CloudError::IncompatibleSchema {
            found: file.schema_version,
            max_supported: SECRETS_SCHEMA_VERSION,
        });
    }
    Ok(Some(file))
}

/// Atomically save the secrets file, holding the sentinel lock.
///
/// **Do NOT call from inside [`mutate`]** — that holds the same lock and
/// would deadlock on the second `lock_exclusive`. Use [`save_locked`]
/// from within the locked region instead.
pub fn save(file: &SecretsFile) -> Result<(), CloudError> {
    let lock_file = acquire_lock()?;
    save_locked(file)?;
    let _ = fs2::FileExt::unlock(&lock_file);
    Ok(())
}

/// Set a single secret, atomically. Creates the file if absent.
pub fn set(name: &str, value: &str) -> Result<(), CloudError> {
    mutate(|f| {
        f.secrets.insert(name.to_string(), value.to_string());
    })
    .map(|_| ())
}

/// Remove a single secret. Returns `true` if it existed.
pub fn unset(name: &str) -> Result<bool, CloudError> {
    let mut existed = false;
    mutate(|f| {
        existed = f.secrets.remove(name).is_some();
    })?;
    Ok(existed)
}

/// List secret names (never values). Honesty: empty vec when no file.
pub fn list_names() -> Result<Vec<String>, CloudError> {
    match load()? {
        Some(f) => Ok(f.secrets.keys().cloned().collect()),
        None => Ok(Vec::new()),
    }
}

/// Resolve a secret by name with the cloud↔local precedence contract:
/// a non-empty process env var wins; otherwise fall back to
/// `secrets.toml`. Returns `None` if neither source has it.
///
/// A read error on the file is treated as "absent" rather than
/// propagated — the executor must not crash a function run because the
/// optional local store is malformed; the env-var path still works.
pub fn resolve_secret(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    match load() {
        Ok(Some(f)) => f.secrets.get(name).cloned(),
        _ => None,
    }
}

/// Apply a closure to the loaded file, then save — serialised via the
/// sentinel lock so concurrent CLI/desktop writers never interleave.
fn mutate<F>(f: F) -> Result<SecretsFile, CloudError>
where
    F: FnOnce(&mut SecretsFile),
{
    let lock_file = acquire_lock()?;
    let mut file = match read_locked()? {
        Some(c) => c,
        None => SecretsFile::empty(),
    };
    f(&mut file);
    save_locked(&file)?;
    let _ = fs2::FileExt::unlock(&lock_file);
    Ok(file)
}

fn acquire_lock() -> Result<fs::File, CloudError> {
    let lock_path = secrets_lockfile_path()?;
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
    Ok(lock_file)
}

fn read_locked() -> Result<Option<SecretsFile>, CloudError> {
    // Same logic as `load`, factored so `mutate` reads inside the lock.
    load()
}

fn save_locked(file: &SecretsFile) -> Result<(), CloudError> {
    let path = secrets_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(CloudError::Io)?;
    }
    let body = toml::to_string_pretty(file).map_err(|e| CloudError::TomlParse(e.to_string()))?;
    let tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(CloudError::Io)?;
    fs::write(tmp.path(), body.as_bytes()).map_err(CloudError::Io)?;
    tmp.as_file().sync_all().map_err(CloudError::Io)?;
    tmp.persist(&path).map_err(|e| CloudError::Io(e.error))?;
    chmod_0600(&path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test config-dir isolation, identical discipline to
    /// `config.rs` tests: hold the workspace `ENV_GUARD` while mutating
    /// process env so no sibling test observes the intermediate state.
    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_xdg: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        prev_appdata: Option<std::ffi::OsString>,
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            // SAFETY: `_guard` is held for this Drop body.
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
        // SAFETY: ENV_GUARD held above serialises this mutation.
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
    fn load_returns_none_when_absent() {
        let _tmp = use_temp_home();
        assert!(load().unwrap().is_none());
        assert!(list_names().unwrap().is_empty());
    }

    #[test]
    fn set_get_round_trip() {
        let _tmp = use_temp_home();
        set("OPENAI_API_KEY", "sk-abc").unwrap();
        set("STRIPE_KEY", "rk-xyz").unwrap();
        let names = list_names().unwrap();
        assert_eq!(names, vec!["OPENAI_API_KEY".to_string(), "STRIPE_KEY".to_string()]);
        let loaded = load().unwrap().expect("file");
        assert_eq!(loaded.secrets.get("OPENAI_API_KEY").unwrap(), "sk-abc");
    }

    #[test]
    fn unset_reports_existence() {
        let _tmp = use_temp_home();
        set("A", "1").unwrap();
        assert!(unset("A").unwrap());
        assert!(!unset("A").unwrap());
        assert!(list_names().unwrap().is_empty());
    }

    #[test]
    fn save_sets_mode_0600_on_unix() {
        let _tmp = use_temp_home();
        set("K", "v").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(secrets_path().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn resolve_prefers_env_var_over_file() {
        let _tmp = use_temp_home();
        set("MY_SECRET", "from-file").unwrap();
        // No env var → file value.
        assert_eq!(resolve_secret("MY_SECRET").as_deref(), Some("from-file"));
        // Env var set → wins. SAFETY: ENV_GUARD held by TempHome.
        unsafe { std::env::set_var("MY_SECRET", "from-env") };
        assert_eq!(resolve_secret("MY_SECRET").as_deref(), Some("from-env"));
        // Empty env var → ignored, falls back to file.
        unsafe { std::env::set_var("MY_SECRET", "") };
        assert_eq!(resolve_secret("MY_SECRET").as_deref(), Some("from-file"));
        unsafe { std::env::remove_var("MY_SECRET") };
    }

    #[test]
    fn resolve_returns_none_for_unknown() {
        let _tmp = use_temp_home();
        assert!(resolve_secret("DEFINITELY_NOT_SET_ANYWHERE_XYZ").is_none());
    }

    #[test]
    fn schema_v99_load_returns_incompatible() {
        let _tmp = use_temp_home();
        let path = secrets_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "schema_version = 99\n[secrets]\n").unwrap();
        match load() {
            Err(CloudError::IncompatibleSchema { found: 99, max_supported: 1 }) => {}
            other => panic!("expected IncompatibleSchema, got {other:?}"),
        }
    }
}
