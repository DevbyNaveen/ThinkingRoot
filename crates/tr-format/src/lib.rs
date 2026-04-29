//! `tr-format` — reader/writer for the TR-1 `.tr` portable knowledge
//! pack format.
//!
//! A `.tr` file is a `tar` archive compressed with `zstd` (v0.1) that
//! bundles a well-known directory layout:
//!
//! ```text
//! manifest.json         — canonical TR-1 manifest (this crate's Manifest)
//! graph/                — CozoDB export: triples + meta
//! vectors/              — embedding matrices (MRL + BBQ)
//! artifacts/            — rendered documents (knowledge.card.md, …)
//! provenance/           — per-claim source-byte references
//! signatures/           — Sigstore / cosign attestations (T2+)
//! .mcpb/                — optional MCP bundle payload (dual identity)
//! ```
//!
//! This crate does **not** execute anything from a `.tr`. Mount/execute
//! is the responsibility of the `root` CLI. Here we only parse, verify,
//! and (re)assemble the container.
//!
//! Primary entry points:
//! - [`Manifest`] — the structural contract.
//! - [`reader::read_bytes`] / [`reader::read_file`] — open and parse.
//! - [`writer::PackBuilder`] — assemble a new pack programmatically.
//! - [`digest::blake3_hex`] — canonical BLAKE3 helper used for
//!   content hashes and revocation lookups.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capabilities;
pub mod claims;
pub mod digest;
pub mod error;
pub mod manifest;
pub mod reader;
pub mod writer;
pub mod writer_v3;

pub use claims::ClaimRecord;
pub use error::Error;
pub use manifest::{FORMAT_VERSION_V3, Manifest, ManifestV3, TrustTier};
pub use writer_v3::V3PackBuilder;

// Re-export so consumers don't need a direct `semver` dep just to
// parse a pack version — `semver::Version` is already on the public
// surface of `Manifest`.
pub use semver::Version;
