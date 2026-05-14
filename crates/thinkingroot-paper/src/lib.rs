//! Living Paper synthesiser — produces a per-compile `paper.md`
//! artefact that humans can read and AI agents can parse.
//!
//! # Two-layer design
//!
//! ## Layer 1 — Deterministic skeleton
//! Built from the Witness Mesh substrate alone, no LLM. Sections:
//! - **At a glance** — workspace + witness + source + branch counts
//! - **Architecture** — Mermaid concept-map of the top witness clusters
//! - **Promises it keeps** — verbatim invariant list from the rule catalog
//! - **How it's tested** — test-annotation witness counts
//! - **Provenance** — pack identity, rule catalog hash, workspace id
//!
//! ## Layer 2 — AI narrative (v1.1 — not in this scaffold)
//! Will add: Abstract, Key Ideas, How it fits together, Recent changes,
//! How to use it. Each section requires `[[witness:<id>]]` citations
//! validated post-synthesis so the LLM cannot hallucinate sources.
//!
//! # Output shape
//!
//! Single `paper.md` file with YAML frontmatter (machine-readable
//! spine: workspace, version, sections index, witness IDs, signing
//! identity) plus human-readable markdown body. The same file serves
//! both audiences — human renderers hide the frontmatter, machine
//! agents parse it directly.
//!
//! # Wiring
//!
//! Called from `pipeline.rs` Phase 10b (after the deterministic
//! README synthesis). Non-fatal on failure: a paper synthesis error
//! means the paper is stale, not that the compile produced bad data.
//! Writes to `<root>/.thinkingroot/paper.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod frontmatter;
pub mod mermaid;
pub mod sections;
pub mod synthesizer;

pub use synthesizer::{synthesize, synthesize_and_persist, PaperOutput, PaperSynthesisError};

/// Default filename for the Living Paper artefact inside a workspace.
/// Loaded by the pack-export path (`V3PackBuilder::with_paper`) and the
/// desktop's `paper_get` Tauri command.
pub const PAPER_FILE_NAME: &str = "paper.md";

/// Version of the `paper.md` schema (frontmatter `paper_version` field).
/// Bump when the YAML frontmatter shape changes in a way machine
/// agents need to detect.
pub const PAPER_VERSION: u32 = 1;
