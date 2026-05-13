//! `root push` — GitHub-feel alias for `root publish`.
//!
//! Forwards to [`publish::run`] with friendlier console copy. No new
//! wire format; same auth, same `tr-pack.toml` manifest, same tar+zstd
//! upload, same compile-job poll. Adding this command unlocks the
//! `git push`-style mental model that users with GitHub habits expect.

use std::path::PathBuf;

use anyhow::Result;
use console::style;

use super::publish::{self, Visibility};

pub async fn run(
    path: PathBuf,
    wait: bool,
    timeout_secs: u64,
    server: Option<String>,
    visibility: Option<Visibility>,
) -> Result<()> {
    println!(
        "{} pushing workspace at {} to ThinkingRoot Cloud{}",
        style("→").cyan(),
        style(path.display()).dim(),
        visibility
            .map(|v| format!(" ({})", v.as_str()))
            .unwrap_or_default(),
    );
    publish::run(path, wait, timeout_secs, server, visibility).await
}
