//! Client-side cache + verifier for the TR-1 revocation deny-list.
//!
//! This crate implements the *client* side of the TR-1 revocation
//! protocol. It fetches the signed deny-list from a registry, verifies
//! the signature against pinned keys, persists the snapshot atomically
//! to disk, and answers the hot-path question "is this content hash
//! revoked?" with a pure, non-async lookup.
//!
//! The wire protocol is locked in
//! `docs/2026-04-24-revocation-protocol-spec.md`. The OSS
//! implementation contract is in
//! `docs/2026-04-27-phase-f-trust-verify-design.md`.
//!
//! Primary entry points:
//! - [`RevocationCache::new`] — construct against a [`CacheConfig`].
//! - [`RevocationCache::load_or_refresh`] — the convenience method
//!   `root install` calls before unpacking a `.tr`.
//! - [`RevocationCache::is_revoked`] — pure lookup, called per install.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cache;
pub mod error;
pub mod keys;
pub mod snapshot;

pub use cache::{
    CacheConfig, FreshnessState, RefreshOutcome, RevocationCache, default_cache_dir,
};
pub use error::{Error, Result};
pub use keys::{PinnedKey, pinned_keys};
pub use snapshot::{Advisory, Authority, Reason, Snapshot};
