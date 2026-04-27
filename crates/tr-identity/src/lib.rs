//! `tr-identity` — Ed25519 keypair lifecycle, on-disk keystore, and
//! DID method resolution.
//!
//! Every public surface in this crate is consumed by at least two
//! callers — `tr-verify` (T1 author-key checks), the future `root
//! publish` flow (signing a freshly built `.tr`), and the desktop
//! install sheet (showing which key signed an incoming pack). This
//! is the verify-once-use-everywhere foundation: implementations
//! must not duplicate Ed25519 plumbing across crates.
//!
//! The crate is split into four modules:
//!
//! - [`keypair`] — generate/load/sign with Ed25519.
//! - [`keystore`] — on-disk store of trusted public keys (and,
//!   optionally, our own private signing keys), default-pathed to
//!   `~/.config/thinkingroot/keys/`.
//! - [`did`] — `did:web:` and `did:tr:agent:` method resolvers,
//!   plus the [`Did`] wrapper type used across the surface.
//! - [`error`] — single error enum surfaced by every public call.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod did;
pub mod error;
pub mod keypair;
pub mod keystore;

pub use did::{Did, DidMethod, DidResolver, ResolvedDid, VcVerifier};
pub use error::{Error, Result};
pub use keypair::{Keypair, PublicKeyRef};
pub use keystore::{Keystore, TrustedKey};
