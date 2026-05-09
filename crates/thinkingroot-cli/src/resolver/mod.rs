//! Pluggable resolvers for `root install`.
//!
//! Each [`PackResolver`] knows how to fetch the raw bytes of one
//! `.tr` file from a specific source — the local filesystem, a direct
//! HTTPS URL, or a cloud registry coordinate. Future backends (OCI,
//! S3-mirror, IPFS) slot in by implementing the trait without
//! touching `pack_cmd::run_install`.
//!
//! The trait itself lives in `thinkingroot-core` so cloud services
//! and the desktop can implement custom backends without depending on
//! this CLI binary crate. We re-export it here so existing callers
//! (`use crate::resolver::PackResolver`) keep working unchanged.
//!
//! Construction is the responsibility of the caller — `pack_cmd`
//! parses the user's pack reference into an [`crate::pack_cmd::InstallRef`]
//! and dispatches to the matching resolver via the
//! [`crate::pack_cmd::build_resolver`] helper. The trait itself is
//! state-bearing (`&self`) so each resolver carries the ref it
//! resolves.
//!
//! `anyhow::Error` consumes [`ResolverError`] automatically through
//! its blanket `From<E: Error>` impl, so call sites in `pack_cmd` can
//! continue to use `?` unchanged.

pub mod http;
pub mod local;

pub use http::{HttpDirectUrlResolver, HttpRegistryResolver};
pub use local::LocalFsResolver;

// Re-exported from core so existing imports
// (`use crate::resolver::PackResolver`) keep working unchanged.
// `ResolverDescriptor` / `ResolverError` are part of the trait's
// surface contract — surfaced here even though the binary itself
// doesn't currently call them, so external integrators (cloud, the
// future custom-resolver plugin path) can `use crate::resolver::*`
// without reaching across the workspace boundary.
#[allow(unused_imports)]
pub use thinkingroot_core::resolver::{PackResolver, ResolverDescriptor, ResolverError};
