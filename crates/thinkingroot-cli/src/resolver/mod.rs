//! Pluggable resolvers for `root install`.
//!
//! Each [`PackResolver`] knows how to fetch the raw bytes of one
//! `.tr` file from a specific source — the local filesystem, a direct
//! HTTPS URL, or a cloud registry coordinate. Future backends (OCI,
//! S3-mirror, IPFS) slot in by implementing the trait without
//! touching `pack_cmd::run_install`.
//!
//! Construction is the responsibility of the caller — `pack_cmd`
//! parses the user's pack reference into an [`crate::pack_cmd::InstallRef`]
//! and dispatches to the matching resolver via the
//! [`crate::pack_cmd::build_resolver`] helper. The trait itself is
//! state-bearing (`&self`) so each resolver carries the ref it
//! resolves.

use anyhow::Result;
use async_trait::async_trait;

pub mod http;
pub mod local;

pub use http::{HttpDirectUrlResolver, HttpRegistryResolver};
pub use local::LocalFsResolver;

/// Backend-agnostic source for `.tr` archive bytes.
///
/// Implementations are responsible for any source-specific integrity
/// check (e.g. BLAKE3 cross-check against a registry-advertised
/// header) before returning. Trust verification — Sigstore signatures,
/// revocation deny-lists — happens *after* this layer in
/// [`crate::pack_cmd::install_from_bytes_with_verifier`].
#[async_trait]
pub trait PackResolver: Send + Sync {
    /// Fetch the raw bytes of the `.tr` file this resolver was
    /// configured for.
    async fn resolve(&self) -> Result<Vec<u8>>;
}
