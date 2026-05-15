//! Witness Mesh anchor verification — the only surviving piece of
//! the pre-Witness-Mesh grounding tribunal.
//!
//! **Deleted in Witness Mesh cutover (2026-05-11) + cleanup (2026-05-15):**
//! - `grounder.rs` — the 4-judge orchestrator
//! - `nli.rs` — Judge 4 (ONNX NLI cross-encoder)
//! - `semantic.rs` — Judge 3 (fastembed cosine similarity)
//! - `span.rs` — Judge 2 (LLM-quote span attribution) — superseded
//!   by `witness_verifier.rs` which is byte-exact rather than fuzzy
//! - `dedup.rs` — Jaccard claim-text dedup, superseded by
//!   content-addressed Witness id dedup in
//!   `thinkingroot_extract::witness_mesh::assemble`
//! - `lexical.rs` — `LexicalJudge` tokenizer (the Rooting consumer
//!   was deleted on 2026-05-14; the helper had zero remaining
//!   callers and was removed on 2026-05-15 along with the
//!   ~175 MB of checked-in NLI ONNX models + tokenizer)
//!
//! **Surviving:**
//! - `witness_verifier.rs` — `BLAKE3(source[start..end]) ==
//!   content_blake3`. The one mechanical check the Witness Mesh
//!   substrate needs. ~10µs per witness; replaces ~57KB of judges
//!   with ~200 LOC of cryptographic comparison.

mod witness_verifier;

pub use witness_verifier::{
    AnchorVerdict, WitnessAnchorError, is_witness_anchor_intact, verify_witness_anchor,
};
