//! T8+T9: IncrementalSummary wire shape and per-phase timing tests.
//!
//! Tests are structured in TDD order — each group exercises one
//! invariant from the spec.  The pipeline-level tests (tests 5–9)
//! use a real tempdir workspace compiled with structural extraction
//! only (TR_SKIP_BYTE_AUDIT=1 + no-rooting flag + no LLM config) so
//! they run fully offline.

use std::fs;
use std::path::PathBuf;

use tempfile::tempdir;
use thinkingroot_core::{IncrementalSummary, PHASE_NAMES};
use thinkingroot_serve::pipeline::{ProgressEvent, PipelineOptions, run_pipeline_with_options};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ── 1. PHASE_NAMES constant ───────────────────────────────────────────────────

#[test]
fn phase_names_constant_is_complete() {
    let expected = [
        "diff",
        "extract",
        "ground",
        "fingerprint",
        "remove_sources",
        "entity_relations",
        "link",
        "structural_persist",
        "structural_resolve",
        "audit",
        "other",
    ];
    assert_eq!(
        PHASE_NAMES.len(),
        expected.len(),
        "PHASE_NAMES must have exactly 11 stable keys"
    );
    for name in &expected {
        assert!(
            PHASE_NAMES.contains(name),
            "PHASE_NAMES is missing key {name:?}"
        );
    }
}

// ── 2. IncrementalSummary::default ────────────────────────────────────────────

#[test]
fn incremental_summary_default_is_all_zero() {
    let s = IncrementalSummary::default();
    assert_eq!(s.sources_total, 0);
    assert_eq!(s.sources_unchanged, 0);
    assert_eq!(s.sources_truly_changed, 0);
    assert_eq!(s.sources_deleted, 0);
    assert_eq!(s.sources_resolution_dirty, 0);
    assert_eq!(s.claims_added, 0);
    assert_eq!(s.claims_updated, 0);
    assert_eq!(s.claims_deleted, 0);
    assert_eq!(s.structural_rows_emitted, 0);
    assert_eq!(s.structural_rows_cascaded, 0);
    assert_eq!(s.bytes_re_extracted, 0);
    assert_eq!(s.llm_calls, 0);
    assert_eq!(s.cache_hits, 0);
    assert_eq!(s.structural_extractions, 0);
    assert!(s.phase_timings.is_empty());
    assert_eq!(s.total_elapsed_ms, 0);
}

// ── 3. Serde round-trip ───────────────────────────────────────────────────────

#[test]
fn incremental_summary_serde_round_trip() {
    let mut s = IncrementalSummary {
        sources_total: 10,
        sources_unchanged: 7,
        sources_truly_changed: 3,
        sources_deleted: 1,
        sources_resolution_dirty: 2,
        claims_added: 42,
        claims_updated: 0,
        claims_deleted: 15,
        structural_rows_emitted: 300,
        structural_rows_cascaded: 50,
        bytes_re_extracted: 819_200,
        llm_calls: 12,
        cache_hits: 8,
        structural_extractions: 22,
        phase_timings: Default::default(),
        total_elapsed_ms: 4_321,
    };
    s.phase_timings.insert("diff".to_string(), 120);
    s.phase_timings.insert("extract".to_string(), 3_800);
    s.phase_timings.insert("link".to_string(), 200);

    let json = serde_json::to_string(&s).expect("serialize");
    let decoded: IncrementalSummary = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(decoded.sources_total, s.sources_total);
    assert_eq!(decoded.sources_unchanged, s.sources_unchanged);
    assert_eq!(decoded.sources_truly_changed, s.sources_truly_changed);
    assert_eq!(decoded.sources_deleted, s.sources_deleted);
    assert_eq!(decoded.sources_resolution_dirty, s.sources_resolution_dirty);
    assert_eq!(decoded.claims_added, s.claims_added);
    assert_eq!(decoded.claims_updated, 0);
    assert_eq!(decoded.claims_deleted, s.claims_deleted);
    assert_eq!(decoded.structural_rows_emitted, s.structural_rows_emitted);
    assert_eq!(decoded.structural_rows_cascaded, s.structural_rows_cascaded);
    assert_eq!(decoded.bytes_re_extracted, s.bytes_re_extracted);
    assert_eq!(decoded.llm_calls, s.llm_calls);
    assert_eq!(decoded.cache_hits, s.cache_hits);
    assert_eq!(decoded.structural_extractions, s.structural_extractions);
    assert_eq!(decoded.total_elapsed_ms, s.total_elapsed_ms);
    assert_eq!(decoded.phase_timings.get("diff"), Some(&120u64));
    assert_eq!(decoded.phase_timings.get("extract"), Some(&3_800u64));
    assert_eq!(decoded.phase_timings.get("link"), Some(&200u64));
}

// ── 4. Unknown fields → default ──────────────────────────────────────────────

#[test]
fn incremental_summary_unknown_fields_default_to_zero() {
    let json = r#"{"sources_total": 5}"#;
    let s: IncrementalSummary = serde_json::from_str(json).expect("deserialize partial");
    assert_eq!(s.sources_total, 5);
    assert_eq!(s.sources_unchanged, 0);
    assert_eq!(s.sources_truly_changed, 0);
    assert_eq!(s.claims_added, 0);
    assert_eq!(s.claims_deleted, 0);
    assert!(s.phase_timings.is_empty());
    assert_eq!(s.total_elapsed_ms, 0);
}

// ── Shared fixture helpers ────────────────────────────────────────────────────

/// Create a minimal workspace at `root` with the given files.
///
/// No `config.toml` is written — `Config::load_merged` falls back to
/// `Config::default()` when the file is absent, which means no LLM
/// provider is configured and structural extraction runs (zero network
/// calls).  TR_SKIP_BYTE_AUDIT=1 is set in each `compile` call so the
/// byte-coverage audit does not reject purely-structural workspaces.
fn make_workspace(root: &PathBuf, files: &[(&str, &str)]) {
    let tr_dir = root.join(".thinkingroot");
    fs::create_dir_all(&tr_dir).unwrap();

    for (name, content) in files {
        fs::write(root.join(name), content).unwrap();
    }
}

async fn compile(root: &PathBuf) -> thinkingroot_core::Result<thinkingroot_serve::pipeline::PipelineResult> {
    // Force structural-only: no real LLM calls in tests.
    // SAFETY: single-threaded test; no concurrent reads of this env var.
    unsafe { std::env::set_var("TR_SKIP_BYTE_AUDIT", "1") };
    let (tx, _rx) = mpsc::unbounded_channel();
    run_pipeline_with_options(
        root,
        None,
        Some(tx),
        PipelineOptions {
            cancel: CancellationToken::new(),
            no_rooting: true,
        },
    )
    .await
}

async fn compile_collect_events(
    root: &PathBuf,
) -> (
    thinkingroot_core::Result<thinkingroot_serve::pipeline::PipelineResult>,
    Vec<ProgressEvent>,
) {
    // SAFETY: single-threaded test; no concurrent reads of this env var.
    unsafe { std::env::set_var("TR_SKIP_BYTE_AUDIT", "1") };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = run_pipeline_with_options(
        root,
        None,
        Some(tx),
        PipelineOptions {
            cancel: CancellationToken::new(),
            no_rooting: true,
        },
    )
    .await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (result, events)
}

// ── 5. Full compile populates summary ────────────────────────────────────────

#[tokio::test]
async fn pipeline_result_includes_summary_after_compile() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    make_workspace(
        &root,
        &[
            ("README.md", "# Hello\n\nThis is a test workspace.\n"),
            ("notes.md", "## Notes\n\nSome important notes here.\n"),
        ],
    );

    let result = compile(&root).await.expect("pipeline should succeed");
    let s = &result.incremental_summary;

    assert_eq!(
        s.sources_total, 2,
        "sources_total should equal the number of parsed files"
    );
    assert!(
        s.total_elapsed_ms > 0,
        "total_elapsed_ms must be non-zero for a real compile"
    );
    assert!(
        s.phase_timings.contains_key("diff"),
        "phase_timings must contain the 'diff' key; got: {:?}",
        s.phase_timings
    );
}

// ── 6. Early-return path populates summary ────────────────────────────────────

#[tokio::test]
async fn pipeline_early_return_still_populates_summary() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    make_workspace(
        &root,
        &[
            ("doc_a.md", "# Alpha\n\nContent for alpha document.\n"),
            ("doc_b.md", "# Beta\n\nContent for beta document.\n"),
        ],
    );

    // First compile — establishes the baseline fingerprint state.
    let first = compile(&root).await.expect("first compile should succeed");
    assert!(
        first.incremental_summary.sources_total > 0,
        "first compile should see sources"
    );

    // Second compile on identical files — hits the early-return path.
    let second = compile(&root).await.expect("second compile should succeed");
    let s = &second.incremental_summary;

    assert!(
        s.sources_total > 0,
        "early-return summary must still carry sources_total"
    );
    assert_eq!(
        s.sources_unchanged,
        s.sources_total,
        "on identical recompile all sources should be unchanged; \
         total={}, unchanged={}",
        s.sources_total,
        s.sources_unchanged
    );
    assert_eq!(
        s.sources_truly_changed, 0,
        "no sources should be truly-changed on identical recompile"
    );
}

// ── 7. Timing invariant: phase sum ≤ total ────────────────────────────────────

#[tokio::test]
async fn phase_timings_sum_within_total_elapsed() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    make_workspace(
        &root,
        &[("timing_test.md", "# Timing\n\nContent to compile.\n")],
    );

    let result = compile(&root).await.expect("pipeline should succeed");
    let s = &result.incremental_summary;

    let phase_sum: u64 = s.phase_timings.values().sum();
    assert!(
        phase_sum <= s.total_elapsed_ms,
        "sum of phase timings ({phase_sum}) must be ≤ total_elapsed_ms ({}) \
         — the 'other' key absorbs any residual",
        s.total_elapsed_ms
    );
}

// ── 8. PhaseDone events emitted ───────────────────────────────────────────────

#[tokio::test]
async fn pipeline_emits_phase_done_events() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    make_workspace(
        &root,
        &[("phase_done.md", "# Phase Done\n\nSome content here.\n")],
    );

    let (result, events) = compile_collect_events(&root).await;
    result.expect("pipeline should succeed");

    let phase_done_names: Vec<&str> = events
        .iter()
        .filter_map(|ev| {
            if let ProgressEvent::PhaseDone { name, .. } = ev {
                Some(name.as_str())
            } else {
                None
            }
        })
        .collect();

    assert!(
        !phase_done_names.is_empty(),
        "expected at least one PhaseDone event; got none. \
         All events: {events:?}"
    );
    let canonical_names: std::collections::HashSet<&str> =
        PHASE_NAMES.iter().copied().collect();
    for name in &phase_done_names {
        assert!(
            canonical_names.contains(name),
            "PhaseDone emitted non-canonical name {name:?}; \
             valid names: {canonical_names:?}"
        );
    }
}

// ── 9. IncrementalDone event emitted ─────────────────────────────────────────

#[tokio::test]
async fn pipeline_emits_incremental_done_event() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    make_workspace(
        &root,
        &[("incremental_done.md", "# Done\n\nContent for done test.\n")],
    );

    let (result, events) = compile_collect_events(&root).await;
    result.expect("pipeline should succeed");

    let done_events: Vec<&ProgressEvent> = events
        .iter()
        .filter(|ev| matches!(ev, ProgressEvent::IncrementalDone { .. }))
        .collect();

    assert_eq!(
        done_events.len(),
        1,
        "exactly one IncrementalDone event must be emitted; \
         got {} — all events: {events:?}",
        done_events.len()
    );

    if let ProgressEvent::IncrementalDone { summary } = done_events[0] {
        assert_eq!(
            summary.sources_total, 1,
            "IncrementalDone summary should reflect 1 parsed file"
        );
        assert!(
            summary.total_elapsed_ms > 0,
            "IncrementalDone summary must have non-zero total_elapsed_ms"
        );
    }
}
