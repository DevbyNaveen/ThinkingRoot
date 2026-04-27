//! Single error enum surfaced by every public `tr-identity` call.

use std::io;

/// Errors surfaced by `tr-identity`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Ed25519 signature verification failed or signature byte length
    /// was wrong.
    #[error("ed25519 signature verification failed: {0}")]
    Signature(String),

    /// I/O failure reading or writing keystore files.
    #[error("io: {0}")]
    Io(#[from] io::Error),

    /// JSON encode/decode failure for keystore files.
    #[error("serde_json: {0}")]
    Json(#[from] serde_json::Error),

    /// Base64 decode failure.
    #[error("base64 decode: {0}")]
    Base64(#[from] base64::DecodeError),

    /// `did:` URI parse failure.
    #[error("invalid DID: {0}")]
    InvalidDid(String),

    /// HTTP fetch failure during `did:web:` resolution.
    #[error("did:web fetch: {0}")]
    DidWebFetch(String),

    /// The key length was wrong (Ed25519 expects exactly 32 bytes).
    #[error("invalid key length: expected {expected}, got {actual}")]
    InvalidKeyLength {
        /// Expected length.
        expected: usize,
        /// Length we observed.
        actual: usize,
    },

    /// Generic error so callers do not have to enumerate every
    /// upstream failure.
    #[error("tr-identity: {0}")]
    Other(String),
}

/// `tr-identity`'s `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
