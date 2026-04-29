//! Sigstore keyless OIDC signing + offline DSSE verification for v3 packs.
//!
//! This crate is the Week-0 scaffold. The real implementation lands in
//! Week 3 of the v3 implementation plan
//! (`/Users/naveen/.claude/plans/zippy-wiggling-pelican.md`).
//!
//! At Week 3 the public surface becomes:
//!
//! - [`sign_pack_dsse`] — drives the Fulcio keyless OIDC flow, submits the
//!   DSSE envelope to Rekor, and returns a [`SigstoreBundle`] that bundles
//!   the cert chain, the DSSE envelope, and the Rekor inclusion proof.
//! - [`verify_dsse_offline`] — verifies a [`SigstoreBundle`] against a
//!   pre-computed pack hash without contacting the network. The function
//!   replays the verification chain documented in
//!   `docs/2026-04-29-thinkingroot-v3-final-plan.md` §7.6.
//!
//! Both functions are gated behind the `sigstore-impl` feature on
//! `tr-verify` so that consumers who don't need keyless signing aren't
//! forced to compile `sigstore-rs` and its TLS chain.
//!
//! The DSSE statement type for v3 packs is locked at
//! `application/vnd.thinkingroot.pack.v3+json` per the v3 spec §3.4.

#![forbid(unsafe_code)]

/// DSSE statement type for v3 packs. Locked by spec §3.4.
pub const DSSE_STATEMENT_TYPE: &str = "application/vnd.thinkingroot.pack.v3+json";
