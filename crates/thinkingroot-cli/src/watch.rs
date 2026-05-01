use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use console::style;
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};

use thinkingroot_core::config::Config;

use crate::pipeline;

/// Build a gitignore-aware matcher from the workspace's `exclude_patterns`
/// and `.gitignore` rules. Returns a closure that returns `true` for paths
/// the watcher should **ignore** (i.e. noise).
///
/// The matcher also defends against symlink loops: a path whose
/// canonicalised form escapes the workspace root (or whose
/// canonicalisation fails) is treated as ignore. This stops the
/// classic inotify spin where a symlink inside the watched tree
/// points back at an ancestor — `notify`'s recursive watcher crawls
/// the cycle, fires events forever, saturates the event queue, and
/// floods stderr with errors.
fn build_ignore_matcher(root: &Path, config: &Config) -> impl Fn(&Path) -> bool {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);

    // Load .gitignore if respect_gitignore is enabled.
    if config.parsers.respect_gitignore {
        let gitignore_path = root.join(".gitignore");
        if gitignore_path.exists() {
            let _ = builder.add(&gitignore_path);
        }
    }

    // Add the config's exclude_patterns as additional ignore rules.
    for pattern in &config.parsers.exclude_patterns {
        let _ = builder.add_line(None, pattern);
    }

    let gitignore = builder.build().unwrap_or_else(|_| {
        ignore::gitignore::GitignoreBuilder::new(root)
            .build()
            .expect("empty gitignore builder must succeed")
    });

    // Built-in noise directories that should always be ignored, regardless of
    // config — these never contain user-authored source files.
    const ALWAYS_IGNORE: &[&str] = &[
        ".thinkingroot",
        ".git",
        "target",
        "node_modules",
        ".next",
        "dist",
        "build",
        "__pycache__",
        ".tox",
        ".venv",
    ];

    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    move |path: &Path| {
        // Fast path: check path components against the built-in blocklist.
        for component in path.components() {
            let name = component.as_os_str();
            if ALWAYS_IGNORE.iter().any(|&blocked| name == blocked) {
                return true;
            }
        }

        // Symlink-loop guard: any event coming from a path that
        // doesn't canonically live inside the workspace root is
        // either a cyclic-symlink artefact or filesystem noise from
        // a sibling directory. Either way it isn't a real source
        // change worth recompiling for. Treating canonicalisation
        // failure as ignore is safer than treating it as include
        // (per the audit's "walker errors mean skip" rule).
        match path.canonicalize() {
            Ok(canon) if canon.starts_with(&root_canon) => {}
            _ => return true,
        }

        // Check against gitignore + config exclude_patterns.
        gitignore
            .matched_path_or_any_parents(path, path.is_dir())
            .is_ignore()
    }
}

/// Watch a directory for changes and run incremental compilation.
/// Debounces file events with a 500ms window before triggering a compile.
/// Respects `.gitignore` and `exclude_patterns` from config — only reacts
/// to files the parser would actually process.
pub async fn run_watch(root_path: &Path) -> anyhow::Result<()> {
    let config = Config::load_merged(root_path)?;
    let should_ignore = build_ignore_matcher(root_path, &config);

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

    // Set up file watcher with 500ms debounce (up from 300ms to reduce noise).
    let (tx, rx) = mpsc::channel();
    let rx = Arc::new(Mutex::new(rx));
    let mut debouncer = new_debouncer(Duration::from_millis(500), tx)?;

    debouncer
        .watcher()
        .watch(root_path, notify::RecursiveMode::Recursive)?;

    println!("  {} waiting for changes...\n", style("--").dim());

    loop {
        let rx_clone = Arc::clone(&rx);
        let recv_result =
            tokio::task::spawn_blocking(move || rx_clone.lock().unwrap().recv()).await?;

        match recv_result {
            Ok(Ok(events)) => {
                let relevant: Vec<_> = events
                    .iter()
                    .filter(|e| e.kind == DebouncedEventKind::Any && !should_ignore(&e.path))
                    .collect();

                if relevant.is_empty() {
                    continue;
                }

                let changed_count = relevant.len();
                let sample = relevant
                    .first()
                    .map(|e| {
                        e.path
                            .strip_prefix(root_path)
                            .unwrap_or(&e.path)
                            .display()
                            .to_string()
                    })
                    .unwrap_or_default();

                let extra = if changed_count > 1 {
                    format!(" (+{} more)", changed_count - 1)
                } else {
                    String::new()
                };

                println!(
                    "  {} {}{}",
                    style(">>").cyan().bold(),
                    style(&sample).white(),
                    style(&extra).dim(),
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

                println!("  {} waiting for changes...\n", style("--").dim());
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
