//! ThinkingRoot Cloud Auth — substrate shared between the `root` CLI
//! and the Desktop app for browser-flow login, auth.json persistence,
//! and `/me` + `/credits/balance` polling against the hub.
//!
//! Spec: `docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md`.

pub mod config;
pub mod error;
pub mod http;
pub mod me;
pub mod auth_flow;

pub use error::CloudError;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles_and_links() {
        // Anchor test. Real coverage lives in the per-module test
        // suites + integration tests under `tests/`.
        assert_eq!(2 + 2, 4);
    }
}
