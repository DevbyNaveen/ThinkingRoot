//! `root render` — standalone artifact compilation.
//!
//! Per the v3 spec §11, the markdown artifact compiler (entity pages,
//! architecture map, decision log, agent brief, runbook, health
//! report, etc.) moves out of the v3 default build pipeline into a
//! standalone subcommand. v3 packs ship source bytes + claims; the
//! markdown artifacts are derived state — agents synthesise on demand
//! at $0.05/view per spec §11.
//!
//! Users who DO want pre-rendered markdown (the desktop knowledge-card
//! view, the legacy `.thinkingroot/artifacts/` consumers) invoke
//! `root render` explicitly.
//!
//! This is identical to what Phase 10 of the v1 pipeline runs — same
//! `thinkingroot_compile::Compiler::compile_affected` call with empty
//! `affected_entity_ids` (compile everything) and `has_changes = true`
//! (force-rebuild globals like the architecture map and decision log).

use std::path::Path;

use anyhow::{Context, Result};
use thinkingroot_compile::Compiler;
use thinkingroot_core::config::Config;
use thinkingroot_graph::graph::GraphStore;

/// Run `root render` against `path` (the workspace root). Walks
/// `<path>/.thinkingroot/graph.db` and emits artifacts under
/// `<path>/.thinkingroot/artifacts/` per the compile crate's output
/// layout.
pub fn run(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("workspace not found: {}", path.display());
    }
    let engine_dir = path.join(".thinkingroot");
    if !engine_dir.exists() {
        anyhow::bail!(
            "no engine output at `{}`; run `root compile {}` first",
            engine_dir.display(),
            path.display()
        );
    }

    let config = Config::load_merged(path)
        .with_context(|| format!("load config at {}", path.display()))?;
    let graph = GraphStore::init(&engine_dir)
        .with_context(|| format!("open graph at {}", engine_dir.display()))?;

    let compiler = Compiler::new(&config).with_context(|| "construct compiler")?;
    let artifacts = compiler
        .compile_affected(&graph, &engine_dir, &[], true)
        .with_context(|| "render artifacts")?;

    println!(
        "  rendered {} artifact{} -> {}",
        artifacts.len(),
        if artifacts.len() == 1 { "" } else { "s" },
        engine_dir.join("artifacts").display(),
    );
    Ok(())
}
