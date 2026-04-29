//! Public trust-verification API for `.tr` packs.
//!
//! Composes [`tr_format`], [`tr_revocation`], and Ed25519 primitives
//! into a single [`Verifier::verify`] call. Used by `root install` and
//! the desktop install sheet to decide whether a pack is safe to mount.
//!
//! See `docs/2026-04-27-phase-f-trust-verify-design.md` for the full
//! contract this crate implements.
//!
//! Phase F.1 (this crate) ships verification for [`TrustTier::T0`] and
//! [`TrustTier::T1`] (author-key Ed25519). Sigstore-keyless verification
//! for T2+ arrives in Step 4b — see the `Verdict::Unsupported` variant.
//!
//! [`TrustTier::T0`]: tr_format::TrustTier::T0
//! [`TrustTier::T1`]: tr_format::TrustTier::T1
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use tr_format::TrustTier;
//! use tr_revocation::{CacheConfig, RevocationCache};
//! use tr_verify::{AuthorKeyStore, Verifier, VerifierConfig};
//!
//! # async fn run(pack: &tr_format::reader::Pack) {
//! let cache = Arc::new(RevocationCache::new(
//!     CacheConfig::defaults_for(
//!         "https://hub.thinkingroot.dev".parse().unwrap(),
//!         tr_revocation::default_cache_dir().unwrap(),
//!     ),
//! ));
//! let keys = Arc::new(AuthorKeyStore::empty());
//!
//! let verifier = Verifier::new(VerifierConfig {
//!     revocation: cache,
//!     author_keys: keys,
//!     require_min_tier: TrustTier::T1,
//!     allow_unsigned: false,
//! });
//!
//! let verdict = verifier.verify(pack).await;
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod keys;
pub mod v3;
pub mod verdict;
pub mod verifier;

pub use error::{Error, Result};
pub use keys::{AuthorKeyStore, TrustedAuthorKey};
pub use v3::{V3TamperedKind, V3Verdict, verify_v3_pack};
pub use verdict::{RevokedDetails, TamperedKind, Verdict, VerifiedDetails};
pub use verifier::{Verifier, VerifierConfig};
