//! Tier 1 acceptance gate — real-data incremental compile benchmark.
//!
//! Exercises the full pipeline (Phase 1 → 9) on a non-trivial workspace
//! of 300 sources designed to look like a real codebase: a heading
//! structure, cross-source markdown links resolving into the Phase 7e
//! call/link graph, and TOML config trees feeding the structural
//! emitters. The fixture stays structural-only (no LLM provider
//! configured) so the bench is deterministic and network-free, but
//! the workspace shape mirrors a 300-file repo — large enough to
//! make Tier 1's wins visible.
//!
//! Acceptance gates (wall time on a typical dev machine):
//!   - 1-file incremental edit:     ≤ 3 s
//!   - 5-file incremental edit:     ≤ 8 s
//!   - 47-file incremental edit:    ≤ 35 s
//!   - sources_truly_changed matches the edit count
//!
//! Run with:
//!   cargo test --test incremental_realdata_bench -- --ignored --nocapture
//!
//! Marked `#[ignore]` because it takes ~30-60s and writes a 300-file
//! tempdir. CI may opt to run nightly.

use std::path::PathBuf;
use std::time::Instant;

use tempfile::tempdir;
use thinkingroot_serve::pipeline::{run_pipeline_with_options, PipelineOptions, PipelineResult};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const N_SOURCES: usize = 300;

/// Acceptance gates in milliseconds. Generous enough to absorb noisy
/// CI hardware while still catching a meaningful regression — pre-Tier 1
/// the same shape took ~140 s on the 47-file trial.
const GATE_ONE_FILE_MS: u128 = 3_000;
const GATE_FIVE_FILE_MS: u128 = 8_000;
const GATE_MANY_FILE_MS: u128 = 35_000;

fn make_workspace(root: &PathBuf) {
    let tr_dir = root.join(".thinkingroot");
    std::fs::create_dir_all(&tr_dir).unwrap();
    // Structural-only — no LLM provider. See incremental_smoke.rs:91-107
    // for the rationale on the empty-string field set.
    std::fs::write(
        tr_dir.join("config.toml"),
        "[llm]\n\
         default_provider = \"\"\n\
         extraction_model = \"\"\n\
         compilation_model = \"\"\n\
         max_concurrent_requests = 5\n\
         request_timeout_secs = 120\n",
    )
    .unwrap();

    // Markdown corpus with heading + cross-source link shape. Every
    // prose line carries a Markdown link so the tier router stays in
    // the link-bearing structural path (zero LLM calls). The link
    // graph forms a ring: doc_i links to doc_{i-1} and doc_{i+1}.
    for i in 0..N_SOURCES {
        let path = root.join(format!("doc_{i:03}.md"));
        let next = (i + 1) % N_SOURCES;
        let prev = if i == 0 { N_SOURCES - 1 } else { i - 1 };
        let content = format!(
            "# Document {i}\n\n\
             ## Overview\n\n\
             See [neighbour](./doc_{next:03}.md) for the next document in this corpus.\n\n\
             ## References\n\n\
             - [previous](./doc_{prev:03}.md) — prior in the ring\n\
             - [next](./doc_{next:03}.md) — subsequent in the ring\n\n\
             ## Summary\n\n\
             This document is part of the Tier 1 benchmark corpus and \
             cross-links [doc {prev}](./doc_{prev:03}.md) and \
             [doc {next}](./doc_{next:03}.md).\n",
        );
        std::fs::write(&path, &content).unwrap();
    }
}

async fn compile_once(root: &PathBuf) -> thinkingroot_core::Result<PipelineResult> {
    let (tx, _rx) = mpsc::unbounded_channel();
    run_pipeline_with_options(
        root,
        None,
        Some(tx),
        PipelineOptions {
            cancel: CancellationToken::new(),
            no_rooting: true,
            skip_byte_audit: true,
            no_incremental: false,
            emit_summaries: false,
        },
    )
    .await
}

/// Edit `count` files by appending a new heading + link-bearing
/// prose line. Returns the set of (1-indexed) doc indices that were
/// edited so the test can assert `sources_truly_changed` matches.
fn edit_files(root: &PathBuf, count: usize, trial_tag: &str) -> Vec<usize> {
    let mut edited = Vec::with_capacity(count);
    // Stride 7 spreads edits across the corpus without overlap up to
    // ~N_SOURCES/7 trials, avoiding accidental same-file collisions.
    for k in 0..count {
        let idx = (k * 7) % N_SOURCES;
        let target = root.join(format!("doc_{idx:03}.md"));
        let mut content = std::fs::read_to_string(&target).unwrap();
        content.push_str(&format!(
            "\n## Trial {trial_tag} edit {k}\n\nAdded by [benchmark {trial_tag}](./trial.md).\n"
        ));
        std::fs::write(&target, &content).unwrap();
        edited.push(idx);
    }
    edited
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real-data acceptance gate; takes 30-60s. Run with `-- --ignored --nocapture`."]
async fn tier1_acceptance_gate() {
    let tmp = tempdir().expect("tempdir");
    let root: PathBuf = tmp.path().to_path_buf();
    make_workspace(&root);

    // Baseline: full compile. We don't gate this — its wall time
    // varies with hardware and isn't part of the Tier 1 win story.
    // It just primes fingerprints for the incremental trials.
    let baseline_started = Instant::now();
    let baseline = compile_once(&root)
        .await
        .expect("baseline compile must succeed");
    let baseline_ms = baseline_started.elapsed().as_millis();
    eprintln!(
        "[bench] BASELINE full compile: {baseline_ms}ms (sources_total={}, sources_truly_changed={})",
        baseline.incremental_summary.sources_total,
        baseline.incremental_summary.sources_truly_changed,
    );

    // ── Trial 1: 1-file edit ──────────────────────────────────────
    edit_files(&root, 1, "T1");
    let t1_start = Instant::now();
    let t1 = compile_once(&root)
        .await
        .expect("trial 1 compile must succeed");
    let t1_ms = t1_start.elapsed().as_millis();
    eprintln!(
        "[bench] INCREMENTAL 1-file edit:  {t1_ms}ms — phase_timings: {:?}",
        t1.incremental_summary.phase_timings
    );
    assert_eq!(
        t1.incremental_summary.sources_truly_changed, 1,
        "1-file edit must report sources_truly_changed=1"
    );
    assert!(
        t1_ms < GATE_ONE_FILE_MS,
        "1-file edit took {t1_ms}ms, gate is {GATE_ONE_FILE_MS}ms — Tier 1 regression"
    );

    // ── Trial 2: 5-file edit ──────────────────────────────────────
    edit_files(&root, 5, "T2");
    let t2_start = Instant::now();
    let t2 = compile_once(&root)
        .await
        .expect("trial 2 compile must succeed");
    let t2_ms = t2_start.elapsed().as_millis();
    eprintln!(
        "[bench] INCREMENTAL 5-file edit:  {t2_ms}ms — phase_timings: {:?}",
        t2.incremental_summary.phase_timings
    );
    assert_eq!(
        t2.incremental_summary.sources_truly_changed, 5,
        "5-file edit must report sources_truly_changed=5"
    );
    assert!(
        t2_ms < GATE_FIVE_FILE_MS,
        "5-file edit took {t2_ms}ms, gate is {GATE_FIVE_FILE_MS}ms — Tier 1 regression"
    );

    // ── Trial 3: 47-file edit ─────────────────────────────────────
    // Mirrors the screenshot the user shared — a representative
    // many-file incremental compile.
    edit_files(&root, 47, "T3");
    let t3_start = Instant::now();
    let t3 = compile_once(&root)
        .await
        .expect("trial 3 compile must succeed");
    let t3_ms = t3_start.elapsed().as_millis();
    eprintln!(
        "[bench] INCREMENTAL 47-file edit: {t3_ms}ms — phase_timings: {:?}",
        t3.incremental_summary.phase_timings
    );
    assert_eq!(
        t3.incremental_summary.sources_truly_changed, 47,
        "47-file edit must report sources_truly_changed=47"
    );
    assert!(
        t3_ms < GATE_MANY_FILE_MS,
        "47-file edit took {t3_ms}ms, gate is {GATE_MANY_FILE_MS}ms — Tier 1 regression"
    );

    // ── Trial 4: no-op recompile ──────────────────────────────────
    // Identical recompile must take the early-return path. Provides a
    // floor measurement for "the pipeline overhead when nothing changed".
    let t4_start = Instant::now();
    let t4 = compile_once(&root)
        .await
        .expect("trial 4 (no-op) compile must succeed");
    let t4_ms = t4_start.elapsed().as_millis();
    eprintln!("[bench] NO-OP recompile: {t4_ms}ms");
    assert_eq!(
        t4.incremental_summary.sources_truly_changed, 0,
        "no-op recompile must report sources_truly_changed=0"
    );
    // No-op should be fast — under 1s on any modern hardware. This is
    // the early-return guarantee from pipeline.rs:1083.
    assert!(
        t4_ms < 1_000,
        "no-op recompile took {t4_ms}ms, gate is 1000ms — early-return path may be broken"
    );

    eprintln!(
        "[bench] PASS: tier1 acceptance gates met. \
         baseline={baseline_ms}ms, 1-file={t1_ms}ms, 5-file={t2_ms}ms, 47-file={t3_ms}ms, no-op={t4_ms}ms"
    );
}
