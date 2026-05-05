use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

/// Structured incremental-compile delta surfaced at the end of every
/// `run_pipeline` call.  Wire-shape consumers: CLI summary printer
/// (T10), desktop progress emitter, SSE `IncrementalDone` event.
///
/// Every successful compile populates this — including the early-return
/// path when nothing changed (in which case sources_truly_changed = 0,
/// claims_added = 0, etc.).  This guarantees consumers never have to
/// branch on "is the summary present" and gives honest telemetry on
/// the steady-state "no edits since last compile" case.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IncrementalSummary {
    // Source-level deltas (counts derived from Phase 1 + Phase 3 sets).
    #[serde(default)] pub sources_total: usize,
    #[serde(default)] pub sources_unchanged: usize,
    #[serde(default)] pub sources_truly_changed: usize,
    #[serde(default)] pub sources_deleted: usize,
    #[serde(default)] pub sources_resolution_dirty: usize,

    // Claim-level deltas — computed from cascade snapshot, NOT stubbed to 0.
    // claims_deleted = rows removed in Phase 4 cascade (truly-changed + deleted sources).
    // claims_added   = new claims persisted by Phase 7 for truly-changed sources.
    // claims_updated = 0 ALWAYS in the snapshot model — the per-source rebuild
    //                  is always delete-then-insert (I-W4 atomic rebuild boundary).
    #[serde(default)] pub claims_added: usize,
    #[serde(default)] pub claims_updated: usize,
    #[serde(default)] pub claims_deleted: usize,

    // Structural-row work (33-table substrate per CCC).
    #[serde(default)] pub structural_rows_emitted: usize,
    #[serde(default)] pub structural_rows_cascaded: usize,

    // Extraction work — every byte of every truly-changed source.
    #[serde(default)] pub bytes_re_extracted: u64,
    #[serde(default)] pub llm_calls: usize,
    #[serde(default)] pub cache_hits: usize,
    #[serde(default)] pub structural_extractions: usize,

    // Per-phase wall-clock (stable string keys; see `PHASE_NAMES`).
    #[serde(default)] pub phase_timings: BTreeMap<String, u64>,
    #[serde(default)] pub total_elapsed_ms: u64,
}

/// Canonical phase name list — the keys IncrementalSummary.phase_timings
/// carries.  Adding a new phase requires extending this list.  The
/// pipeline emits these in order; "other" is the residual that captures
/// any time spent outside an instrumented region (config load, drop
/// guards, etc.).
pub const PHASE_NAMES: &[&str] = &[
    "diff", "extract", "ground", "fingerprint", "remove_sources",
    "entity_relations", "link", "structural_persist",
    "structural_resolve", "audit", "other",
];
