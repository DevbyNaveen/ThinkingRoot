//! The five deterministic probes that gate claim admission.
//!
//! Each probe takes a [`ProbeContext`] and returns a [`ProbeResult`]. Fatal
//! probes (provenance, contradiction) short-circuit the trial on failure.
//! Non-fatal probes continue but push the claim to the Quarantined tier.

use serde::{Deserialize, Serialize};
use thinkingroot_core::types::{Claim, DerivationProof, Predicate};
use thinkingroot_graph::graph::GraphStore;

use crate::{Result, RootingConfig, SourceByteStore};

pub(crate) mod contradiction;
pub(crate) mod predicate;
pub(crate) mod provenance;
pub(crate) mod temporal;
pub(crate) mod topology;

/// Identifier for each probe. Fixed ordering — do not reorder, this leaks
/// into certificates and rooter version semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeName {
    /// Byte-range token overlap against source.
    Provenance,
    /// Datalog query for opposing high-confidence claims.
    Contradiction,
    /// Executable assertion against source bytes.
    Predicate,
    /// Structural co-occurrence with parent claims.
    Topology,
    /// Timestamp consistency with parent claims.
    Temporal,
}

impl ProbeName {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Provenance => "provenance",
            Self::Contradiction => "contradiction",
            Self::Predicate => "predicate",
            Self::Topology => "topology",
            Self::Temporal => "temporal",
        }
    }
}

/// Per-probe outcome. Score is `-1.0` when the probe was skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub name: ProbeName,
    pub score: f64,
    pub passed: bool,
    pub detail: String,
}

impl ProbeResult {
    pub fn skipped(name: ProbeName, detail: impl Into<String>) -> Self {
        Self {
            name,
            score: -1.0,
            passed: true,
            detail: detail.into(),
        }
    }
}

/// Inputs shared across all probes for a single claim trial.
pub struct ProbeContext<'a> {
    pub claim: &'a Claim,
    pub predicate: Option<&'a Predicate>,
    pub derivation: Option<&'a DerivationProof>,
    pub graph: &'a GraphStore,
    pub store: &'a dyn SourceByteStore,
    pub config: &'a RootingConfig,
}

/// Contract implemented by every probe.
pub trait Probe: Send + Sync {
    /// Stable name, recorded in verdicts.
    const NAME: ProbeName;

    /// Whether failure is fatal (Rejected) rather than demoting (Quarantined).
    const FATAL: bool;

    /// Execute the probe. Must be deterministic — no LLM calls, no clock
    /// reads beyond `config.trial_at`, no network I/O.
    fn run(&self, ctx: &ProbeContext<'_>) -> Result<ProbeResult>;
}
