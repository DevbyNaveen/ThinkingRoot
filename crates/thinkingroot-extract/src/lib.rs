//! Mechanical extraction — Witness Mesh era.
//!
//! Pre-cutover this crate dispatched chunks through a 5-provider LLM
//! batch pipeline (Anthropic, OpenAI, Azure, Bedrock, Ollama). Post-
//! cutover (Witness Mesh v1.0, 2026-05-11) it ships:
//!
//! - **Structural extraction** (`structural`) — tree-sitter / regex
//!   over `Chunk` metadata; produces legacy `Claim` rows for the
//!   dual-write transition.
//! - **Rule catalog** (`rule_catalog`) — 56-rule registry pinned to
//!   `Cargo.lock` grammar versions; the source of truth for every
//!   Witness's `rule` field.
//! - **Witness Mesh extractors** — `comment_claims` (`@claim` /
//!   `@invariant` / `@owns` / `SAFETY:`), `parse_doc_rules`
//!   (rustdoc/jsdoc/javadoc/markdown), `test_assertions`
//!   (cargo-test/pytest/jest/junit), `lsp_rules` (backend
//!   detection + `lsp::skipped@v1` honest absence).
//! - **Mesh assembler** (`witness_mesh`) — dedup, SAFETY-rule
//!   cross-check, deterministic output sort.
//! - **Decorators** kept from the LLM era for the structural
//!   path: `expiration`, `quantity`, `sensitivity` (regex-only
//!   PII classifier).
//!
//! Moved to `thinkingroot-llm` crate (Phase 2 cleanup): `llm`,
//! `prompts`, `scheduler`, `citation`, `readme`, `graph_context`,
//! `events`, `checkpoint`. The chat-time LLM substrate is now
//! honestly separated from mechanical Witness Mesh extraction; the
//! `thinkingroot-extract` name reflects only the compile-time
//! extraction story. See `.claude/rules/witness-mesh.md` for the
//! cutover record.

pub mod comment_claims;
pub mod expiration;
pub mod extractor;
pub mod lsp_rules;
pub mod parse_doc_rules;
pub mod quantity;
pub mod rule_catalog;
pub mod schema;
pub mod sensitivity;
pub mod structural;
pub mod test_assertions;
pub mod witness_mesh;

pub use extractor::{ExtractionOutput, Extractor};
pub use rule_catalog::{
    CATALOG_VERSION, RULE_CATALOG, RuleDescriptor, rule_catalog_toml,
};
pub use witness_mesh::{AssembledMesh, MeshError, assemble as assemble_witness_mesh};
