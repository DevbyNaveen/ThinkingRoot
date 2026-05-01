//! `root reflect` subcommand — run a reflect cycle and surface known
//! unknowns.
//!
//! Reflect (see `crates/thinkingroot-reflect`) is the engine's
//! "what knowledge SHOULD exist but doesn't" pass. It runs against
//! the graph that `root compile` already populated:
//!
//! 1. Discovers structural patterns from co-occurrence frequencies
//!    (per entity-type / condition-claim-type / expected-claim-type).
//! 2. Generates `known_unknowns` rows for entities missing the
//!    expected claim types implied by the patterns above.
//! 3. Resolves gaps that have since been filled.
//!
//! The CLI subcommand exposes two modes:
//!
//! * Interactive (default): prints a human-readable report of the
//!   open gaps, oldest first.
//! * `--json <path>`: writes a machine-readable artifact to disk
//!   so downstream tools (the cloud's compile-worker) can ingest
//!   gaps into a queryable surface (federation's `pack_reflect_gaps`
//!   table).
//!
//! `--json` writes a single JSON document with this shape:
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "computed_at": "2026-04-28T11:23:45Z",
//!   "patterns_discovered": 12,
//!   "gaps_open": 47,
//!   "gaps_created": 3,
//!   "gaps_resolved": 1,
//!   "open_gaps": [
//!     {
//!       "id": "gap_<sha>",
//!       "entity_id": "...",
//!       "pattern_id": "...",
//!       "expected_claim_type": "creation_date",
//!       "confidence": 0.84,
//!       "status": "open",
//!       "created_at": 1761640225.0,
//!       "resolved_at": 0.0,
//!       "resolved_by": ""
//!     }
//!   ]
//! }
//! ```
//!
//! Stable wire shape — bumping `schema_version` is a breaking change.

use std::path::{Path, PathBuf};

use console::style;
use serde::Serialize;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_reflect::{
    GapReport, ReflectConfig, ReflectEngine, ReflectResult, list_open_gaps,
};

#[derive(Debug, Serialize)]
struct GapsJson<'a> {
    schema_version: u32,
    computed_at: String,
    patterns_discovered: usize,
    gaps_open: usize,
    gaps_created: usize,
    gaps_resolved: usize,
    open_gaps: &'a [GapReport],
}

const SCHEMA_VERSION: u32 = 1;

pub fn run(workspace_path: &Path, json_out: Option<&PathBuf>) -> anyhow::Result<()> {
    let data_dir = resolve_data_dir(workspace_path)?;
    let graph_dir = data_dir.join("graph");
    if !graph_dir.exists() {
        anyhow::bail!(
            "no compiled graph at {} — run `root compile` first",
            graph_dir.display()
        );
    }
    let graph = GraphStore::init(&graph_dir)?;

    let engine = ReflectEngine::new(ReflectConfig::default());
    let result: ReflectResult = engine.reflect(&graph)?;
    let open: Vec<GapReport> = list_open_gaps(&graph, None, 0.0)?;

    if let Some(out) = json_out {
        let body = GapsJson {
            schema_version: SCHEMA_VERSION,
            computed_at: chrono::Utc::now().to_rfc3339(),
            patterns_discovered: result.patterns.len(),
            gaps_open: result.open_gaps_total,
            gaps_created: result.gaps_created,
            gaps_resolved: result.gaps_resolved,
            open_gaps: &open,
        };
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let serialised = serde_json::to_vec_pretty(&body)?;
        std::fs::write(out, serialised)?;
        println!(
            "{} {} {}",
            style("✓").green(),
            style("wrote").bold(),
            style(out.display()).cyan()
        );
        println!(
            "  {} patterns, {} open gaps ({} new this run, {} resolved)",
            result.patterns.len(),
            result.open_gaps_total,
            result.gaps_created,
            result.gaps_resolved
        );
        return Ok(());
    }

    // Interactive output.
    println!();
    println!("{}", style("Reflect").bold());
    println!(
        "{}",
        style(format!("  workspace: {}", data_dir.display())).dim()
    );
    println!();
    println!(
        "  {} patterns discovered    {} open gaps    {} new    {} resolved",
        result.patterns.len(),
        result.open_gaps_total,
        result.gaps_created,
        result.gaps_resolved,
    );
    println!();

    if open.is_empty() {
        println!(
            "  {}  no open gaps — every observed pattern is fully covered",
            style("✓").green()
        );
        return Ok(());
    }

    let to_show = open.len().min(20);
    for gap in open.iter().take(to_show) {
        println!(
            "  {}  {} — expected `{}` ({:.0}% confidence)",
            style("?").yellow(),
            style(&gap.entity_name).bold(),
            gap.expected_claim_type,
            gap.confidence * 100.0,
        );
    }
    if open.len() > to_show {
        println!("  {}  …{} more", style("…").dim(), open.len() - to_show);
    }
    println!();
    Ok(())
}

fn resolve_data_dir(workspace_path: &Path) -> anyhow::Result<PathBuf> {
    let dir = workspace_path.join(".thinkingroot");
    if !dir.exists() {
        anyhow::bail!(
            "no ThinkingRoot workspace found at {} — run `root compile` first",
            workspace_path.display()
        );
    }
    Ok(dir)
}
