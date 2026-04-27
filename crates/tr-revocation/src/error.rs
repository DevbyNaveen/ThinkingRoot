//! Error type for the `tr-revocation` crate.

use thiserror::Error;

/// Failure modes of the revocation cache.
///
/// Variants distinguish the three classes of failure the caller cares
/// about: transport problems (`Network`), trust problems (`BadSignature`,
/// `KeyUnknown`), and local-state problems (`Io`, `BadJson`, `TooLarge`,
/// `ClockSkew`). The `root install` flow maps each to a distinct exit
/// code.
#[derive(Debug, Error)]
pub enum Error {
    /// The HTTP request to the registry failed before we could parse a
    /// response (DNS, TLS, timeout, non-2xx, …).
    #[error("network: {0}")]
    Network(String),

    /// The snapshot's Ed25519 signature did not verify against the
    /// pinned key it claims to be signed by.
    #[error("revocation snapshot signature verification failed")]
    BadSignature,

    /// The snapshot is signed by a `signing_key_id` we do not have a
    /// pinned public key for. Almost always means the binary is too old
    /// for the current rotation window — upgrade `root`.
    #[error("revocation snapshot signed by unknown key id: {0}")]
    KeyUnknown(String),

    /// A local filesystem read or write failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The snapshot bytes did not parse as our `Snapshot` schema.
    #[error("json: {0}")]
    BadJson(#[from] serde_json::Error),

    /// The snapshot exceeds the configured size cap. Default cap is
    /// 50 MB per `revocation-protocol-spec.md` §5.3.
    #[error("snapshot exceeds size cap: {actual} bytes > {cap} bytes")]
    TooLarge {
        /// The cap we were configured with.
        cap: u64,
        /// The observed size.
        actual: u64,
    },

    /// `SystemTime::now()` returned a value before the Unix epoch —
    /// almost always a misconfigured clock on first boot of a VM.
    #[error("system time is before the Unix epoch")]
    ClockSkew,
}

/// Convenience alias for `Result<T, crate::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
