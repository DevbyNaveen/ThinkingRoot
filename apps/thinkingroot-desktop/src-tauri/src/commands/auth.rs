//! Read-only `auth_state` Tauri command (re-exported from
//! `commands::cloud`).
//!
//! Before Slice 1, this module owned its own implementation reading
//! from `DesktopState`. As of Task 16, the active implementation
//! lives in `commands/cloud.rs` and reads from
//! `thinkingroot-cloud-auth`'s auth.json. This file is kept only as
//! a re-export to preserve any source-compat imports (`use commands::auth;`)
//! that may exist elsewhere in the desktop crate.

// The `unused_imports` allow is load-bearing: the re-exports are a
// public API for downstream `use commands::auth::{auth_state, AuthState}`
// callers. The crate has none today, but the re-export's purpose IS
// the import path — removing it would break the source-compat contract.
#[allow(unused_imports)]
pub use crate::commands::cloud::{auth_state, AuthState};
