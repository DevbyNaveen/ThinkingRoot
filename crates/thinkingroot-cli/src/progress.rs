//! 8-phase progress display for `root compile`.
//!
//! Drives eight `indicatif` bars driven by `ProgressEvent`s from the pipeline.
//! Each bar transitions: (not yet visible) → active spinner → solidified (done).
//!
//! Bars are added to `MultiProgress` on demand — only when their phase begins.
//! This avoids the "ghost line" problem where dim `○ waiting...` bars clutter
//! the terminal before they're relevant.
//!
//! Phase mapping (pipeline → bars):
//!   1. Parse             →  Parsing
//!   2. Extract (LLM)     →  Extracting
//!   3. Grounding tribunal→  Grounding
//!   4. Fingerprint check →  Fingerprint  (shown only when cutoffs > 0)
//!   5. Link              →  Linking
//!   6. Vector index      →  Indexing
//!   7. Compile artifacts →  Compiling
//!   8. Verify health     →  Verifying
//!
//! Only used in TTY mode. Non-TTY and --verbose paths skip this entirely.

use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::pipeline::{PipelineResult, ProgressEvent, run_pipeline};

#[derive(Debug, Clone)]
struct ActiveExtractionBatch {
    batch_index: usize,
    total_batches: usize,
    range_start: usize,
    range_end: usize,
    batch_chunks: usize,
    started_at: Instant,
    accounted_done: usize,
}

/// Run the pipeline with a live progress display.
///
/// Returns the same `PipelineResult` as `run_pipeline`. Callers print their
/// own pre/post output (banner, summary) — this function only drives the bars.
pub async fn run_compile_progress(
    root_path: &Path,
    branch: Option<&str>,
) -> anyhow::Result<PipelineResult> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();

    let mp = MultiProgress::new();

    // Parse is the only bar created upfront — it starts immediately.
    let parse_bar = mp.add(new_bar("Parsing"));
    activate_spinner(&parse_bar, "scanning files...");
    let parse_start = Instant::now();

    // All other bars are created on demand inside the driver.

    // ── Bar driver ──────────────────────────────────────────────────────────
    let bar_driver = {
        let mp = mp.clone();
        async move {
            let mut extract_bar: Option<ProgressBar> = None;
            let mut grounding_bar: Option<ProgressBar> = None;
            let mut fingerprint_bar: Option<ProgressBar> = None;
            let mut link_bar: Option<ProgressBar> = None;
            let mut index_bar: Option<ProgressBar> = None;
            let mut compile_bar: Option<ProgressBar> = None;
            let mut verify_bar: Option<ProgressBar> = None;

            let mut extract_start: Option<Instant> = None;
            let mut extract_total_chunks: usize = 0;
            let mut extract_real_done: usize = 0;
            let mut extract_last_source: Option<String> = None;
            let mut extract_active_batches: VecDeque<ActiveExtractionBatch> = VecDeque::new();
            let mut extract_completed_batch_secs: Vec<f64> = Vec::new();
            let mut ground_start: Option<Instant> = None;
            let mut link_start: Option<Instant> = None;
            let mut index_start: Option<Instant> = None;
            let mut compile_start: Option<Instant> = None;
            let mut verify_start: Option<Instant> = None;

            let mut extract_tick = tokio::time::interval(std::time::Duration::from_millis(250));
            extract_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = extract_tick.tick() => {
                        if let Some(ref eb) = extract_bar
                            && extract_total_chunks > 0
                        {
                            refresh_extract_bar(
                                eb,
                                extract_start,
                                extract_total_chunks,
                                extract_real_done,
                                &extract_active_batches,
                                &extract_completed_batch_secs,
                                extract_last_source.as_deref(),
                            );
                        }
                    }
                    maybe_event = rx.recv() => {
                        let Some(event) = maybe_event else {
                            break;
                        };
                        match event {
                    // ── Parse ───────────────────────────────────────────
                    ProgressEvent::ParseComplete { files } => {
                        finish_bar(
                            &parse_bar,
                            &format!(
                                "{}  {}",
                                style(format!("{files} files")).white(),
                                style(format!("{:.1}s", parse_start.elapsed().as_secs_f64())).dim(),
                            ),
                        );

                        // Spawn extract bar — use elapsed-aware style so users can
                        // distinguish a slow-but-alive LLM call from a genuine hang.
                        let eb = mp.add(new_bar("Extracting"));
                        activate_llm_wait_spinner(&eb);
                        extract_start = Some(Instant::now());
                        extract_bar = Some(eb);
                    }

                    // ── Extraction ──────────────────────────────────────
                    ProgressEvent::ExtractionStart {
                        total_chunks,
                        batch_size,
                        total_batches,
                    } => {
                        extract_total_chunks = total_chunks;
                        extract_real_done = 0;
                        extract_last_source = None;
                        extract_active_batches.clear();
                        extract_completed_batch_secs.clear();
                        if let Some(ref eb) = extract_bar
                            && total_chunks > 0
                        {
                            eb.set_length(total_chunks as u64);
                            eb.set_position(0);
                            eb.set_style(active_bar_elapsed_style());
                            let batch_note = if total_batches > 0 {
                                format!(
                                    "batch size {}  {} batches queued",
                                    style(batch_size).white(),
                                    style(total_batches).white()
                                )
                            } else {
                                "cache hits only".to_string()
                            };
                            eb.set_message(batch_note);
                            eb.enable_steady_tick(std::time::Duration::from_millis(80));
                        }
                    }

                    ProgressEvent::ExtractionBatchStart {
                        batch_index,
                        total_batches,
                        range_start,
                        range_end,
                        batch_chunks,
                    } => {
                        extract_active_batches.push_back(ActiveExtractionBatch {
                            batch_index,
                            total_batches,
                            range_start,
                            range_end,
                            batch_chunks,
                            started_at: Instant::now(),
                            accounted_done: 0,
                        });
                        if let Some(ref eb) = extract_bar {
                            refresh_extract_bar(
                                eb,
                                extract_start,
                                extract_total_chunks,
                                extract_real_done,
                                &extract_active_batches,
                                &extract_completed_batch_secs,
                                extract_last_source.as_deref(),
                            );
                        }
                    }

                    ProgressEvent::ChunkDone {
                        done,
                        total,
                        source_uri,
                    } => {
                        let delta = done.saturating_sub(extract_real_done);
                        extract_real_done = done;
                        if !source_uri.is_empty() {
                            extract_last_source = Some(source_uri.clone());
                        }
                        let mut remaining = delta;
                        for batch in &mut extract_active_batches {
                            if remaining == 0 {
                                break;
                            }
                            let batch_remaining =
                                batch.batch_chunks.saturating_sub(batch.accounted_done);
                            let consumed = batch_remaining.min(remaining);
                            batch.accounted_done += consumed;
                            remaining -= consumed;
                        }
                        let now = Instant::now();
                        let mut idx = 0;
                        while idx < extract_active_batches.len() {
                            if extract_active_batches[idx].accounted_done
                                >= extract_active_batches[idx].batch_chunks
                            {
                                let completed = extract_active_batches.remove(idx).expect("index checked");
                                extract_completed_batch_secs
                                    .push(now.duration_since(completed.started_at).as_secs_f64());
                            } else {
                                idx += 1;
                            }
                        }
                        if let Some(ref eb) = extract_bar {
                            if total > 0 {
                                eb.set_length(total as u64);
                            }
                            refresh_extract_bar(
                                eb,
                                extract_start,
                                extract_total_chunks,
                                extract_real_done,
                                &extract_active_batches,
                                &extract_completed_batch_secs,
                                extract_last_source.as_deref(),
                            );
                        }
                    }

                    ProgressEvent::ExtractionComplete {
                        claims,
                        entities,
                        cache_hits,
                    } => {
                        if let Some(ref eb) = extract_bar {
                            let elapsed = extract_start
                                .as_ref()
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            let total = eb.length().unwrap_or(0) as usize;
                            let cache_note = if cache_hits > 0 && total > 0 {
                                let pct = cache_hits * 100 / total;
                                format!(
                                    "  {}",
                                    style(format!("({cache_hits} cached, {pct}% saved)")).dim()
                                )
                            } else {
                                String::new()
                            };
                            finish_bar(
                                eb,
                                &format!(
                                    "{} claims · {} entities{}  {}",
                                    style(claims).white(),
                                    style(entities).white(),
                                    cache_note,
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }
                        // Grounding bar will be spawned by GroundingStart.
                    }

                    // ── Grounding ───────────────────────────────────────
                    ProgressEvent::GroundingStart {
                        llm_claims,
                        structural_claims,
                    } => {
                        let gb = mp.add(new_bar("Grounding"));
                        if llm_claims > 0 {
                            // Immediately show a real counted bar so users see
                            // 0/N from the very start — NLI batches are slow
                            // (30-60 s each on CPU) so the first GroundingProgress
                            // event can take minutes.  Without this the bar just
                            // spins with no indication of how much work remains.
                            gb.set_length(llm_claims as u64);
                            gb.set_position(0);
                            gb.set_style(active_bar_style());
                            gb.enable_steady_tick(std::time::Duration::from_millis(80));
                            let struct_note = if structural_claims > 0 {
                                format!(
                                    "  {} structural auto-grounded",
                                    style(structural_claims).dim()
                                )
                            } else {
                                String::new()
                            };
                            gb.set_message(format!("NLI tribunal{struct_note}"));
                        } else {
                            activate_spinner(
                                &gb,
                                &format!("{} structural claims auto-grounded", structural_claims),
                            );
                        }
                        ground_start = Some(Instant::now());
                        grounding_bar = Some(gb);
                    }

                    ProgressEvent::GroundingModelReady => {
                        if let Some(ref gb) = grounding_bar {
                            gb.set_message("NLI tribunal  running…".to_string());
                        }
                    }

                    ProgressEvent::GroundingProgress { done, total } => {
                        if let Some(ref gb) = grounding_bar {
                            // If GroundingStart wasn't received first (shouldn't
                            // happen), fall back to switching from spinner here.
                            if gb.length().is_none() {
                                gb.set_length(total as u64);
                                gb.set_position(0);
                                gb.set_style(active_bar_style());
                                gb.enable_steady_tick(std::time::Duration::from_millis(80));
                            }
                            gb.set_length(total as u64);
                            gb.set_position(done as u64);
                            let elapsed = ground_start
                                .as_ref()
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            gb.set_message(format!(
                                "NLI tribunal  {}",
                                style(format!("{elapsed:.0}s")).dim()
                            ));
                        }
                    }

                    ProgressEvent::GroundingDone { accepted, rejected } => {
                        if let Some(ref gb) = grounding_bar {
                            let elapsed = ground_start
                                .as_ref()
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            let reject_note = if rejected > 0 {
                                format!("  {}", style(format!("({rejected} rejected)")).yellow())
                            } else {
                                String::new()
                            };
                            finish_bar(
                                gb,
                                &format!(
                                    "{} accepted{}  {}",
                                    style(accepted).white(),
                                    reject_note,
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }
                    }

                    // ── Fingerprint ─────────────────────────────────────
                    ProgressEvent::FingerprintDone {
                        truly_changed,
                        cutoffs,
                    } => {
                        // Only show bar if fingerprint actually skipped something.
                        if cutoffs > 0 {
                            let fb = mp.add(new_bar("Fingerprint"));
                            finish_bar(
                                &fb,
                                &format!(
                                    "{} changed, {} {}",
                                    style(truly_changed).white(),
                                    style(cutoffs).cyan(),
                                    style("unchanged (skipped)").dim(),
                                ),
                            );
                            fingerprint_bar = Some(fb);
                        }

                        // Spawn link bar — linking is the next phase.
                        let lb = mp.add(new_bar("Linking"));
                        activate_spinner(&lb, "resolving entities...");
                        link_start = Some(Instant::now());
                        link_bar = Some(lb);
                    }

                    // ── Linking ─────────────────────────────────────────
                    ProgressEvent::LinkingStart { total_entities } => {
                        if let Some(ref lb) = link_bar && total_entities > 0 {
                            // Switch from spinner to real counted bar immediately.
                            lb.set_length(total_entities as u64);
                            lb.set_position(0);
                            lb.set_style(active_bar_style());
                            lb.enable_steady_tick(std::time::Duration::from_millis(80));
                            lb.set_message("entities".to_string());
                        }
                    }

                    ProgressEvent::EntityResolved { done, total: _ } => {
                        if let Some(ref lb) = link_bar {
                            lb.set_position(done as u64);
                            lb.set_message("entities".to_string());
                        }
                    }

                    ProgressEvent::LinkComplete {
                        entities,
                        relations,
                        contradictions,
                    } => {
                        if let Some(ref lb) = link_bar {
                            let elapsed = link_start
                                .as_ref()
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            let contra_note = if contradictions > 0 {
                                format!(
                                    "  {}",
                                    style(format!("· {contradictions} contradictions")).yellow()
                                )
                            } else {
                                String::new()
                            };
                            finish_bar(
                                lb,
                                &format!(
                                    "{} entities · {} relations{}  {}",
                                    style(entities).white(),
                                    style(relations).white(),
                                    contra_note,
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }
                        // Vector update runs next — start its timer.
                        index_start = Some(Instant::now());
                    }

                    // ── Vector indexing ─────────────────────────────────
                    ProgressEvent::VectorProgress { done, total } => {
                        // Create index bar on first event.
                        if index_bar.is_none() {
                            let ib = mp.add(new_bar("Indexing"));
                            ib.set_length(total as u64);
                            ib.set_position(0);
                            ib.set_style(active_bar_style());
                            ib.enable_steady_tick(std::time::Duration::from_millis(80));
                            index_start = Some(Instant::now());
                            index_bar = Some(ib);
                        }
                        if let Some(ref ib) = index_bar {
                            ib.set_length(total as u64);
                            ib.set_position(done as u64);
                            let elapsed = index_start
                                .as_ref()
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            ib.set_message(format!(
                                "embedding  {}",
                                style(format!("{elapsed:.0}s")).dim()
                            ));
                        }
                    }

                    ProgressEvent::VectorUpdateDone {
                        entities_indexed,
                        claims_indexed,
                    } => {
                        let elapsed = index_start
                            .as_ref()
                            .map_or(0.0, |t| t.elapsed().as_secs_f64());
                        let summary = format!(
                            "{} entities · {} claims  {}",
                            style(entities_indexed).white(),
                            style(claims_indexed).white(),
                            style(format!("{:.1}s", elapsed)).dim(),
                        );
                        if let Some(ref ib) = index_bar {
                            // Bar was driven by VectorProgress — just finish it.
                            finish_bar(ib, &summary);
                        } else {
                            // No VectorProgress fired (empty index) — flash create + finish.
                            let ib = mp.add(new_bar("Indexing"));
                            finish_bar(&ib, &summary);
                            index_bar = Some(ib);
                        }

                        // Spawn compile bar.
                        let cb = mp.add(new_bar("Compiling"));
                        activate_spinner(&cb, "generating artifacts...");
                        compile_start = Some(Instant::now());
                        compile_bar = Some(cb);
                    }

                    // ── Compilation ─────────────────────────────────────
                    ProgressEvent::CompilationProgress { done, total } => {
                        // Ensure bar exists (may not exist on early-exit paths).
                        if compile_bar.is_none() {
                            let cb = mp.add(new_bar("Compiling"));
                            compile_start = Some(Instant::now());
                            compile_bar = Some(cb);
                        }
                        if let Some(ref cb) = compile_bar {
                            // First progress event: switch spinner → real bar.
                            if cb.length().is_none() {
                                cb.set_length(total as u64);
                                cb.set_position(0);
                                cb.set_style(active_bar_style());
                                cb.enable_steady_tick(std::time::Duration::from_millis(80));
                            }
                            cb.set_length(total as u64);
                            cb.set_position(done as u64);
                            cb.set_message("artifacts".to_string());
                        }
                    }

                    ProgressEvent::CompilationDone { artifacts } => {
                        // If compile bar wasn't spawned yet (VectorUpdateDone
                        // was skipped on early-exit paths), create it now.
                        if compile_bar.is_none() {
                            let cb = mp.add(new_bar("Compiling"));
                            compile_start = Some(Instant::now());
                            compile_bar = Some(cb);
                        }
                        if let Some(ref cb) = compile_bar {
                            let elapsed = compile_start
                                .as_ref()
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            finish_bar(
                                cb,
                                &format!(
                                    "{} artifacts  {}",
                                    style(artifacts).white(),
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }

                        // Spawn verify bar.
                        let vb = mp.add(new_bar("Verifying"));
                        activate_spinner(&vb, "checking health...");
                        verify_start = Some(Instant::now());
                        verify_bar = Some(vb);
                    }

                    // ── Verification ────────────────────────────────────
                    ProgressEvent::VerificationDone { health } => {
                        if let Some(ref vb) = verify_bar {
                            let elapsed = verify_start
                                .as_ref()
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            let health_str = if health >= 80 {
                                style(format!("Health {health}%")).green().to_string()
                            } else if health >= 60 {
                                style(format!("Health {health}%")).yellow().to_string()
                            } else {
                                style(format!("Health {health}%")).red().to_string()
                            };
                            finish_bar(
                                vb,
                                &format!(
                                    "{}  {}",
                                    health_str,
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }
                    }
                        }
                    }
                }
            }

            // Channel closed — pipeline finished. Finalize any bars that were
            // spawned but never received their completion events.
            let all_bars: Vec<&ProgressBar> = [
                Some(&parse_bar),
                extract_bar.as_ref(),
                grounding_bar.as_ref(),
                fingerprint_bar.as_ref(),
                link_bar.as_ref(),
                index_bar.as_ref(),
                compile_bar.as_ref(),
                verify_bar.as_ref(),
            ]
            .into_iter()
            .flatten()
            .collect();

            for bar in all_bars {
                if !bar.is_finished() {
                    bar.set_style(skipped_style());
                    bar.finish_with_message(style("—").dim().to_string());
                }
            }
        }
    };

    // ── Run pipeline and driver concurrently ───────────────────────────────
    // bar_driver must be a separate spawned task — NOT tokio::join! with the pipeline.
    //
    // Why: grounder.ground() and upsert_batch() are long synchronous operations that
    // block the tokio worker thread.  tokio::join! runs both futures in the same task
    // (same thread), so when the pipeline blocks, bar_driver never gets polled and
    // progress events pile up in the channel unseen.
    //
    // spawn() makes bar_driver a fully independent task scheduled on any free thread,
    // so it keeps draining the channel even while the pipeline thread is blocked.
    let driver_handle = tokio::task::spawn(bar_driver);
    let pipeline_result = run_pipeline(root_path, branch, Some(tx)).await;
    // tx drops here → channel closes → driver's rx.recv() returns None → driver exits.
    let _ = driver_handle.await;

    // Blank line after the bars for visual breathing room.
    eprintln!();

    pipeline_result.context("pipeline failed")
}

// ── Bar lifecycle helpers ───────────────────────────────────────────────────

fn new_bar(prefix: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(waiting_style());
    pb.set_prefix(format!("{prefix:<11}"));
    pb.tick();
    pb
}

fn activate_spinner(bar: &ProgressBar, msg: &str) {
    bar.set_style(active_spinner_style());
    bar.set_message(msg.to_string());
    bar.enable_steady_tick(std::time::Duration::from_millis(80));
}

/// Spinner that shows elapsed time so users can distinguish a slow LLM call from a hang.
/// After ~5s the hint "use --verbose to see logs" appears.
fn activate_llm_wait_spinner(bar: &ProgressBar) {
    bar.set_style(llm_wait_style());
    bar.set_message("waiting for LLM...".to_string());
    bar.enable_steady_tick(std::time::Duration::from_millis(80));
}

fn finish_bar(bar: &ProgressBar, msg: &str) {
    bar.set_style(done_style());
    bar.finish_with_message(msg.to_string());
}

// ── Style definitions ────────────────────────────────────────────────────────

fn waiting_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.dim} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["○", "○"])
}

fn active_spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.cyan} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn llm_wait_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.cyan} {prefix} {msg}  {elapsed}")
        .expect("static template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn active_bar_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template("  {spinner:.cyan} {prefix} [{bar:30.cyan/white.dim}] {pos}/{len}  {msg}")
        .expect("static template is valid")
        .progress_chars("█░")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn active_bar_elapsed_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(
            "  {spinner:.cyan} {prefix} [{bar:30.cyan/white.dim}] {pos}/{len}  {msg}  {elapsed}",
        )
        .expect("static template is valid")
        .progress_chars("█░")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn done_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.green} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["✓", "✓"])
}

fn skipped_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.dim} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["─", "─"])
}

// ── Utility ──────────────────────────────────────────────────────────────────

/// Extract the last path component for display (e.g. "src/auth/service.rs" → "service.rs").
fn uri_basename(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

fn format_eta(total_secs: u64) -> String {
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn estimated_extract_done(
    real_done: usize,
    active_batches: &VecDeque<ActiveExtractionBatch>,
    completed_batch_secs: &[f64],
) -> usize {
    let expected_batch_secs = if completed_batch_secs.is_empty() {
        90.0
    } else {
        completed_batch_secs.iter().sum::<f64>() / completed_batch_secs.len() as f64
    }
    .max(10.0);

    let mut estimated = real_done;
    for batch in active_batches {
        let elapsed = batch.started_at.elapsed().as_secs_f64();
        let est_for_batch =
            ((elapsed / expected_batch_secs) * batch.batch_chunks as f64).floor() as usize;
        let additional = est_for_batch
            .saturating_sub(batch.accounted_done)
            .min(batch.batch_chunks.saturating_sub(batch.accounted_done));
        estimated += additional;
    }
    estimated
}

fn refresh_extract_bar(
    bar: &ProgressBar,
    extract_start: Option<Instant>,
    total_chunks: usize,
    real_done: usize,
    active_batches: &VecDeque<ActiveExtractionBatch>,
    completed_batch_secs: &[f64],
    last_source: Option<&str>,
) {
    if total_chunks == 0 {
        return;
    }

    let mut estimated_done =
        estimated_extract_done(real_done, active_batches, completed_batch_secs)
            .min(total_chunks)
            .max(real_done);
    if real_done < total_chunks && estimated_done >= total_chunks {
        estimated_done = total_chunks.saturating_sub(1).max(real_done);
    }
    bar.set_length(total_chunks as u64);
    bar.set_position(estimated_done as u64);

    let elapsed = extract_start.map_or(0.0, |t| t.elapsed().as_secs_f64());
    let rate = if elapsed > 0.0 {
        estimated_done as f64 / elapsed
    } else {
        0.0
    };
    let eta_secs = if rate > 0.0 && total_chunks > estimated_done {
        ((total_chunks - estimated_done) as f64 / rate).round() as u64
    } else {
        0
    };
    let estimated = estimated_done > real_done;

    let context = if let Some(batch) = active_batches.back() {
        format!(
            "batch {}/{}  files {}-{}  ({} files)",
            batch.batch_index,
            batch.total_batches,
            batch.range_start,
            batch.range_end,
            batch.batch_chunks
        )
    } else {
        String::new()
    };
    let source = last_source
        .filter(|s| !s.is_empty())
        .map(|s| format!("↳ {}  ", uri_basename(s)))
        .unwrap_or_default();
    let speed = if rate > 0.0 {
        if estimated {
            format!("{rate:.1} files/s est")
        } else {
            format!("{rate:.1} files/s")
        }
    } else {
        "warming up".to_string()
    };
    let eta = if eta_secs > 0 {
        format!("  ETA {}", format_eta(eta_secs))
    } else {
        String::new()
    };

    let mut parts = Vec::new();
    if !source.is_empty() {
        parts.push(source.trim_end().to_string());
    }
    if !context.is_empty() {
        parts.push(context);
    }
    parts.push(speed);
    if !eta.is_empty() {
        parts.push(eta.trim().to_string());
    }
    bar.set_message(parts.join("  "));
}
