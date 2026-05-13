//! E2E: drive `run_browser_login` against an unreachable hub URI.
//!
//! Covers the deterministic paths (AlreadyInFlight, Cancelled). The
//! full browser→callback hop is left as a documenting marker because
//! a deterministic test of that hop requires shimming
//! `webbrowser::open` with a feature flag or a Tauri test harness —
//! out of scope for this slice. The other two paths exercise the
//! load-bearing concurrency + cancellation invariants (I-CA9).
//!
//! Note: the `FakeCloud` substrate from Task 7 (`tests/fake_cloud.rs`)
//! is intentionally NOT used here. Its `/auth/cli` route auto-redirects
//! to the callback URL, which races the cancellation + concurrency
//! probes — a real browser opening on macOS during the test can
//! complete the loop before the assertion runs. Pointing at an
//! unreachable URI keeps both tests deterministic.

use std::time::Duration;

use thinkingroot_cloud_auth::auth_flow::{run_browser_login, Surface};
use tokio_util::sync::CancellationToken;

/// Per-test isolation: thinkingroot-core's ENV_GUARD serialises the
/// HOME / XDG_CONFIG_HOME mutation across parallel test workers.
/// Cloned from the config.rs test helper but local to integration
/// tests because `tests/` files don't share modules with `src/`.
///
/// `into_inner()` on a poisoned guard is intentional: the mutex
/// protects no semantic state, it only serialises env-var mutation;
/// a previous test panicking does not corrupt the env-var space.
fn use_temp_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let guard = thinkingroot_core::test_util::ENV_GUARD
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    #[cfg(target_os = "macos")]
    unsafe {
        std::env::set_var("HOME", tmp.path());
    }
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
    }
    #[cfg(target_os = "windows")]
    unsafe {
        std::env::set_var("APPDATA", tmp.path());
    }
    (tmp, guard)
}

/// AlreadyInFlight: when one login is in progress, a second concurrent
/// login returns CloudError::AlreadyInFlight immediately.
///
/// First login points at an unreachable URL so it stays parked on
/// the callback channel (holding LOGIN_IN_FLIGHT) until cancelled.
/// If we pointed at the FakeCloud, its `/auth/cli` auto-redirect +
/// a real browser opening on macOS would race the second-login
/// attempt — sometimes first finishes before second tries, sometimes
/// not. Unreachable URL → deterministic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// ENV_GUARD must be held for the test's lifetime to keep HOME pointing
// at this test's tempdir; clippy's await-holding-lock advice is too
// generic here. Semantically the same as install_manifest.rs::tests
// which holds the same guard across `cargo test` sync code.
#[allow(clippy::await_holding_lock)]
async fn second_login_returns_already_in_flight() {
    let (_home, _guard) = use_temp_home();
    let uri1 = "http://127.0.0.1:1".to_string();
    let uri2 = "http://127.0.0.1:1".to_string();

    let cancel1 = CancellationToken::new();
    let cancel2 = CancellationToken::new();

    let first = tokio::spawn({
        let cancel = cancel1.clone();
        async move { run_browser_login(&uri1, Surface::Cli, cancel).await }
    });

    // Give the first login time to acquire the LOGIN_IN_FLIGHT mutex.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let second = run_browser_login(&uri2, Surface::Cli, cancel2.clone()).await;
    assert!(
        matches!(
            second,
            Err(thinkingroot_cloud_auth::CloudError::AlreadyInFlight)
        ),
        "expected AlreadyInFlight, got {second:?}"
    );

    cancel1.cancel();
    let _ = first.await;
}

/// Cancelled: the CancellationToken shuts the listener cleanly and
/// returns Err(Cancelled). I-CA9: cancellation is clean — no stray
/// listening port, no panic, no stack trace.
///
/// We point at an unreachable address (port 1) rather than the
/// FakeCloud because FakeCloud's `/auth/cli` route auto-redirects
/// to the callback — if `webbrowser::open` succeeds on this host
/// (which it does on macOS), a real browser races the callback
/// loop to success and the cancellation never wins. Pointing at an
/// unreachable server keeps the future parked on the callback
/// channel; the only way out is the cancel arm of the select.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
// ENV_GUARD must be held for the test's lifetime to keep HOME pointing
// at this test's tempdir; clippy's await-holding-lock advice is too
// generic here.
#[allow(clippy::await_holding_lock)]
async fn cancel_returns_cancelled_error() {
    let (_home, _guard) = use_temp_home();
    // Port 1 is reserved + unreachable; webbrowser::open will load
    // the URL but the browser's request will hang/fail with no
    // redirect path. The localhost listener stays parked waiting
    // for a callback that never arrives.
    let uri = "http://127.0.0.1:1".to_string();

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let login = tokio::spawn(async move {
        run_browser_login(&uri, Surface::Cli, cancel_clone).await
    });

    // Allow the login to bind the listener + open the browser.
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();

    let result = login.await.unwrap();
    assert!(
        matches!(result, Err(thinkingroot_cloud_auth::CloudError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

/// Documenting marker: the full browser→callback hop test is left
/// commented out because deterministic simulation of the browser-open
/// step requires a feature-flag shim around `webbrowser::open`. The
/// other two tests cover the deterministic invariants.
#[test]
fn full_hop_test_is_a_documenting_marker_see_task_17() {
    // Intentional placeholder. Real coverage of the full hop lands
    // in apps/thinkingroot-desktop/src-tauri/tests/ during Task 15/17.
}
