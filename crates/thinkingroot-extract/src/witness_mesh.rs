//! Witness Mesh assembly: take a stream of Witnesses from per-rule
//! extraction passes, deduplicate, validate against the rule catalog,
//! and emit the DAG.
//!
//! Determinism contract:
//! - Output `witnesses` vector is sorted by `WitnessId` (lower-hex
//!   ascending). Same inputs → same order, byte-for-byte.
//! - Edges are sorted `(parent, child)`. Same.
//! - Witnesses whose `rule` is not in `RULE_CATALOG` are **rejected**
//!   with a typed error (`MeshError::UnknownRule`). Silent drop is a
//!   honesty-bar violation.
//! - Witnesses with a `comment::SAFETY@v1` rule but no parent
//!   `code::unsafe-region` Witness in their `inputs` are flagged
//!   `MeshError::SafetyOrphan` and dropped from the mesh (the user's
//!   `// SAFETY:` annotation in non-unsafe code is meaningless, but
//!   we log it via `tracing::warn!` for debuggability).
//!
//! Dedup: two Witnesses with identical `(rule, spans)` produce the
//! same `WitnessId` by construction. The mesh hashes by id and keeps
//! the first occurrence; later duplicates increment a `dedup_count`
//! statistic for telemetry without changing output.

use std::collections::{HashMap, HashSet};

use thinkingroot_core::types::{Witness, WitnessId, WitnessInput};
use tracing::warn;

use crate::rule_catalog;

/// Reasons assembly may reject a Witness. Each is a typed error;
/// `MeshError::collect` accumulates them per Witness for diagnostic
/// reporting at the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeshError {
    /// The Witness's `rule` is not in `RULE_CATALOG`. The set of
    /// rules is closed at build time; an unknown rule means the
    /// caller is producing Witnesses out-of-catalog.
    UnknownRule { witness_id: WitnessId, rule: String },
    /// A `comment::SAFETY@v1` Witness must have a `WitnessRef`
    /// input to a parent `code::unsafe-region` Witness. The mesh
    /// drops violators (the annotation is meaningless without an
    /// `unsafe` block to justify).
    SafetyOrphan { witness_id: WitnessId },
    /// A Witness has no spans. Every Witness is byte-grounded by
    /// definition — empty spans is a malformed input.
    EmptySpans { witness_id: WitnessId },
    /// A Witness has no inputs. Even "root" Witnesses (those
    /// deriving directly from bytes) carry a `ByteRef` input per
    /// the spec — a zero-input Witness has nothing to derive from.
    EmptyInputs { witness_id: WitnessId },
}

impl std::fmt::Display for MeshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRule { witness_id, rule } => {
                write!(f, "witness {witness_id} references unknown rule `{rule}`")
            }
            Self::SafetyOrphan { witness_id } => {
                write!(
                    f,
                    "witness {witness_id}: comment::SAFETY@v1 requires a `code::unsafe-region` parent input"
                )
            }
            Self::EmptySpans { witness_id } => {
                write!(f, "witness {witness_id} has no spans (byte-grounding violation)")
            }
            Self::EmptyInputs { witness_id } => {
                write!(f, "witness {witness_id} has no inputs (zero derivation source)")
            }
        }
    }
}

impl std::error::Error for MeshError {}

/// Output of mesh assembly. `witnesses` and `edges` are
/// deterministically sorted; `errors` is the typed list of dropped
/// Witnesses with their failure reason (always non-fatal at this
/// layer — fatal validation happens in the Phase 9 audit).
pub struct AssembledMesh {
    pub witnesses: Vec<Witness>,
    pub edges: Vec<(WitnessId, WitnessId)>,
    pub errors: Vec<MeshError>,
    pub dedup_count: usize,
}

impl AssembledMesh {
    pub fn is_empty(&self) -> bool {
        self.witnesses.is_empty()
    }

    pub fn len(&self) -> usize {
        self.witnesses.len()
    }
}

/// Assemble a mesh from a stream of Witnesses.
///
/// The function is pure: same `witnesses` vector in any order →
/// same `AssembledMesh` output (modulo `dedup_count` which is just a
/// counter and not part of the hash).
pub fn assemble(witnesses: Vec<Witness>) -> AssembledMesh {
    let mut by_id: HashMap<WitnessId, Witness> = HashMap::with_capacity(witnesses.len());
    let mut errors: Vec<MeshError> = Vec::new();
    let mut dedup_count: usize = 0;

    // Pass 1: validate + dedup. We collect all valid witnesses first
    // so a SAFETY rule can check for the existence of its parent
    // unsafe-region witness regardless of input order.
    for witness in witnesses {
        if let Some(err) = validate_one(&witness) {
            errors.push(err);
            continue;
        }
        if by_id.contains_key(&witness.id) {
            dedup_count += 1;
            continue;
        }
        by_id.insert(witness.id, witness);
    }

    // Pass 2: SAFETY rule cross-check (needs the full set of valid
    // witnesses to look up parent unsafe-region by id).
    let safety_violations = check_safety_orphans(&by_id);
    for id in &safety_violations {
        if let Some(w) = by_id.remove(id) {
            warn!(
                witness_id = %w.id,
                rule = %w.rule,
                "dropping comment::SAFETY@v1 witness with no parent unsafe-region input"
            );
            errors.push(MeshError::SafetyOrphan { witness_id: w.id });
        }
    }

    // Pass 3: deterministic emit. Sort witnesses by id (lower-hex
    // ascending) and edges by (parent, child).
    let mut witnesses_out: Vec<Witness> = by_id.into_values().collect();
    witnesses_out.sort_by(|a, b| a.id.to_hex().cmp(&b.id.to_hex()));

    let mut edges: Vec<(WitnessId, WitnessId)> = Vec::new();
    for w in &witnesses_out {
        for input in &w.inputs {
            if let WitnessInput::WitnessRef { id: parent } = input {
                edges.push((*parent, w.id));
            }
        }
    }
    edges.sort_by(|a, b| {
        a.0.to_hex()
            .cmp(&b.0.to_hex())
            .then_with(|| a.1.to_hex().cmp(&b.1.to_hex()))
    });
    edges.dedup();

    AssembledMesh {
        witnesses: witnesses_out,
        edges,
        errors,
        dedup_count,
    }
}

fn validate_one(witness: &Witness) -> Option<MeshError> {
    if witness.spans.is_empty() {
        return Some(MeshError::EmptySpans {
            witness_id: witness.id,
        });
    }
    if witness.inputs.is_empty() {
        return Some(MeshError::EmptyInputs {
            witness_id: witness.id,
        });
    }
    if rule_catalog::get(&witness.rule).is_none() {
        return Some(MeshError::UnknownRule {
            witness_id: witness.id,
            rule: witness.rule.clone(),
        });
    }
    None
}

fn check_safety_orphans(by_id: &HashMap<WitnessId, Witness>) -> Vec<WitnessId> {
    let unsafe_ids: HashSet<WitnessId> = by_id
        .values()
        .filter(|w| w.witness_type == "code::unsafe-region")
        .map(|w| w.id)
        .collect();

    let mut violations = Vec::new();
    for w in by_id.values() {
        if w.rule != "comment::SAFETY@v1" {
            continue;
        }
        let has_unsafe_parent = w.inputs.iter().any(|input| match input {
            WitnessInput::WitnessRef { id } => unsafe_ids.contains(id),
            WitnessInput::ByteRef { .. } => false,
        });
        if !has_unsafe_parent {
            violations.push(w.id);
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use thinkingroot_core::types::{
        Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
    };

    fn span(file: &str, start: u64, end: u64) -> WitnessSpan {
        WitnessSpan {
            file_blake3: file.to_string(),
            start,
            end,
        }
    }

    fn function_witness() -> Witness {
        Witness::new(
            "tree-sitter::function-decl@v1",
            "declares::function",
            vec![WitnessInput::ByteRef {
                file_blake3: "f".into(),
                start: 0,
                end: 10,
            }],
            vec![span("f", 0, 10)],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            "deadbeef",
            Utc::now(),
        )
    }

    fn unsafe_witness() -> Witness {
        Witness::new(
            "tree-sitter::unsafe-block@v1",
            "code::unsafe-region",
            vec![WitnessInput::ByteRef {
                file_blake3: "f".into(),
                start: 5,
                end: 8,
            }],
            vec![span("f", 5, 8)],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            "deadbeef",
            Utc::now(),
        )
    }

    #[test]
    fn empty_input_produces_empty_mesh() {
        let m = assemble(vec![]);
        assert!(m.is_empty());
        assert_eq!(m.dedup_count, 0);
        assert!(m.errors.is_empty());
    }

    #[test]
    fn unknown_rule_is_rejected_with_typed_error() {
        let bad = Witness::new(
            "nonexistent::rule@v99",
            "garbage::type",
            vec![WitnessInput::ByteRef {
                file_blake3: "f".into(),
                start: 0,
                end: 1,
            }],
            vec![span("f", 0, 1)],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            "deadbeef",
            Utc::now(),
        );
        let m = assemble(vec![bad]);
        assert!(m.witnesses.is_empty());
        assert_eq!(m.errors.len(), 1);
        assert!(matches!(m.errors[0], MeshError::UnknownRule { .. }));
    }

    #[test]
    fn dedup_collapses_identical_witnesses() {
        let a = function_witness();
        let b = function_witness();
        // Different timestamps but same (rule, spans) → same id.
        assert_eq!(a.id, b.id);
        let m = assemble(vec![a, b]);
        assert_eq!(m.witnesses.len(), 1);
        assert_eq!(m.dedup_count, 1);
    }

    #[test]
    fn empty_inputs_is_rejected() {
        let mut w = function_witness();
        w.inputs.clear();
        let m = assemble(vec![w]);
        assert_eq!(m.errors.len(), 1);
        assert!(matches!(m.errors[0], MeshError::EmptyInputs { .. }));
    }

    #[test]
    fn empty_spans_is_rejected() {
        let mut w = function_witness();
        w.spans.clear();
        let m = assemble(vec![w]);
        assert_eq!(m.errors.len(), 1);
        assert!(matches!(m.errors[0], MeshError::EmptySpans { .. }));
    }

    #[test]
    fn safety_witness_without_unsafe_parent_is_dropped() {
        let safety = Witness::new(
            "comment::SAFETY@v1",
            "code::safety-justification",
            vec![WitnessInput::ByteRef {
                file_blake3: "f".into(),
                start: 0,
                end: 20,
            }],
            vec![span("f", 0, 20)],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.95),
            "cafebabe",
            Utc::now(),
        );
        let m = assemble(vec![safety]);
        assert!(m.witnesses.is_empty());
        assert_eq!(m.errors.len(), 1);
        assert!(matches!(m.errors[0], MeshError::SafetyOrphan { .. }));
    }

    #[test]
    fn safety_witness_with_unsafe_parent_is_kept() {
        let unsafe_w = unsafe_witness();
        let unsafe_id = unsafe_w.id;
        let safety = Witness::new(
            "comment::SAFETY@v1",
            "code::safety-justification",
            vec![
                WitnessInput::WitnessRef { id: unsafe_id },
                WitnessInput::ByteRef {
                    file_blake3: "f".into(),
                    start: 0,
                    end: 20,
                },
            ],
            vec![span("f", 0, 20)],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.95),
            "cafebabe",
            Utc::now(),
        );
        let m = assemble(vec![unsafe_w, safety]);
        assert_eq!(m.witnesses.len(), 2);
        assert!(m.errors.is_empty());
        // Edge from unsafe → safety should be emitted.
        assert!(
            m.edges.iter().any(|(p, _)| *p == unsafe_id),
            "expected edge with unsafe witness as parent"
        );
    }

    #[test]
    fn output_order_is_deterministic() {
        // Two runs with the inputs in opposite order produce the
        // same `witnesses` and `edges` byte-for-byte.
        let f1 = function_witness();
        let f2 = Witness::new(
            "tree-sitter::function-decl@v1",
            "declares::function",
            vec![WitnessInput::ByteRef {
                file_blake3: "g".into(),
                start: 0,
                end: 5,
            }],
            vec![span("g", 0, 5)],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            "deadbeef",
            Utc::now(),
        );
        let m_a = assemble(vec![f1.clone(), f2.clone()]);
        let m_b = assemble(vec![f2, f1]);
        let ids_a: Vec<_> = m_a.witnesses.iter().map(|w| w.id.to_hex()).collect();
        let ids_b: Vec<_> = m_b.witnesses.iter().map(|w| w.id.to_hex()).collect();
        assert_eq!(ids_a, ids_b);
    }
}
