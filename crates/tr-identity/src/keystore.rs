//! On-disk keystore.
//!
//! The default location is `~/.config/thinkingroot/keys/trusted.json`
//! (resolved via the `dirs` crate so it works on every platform).
//! Per-user trust decisions live in this file; the format is plain
//! JSON so the user can audit it without any tool.
//!
//! This is intentionally minimal — no Secure Enclave, no TPM, no
//! key-wrapping. Phase F.1 trades convenience for auditability;
//! later phases can add a secure-storage backend behind the same
//! [`Keystore`] interface.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::keypair::PublicKeyRef;

/// One trusted public key plus a stable identifier.
///
/// `id` is the lookup key — for author-signed packs (T1) the
/// matching field on the manifest is the first `authors[]` entry.
/// For DID-resolved keys it can be a `did:web:…` or `did:tr:agent:…`
/// string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedKey {
    /// Identifier the manifest's author claim is matched against.
    pub id: String,
    /// 32-byte Ed25519 public key.
    pub public: PublicKeyRef,
    /// Free-form note describing where this key came from
    /// (e.g. `"imported from did:web:alice.example"` or
    /// `"hand-pasted on 2026-04-27"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// On-disk and in-memory store of trusted keys.
///
/// Construction:
/// - [`Keystore::open`] reads from a path (creating an empty store
///   if the file is missing).
/// - [`Keystore::default_path`] returns the canonical user-config
///   location used by `root` and the desktop.
#[derive(Debug, Clone, Default)]
pub struct Keystore {
    keys: BTreeMap<String, TrustedKey>,
    path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    keys: Vec<TrustedKey>,
}

impl Keystore {
    /// Construct an empty in-memory keystore (no on-disk path).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct a keystore seeded with the given keys. Duplicate
    /// `id`s collapse to the last entry.
    pub fn with_keys(keys: impl IntoIterator<Item = TrustedKey>) -> Self {
        let mut store = Self::empty();
        for k in keys {
            store.import_trusted(k);
        }
        store
    }

    /// Default on-disk path: `<config_dir>/thinkingroot/keys/trusted.json`.
    /// Returns `None` if no platform config directory is available
    /// (e.g. headless CI without `$HOME`).
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("thinkingroot").join("keys").join("trusted.json"))
    }

    /// Load the keystore from a JSON file at `path`. If the file
    /// does not exist, returns an empty store *with the path set*
    /// so subsequent [`Keystore::save`] calls write back to the
    /// expected location.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Ok(Self {
                keys: BTreeMap::new(),
                path: Some(path),
            });
        }
        let raw = std::fs::read_to_string(&path)?;
        let parsed: OnDisk = serde_json::from_str(&raw)?;
        let keys = parsed.keys.into_iter().map(|k| (k.id.clone(), k)).collect();
        Ok(Self {
            keys,
            path: Some(path),
        })
    }

    /// Persist the current store back to its on-disk path. Errors
    /// if the keystore was created without a path
    /// (`empty`/`with_keys`).
    pub fn save(&self) -> Result<()> {
        let path = self.path.as_ref().ok_or_else(|| {
            Error::Other("keystore has no on-disk path; call open(path) first".into())
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let on_disk = OnDisk {
            keys: self.keys.values().cloned().collect(),
        };
        let raw = serde_json::to_string_pretty(&on_disk)?;
        std::fs::write(path, raw)?;
        Ok(())
    }

    /// Insert or replace a trusted key. Returns the previous entry
    /// for the same id, if any.
    pub fn import_trusted(&mut self, key: TrustedKey) -> Option<TrustedKey> {
        self.keys.insert(key.id.clone(), key)
    }

    /// Remove the key matching `id`. Returns the removed entry.
    pub fn revoke(&mut self, id: &str) -> Option<TrustedKey> {
        self.keys.remove(id)
    }

    /// Look up a trusted key by id.
    pub fn get(&self, id: &str) -> Option<&TrustedKey> {
        self.keys.get(id)
    }

    /// Iterator over every trusted key, sorted by id.
    pub fn iter(&self) -> impl Iterator<Item = &TrustedKey> {
        self.keys.values()
    }

    /// Number of trusted keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// `true` if no keys are registered.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keypair::Keypair;

    fn fixture_key(id: &str) -> TrustedKey {
        TrustedKey {
            id: id.to_string(),
            public: Keypair::generate().public(),
            note: None,
        }
    }

    #[test]
    fn import_and_get_round_trip() {
        let mut store = Keystore::empty();
        let key = fixture_key("alice");
        store.import_trusted(key.clone());
        assert_eq!(store.get("alice").unwrap().public, key.public);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn save_and_open_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted.json");

        let mut store = Keystore::open(&path).unwrap();
        store.import_trusted(fixture_key("alice"));
        store.import_trusted(fixture_key("bob"));
        store.save().unwrap();

        let reopened = Keystore::open(&path).unwrap();
        assert_eq!(reopened.len(), 2);
        assert!(reopened.get("alice").is_some());
        assert!(reopened.get("bob").is_some());
    }

    #[test]
    fn revoke_removes_entry() {
        let mut store = Keystore::empty();
        store.import_trusted(fixture_key("alice"));
        let removed = store.revoke("alice").unwrap();
        assert_eq!(removed.id, "alice");
        assert!(store.is_empty());
    }

    #[test]
    fn save_without_path_errors_loudly() {
        let store = Keystore::empty();
        let err = store.save().unwrap_err();
        assert!(matches!(err, Error::Other(_)));
    }
}
