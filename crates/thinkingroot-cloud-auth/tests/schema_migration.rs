//! I-CA8: schema-forward refusal + v1 → v2 round-trip migration.

use thinkingroot_cloud_auth::{config, CloudError};

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

#[test]
fn schema_v99_refused_on_load() {
    let (_home, _guard) = use_temp_home();
    let path = config::config_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"schema_version":99,"server":"https://x.com"}"#,
    )
    .unwrap();

    match config::load() {
        Err(CloudError::IncompatibleSchema {
            found: 99,
            max_supported: 2,
        }) => {}
        other => panic!("expected IncompatibleSchema, got {other:?}"),
    }
}

#[test]
fn legacy_v1_file_without_schema_version_loads_as_v2() {
    let (_home, _guard) = use_temp_home();
    let path = config::config_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Pre-schema-version files written by the old `tr login` paste
    // flow.
    std::fs::write(
        &path,
        r#"{"server":"https://api.thinkingroot.dev","token":"legacy","handle":"naveen"}"#,
    )
    .unwrap();

    let loaded = config::load().unwrap().expect("loaded");
    assert_eq!(loaded.schema_version, 2);
    assert_eq!(loaded.token.as_deref(), Some("legacy"));
    assert_eq!(loaded.handle.as_deref(), Some("naveen"));
}
