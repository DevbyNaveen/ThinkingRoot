//! Top-level Rooter orchestrator.
//!
//! Runs the five-probe battery on each candidate claim, short-circuits on
//! fatal probe failures, and produces signed [`TrialVerdict`]s and
//! [`Certificate`]s ready to be persisted to CozoDB.
//!
//! Week 2 lights up two fatal probes (provenance, contradiction). The three
//! non-fatal probes (predicate, topology, temporal) are stubs that return
//! `skipped` results — they will wire in Weeks 3–5 without changing this
//! orchestrator.

use std::sync::Arc;

use chrono::Utc;
use thinkingroot_core::types::{AdmissionTier, Claim, ClaimId, DerivationProof, Predicate};
use thinkingroot_graph::graph::GraphStore;

use crate::certificate::Certificate;
use crate::config::RootingConfig;
use crate::probes::{
    contradiction::ContradictionProbe, predicate::PredicateProbe, provenance::ProvenanceProbe,
    temporal::TemporalProbe, topology::TopologyProbe, Probe, ProbeContext, ProbeName, ProbeResult,
};
use crate::source_store::SourceByteStore;
use crate::verdict::TrialVerdict;
use crate::{Result, ROOTER_VERSION};

/// Progress callback invoked after each claim is tried. Signature matches the
/// existing `GroundingProgressFn` convention used elsewhere in the pipeline.
pub type RootingProgressFn = Arc<dyn Fn(usize, usize) + Send + Sync>;

/// Candidate passed to the Rooter for trial.
pub struct CandidateClaim<'c> {
    pub claim: &'c Claim,
    pub predicate: Option<&'c Predicate>,
    pub derivation: Option<&'c DerivationProof>,
}

/// Aggregate result of a batch trial.
#[derive(Debug, Default)]
pub struct RootingOutput {
    pub verdicts: Vec<TrialVerdict>,
    pub certificates: Vec<Certificate>,
    pub admitted_count: usize,
    pub quarantined_count: usize,
    pub rejected_count: usize,
}

impl RootingOutput {
    /// Look up the verdict for a specific claim id, if present.
    pub fn verdict_for(&self, claim_id: ClaimId) -> Option<&TrialVerdict> {
        self.verdicts.iter().find(|v| v.claim_id == claim_id)
    }
}

/// The Rooter runs trials against a knowledge graph and a byte store.
pub struct Rooter<'a> {
    graph: &'a GraphStore,
    store: &'a dyn SourceByteStore,
    config: RootingConfig,
    progress: Option<RootingProgressFn>,
}

impl<'a> Rooter<'a> {
    /// Create a new Rooter bound to a graph + byte store with the given config.
    pub fn new(
        graph: &'a GraphStore,
        store: &'a dyn SourceByteStore,
        config: RootingConfig,
    ) -> Self {
        Self {
            graph,
            store,
            config,
            progress: None,
        }
    }

    /// Attach a progress callback invoked after each claim trial.
    pub fn with_progress(mut self, progress: RootingProgressFn) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn config(&self) -> &RootingConfig {
        &self.config
    }

    /// Run the five-probe battery on each candidate. Short-circuits on fatal
    /// probe failures; non-fatal failures push the claim to `Quarantined` tier.
    pub fn root_batch(&self, candidates: &[CandidateClaim<'_>]) -> Result<RootingOutput> {
        let mut output = RootingOutput::default();
        let total = candidates.len();
        for (idx, candidate) in candidates.iter().enumerate() {
            let ctx = ProbeContext {
                claim: candidate.claim,
                predicate: candidate.predicate,
                derivation: candidate.derivation,
                graph: self.graph,
                store: self.store,
                config: &self.config,
            };

            let (verdict, certificate) = trial_one(&ctx)?;

            match verdict.admission_tier {
                AdmissionTier::Rooted => output.admitted_count += 1,
                AdmissionTier::Attested => output.admitted_count += 1,
                AdmissionTier::Quarantined => output.quarantined_count += 1,
                AdmissionTier::Rejected => output.rejected_count += 1,
            }

            if let Some(c) = certificate {
                output.certificates.push(c);
            }
            output.verdicts.push(verdict);

            if let Some(ref progress) = self.progress {
                progress(idx + 1, total);
            }
        }
        Ok(output)
    }
}

/// Execute the full probe battery on a single claim, returning the verdict
/// and (for admitted claims) a verification certificate.
fn trial_one(ctx: &ProbeContext<'_>) -> Result<(TrialVerdict, Option<Certificate>)> {
    let trial_at = Utc::now();
    let mut probes: Vec<ProbeResult> = Vec::with_capacity(5);

    // Provenance (FATAL).
    let provenance = ProvenanceProbe.run(ctx)?;
    let provenance_passed = provenance.passed;
    probes.push(provenance);
    if !provenance_passed {
        return Ok((
            build_verdict(
                ctx.claim.id,
                trial_at,
                AdmissionTier::Rejected,
                probes,
                None,
                Some("provenance failed"),
            ),
            None,
        ));
    }

    // Contradiction (FATAL).
    let contradiction = ContradictionProbe.run(ctx)?;
    let contradiction_passed = contradiction.passed;
    probes.push(contradiction);
    if !contradiction_passed {
        return Ok((
            build_verdict(
                ctx.claim.id,
                trial_at,
                AdmissionTier::Rejected,
                probes,
                None,
                Some("contradiction failed"),
            ),
            None,
        ));
    }

    // Predicate (non-fatal — Week 3+).
    let predicate_result = match ctx.predicate {
        Some(_) => match PredicateProbe.run(ctx) {
            Ok(r) => r,
            Err(_) => ProbeResult::skipped(ProbeName::Predicate, "engine not yet available"),
        },
        None => ProbeResult::skipped(ProbeName::Predicate, "no predicate attached"),
    };
    let predicate_passed = predicate_result.passed;
    probes.push(predicate_result);

    // Topology (non-fatal — Week 4).
    let topology_result = match TopologyProbe.run(ctx) {
        Ok(r) => r,
        Err(_) => ProbeResult::skipped(ProbeName::Topology, "engine not yet available"),
    };
    let topology_passed = topology_result.passed;
    probes.push(topology_result);

    // Temporal (non-fatal — Week 5).
    let temporal_result = match TemporalProbe.run(ctx) {
        Ok(r) => r,
        Err(_) => ProbeResult::skipped(ProbeName::Temporal, "engine not yet available"),
    };
    let temporal_passed = temporal_result.passed;
    probes.push(temporal_result);

    let all_non_fatal_pass = predicate_passed && topology_passed && temporal_passed;
    // A claim with no probe evidence beyond fatal probes (i.e., no predicate
    // and the non-fatal probes were skipped) is Attested, not Rooted. Rooted
    // requires at least one non-skipped non-fatal probe to have passed.
    let has_active_nonfatal = probes
        .iter()
        .skip(2) // skip provenance + contradiction
        .any(|p| p.score >= 0.0);
    let tier = if !all_non_fatal_pass {
        AdmissionTier::Quarantined
    } else if has_active_nonfatal {
        AdmissionTier::Rooted
    } else {
        AdmissionTier::Attested
    };

    let (certificate, cert_hash) = match tier {
        AdmissionTier::Rooted | AdmissionTier::Attested => {
            let cert = build_certificate(ctx, trial_at, &probes)?;
            let hash = cert.hash.clone();
            (Some(cert), Some(hash))
        }
        _ => (None, None),
    };

    let failure_reason = if tier == AdmissionTier::Quarantined {
        Some(
            probes
                .iter()
                .skip(2)
                .find(|p| !p.passed)
                .map(|p| p.detail.clone())
                .unwrap_or_else(|| "non-fatal probe failed".into()),
        )
    } else {
        None
    };

    Ok((
        build_verdict(
            ctx.claim.id,
            trial_at,
            tier,
            probes,
            cert_hash,
            failure_reason.as_deref(),
        ),
        certificate,
    ))
}

fn build_verdict(
    claim_id: ClaimId,
    trial_at: chrono::DateTime<Utc>,
    tier: AdmissionTier,
    probes: Vec<ProbeResult>,
    certificate_hash: Option<String>,
    failure_reason: Option<&str>,
) -> TrialVerdict {
    TrialVerdict {
        id: ulid::Ulid::new().to_string(),
        claim_id,
        trial_at,
        admission_tier: tier,
        probes,
        certificate_hash,
        failure_reason: failure_reason.map(|s| s.to_string()),
        rooter_version: ROOTER_VERSION.to_string(),
    }
}

fn build_certificate(
    ctx: &ProbeContext<'_>,
    trial_at: chrono::DateTime<Utc>,
    probes: &[ProbeResult],
) -> Result<Certificate> {
    // Canonical inputs: fields that, if any change, should produce a new hash.
    let source = ctx
        .graph
        .get_source_by_id(&ctx.claim.source.to_string())
        .map_err(|e| crate::RootingError::Graph(format!("source lookup for cert: {e}")))?;
    let source_content_hash = source
        .as_ref()
        .map(|s| s.content_hash.0.clone())
        .unwrap_or_default();

    let inputs = CertificateInput {
        rooter_version: ROOTER_VERSION,
        claim_id: ctx.claim.id.to_string(),
        statement_hash: blake3::hash(ctx.claim.statement.as_bytes()).to_hex().to_string(),
        source_content_hash: source_content_hash.clone(),
        predicate_hash: ctx
            .predicate
            .map(|p| blake3::hash(format!("{:?}", p).as_bytes()).to_hex().to_string()),
        parent_claim_ids: ctx
            .derivation
            .map(|d| d.parent_claim_ids.iter().map(|id| id.to_string()).collect())
            .unwrap_or_default(),
        trial_day: (trial_at.timestamp() / 86_400),
    };
    let inputs_json = serde_json::to_string(&inputs)?;
    let outputs_json = serde_json::to_string(probes)?;

    let hash = blake3::hash(inputs_json.as_bytes()).to_hex().to_string();

    Ok(Certificate {
        hash,
        claim_id: ctx.claim.id.to_string(),
        created_at: trial_at,
        probe_inputs_json: inputs_json,
        probe_outputs_json: outputs_json,
        rooter_version: ROOTER_VERSION.to_string(),
        source_content_hash,
    })
}

#[derive(serde::Serialize)]
struct CertificateInput<'a> {
    rooter_version: &'a str,
    claim_id: String,
    statement_hash: String,
    source_content_hash: String,
    predicate_hash: Option<String>,
    parent_claim_ids: Vec<String>,
    trial_day: i64,
}
