//! Error type for the `tr-verify` crate.
//!
//! Most failures of [`crate::verify_v3_pack`] /
//! [`crate::verify_v3_pack_with_revocation`] do **not** surface as
//! `Error` — they are reported as a non-`Verified`
//! [`crate::V3Verdict`] so the caller can render a user-facing
//! message.  `Error` is reserved for the few cases where verification
//! itself cannot run at all (revocation cache I/O on the async path).

use thiserror::Error;

/// Failure modes that prevent verification from producing a verdict.
#[derive(Debug, Error)]
pub enum Error {
    /// The revocation cache could not be read or refreshed at all.
    /// Distinct from [`crate::V3Verdict::RevocationUnverifiable`],
    /// which carries a usable-but-stale cache decision.  This variant
    /// means there is nothing on disk and the network refresh also
    /// failed.
    #[error("revocation cache unavailable: {0}")]
    RevocationUnavailable(#[from] tr_revocation::Error),
}

/// Convenience alias for `Result<T, crate::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
