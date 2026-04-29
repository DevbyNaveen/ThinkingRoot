//! v3 wire-format claim record — one entry per line in `claims.jsonl`.
//!
//! Field names match the v3 spec §3.3 verbatim. Required fields are
//! `id`, `stmt`, `ents`, `file`, `start`, `end`. Everything else is
//! optional and forward-compatible: unknown keys deserialize without
//! error, optional fields skip serialization when `None`/empty.
//!
//! The companion writer in `writer_v3` sorts records by `id` ascending
//! so byte-for-byte reproducibility holds across runs (locked per the
//! v3 implementation plan §10.2 acceptance criterion: "stable line
//! ordering by claim id, ascending").

use serde::{Deserialize, Serialize};

/// One line of `claims.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimRecord {
    /// Stable identifier scoped to the pack. Spec §3.3 says "stable
    /// identifier"; we use the CozoDB claim id verbatim.
    pub id: String,

    /// Natural-language atomic statement.
    pub stmt: String,

    /// Entity names this claim is about. Enables client-side graph
    /// rendering without a graph database in the pack.
    pub ents: Vec<String>,

    /// POSIX path inside `source.tar.zst`.
    pub file: String,

    /// Byte offset (inclusive) within `file`.
    pub start: u64,

    /// Byte offset (exclusive) within `file`.
    pub end: u64,

    /// Claim taxonomy tag (Definition, Behavior, Constraint, Relation,
    /// Decision, Plan, etc.). Wire field name is `"type"` per spec
    /// §3.3 — `claim_type` is the Rust struct field name (Rust keyword
    /// avoidance).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub claim_type: Option<String>,

    /// Extractor's confidence in `[0.0, 1.0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,

    /// Derivation chain — claim ids this claim was derived from. Empty
    /// for non-derived (extracted-from-source) claims.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,

    /// Rooting admission tier: `Rooted | Attested | Quarantined |
    /// Rejected`. Quarantined claims are emitted to the pack with this
    /// flag so consumers can choose to filter them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_tier: Option<String>,

    /// ISO 8601 timestamp the claim is *about* (not when extracted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_date: Option<String>,

    /// ISO 8601 timestamp of extraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_at: Option<String>,

    /// Model + version that produced the claim (e.g. `"gpt-5.4@2026-04"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extractor: Option<String>,
}

impl ClaimRecord {
    /// Minimal builder for the required fields. Optional fields default
    /// to absent; chain `with_*` setters to populate them.
    pub fn new(
        id: impl Into<String>,
        stmt: impl Into<String>,
        ents: Vec<String>,
        file: impl Into<String>,
        start: u64,
        end: u64,
    ) -> Self {
        Self {
            id: id.into(),
            stmt: stmt.into(),
            ents,
            file: file.into(),
            start,
            end,
            claim_type: None,
            confidence: None,
            parents: Vec::new(),
            admission_tier: None,
            event_date: None,
            extracted_at: None,
            extractor: None,
        }
    }

    /// Set the claim type (e.g. `"Definition"`, `"Behavior"`).
    pub fn with_claim_type(mut self, t: impl Into<String>) -> Self {
        self.claim_type = Some(t.into());
        self
    }

    /// Set the extractor's confidence.
    pub fn with_confidence(mut self, c: f64) -> Self {
        self.confidence = Some(c);
        self
    }

    /// Set the derivation parents.
    pub fn with_parents(mut self, parents: Vec<String>) -> Self {
        self.parents = parents;
        self
    }

    /// Set the admission tier.
    pub fn with_admission_tier(mut self, tier: impl Into<String>) -> Self {
        self.admission_tier = Some(tier.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_required_fields_only() {
        let c = ClaimRecord::new(
            "c-1",
            "useState returns a tuple",
            vec!["useState".into(), "tuple".into()],
            "react/hooks.md",
            4521,
            4598,
        );
        let json = serde_json::to_string(&c).unwrap();
        let back: ClaimRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn type_field_uses_wire_name() {
        let c = ClaimRecord::new("c-1", "x", vec![], "f.md", 0, 1).with_claim_type("Definition");
        let json = serde_json::to_string(&c).unwrap();
        assert!(
            json.contains(r#""type":"Definition""#),
            "wire field is `type` not `claim_type`: {json}"
        );
    }

    #[test]
    fn empty_optional_fields_dropped_from_output() {
        let c = ClaimRecord::new("c-1", "x", vec![], "f.md", 0, 1);
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("confidence"));
        assert!(!json.contains("parents"));
        assert!(!json.contains("admission_tier"));
    }

    #[test]
    fn unknown_fields_round_trip_silently() {
        let raw = r#"{"id":"c-1","stmt":"x","ents":[],"file":"f.md","start":0,"end":1,"some_future_key":"value"}"#;
        let parsed: ClaimRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.id, "c-1");
    }
}
