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

use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::pipeline::{run_pipeline, ProgressEvent, PipelineResult};

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
            let mut extract_bar:     Option<ProgressBar> = None;
            let mut grounding_bar:   Option<ProgressBar> = None;
            let mut fingerprint_bar: Option<ProgressBar> = None;
            let mut link_bar:        Option<ProgressBar> = None;
            let mut index_bar:       Option<ProgressBar> = None;
            let mut compile_bar:     Option<ProgressBar> = None;
            let mut verify_bar:      Option<ProgressBar> = None;

            let mut extract_start:  Option<Instant> = None;
            let mut ground_start:   Option<Instant> = None;
            let mut link_start:     Option<Instant> = None;
            let mut index_start:    Option<Instant> = None;
            let mut compile_start:  Option<Instant> = None;
            let mut verify_start:   Option<Instant> = None;

            while let Some(event) = rx.recv().await {
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

                        // Spawn extract bar.
                        let eb = mp.add(new_bar("Extracting"));
                        activate_spinner(&eb, "waiting for LLM...");
                        extract_start = Some(Instant::now());
                        extract_bar = Some(eb);
                    }

                    // ── Extraction ──────────────────────────────────────
                    ProgressEvent::ExtractionStart { total_chunks } => {
                        if let Some(ref eb) = extract_bar {
                            if total_chunks > 0 {
                                eb.set_length(total_chunks as u64);
                                eb.set_position(0);
                                eb.set_style(active_bar_style());
                                eb.enable_steady_tick(std::time::Duration::from_millis(80));
                            }
                        }
                    }

                    ProgressEvent::ChunkDone { done, total, source_uri } => {
                        if let Some(ref eb) = extract_bar {
                            if total > 0 {
                                eb.set_length(total as u64);
                            }
                            eb.set_position(done as u64);
                            eb.set_message(format!("↳ {}", uri_basename(&source_uri)));
                        }
                    }

                    ProgressEvent::ExtractionComplete { claims, entities, cache_hits } => {
                        if let Some(ref eb) = extract_bar {
                            let elapsed = extract_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
                            let total = eb.length().unwrap_or(0) as usize;
                            let cache_note = if cache_hits > 0 && total > 0 {
                                let pct = cache_hits * 100 / total;
                                format!("  {}", style(format!("({cache_hits} cached, {pct}% saved)")).dim())
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
                    ProgressEvent::GroundingStart { llm_claims, structural_claims } => {
                        let gb = mp.add(new_bar("Grounding"));
                        if llm_claims > 0 {
                            activate_spinner(
                                &gb,
                                &format!(
                                    "{} LLM claims → tribunal, {} structural auto-grounded",
                                    llm_claims, structural_claims
                                ),
                            );
                        } else {
                            activate_spinner(
                                &gb,
                                &format!("{} structural claims auto-grounded", structural_claims),
                            );
                        }
                        ground_start = Some(Instant::now());
                        grounding_bar = Some(gb);
                    }

                    ProgressEvent::GroundingDone { accepted, rejected } => {
                        if let Some(ref gb) = grounding_bar {
                            let elapsed = ground_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
                            let reject_note = if rejected > 0 {
                                format!("  {}", style(format!("({rejected} rejected)")).yellow())
                            } else {
                                String::new()
                            };
                            finish_bar(
                                gb,
                                &format!(
                                    "{} claims accepted{}  {}",
                                    style(accepted).white(),
                                    reject_note,
                                    style(format!("{:.1}s", elapsed)).dim(),
                                ),
                            );
                        }
                    }

                    // ── Fingerprint ─────────────────────────────────────
                    ProgressEvent::FingerprintDone { truly_changed, cutoffs } => {
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
                        if let Some(ref lb) = link_bar {
                            if total_entities > 0 {
                                lb.set_message(format!("0/{total_entities} entities"));
                            }
                        }
                    }

                    ProgressEvent::EntityResolved { done, total } => {
                        if let Some(ref lb) = link_bar {
                            lb.set_message(format!("{done}/{total} entities"));
                        }
                    }

                    ProgressEvent::LinkComplete { entities, relations, contradictions } => {
                        if let Some(ref lb) = link_bar {
                            let elapsed = link_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
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
                    ProgressEvent::VectorUpdateDone { entities_indexed, claims_indexed } => {
                        let ib = mp.add(new_bar("Indexing"));
                        let elapsed = index_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
                        finish_bar(
                            &ib,
                            &format!(
                                "{} entities · {} claims  {}",
                                style(entities_indexed).white(),
                                style(claims_indexed).white(),
                                style(format!("{:.1}s", elapsed)).dim(),
                            ),
                        );
                        index_bar = Some(ib);

                        // Spawn compile bar.
                        let cb = mp.add(new_bar("Compiling"));
                        activate_spinner(&cb, "generating artifacts...");
                        compile_start = Some(Instant::now());
                        compile_bar = Some(cb);
                    }

                    // ── Compilation ─────────────────────────────────────
                    ProgressEvent::CompilationDone { artifacts } => {
                        // If compile bar wasn't spawned yet (VectorUpdateDone
                        // was skipped on early-exit paths), create it now.
                        if compile_bar.is_none() {
                            let cb = mp.add(new_bar("Compiling"));
                            compile_start = Some(Instant::now());
                            compile_bar = Some(cb);
                        }
                        if let Some(ref cb) = compile_bar {
                            let elapsed = compile_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
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
                            let elapsed = verify_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
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
    let (pipeline_result, ()) = tokio::join!(
        run_pipeline(root_path, branch, Some(tx)),
        bar_driver,
    );

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
        .template(
            "  {spinner:.cyan} {prefix} [{bar:30.cyan/white.dim}] {pos}/{len}  {msg}",
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
