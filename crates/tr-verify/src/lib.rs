//! Public trust-verification API for v3 `.tr` packs.
//!
//! Composes [`tr_format`], [`tr_revocation`], and [`tr_sigstore`]
//! into the v3 verification pipeline. Used by `root verify` and the
//! `root install` extraction path to decide whether a pack is safe
//! to mount.
//!
//! See `docs/2026-04-29-phase-f-trust-verify-spec.md` for the
//! full contract this crate implements.
//!
//! # Example
//!
//! ```no_run
//! use tr_format::read_v3_pack;
//! use tr_revocation::{CacheConfig, RevocationCache};
//!
//! # async fn run(bytes: &[u8]) {
//! let pack = read_v3_pack(bytes).expect("parse v3 pack");
//! let cache = RevocationCache::new(CacheConfig::defaults_for(
//!     "https://hub.thinkingroot.dev".parse().unwrap(),
//!     tr_revocation::default_cache_dir().unwrap(),
//! ));
//! let verdict = tr_verify::verify_v3_pack_with_revocation(&pack, &cache).await;
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod v3;
pub mod verdict;

pub use error::{Error, Result};
pub use v3::{V3TamperedKind, V3Verdict, verify_v3_pack, verify_v3_pack_with_revocation};
pub use verdict::RevokedDetails;
