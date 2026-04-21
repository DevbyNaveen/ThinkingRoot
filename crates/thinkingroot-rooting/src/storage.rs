//! Bridge between Rooting's in-memory types and the thinkingroot-graph CozoDB
//! helpers. Translates [`TrialVerdict`] and [`Certificate`] into the primitive
//! tuples that GraphStore accepts.

use thinkingroot_graph::graph::GraphStore;

use crate::certificate::Certificate;
use crate::verdict::TrialVerdict;
use crate::{Result, RootingError};

/// Persist a batch of trial verdicts to the `trial_verdicts` relation.
pub fn insert_verdicts_batch(graph: &GraphStore, verdicts: &[TrialVerdict]) -> Result<()> {
    if verdicts.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String, String, f64, String, f64, f64, f64, f64, f64, String, String, String)> =
        verdicts
            .iter()
            .map(|v| {
                let prov = find_score(v, crate::probes::ProbeName::Provenance);
                let contra = find_score(v, crate::probes::ProbeName::Contradiction);
                let pred = find_score(v, crate::probes::ProbeName::Predicate);
                let topo = find_score(v, crate::probes::ProbeName::Topology);
                let temp = find_score(v, crate::probes::ProbeName::Temporal);
                (
                    v.id.clone(),
                    v.claim_id.to_string(),
                    v.trial_at.timestamp() as f64,
                    v.admission_tier.as_str().to_string(),
                    prov,
                    contra,
                    pred,
                    topo,
                    temp,
                    v.certificate_hash.clone().unwrap_or_default(),
                    v.failure_reason.clone().unwrap_or_default(),
                    v.rooter_version.clone(),
                )
            })
            .collect();

    graph
        .insert_trial_verdicts_batch(&rows)
        .map_err(|e| RootingError::Graph(format!("insert_trial_verdicts_batch: {e}")))
}

/// Persist a batch of verification certificates. Idempotent — rows with the
/// same hash upsert cleanly.
pub fn insert_certificates_batch(
    graph: &GraphStore,
    certificates: &[Certificate],
) -> Result<()> {
    if certificates.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String, String, f64, String, String, String, String)> = certificates
        .iter()
        .map(|c| {
            (
                c.hash.clone(),
                c.claim_id.clone(),
                c.created_at.timestamp() as f64,
                c.probe_inputs_json.clone(),
                c.probe_outputs_json.clone(),
                c.rooter_version.clone(),
                c.source_content_hash.clone(),
            )
        })
        .collect();

    graph
        .insert_certificates_batch(&rows)
        .map_err(|e| RootingError::Graph(format!("insert_certificates_batch: {e}")))
}

fn find_score(verdict: &TrialVerdict, name: crate::probes::ProbeName) -> f64 {
    verdict
        .probes
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.score)
        .unwrap_or(-1.0)
}
