//! The Witness primitive — the atom of the Witness Mesh.
//!
//! A `Witness` is a typed, content-addressed unit derived from primary
//! bytes via a named rule from a fixed catalog. See the design spec
//! `docs/superpowers/specs/2026-05-10-witness-mesh-design.md` §2.
//!
//! Identity rule (load-bearing): `id = BLAKE3(rule || canonical_cbor(spans))`.
//! Same rule + same spans + same catalog version → same id, byte-for-byte.
//! Cross-pack, cross-workspace dedup falls out of the identity scheme.
//!
//! This file defines the in-memory shape. The wire format lives in
//! `crates/tr-format/src/witness.rs` (`WitnessRecord`); the CozoDB row
//! shape lives in `crates/thinkingroot-graph/src/graph.rs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

use crate::types::{Confidence, Sensitivity, SourceId, WorkspaceId};

/// Content-addressed witness identifier.
///
/// Inner storage is the raw 32 bytes of a BLAKE3 hash. The on-wire form
/// is lower-hex (64 characters) — matches what CozoDB stores in the
/// `witnesses.id` column and what `WitnessRecord.id` carries through
/// the pack format.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct WitnessId(pub [u8; 32]);

impl WitnessId {
    /// Derive a Witness id from its `(rule, spans)` tuple.
    ///
    /// The hash domain-separates by length-prefixing the rule name so
    /// `"foo" || "bar"` cannot collide with `"foobar" || ""`. Spans are
    /// canonicalised to their `(file_blake3, start, end)` triple — the
    /// same span set in a different in-memory order produces a different
    /// id, which is correct: span order is part of a Witness's identity
    /// (the first span is the canonical anchor for `content_blake3`).
    pub fn derive(rule: &str, spans: &[WitnessSpan]) -> Self {
        let mut hasher = blake3::Hasher::new();
        // Length-prefix the rule name (8 bytes little-endian) so rule
        // names of different lengths can never collide.
        hasher.update(&(rule.len() as u64).to_le_bytes());
        hasher.update(rule.as_bytes());
        // Length-prefix the span count.
        hasher.update(&(spans.len() as u64).to_le_bytes());
        for span in spans {
            hasher.update(&(span.file_blake3.len() as u64).to_le_bytes());
            hasher.update(span.file_blake3.as_bytes());
            hasher.update(&span.start.to_le_bytes());
            hasher.update(&span.end.to_le_bytes());
        }
        Self(*hasher.finalize().as_bytes())
    }

    /// Lower-hex (64 chars) for storage and wire format.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.0 {
            use std::fmt::Write as _;
            // Infallible on String; ignoring the Result is intentional.
            let _ = write!(&mut out, "{:02x}", byte);
        }
        out
    }

    /// Parse a 64-char lower-hex string. Rejects upper-case to keep
    /// the wire format canonical (saves us from "is this id the same
    /// as the one in the other pack?" ambiguity).
    pub fn from_hex(s: &str) -> Result<Self, WitnessIdParseError> {
        if s.len() != 64 {
            return Err(WitnessIdParseError::WrongLength(s.len()));
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let high = hex_nibble(chunk[0]).ok_or(WitnessIdParseError::NonHex)?;
            let low = hex_nibble(chunk[1]).ok_or(WitnessIdParseError::NonHex)?;
            out[i] = (high << 4) | low;
        }
        Ok(Self(out))
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WitnessIdParseError {
    #[error("witness id must be 64 lower-hex chars, got {0}")]
    WrongLength(usize),
    #[error("witness id contained non lower-hex characters")]
    NonHex,
}

impl fmt::Debug for WitnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Display for WitnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for WitnessId {
    type Err = WitnessIdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for WitnessId {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for WitnessId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// A primary-byte span — a (file, byte-range) pointer into
/// `source.tar.zst`.
///
/// `file_blake3` is the BLAKE3 of the canonicalised source bytes (line
/// endings + BOM-stripped per v3 writer). `start` is inclusive,
/// `end` is exclusive. Empty spans (start == end) are allowed for rules
/// that emit a Witness at a point location (e.g. cursor position from
/// LSP) but every Witness must have at least one span.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct WitnessSpan {
    pub file_blake3: String,
    pub start: u64,
    pub end: u64,
}

/// A Witness's derivation input — either another Witness by id, or a
/// raw byte reference into a source.
///
/// A "root" Witness has only `ByteRef` inputs (it derives directly from
/// bytes). A "leaf summary" Witness has only `WitnessRef` inputs (it
/// composes other Witnesses). Mixed inputs are allowed — a
/// `rustdoc::function-summary` Witness has a `ByteRef` to its docstring
/// bytes plus a `WitnessRef` to the function-decl Witness it documents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WitnessInput {
    WitnessRef { id: WitnessId },
    ByteRef { file_blake3: String, start: u64, end: u64 },
}

/// The in-memory Witness row.
///
/// CozoDB persistence shape (`witnesses` table) packs `inputs` and
/// `spans` into JSON-encoded String columns for query-time decode;
/// see `crates/thinkingroot-graph/src/graph.rs` for the DDL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Witness {
    /// Content-addressed identity. Derived via `WitnessId::derive`.
    pub id: WitnessId,
    /// Witness type, e.g. `"declares::function"`, `"calls"`,
    /// `"documents::function-summary"`, `"claim::@claim"`. Validated
    /// against the rule catalog at compile time.
    pub witness_type: String,
    /// Rule name + version, e.g. `"tree-sitter::function-decl@v1"`.
    /// Must exist in the rule catalog at compile time and at pack
    /// read time.
    pub rule: String,
    /// Derivation inputs. At least one entry — a Witness with no
    /// inputs is malformed and rejected by `witness_mesh::assemble`.
    pub inputs: Vec<WitnessInput>,
    /// Primary byte spans this Witness derives from. At least one
    /// entry — the first span is the canonical anchor that
    /// `content_blake3` is computed over.
    pub spans: Vec<WitnessSpan>,
    /// The source the Witness ultimately derives from. Stored for
    /// fast per-source rebuild (water-flow I-W4 snapshot consistency);
    /// always equals `spans[0].file_blake3` resolved to a source id.
    pub source: SourceId,
    /// Workspace scope.
    pub workspace: WorkspaceId,
    /// Sensitivity label inherited from the rule descriptor unless
    /// the rule body overrides at apply time (e.g. a TOML witness
    /// over a `.env`-shaped file may carry `Confidential`).
    pub sensitivity: Sensitivity,
    /// Static rule confidence. Tree-sitter witnesses carry 0.99;
    /// LSP-resolved witnesses carry 0.99; opt-in `// @claim`
    /// witnesses carry 0.95 (the human author may be wrong but the
    /// extraction is exact). No 1.0 — no rule is infallible.
    pub confidence: Confidence,
    /// I-4 tamper evidence — lower-hex BLAKE3 over
    /// `source_bytes[spans[0].start..spans[0].end]`. Re-verified on
    /// every probe; mismatches surface `StaleWitness`.
    pub content_blake3: String,
    /// Symbol identifier for code witnesses (function name, type
    /// name). Drives Phase 7e callee resolution. `None` for
    /// non-code witnesses.
    pub symbol: Option<String>,
    /// Wall-clock at compile time. Uses the canonical
    /// `pipeline_started_at` so re-runs of the same compile produce
    /// the same value (reproducibility).
    pub created_at: DateTime<Utc>,
    /// Validity window. `valid_from` matches `created_at` for newly
    /// derived witnesses. `valid_until` is `Some` only when a
    /// subsequent compile drops this Witness (its source bytes
    /// changed and the new bytes do not re-derive it).
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
}

impl Witness {
    /// Construct a new Witness with a content-addressed id derived
    /// from `(rule, spans)`. `content_blake3` is the responsibility
    /// of the caller because computing it requires reading source
    /// bytes; rule implementations stamp it at extraction time.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rule: impl Into<String>,
        witness_type: impl Into<String>,
        inputs: Vec<WitnessInput>,
        spans: Vec<WitnessSpan>,
        source: SourceId,
        workspace: WorkspaceId,
        sensitivity: Sensitivity,
        confidence: Confidence,
        content_blake3: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Self {
        let rule = rule.into();
        let id = WitnessId::derive(&rule, &spans);
        Self {
            id,
            witness_type: witness_type.into(),
            rule,
            inputs,
            spans,
            source,
            workspace,
            sensitivity,
            confidence,
            content_blake3: content_blake3.into(),
            symbol: None,
            created_at: now,
            valid_from: now,
            valid_until: None,
        }
    }

    /// Attach a symbol identifier (function/type name) for Phase 7e
    /// callee resolution.
    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }

    /// True iff this Witness has any `WitnessRef` input (i.e. it
    /// composes other Witnesses rather than deriving purely from
    /// bytes).
    pub fn is_derived(&self) -> bool {
        self.inputs
            .iter()
            .any(|i| matches!(i, WitnessInput::WitnessRef { .. }))
    }

    /// The canonical anchor span — `spans[0]`. Mandatory by
    /// construction (every Witness has at least one span).
    pub fn anchor_span(&self) -> &WitnessSpan {
        &self.spans[0]
    }
}

/// In-memory mesh produced by extraction. The CBOR pack writer
/// flattens this to a CBOR array; the read path rebuilds the DAG
/// from the embedded `inputs` chains.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WitnessMesh {
    pub witnesses: Vec<Witness>,
    /// `(parent_witness_id, child_witness_id)` edges — denormalised
    /// from `Witness.inputs.WitnessRef` for index-friendly graph
    /// traversal. The CozoDB `witness_input_edges` table mirrors
    /// this shape exactly.
    pub edges: Vec<(WitnessId, WitnessId)>,
}

impl WitnessMesh {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.witnesses.is_empty()
    }

    pub fn len(&self) -> usize {
        self.witnesses.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(file: &str, start: u64, end: u64) -> WitnessSpan {
        WitnessSpan {
            file_blake3: file.to_string(),
            start,
            end,
        }
    }

    #[test]
    fn witness_id_is_deterministic() {
        let s = vec![span("file_a", 0, 10)];
        let a = WitnessId::derive("tree-sitter::function-decl@v1", &s);
        let b = WitnessId::derive("tree-sitter::function-decl@v1", &s);
        assert_eq!(a, b);
    }

    #[test]
    fn witness_id_changes_with_rule() {
        let s = vec![span("file_a", 0, 10)];
        let a = WitnessId::derive("tree-sitter::function-decl@v1", &s);
        let b = WitnessId::derive("tree-sitter::function-decl@v2", &s);
        assert_ne!(a, b);
    }

    #[test]
    fn witness_id_changes_with_spans() {
        let a = WitnessId::derive("rule@v1", &[span("f", 0, 10)]);
        let b = WitnessId::derive("rule@v1", &[span("f", 0, 11)]);
        assert_ne!(a, b);
    }

    #[test]
    fn witness_id_length_prefix_prevents_collision() {
        // Without length-prefixing, ("foo" || "bar") and ("foobar" || "")
        // would produce the same byte stream. The length prefix prevents
        // this for both the rule and the span set.
        let s1 = vec![span("foobar", 0, 0)];
        let s2 = vec![span("foo", 0, 0), span("bar", 0, 0)];
        let a = WitnessId::derive("rule@v1", &s1);
        let b = WitnessId::derive("rule@v1", &s2);
        assert_ne!(a, b);
    }

    #[test]
    fn witness_id_hex_round_trip() {
        let original = WitnessId::derive("rule@v1", &[span("file", 0, 5)]);
        let hex = original.to_hex();
        assert_eq!(hex.len(), 64);
        let parsed = WitnessId::from_hex(&hex).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn witness_id_rejects_upper_hex() {
        let lower = WitnessId::derive("rule@v1", &[span("f", 0, 0)]).to_hex();
        let upper = lower.to_uppercase();
        assert!(matches!(
            WitnessId::from_hex(&upper),
            Err(WitnessIdParseError::NonHex)
        ));
    }

    #[test]
    fn witness_id_rejects_wrong_length() {
        assert!(matches!(
            WitnessId::from_hex("abc"),
            Err(WitnessIdParseError::WrongLength(3))
        ));
    }

    #[test]
    fn witness_id_serde_round_trip() {
        let id = WitnessId::derive("rule@v1", &[span("f", 0, 5)]);
        let json = serde_json::to_string(&id).unwrap();
        // Should be a JSON string of 64 lower-hex characters.
        assert_eq!(json.len(), 66); // 64 + 2 quotes
        let back: WitnessId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn witness_input_serde_uses_kind_tag() {
        let input = WitnessInput::WitnessRef {
            id: WitnessId::derive("r", &[span("f", 0, 0)]),
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains(r#""kind":"witness_ref""#), "{json}");

        let input = WitnessInput::ByteRef {
            file_blake3: "f".into(),
            start: 0,
            end: 1,
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains(r#""kind":"byte_ref""#), "{json}");
    }

    #[test]
    fn witness_is_derived_distinguishes_inputs() {
        let now = Utc::now();
        let root_w = Witness::new(
            "rule@v1",
            "declares::function",
            vec![WitnessInput::ByteRef {
                file_blake3: "f".into(),
                start: 0,
                end: 5,
            }],
            vec![span("f", 0, 5)],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            "deadbeef",
            now,
        );
        assert!(!root_w.is_derived());

        let derived_w = Witness::new(
            "rule@v2",
            "documents::function-summary",
            vec![
                WitnessInput::WitnessRef { id: root_w.id },
                WitnessInput::ByteRef {
                    file_blake3: "f".into(),
                    start: 5,
                    end: 10,
                },
            ],
            vec![span("f", 5, 10)],
            SourceId::new(),
            WorkspaceId::new(),
            Sensitivity::Public,
            Confidence::new(0.99),
            "cafebabe",
            now,
        );
        assert!(derived_w.is_derived());
    }

    #[test]
    fn witness_mesh_default_is_empty() {
        let m = WitnessMesh::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }
}
