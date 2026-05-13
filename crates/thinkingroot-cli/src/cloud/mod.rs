//! `root` cloud subcommands.
//!
//! These subcommands now delegate to the shared
//! `thinkingroot-cloud-auth` crate for HTTP + config + browser-flow.
//! What stays here is CLI-shaped: argument parsing, console-friendly
//! output, and workspace-aware operations (publish/push/pull/init).
//!
//! - [`login::run`] — browser flow (or `--token` paste).
//! - [`whoami::run`] — print the logged-in identity.
//! - [`init::run`] — scaffold `tr-pack.toml`.
//! - [`publish::run`] — tar+zstd, upload, enqueue compile.
//! - [`status::run`] — list recent compile jobs.

pub mod init;
pub mod login;
pub mod logout;
pub mod publish;
pub mod pull;
pub mod push;
pub mod status;
pub mod whoami;

// Re-export the shared cloud-auth substrate so subcommand files can
// keep using `super::{config, http}` imports unchanged. `CloudError`
// and `me` are surfaced too so future cloud subcommands (post-Slice-1
// `credits`, `logout`, `pull`) can reach them through one path; the
// `unused_imports` warning fires on the bin crate today because no
// other module here references them yet.
#[allow(unused_imports)]
pub use thinkingroot_cloud_auth::{config, error::CloudError, http, me};

use anyhow::{Result, anyhow};

use thinkingroot_cloud_auth::config::Config;

/// CLI-shaped wrapper over `config::load()`: missing file → empty
/// `Config` with defaults; `override_server`, when provided, replaces
/// the stored server URL (used by the global `--server` flag).
pub fn load_or_default(override_server: Option<&str>) -> Result<Config> {
    let mut cfg = match config::load()? {
        Some(c) => c,
        None => Config::empty(),
    };
    if let Some(s) = override_server {
        cfg.server = s.to_string();
    }
    Ok(cfg)
}

/// Return the saved token or a friendly error if the user hasn't
/// logged in.
pub fn require_token(cfg: &Config) -> Result<&str> {
    cfg.token
        .as_deref()
        .ok_or_else(|| anyhow!("not logged in — run `root login` first"))
}
