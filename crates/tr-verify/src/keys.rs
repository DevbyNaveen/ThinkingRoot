//! Trusted author key storage.
//!
//! In Phase F.1 the store is an in-memory map. A future step (post-v0.1
//! per `docs/2026-04-27-phase-f-trust-verify-design.md` §11 deferred
//! item 3) backs it with an on-disk directory at
//! `~/.config/thinkingroot/keys/` and optional Secure Enclave / TPM
//! integration.

use std::collections::HashMap;

/// One Ed25519 author key the local trust store accepts.
#[derive(Debug, Clone)]
pub struct TrustedAuthorKey {
    /// Identifier the pack's author claim is matched against. Phase F.1
    /// uses `manifest.authors[0]` as the lookup key; Step 4b extends
    /// this with explicit per-pack key-id metadata.
    pub key_id: String,
    /// Raw 32-byte Ed25519 public key.
    pub ed25519_public: [u8; 32],
}

/// In-memory store of trusted author keys.
#[derive(Debug, Default, Clone)]
pub struct AuthorKeyStore {
    by_id: HashMap<String, TrustedAuthorKey>,
}

impl AuthorKeyStore {
    /// Construct an empty store. Useful when policy demands rejecting
    /// every T1 pack until the user explicitly trusts an author.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct a store seeded with the given keys. Later
    /// `key_id`s overwrite earlier ones if duplicated.
    pub fn with_keys(keys: impl IntoIterator<Item = TrustedAuthorKey>) -> Self {
        let mut by_id = HashMap::new();
        for k in keys {
            by_id.insert(k.key_id.clone(), k);
        }
        Self { by_id }
    }

    /// Look up a key by id.
    pub fn get(&self, key_id: &str) -> Option<&TrustedAuthorKey> {
        self.by_id.get(key_id)
    }

    /// Register or replace a trusted key.
    pub fn insert(&mut self, key: TrustedAuthorKey) {
        self.by_id.insert(key.key_id.clone(), key);
    }

    /// `true` if no keys are registered.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Number of registered keys.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }
}
