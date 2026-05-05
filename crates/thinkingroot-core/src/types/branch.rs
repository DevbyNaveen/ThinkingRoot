// crates/thinkingroot-core/src/types/branch.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::Sensitivity;

/// A reference to a knowledge branch, tracking its lifecycle status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRef {
    /// Human-readable branch name, e.g. "feature/add-auth-docs".
    pub name: String,
    /// URL-safe slug derived from the name, e.g. "feature-add-auth-docs".
    pub slug: String,
    /// The parent branch this was forked from, e.g. "main".
    pub parent: String,
    /// When the branch was created.
    pub created_at: DateTime<Utc>,
    /// Current lifecycle status of the branch.
    pub status: BranchStatus,
    /// Optional human-readable description of the branch's purpose.
    pub description: Option<String>,
    /// Optional owner identity for SaaS / collaborative permission checks.
    #[serde(default)]
    pub owner: Option<String>,
    /// Branch-level read/write/merge permissions.
    #[serde(default)]
    pub permissions: BranchPermissions,
    /// First-class branch classification — replaces the historical
    /// `stream/` name-prefix convention with a typed discriminator
    /// (T0.6, branch-system-improvements §T0.6).
    #[serde(default)]
    pub kind: BranchKind,
    /// Merge gating policy — controls auto-merge / discard / proposal
    /// requirements at merge time (T0.6, branch-system-improvements §T0.6).
    #[serde(default)]
    pub merge_policy: MergePolicy,
    /// Optional outbound PII / sensitivity redaction policy applied at
    /// the response boundary for `brief`, `list_claims`, `search`, and
    /// `probe` paths (T2.6, branch-system-improvements §T2.6).
    ///
    /// Default `None` is "no redaction" — preserves the historical
    /// behaviour for every branch that pre-dates this field.
    #[serde(default)]
    pub redaction: Option<RedactionPolicy>,
    /// BLAKE3 hash of the parent's `graph.db` at fork time (T0.5,
    /// branch-system-improvements §T0.5).  Pinned so a three-way merge
    /// can identify the lowest common ancestor (LCA) and surface real
    /// conflicts where two-way diff would silently last-writer-win.
    ///
    /// `None` for branches created before T0.5 shipped — those keep
    /// using the existing two-way `compute_diff_into` path on merge,
    /// which is correct for the >99% of merges with no concurrent
    /// edits to the same claim.  Pre-T0.5 branches that DO have
    /// concurrent edits behave exactly as they did before this field
    /// existed; only new branches gain three-way semantics.
    #[serde(default)]
    pub parent_commit_hash: Option<String>,
}

/// First-class branch classification.
///
/// Replaces the historical `stream/{session_id}` *name prefix* convention
/// (still recognised as a fallback for branches created before T0.6) with
/// a typed discriminator that:
///
/// - Lets `maintenance::cleanup_once` filter by `kind == Stream` instead
///   of string-matching the name (avoids accidentally cleaning up a
///   user-created branch literally named `stream/foo`).
/// - Lets a future `Tag` variant gate write attempts at the permission
///   layer without re-reading the name.
/// - Lets the connector path (T0.7) attribute branches to a connector
///   without a parallel `is_connector_branch()` predicate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchKind {
    /// The workspace primary branch (typically named "main"). At most one
    /// per workspace; merges target a Main branch by default.
    Main,
    /// Long-lived named branch (the historical default — feature work,
    /// proposed changes). The default for new branches.
    #[default]
    Feature,
    /// Per-MCP-session branch, auto-created when
    /// `streams.auto_session_branch = true`. Disposed by
    /// `maintenance::cleanup_once` when the session goes idle.
    Stream { session_id: String },
    /// Disposable agent scratch space — typically `MergePolicy::Ephemeral`
    /// so it's discarded rather than merged on session end.
    Sandbox { agent_id: String },
    /// Immutable named ref pinning a target commit. Writes to a Tag are
    /// rejected by the permission layer (post-T2.5 ship).
    Tag { ref_name: String, target: String },
}

/// Branch merge gating policy.
///
/// Decides what happens when a merge into / out of this branch is
/// attempted. Each policy maps to a specific behaviour in
/// `thinkingroot-branch::merge` and the maintenance task:
///
/// - `Manual` — explicit `merge_branch` call required (current default).
/// - `AutoOnSessionEnd` — stream branches whose session has gone idle
///   may auto-merge if `KnowledgeDiff::merge_allowed` is true; otherwise
///   the maintenance task downgrades to abandon.
/// - `Ephemeral` — never merges; the maintenance task always abandons,
///   bypassing the merge pipeline entirely.
/// - `RequiresProposal` — merge is gated on an open + approved Knowledge
///   Proposal (T0.4, not yet implemented). Until T0.4 ships, raw merges
///   against a `RequiresProposal` branch are rejected unless `force=true`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum MergePolicy {
    /// Explicit merge call required (the historical default).
    #[default]
    Manual,
    /// Auto-merge stream branches when the session ends and health passes.
    AutoOnSessionEnd,
    /// Discard on session end; never merges.
    Ephemeral,
    /// Merge gated on an approved Knowledge Proposal (T0.4 gate).
    RequiresProposal {
        min_reviewers: u8,
        #[serde(default)]
        required_checks: Vec<String>,
    },
}

impl MergePolicy {
    /// True when this policy *requires* a proposal review before any
    /// merge. Used by `merge.rs` to short-circuit raw merges with a
    /// typed `Error::MergeBlocked` instead of silently merging
    /// (defense in depth until T0.4 lands the proposal layer).
    pub fn requires_proposal(&self) -> bool {
        matches!(self, MergePolicy::RequiresProposal { .. })
    }

    /// True when this policy means "never merge" — `merge.rs` short
    /// circuits to the discard / abandon path and `maintenance` skips
    /// the merge attempt entirely.
    pub fn is_ephemeral(&self) -> bool {
        matches!(self, MergePolicy::Ephemeral)
    }
}

/// Per-branch collaborative permissions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchPermissions {
    /// Identities allowed to read the branch when read-gating is enabled.
    #[serde(default)]
    pub readers: Vec<String>,
    /// Identities allowed to contribute new claims to the branch.
    #[serde(default)]
    pub writers: Vec<String>,
    /// Identities allowed to merge into or delete the branch.
    #[serde(default)]
    pub mergers: Vec<String>,
}

/// Lifecycle status of a knowledge branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BranchStatus {
    /// Branch is active and accepting changes.
    Active,
    /// Branch has been merged into its parent.
    Merged {
        merged_at: DateTime<Utc>,
        merged_by: MergedBy,
    },
    /// Branch was abandoned without merging.
    Abandoned { abandoned_at: DateTime<Utc> },
}

/// Records who or what performed a branch merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MergedBy {
    /// A human user performed the merge.
    Human { user: String },
    /// An AI agent performed the merge.
    Agent { agent_id: String },
    /// A connector (GitHub webhook, Slack archive, Notion sync, …)
    /// performed the merge. `install_id` disambiguates "alice's
    /// production slack" from "bob's production slack" — same
    /// connector_id, different installs, different attribution.
    Connector {
        connector_id: String,
        install_id: String,
    },
    /// Internal system actor (gc, maintenance, scheduled reflect).
    System,
}

/// Outbound surfaces that a `RedactionPolicy` may filter.
///
/// Lets a single policy target only `brief` summaries (e.g. dashboards
/// scrubbed for screen-share) without affecting `list_claims` data
/// pulls, etc.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundMode {
    /// `get_workspace_brief_branched` and similar summary outputs.
    Brief,
    /// `list_claims_branched` listing API.
    ListClaims,
    /// `search_branched` and `hybrid_retrieve` retrieval results.
    Search,
    /// AEP `probe_engram` answer rows.
    Probe,
}

impl OutboundMode {
    /// All four modes — the default policy applies everywhere.
    pub fn all() -> Vec<Self> {
        vec![
            OutboundMode::Brief,
            OutboundMode::ListClaims,
            OutboundMode::Search,
            OutboundMode::Probe,
        ]
    }
}

/// Per-branch outbound PII / sensitivity redaction policy.
///
/// Two complementary mechanisms:
///
/// 1. **Pattern rewrite**: every regex in `patterns` is compiled on use
///    (cheap relative to a CozoDB query) and matches in claim
///    statements / brief excerpts are replaced with `replacement`.
///    Useful for "redact every email address" / "scrub AWS keys".
/// 2. **Sensitivity threshold**: if `min_sensitivity` is set, claims
///    whose `claims.sensitivity` column is `>= min_sensitivity` are
///    either dropped from the response (`drop_above_min = true`) or
///    have their `statement` rewritten to `"[redacted: <tier>]"`
///    (`drop_above_min = false`).
///
/// `modes` decides which outbound surfaces this applies to. An empty
/// `modes` vec means "all modes".
///
/// **Storage note:** patterns are stored as `Vec<String>` (not
/// `Vec<regex::Regex>`) because `regex::Regex` is not Serialize.
/// Each application compiles + caches via `regex::Regex::new`; for the
/// expected per-branch policy size (a handful of patterns) this is
/// well below the per-call latency budget.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactionPolicy {
    /// Regex patterns to match against claim statements and brief excerpts.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Replacement string (literal, no `$1` capture-group expansion;
    /// keeps redaction conservative — no surprise leakage via group
    /// substitution typo).
    #[serde(default = "default_replacement")]
    pub replacement: String,
    /// Outbound surfaces this policy applies to. Empty = all surfaces.
    #[serde(default)]
    pub modes: Vec<OutboundMode>,
    /// Drop or redact claims whose sensitivity is at or above this tier.
    /// `None` means "no sensitivity gating" (only pattern rewriting).
    #[serde(default)]
    pub min_sensitivity: Option<Sensitivity>,
    /// When `min_sensitivity` triggers, drop the claim entirely (`true`)
    /// versus rewrite its statement to `[redacted: <tier>]` and keep the
    /// row (`false`). Default `true` — drop is the safer default.
    #[serde(default = "default_true")]
    pub drop_above_min: bool,
}

fn default_replacement() -> String {
    "[redacted]".to_string()
}

fn default_true() -> bool {
    true
}

impl RedactionPolicy {
    /// True when this policy targets the given outbound surface.
    /// Empty `modes` matches every surface.
    pub fn applies_to(&self, mode: &OutboundMode) -> bool {
        self.modes.is_empty() || self.modes.iter().any(|m| m == mode)
    }

    /// Apply pattern rewrites to a borrowed string and return the
    /// (possibly-rewritten) result.
    ///
    /// Compilation errors per pattern are logged at WARN and the
    /// pattern is skipped — a malformed regex must never wedge an
    /// outbound response (the alternative is "redact-fail-closed",
    /// which would silently break every consumer the moment a typo
    /// reaches the policy file).
    pub fn rewrite(&self, input: &str) -> String {
        if self.patterns.is_empty() {
            return input.to_string();
        }
        let mut out = input.to_string();
        for raw in &self.patterns {
            match regex::Regex::new(raw) {
                Ok(re) => {
                    out = re.replace_all(&out, self.replacement.as_str()).into_owned();
                }
                Err(e) => {
                    tracing::warn!(
                        pattern = %raw,
                        "branch redaction pattern failed to compile (skipped): {e}"
                    );
                }
            }
        }
        out
    }

    /// Decide what to do with a claim of `sensitivity` per the
    /// `min_sensitivity` gate. Returns `None` to pass through, or
    /// `Some(text)` to substitute (when `drop_above_min = false`).
    /// When `drop_above_min = true` the caller is expected to filter
    /// the row out — see [`RedactionPolicy::should_drop`].
    pub fn redact_text(&self, sensitivity: Sensitivity) -> Option<String> {
        let Some(min) = self.min_sensitivity else {
            return None;
        };
        if sensitivity < min {
            return None;
        }
        if self.drop_above_min {
            None
        } else {
            Some(format!("[redacted: {}]", sensitivity.as_str()))
        }
    }

    /// True when a claim of this `sensitivity` should be dropped from
    /// the outbound response per the policy's drop-above-min rule.
    pub fn should_drop(&self, sensitivity: Sensitivity) -> bool {
        let Some(min) = self.min_sensitivity else {
            return false;
        };
        sensitivity >= min && self.drop_above_min
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn branch_ref_roundtrip() {
        let b = BranchRef {
            name: "feature/x".to_string(),
            slug: "feature-x".to_string(),
            parent: "main".to_string(),
            created_at: Utc::now(),
            status: BranchStatus::Active,
            description: Some("test branch".to_string()),
            owner: Some("alice".to_string()),
            permissions: BranchPermissions {
                readers: vec!["bob".to_string()],
                writers: vec!["carol".to_string()],
                mergers: vec!["dave".to_string()],
            },
            kind: BranchKind::default(),
            merge_policy: MergePolicy::default(),
            redaction: None,
        };
        assert_eq!(b.name, "feature/x");
        assert!(matches!(b.status, BranchStatus::Active));
        assert_eq!(b.owner.as_deref(), Some("alice"));
        assert_eq!(b.permissions.readers, vec!["bob"]);
        assert_eq!(b.kind, BranchKind::Feature);
        assert_eq!(b.merge_policy, MergePolicy::Manual);
        assert!(b.redaction.is_none());
    }

    #[test]
    fn merged_by_agent() {
        let mb = MergedBy::Agent {
            agent_id: "thinkingroot".to_string(),
        };
        assert!(matches!(mb, MergedBy::Agent { .. }));
    }

    #[test]
    fn merged_by_connector_carries_install_id() {
        let mb = MergedBy::Connector {
            connector_id: "github".into(),
            install_id: "alice-acme".into(),
        };
        match mb {
            MergedBy::Connector {
                connector_id,
                install_id,
            } => {
                assert_eq!(connector_id, "github");
                assert_eq!(install_id, "alice-acme");
            }
            _ => panic!("expected Connector variant"),
        }
    }

    #[test]
    fn branch_kind_stream_carries_session_id() {
        let k = BranchKind::Stream {
            session_id: "sess-42".into(),
        };
        match k {
            BranchKind::Stream { session_id } => assert_eq!(session_id, "sess-42"),
            _ => panic!("expected Stream variant"),
        }
    }

    #[test]
    fn merge_policy_helpers() {
        assert!(!MergePolicy::Manual.requires_proposal());
        assert!(!MergePolicy::Manual.is_ephemeral());
        assert!(MergePolicy::Ephemeral.is_ephemeral());
        assert!(
            MergePolicy::RequiresProposal {
                min_reviewers: 2,
                required_checks: vec!["health_score".into()],
            }
            .requires_proposal()
        );
    }

    #[test]
    fn old_branch_toml_loads_with_defaults() {
        // Pre-T0.6 branches.toml entries lack `kind`, `merge_policy`,
        // `redaction`. Backward compat is non-negotiable — every
        // existing user workspace would fail to mount otherwise.
        let toml_str = r#"
            name = "legacy"
            slug = "legacy"
            parent = "main"
            created_at = "2026-04-01T00:00:00Z"
            description = "pre-T0.6"

            [status]
            type = "Active"
        "#;
        let b: BranchRef = toml::from_str(toml_str).expect("legacy toml must round-trip");
        assert_eq!(b.kind, BranchKind::Feature);
        assert_eq!(b.merge_policy, MergePolicy::Manual);
        assert!(b.redaction.is_none());
        assert!(b.owner.is_none());
    }

    #[test]
    fn branch_kind_serializes_with_kind_tag() {
        let k = BranchKind::Sandbox {
            agent_id: "claude".into(),
        };
        let s = serde_json::to_string(&k).unwrap();
        assert!(s.contains("\"kind\":\"sandbox\""), "got: {s}");
        assert!(s.contains("\"agent_id\":\"claude\""), "got: {s}");
    }

    #[test]
    fn redaction_policy_pattern_rewrite() {
        let policy = RedactionPolicy {
            patterns: vec![r"\b[\w.+-]+@[\w.-]+\.[A-Za-z]{2,}\b".to_string()],
            replacement: "[email]".into(),
            modes: vec![],
            min_sensitivity: None,
            drop_above_min: true,
        };
        let out = policy.rewrite("contact alice@corp.com or bob@corp.com");
        assert_eq!(out, "contact [email] or [email]");
    }

    #[test]
    fn redaction_policy_min_sensitivity_drop() {
        let policy = RedactionPolicy {
            patterns: vec![],
            replacement: String::new(),
            modes: vec![],
            min_sensitivity: Some(Sensitivity::Confidential),
            drop_above_min: true,
        };
        assert!(policy.should_drop(Sensitivity::Confidential));
        assert!(policy.should_drop(Sensitivity::Restricted));
        assert!(!policy.should_drop(Sensitivity::Internal));
        assert!(!policy.should_drop(Sensitivity::Public));
        // Drop mode → text substitution is None (caller filters row out).
        assert!(policy.redact_text(Sensitivity::Confidential).is_none());
    }

    #[test]
    fn redaction_policy_min_sensitivity_substitute() {
        let policy = RedactionPolicy {
            patterns: vec![],
            replacement: String::new(),
            modes: vec![],
            min_sensitivity: Some(Sensitivity::Confidential),
            drop_above_min: false,
        };
        assert!(!policy.should_drop(Sensitivity::Confidential));
        assert_eq!(
            policy.redact_text(Sensitivity::Confidential),
            Some("[redacted: Confidential]".to_string())
        );
        assert!(policy.redact_text(Sensitivity::Public).is_none());
    }

    #[test]
    fn redaction_policy_modes_filter_surfaces() {
        let policy = RedactionPolicy {
            patterns: vec![],
            replacement: String::new(),
            modes: vec![OutboundMode::Search, OutboundMode::Probe],
            min_sensitivity: None,
            drop_above_min: true,
        };
        assert!(policy.applies_to(&OutboundMode::Search));
        assert!(policy.applies_to(&OutboundMode::Probe));
        assert!(!policy.applies_to(&OutboundMode::Brief));
        assert!(!policy.applies_to(&OutboundMode::ListClaims));

        // Empty modes vec means "every surface".
        let universal = RedactionPolicy::default();
        assert!(universal.applies_to(&OutboundMode::Brief));
        assert!(universal.applies_to(&OutboundMode::ListClaims));
        assert!(universal.applies_to(&OutboundMode::Search));
        assert!(universal.applies_to(&OutboundMode::Probe));
    }

    #[test]
    fn redaction_policy_invalid_regex_is_skipped() {
        let policy = RedactionPolicy {
            patterns: vec![r"[unbalanced".to_string(), r"\d+".to_string()],
            replacement: "<num>".into(),
            modes: vec![],
            min_sensitivity: None,
            drop_above_min: true,
        };
        // Bad pattern logs a warning but doesn't panic; good pattern still applies.
        assert_eq!(policy.rewrite("abc123def456"), "abc<num>def<num>");
    }
}
