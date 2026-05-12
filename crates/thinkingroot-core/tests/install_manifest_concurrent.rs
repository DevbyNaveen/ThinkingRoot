//! Integration test: 8 threads each register a distinct binary id
//! variant, no updates are lost, manifest stays parseable throughout.
//! Exercises the `fs2` advisory-lock serialisation in
//! `InstallManifest::register_or_update`.

use std::sync::{Arc, Barrier};

use thinkingroot_core::install_manifest::{BinaryEntry, BinaryId, InstallManifest};

/// Inline cross-platform config-dir override for this integration
/// test. Mirrors the pattern in `cortex.rs::ConfigDirOverride` and
/// the in-module test helper, but the unit-test `ENV_GUARD` doesn't
/// cross binary boundaries — this binary's tests are single-threaded
/// because we have only one test function here.
fn override_config_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: this integration-test binary has exactly one test
    // function; no concurrent env access within this process.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("APPDATA", tmp.path());
    }
    tmp
}

#[test]
fn concurrent_register_or_update_does_not_lose_writes() {
    let _tmp = override_config_dir();

    // Two BinaryId variants supported today; alternate across 8
    // threads. The point: re-registration of the same id is
    // idempotent and concurrent registration doesn't tear the file.
    let ids = [BinaryId::CliScript, BinaryId::DesktopBundle];
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8u32)
        .map(|i| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let entry = BinaryEntry {
                    id: ids[(i % 2) as usize],
                    path: std::path::PathBuf::from(format!("/tmp/fake-{i}")),
                    version: format!("0.9.{i}"),
                    installed_at: chrono::Utc::now(),
                    checksum_blake3: format!("{i:064x}"),
                };
                InstallManifest::register_or_update(entry).expect("registered");
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    let m = InstallManifest::load()
        .expect("manifest parseable after concurrent writes")
        .expect("manifest present after at least one register");
    assert_eq!(m.binaries.len(), 2, "exactly one row per BinaryId");
    let ids_present: std::collections::HashSet<_> = m.binaries.iter().map(|e| e.id).collect();
    assert!(ids_present.contains(&BinaryId::CliScript));
    assert!(ids_present.contains(&BinaryId::DesktopBundle));
}
