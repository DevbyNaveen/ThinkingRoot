//! Phased progress display for `root compile`.
//!
//! Drives a set of `indicatif` bars from the `ProgressEvent` stream emitted by
//! the pipeline. Every bar is created **lazily** when its phase begins — there
//! is no ghost line cluttering the terminal before a phase runs, and every
//! bar's elapsed time reflects only the work of that phase.
//!
//! Bar lifecycle: (not yet visible) → active spinner / counted bar → finished.
//! On pipeline error, any bar still in flight when the channel closes is
//! finalised with a red ✗ "failed" marker (`failed_style`). On clean exit,
//! unfinished bars finalise as a dim ─ "skipped" (`skipped_style`).
//!
//! Phase mapping (pipeline event → bar):
//!   ParseStart / ParseComplete             →  Parsing
//!   DiffStart / DiffComplete               →  Diffing
//!   ExtractionStart / …Done                →  Extracting
//!   GroundingStart / …Done                 →  Grounding
//!   FingerprintDone (cutoffs > 0 only)     →  Fingerprint
//!   RootingStart / …Done (claims > 0 only) →  Rooting
//!   LinkingStart / LinkComplete            →  Linking
//!   VectorProgress / VectorUpdateDone      →  Indexing
//!   CompilationProgress / CompilationDone  →  Compiling
//!   VerificationDone                       →  Verifying
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

    // All bars — including Parsing — are created lazily inside the driver as
    // each phase begins. This way every bar's elapsed time reflects only the
    // work of that phase, never the gap between two events.

    // ── Bar driver ──────────────────────────────────────────────────────────
    let bar_driver = {
        let mp = mp.clone();
        async move {
            let mut parse_bar: Option<ProgressBar> = None;
            let mut diffing_bar: Option<ProgressBar> = None;
            let mut extract_bar: Option<ProgressBar> = None;
            let mut grounding_bar: Option<ProgressBar> = None;
            let mut fingerprint_bar: Option<ProgressBar> = None;
            let mut rooting_bar: Option<ProgressBar> = None;
            let mut link_bar: Option<ProgressBar> = None;
            let mut index_bar: Option<ProgressBar> = None;
            let mut compile_bar: Option<ProgressBar> = None;
            let mut verify_bar: Option<ProgressBar> = None;

            let mut parse_start: Option<Instant> = None;
            let mut diff_start: Option<Instant> = None;
            let mut extract_start: Option<Instant> = None;
            let mut extract_total_chunks: usize = 0;
            let mut extract_real_done: usize = 0;
            let mut extract_last_source: Option<String> = None;
            let mut extract_active_batches: VecDeque<ActiveExtractionBatch> = VecDeque::new();
            let mut extract_completed_batch_secs: Vec<f64> = Vec::new();
            let mut ground_start: Option<Instant> = None;
            let mut root_start: Option<Instant> = None;
            let mut link_start: Option<Instant> = None;
            let mut index_start: Option<Instant> = None;
            let mut compile_start: Option<Instant> = None;
            let mut verify_start: Option<Instant> = None;

            // Set when `PipelineFailed` arrives so the cleanup loop can render
            // unfinished bars in red ✗ instead of the ambiguous dim ─.
            let mut pipeline_errored = false;

            let mut extract_tick = tokio::time::interval(std::time::Duration::from_millis(250));
            extract_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = extract_tick.tick() => {
                        // Only refresh while extraction is in flight. Without
                        // the `is_finished` guard the tick would keep mutating
                        // the bar after `ExtractionComplete` ran `finish_bar`,
                        // overwriting the solid finish line with stale ETA /
                        // batch text on the next MultiProgress redraw.
                        if let Some(ref eb) = extract_bar
                            && !eb.is_finished()
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
                    ProgressEvent::ParseStart => {
                        let pb = mp.add(new_bar("Parsing"));
                        activate_spinner(&pb, "scanning files...");
                        parse_start = Some(Instant::now());
                        parse_bar = Some(pb);
                    }

                    ProgressEvent::ParseComplete { files } => {
                        if let Some(ref pb) = parse_bar {
                            let elapsed = parse_start
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            finish_bar(
                                pb,
                                &format!(
                                    "{}  {}",
                                    style(format!("{files} files")).white(),
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }
                        // The Extracting bar is created lazily on
                        // `ExtractionStart` — the gap between Parse and
                        // Extract is now covered by the Diffing bar.
                    }

                    // ── Diff (storage init + content-hash scan + graph priming) ─
                    ProgressEvent::DiffStart => {
                        let db = mp.add(new_bar("Diffing"));
                        activate_spinner(&db, "comparing graph state...");
                        diff_start = Some(Instant::now());
                        diffing_bar = Some(db);
                    }

                    ProgressEvent::DiffComplete {
                        changed,
                        unchanged,
                        deleted,
                    } => {
                        if let Some(ref db) = diffing_bar {
                            let elapsed = diff_start
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            let mut parts = vec![
                                style(format!("{changed} changed")).white().to_string(),
                                style(format!("{unchanged} unchanged")).dim().to_string(),
                            ];
                            if deleted > 0 {
                                parts.push(
                                    style(format!("{deleted} deleted")).yellow().to_string(),
                                );
                            }
                            finish_bar(
                                db,
                                &format!(
                                    "{}  {}",
                                    parts.join(" · "),
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }
                    }

                    // ── Extraction ──────────────────────────────────────
                    ProgressEvent::ExtractionStart {
                        total_chunks,
                        batch_size,
                        total_batches,
                    } => {
                        let eb = mp.add(new_bar("Extracting"));
                        extract_start = Some(Instant::now());
                        extract_total_chunks = total_chunks;
                        extract_real_done = 0;
                        extract_last_source = None;
                        extract_active_batches.clear();
                        extract_completed_batch_secs.clear();

                        if total_chunks > 0 {
                            eb.set_length(total_chunks as u64);
                            eb.set_position(0);
                            eb.set_style(active_bar_elapsed_style());
                            eb.set_message(format!(
                                "batch size {} · {} batches queued",
                                style(batch_size).white(),
                                style(total_batches).white(),
                            ));
                            eb.enable_steady_tick(std::time::Duration::from_millis(80));
                        } else {
                            // No work to extract — show briefly as a spinner
                            // until `ExtractionComplete` finalises it. This
                            // happens when every chunk hit the cache.
                            activate_spinner(&eb, "cache hits only");
                        }
                        extract_bar = Some(eb);
                    }

                    ProgressEvent::ExtractionBatchStart {
                        batch_index,
                        total_batches,
                        range_start: _,
                        range_end: _,
                        batch_chunks,
                    } => {
                        extract_active_batches.push_back(ActiveExtractionBatch {
                            batch_index,
                            total_batches,
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
                        // Only show a bar if fingerprint skipped something —
                        // otherwise the phase has no user-visible signal.
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
                        // The Linking bar is now created lazily on
                        // `LinkingStart`. Between Fingerprint and Linking the
                        // pipeline may run Rooting (its own bar) and a few
                        // fast graph mutations.
                    }

                    // ── Linking ─────────────────────────────────────────
                    ProgressEvent::LinkingStart { total_entities } => {
                        let lb = mp.add(new_bar("Linking"));
                        link_start = Some(Instant::now());
                        if total_entities > 0 {
                            lb.set_length(total_entities as u64);
                            lb.set_position(0);
                            lb.set_style(active_bar_style());
                            lb.enable_steady_tick(std::time::Duration::from_millis(80));
                            lb.set_message("entities".to_string());
                        } else {
                            activate_spinner(&lb, "resolving entities...");
                        }
                        link_bar = Some(lb);
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

                    // ── Rooting (Phase 6.5) ────────────────────────────
                    ProgressEvent::RootingStart { candidates } => {
                        let rb = mp.add(new_bar("Rooting"));
                        root_start = Some(Instant::now());
                        if candidates > 0 {
                            rb.set_length(candidates as u64);
                            rb.set_position(0);
                            rb.set_style(active_bar_style());
                            rb.enable_steady_tick(std::time::Duration::from_millis(80));
                            rb.set_message("admission probes".to_string());
                        } else {
                            activate_spinner(&rb, "admission probes...");
                        }
                        rooting_bar = Some(rb);
                    }
                    ProgressEvent::RootingProgress { done, total } => {
                        if let Some(ref rb) = rooting_bar {
                            // Upgrade spinner → counted bar on first progress
                            // event when `RootingStart` had no candidate count.
                            if rb.length().is_none() {
                                rb.set_style(active_bar_style());
                                rb.enable_steady_tick(std::time::Duration::from_millis(80));
                            }
                            rb.set_length(total as u64);
                            rb.set_position(done as u64);
                        }
                    }
                    ProgressEvent::RootingDone {
                        rooted,
                        attested,
                        quarantined,
                        rejected,
                    } => {
                        if let Some(ref rb) = rooting_bar {
                            let elapsed = root_start
                                .map_or(0.0, |t| t.elapsed().as_secs_f64());
                            let mut parts =
                                vec![style(format!("{rooted} rooted")).green().to_string()];
                            if attested > 0 {
                                parts.push(
                                    style(format!("{attested} attested")).white().to_string(),
                                );
                            }
                            if quarantined > 0 {
                                parts.push(
                                    style(format!("{quarantined} quarantined"))
                                        .yellow()
                                        .to_string(),
                                );
                            }
                            if rejected > 0 {
                                parts.push(
                                    style(format!("{rejected} rejected")).red().to_string(),
                                );
                            }
                            finish_bar(
                                rb,
                                &format!(
                                    "{}  {}",
                                    parts.join(" · "),
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }
                    }

                    ProgressEvent::ExtractionPartial { failed_batches: _, failed_chunk_ranges: _ } => {
                        // Pipeline summary at the end of run_compile already
                        // prints the warning unconditionally.  No mid-bar
                        // render here — keeping the bar layout clean.
                    }

                    ProgressEvent::PipelineFailed { error: _ } => {
                        // Don't render here — the cleanup loop after the channel
                        // closes will paint every unfinished bar with
                        // `failed_style()` instead of the dim-skipped style.
                        pipeline_errored = true;
                    }
                        }
                    }
                }
            }

            // Channel closed — pipeline finished. Finalize any bars that were
            // spawned but never received their completion events. The visual
            // signal differs based on `pipeline_errored`:
            //   * errored = true  → red ✗ + "failed" message
            //   * errored = false → dim ─ "—" (legitimately skipped phase)
            // Without this distinction, a crashed phase and a correctly-skipped
            // phase render identically.
            let all_bars: Vec<&ProgressBar> = [
                parse_bar.as_ref(),
                diffing_bar.as_ref(),
                extract_bar.as_ref(),
                grounding_bar.as_ref(),
                fingerprint_bar.as_ref(),
                rooting_bar.as_ref(),
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
                    if pipeline_errored {
                        bar.set_style(failed_style());
                        bar.finish_with_message(style("failed").red().to_string());
                    } else {
                        bar.set_style(skipped_style());
                        bar.finish_with_message(style("—").dim().to_string());
                    }
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

fn failed_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.red} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["✗", "✗"])
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

/// Exponentially-weighted moving average of completed batch durations.
///
/// A simple arithmetic mean is dragged upward by the (always-slow) first batch
/// — model warm-up, ONNX session creation, cache miss — and stays pessimistic
/// for the rest of the run. EWMA with α = 0.3 down-weights early samples as
/// fresh measurements arrive, so ETA tracks real velocity.
///
/// Falls back to 90 s for an empty sample (a defensible "first batch" default
/// based on observed median LLM batch latency in this pipeline) and clamps to
/// at least 10 s to avoid divide-by-near-zero in the projection above.
fn ewma_expected_batch_secs(samples: &[f64]) -> f64 {
    const ALPHA: f64 = 0.3;
    if samples.is_empty() {
        return 90.0;
    }
    let mut state = samples[0];
    for &s in &samples[1..] {
        state = ALPHA * s + (1.0 - ALPHA) * state;
    }
    state.max(10.0)
}

fn estimated_extract_done(
    real_done: usize,
    active_batches: &VecDeque<ActiveExtractionBatch>,
    completed_batch_secs: &[f64],
) -> usize {
    let expected_batch_secs = ewma_expected_batch_secs(completed_batch_secs);

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

/// Render the set of currently-running extraction batches for the bar message.
///
/// Up to three batch indices are shown explicitly; remaining batches collapse
/// into a `+N more` tail. Empty input → empty string (caller suppresses the
/// segment cleanly).
fn format_active_batches(active: &VecDeque<ActiveExtractionBatch>) -> String {
    if active.is_empty() {
        return String::new();
    }
    const MAX_SHOW: usize = 3;
    let total = active[0].total_batches;
    let mut shown: Vec<String> = active
        .iter()
        .take(MAX_SHOW)
        .map(|b| format!("{}/{}", b.batch_index, total))
        .collect();
    if active.len() > MAX_SHOW {
        shown.push(format!("+{} more", active.len() - MAX_SHOW));
    }
    if active.len() == 1 {
        format!("batch {}", shown[0])
    } else {
        format!("running batches {}", shown.join(" · "))
    }
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

    let context = format_active_batches(active_batches);
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── EWMA ────────────────────────────────────────────────────────────────

    #[test]
    fn ewma_empty_falls_back_to_default() {
        // 90 s default mirrors the historical median LLM-batch latency.
        assert!((ewma_expected_batch_secs(&[]) - 90.0).abs() < 1e-9);
    }

    #[test]
    fn ewma_single_sample_returns_that_sample_clamped() {
        // One observation → that observation; clamp floor is 10 s.
        assert!((ewma_expected_batch_secs(&[42.0]) - 42.0).abs() < 1e-9);
        assert!((ewma_expected_batch_secs(&[3.0]) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn ewma_eventually_tracks_recent_regime_after_cold_start() {
        // Cold-start batch is 120 s; 20 subsequent batches stabilise at 30 s.
        // After enough samples in the new regime, EWMA should decay close to
        // 30 s (the steady-state value), unlike a simple arithmetic mean which
        // remains permanently biased upward by the cold sample.
        let mut samples = vec![120.0];
        samples.extend(std::iter::repeat_n(30.0, 20));
        let ewma = ewma_expected_batch_secs(&samples);
        let mean: f64 = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!(
            ewma < 35.0,
            "ewma {ewma} should have tracked the 30s steady state"
        );
        assert!(
            ewma < mean,
            "ewma {ewma} should be below the bias-anchored mean {mean}"
        );
    }

    #[test]
    fn ewma_short_run_still_dominated_by_initial_sample() {
        // Documents the trade-off: with α = 0.3, three follow-up samples
        // aren't enough to fully decay a cold start. The function clamps to
        // a sensible upper estimate rather than overreacting on tiny samples.
        let ewma = ewma_expected_batch_secs(&[120.0, 30.0, 30.0, 30.0]);
        assert!(
            ewma > 50.0 && ewma < 80.0,
            "with α=0.3 and 3 trailing samples, ewma {ewma} should sit between mean (52.5) and cold start (120)"
        );
    }

    #[test]
    fn ewma_floor_is_ten_seconds() {
        // Pathologically fast batches still clamp to 10 s so the divisor in
        // `estimated_extract_done` never approaches zero.
        let ewma = ewma_expected_batch_secs(&[0.5, 0.5, 0.5]);
        assert!((ewma - 10.0).abs() < 1e-9);
    }

    // ── format_active_batches ──────────────────────────────────────────────

    fn make_batch(idx: usize, total: usize) -> ActiveExtractionBatch {
        ActiveExtractionBatch {
            batch_index: idx,
            total_batches: total,
            batch_chunks: 1,
            started_at: Instant::now(),
            accounted_done: 0,
        }
    }

    #[test]
    fn format_active_batches_empty_returns_empty_string() {
        assert_eq!(format_active_batches(&VecDeque::new()), "");
    }

    #[test]
    fn format_active_batches_singular() {
        let mut q = VecDeque::new();
        q.push_back(make_batch(2, 5));
        assert_eq!(format_active_batches(&q), "batch 2/5");
    }

    #[test]
    fn format_active_batches_two_batches_uses_running_phrase() {
        let mut q = VecDeque::new();
        q.push_back(make_batch(1, 10));
        q.push_back(make_batch(2, 10));
        assert_eq!(format_active_batches(&q), "running batches 1/10 · 2/10");
    }

    #[test]
    fn format_active_batches_caps_at_three_with_overflow_tail() {
        // Five concurrent batches → show 3, summarise the rest.
        let mut q = VecDeque::new();
        for i in 1..=5 {
            q.push_back(make_batch(i, 12));
        }
        assert_eq!(
            format_active_batches(&q),
            "running batches 1/12 · 2/12 · 3/12 · +2 more"
        );
    }
}
