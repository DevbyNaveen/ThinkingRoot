//! Error type for the `tr-format` crate.

use thiserror::Error;

/// Errors that can arise while reading, validating, or writing a `.tr`
/// pack. Every variant carries enough context that callers can render a
/// reasonable error message without further lookups.
#[derive(Debug, Error)]
pub enum Error {
    /// A required component is missing from the archive.
    #[error("missing required entry: {0}")]
    Missing(&'static str),

    /// An entry is present but doesn't parse.
    #[error("invalid {what}: {detail}")]
    Invalid {
        /// Which component (e.g. `"manifest.json"`).
        what: &'static str,
        /// Parser-specific detail.
        detail: String,
    },

    /// An invariant that spans multiple components failed — e.g. the
    /// manifest's `content_hash` doesn't match the recomputed hash of
    /// the archive.
    #[error("inconsistent pack: {0}")]
    Inconsistent(String),

    /// An I/O failure reading or writing bytes.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON serialisation / deserialisation failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// A semver parse failure.
    #[error("semver: {0}")]
    SemVer(#[from] semver::Error),

    /// Archive is larger than we're willing to load into memory.
    #[error("pack exceeds size cap: {actual} bytes > {cap} bytes")]
    TooLarge {
        /// The cap we were configured with.
        cap: u64,
        /// The observed size.
        actual: u64,
    },

    /// An entry path inside the archive would escape the pack root.
    /// This is the Zip-Slip defence.
    #[error("unsafe entry path: {0}")]
    UnsafePath(String),
}

/// Convenience alias for `Result<T, crate::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
