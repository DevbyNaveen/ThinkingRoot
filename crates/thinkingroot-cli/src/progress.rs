//! World-class 5-phase progress display for `root compile`.
//!
//! Drives five `indicatif` phase bars driven by `ProgressEvent`s from the
//! pipeline. Each bar transitions: waiting → active → solidified (done).
//!
//! Only used in TTY mode. Non-TTY and --verbose paths skip this entirely.

use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::pipeline::{run_pipeline, ProgressEvent, PipelineResult};

/// Run the pipeline with a live 5-phase progress display.
///
/// Returns the same `PipelineResult` as `run_pipeline`. Callers print their
/// own pre/post output (banner, summary) — this function only drives the bars.
pub async fn run_compile_progress(
    root_path: &Path,
    branch: Option<&str>,
) -> anyhow::Result<PipelineResult> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();

    let mp = MultiProgress::new();

    // Five fixed-position bars in pipeline order.
    let parse_bar   = mp.add(new_waiting_bar("Parsing"));
    let extract_bar = mp.add(new_waiting_bar("Extracting"));
    let link_bar    = mp.add(new_waiting_bar("Linking"));
    let compile_bar = mp.add(new_waiting_bar("Compiling"));
    let verify_bar  = mp.add(new_waiting_bar("Verifying"));

    // Parse starts immediately — activate its spinner before the pipeline even begins.
    activate_spinner(&parse_bar, "scanning files...");

    // Phase timers (parse_start is set here; others are set as events arrive).
    let parse_start = Instant::now();

    // Clone bar handles for the driver closure.
    let (pb, eb, lb, cb, vb) = (
        parse_bar.clone(),
        extract_bar.clone(),
        link_bar.clone(),
        compile_bar.clone(),
        verify_bar.clone(),
    );

    // ── Bar driver ──────────────────────────────────────────────────────────
    // Runs concurrently with the pipeline via tokio::join!.
    // Receives ProgressEvents and updates bars. Exits when the channel closes
    // (pipeline future completes and drops the sender).
    let bar_driver = async move {
        // Phase timers: set to Some(Instant::now()) when each phase activates.
        // None means the phase was skipped (early-exit path) — shows "0.0s".
        let mut extract_start: Option<Instant> = None;
        let mut link_start:    Option<Instant> = None;
        let mut compile_start: Option<Instant> = None;
        let mut verify_start:  Option<Instant> = None;

        while let Some(event) = rx.recv().await {
            match event {
                // ── Parse ───────────────────────────────────────────────
                ProgressEvent::ParseComplete { files } => {
                    finish_bar(
                        &pb,
                        &format!(
                            "{}  {}",
                            style(format!("{files} files")).white(),
                            style(format!("{:.1}s", parse_start.elapsed().as_secs_f64())).dim(),
                        ),
                    );
                    extract_start = Some(Instant::now());
                    // Activate extract as spinner until ExtractionStart arrives.
                    activate_spinner(&eb, "waiting for LLM...");
                }

                // ── Extraction ──────────────────────────────────────────
                ProgressEvent::ExtractionStart { total_chunks } => {
                    if total_chunks > 0 {
                        // Length MUST be set before style — the 80ms redraw timer
                        // can fire between the two calls, and {bar:30} panics on len=0.
                        eb.set_length(total_chunks as u64);
                        eb.set_position(0);
                        eb.set_style(active_bar_style());
                        eb.enable_steady_tick(std::time::Duration::from_millis(80));
                    }
                }

                ProgressEvent::ChunkDone { done, total, source_uri } => {
                    if total > 0 {
                        eb.set_length(total as u64);
                    }
                    eb.set_position(done as u64);
                    eb.set_message(format!("↳ {}", uri_basename(&source_uri)));
                }

                ProgressEvent::ExtractionComplete { claims, entities, cache_hits } => {
                    let elapsed_secs = extract_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
                    let total = eb.length().unwrap_or(0) as usize;
                    let cache_note = if cache_hits > 0 && total > 0 {
                        let pct = cache_hits * 100 / total;
                        format!("  {}", style(format!("({cache_hits} cached, {pct}% saved)")).dim())
                    } else {
                        String::new()
                    };
                    finish_bar(
                        &eb,
                        &format!(
                            "{} claims · {} entities{}  {}",
                            style(claims).white(),
                            style(entities).white(),
                            cache_note,
                            style(format!("{:.1}s", elapsed_secs)).dim(),
                        ),
                    );
                    link_start = Some(Instant::now());
                    activate_spinner(&lb, "resolving entities...");
                }

                // ── Linking ─────────────────────────────────────────────
                ProgressEvent::LinkingStart { total_entities } => {
                    if total_entities > 0 {
                        lb.set_message(format!("0/{total_entities} entities"));
                    }
                }

                ProgressEvent::EntityResolved { done, total } => {
                    lb.set_message(format!("{done}/{total} entities"));
                }

                ProgressEvent::LinkComplete { entities, relations, contradictions } => {
                    let elapsed_secs = link_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
                    let contra_note = if contradictions > 0 {
                        format!(
                            "  {}",
                            style(format!("· {contradictions} contradictions")).yellow()
                        )
                    } else {
                        String::new()
                    };
                    finish_bar(
                        &lb,
                        &format!(
                            "{} entities · {} relations{}  {}",
                            style(entities).white(),
                            style(relations).white(),
                            contra_note,
                            style(format!("{:.1}s", elapsed_secs)).dim(),
                        ),
                    );
                    compile_start = Some(Instant::now());
                    activate_spinner(&cb, "generating artifacts...");
                }

                // ── Compilation ─────────────────────────────────────────
                ProgressEvent::CompilationDone { artifacts } => {
                    let elapsed_secs = compile_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
                    finish_bar(
                        &cb,
                        &format!(
                            "{} artifacts  {}",
                            style(artifacts).white(),
                            style(format!("{:.1}s", elapsed_secs)).dim(),
                        ),
                    );
                    verify_start = Some(Instant::now());
                    activate_spinner(&vb, "checking health...");
                }

                // ── Verification ────────────────────────────────────────
                ProgressEvent::VerificationDone { health } => {
                    let elapsed_secs = verify_start.as_ref().map_or(0.0, |t| t.elapsed().as_secs_f64());
                    let health_str = if health >= 80 {
                        style(format!("Health {health}%")).green().to_string()
                    } else if health >= 60 {
                        style(format!("Health {health}%")).yellow().to_string()
                    } else {
                        style(format!("Health {health}%")).red().to_string()
                    };
                    finish_bar(
                        &vb,
                        &format!(
                            "{}  {}",
                            health_str,
                            style(format!("{:.1}s", elapsed_secs)).dim(),
                        ),
                    );
                }
            }
        }

        // Channel closed — pipeline finished. Finalize any bars that never
        // received their events (early-exit paths: nothing changed, etc.).
        for bar in [&pb, &eb, &lb, &cb, &vb] {
            if !bar.is_finished() {
                bar.set_style(skipped_style());
                bar.finish_with_message(style("—").dim().to_string());
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

fn new_waiting_bar(prefix: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(waiting_style());
    pb.set_prefix(format!("{prefix:<11}"));
    pb.set_message(style("waiting...").dim().to_string());
    pb.tick(); // Render the initial waiting state immediately.
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
        .tick_strings(&["○", "○"]) // indicatif uses last entry as "done" state; need ≥2
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
        .tick_strings(&["✓", "✓"]) // last entry = done state; need ≥2
}

fn skipped_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.dim} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["─", "─"]) // last entry = done state; need ≥2
}

// ── Utility ──────────────────────────────────────────────────────────────────

/// Extract the last path component for display (e.g. "src/auth/service.rs" → "service.rs").
fn uri_basename(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}
