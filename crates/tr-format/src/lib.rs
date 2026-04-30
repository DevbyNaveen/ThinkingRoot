//! `tr-format` — reader/writer for the v3 `.tr` portable knowledge
//! pack format.
//!
//! A `.tr` file is an outer `tar` archive (uncompressed) containing
//! exactly three (or four, when signed) entries per the v3 spec §3.1:
//!
//! ```text
//! manifest.toml         — canonical TR-3 manifest (this crate's ManifestV3)
//! source.tar.zst        — inner tar+zstd of the source files
//! claims.jsonl          — JSONL stream of claim records, byte-range citations
//! signature.sig         — optional Sigstore Bundle v0.3 (when --sign or --sign-keyless was used)
//! ```
//!
//! This crate does **not** execute anything from a `.tr`. Mount/execute
//! is the responsibility of the `root` CLI. Here we only parse, verify,
//! and (re)assemble the container.
//!
//! Primary entry points:
//! - [`ManifestV3`] — the structural contract.
//! - [`read_v3_pack`] — open and parse a v3 pack.
//! - [`V3PackBuilder`] — assemble a new pack programmatically. Three
//!   build methods: [`V3PackBuilder::build`] (unsigned),
//!   [`V3PackBuilder::build_signed`] (Ed25519 self-signed), and
//!   [`V3PackBuilder::build_with_signer`] (caller-supplied
//!   `SigstoreBundle`, e.g. for Sigstore-keyless DSSE).
//! - [`digest::blake3_hex`] — canonical BLAKE3 helper used by the
//!   pack-hash recipe (spec §3.1).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod claims;
pub mod digest;
pub mod error;
pub mod manifest;
pub mod reader_v3;
pub mod writer_v3;

pub use claims::ClaimRecord;
pub use error::Error;
pub use manifest::{FORMAT_VERSION_V3, ManifestV3};
pub use reader_v3::{V3Pack, read_v3_pack};
pub use writer_v3::V3PackBuilder;

// Re-export so consumers don't need a direct `semver` dep just to
// parse a pack version — `semver::Version` is on the public surface
// of `ManifestV3`.
pub use semver::Version;
