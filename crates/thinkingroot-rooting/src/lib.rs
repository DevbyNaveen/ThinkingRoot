//! # Rooting
//!
//! Deterministic admission gate for derived knowledge claims. Every candidate
//! claim must survive five deterministic probes before admission:
//!
//! 1. **Provenance** — byte-range token overlap against source (fatal)
//! 2. **Contradiction** — Datalog check against high-confidence opposing claims (fatal)
//! 3. **Predicate** — executable assertion (regex / tree-sitter-rust AST / JSONPath)
//! 4. **Topology** — structural co-occurrence check against parent claims
//! 5. **Temporal** — timestamp consistency across derivation parents
//!
//! Survivors receive an [`AdmissionTier`] (`Rooted` or `Quarantined`) and a
//! cryptographic [`Certificate`] that anyone can re-verify by re-running the
//! probes against the stored source bytes.
//!
//! This crate provides the primitive. Consumers:
//! - The compile pipeline inserts Rooting as Phase 6.5 (see `thinkingroot-serve::pipeline`).
//! - Agent writes via the `contribute` MCP tool route through this gate.
//! - The SaaS re-rooting worker periodically re-executes all certificates.

#![deny(rust_2018_idioms)]

mod certificate;
mod config;
mod predicate;
mod probes;
mod rooter;
mod source_store;
pub mod storage;
mod verdict;

pub use thinkingroot_core::types::{
    AdmissionTier, DerivationProof, Predicate, PredicateLanguage, PredicateScope,
};

pub use crate::certificate::Certificate;
pub use crate::config::RootingConfig;
pub use crate::predicate::{PredicateEngine, PredicateEvaluation};
pub use crate::probes::{Probe, ProbeContext, ProbeName, ProbeResult};
pub use crate::rooter::{CandidateClaim, Rooter, RootingOutput, RootingProgressFn};
pub use crate::source_store::{FileSystemSourceStore, SourceByteStore, SourceBytes};
pub use crate::verdict::TrialVerdict;

/// Error type for Rooting operations.
#[derive(Debug, thiserror::Error)]
pub enum RootingError {
    /// I/O error reading or writing source bytes.
    #[error("source byte store I/O error: {0}")]
    SourceStoreIo(#[from] std::io::Error),
    /// Graph query error propagated from thinkingroot-graph.
    #[error("graph error: {0}")]
    Graph(String),
    /// Malformed predicate (bad regex / AST / JSONPath).
    #[error("invalid predicate: {0}")]
    InvalidPredicate(String),
    /// Serialization failure when building a certificate or verdict.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Crate-local result alias.
pub type Result<T> = std::result::Result<T, RootingError>;

/// Version string recorded on every trial verdict. Changes invalidate
/// previously-stored certificates (they remain readable for audit).
pub const ROOTER_VERSION: &str = env!("CARGO_PKG_VERSION");
