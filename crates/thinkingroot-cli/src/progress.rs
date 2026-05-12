//! Unified compile-progress display for `root compile`.
//!
//! Renders a **single** `indicatif` bar driven by the daemon's
//! `CompileTick` snapshots (one every 250 ms). The bar's label updates
//! as the pipeline transitions between user-facing steps (`Reading` →
//! `Extracting` → `Linking` → `Persisting` → `Linking` → `Persisting`
//! → ...); when the active step has a known total, the bar shows a
//! counted progress bar with ETA; otherwise it falls back to a spinner
//! with elapsed-only readout.
//!
//! On clean exit the bar finishes green with the total elapsed; on
//! `PipelineFailed` it finishes red with the error message.
//!
//! All other `ProgressEvent` variants (`ChunkDone`, `WitnessMeshDone`,
//! `EntityResolved`, …) are ignored — they're kept on the wire only
//! for back-compat with consumers that haven't migrated to
//! `CompileTick` yet. New code should match exclusively on
//! `CompileTick` + `IncrementalDone` + `PipelineFailed`.
//!
//! Only used in TTY mode. Non-TTY and `--verbose` paths skip this
//! entirely.

use std::path::Path;

use anyhow::Context as _;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use thinkingroot_core::CompileStep;

use crate::pipeline::{PipelineOptions, PipelineResult, ProgressEvent, run_pipeline_with_options};

/// Drain a `ProgressEvent` stream and render it as a live single-bar
/// display. Returns when the channel closes.
///
/// Used by both compile paths so neither can drift from the other:
///   * In-process (`run_compile_progress`): the pipeline sends events
///     directly into the channel.
///   * Remote (`cortex_remote::run_compile_remote`): the SSE consumer
///     parses each `progress` event back into a typed `ProgressEvent`
///     and forwards it.
///
/// `mp` is exposed so callers that print their own banner above the
/// bar can keep the layout coherent (otherwise indicatif and the raw
/// `println!` lines fight for the cursor).
pub async fn drive_progress_bars(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ProgressEvent>,
    mp: MultiProgress,
) {
    // Lazy bar — created on the first CompileTick so we don't paint
    // anything if the pipeline errors before emitting one snapshot.
    let mut bar: Option<ProgressBar> = None;
    let mut current_step: Option<CompileStep> = None;
    let mut errored = false;
    let mut error_msg: Option<String> = None;
    let mut finished_cleanly = false;

    while let Some(event) = rx.recv().await {
        match event {
            // ── The one canonical progress event ─────────────────────
            ProgressEvent::CompileTick(tick) => {
                let pb = bar.get_or_insert_with(|| {
                    let pb = mp.add(ProgressBar::new_spinner());
                    pb.set_style(active_spinner_style());
                    pb.set_prefix(format!("{:<11}", tick.step.label()));
                    pb.set_message("starting…".to_string());
                    pb.enable_steady_tick(std::time::Duration::from_millis(80));
                    pb
                });

                // Step transition — relabel the bar prefix and reset
                // any prior counted-bar length so the new step's
                // 0/N reads correctly.
                if current_step != Some(tick.step) {
                    current_step = Some(tick.step);
                    pb.set_prefix(format!("{:<11}", tick.step.label()));
                }

                // Choose the active style based on whether we have a
                // known total. Counted bar + ETA when yes; spinner +
                // elapsed when no.
                if tick.total > 0 {
                    pb.set_style(active_bar_style());
                    pb.set_length(tick.total);
                    pb.set_position(tick.done);
                } else {
                    pb.set_style(active_spinner_style());
                }

                pb.set_message(format_tick_message(&tick));
            }

            // ── Terminal events ──────────────────────────────────────
            ProgressEvent::IncrementalDone { summary } => {
                finished_cleanly = true;
                if let Some(ref pb) = bar {
                    let total_secs = summary.total_elapsed_ms as f64 / 1000.0;
                    let msg = format!(
                        "{} sources · {} claims · {} entities · {} relations  {}",
                        style(summary.sources_total).white(),
                        style(summary.claims_added).white(),
                        style(summary.structural_extractions).dim(),
                        style(summary.structural_rows_emitted).dim(),
                        style(format!("{total_secs:.1}s total")).dim(),
                    );
                    pb.set_style(done_style());
                    pb.set_prefix(format!("{:<11}", "Done"));
                    pb.finish_with_message(msg);
                }
            }

            ProgressEvent::PipelineFailed { error } => {
                errored = true;
                error_msg = Some(error);
                // Don't break yet — wait for channel close so any
                // late CompileTick still arrives in order.
            }

            // ── Legacy events (deliberately ignored) ────────────────
            // These keep flowing on the wire for back-compat with the
            // pre-CompileTick desktop component and any older daemon
            // releases. Drop them silently.
            _ => {}
        }
    }

    // Channel closed — finalise the bar based on whether the pipeline
    // ended cleanly, errored, or just disconnected mid-stream.
    if let Some(pb) = bar {
        if errored {
            pb.set_style(failed_style());
            pb.set_prefix(format!("{:<11}", "Failed"));
            pb.finish_with_message(
                error_msg
                    .map(|m| style(m).red().to_string())
                    .unwrap_or_else(|| style("pipeline error").red().to_string()),
            );
        } else if !finished_cleanly {
            pb.set_style(skipped_style());
            pb.finish_with_message(style("—").dim().to_string());
        }
        // If finished_cleanly, the IncrementalDone arm already did finish_with_message.
    }
}

/// Run the pipeline with a live progress display (in-process path).
///
/// Returns the same `PipelineResult` as `run_pipeline`. Callers print
/// their own pre/post output (banner, summary) — this function only
/// drives the bar.
///
/// The bar driver is a separate spawned task — NOT `tokio::join!`
/// with the pipeline. Long synchronous operations in the pipeline
/// (`upsert_batch`, CozoDB writes) would otherwise block the same
/// task and starve the bar of redraws.
pub async fn run_compile_progress(
    root_path: &Path,
    branch: Option<&str>,
    no_rooting: bool,
) -> anyhow::Result<PipelineResult> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let mp = MultiProgress::new();
    let driver_handle = tokio::task::spawn(drive_progress_bars(rx, mp));
    let pipeline_result = run_pipeline_with_options(
        root_path,
        branch,
        Some(tx),
        PipelineOptions {
            no_rooting,
            ..Default::default()
        },
    )
    .await;
    // tx drops here → channel closes → driver's rx.recv() returns None → driver exits.
    let _ = driver_handle.await;

    // Blank line after the bar for visual breathing room before the
    // summary table.
    eprintln!();

    pipeline_result.context("pipeline failed")
}

// ── Message rendering ───────────────────────────────────────────────

/// Render the bar's right-hand message line from a `CompileTick`.
/// Format depends on what's known:
///   * `total > 0` + `eta_ms.is_some()` → `"done/total  3.2s  ETA 1m2s"`
///   * `total > 0` + no ETA              → `"done/total  3.2s"`
///   * `total == 0`                      → `"3.2s"`
fn format_tick_message(tick: &thinkingroot_core::CompileTick) -> String {
    let elapsed_s = tick.step_elapsed_ms as f64 / 1000.0;
    let elapsed_str = style(format_secs(elapsed_s)).dim().to_string();
    match (tick.total, tick.eta_ms) {
        (0, _) => elapsed_str,
        (_, Some(eta_ms)) => {
            let eta_s = eta_ms as f64 / 1000.0;
            format!(
                "{} / {}  {}  ETA {}",
                tick.done,
                tick.total,
                elapsed_str,
                style(format_secs(eta_s)).dim(),
            )
        }
        (_, None) => format!("{} / {}  {}", tick.done, tick.total, elapsed_str),
    }
}

/// Human-friendly duration formatter for short displays.
/// Examples: `"0.4s"`, `"42s"`, `"2m18s"`, `"1h03m"`.
fn format_secs(s: f64) -> String {
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 60.0 {
        format!("{:.0}s", s.round())
    } else if s < 3600.0 {
        let m = (s / 60.0).floor() as u64;
        let r = (s - (m as f64) * 60.0).round() as u64;
        format!("{m}m{r:02}s")
    } else {
        let h = (s / 3600.0).floor() as u64;
        let m = ((s - (h as f64) * 3600.0) / 60.0).round() as u64;
        format!("{h}h{m:02}m")
    }
}

// ── Style definitions ───────────────────────────────────────────────

fn active_spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.cyan} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn active_bar_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template("  {spinner:.cyan} {prefix} [{bar:30.cyan/white.dim}] {msg}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_secs_handles_sub_minute_and_minutes_and_hours() {
        assert_eq!(format_secs(0.4), "0.4s");
        assert_eq!(format_secs(9.9), "9.9s");
        assert_eq!(format_secs(10.0), "10s");
        assert_eq!(format_secs(42.0), "42s");
        assert_eq!(format_secs(60.0), "1m00s");
        assert_eq!(format_secs(138.0), "2m18s");
        assert_eq!(format_secs(3780.0), "1h03m");
    }

    #[test]
    fn format_tick_message_spinner_mode_shows_only_elapsed() {
        let tick = thinkingroot_core::CompileTick {
            step: CompileStep::Linking,
            done: 0,
            total: 0,
            step_elapsed_ms: 4500,
            total_elapsed_ms: 12000,
            eta_ms: None,
            detail: None,
        };
        let msg = format_tick_message(&tick);
        // Spinner mode: just the elapsed. Strip ANSI for the assertion.
        let plain = console::strip_ansi_codes(&msg).to_string();
        assert_eq!(plain, "4.5s");
    }

    #[test]
    fn format_tick_message_counted_with_eta_shows_full_breakdown() {
        let tick = thinkingroot_core::CompileTick {
            step: CompileStep::Reading,
            done: 250,
            total: 548,
            step_elapsed_ms: 1100,
            total_elapsed_ms: 1100,
            eta_ms: Some(1310),
            detail: None,
        };
        let plain = console::strip_ansi_codes(&format_tick_message(&tick)).to_string();
        assert_eq!(plain, "250 / 548  1.1s  ETA 1.3s");
    }
}
