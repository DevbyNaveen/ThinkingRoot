//! Persistent revocation snapshot cache + freshness logic + signature
//! verification + the hot-path lookup.
//!
//! The cache owns four files inside its configured directory:
//!
//! ```text
//! <cache_dir>/
//! ├── snapshot.json        canonical signed deny-list
//! ├── snapshot.json.tmp    write target; renamed atomically
//! ├── snapshot.etag        last-seen ETag for conditional GETs
//! └── snapshot.fetched_at  unix epoch seconds of last successful fetch
//! ```
//!
//! Atomic replacement uses the OS `rename` syscall, which is atomic on
//! POSIX and uses replace semantics on Windows.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use reqwest::header::{ETAG, IF_NONE_MATCH};

use crate::error::{Error, Result};
use crate::keys::PinnedKey;
use crate::snapshot::{Advisory, Snapshot};

/// Configuration for a [`RevocationCache`].
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Base URL of the registry serving `/api/v1/revoked`. Trailing
    /// slash optional — the cache appends the path component.
    pub registry_url: url::Url,
    /// Directory the cache owns. Created lazily on first
    /// [`RevocationCache::refresh`]; not touched at construction.
    pub cache_dir: PathBuf,
    /// Maximum age before [`FreshnessState::Stale`] fires.
    pub fresh_ttl: Duration,
    /// Maximum age before [`FreshnessState::Expired`] fires; beyond
    /// this the caller should refuse to install per
    /// `phase-f-trust-verify-design.md` §3.
    pub stale_grace: Duration,
    /// Pinned keys the client will accept signatures from.
    pub trusted_keys: Vec<PinnedKey>,
    /// Hard cap on response body size. Default 50 MB matches
    /// `revocation-protocol-spec.md` §5.3.
    pub max_snapshot_bytes: u64,
}

impl CacheConfig {
    /// Production defaults: 60-min fresh window, 7-day stale grace,
    /// 50 MB cap, [`crate::pinned_keys`] as the trust anchor.
    pub fn defaults_for(registry_url: url::Url, cache_dir: PathBuf) -> Self {
        Self {
            registry_url,
            cache_dir,
            fresh_ttl: Duration::from_secs(60 * 60),
            stale_grace: Duration::from_secs(7 * 24 * 60 * 60),
            trusted_keys: crate::keys::pinned_keys(),
            max_snapshot_bytes: 50 * 1024 * 1024,
        }
    }
}

/// Freshness of the on-disk snapshot, computed from
/// `snapshot.fetched_at` against `SystemTime::now()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessState {
    /// Within `fresh_ttl` of the last successful fetch.
    Fresh,
    /// Past `fresh_ttl` but within `stale_grace`. Caller should warn
    /// loudly and attempt a refresh; usable in the meantime.
    Stale,
    /// Past `stale_grace`. Caller should refuse to install.
    Expired,
    /// No snapshot has ever been written.
    Missing,
}

/// Outcome of a [`RevocationCache::refresh`] call.
#[derive(Debug, Clone)]
pub enum RefreshOutcome {
    /// A fresh snapshot was downloaded and persisted.
    Updated {
        /// Number of advisories in the new snapshot.
        added: usize,
    },
    /// Registry returned 304 Not Modified; freshness timestamp bumped.
    NotModified,
    /// Network call failed but the caller has a usable cached snapshot.
    /// Returned by [`RevocationCache::load_or_refresh`] only.
    Offline,
}

/// The default cache directory for the current platform.
///
/// Matches `revocation-protocol-spec.md` §5.3:
///
/// - Linux:   `~/.cache/thinkingroot/revocation/`
/// - macOS:   `~/Library/Caches/thinkingroot/revocation/`
/// - Windows: `%LOCALAPPDATA%\thinkingroot\revocation\`
///
/// Returns `None` only when `dirs::cache_dir()` itself fails — which
/// in practice means an unrecognised platform.
pub fn default_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|p| p.join("thinkingroot").join("revocation"))
}

/// Persistent cache + verifier for revocation snapshots.
pub struct RevocationCache {
    config: CacheConfig,
    http: reqwest::Client,
}

impl RevocationCache {
    /// Build a cache against the given config. Does not touch disk or
    /// the network — first I/O happens on [`Self::load`] or
    /// [`Self::refresh`].
    pub fn new(config: CacheConfig) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("tr-revocation/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client construction with rustls + json features");
        Self { config, http }
    }

    /// Read-only view of the configuration this cache was built with.
    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    /// Read the cached snapshot from disk and classify its freshness.
    ///
    /// Returns `(None, FreshnessState::Missing)` when no snapshot has
    /// ever been written. Otherwise returns the parsed snapshot and a
    /// freshness label derived from `snapshot.fetched_at`.
    pub fn load(&self) -> Result<(Option<Snapshot>, FreshnessState)> {
        let snap_path = self.snapshot_path();
        if !snap_path.exists() {
            return Ok((None, FreshnessState::Missing));
        }
        let bytes = std::fs::read(&snap_path)?;
        if bytes.len() as u64 > self.config.max_snapshot_bytes {
            return Err(Error::TooLarge {
                cap: self.config.max_snapshot_bytes,
                actual: bytes.len() as u64,
            });
        }
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        let fetched_at = self.read_fetched_at().unwrap_or(0);
        let now = unix_now()?;
        let age = now.saturating_sub(fetched_at);
        let state = classify_age(age, &self.config);
        Ok((Some(snapshot), state))
    }

    /// Force a refresh from the registry. Persists atomically on
    /// success. Honors `If-None-Match` against the cached ETag and
    /// returns [`RefreshOutcome::NotModified`] for a 304 response.
    pub async fn refresh(&self) -> Result<RefreshOutcome> {
        std::fs::create_dir_all(&self.config.cache_dir)?;
        let url = self
            .config
            .registry_url
            .join("/api/v1/revoked")
            .map_err(|e| Error::Network(format!("registry url join failed: {e}")))?;

        let mut req = self.http.get(url);
        if let Some(etag) = self.read_etag() {
            req = req.header(IF_NONE_MATCH, etag);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            self.write_fetched_at(unix_now()?)?;
            return Ok(RefreshOutcome::NotModified);
        }
        if !resp.status().is_success() {
            return Err(Error::Network(format!(
                "registry returned {}",
                resp.status()
            )));
        }

        if let Some(len) = resp.content_length() {
            if len > self.config.max_snapshot_bytes {
                return Err(Error::TooLarge {
                    cap: self.config.max_snapshot_bytes,
                    actual: len,
                });
            }
        }

        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        if bytes.len() as u64 > self.config.max_snapshot_bytes {
            return Err(Error::TooLarge {
                cap: self.config.max_snapshot_bytes,
                actual: bytes.len() as u64,
            });
        }

        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        self.verify_signature(&snapshot)?;
        let added = snapshot.entries.len();

        self.persist_atomically(&bytes, etag.as_deref())?;
        self.write_fetched_at(unix_now()?)?;

        tracing::info!(
            added,
            key_id = %snapshot.signing_key_id,
            "revocation snapshot refreshed"
        );

        Ok(RefreshOutcome::Updated { added })
    }

    /// Return the snapshot to use for an install decision.
    ///
    /// If the cached snapshot is [`FreshnessState::Fresh`], return it
    /// without I/O. Otherwise attempt a refresh; on success re-load.
    /// On network failure, fall back to the cached snapshot if any —
    /// the caller inspects [`FreshnessState`] to decide whether to
    /// proceed.
    ///
    /// Returns an error only when there is no usable snapshot at all
    /// (no cache and refresh failed).
    pub async fn load_or_refresh(&self) -> Result<(Snapshot, FreshnessState)> {
        let (cached, state) = self.load()?;

        if matches!(state, FreshnessState::Fresh) {
            // Fresh implies Some by construction of `load`.
            return Ok((cached.expect("Fresh implies cached snapshot"), state));
        }

        match self.refresh().await {
            Ok(_) => {
                let (snap, new_state) = self.load()?;
                let snap = snap.ok_or_else(|| {
                    Error::Network("registry refresh succeeded but cache load returned None".into())
                })?;
                Ok((snap, new_state))
            }
            Err(network_err) => match cached {
                Some(snap) => {
                    tracing::warn!(
                        error = %network_err,
                        "revocation refresh failed; using cached snapshot"
                    );
                    Ok((snap, state))
                }
                None => {
                    // First-boot grace per `revocation-protocol-spec.md`
                    // §5.4: a brand-new client that cannot reach the
                    // registry yet proceeds with an empty deny-list.
                    // Freshness stays `Missing` so the verifier surface
                    // can warn in the user-facing log; a follow-up step
                    // (Step 5b) tracks an install-time marker file to
                    // refuse after 7 days of never-fetched.
                    tracing::warn!(
                        error = %network_err,
                        "no cached revocation snapshot and registry is unreachable; proceeding with empty deny-list (first-boot grace)"
                    );
                    Ok((empty_snapshot(), FreshnessState::Missing))
                }
            },
        }
    }

    /// Pure lookup. Linear scan over `snapshot.entries`.
    ///
    /// The `content_hash` argument may be passed with or without the
    /// `"blake3:"` prefix; both forms match.
    pub fn is_revoked<'a>(snapshot: &'a Snapshot, content_hash: &str) -> Option<&'a Advisory> {
        let needle = normalize_hash(content_hash);
        snapshot
            .entries
            .iter()
            .find(|a| normalize_hash(&a.content_hash) == needle)
    }

    /// Verify that the snapshot's signature is valid for one of the
    /// pinned keys in [`CacheConfig::trusted_keys`].
    ///
    /// Public so callers that obtained a snapshot out-of-band (e.g.
    /// the cloud team's CI mirroring it into a test fixture) can
    /// re-verify before trusting it.
    pub fn verify_signature(&self, snapshot: &Snapshot) -> Result<()> {
        let key = self
            .config
            .trusted_keys
            .iter()
            .find(|k| k.key_id == snapshot.signing_key_id)
            .ok_or_else(|| Error::KeyUnknown(snapshot.signing_key_id.clone()))?;

        let verifying =
            VerifyingKey::from_bytes(&key.ed25519_public).map_err(|_| Error::BadSignature)?;

        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(&snapshot.signature)
            .map_err(|_| Error::BadSignature)?;
        let signature = Signature::from_slice(&sig_bytes).map_err(|_| Error::BadSignature)?;

        let payload = snapshot.canonical_bytes_for_signing()?;
        verifying
            .verify(&payload, &signature)
            .map_err(|_| Error::BadSignature)
    }

    fn snapshot_path(&self) -> PathBuf {
        self.config.cache_dir.join("snapshot.json")
    }

    fn etag_path(&self) -> PathBuf {
        self.config.cache_dir.join("snapshot.etag")
    }

    fn fetched_at_path(&self) -> PathBuf {
        self.config.cache_dir.join("snapshot.fetched_at")
    }

    fn persist_atomically(&self, bytes: &[u8], etag: Option<&str>) -> Result<()> {
        std::fs::create_dir_all(&self.config.cache_dir)?;
        let final_path = self.snapshot_path();
        let tmp_path = self.config.cache_dir.join("snapshot.json.tmp");
        std::fs::write(&tmp_path, bytes)?;
        rename_atomic(&tmp_path, &final_path)?;
        match etag {
            Some(e) => std::fs::write(self.etag_path(), e)?,
            None => {
                let _ = std::fs::remove_file(self.etag_path());
            }
        }
        Ok(())
    }

    fn read_etag(&self) -> Option<String> {
        std::fs::read_to_string(self.etag_path()).ok()
    }

    fn read_fetched_at(&self) -> Option<u64> {
        std::fs::read_to_string(self.fetched_at_path())
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    fn write_fetched_at(&self, ts: u64) -> Result<()> {
        std::fs::write(self.fetched_at_path(), ts.to_string())?;
        Ok(())
    }
}

fn classify_age(age_secs: u64, config: &CacheConfig) -> FreshnessState {
    if age_secs <= config.fresh_ttl.as_secs() {
        FreshnessState::Fresh
    } else if age_secs <= config.stale_grace.as_secs() {
        FreshnessState::Stale
    } else {
        FreshnessState::Expired
    }
}

fn unix_now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| Error::ClockSkew)
}

fn normalize_hash(s: &str) -> &str {
    s.strip_prefix("blake3:").unwrap_or(s)
}

fn rename_atomic(from: &Path, to: &Path) -> std::io::Result<()> {
    // POSIX `rename(2)` is atomic for files; Windows `MoveFileExW` with
    // replace semantics behaves the same. `std::fs::rename` covers both.
    std::fs::rename(from, to)
}

fn empty_snapshot() -> Snapshot {
    Snapshot {
        schema_version: "1.0.0".into(),
        generated_at: 0,
        generated_by: "<empty-first-boot-grace>".into(),
        full_list: false,
        entries: Vec::new(),
        signature: String::new(),
        signing_key_id: String::new(),
        next_poll_hint_sec: 0,
    }
}
