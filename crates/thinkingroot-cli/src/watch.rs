use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEventKind, DebounceEventResult, new_debouncer};

/// Options for the `run_watch_loop` driver.
#[derive(Debug, Clone, Copy)]
pub struct WatchOptions {
    /// Quiet-window in milliseconds.  File events arriving within
    /// `debounce_ms` of each other are collapsed into a single batch.
    pub debounce_ms: u64,
    /// Maximum number of compile ticks before the loop exits.  `None`
    /// means run forever (normal production use).  `Some(n)` is a
    /// test-only circuit-breaker so tests can drive the loop without
    /// sending SIGINT.
    pub max_ticks: Option<usize>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            debounce_ms: 200,
            max_ticks: None,
        }
    }
}

/// Returns `true` for paths the watcher should **ignore** (noise).
///
/// Excludes:
/// - `.git/`, `.thinkingroot/`, `target/`, `node_modules/`, `.next/`,
///   `dist/`, `build/`, `__pycache__/`, `.tox/`, `.venv/`
/// - Dotfiles (final component starts with `.`)
/// - Editor swap files: `.swp`, `.swo`, `.swx`, `~`-suffixed, `.tmp`,
///   `.bak`, `4913` (vim pre-write probe)
pub fn is_noise(p: &Path) -> bool {
    const NOISE_DIRS: &[&str] = &[
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

    for component in p.components() {
        let s = component.as_os_str();
        if NOISE_DIRS.iter().any(|&d| s == d) {
            return true;
        }
    }

    let Some(file_name) = p.file_name() else {
        return true;
    };
    let name = file_name.to_string_lossy();

    if name.starts_with('.') {
        return true;
    }

    if name == "4913" {
        return true;
    }

    if name.ends_with('~') {
        return true;
    }

    const NOISE_EXTENSIONS: &[&str] = &["swp", "swo", "swx", "tmp", "bak"];
    if let Some(ext) = p.extension() {
        let ext = ext.to_string_lossy();
        if NOISE_EXTENSIONS.iter().any(|&e| ext == e) {
            return true;
        }
    }

    false
}

/// Run the watch loop until `max_ticks` is reached or the channel closes.
///
/// In production (`max_ticks: None`) the loop runs until the watcher is
/// dropped (e.g. process exit via Ctrl-C).  Each filesystem batch is
/// debounced into a single call to `compile_fn`.  The loop awaits the
/// future to completion BEFORE processing the next batch, giving
/// single-writer behaviour by sequential construction: no two compiles are
/// ever in flight simultaneously.
///
/// Errors from `compile_fn` are logged and printed to stderr; the loop
/// continues so a compile failure does not tear down the watcher.
///
/// The implementation uses a `tokio::sync::mpsc` unbounded channel bridged
/// from the synchronous debouncer callback, so no blocking thread is held
/// while waiting for filesystem events.
pub async fn run_watch_loop<F, Fut>(
    workspace_root: PathBuf,
    options: WatchOptions,
    mut compile_fn: F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<PathBuf>) -> Fut + Send,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DebounceEventResult>();

    let mut debouncer = new_debouncer(
        Duration::from_millis(options.debounce_ms),
        move |result: DebounceEventResult| {
            // Ignore send errors — they only happen when the receiver is dropped
            // (i.e. the loop exited due to max_ticks).
            let _ = tx.send(result);
        },
    )?;

    debouncer
        .watcher()
        .watch(&workspace_root, RecursiveMode::Recursive)?;

    let mut ticks = 0usize;

    loop {
        if options.max_ticks.is_some_and(|max| ticks >= max) {
            break;
        }

        let result = rx.recv().await;

        match result {
            Some(Ok(events)) => {
                let relevant: Vec<PathBuf> = events
                    .into_iter()
                    .filter(|e| e.kind == DebouncedEventKind::Any && !is_noise(&e.path))
                    .map(|e| e.path)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();

                if relevant.is_empty() {
                    continue;
                }

                let count = relevant.len();
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "[{ts}] {count} file{} changed (debounced {}ms) — recompiling...",
                    if count == 1 { "" } else { "s" },
                    options.debounce_ms
                );

                ticks += 1;

                if let Err(e) = compile_fn(relevant).await {
                    tracing::error!("compile failed: {e:#}");
                    eprintln!("[watch] compile error: {e:#}");
                }
            }
            Some(Err(e)) => {
                tracing::warn!("watch error: {e:?}");
            }
            None => {
                // Channel closed — debouncer dropped.
                break;
            }
        }
    }

    Ok(())
}
