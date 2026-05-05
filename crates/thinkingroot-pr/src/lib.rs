//! Knowledge Proposal layer (T0.4 from `docs/branch-system-improvements.md`).
//!
//! Closes the production-blocking gap where `MergePolicy::RequiresProposal`
//! gated branches at `thinkingroot-branch::merge::execute_merge_into:336-341`
//! pointed users at "open a Knowledge Proposal (T0.4) instead of a raw merge"
//! — but the proposal layer didn't exist.  This crate is that layer.
//!
//! # Lifecycle
//!
//! ```text
//!   open_proposal       review_proposal     merge_proposal
//!         │                    │                   │
//!         ▼                    ▼                   ▼
//!   Open ────► ChangesRequested ────► Approved ────► Merged
//!         │                                        ▲
//!         └────────► Closed                        │
//!                       (rejected by author)      ┌┘
//!                                                 │
//!                              raw merge after approval picks this up
//! ```
//!
//! Reaching `Approved` requires `min_reviewers` distinct approving
//! reviewers (configured per `MergePolicy::RequiresProposal { min_reviewers,
//! required_checks }` on the source branch).  Reviewers can either
//! `Approve`, `RequestChanges` (resets status to `ChangesRequested`),
//! or `Comment` (no status change, just adds to the review log).
//!
//! Once `Approved`, the merge gate at `merge.rs:336-341` looks the
//! proposal up and lets the merge proceed.  The proposal's status flips
//! to `Merged` after the merge succeeds; a failed merge leaves the
//! proposal `Approved` so a retry doesn't require a re-review.
//!
//! # Storage
//!
//! One TOML file per proposal at
//! `<workspace>/.thinkingroot-refs/proposals/<proposal_id>.toml`.  The
//! `proposal_id` is a ULID (lexicographically sortable, time-ordered),
//! which makes "list newest first" a trivial directory scan.  No
//! database, no index — simple files that round-trip through serde.
//!
//! # Invariants (load-bearing)
//!
//! - **Approved means at least `min_reviewers` distinct approve
//!   reviewers** — counted by reviewer identity, not by review count
//!   (a single reviewer toggling Approve→ChangesRequested→Approve still
//!   counts once).
//! - **Author cannot self-approve** — at least one approving reviewer
//!   must be different from `author`.  Prevents single-actor bypass of
//!   the `min_reviewers >= 1` constraint.
//! - **`RequestChanges` is sticky** — once any reviewer requests changes,
//!   status drops to `ChangesRequested` until that reviewer either
//!   re-reviews (any decision flips them back to their new state) or
//!   the author opens a fresh proposal.
//! - **Atomic file writes** via tmp + rename — concurrent reviewers
//!   never observe a torn TOML file.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use thinkingroot_core::Result;
use thinkingroot_core::error::Error;

/// Subdirectory under `.thinkingroot-refs/` carrying one TOML per
/// proposal.
pub const PROPOSALS_DIR: &str = "proposals";

/// One Knowledge Proposal — the proposal-review-approve gate that sits
/// in front of `MergePolicy::RequiresProposal` merges.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeProposal {
    /// Time-sortable ULID; doubles as the file name on disk.
    pub id: String,
    /// Branch the proposal is asking to merge from.
    pub source_branch: String,
    /// Branch the proposal is asking to merge into; `None` means main.
    #[serde(default)]
    pub target_branch: Option<String>,
    /// The principal who opened the proposal.  Stored as the
    /// canonical `Principal::identity()` string (mirrors the
    /// branch-system author convention).
    pub author: String,
    /// Free-form proposal description supplied by the author.
    #[serde(default)]
    pub description: Option<String>,
    /// All reviews recorded so far, in chronological order.  The
    /// effective per-reviewer decision is the LATEST review by that
    /// reviewer (older entries are kept for audit).
    #[serde(default)]
    pub reviews: Vec<ProposalReview>,
    /// Required-checks list copied from the branch's
    /// `MergePolicy::RequiresProposal { required_checks }` at proposal
    /// open time.  Frozen so a policy change post-open doesn't quietly
    /// loosen an in-flight proposal.
    #[serde(default)]
    pub required_checks: Vec<String>,
    /// Minimum distinct approving reviewers needed before the proposal
    /// can advance to `Approved`.  Copied from the branch policy at
    /// open time, same freeze rationale as `required_checks`.
    pub min_reviewers: u8,
    /// Current lifecycle status — derived from the latest reviews and
    /// merge action.  Persisted so callers don't have to recompute on
    /// every load.
    pub status: ProposalStatus,
    pub created_at: DateTime<Utc>,
    /// Set when the proposal status reaches `Merged`; `None` otherwise.
    #[serde(default)]
    pub merged_at: Option<DateTime<Utc>>,
}

/// Lifecycle states of a [`KnowledgeProposal`].
///
/// `serde(tag = "status")` so the TOML representation is
/// `status = "open"` rather than a nested `[status.open]` table — keeps
/// the on-disk file readable in `cat`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Awaiting reviews; no decision yet.
    Open,
    /// At least one reviewer has requested changes; author must
    /// address them before the proposal can advance.
    ChangesRequested,
    /// `min_reviewers` distinct approves have been recorded and no
    /// reviewer is currently in the `RequestChanges` state.  Eligible
    /// for merge via `merge_proposal`.
    Approved,
    /// Merged successfully via [`merge_proposal_marked`].
    Merged,
    /// Author closed the proposal without merging.  Terminal state.
    Closed,
}

/// A single review event.  The effective decision per reviewer is the
/// latest entry in [`KnowledgeProposal::reviews`] for that reviewer;
/// earlier entries are retained for audit only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalReview {
    /// Reviewer's `Principal::identity()` string (see
    /// `thinkingroot-serve::engine::Principal`).
    pub reviewer: String,
    pub decision: ReviewDecision,
    #[serde(default)]
    pub comment: Option<String>,
    pub at: DateTime<Utc>,
}

/// Per-reviewer decision recorded against a [`KnowledgeProposal`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewDecision {
    /// Reviewer signs off on the proposal as-is.
    Approve,
    /// Reviewer wants changes; status drops to `ChangesRequested`
    /// until they re-review or the author closes + reopens.
    RequestChanges,
    /// Comment-only — does NOT change proposal status.  Useful for
    /// "I noticed something but it's not blocking" notes.
    Comment,
}

/// Path to `<workspace>/.thinkingroot-refs/proposals/`.
fn proposals_dir(refs_dir: &Path) -> PathBuf {
    refs_dir.join(PROPOSALS_DIR)
}

/// Path to one proposal's TOML file.  `proposal_id` MUST be a ULID
/// produced by [`open_proposal`]; arbitrary user-supplied ids are
/// rejected by [`validate_id`] before reaching this helper.
fn proposal_path(refs_dir: &Path, proposal_id: &str) -> PathBuf {
    proposals_dir(refs_dir).join(format!("{proposal_id}.toml"))
}

/// Reject IDs that aren't valid ULIDs.  Defends both against
/// path-traversal (a `../foo` filename would write outside
/// `proposals/`) and against typos that would silently create a new
/// proposal under the wrong key.
fn validate_id(id: &str) -> Result<()> {
    if Ulid::from_string(id).is_err() {
        return Err(Error::Config(format!(
            "invalid proposal id `{id}` — expected a ULID (26 chars, Crockford base32)"
        )));
    }
    Ok(())
}

/// Open a new proposal against a `RequiresProposal`-gated branch.
///
/// `min_reviewers` and `required_checks` are frozen onto the proposal
/// at open time so a later policy change cannot quietly loosen this
/// proposal's gate.
pub fn open_proposal(
    refs_dir: &Path,
    source_branch: &str,
    target_branch: Option<&str>,
    author: &str,
    description: Option<String>,
    min_reviewers: u8,
    required_checks: Vec<String>,
) -> Result<KnowledgeProposal> {
    let now = Utc::now();
    let id = Ulid::new().to_string();
    let proposal = KnowledgeProposal {
        id: id.clone(),
        source_branch: source_branch.to_string(),
        target_branch: target_branch.map(String::from),
        author: author.to_string(),
        description,
        reviews: Vec::new(),
        required_checks,
        min_reviewers,
        status: ProposalStatus::Open,
        created_at: now,
        merged_at: None,
    };
    write_proposal(refs_dir, &proposal)?;
    tracing::info!(
        proposal_id = %id,
        source = %source_branch,
        target = ?target_branch,
        author = %author,
        "knowledge-pr: proposal opened"
    );
    Ok(proposal)
}

/// Append a review and recompute the proposal's status.
///
/// Status transitions:
/// - Any review on a `Merged` or `Closed` proposal returns
///   `Error::Config(...)` — terminal states are immutable.
/// - `RequestChanges` always drops status to `ChangesRequested`
///   (sticky until that reviewer re-reviews with a different
///   decision).
/// - `Approve` advances status to `Approved` IFF (a) no reviewer's
///   latest decision is `RequestChanges`, AND (b) the count of
///   distinct approving reviewers (excluding the author) >=
///   `min_reviewers`.
/// - `Comment` never changes status.
pub fn review_proposal(
    refs_dir: &Path,
    proposal_id: &str,
    reviewer: &str,
    decision: ReviewDecision,
    comment: Option<String>,
) -> Result<KnowledgeProposal> {
    validate_id(proposal_id)?;
    let mut proposal = read_proposal(refs_dir, proposal_id)?
        .ok_or_else(|| Error::Config(format!("proposal `{proposal_id}` not found")))?;
    if matches!(
        proposal.status,
        ProposalStatus::Merged | ProposalStatus::Closed
    ) {
        return Err(Error::Config(format!(
            "proposal `{proposal_id}` is in terminal state {:?} and cannot accept new reviews",
            proposal.status
        )));
    }
    proposal.reviews.push(ProposalReview {
        reviewer: reviewer.to_string(),
        decision,
        comment,
        at: Utc::now(),
    });
    proposal.status = recompute_status(&proposal);
    write_proposal(refs_dir, &proposal)?;
    tracing::info!(
        proposal_id = %proposal_id,
        reviewer = %reviewer,
        new_status = ?proposal.status,
        "knowledge-pr: review recorded"
    );
    Ok(proposal)
}

/// Author-initiated close.  Drops a non-terminal proposal into
/// `Closed`.  No-op (returns the proposal as-is) on already-terminal
/// states.
pub fn close_proposal(
    refs_dir: &Path,
    proposal_id: &str,
    closer: &str,
) -> Result<KnowledgeProposal> {
    validate_id(proposal_id)?;
    let mut proposal = read_proposal(refs_dir, proposal_id)?
        .ok_or_else(|| Error::Config(format!("proposal `{proposal_id}` not found")))?;
    if proposal.author != closer {
        return Err(Error::PermissionDenied {
            actor: closer.to_string(),
            action: format!("close proposal `{proposal_id}` (only the author may close)"),
        });
    }
    if matches!(
        proposal.status,
        ProposalStatus::Merged | ProposalStatus::Closed
    ) {
        return Ok(proposal);
    }
    proposal.status = ProposalStatus::Closed;
    write_proposal(refs_dir, &proposal)?;
    tracing::info!(
        proposal_id = %proposal_id,
        closer = %closer,
        "knowledge-pr: proposal closed by author"
    );
    Ok(proposal)
}

/// Mark a proposal as merged.  Called from
/// `thinkingroot-branch::merge::execute_merge_into` AFTER the merge
/// has actually succeeded — keeps the `Merged` status honest with the
/// branch registry.  No-op (idempotent) when already `Merged`.
///
/// Returns `Error::Config(...)` if the proposal isn't `Approved` —
/// the merge gate must have called [`find_approved_proposal`] first.
pub fn mark_proposal_merged(
    refs_dir: &Path,
    proposal_id: &str,
) -> Result<KnowledgeProposal> {
    validate_id(proposal_id)?;
    let mut proposal = read_proposal(refs_dir, proposal_id)?
        .ok_or_else(|| Error::Config(format!("proposal `{proposal_id}` not found")))?;
    if matches!(proposal.status, ProposalStatus::Merged) {
        return Ok(proposal);
    }
    if !matches!(proposal.status, ProposalStatus::Approved) {
        return Err(Error::Config(format!(
            "proposal `{proposal_id}` cannot be marked merged from status {:?} — \
             must be Approved first",
            proposal.status
        )));
    }
    proposal.status = ProposalStatus::Merged;
    proposal.merged_at = Some(Utc::now());
    write_proposal(refs_dir, &proposal)?;
    Ok(proposal)
}

/// Find an `Approved` proposal that authorises a merge from
/// `source_branch` into `target_branch` (`None` = main).  Used by the
/// `RequiresProposal` gate at
/// `thinkingroot-branch::merge::execute_merge_into:336-341` to decide
/// whether to allow a merge through.
///
/// Returns the most-recently-approved match, or `None` if no approved
/// proposal exists.
pub fn find_approved_proposal(
    refs_dir: &Path,
    source_branch: &str,
    target_branch: Option<&str>,
) -> Result<Option<KnowledgeProposal>> {
    let mut matches: Vec<KnowledgeProposal> = list_proposals(refs_dir)?
        .into_iter()
        .filter(|p| {
            p.source_branch == source_branch
                && p.target_branch.as_deref() == target_branch
                && matches!(p.status, ProposalStatus::Approved)
        })
        .collect();
    // ULID id is time-sortable — last() gives the newest approved match.
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(matches.pop())
}

/// Read a proposal by id.  Returns `Ok(None)` when the file doesn't
/// exist — distinct from a real I/O error.
pub fn read_proposal(refs_dir: &Path, proposal_id: &str) -> Result<Option<KnowledgeProposal>> {
    validate_id(proposal_id)?;
    let path = proposal_path(refs_dir, proposal_id);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
    let proposal: KnowledgeProposal =
        toml::from_str(&raw).map_err(|e| Error::Config(format!("parsing {path:?}: {e}")))?;
    Ok(Some(proposal))
}

/// List every proposal in the workspace, oldest-first by ULID order.
pub fn list_proposals(refs_dir: &Path) -> Result<Vec<KnowledgeProposal>> {
    let dir = proposals_dir(refs_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| Error::io_path(&dir, e))? {
        let entry = entry.map_err(|e| Error::io_path(&dir, e))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
        match toml::from_str::<KnowledgeProposal>(&raw) {
            Ok(p) => out.push(p),
            Err(e) => {
                // A malformed proposal file is logged but does not
                // poison the whole list — operators can `cat` the
                // file and fix or remove it.  Honesty rule #1: don't
                // silently hide real I/O errors, but a single bad
                // TOML on disk shouldn't stop `list_proposals` from
                // returning the rest.
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "knowledge-pr: skipping malformed proposal TOML"
                );
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Atomic write via tmp + rename so concurrent readers never see a
/// torn TOML file.
fn write_proposal(refs_dir: &Path, proposal: &KnowledgeProposal) -> Result<()> {
    let dir = proposals_dir(refs_dir);
    std::fs::create_dir_all(&dir).map_err(|e| Error::io_path(&dir, e))?;
    let path = proposal_path(refs_dir, &proposal.id);
    let body = toml::to_string_pretty(proposal)
        .map_err(|e| Error::Serialization(format!("serialising proposal: {e}")))?;
    thinkingroot_core::atomic_write(&path, body.as_bytes(), None)?;
    Ok(())
}

/// Recompute the proposal's lifecycle status from its review log.
///
/// Algorithm:
/// 1. Group reviews by reviewer; keep only the LATEST entry per
///    reviewer.  Earlier reviews are audit history, not active
///    decisions.
/// 2. If any latest decision is `RequestChanges` →
///    `ChangesRequested`.
/// 3. Otherwise count distinct latest-`Approve` reviewers, excluding
///    the proposal author (no self-approve).  If count >=
///    `min_reviewers` → `Approved`.
/// 4. Otherwise → `Open`.
fn recompute_status(proposal: &KnowledgeProposal) -> ProposalStatus {
    use std::collections::HashMap;
    // Latest review per reviewer (`reviews` is appended in order).
    let mut latest: HashMap<&str, &ProposalReview> = HashMap::new();
    for review in &proposal.reviews {
        latest.insert(review.reviewer.as_str(), review);
    }

    if latest
        .values()
        .any(|r| matches!(r.decision, ReviewDecision::RequestChanges))
    {
        return ProposalStatus::ChangesRequested;
    }

    let approve_count = latest
        .iter()
        .filter(|(reviewer, r)| {
            matches!(r.decision, ReviewDecision::Approve) && **reviewer != proposal.author
        })
        .count();

    if approve_count >= proposal.min_reviewers as usize {
        ProposalStatus::Approved
    } else {
        ProposalStatus::Open
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn refs(dir: &tempfile::TempDir) -> PathBuf {
        let r = dir.path().join(".thinkingroot-refs");
        std::fs::create_dir_all(&r).unwrap();
        r
    }

    #[test]
    fn open_then_read_round_trip() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let opened = open_proposal(
            &refs_dir,
            "feature/x",
            Some("main"),
            "alice",
            Some("Adds X.".into()),
            2,
            vec!["health_score".into()],
        )
        .unwrap();
        assert!(matches!(opened.status, ProposalStatus::Open));

        let loaded = read_proposal(&refs_dir, &opened.id).unwrap().unwrap();
        assert_eq!(loaded, opened);
    }

    #[test]
    fn invalid_id_rejected() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let result = read_proposal(&refs_dir, "../etc/passwd");
        assert!(matches!(result, Err(Error::Config(_))));
    }

    #[test]
    fn approve_advances_status_when_min_reviewers_met() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let opened = open_proposal(
            &refs_dir,
            "feature/x",
            None,
            "alice",
            None,
            2,
            vec![],
        )
        .unwrap();

        // First approve — still Open (need 2 distinct).
        let after_one = review_proposal(
            &refs_dir,
            &opened.id,
            "bob",
            ReviewDecision::Approve,
            None,
        )
        .unwrap();
        assert!(matches!(after_one.status, ProposalStatus::Open));

        // Second approve from a DIFFERENT reviewer — now Approved.
        let after_two = review_proposal(
            &refs_dir,
            &opened.id,
            "carol",
            ReviewDecision::Approve,
            None,
        )
        .unwrap();
        assert!(matches!(after_two.status, ProposalStatus::Approved));
    }

    #[test]
    fn author_self_approve_does_not_count() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let opened = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();

        let after = review_proposal(
            &refs_dir,
            &opened.id,
            "alice", // same as author
            ReviewDecision::Approve,
            None,
        )
        .unwrap();
        assert!(
            matches!(after.status, ProposalStatus::Open),
            "author self-approve must not advance status — got {:?}",
            after.status
        );
    }

    #[test]
    fn request_changes_drops_status_after_approve() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let opened = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();

        let approved = review_proposal(
            &refs_dir,
            &opened.id,
            "bob",
            ReviewDecision::Approve,
            None,
        )
        .unwrap();
        assert!(matches!(approved.status, ProposalStatus::Approved));

        // Carol requests changes — should drop status.
        let changes = review_proposal(
            &refs_dir,
            &opened.id,
            "carol",
            ReviewDecision::RequestChanges,
            None,
        )
        .unwrap();
        assert!(matches!(changes.status, ProposalStatus::ChangesRequested));
    }

    #[test]
    fn comment_does_not_change_status() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let opened = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();
        let after = review_proposal(
            &refs_dir,
            &opened.id,
            "bob",
            ReviewDecision::Comment,
            Some("nit: rename foo".into()),
        )
        .unwrap();
        assert!(matches!(after.status, ProposalStatus::Open));
        assert_eq!(after.reviews.len(), 1);
    }

    #[test]
    fn reviewer_can_flip_decision() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let opened = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();

        // Bob requests changes, then approves — final state is approved.
        review_proposal(
            &refs_dir,
            &opened.id,
            "bob",
            ReviewDecision::RequestChanges,
            None,
        )
        .unwrap();
        let after = review_proposal(
            &refs_dir,
            &opened.id,
            "bob",
            ReviewDecision::Approve,
            None,
        )
        .unwrap();
        assert!(
            matches!(after.status, ProposalStatus::Approved),
            "reviewer flipping RequestChanges → Approve must clear the gate"
        );
        assert_eq!(after.reviews.len(), 2, "history retained for audit");
    }

    #[test]
    fn close_only_by_author() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let opened = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();
        let denied = close_proposal(&refs_dir, &opened.id, "mallory");
        assert!(matches!(denied, Err(Error::PermissionDenied { .. })));

        let closed = close_proposal(&refs_dir, &opened.id, "alice").unwrap();
        assert!(matches!(closed.status, ProposalStatus::Closed));
    }

    #[test]
    fn mark_merged_requires_approved() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let opened = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();
        // Cannot mark merged from Open.
        let from_open = mark_proposal_merged(&refs_dir, &opened.id);
        assert!(matches!(from_open, Err(Error::Config(_))));

        // Approve, then mark merged — succeeds.
        review_proposal(
            &refs_dir,
            &opened.id,
            "bob",
            ReviewDecision::Approve,
            None,
        )
        .unwrap();
        let merged = mark_proposal_merged(&refs_dir, &opened.id).unwrap();
        assert!(matches!(merged.status, ProposalStatus::Merged));
        assert!(merged.merged_at.is_some());

        // Idempotent re-call.
        let again = mark_proposal_merged(&refs_dir, &opened.id).unwrap();
        assert!(matches!(again.status, ProposalStatus::Merged));
    }

    #[test]
    fn find_approved_proposal_picks_newest() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let p1 = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();
        review_proposal(&refs_dir, &p1.id, "bob", ReviewDecision::Approve, None).unwrap();

        // A second approved proposal for same source/target.
        let p2 = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();
        review_proposal(&refs_dir, &p2.id, "carol", ReviewDecision::Approve, None).unwrap();

        let found = find_approved_proposal(&refs_dir, "feature/x", None)
            .unwrap()
            .expect("must find an approved proposal");
        // ULIDs are time-sortable; newer wins.
        assert_eq!(found.id, p2.id);
    }

    #[test]
    fn find_approved_skips_closed_and_open() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let p_open = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();
        // Open, no approval.
        let _ = p_open;

        let p_closed = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();
        close_proposal(&refs_dir, &p_closed.id, "alice").unwrap();

        let found = find_approved_proposal(&refs_dir, "feature/x", None).unwrap();
        assert!(found.is_none(), "must not return non-Approved proposals");
    }

    #[test]
    fn list_proposals_skips_malformed_files_with_warn() {
        let dir = tempdir().unwrap();
        let refs_dir = refs(&dir);
        let p1 = open_proposal(&refs_dir, "feature/x", None, "alice", None, 1, vec![])
            .unwrap();

        // Drop a junk TOML file alongside the real one.
        let bad = proposals_dir(&refs_dir).join("01J0000000000000000000XXXX.toml");
        std::fs::write(&bad, "this is not valid toml \0").unwrap();

        let listed = list_proposals(&refs_dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, p1.id);
    }
}
