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
/// carries (when the phase actually ran on this compile).  Adding a new
/// phase requires extending this list.  "other" is the residual that
/// captures any time spent outside an instrumented region.
///
/// Note: Phase 7e (`structural_resolve`) is not listed here — it runs
/// inside `Linker::link()` and its elapsed time is subsumed under the
/// `link` key.  Splitting it out would require instrumenting the linker
/// crate; deferred until a downstream consumer asks for the breakdown.
pub const PHASE_NAMES: &[&str] = &[
    "diff", "extract", "ground", "fingerprint", "remove_sources",
    "entity_relations", "link", "structural_persist",
    "audit", "other",
];

/// Format a byte count using IEC binary units (KiB/MiB/GiB).
///
/// Below 1024 bytes shows the raw count (e.g. `"512 B"`); above shows
/// two decimals (e.g. `"1.50 MiB"`). Uses IEC labels (1 KiB = 1024 bytes)
/// rather than SI labels (1 KB = 1000 bytes) to avoid confusion.
/// Canonical implementation — used by the CLI summary printer and
/// tr-render markdown summaries. Both must call this to guarantee
/// consistent output for the same byte count.
pub fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;
    if n >= TIB {
        format!("{:.2} TiB", n as f64 / TIB as f64)
    } else if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_below_kib() {
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn format_bytes_exact_kib() {
        assert_eq!(format_bytes(1024), "1.00 KiB");
    }

    #[test]
    fn format_bytes_mib_range() {
        assert_eq!(format_bytes(1_500_000), "1.43 MiB");
    }

    #[test]
    fn format_bytes_exact_gib() {
        assert_eq!(format_bytes(1_073_741_824), "1.00 GiB");
    }
}
