//! `root` cloud subcommands — the local-to-cloud publish workflow.
//!
//! These five subcommands replace the legacy `tr` binary that lived in
//! `thinkingroot-cloud/apps/cli` (Phase G consolidation). The cloud
//! binary is now a deprecation shim that forwards to `root`.
//!
//! - [`login::run`] — paste an API token, validate via `/me`, persist.
//! - [`whoami::run`] — print the logged-in identity.
//! - [`init::run`] — scaffold `tr-pack.toml` for the publish flow.
//! - [`publish::run`] — tar+zstd the workspace, upload, enqueue compile.
//! - [`status::run`] — list recent compile jobs.
//!
//! Config lives at `~/.config/thinkingroot/auth.json` — the same path
//! the legacy `tr` binary used, so users keep their session across
//! the rename.

pub mod config;
pub mod http;
pub mod init;
pub mod login;
pub mod publish;
pub mod status;
pub mod whoami;
