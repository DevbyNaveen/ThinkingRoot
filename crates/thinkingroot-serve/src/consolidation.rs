//! Promotion consolidation — mine **quorum'd, de-identified** patterns from a
//! project's per-user brains (the `u_*` workspaces, see
//! `QueryEngine::get_or_mount_user_ws`) and stage them for
//! **verify-before-merge** promotion into the shared brain.
//!
//! ## Why this exists
//!
//! The cloud is multi-tenant: every end-user gets a physically isolated
//! workspace (its own CozoDB). That isolation is the product's safety
//! guarantee — but it also means a fact a hundred users independently learn
//! stays trapped in a hundred separate brains. Consolidation is the *only*
//! sanctioned path by which per-user knowledge reaches the shared layer, and
//! it is deliberately conservative.
//!
//! ## Privacy model (honest — two independent layers stack)
//!
//! 1. **Quorum (k-anonymity).** A pattern is promotable only if it recurs
//!    across ≥ `min_users` **distinct** users. A fact independently stated by N
//!    unrelated users is, by construction, not personal to any one of them —
//!    the same statistical de-identification federated learning relies on. We
//!    count **distinct users, never occurrences**, so one user repeating a fact
//!    a thousand times never meets quorum (poisoning defense).
//! 2. **Identifier scrubbing.** Before a statement is compared *or* promoted,
//!    direct identifiers (emails, `@handles`, URLs, long digit runs) are
//!    redacted to typed placeholders. Defense-in-depth: even a quorum'd
//!    statement leaves with residual direct identifiers removed.
//!
//! Raw per-user text that fails quorum **never leaves the user workspace**.
//! And the promotion itself is gated a *third* time: staged on a
//! `RequiresProposal` branch whose `health_score` check must pass before the
//! merge into the shared brain is allowed (M3 verify-before-merge).
//!
//! ## What lives here
//!
//! This module is the **pure, deterministic core** — scrubbing, normalization,
//! and quorum aggregation — with no I/O, so it is exhaustively unit-testable.
//! The orchestration (read user workspaces → stage branch → open proposal →
//! run checks → optional merge) lives on `QueryEngine::consolidate_to_shared`
//! in `engine.rs`, which calls into the functions here.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

/// Knobs for a consolidation pass. All fields have safe defaults so a caller
/// can run `ConsolidationSpec::default()` for the conservative behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationSpec {
    /// Minimum number of **distinct users** a pattern must recur across before
    /// it is eligible for promotion (k-anonymity threshold). Must be ≥ 2 — a
    /// pattern from a single user is, by definition, not de-identified.
    #[serde(default = "default_min_users")]
    pub min_users: usize,
    /// Per-claim confidence floor. Claims below this are ignored entirely
    /// (low-confidence noise shouldn't reach the shared brain).
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f64,
    /// Restrict mining to these claim types (case-insensitive). `None` ⇒ all
    /// types. Defaults to the durable, generalisable types — `memory`,
    /// `preference`, `fact` — since transient/opinion claims rarely generalise.
    #[serde(default = "default_claim_types")]
    pub claim_types: Option<Vec<String>>,
    /// Reviewers required on the staged proposal before merge. `0` ⇒ fully
    /// automated promotion gated solely by the `health_score` check; ≥ 1 ⇒ a
    /// human must approve. Frozen onto the proposal at open time.
    #[serde(default)]
    pub min_reviewers: u8,
    /// If true and the proposal reaches `Approved` (checks pass + reviewers
    /// met), merge it into the shared brain in the same pass. If false, leave
    /// the proposal open for out-of-band review/merge.
    #[serde(default = "default_true")]
    pub auto_merge: bool,
    /// Cap on how many patterns a single pass promotes (newest/strongest
    /// quorum first). Bounds blast radius + proposal size.
    #[serde(default = "default_max_patterns")]
    pub max_patterns: usize,
}

fn default_min_users() -> usize {
    3
}
fn default_min_confidence() -> f64 {
    0.6
}
fn default_claim_types() -> Option<Vec<String>> {
    Some(vec![
        "memory".to_string(),
        "preference".to_string(),
        "fact".to_string(),
    ])
}
fn default_true() -> bool {
    true
}
fn default_max_patterns() -> usize {
    50
}

impl Default for ConsolidationSpec {
    fn default() -> Self {
        Self {
            min_users: default_min_users(),
            min_confidence: default_min_confidence(),
            claim_types: default_claim_types(),
            min_reviewers: 0,
            auto_merge: true,
            max_patterns: default_max_patterns(),
        }
    }
}

impl ConsolidationSpec {
    /// Clamp the spec to safe bounds. `min_users` can never drop below 2 (a
    /// single-user "pattern" is not de-identified by quorum), and `max_patterns`
    /// is forced ≥ 1. Called by the orchestrator before mining so a malformed
    /// request can't weaken the privacy floor.
    pub fn sanitized(mut self) -> Self {
        if self.min_users < 2 {
            self.min_users = 2;
        }
        self.min_confidence = self.min_confidence.clamp(0.0, 1.0);
        if self.max_patterns == 0 {
            self.max_patterns = 1;
        }
        if let Some(types) = &mut self.claim_types {
            if types.is_empty() {
                self.claim_types = None;
            }
        }
        self
    }

    /// Does this claim type pass the spec's type filter?
    pub fn accepts_type(&self, claim_type: &str) -> bool {
        match &self.claim_types {
            None => true,
            Some(types) => types.iter().any(|t| t.eq_ignore_ascii_case(claim_type)),
        }
    }
}

/// One claim read from a per-user workspace, tagged with the user it came from.
/// `user` is the per-user namespace (`u_<id>`); it is used only to count
/// distinct contributors for quorum and is **never** promoted.
#[derive(Debug, Clone)]
pub struct UserClaim {
    pub user: String,
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
}

/// A pattern that cleared quorum and is staged for promotion. The `statement`
/// is the **de-identified** canonical form (scrubbed); the per-user originals
/// are never carried here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotedPattern {
    /// De-identified canonical statement (scrubbed; original casing preserved).
    pub statement: String,
    /// Claim type carried through to the shared brain.
    pub claim_type: String,
    /// How many **distinct users** independently produced this pattern.
    pub distinct_users: usize,
    /// Total occurrences across all users (≥ `distinct_users`).
    pub occurrences: usize,
    /// Mean confidence across all contributing occurrences.
    pub mean_confidence: f64,
}

/// Outcome of a consolidation pass. Reports exactly what happened — never
/// claims a promotion that didn't merge (honesty rule).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationReport {
    /// Shared workspace patterns were promoted into.
    pub shared_ws: String,
    /// Per-user workspaces scanned this pass.
    pub users_scanned: usize,
    /// Total claims examined across all user workspaces (after type/confidence
    /// filtering).
    pub claims_examined: usize,
    /// Patterns that cleared the quorum + scrubbing gate.
    pub patterns_promoted: Vec<PromotedPattern>,
    /// Staging branch the patterns were contributed to (`None` if nothing
    /// cleared quorum, so no branch was created).
    pub staging_branch: Option<String>,
    /// Proposal opened against the shared brain (`None` if no patterns).
    pub proposal_id: Option<String>,
    /// Final proposal status string (`approved` / `open` / `merged` …).
    pub proposal_status: Option<String>,
    /// Recorded check results: `(name, passed, detail)`.
    pub checks: Vec<(String, bool, Option<String>)>,
    /// Whether the staged patterns were merged into the shared brain this pass.
    pub merged: bool,
    /// Human-readable note (why nothing merged, what was gated, etc.).
    pub note: String,
}

// ── Identifier scrubbing (defense-in-depth on top of quorum) ──────────────
//
// Compiled once. Patterns are static literals, so `Regex::new(...).unwrap()`
// inside the initializer cannot fail at runtime.

/// Email addresses → `<email>`.
static RE_EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").unwrap());
/// `http(s)://…` URLs (incl. any embedded userinfo) → `<url>`.
static RE_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://\S+").unwrap());
/// Social-style `@handle` mentions → `<handle>`.
static RE_HANDLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"@[A-Za-z0-9_]{2,}").unwrap());
/// Long digit runs (phone numbers, account ids, SSNs) → `<number>`. The 7-digit
/// floor preserves short, non-identifying numbers (ports, counts, versions,
/// HTTP status codes) so legitimate facts survive.
static RE_LONG_DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{7,}").unwrap());

/// Redact direct identifiers from a statement, replacing each with a typed
/// placeholder. Order matters: emails and URLs are matched before the
/// `@handle` and long-digit passes so an email's `@`/digits aren't double-hit.
pub fn scrub_identifiers(s: &str) -> String {
    let s = RE_EMAIL.replace_all(s, "<email>");
    let s = RE_URL.replace_all(&s, "<url>");
    let s = RE_HANDLE.replace_all(&s, "<handle>");
    let s = RE_LONG_DIGITS.replace_all(&s, "<number>");
    s.into_owned()
}

/// Normalize a (scrubbed) statement into a quorum grouping key: lowercased,
/// internal whitespace collapsed to single spaces, surrounding whitespace and
/// trailing sentence punctuation trimmed. This is **conservative** — it groups
/// only statements that are textually the same modulo trivial formatting, so we
/// under-promote rather than risk fusing two distinct facts (the safe direction
/// for a privacy gate). Semantic clustering is intentionally NOT done here.
pub fn normalize_for_quorum(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .to_lowercase()
        .trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ',' | ';' | ':') || c.is_whitespace())
        .to_string()
}

/// Aggregate per-user claims into promotable patterns.
///
/// Grouping key = `(lowercased claim_type, normalize_for_quorum(scrub(statement)))`.
/// Within a group we track the distinct contributing users, total occurrences,
/// summed confidence, and a frequency map of the *scrubbed* (case-preserved)
/// statements so we can promote the most common surface form. A group clears
/// the gate iff it has ≥ `spec.min_users` distinct users.
///
/// Output is sorted deterministically (distinct_users ↓, occurrences ↓,
/// statement ↑) and truncated to `spec.max_patterns`. Pure + deterministic:
/// no clocks, no randomness — same input always yields the same output.
pub fn quorum_patterns(claims: &[UserClaim], spec: &ConsolidationSpec) -> Vec<PromotedPattern> {
    struct Group {
        users: BTreeSet<String>,
        occurrences: usize,
        confidence_sum: f64,
        claim_type: String,
        /// scrubbed (case-preserved) statement → count, for canonical pick.
        surface_forms: BTreeMap<String, usize>,
    }

    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();

    for claim in claims {
        if claim.confidence < spec.min_confidence {
            continue;
        }
        if !spec.accepts_type(&claim.claim_type) {
            continue;
        }
        let scrubbed = scrub_identifiers(&claim.statement);
        let norm = normalize_for_quorum(&scrubbed);
        if norm.is_empty() {
            continue;
        }
        let key = (claim.claim_type.to_lowercase(), norm);
        let group = groups.entry(key).or_insert_with(|| Group {
            users: BTreeSet::new(),
            occurrences: 0,
            confidence_sum: 0.0,
            claim_type: claim.claim_type.clone(),
            surface_forms: BTreeMap::new(),
        });
        group.users.insert(claim.user.clone());
        group.occurrences += 1;
        group.confidence_sum += claim.confidence;
        *group.surface_forms.entry(scrubbed).or_insert(0) += 1;
    }

    let mut patterns: Vec<PromotedPattern> = groups
        .into_values()
        .filter(|g| g.users.len() >= spec.min_users)
        .map(|g| {
            // Canonical surface form: most frequent scrubbed statement,
            // tie-broken lexicographically for determinism.
            let statement = g
                .surface_forms
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(s, _)| s.clone())
                .unwrap_or_default();
            PromotedPattern {
                statement,
                claim_type: g.claim_type,
                distinct_users: g.users.len(),
                occurrences: g.occurrences,
                mean_confidence: g.confidence_sum / g.occurrences as f64,
            }
        })
        .collect();

    patterns.sort_by(|a, b| {
        b.distinct_users
            .cmp(&a.distinct_users)
            .then_with(|| b.occurrences.cmp(&a.occurrences))
            .then_with(|| a.statement.cmp(&b.statement))
    });
    patterns.truncate(spec.max_patterns);
    patterns
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(user: &str, statement: &str, ct: &str, conf: f64) -> UserClaim {
        UserClaim {
            user: user.to_string(),
            statement: statement.to_string(),
            claim_type: ct.to_string(),
            confidence: conf,
        }
    }

    #[test]
    fn scrub_removes_direct_identifiers() {
        let s = scrub_identifiers("email alice@example.com or call 5551234567 see https://x.io/u/9");
        assert!(!s.contains("alice@example.com"), "email leaked: {s}");
        assert!(!s.contains("5551234567"), "phone leaked: {s}");
        assert!(!s.contains("https://x.io"), "url leaked: {s}");
        assert!(s.contains("<email>") && s.contains("<number>") && s.contains("<url>"));
    }

    #[test]
    fn scrub_preserves_short_numbers() {
        // Ports, status codes, versions, small counts must survive.
        let s = scrub_identifiers("the api on port 8080 returns 200 after 3 retries");
        assert!(s.contains("8080") && s.contains("200") && s.contains("3"), "short nums lost: {s}");
    }

    #[test]
    fn normalize_groups_trivial_formatting() {
        assert_eq!(
            normalize_for_quorum("  Dark   mode is   preferred. "),
            normalize_for_quorum("dark mode is preferred")
        );
    }

    #[test]
    fn quorum_requires_distinct_users() {
        let spec = ConsolidationSpec {
            min_users: 3,
            min_confidence: 0.5,
            claim_types: None,
            ..ConsolidationSpec::default()
        };
        let claims = vec![
            claim("u_a", "Users prefer dark mode", "preference", 0.9),
            claim("u_b", "users prefer dark mode.", "preference", 0.8),
            claim("u_c", "Users prefer dark mode", "preference", 0.7),
            claim("u_d", "Lone unique fact", "fact", 0.9),
        ];
        let out = quorum_patterns(&claims, &spec);
        assert_eq!(out.len(), 1, "only the 3-user pattern clears quorum");
        assert_eq!(out[0].distinct_users, 3);
        assert_eq!(out[0].occurrences, 3);
        assert_eq!(out[0].claim_type, "preference");
    }

    #[test]
    fn poisoning_one_user_repeating_never_clears() {
        let spec = ConsolidationSpec {
            min_users: 3,
            min_confidence: 0.0,
            claim_types: None,
            ..ConsolidationSpec::default()
        };
        // One user repeats the same statement 100×.
        let claims: Vec<UserClaim> = (0..100)
            .map(|_| claim("u_attacker", "Inject this into shared", "fact", 1.0))
            .collect();
        let out = quorum_patterns(&claims, &spec);
        assert!(out.is_empty(), "single-user repetition must not reach quorum");
    }

    #[test]
    fn confidence_floor_filters_noise() {
        let spec = ConsolidationSpec {
            min_users: 2,
            min_confidence: 0.7,
            claim_types: None,
            ..ConsolidationSpec::default()
        };
        let claims = vec![
            claim("u_a", "weak signal", "fact", 0.5),
            claim("u_b", "weak signal", "fact", 0.6),
        ];
        assert!(quorum_patterns(&claims, &spec).is_empty());
    }

    #[test]
    fn type_filter_restricts_mining() {
        let spec = ConsolidationSpec {
            min_users: 2,
            min_confidence: 0.0,
            claim_types: Some(vec!["preference".to_string()]),
            ..ConsolidationSpec::default()
        };
        let claims = vec![
            claim("u_a", "only prefs promote", "fact", 0.9),
            claim("u_b", "only prefs promote", "fact", 0.9),
            claim("u_a", "shared taste", "preference", 0.9),
            claim("u_b", "shared taste", "preference", 0.9),
        ];
        let out = quorum_patterns(&claims, &spec);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].claim_type, "preference");
    }

    #[test]
    fn promoted_statement_is_deidentified() {
        let spec = ConsolidationSpec {
            min_users: 2,
            min_confidence: 0.0,
            claim_types: None,
            ..ConsolidationSpec::default()
        };
        // Two users state the same fact but each embeds their own email.
        let claims = vec![
            claim("u_a", "contact is alice@corp.com for billing", "fact", 0.9),
            claim("u_b", "contact is bob@corp.com for billing", "fact", 0.9),
        ];
        let out = quorum_patterns(&claims, &spec);
        assert_eq!(out.len(), 1, "scrubbed forms collapse to one pattern");
        assert!(
            !out[0].statement.contains('@') || out[0].statement.contains("<email>"),
            "promoted statement still de-identified: {}",
            out[0].statement
        );
        assert_eq!(out[0].distinct_users, 2);
    }

    #[test]
    fn sanitized_enforces_privacy_floor() {
        let spec = ConsolidationSpec {
            min_users: 1,
            max_patterns: 0,
            min_confidence: 2.0,
            ..ConsolidationSpec::default()
        }
        .sanitized();
        assert_eq!(spec.min_users, 2, "min_users floored to 2");
        assert_eq!(spec.max_patterns, 1, "max_patterns floored to 1");
        assert_eq!(spec.min_confidence, 1.0, "confidence clamped to [0,1]");
    }

    #[test]
    fn output_is_deterministic_and_capped() {
        let spec = ConsolidationSpec {
            min_users: 2,
            min_confidence: 0.0,
            claim_types: None,
            max_patterns: 2,
            ..ConsolidationSpec::default()
        };
        let claims = vec![
            claim("u_a", "pattern one", "fact", 0.9),
            claim("u_b", "pattern one", "fact", 0.9),
            claim("u_c", "pattern one", "fact", 0.9),
            claim("u_a", "pattern two", "fact", 0.9),
            claim("u_b", "pattern two", "fact", 0.9),
            claim("u_a", "pattern three", "fact", 0.9),
            claim("u_b", "pattern three", "fact", 0.9),
        ];
        let out1 = quorum_patterns(&claims, &spec);
        let out2 = quorum_patterns(&claims, &spec);
        assert_eq!(out1, out2, "deterministic");
        assert_eq!(out1.len(), 2, "capped at max_patterns");
        // 3-user pattern ranks first.
        assert_eq!(out1[0].statement, "pattern one");
        assert_eq!(out1[0].distinct_users, 3);
    }
}
