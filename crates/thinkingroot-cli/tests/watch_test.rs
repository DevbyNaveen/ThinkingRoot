use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serial_test::serial;
use thinkingroot_cli::watch::{WatchOptions, is_noise, run_watch_loop};

// ── Unit tests for the is_noise filter (no filesystem / watcher involved) ──

#[test]
fn is_noise_rejects_thinkingroot_dir() {
    assert!(is_noise(std::path::Path::new(".thinkingroot/graph.db")));
    assert!(is_noise(std::path::Path::new(".git/COMMIT_EDITMSG")));
    assert!(is_noise(std::path::Path::new("target/debug/root")));
    assert!(is_noise(std::path::Path::new("node_modules/.cache/foo")));
}

#[test]
fn is_noise_rejects_dotfiles() {
    assert!(is_noise(std::path::Path::new(".hidden_file")));
    assert!(is_noise(std::path::Path::new(".env")));
    assert!(is_noise(std::path::Path::new(".DS_Store")));
}

#[test]
fn is_noise_rejects_editor_swap_files() {
    assert!(is_noise(std::path::Path::new("notes.md.swp")));
    assert!(is_noise(std::path::Path::new("notes.md.swo")));
    assert!(is_noise(std::path::Path::new("notes.md.swx")));
    assert!(is_noise(std::path::Path::new("notes.md~")));
    assert!(is_noise(std::path::Path::new("notes.md.tmp")));
    assert!(is_noise(std::path::Path::new("notes.md.bak")));
    assert!(is_noise(std::path::Path::new("4913")));
}

#[test]
fn is_noise_passes_real_source_files() {
    assert!(!is_noise(std::path::Path::new("main.rs")));
    assert!(!is_noise(std::path::Path::new("README.md")));
    assert!(!is_noise(std::path::Path::new("src/lib.rs")));
    assert!(!is_noise(std::path::Path::new("docs/design.md")));
    assert!(!is_noise(std::path::Path::new("config.toml")));
}

// ── Integration tests for run_watch_loop ───────────────────────────────────
//
// These tests create real filesystem watchers via notify-rs / FSEvents (macOS).
// On macOS, concurrent FSEvents watchers on multiple temp directories compete
// for kernel resources and cause spurious timeouts.  `#[serial]` serialises
// these tests so only one watcher is active at a time.

/// Waits up to `timeout` for `condition` to become true, polling every 20ms.
async fn wait_for<F: Fn() -> bool>(condition: F, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn watch_debounces_burst_into_one_compile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_inner = Arc::clone(&counter);

    let options = WatchOptions {
        debounce_ms: 200,
        max_ticks: Some(1),
    };

    let handle = tokio::spawn({
        let root = root.clone();
        async move {
            run_watch_loop(
                root,
                options,
                move |_changed| {
                    counter_inner.fetch_add(1, Ordering::SeqCst);
                    async { Ok(()) }
                },
            )
            .await
        }
    });

    // Give the watcher time to initialise — FSEvents on macOS can take up to
    // 500ms to register a new directory.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Write multiple different files in a burst — all within the debounce window.
    // Writing different files (rather than overwriting the same one) ensures FSEvents
    // sees distinct create events and doesn't coalesce them before they reach notify.
    for i in 0..5u8 {
        std::fs::write(dir.path().join(format!("source_{i}.md")), [i]).expect("write");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Wait for the compile callback to fire; max_ticks=1 stops after 1 batch.
    // FSEvents + batch_mode debounce can take up to ~600ms from last write.
    let ok = wait_for(|| counter.load(Ordering::SeqCst) >= 1, Duration::from_secs(8)).await;
    assert!(ok, "compile_fn was not called within timeout");

    // The burst of 5 writes must have been collapsed into 1 compile invocation.
    handle.await.expect("task").expect("watch_loop");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "debounce should collapse the burst into exactly 1 compile"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn watch_filters_thinkingroot_target_git_dotfiles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_inner = Arc::clone(&counter);

    let options = WatchOptions {
        debounce_ms: 200,
        max_ticks: Some(1),
    };

    // Create directories that should be excluded.
    for excluded_dir in &[".thinkingroot", ".git", "target"] {
        std::fs::create_dir_all(dir.path().join(excluded_dir)).expect("mkdir");
    }

    let handle = tokio::spawn({
        let root = root.clone();
        async move {
            run_watch_loop(
                root,
                options,
                move |_changed| {
                    counter_inner.fetch_add(1, Ordering::SeqCst);
                    async { Ok(()) }
                },
            )
            .await
        }
    });

    // Watcher init — give FSEvents time to register.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Write only to excluded directories and excluded file types.
    std::fs::write(dir.path().join(".thinkingroot/graph.db"), b"noise").ok();
    std::fs::write(dir.path().join(".git/COMMIT_EDITMSG"), b"noise").ok();
    std::fs::write(dir.path().join("target/debug/root"), b"noise").ok();
    std::fs::write(dir.path().join(".hidden_file"), b"noise").ok();
    std::fs::write(dir.path().join("real_source.rs.swp"), b"noise").ok();
    std::fs::write(dir.path().join("real_source.rs~"), b"noise").ok();

    // Wait past debounce window + batch-mode delay + margin.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // None of those writes should have triggered a compile.
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "excluded paths must not trigger compilation"
    );

    // Now write a real source file — should trigger max_ticks=1 compile.
    std::fs::write(dir.path().join("main.rs"), b"fn main() {}").expect("write");

    let ok = wait_for(
        || counter.load(Ordering::SeqCst) >= 1,
        Duration::from_secs(8),
    )
    .await;
    assert!(ok, "real source change should trigger compile");

    handle.await.expect("task").expect("watch_loop");
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn watch_serializes_compiles_when_edits_arrive_mid_compile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    let compile_calls = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let overlap_detected = Arc::new(AtomicUsize::new(0));

    let compile_calls_inner = Arc::clone(&compile_calls);
    let in_flight_inner = Arc::clone(&in_flight);
    let overlap_inner = Arc::clone(&overlap_detected);

    let options = WatchOptions {
        debounce_ms: 200,
        max_ticks: Some(2),
    };

    let handle = tokio::spawn({
        let root = root.clone();
        async move {
            run_watch_loop(
                root,
                options,
                move |_changed| {
                    let calls = Arc::clone(&compile_calls_inner);
                    let inflight = Arc::clone(&in_flight_inner);
                    let overlap = Arc::clone(&overlap_inner);
                    async move {
                        let prev = inflight.fetch_add(1, Ordering::SeqCst);
                        if prev > 0 {
                            overlap.fetch_add(1, Ordering::SeqCst);
                        }
                        calls.fetch_add(1, Ordering::SeqCst);
                        // Simulate a slow compile.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        inflight.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    }
                },
            )
            .await
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    let file = dir.path().join("source.rs");
    // First batch: trigger compile.
    std::fs::write(&file, b"v1").expect("write");

    // While compile is in flight (300ms), write more files.
    tokio::time::sleep(Duration::from_millis(50)).await;
    for i in 0..5u8 {
        std::fs::write(dir.path().join(format!("extra_{i}.rs")), [i]).expect("write");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Wait for both ticks to complete.
    let ok = wait_for(
        || compile_calls.load(Ordering::SeqCst) >= 2,
        Duration::from_secs(15),
    )
    .await;
    assert!(ok, "expected 2 compile calls within timeout");

    handle.await.expect("task").expect("watch_loop");

    assert_eq!(
        overlap_detected.load(Ordering::SeqCst),
        0,
        "compiles must be serialized — no two in flight simultaneously"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn watch_continues_after_compile_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    let compile_calls = Arc::new(AtomicUsize::new(0));
    let compile_calls_inner = Arc::clone(&compile_calls);

    let options = WatchOptions {
        debounce_ms: 200,
        max_ticks: Some(2),
    };

    let handle = tokio::spawn({
        let root = root.clone();
        async move {
            run_watch_loop(
                root,
                options,
                move |_changed| {
                    let calls = Arc::clone(&compile_calls_inner);
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            anyhow::bail!("simulated compile error on first call");
                        }
                        Ok(())
                    }
                },
            )
            .await
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    let file = dir.path().join("source.md");

    // First write: triggers the error compile.
    std::fs::write(&file, b"v1").expect("write");

    // Wait for the first compile to fail.
    let ok = wait_for(
        || compile_calls.load(Ordering::SeqCst) >= 1,
        Duration::from_secs(8),
    )
    .await;
    assert!(ok, "first compile should have been called");

    // Second write: triggers the success compile.
    // Wait past debounce + margin before writing.
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(&file, b"v2").expect("write");

    let ok = wait_for(
        || compile_calls.load(Ordering::SeqCst) >= 2,
        Duration::from_secs(8),
    )
    .await;
    assert!(ok, "second compile should be called despite first error");

    handle.await.expect("task").expect("watch_loop");
    assert_eq!(
        compile_calls.load(Ordering::SeqCst),
        2,
        "loop must survive an error and continue"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn watch_filters_editor_swap_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_inner = Arc::clone(&counter);

    let options = WatchOptions {
        debounce_ms: 200,
        max_ticks: Some(1),
    };

    let handle = tokio::spawn({
        let root = root.clone();
        async move {
            run_watch_loop(
                root,
                options,
                move |_changed| {
                    counter_inner.fetch_add(1, Ordering::SeqCst);
                    async { Ok(()) }
                },
            )
            .await
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Write only editor noise.
    for name in &[
        "notes.md.swp",
        "notes.md.swo",
        "notes.md.swx",
        "notes.md~",
        "notes.md.tmp",
        "notes.md.bak",
        "4913",
        ".#notes.md",
    ] {
        std::fs::write(dir.path().join(name), b"editor noise").ok();
    }

    // Wait past debounce + batch-mode delay + margin.
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "editor swap files must not trigger compilation"
    );

    // Real file should still trigger.
    std::fs::write(dir.path().join("notes.md"), b"real content").expect("write");

    let ok = wait_for(
        || counter.load(Ordering::SeqCst) >= 1,
        Duration::from_secs(8),
    )
    .await;
    assert!(ok, "real file write should trigger compile");

    handle.await.expect("task").expect("watch_loop");
}
