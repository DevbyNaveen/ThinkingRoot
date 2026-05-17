pub mod config;
pub mod cortex;
pub mod error;
pub mod global_config;
pub mod id;
pub mod install_manifest;
pub mod ir;
pub mod permissions;
pub mod recovery_log;
pub mod restart_state;
pub mod resolver;
pub mod safe_path;
pub mod structural_registry;
pub mod types;

pub use config::Config;
pub use error::{Error, Result};
pub use global_config::{
    Credentials, GlobalConfig, ServeConfig, WorkspaceEntry, WorkspaceRegistry,
};
pub use id::Id;
pub use install_manifest::{BinaryEntry, BinaryId, InstallManifest};
pub use recovery_log::{LogError as RecoveryLogError, RecoveryEvent, RecoveryEventKind};
pub use resolver::{PackResolver, ResolverDescriptor, ResolverError};
pub use restart_state::{
    AttemptOutcome, MAX_ATTEMPTS as RESTART_MAX_ATTEMPTS, RestartAttempt, RestartState,
    RestartStateError, backoff_for_attempt as restart_backoff_for_attempt,
};
pub use safe_path::{atomic_write, is_loopback_host, safe_join_under, validate_id};
pub use structural_registry::{STRUCTURAL_TABLES, StructuralTableSpec};
pub use types::*;

/// Test-only utilities shared across modules within
/// `thinkingroot-core` AND across crates that consume `thinkingroot-core`
/// from their `[dev-dependencies]` (gated behind the `test-helpers`
/// feature).  The `ENV_GUARD` lets multiple test modules serialise
/// env-var mutations against each other — per-module guards are
/// insufficient because `cargo test` runs tests from different modules
/// in parallel within the same binary, and `std::env::set_var` is a
/// process-wide mutation.
///
/// Always compiled when `test-helpers` is on so downstream test
/// binaries can reach `thinkingroot_core::test_util::ENV_GUARD`;
/// always compiled in-crate under `cfg(test)` for the existing
/// `install_manifest`, `cortex`, and `recovery_log` tests.
#[cfg(any(test, feature = "test-helpers"))]
pub mod test_util {
    use std::sync::Mutex;

    /// Single shared mutex for all env-mutating tests in the
    /// workspace.  Acquire BEFORE any `std::env::set_var` and hold
    /// until env vars are restored.  See `cortex::tests::ConfigDirOverride`
    /// and `install_manifest::tests::ConfigDirOverride` for usage,
    /// and `thinkingroot-cli/src/doctor/checks.rs` for the cross-crate
    /// consumer pattern.
    pub static ENV_GUARD: Mutex<()> = Mutex::new(());
}
