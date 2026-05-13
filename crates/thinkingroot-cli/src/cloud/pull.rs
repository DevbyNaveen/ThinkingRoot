//! `root pull` / `root clone` — alias for `root install owner/slug[@version]`.
//!
//! Forwards to the existing install command. Provides the
//! `git pull` / `git clone` mental model GitHub-habit users expect.
//! Same wire flow as `root install`, same Sigstore + BLAKE3
//! verification — only the console label changes.

use std::path::PathBuf;

use anyhow::Result;
use console::style;

/// Pull a pack into the local workspace.
///
/// `pack_ref` is `owner/slug` or `owner/slug@version`. When the
/// version is omitted, `pack_cmd::run_install` resolves to the
/// latest published via the registry's discovery doc.
///
/// `target` defaults (inside `run_install`) to
/// `~/.thinkingroot/packs/<owner>/<slug>/<version>/`. The CLI's
/// `--target` flag forwards through unchanged.
pub async fn run(pack_ref: String, target_dir: Option<PathBuf>) -> Result<()> {
    println!(
        "{} pulling pack {} from ThinkingRoot Cloud",
        style("→").cyan(),
        style(&pack_ref).bold(),
    );
    crate::pack_cmd::run_install(
        &pack_ref,
        target_dir,
        /* registry_override */ None,
        /* allow_unsigned */ false,
    )
    .await
}
