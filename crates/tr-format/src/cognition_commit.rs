//! `CognitionCommitRecord` — the on-wire cognition-commit type for
//! `.tr/3.3` packs (Phase ζ.1 of the Cognition Commits design,
//! `docs/2026-05-15-cognition-commits-design.md`).
//!
//! Lives in `cognition_commits.cbor` (a new pack segment alongside
//! `witnesses.cbor` and `claims.jsonl`). CBOR canonical-sort encoding
//! via `ciborium::into_writer` makes the bytes deterministic across
//! runs and machines — same commit DAG → byte-identical
//! `cognition_commits.cbor`.
//!
//! Field-level decisions worth recording:
//!
//! - **`id` is wire-form hex, not raw bytes.** Matches the
//!   `WitnessRecord.id` convention: the pack format doesn't commit
//!   to the `[u8; 32]` engine encoding so future hash changes are
//!   absorbed by the engine layer alone.
//! - **`citations` + `witnesses_added` are witness-id hex strings**,
//!   pointing into the same pack's `witnesses.cbor`. `tr-verify`
//!   (ζ.2 enforcement) refuses a pack where a citation hex doesn't
//!   resolve to a Witness id in the same pack. Pack-scoped honesty:
//!   a citation that can't be byte-walked has no place in the
//!   on-disk record.
//! - **`author` is a tagged enum**, matching the engine's
//!   `CommitAuthor::{User, Agent}` shape. The wire tag is `kind`
//!   so consumers tell user-authored from agent-authored without
//!   string-sniffing the `principal` field.
//! - **`created_at` is ISO 8601 (RFC 3339) string**, not a CBOR
//!   epoch. Matches the manifest's `extracted_at` convention.
//!   Wall-clock timestamp is informational only — the commit id
//!   is content-derived and does NOT include `created_at`.
//! - **`prompt` and `reasoning` are present** (unlike `WitnessRecord`
//!   which deliberately omits `stmt` and rematerialises span text
//!   from source bytes). Reason: a cognition commit's prose IS the
//!   substrate — there are no source bytes to rematerialise from.
//!   The trade-off is wire size; commits are far rarer than
//!   witnesses, so the cost is bounded.

use serde::{Deserialize, Serialize};

/// One row in `cognition_commits.cbor`.
///
/// `Eq` is intentionally not derived — `CognitionCommitRecord`
/// contains no floats today, but a future field bump for confidence
/// or trust scores would make `Eq` invalid without `OrderedFloat`
/// wrapping. Matches `WitnessRecord`'s choice; downstream consumers
/// use `PartialEq` for round-trip tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CognitionCommitRecord {
    /// 64-char lower-hex BLAKE3 — the content-derived commit id.
    /// Matches `CommitId.to_hex()` in the engine.
    pub id: String,

    /// Parent commit id (64-char hex) or empty string for the genesis
    /// commit of a branch. Wire form is a string rather than
    /// `Option<String>` so empty parents emit `""` consistently
    /// across CBOR + JSON encodings (no `null` ambiguity).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,

    /// Branch this commit lives on. Joins against the engine's
    /// branch registry at mount time.
    pub branch: String,

    /// Who emitted the commit. Tagged enum matches the engine's
    /// `CommitAuthor::{User, Agent}` shape exactly.
    pub author: CognitionCommitAuthor,

    /// User prompt / system-event description that produced the
    /// commit. May be empty for the genesis commit on a branch.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,

    /// AI reasoning text or structured-event description, pre-citation.
    /// May contain `[[witness:<id>]]` markers; hybrid retrieval honours
    /// these when reading from a mounted pack.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,

    /// 64-char hex witness ids this commit produced. Every entry MUST
    /// resolve to a `WitnessRecord.id` in the same pack — `tr-verify`
    /// enforces in ζ.2.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub witnesses_added: Vec<String>,

    /// 64-char hex witness ids cited in `reasoning`. Same in-pack
    /// resolution invariant as `witnesses_added`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<String>,

    /// Gap ids surfaced by this commit. Free-form strings — gaps are
    /// not (yet) byte-anchored, so they don't carry the same
    /// resolution invariant as witnesses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps_surfaced: Vec<String>,

    /// ISO 8601 RFC 3339 timestamp when the commit was recorded.
    /// NOT part of the content-derived id (replaying tomorrow
    /// produces the same id but a fresh `created_at`).
    pub created_at: String,
}

/// Author of a cognition commit. The `kind` discriminator matches
/// the engine's `CommitAuthor` enum exactly so the pack reader can
/// reconstruct the in-engine variant without ambiguity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CognitionCommitAuthor {
    /// Human-driven commit (note saved, manual proposal, etc.).
    User {
        /// Surface-chosen user id (chat session owner, mount
        /// consumer id, etc.). Treat as opaque.
        id: String,
    },
    /// AI-driven commit. `model` is the model identifier
    /// (`claude-opus-4-7`); `principal` is the authorising agent
    /// per the branch-system Principal model (`thinkingroot` for
    /// the default in-app agent).
    Agent {
        /// Model identifier (e.g. `claude-opus-4-7`). Matches the
        /// engine-side `CommitAuthor::Agent.model` field.
        model: String,
        /// Authorising-agent principal per the branch-system
        /// Principal model (e.g. `thinkingroot`). Matches the
        /// engine-side `CommitAuthor::Agent.principal` field.
        principal: String,
    },
}

impl CognitionCommitRecord {
    /// Minimal constructor for the required fields. Optional fields
    /// (prompt, reasoning, witnesses_added, citations, gaps_surfaced,
    /// parent) default to empty / `Vec::new()`. Callers chain setters
    /// to populate as needed.
    pub fn new(
        id: impl Into<String>,
        branch: impl Into<String>,
        author: CognitionCommitAuthor,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            parent: String::new(),
            branch: branch.into(),
            author,
            prompt: String::new(),
            reasoning: String::new(),
            witnesses_added: Vec::new(),
            citations: Vec::new(),
            gaps_surfaced: Vec::new(),
            created_at: created_at.into(),
        }
    }

    /// Attach a parent commit hex id.
    pub fn with_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent = parent.into();
        self
    }

    /// Set the user-facing prompt.
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Set the AI reasoning text.
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = reasoning.into();
        self
    }

    /// Replace the witnesses-added list.
    pub fn with_witnesses_added(mut self, ids: Vec<String>) -> Self {
        self.witnesses_added = ids;
        self
    }

    /// Replace the citations list.
    pub fn with_citations(mut self, ids: Vec<String>) -> Self {
        self.citations = ids;
        self
    }

    /// Replace the gaps-surfaced list.
    pub fn with_gaps_surfaced(mut self, gaps: Vec<String>) -> Self {
        self.gaps_surfaced = gaps;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_agent_author() -> CognitionCommitAuthor {
        CognitionCommitAuthor::Agent {
            model: "claude-opus-4-7".to_string(),
            principal: "thinkingroot".to_string(),
        }
    }

    fn sample_record() -> CognitionCommitRecord {
        CognitionCommitRecord::new(
            "a".repeat(64),
            "main",
            sample_agent_author(),
            "2026-05-16T00:00:00Z",
        )
        .with_prompt("what is x?")
        .with_reasoning("x is y, see [[witness:bb...]]")
        .with_citations(vec!["b".repeat(64)])
    }

    #[test]
    fn round_trip_required_fields_only() {
        let record = CognitionCommitRecord::new(
            "0".repeat(64),
            "main",
            CognitionCommitAuthor::User {
                id: "alice".to_string(),
            },
            "2026-05-16T00:00:00Z",
        );
        let json = serde_json::to_string(&record).unwrap();
        let back: CognitionCommitRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn round_trip_full_record_with_setters() {
        let record = sample_record()
            .with_parent("c".repeat(64))
            .with_witnesses_added(vec!["d".repeat(64)])
            .with_gaps_surfaced(vec!["gap-1".to_string()]);
        let json = serde_json::to_string(&record).unwrap();
        let back: CognitionCommitRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn empty_parent_is_omitted_from_output() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            !json.contains("\"parent\""),
            "empty parent should be omitted: {json}"
        );
    }

    #[test]
    fn non_empty_parent_appears_in_output() {
        let record = sample_record().with_parent("c".repeat(64));
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"parent\""), "{json}");
    }

    #[test]
    fn empty_optional_lists_are_omitted() {
        let record = CognitionCommitRecord::new(
            "0".repeat(64),
            "main",
            sample_agent_author(),
            "2026-05-16T00:00:00Z",
        );
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("witnesses_added"), "{json}");
        assert!(!json.contains("citations"), "{json}");
        assert!(!json.contains("gaps_surfaced"), "{json}");
        assert!(!json.contains("prompt"), "{json}");
        assert!(!json.contains("reasoning"), "{json}");
    }

    #[test]
    fn author_kind_uses_snake_case_tag() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.contains(r#""kind":"agent""#),
            "expected agent kind tag: {json}"
        );
    }

    #[test]
    fn user_author_round_trips_via_kind_tag() {
        let record = CognitionCommitRecord::new(
            "0".repeat(64),
            "main",
            CognitionCommitAuthor::User {
                id: "alice".to_string(),
            },
            "2026-05-16T00:00:00Z",
        );
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""kind":"user""#), "{json}");
        let back: CognitionCommitRecord = serde_json::from_str(&json).unwrap();
        match back.author {
            CognitionCommitAuthor::User { id } => assert_eq!(id, "alice"),
            other => panic!("expected User author, got {other:?}"),
        }
    }

    #[test]
    fn cbor_round_trip_preserves_every_field() {
        let record = sample_record()
            .with_parent("c".repeat(64))
            .with_witnesses_added(vec!["d".repeat(64), "e".repeat(64)])
            .with_gaps_surfaced(vec!["gap-1".to_string(), "gap-2".to_string()]);
        let mut buf = Vec::new();
        ciborium::into_writer(&record, &mut buf).expect("CBOR encode");
        let decoded: CognitionCommitRecord =
            ciborium::from_reader(buf.as_slice()).expect("CBOR decode");
        assert_eq!(record, decoded);
    }

    #[test]
    fn unknown_fields_round_trip_silently() {
        // Forward-compat: a future v3.4 might add `health_score` or
        // similar — current readers should parse and drop it.
        let raw = r#"{
            "id": "0",
            "branch": "main",
            "author": {"kind":"agent","model":"m","principal":"p"},
            "created_at": "2026-05-16T00:00:00Z",
            "future_field": "extra-payload"
        }"#;
        let parsed: CognitionCommitRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.id, "0");
        assert_eq!(parsed.branch, "main");
    }
}
