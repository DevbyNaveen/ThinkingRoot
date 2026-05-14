//! Witness Mesh anchor verification — the surviving 1-of-5 piece of
//! the pre-Witness-Mesh grounding tribunal.
//!
//! **Deleted in Witness Mesh cutover (2026-05-11):**
//! - `grounder.rs` — the 4-judge orchestrator
//! - `nli.rs` — Judge 4 (ONNX NLI cross-encoder)
//! - `semantic.rs` — Judge 3 (fastembed cosine similarity)
//! - `span.rs` — Judge 2 (LLM-quote span attribution) — superseded
//!   by `witness_verifier.rs` which is byte-exact rather than fuzzy
//! - `dedup.rs` — Jaccard claim-text dedup, superseded by
//!   content-addressed Witness id dedup in
//!   `thinkingroot_extract::witness_mesh::assemble`
//!
//! **Surviving:**
//! - `witness_verifier.rs` — `BLAKE3(source[start..end]) ==
//!   content_blake3`. The one mechanical check the Witness Mesh
//!   substrate needs. ~10µs per witness; replaces ~57KB of judges
//!   with ~200 LOC of cryptographic comparison.
//! - `lexical.rs` — `LexicalJudge` tokenizer + Jaccard overlap
//!   scorer. The Rooting crate that originally consumed it has
//!   been deleted (post-Witness-Mesh cleanup, 2026-05-14); the
//!   helper stays here as the single home for lexical scoring.

mod lexical;
mod witness_verifier;

pub use lexical::LexicalJudge;
pub use witness_verifier::{
    AnchorVerdict, WitnessAnchorError, is_witness_anchor_intact, verify_witness_anchor,
};
