//! `WitnessRecord` — the on-wire Witness type for `.tr/3.2` packs.
//!
//! Lives in `witnesses.cbor` (the CBOR-encoded successor to
//! `claims.jsonl`). The CBOR canonical-sort encoding produced by
//! `ciborium::into_writer` is deterministic: same input mesh →
//! byte-identical `witnesses.cbor`.
//!
//! Field-level decisions worth recording:
//!
//! - `id` is the 64-char lower-hex of the in-engine `WitnessId`. We
//!   keep it as a String on the wire so the pack format does not have
//!   to commit to the `[u8; 32]` encoding choice the engine made; if
//!   the engine ever switches to a different hash function, the wire
//!   format stays valid.
//! - `inputs` is a typed enum (Witness | Bytes) so a consumer can tell
//!   "this Witness derives from another Witness in this pack" from
//!   "this Witness derives from primary bytes." Both forms carry
//!   strings (witness id hex or file_blake3 hex + offsets).
//! - `spans` is non-empty by construction — `WitnessRecord::new`
//!   demands at least one entry. The first span is the canonical
//!   anchor that `content_blake3` is computed over.
//! - `stmt` is **absent** by design. The witness span text is
//!   materialised from `source.tar.zst` at read time, never
//!   duplicated into the pack. This is the load-bearing change vs
//!   `ClaimRecord` (deleted in the Commit 2 cutover).

use serde::{Deserialize, Serialize};

/// One row in `witnesses.cbor`. See module docs for the canonical
/// CBOR encoding contract.
///
/// `Eq` is intentionally not derived — `confidence: f64` would force
/// us to wrap in `OrderedFloat<f64>` only to satisfy the trait, and
/// no current consumer needs `Eq`. `PartialEq` suffices for
/// round-trip tests and structural equality checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WitnessRecord {
    /// 64-char lower-hex BLAKE3 — the pack-scoped stable id. Matches
    /// `Witness.id.to_hex()` in the engine.
    pub id: String,

    /// Witness type, e.g. `"declares::function"`.
    #[serde(rename = "type")]
    pub witness_type: String,

    /// Rule name + version, e.g. `"tree-sitter::function-decl@v1"`.
    /// MUST exist in the pack's `rule_catalog.toml`; `tr-verify`
    /// fails the pack on a dangling rule reference.
    pub rule: String,

    /// Derivation inputs: zero or more parents (`Witness { id }`) and
    /// zero or more primary-byte references (`Bytes { file, start,
    /// end }`). At least one entry by construction.
    pub inputs: Vec<WitnessRecordInput>,

    /// Primary byte spans (file_blake3, start, end). At least one
    /// entry; the first is the canonical anchor for `content_blake3`.
    pub spans: Vec<WitnessRecordSpan>,

    /// Lower-hex BLAKE3 over `source.tar.zst[spans[0].file][start..end]`.
    /// Verified by `tr-verify` and at every Witness probe in the
    /// running daemon.
    pub content_blake3: String,

    /// Optional symbol — function/type name for code witnesses.
    /// Empty / absent for non-code witnesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,

    /// Sensitivity label (PascalCase: `Public` | `Internal` |
    /// `Confidential` | `Restricted`).
    pub sensitivity: String,

    /// Static rule confidence in `[0.0, 1.0)`. Inherited from the
    /// rule descriptor in `rule_catalog.toml`.
    pub confidence: f64,
}

/// One input entry in `WitnessRecord.inputs`. CBOR tag-discriminated
/// so consumers can distinguish "derives from another Witness" from
/// "derives from primary bytes."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WitnessRecordInput {
    /// Reference to another Witness in this pack by id (64-char hex).
    Witness {
        /// 64-char lower-hex BLAKE3 of the parent Witness.
        id: String,
    },
    /// Reference to primary bytes inside `source.tar.zst`.
    Bytes {
        /// POSIX path inside `source.tar.zst`.
        file: String,
        /// Inclusive byte offset within `file`.
        start: u64,
        /// Exclusive byte offset within `file`.
        end: u64,
    },
}

/// One span entry in `WitnessRecord.spans` — a (file, byte-range)
/// pointer into `source.tar.zst`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessRecordSpan {
    /// POSIX path inside `source.tar.zst`.
    pub file: String,
    /// Inclusive byte offset within `file`.
    pub start: u64,
    /// Exclusive byte offset within `file`.
    pub end: u64,
}

impl WitnessRecord {
    /// Minimal constructor for the required fields. `symbol` defaults
    /// to absent; the optional setter chain populates it.
    ///
    /// Panics if `spans` is empty or `inputs` is empty — both are
    /// byte-grounding invariants the engine enforces upstream, and a
    /// wire-format invariant on top of that. Producing a malformed
    /// `WitnessRecord` would let a corrupt pack slip past
    /// `tr-verify`'s structural checks.
    pub fn new(
        id: impl Into<String>,
        witness_type: impl Into<String>,
        rule: impl Into<String>,
        inputs: Vec<WitnessRecordInput>,
        spans: Vec<WitnessRecordSpan>,
        content_blake3: impl Into<String>,
        sensitivity: impl Into<String>,
        confidence: f64,
    ) -> Self {
        assert!(!spans.is_empty(), "WitnessRecord requires at least one span");
        assert!(!inputs.is_empty(), "WitnessRecord requires at least one input");
        Self {
            id: id.into(),
            witness_type: witness_type.into(),
            rule: rule.into(),
            inputs,
            spans,
            content_blake3: content_blake3.into(),
            symbol: None,
            sensitivity: sensitivity.into(),
            confidence,
        }
    }

    /// Attach a symbol identifier (function name / type name).
    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(file: &str, start: u64, end: u64) -> WitnessRecordSpan {
        WitnessRecordSpan {
            file: file.to_string(),
            start,
            end,
        }
    }

    fn sample_record() -> WitnessRecord {
        WitnessRecord::new(
            "0".repeat(64),
            "declares::function",
            "tree-sitter::function-decl@v1",
            vec![WitnessRecordInput::Bytes {
                file: "a".into(),
                start: 0,
                end: 5,
            }],
            vec![span("a", 0, 5)],
            "1".repeat(64),
            "Public",
            0.99,
        )
    }

    #[test]
    fn round_trip_required_fields_only() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        let back: WitnessRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn type_field_uses_wire_name() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains(r#""type":"declares::function""#),
            "wire field is `type` not `witness_type`: {json}"
        );
    }

    #[test]
    fn symbol_absent_is_dropped_from_output() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("symbol"), "{json}");
    }

    #[test]
    fn symbol_present_is_emitted() {
        let record = sample_record().with_symbol("my_function");
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""symbol":"my_function""#), "{json}");
    }

    #[test]
    fn input_kind_tags_are_snake_case() {
        let record = WitnessRecord::new(
            "0".repeat(64),
            "documents::function-summary",
            "rustdoc::function-summary@v1",
            vec![
                WitnessRecordInput::Witness {
                    id: "1".repeat(64),
                },
                WitnessRecordInput::Bytes {
                    file: "a".into(),
                    start: 0,
                    end: 5,
                },
            ],
            vec![span("a", 0, 5)],
            "2".repeat(64),
            "Public",
            0.99,
        );
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""kind":"witness""#), "{json}");
        assert!(json.contains(r#""kind":"bytes""#), "{json}");
    }

    #[test]
    #[should_panic(expected = "WitnessRecord requires at least one span")]
    fn empty_spans_panics() {
        WitnessRecord::new(
            "0".repeat(64),
            "t",
            "r@v1",
            vec![WitnessRecordInput::Bytes {
                file: "a".into(),
                start: 0,
                end: 5,
            }],
            vec![],
            "1".repeat(64),
            "Public",
            0.99,
        );
    }

    #[test]
    #[should_panic(expected = "WitnessRecord requires at least one input")]
    fn empty_inputs_panics() {
        WitnessRecord::new(
            "0".repeat(64),
            "t",
            "r@v1",
            vec![],
            vec![span("a", 0, 5)],
            "1".repeat(64),
            "Public",
            0.99,
        );
    }

    #[test]
    fn unknown_fields_round_trip_silently() {
        let raw = r#"{"id":"0","type":"t","rule":"r@v1","inputs":[{"kind":"bytes","file":"a","start":0,"end":1}],"spans":[{"file":"a","start":0,"end":1}],"content_blake3":"1","sensitivity":"Public","confidence":0.99,"some_future_key":"v"}"#;
        let parsed: WitnessRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.id, "0");
    }
}
