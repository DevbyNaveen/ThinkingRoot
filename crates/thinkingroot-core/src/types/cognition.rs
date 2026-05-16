//! Cognition Commits — Phase β.1 of the design doc
//! (`docs/2026-05-15-cognition-commits-design.md`).
//!
//! A `CognitionCommit` is the unit of agentic thinking: every user-turn
//! and AI-turn that produces an observable cognitive change becomes a
//! content-addressed, branch-scoped, byte-anchored commit referencing
//! the witnesses it cited and the witnesses it added.
//!
//! Identity rule (load-bearing): the commit id is
//!
//! ```text
//! BLAKE3("cognition-commit-v1" ||
//!        parent_id_or_zero ||
//!        len(branch) || branch ||
//!        len(author_key) || author_key ||
//!        len(prompt) || prompt ||
//!        len(reasoning) || reasoning ||
//!        len(witnesses_sorted) || sorted_witness_bytes ||
//!        len(citations_sorted) || sorted_citation_bytes ||
//!        len(gaps_sorted) || sorted_gap_bytes)
//! ```
//!
//! Where:
//!   * Strings are length-prefixed (u64 LE) so concatenation cannot
//!     collide (`"foo"||"bar"` ≠ `"foobar"||""`).
//!   * Witness id lists are sorted by hex string ASCENDING before
//!     hashing so the same set of citations produces the same id
//!     regardless of in-memory iteration order. Order WITHIN
//!     `witnesses_added` and `citations` is not part of identity —
//!     these are sets of provenance pointers, not ordered sequences.
//!   * Gap ids are sorted by string for the same reason.
//!   * Domain separator `"cognition-commit-v1"` length-prefixed at the
//!     front prevents collision with witness ids, claim ids, or any
//!     other BLAKE3-derived identity in the system.
//!
//! Two commits with the same id are byte-for-byte the same cognition
//! event — useful for replay, dedup across packs, and merge-cognition
//! conflict detection in Phase γ.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

use crate::types::WitnessId;

/// Domain separator pinning every CommitId's BLAKE3 input. Bumping
/// this string is a schema-breaking change — every existing pack's
/// commit ids become invalid, so reserve for true wire breaks.
const COMMIT_ID_DOMAIN: &[u8] = b"cognition-commit-v1";

/// Content-addressed identifier for a `CognitionCommit`.
///
/// Inner storage is the raw 32 bytes of a BLAKE3 hash; the on-wire
/// form is lower-hex (64 characters) — matches the column type in
/// `cognition_commits.id` and the spec wire format.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommitId(pub [u8; 32]);

impl CommitId {
    /// Derive a commit id from its content. Pure, deterministic, and
    /// stable across runs / processes / machines — same inputs always
    /// produce the same id, byte-for-byte.
    pub fn from_content(
        parent: Option<&CommitId>,
        branch: &str,
        author: &CommitAuthor,
        prompt: &str,
        reasoning: &str,
        witnesses_added: &[WitnessId],
        citations: &[WitnessId],
        gaps_surfaced: &[String],
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        // Domain separator — length-prefixed so it can't collide with
        // a hand-crafted parent input of the same bytes.
        hasher.update(&(COMMIT_ID_DOMAIN.len() as u64).to_le_bytes());
        hasher.update(COMMIT_ID_DOMAIN);
        // Parent id (or 32 zero bytes for the root commit of a branch).
        match parent {
            Some(p) => {
                hasher.update(&[1u8]);
                hasher.update(&p.0);
            }
            None => {
                hasher.update(&[0u8]);
                hasher.update(&[0u8; 32]);
            }
        }
        update_length_prefixed_string(&mut hasher, branch);
        update_length_prefixed_string(&mut hasher, &author.identity());
        update_length_prefixed_string(&mut hasher, prompt);
        update_length_prefixed_string(&mut hasher, reasoning);
        update_length_prefixed_sorted_witness_ids(&mut hasher, witnesses_added);
        update_length_prefixed_sorted_witness_ids(&mut hasher, citations);
        update_length_prefixed_sorted_strings(&mut hasher, gaps_surfaced);
        Self(*hasher.finalize().as_bytes())
    }

    /// 32 raw bytes of the underlying BLAKE3 hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lower-hex (64 characters) for storage + wire format.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.0 {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "{:02x}", byte);
        }
        out
    }

    /// Parse a 64-char lower-hex string. Rejects upper-case to keep
    /// the wire format canonical (saves us from "is this id the same
    /// as the one in the other pack?" ambiguity).
    pub fn from_hex(s: &str) -> Result<Self, CommitIdParseError> {
        if s.len() != 64 {
            return Err(CommitIdParseError::WrongLength(s.len()));
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let high = hex_nibble(chunk[0]).ok_or(CommitIdParseError::NonHex)?;
            let low = hex_nibble(chunk[1]).ok_or(CommitIdParseError::NonHex)?;
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

fn update_length_prefixed_string(hasher: &mut blake3::Hasher, s: &str) {
    let bytes = s.as_bytes();
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn update_length_prefixed_sorted_witness_ids(
    hasher: &mut blake3::Hasher,
    ids: &[WitnessId],
) {
    // Sort by hex string so commit-id identity is independent of the
    // order in which the agent emitted witnesses / citations.
    let mut sorted: Vec<WitnessId> = ids.to_vec();
    sorted.sort_by_key(|w| w.to_hex());
    hasher.update(&(sorted.len() as u64).to_le_bytes());
    for w in &sorted {
        hasher.update(&w.0);
    }
}

fn update_length_prefixed_sorted_strings(hasher: &mut blake3::Hasher, items: &[String]) {
    let mut sorted: Vec<&String> = items.iter().collect();
    sorted.sort();
    hasher.update(&(sorted.len() as u64).to_le_bytes());
    for s in &sorted {
        update_length_prefixed_string(hasher, s);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommitIdParseError {
    #[error("commit id must be 64 lower-hex chars, got {0}")]
    WrongLength(usize),
    #[error("commit id contained non lower-hex characters")]
    NonHex,
}

impl fmt::Debug for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommitId({})", self.to_hex())
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for CommitId {
    type Err = CommitIdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for CommitId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for CommitId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Who authored a commit. Two variants only — every cognition event
/// is either a deliberate user action (note save, manual proposal,
/// etc.) or an AI-driven turn, with the model id + principal recorded
/// for provenance.
///
/// User identity is whatever the surface chose (chat session owner,
/// connector install, mount consumer). Agent identity carries the
/// model name (e.g. `claude-opus-4-7`) and the principal that
/// authorised the call (e.g. `Agent("thinkingroot")` per
/// `branch-system.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitAuthor {
    User { id: String },
    Agent { model: String, principal: String },
}

impl CommitAuthor {
    /// Canonical string form used in the BLAKE3 identity input and as
    /// the `author_id` / `author_model` projection in
    /// `cognition_commits`. `user:<id>` vs `agent:<model>:<principal>`.
    /// Stable across releases — bumping the format is a wire break.
    pub fn identity(&self) -> String {
        match self {
            CommitAuthor::User { id } => format!("user:{id}"),
            CommitAuthor::Agent { model, principal } => {
                format!("agent:{model}:{principal}")
            }
        }
    }
}

/// A single cognition event recorded against a branch.
///
/// Every field is part of the wire format — the design doc's
/// `CognitionCommit` projection is this struct verbatim. Adding a
/// field requires a coordinated update of `cognition_commits` table
/// + insert/list/get helpers + serde tests, exactly like every other
/// wire type in the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitionCommit {
    /// Content-derived id — `from_content(...)` is the single
    /// constructor; never accept an externally-supplied id (the
    /// audit would have to re-derive and compare anyway).
    pub id: CommitId,
    /// Parent commit on the same branch. `None` ONLY for the very
    /// first commit recorded on a branch — every subsequent commit
    /// must thread back to one.
    pub parent: Option<CommitId>,
    /// Branch this commit belongs to. Joins against
    /// `BranchRef.name` in `thinkingroot-branch::registry`.
    pub branch: String,
    /// Who emitted this commit.
    pub author: CommitAuthor,
    /// The user prompt or system-event description that produced
    /// this commit. Empty for the genesis commit of a branch.
    pub prompt: String,
    /// The AI's reasoning text or a structured-event description.
    /// Pre-citation; citation chips appear as `[[witness:<id>]]`
    /// markers that hybrid retrieval honours.
    pub reasoning: String,
    /// Witnesses this commit produced (e.g. comment claims the agent
    /// added, observation rows the auto-recorder emitted). Empty for
    /// pure-read commits.
    pub witnesses_added: Vec<WitnessId>,
    /// Witnesses cited in `reasoning`. Every entry MUST resolve to a
    /// real Witness in the workspace at insert time —
    /// `cognition_inserts::insert_cognition_commit` verifies this and
    /// refuses fabricated citations.
    pub citations: Vec<WitnessId>,
    /// Known-unknowns the agent flagged this turn. Each entry is a
    /// `gap_id` string from the existing `gaps` MCP tool. Empty when
    /// the agent didn't surface any.
    pub gaps_surfaced: Vec<String>,
    /// Wall-clock timestamp. NOT part of identity — same commit
    /// replayed tomorrow produces the same id but a fresh
    /// created_at.
    pub created_at: DateTime<Utc>,
}

impl CognitionCommit {
    /// Build a new commit with a freshly-derived id. The caller
    /// supplies every content-bearing field; `created_at` is stamped
    /// from the supplied `now` so tests can pin time.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        parent: Option<CommitId>,
        branch: String,
        author: CommitAuthor,
        prompt: String,
        reasoning: String,
        witnesses_added: Vec<WitnessId>,
        citations: Vec<WitnessId>,
        gaps_surfaced: Vec<String>,
        now: DateTime<Utc>,
    ) -> Self {
        let id = CommitId::from_content(
            parent.as_ref(),
            &branch,
            &author,
            &prompt,
            &reasoning,
            &witnesses_added,
            &citations,
            &gaps_surfaced,
        );
        Self {
            id,
            parent,
            branch,
            author,
            prompt,
            reasoning,
            witnesses_added,
            citations,
            gaps_surfaced,
            created_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-16T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn witness(byte: u8) -> WitnessId {
        WitnessId([byte; 32])
    }

    fn agent_author() -> CommitAuthor {
        CommitAuthor::Agent {
            model: "claude-opus-4-7".to_string(),
            principal: "thinkingroot".to_string(),
        }
    }

    #[test]
    fn commit_id_is_deterministic_across_runs() {
        let id1 = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "what is x?",
            "x is y",
            &[witness(1), witness(2)],
            &[witness(3)],
            &["gap-1".to_string()],
        );
        let id2 = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "what is x?",
            "x is y",
            &[witness(1), witness(2)],
            &[witness(3)],
            &["gap-1".to_string()],
        );
        assert_eq!(id1, id2);
    }

    #[test]
    fn commit_id_is_order_independent_for_citations() {
        // Same set of witnesses in different iteration order must
        // produce the same id — citations are a set of pointers, not
        // an ordered sequence.
        let id_a = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "q",
            "r",
            &[witness(1), witness(2), witness(3)],
            &[witness(4), witness(5)],
            &[],
        );
        let id_b = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "q",
            "r",
            &[witness(3), witness(1), witness(2)],
            &[witness(5), witness(4)],
            &[],
        );
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn commit_id_is_order_independent_for_gaps() {
        let id_a = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &["b".to_string(), "a".to_string(), "c".to_string()],
        );
        let id_b = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn commit_id_changes_when_parent_changes() {
        let p1 = CommitId([1u8; 32]);
        let p2 = CommitId([2u8; 32]);
        let id_a = CommitId::from_content(
            Some(&p1),
            "main",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &[],
        );
        let id_b = CommitId::from_content(
            Some(&p2),
            "main",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &[],
        );
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn commit_id_changes_when_branch_changes() {
        let id_a = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &[],
        );
        let id_b = CommitId::from_content(
            None,
            "feature/x",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &[],
        );
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn commit_id_distinguishes_no_parent_from_zero_parent_bytes() {
        // A None parent and an all-zero CommitId parent must NOT
        // collide — the 1-byte tag in the hash input keeps them
        // distinct.
        let zero = CommitId([0u8; 32]);
        let none_id = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &[],
        );
        let zero_id = CommitId::from_content(
            Some(&zero),
            "main",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &[],
        );
        assert_ne!(none_id, zero_id);
    }

    #[test]
    fn commit_id_domain_separates_from_witness_id_input() {
        // A WitnessId-shaped hash and a CommitId-shaped hash should
        // not collide even when the inner content is similar — the
        // domain separator at the front of the commit hash prevents
        // it.
        let pseudo_witness = WitnessId::derive(
            "cognition-commit-v1",
            &[crate::types::WitnessSpan {
                file_blake3: String::new(),
                start: 0,
                end: 0,
            }],
        );
        let commit_id = CommitId::from_content(
            None,
            "",
            &CommitAuthor::User {
                id: String::new(),
            },
            "",
            "",
            &[],
            &[],
            &[],
        );
        assert_ne!(pseudo_witness.0, commit_id.0);
    }

    #[test]
    fn commit_id_hex_round_trip() {
        let id = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "q",
            "r",
            &[witness(7)],
            &[],
            &[],
        );
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        let parsed = CommitId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn commit_id_from_hex_rejects_wrong_length() {
        let err = CommitId::from_hex("deadbeef").unwrap_err();
        match err {
            CommitIdParseError::WrongLength(8) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn commit_id_from_hex_rejects_non_lower_hex() {
        let upper = "F".repeat(64);
        assert!(matches!(
            CommitId::from_hex(&upper),
            Err(CommitIdParseError::NonHex)
        ));
    }

    #[test]
    fn commit_id_serde_round_trip() {
        let id = CommitId::from_content(
            None,
            "main",
            &agent_author(),
            "q",
            "r",
            &[],
            &[],
            &[],
        );
        let json = serde_json::to_string(&id).unwrap();
        // Serialised as a JSON string of 64 hex chars + 2 quotes = 66 bytes.
        assert_eq!(json.len(), 66);
        let parsed: CommitId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn commit_author_identity_strings_are_stable() {
        let user = CommitAuthor::User {
            id: "alice".to_string(),
        };
        let agent = CommitAuthor::Agent {
            model: "claude-opus-4-7".to_string(),
            principal: "thinkingroot".to_string(),
        };
        assert_eq!(user.identity(), "user:alice");
        assert_eq!(agent.identity(), "agent:claude-opus-4-7:thinkingroot");
    }

    #[test]
    fn cognition_commit_new_stamps_self_consistent_id() {
        let author = agent_author();
        let now = fixed_now();
        let commit = CognitionCommit::new(
            None,
            "main".to_string(),
            author.clone(),
            "what is x?".to_string(),
            "x is y".to_string(),
            vec![witness(1)],
            vec![witness(2)],
            vec!["gap-1".to_string()],
            now,
        );
        let recomputed = CommitId::from_content(
            None,
            "main",
            &author,
            "what is x?",
            "x is y",
            &[witness(1)],
            &[witness(2)],
            &["gap-1".to_string()],
        );
        assert_eq!(commit.id, recomputed);
        assert_eq!(commit.created_at, now);
    }

    #[test]
    fn cognition_commit_serde_round_trip() {
        let commit = CognitionCommit::new(
            Some(CommitId([9u8; 32])),
            "feature/x".to_string(),
            CommitAuthor::User {
                id: "alice".to_string(),
            },
            "save the note".to_string(),
            "ok".to_string(),
            vec![],
            vec![witness(7)],
            vec![],
            fixed_now(),
        );
        let json = serde_json::to_string(&commit).unwrap();
        let parsed: CognitionCommit = serde_json::from_str(&json).unwrap();
        assert_eq!(commit.id, parsed.id);
        assert_eq!(commit.parent, parsed.parent);
        assert_eq!(commit.branch, parsed.branch);
        assert_eq!(commit.author, parsed.author);
        assert_eq!(commit.citations.len(), parsed.citations.len());
    }
}
