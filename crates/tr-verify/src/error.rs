//! Error type for the `tr-verify` crate.
//!
//! Most failures of [`crate::Verifier::verify`] do **not** surface as
//! `Error` — they are reported as a non-`Verified` [`crate::Verdict`]
//! so the caller can render a user-facing message. `Error` is reserved
//! for the few cases where the verifier itself cannot run at all
//! (revocation cache I/O, malformed pinned key bytes).

use thiserror::Error;

/// Failure modes that prevent [`crate::Verifier::verify`] from
/// producing a verdict.
#[derive(Debug, Error)]
pub enum Error {
    /// The revocation cache could not be read or refreshed at all.
    /// Distinct from [`crate::Verdict::StaleCache`], which is a usable
    /// but old cache. This variant means there is nothing on disk and
    /// the network refresh also failed.
    #[error("revocation cache unavailable: {0}")]
    RevocationUnavailable(#[from] tr_revocation::Error),

    /// A pinned author public key in [`crate::AuthorKeyStore`] is not
    /// 32 valid Ed25519 bytes.
    #[error("invalid trusted author key: {0}")]
    InvalidAuthorKey(String),
}

/// Convenience alias for `Result<T, crate::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
