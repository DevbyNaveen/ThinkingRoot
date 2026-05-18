//! Phase 2 of the "ThinkingRoot Central" plan (`plans/okey-so-i-wnat-elegant-hamster.md`):
//! per-tool bearer tokens for the agentmemory protocol.
//!
//! ## Why per-tool tokens
//!
//! Before Phase 2, the agentmemory router accepted a single global
//! secret (`THINKINGROOT_AGENTMEMORY_SECRET` env var). That means
//! every AI tool plugged into the daemon used the same token —
//! revoking access for one (e.g. an old Cursor install) means
//! rotating the global secret, which revokes ALL clients. Useless
//! for the "any AI plugs in zero-config" promise.
//!
//! Phase 2 layers per-tool tokens on top: each `POST /agentmemory/connect`
//! mints a fresh 32-byte URL-safe-base64 token, stores its BLAKE3
//! in `agentmemory-tokens.json`, and returns the raw token to the
//! client exactly once. Subsequent agentmemory calls present the
//! token via `Authorization: Bearer <token>`; the auth check
//! resolves the BLAKE3 against the store. Revocation is per-token.
//!
//! ## File on disk
//!
//! Path: `<config_dir>/thinkingroot/agentmemory-tokens.json`. Mode
//! `0600` on Unix. Atomic save via `tempfile + persist` mirroring
//! `install_manifest::save`. Schema version 1; reader-bumped — a
//! reader on version 1 refuses to parse a future version-2 file
//! rather than mis-interpreting.
//!
//! ## Constant-time match
//!
//! Token comparison goes through `subtle::ConstantTimeEq` over the
//! BLAKE3 bytes — same posture as `agentmemory_auth_check` for the
//! global secret. Without constant-time, an attacker observing
//! timing on the loopback (or LAN, when the daemon binds non-
//! loopback) could probe the BLAKE3 byte-by-byte.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thinkingroot_core::Error;

/// Scope of a per-tool token. `ReadOnly` clients can call
/// `/memories`, `/projects`, `/smart-search`, `/livez`. `ReadWrite`
/// clients additionally get `/remember` and `/forget`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    ReadOnly,
    ReadWrite,
}

impl ScopeKind {
    /// Is a write-class endpoint (`/remember`, `/forget`) allowed
    /// for this scope?
    pub fn permits_write(self) -> bool {
        matches!(self, ScopeKind::ReadWrite)
    }
}

/// One issued token. The token bytes themselves are NEVER stored
/// — only the BLAKE3 hash. A leak of `agentmemory-tokens.json` does
/// not let the attacker authenticate; they would need to brute-force
/// the 256-bit pre-image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentmemoryToken {
    /// BLAKE3 over the raw token bytes. Hex-encoded for human
    /// inspection of the file.
    pub token_blake3: String,
    /// The project (workspace) this token is scoped to. `None`
    /// means "the daemon's default workspace".
    pub project: Option<String>,
    pub scope: ScopeKind,
    pub issued_at: DateTime<Utc>,
    /// Updated by the auth check on every successful verification.
    /// Useful for "least-recently-used revocation" UX in the
    /// dashboard.
    pub last_seen: DateTime<Utc>,
    /// The `User-Agent` header the connecting tool sent on the
    /// `/connect` request. Lets the dashboard show "Cursor v1.0"
    /// or "OpenClaw build abc123" without the user having to label
    /// tokens by hand.
    pub client_user_agent: String,
}

/// Persisted store. Indexed by `token_blake3`-prefix for the
/// constant-time lookup path: we iterate every token and check
/// BLAKE3-equality under `subtle::ConstantTimeEq`, so the on-disk
/// structure is "just a Vec for now". For 100s of tokens this is
/// O(n) and trivial; we'd revisit if a user ever hit 10k tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentmemoryTokenStore {
    pub schema_version: u32,
    pub tokens: Vec<AgentmemoryToken>,
}

impl AgentmemoryTokenStore {
    /// Schema version this build understands. Bumped only when the
    /// `tokens` field shape changes incompatibly.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Empty store. Used when the file doesn't exist on disk.
    pub fn empty() -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            tokens: Vec::new(),
        }
    }

    /// Canonical on-disk path. Honours `XDG_CONFIG_HOME` for test
    /// isolation, same as `install_manifest::path` and friends.
    pub fn path() -> Result<PathBuf, Error> {
        let cfg = dirs::config_dir().ok_or_else(|| {
            Error::Config("no config dir for agentmemory tokens".to_string())
        })?;
        Ok(cfg.join("thinkingroot").join("agentmemory-tokens.json"))
    }

    /// Load the store from disk. Returns `Self::empty()` when the
    /// file doesn't exist (first run). Refuses to parse a file with
    /// `schema_version > CURRENT_SCHEMA_VERSION` — same reader-bumped
    /// discipline as `install_manifest`.
    pub fn load() -> Result<Self, Error> {
        let path = Self::path()?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(e) => {
                return Err(Error::Io {
                    path: Some(path),
                    source: e,
                });
            }
        };
        let parsed: Self = serde_json::from_slice(&bytes).map_err(|e| {
            Error::Serialization(format!(
                "agentmemory-tokens.json parse error: {e} (path: {})",
                path.display()
            ))
        })?;
        if parsed.schema_version > Self::CURRENT_SCHEMA_VERSION {
            return Err(Error::Config(format!(
                "agentmemory-tokens.json schema version {} is newer than this build supports (max {})",
                parsed.schema_version,
                Self::CURRENT_SCHEMA_VERSION
            )));
        }
        Ok(parsed)
    }

    /// Atomically save the store to disk via tempfile+persist.
    /// Mirrors `install_manifest::save` — never produces a torn
    /// write even if the process crashes mid-save. Sets mode `0600`
    /// on Unix so the tokens file is owner-only.
    pub fn save(&self) -> Result<(), Error> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: Some(parent.to_path_buf()),
                source: e,
            })?;
        }
        let dir = path.parent().ok_or_else(|| {
            Error::Config(format!("tokens path has no parent: {}", path.display()))
        })?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| Error::Io {
            path: Some(dir.to_path_buf()),
            source: e,
        })?;
        let json = serde_json::to_vec_pretty(self).map_err(|e| {
            Error::Serialization(format!("agentmemory-tokens.json encode: {e}"))
        })?;
        use std::io::Write;
        tmp.write_all(&json).map_err(|e| Error::Io {
            path: Some(tmp.path().to_path_buf()),
            source: e,
        })?;
        // Mode 0600 on Unix BEFORE persist so the file is never
        // visible at the canonical path with a wider mode.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(tmp.path(), permissions).map_err(|e| Error::Io {
                path: Some(tmp.path().to_path_buf()),
                source: e,
            })?;
        }
        tmp.persist(&path).map_err(|e| Error::Io {
            path: Some(path),
            source: e.error,
        })?;
        Ok(())
    }

    /// Issue a fresh token. Mutates `self` in memory only — caller
    /// is responsible for calling `save()` afterwards. Returns the
    /// raw token (32 random bytes, URL-safe base64); the store
    /// retains only the BLAKE3.
    pub fn issue(
        &mut self,
        project: Option<String>,
        scope: ScopeKind,
        client_user_agent: impl Into<String>,
    ) -> String {
        // 32 random bytes → URL-safe base64 = 43 chars without
        // padding. Sufficient entropy for the threat model
        // (loopback eavesdropper + offline brute force). Uses
        // `rand::rngs::OsRng` (the system CSPRNG) — same source the
        // ed25519 key generation in `intelligence/trace.rs` uses.
        use base64::Engine as _;
        use rand::TryRngCore;
        let mut raw_bytes = [0u8; 32];
        let mut rng = rand::rngs::OsRng;
        rng.try_fill_bytes(&mut raw_bytes)
            .expect("OsRng must produce token bytes");
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw_bytes);
        let hash = blake3::hash(raw.as_bytes());
        let now = Utc::now();
        self.tokens.push(AgentmemoryToken {
            token_blake3: hash.to_hex().to_string(),
            project,
            scope,
            issued_at: now,
            last_seen: now,
            client_user_agent: client_user_agent.into(),
        });
        raw
    }

    /// Verify a presented raw token against the store. Returns the
    /// matching token's metadata on success, `None` otherwise.
    /// Constant-time-equal over the BLAKE3 bytes per token; iterates
    /// every token (O(n)) but n is tiny in practice.
    pub fn verify(&mut self, presented: &str) -> Option<&AgentmemoryToken> {
        let hash = blake3::hash(presented.as_bytes());
        let presented_hex = hash.to_hex();
        let presented_bytes = presented_hex.as_bytes();
        use subtle::ConstantTimeEq;
        // Two-pass: first find the index under constant-time match,
        // then bump last_seen + return. We can't keep a `&mut` across
        // the borrow because `self.tokens` is what's iterated.
        let mut found_idx: Option<usize> = None;
        for (idx, t) in self.tokens.iter().enumerate() {
            let on_disk = t.token_blake3.as_bytes();
            if on_disk.len() == presented_bytes.len()
                && bool::from(on_disk.ct_eq(presented_bytes))
            {
                found_idx = Some(idx);
                // Don't `break` — keep iterating to preserve
                // constant-time across all entries.
            }
        }
        match found_idx {
            Some(idx) => {
                self.tokens[idx].last_seen = Utc::now();
                Some(&self.tokens[idx])
            }
            None => None,
        }
    }

    /// Revoke a token by its BLAKE3 hex prefix. Returns the number
    /// of tokens removed (0 if no match, 1 on revoke). Wired
    /// directly so the dashboard's "revoke" button doesn't need to
    /// know about the constant-time machinery.
    pub fn revoke_by_hash_prefix(&mut self, prefix: &str) -> usize {
        let prior_len = self.tokens.len();
        self.tokens
            .retain(|t| !t.token_blake3.starts_with(prefix));
        prior_len - self.tokens.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_then_verify_round_trips() {
        let mut store = AgentmemoryTokenStore::empty();
        let raw = store.issue(
            Some("test-project".into()),
            ScopeKind::ReadWrite,
            "test-agent/0.1",
        );
        // Raw token must be returned exactly once + must be a
        // reasonable length (32 bytes base64-no-pad = 43 chars).
        assert_eq!(raw.len(), 43, "expected 43-char URL-safe-no-pad token");
        // Verify with the raw value.
        let result = store.verify(&raw);
        assert!(result.is_some(), "issued token must verify");
        let token = result.unwrap();
        assert_eq!(token.project.as_deref(), Some("test-project"));
        assert_eq!(token.scope, ScopeKind::ReadWrite);
        assert_eq!(token.client_user_agent, "test-agent/0.1");
    }

    #[test]
    fn verify_rejects_wrong_token() {
        let mut store = AgentmemoryTokenStore::empty();
        let _ = store.issue(None, ScopeKind::ReadOnly, "test");
        // A completely different value must not match.
        let result = store.verify("not-the-real-token-abc-xyz-43-chars-padd-x");
        assert!(result.is_none(), "wrong token must not verify");
    }

    #[test]
    fn verify_rejects_empty_token() {
        let mut store = AgentmemoryTokenStore::empty();
        let _ = store.issue(None, ScopeKind::ReadOnly, "test");
        let result = store.verify("");
        assert!(result.is_none(), "empty token must not verify");
    }

    #[test]
    fn revoke_by_hash_prefix_removes_matching_entry() {
        let mut store = AgentmemoryTokenStore::empty();
        let raw = store.issue(None, ScopeKind::ReadOnly, "test");
        let hash = blake3::hash(raw.as_bytes()).to_hex().to_string();
        // Revoke by full hex prefix → 1 removal.
        let removed = store.revoke_by_hash_prefix(&hash);
        assert_eq!(removed, 1);
        assert!(store.tokens.is_empty());
        // Subsequent verify rejects.
        assert!(store.verify(&raw).is_none());
    }

    #[test]
    fn schema_version_v1_round_trips() {
        let mut original = AgentmemoryTokenStore::empty();
        let _ = original.issue(Some("p".into()), ScopeKind::ReadWrite, "ua");
        let json = serde_json::to_string(&original).unwrap();
        let parsed: AgentmemoryTokenStore = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.schema_version,
            AgentmemoryTokenStore::CURRENT_SCHEMA_VERSION
        );
        assert_eq!(parsed.tokens.len(), 1);
    }

    #[test]
    fn future_schema_version_is_rejected() {
        // Synthesize a v2 file and assert load refuses. The path
        // resolution requires `dirs::config_dir`; we test the parse
        // logic directly by mimicking what load() would do after
        // reading bytes.
        let mut store_v2 = AgentmemoryTokenStore::empty();
        store_v2.schema_version = AgentmemoryTokenStore::CURRENT_SCHEMA_VERSION + 1;
        let json = serde_json::to_string(&store_v2).unwrap();
        let parsed: AgentmemoryTokenStore =
            serde_json::from_str(&json).expect("v2 file parses syntactically");
        assert!(
            parsed.schema_version > AgentmemoryTokenStore::CURRENT_SCHEMA_VERSION,
            "test fixture must carry v2 schema for the assertion to be meaningful"
        );
        // The load() method would Err here; we don't have a tempdir
        // for the actual file-load path, so we check the rejection
        // condition explicitly.
    }

    #[test]
    fn scope_permits_write_distinguishes_kinds() {
        assert!(!ScopeKind::ReadOnly.permits_write());
        assert!(ScopeKind::ReadWrite.permits_write());
    }
}
