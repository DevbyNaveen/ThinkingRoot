use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use console::style;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

use crate::pipeline;

/// Watch a directory for changes and run incremental compilation.
/// Debounces file events with a 300ms window before triggering a compile.
pub async fn run_watch(root_path: &Path) -> anyhow::Result<()> {
    println!(
        "\n  {} watching {} for changes (Ctrl+C to stop)\n",
        style("ThinkingRoot").green().bold(),
        style(root_path.display()).white()
    );

    // Initial compile.
    println!("  {} initial compile...", style(">>").cyan().bold());
    let start = Instant::now();
    match pipeline::run_pipeline(root_path, None, None).await {
        Ok(result) => {
            println!(
                "  {} compiled {} files in {:.1}s (health: {}%)\n",
                style("OK").green().bold(),
                result.files_parsed,
                start.elapsed().as_secs_f64(),
                result.health_score,
            );
        }
        Err(e) => {
            println!("  {} {e}\n", style("ERR").red().bold());
        }
    }

    // Set up file watcher with 300ms debounce.
    let (tx, rx) = mpsc::channel();
    let rx = Arc::new(Mutex::new(rx));
    let mut debouncer = new_debouncer(Duration::from_millis(300), tx)?;

    debouncer
        .watcher()
        .watch(root_path, notify::RecursiveMode::Recursive)?;

    println!(
        "  {} waiting for changes...\n",
        style("--").dim()
    );

    loop {
        // Move the blocking recv off the async executor's worker threads.
        let rx_clone = Arc::clone(&rx);
        let recv_result = tokio::task::spawn_blocking(move || rx_clone.lock().unwrap().recv())
            .await?;

        match recv_result {
            Ok(Ok(events)) => {
                // Filter out events inside .thinkingroot/ using exact component matching
                // to avoid false-positives when a parent dir contains ".thinkingroot".
                let relevant: Vec<_> = events
                    .iter()
                    .filter(|e| {
                        e.kind == DebouncedEventKind::Any
                            && !e.path.components().any(|c| c.as_os_str() == ".thinkingroot")
                    })
                    .collect();

                if relevant.is_empty() {
                    continue;
                }

                let changed_count = relevant.len();
                println!(
                    "  {} {} file(s) changed, recompiling...",
                    style(">>").cyan().bold(),
                    changed_count,
                );

                let start = Instant::now();
                match pipeline::run_pipeline(root_path, None, None).await {
                    Ok(result) => {
                        println!(
                            "  {} {:.1}s | {} claims, {} entities, health {}%\n",
                            style("OK").green().bold(),
                            start.elapsed().as_secs_f64(),
                            result.claims_count,
                            result.entities_count,
                            result.health_score,
                        );
                    }
                    Err(e) => {
                        println!("  {} {e}\n", style("ERR").red().bold());
                    }
                }

                println!(
                    "  {} waiting for changes...\n",
                    style("--").dim()
                );
            }
            Ok(Err(e)) => {
                eprintln!("  {} watch error: {e}", style("ERR").red().bold());
                tracing::warn!("watch error: {e:?}");
            }
            Err(e) => {
                tracing::error!("watcher channel closed: {e}");
                break;
            }
        }
    }

    Ok(())
}
