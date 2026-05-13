//! I-CA5: 20 concurrent `config::update` tasks → consistent state.

use thinkingroot_cloud_auth::config;

fn use_temp_home() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let guard = thinkingroot_core::test_util::ENV_GUARD
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    #[cfg(target_os = "macos")]
    unsafe { std::env::set_var("HOME", tmp.path()); }
    #[cfg(target_os = "linux")]
    unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()); }
    #[cfg(target_os = "windows")]
    unsafe { std::env::set_var("APPDATA", tmp.path()); }
    (tmp, guard)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
// ENV_GUARD must be held for the test's lifetime to keep HOME/XDG
// pointing at this test's tempdir; clippy's await-holding-lock advice
// is too generic here. Matches the pattern in browser_flow_smoke.rs.
#[allow(clippy::await_holding_lock)]
async fn twenty_concurrent_updates_leave_file_consistent() {
    let (_home, _guard) = use_temp_home();

    // Seed an initial config.
    let mut cfg = config::Config::empty();
    cfg.token = Some("seed".into());
    config::save(&cfg).unwrap();

    let mut handles = Vec::new();
    for i in 0..20u64 {
        handles.push(tokio::task::spawn_blocking(move || {
            config::update(|c| {
                c.credits_remaining = Some(i);
            })
            .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let loaded = config::load().unwrap().expect("loaded");
    let remaining = loaded.credits_remaining.expect("remaining set");
    assert!(
        remaining < 20,
        "remaining {remaining} not in [0, 20)"
    );
    assert_eq!(loaded.token.as_deref(), Some("seed"));
}
