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
    // `trial_verdicts` declares `certificate_hash` + `failure_reason` as
    // `String default ''` (see graph.rs schema).  Cozo string columns are
    // non-nullable, so the schema's empty-string default IS the canonical
    // "absent" sentinel for both fields.  A real `certificate_hash` is
    // always 64-char BLAKE3 hex and a real `failure_reason` is always
    // non-empty by construction in `rooter.rs`, so `""` cannot collide
    // with a legitimate value.  We turn that domain invariant into a
    // defensive assertion: if a regression ever stamps `Some("".into())`
    // into a verdict we want to fail the test, not silently round-trip a
    // confusing sentinel.
    let rows: Vec<(
        String,
        String,
        f64,
        String,
        f64,
        f64,
        f64,
        f64,
        f64,
        String,
        String,
        String,
    )> = verdicts
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
                option_to_sentinel("certificate_hash", v.certificate_hash.as_deref()),
                option_to_sentinel("failure_reason", v.failure_reason.as_deref()),
                v.rooter_version.clone(),
            )
        })
        .collect();

    graph
        .insert_trial_verdicts_batch(&rows)
        .map_err(|e| RootingError::Graph(format!("insert_trial_verdicts_batch: {e}")))
}

/// Encode an `Option<&str>` into the schema's empty-string sentinel.
///
/// Asserts that real values are never empty — pre-empts the failure mode
/// where downstream readers cannot tell `Some("")` from `None`.  In debug
/// builds an empty `Some` panics; in release builds it round-trips as the
/// sentinel, the same outcome the legacy `unwrap_or_default()` produced.
fn option_to_sentinel(field: &'static str, value: Option<&str>) -> String {
    match value {
        Some(s) => {
            debug_assert!(
                !s.is_empty(),
                "{field}: real values must never be empty — empty maps to the schema sentinel"
            );
            s.to_string()
        }
        None => String::new(),
    }
}

/// Persist a batch of verification certificates. Idempotent — rows with the
/// same hash upsert cleanly.
pub fn insert_certificates_batch(graph: &GraphStore, certificates: &[Certificate]) -> Result<()> {
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

/// Resolve a probe score for the given name from a verdict, encoding "did
/// not run" as the schema's sentinel `-1.0` (see `trial_verdicts` schema:
/// every `*_score` column declares `Float default -1.0`).
///
/// Probes use two values to communicate "no signal":
/// - `ProbeResult::skipped(...)` returns the `-1.0` sentinel directly
///   (e.g. predicate probe with no predicate attached, temporal probe
///   on a claim with no event date).
/// - "no row in `verdict.probes` at all" — the probe never ran (a fatal
///   probe short-circuited before reaching it).
///
/// Both collapse to `-1.0` on persistence.  In debug builds we assert
/// real, active probe scores stay in `[0.0, 1.0]` so a regression in
/// any probe that produces NaN / out-of-band values is caught loudly;
/// the schema sentinel `-1.0` is exempted from the bound check.
fn find_score(verdict: &TrialVerdict, name: crate::probes::ProbeName) -> f64 {
    match verdict.probes.iter().find(|p| p.name == name) {
        Some(p) => {
            debug_assert!(
                p.score == -1.0 || (0.0..=1.0).contains(&p.score),
                "probe `{name:?}` produced score {} outside [0.0, 1.0] and \
                 not equal to the `-1.0` skipped sentinel — schema invariant \
                 violated",
                p.score
            );
            p.score
        }
        None => -1.0,
    }
}
