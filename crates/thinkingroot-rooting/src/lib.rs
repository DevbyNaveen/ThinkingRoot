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
pub mod storage;
mod verdict;

/// Transitional re-export. The byte-store primitives moved to
/// `thinkingroot_graph::source_store` on 2026-05-14 (Phase 1 of the rooting
/// crate dissolution); this module exists so internal probe + rooter modules
/// and any out-of-tree consumer that still imports
/// `thinkingroot_rooting::{source_store, FileSystemSourceStore, …}` keeps
/// compiling until Phase 6 deletes the rooting crate entirely.
pub mod source_store {
    pub use thinkingroot_graph::source_store::{
        FileSystemSourceStore, SourceByteStore, SourceBytes,
    };
}

pub use thinkingroot_core::types::{
    AdmissionTier, DerivationProof, Predicate, PredicateLanguage, PredicateScope,
};

pub use crate::certificate::Certificate;
pub use crate::config::RootingConfig;
pub use crate::predicate::{PredicateEngine, PredicateEvaluation};
pub use crate::probes::{Probe, ProbeContext, ProbeName, ProbeResult};
pub use crate::rooter::{CandidateClaim, Rooter, RootingOutput, RootingProgressFn};
// Transitional re-export — see `mod source_store` above.
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

/// Bridge to the engine-wide `Error` type. The byte-store primitives moved to
/// `thinkingroot-graph` on 2026-05-14 and now surface
/// `thinkingroot_core::Error` from their `Result`s; the rooting probes still
/// use `?` against those calls so we need a flat `From` to keep the existing
/// error-propagation shape. Maps every variant onto `RootingError::Graph` —
/// good enough for an inert crate scheduled for Phase 6 deletion.
impl From<thinkingroot_core::Error> for RootingError {
    fn from(e: thinkingroot_core::Error) -> Self {
        RootingError::Graph(e.to_string())
    }
}

/// Crate-local result alias.
pub type Result<T> = std::result::Result<T, RootingError>;

/// Version string recorded on every trial verdict. Changes invalidate
/// previously-stored certificates (they remain readable for audit).
pub const ROOTER_VERSION: &str = env!("CARGO_PKG_VERSION");
