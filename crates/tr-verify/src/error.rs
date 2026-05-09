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

    /// `manifest.author_key_id` is malformed (not a valid
    /// `did:method:identifier[#fragment]`). Surfaced before any
    /// network call so a malformed manifest fails fast at the
    /// boundary rather than the resolver.
    #[error("invalid author_key_id `{did}`: {reason}")]
    InvalidAuthorKey {
        /// The malformed DID string.
        did: String,
        /// Why it's malformed.
        reason: String,
    },

    /// The author's DID could not be resolved (HTTPS fetch failed,
    /// DID document malformed, no Ed25519 keys in the document, etc.).
    /// Distinct from [`crate::v3::V3Verdict::Tampered`] — this means
    /// "we cannot decide", not "we decided no".
    #[error("author DID resolution failed for `{did}`: {source}")]
    AuthorKeyResolutionFailed {
        /// The DID we tried to resolve.
        did: String,
        /// The underlying tr-identity error.
        #[source]
        source: tr_identity::Error,
    },
}

/// Convenience alias for `Result<T, crate::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
