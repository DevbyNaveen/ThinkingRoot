// crates/thinkingroot-branch/src/recovery.rs
//! Crash-safe merge recovery (T2.7 — folded with Stream A bug sweep).
//!
//! # Why
//!
//! `apply_branch_diff` snapshots `graph.db` to `graph.db.pre-merge-{slug}-{ts}`
//! before mutating the target, but pre-recovery a crashed merge would leave
//! the snapshot orphaned on disk and the operator had no automated way to
//! roll the target graph back.  Worse: post-A2 (vector-error promotion in
//! merge.rs) the merge can also Err mid-pipeline AFTER it inserted claims
//! into the target, so the target graph is in an undefined state until
//! someone runs `root branch rollback` manually.
//!
//! # How
//!
//! Two cooperating pieces:
//!
//! 1. **Intent write/clear lifecycle** in `execute_merge_into` /
//!    `execute_rebase`: write a `MergeIntent` to
//!    `<refs_dir>/merges_in_flight.toml` *after* the snapshot is taken
//!    but before any graph mutation.  Clear it only on full success
//!    (after `mark_merged`).  A crash, a panic, an `Err` propagation —
//!    any of these leave the intent in place.
//! 2. **Startup-scan recovery** via `recover_orphan_merges`: read every
//!    intent in the file; for each, locate the matching pre-merge
//!    snapshot, copy it back over the live `graph.db`, and clear the
//!    intent.  Idempotent — running it on a clean workspace is a no-op.
//!
//! # Invariants
//!
//! - Atomic write to `merges_in_flight.toml` (tmp + rename via
//!   `thinkingroot_core::atomic_write`) so readers never see torn state.
//! - Intent file uses `[[intent]]` array-of-tables so multiple racing
//!   crashed merges can each leave a record (cross-process scenario).
//! - Recovery is read-only on snapshots that don't match an intent;
//!   `gc_branches` is the existing path for cleaning up old snapshots.
//! - Branch-to-branch merge recovery (target != main) is supported via
//!   `target_branch: Option<String>` on `MergeIntent`; the snapshot
//!   lookup uses the corresponding branch's `graph.db` directory.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use thinkingroot_core::Result;
use thinkingroot_core::error::Error;

use crate::snapshot::{resolve_data_dir, slugify};

/// File name inside `.thinkingroot-refs/` carrying the in-flight
/// merge intents.  Always atomic-written; never appended to in place.
pub const INTENTS_FILE: &str = "merges_in_flight.toml";

/// One in-flight merge.  Persisted between the moment `execute_merge_into`
/// snapshots the target's `graph.db` and the moment it marks the source
/// branch merged.  If the process crashes in that window, this record
/// drives `recover_orphan_merges` on the next startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeIntent {
    /// Source branch being merged.
    pub source_branch: String,
    /// Target branch — `None` means main.
    #[serde(default)]
    pub target_branch: Option<String>,
    /// Wall-clock when the intent was recorded.  Used to disambiguate
    /// multiple intents for the same source branch (rare: cross-process
    /// crash sequence).
    pub started_at: DateTime<Utc>,
    /// String passed to `snapshot_target_db` when the snapshot was
    /// written — currently the source branch name (merge) or the target
    /// branch name (rebase).
    pub snapshot_subject: String,
    /// Either "pre-merge" or "pre-rebase".  Selects which file-name
    /// prefix to look for during recovery.
    pub snapshot_prefix: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IntentsFile {
    #[serde(default, rename = "intent")]
    intents: Vec<MergeIntent>,
}

/// Outcome of one recovery pass.  Recorded for the operator's audit
/// log and (optionally) surfaced via the desktop privacy dashboard.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RecoveryReport {
    /// Successfully rolled-back merges, with the snapshot path that
    /// was used and the intent's original timestamp.
    pub recovered: Vec<RecoveredMerge>,
    /// Intents whose pre-merge snapshot could not be located on disk
    /// (e.g. user-initiated `gc_branches` removed it).  These intents
    /// are still cleared from the file because there is nothing to roll
    /// back to — surface them so the operator knows the workspace is
    /// in a "merge ran partially, snapshot already gone" state.
    pub orphaned_intents_cleared: Vec<MergeIntent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveredMerge {
    pub source_branch: String,
    pub target_branch: Option<String>,
    pub started_at: DateTime<Utc>,
    pub restored_from: PathBuf,
}

fn intents_path(refs_dir: &Path) -> PathBuf {
    refs_dir.join(INTENTS_FILE)
}

fn read_intents(refs_dir: &Path) -> Result<IntentsFile> {
    let path = intents_path(refs_dir);
    if !path.exists() {
        return Ok(IntentsFile::default());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
    toml::from_str(&content).map_err(|e| Error::Config(e.to_string()))
}

fn write_intents(refs_dir: &Path, file: &IntentsFile) -> Result<()> {
    let path = intents_path(refs_dir);
    let content =
        toml::to_string_pretty(file).map_err(|e| Error::Serialization(e.to_string()))?;
    thinkingroot_core::atomic_write(&path, content.as_bytes(), None)?;
    Ok(())
}

/// Append a new merge intent.  The recovery file is read, the new
/// record appended, and the file rewritten atomically.  Concurrent
/// callers are serialised by [`crate::lock::RegistryLock`] (held by
/// the BranchRegistry mutating helpers around the same critical
/// section); merges themselves are serialised by [`crate::lock::MergeLock`]
/// so the read-modify-write window is safe.
pub fn write_merge_intent(refs_dir: &Path, intent: &MergeIntent) -> Result<()> {
    std::fs::create_dir_all(refs_dir).map_err(|e| Error::io_path(refs_dir, e))?;
    let mut file = read_intents(refs_dir)?;
    file.intents.push(intent.clone());
    write_intents(refs_dir, &file)?;
    Ok(())
}

/// Clear the intent matching `(source_branch, started_at)`.  Called
/// from the success path of `execute_merge_into` / `execute_rebase`
/// after the registry has been updated.  No-op if the intent file
/// doesn't exist or the matching record is already gone (idempotent).
pub fn clear_merge_intent(
    refs_dir: &Path,
    source_branch: &str,
    started_at: DateTime<Utc>,
) -> Result<()> {
    let mut file = read_intents(refs_dir)?;
    let before = file.intents.len();
    file.intents
        .retain(|i| !(i.source_branch == source_branch && i.started_at == started_at));
    if file.intents.len() == before {
        return Ok(());
    }
    if file.intents.is_empty() {
        let path = intents_path(refs_dir);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| Error::io_path(&path, e))?;
        }
        return Ok(());
    }
    write_intents(refs_dir, &file)?;
    Ok(())
}

/// Scan `<root>/.thinkingroot-refs/merges_in_flight.toml`; for each
/// intent, attempt to restore the target's `graph.db` from the
/// matching `graph.db.<prefix>-<slug>-<ts>` snapshot.  Returns a
/// [`RecoveryReport`] describing the outcome.
///
/// Always idempotent: if the intent file is absent or empty, returns
/// an empty report without touching the file system.  Safe to call
/// from any startup path — the cost is one stat per call.
pub fn recover_orphan_merges(root_path: &Path) -> Result<RecoveryReport> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    let file = read_intents(&refs_dir)?;
    if file.intents.is_empty() {
        return Ok(RecoveryReport::default());
    }

    let mut report = RecoveryReport::default();
    for intent in &file.intents {
        match restore_one(root_path, intent) {
            Ok(Some(snapshot_path)) => {
                report.recovered.push(RecoveredMerge {
                    source_branch: intent.source_branch.clone(),
                    target_branch: intent.target_branch.clone(),
                    started_at: intent.started_at,
                    restored_from: snapshot_path,
                });
            }
            Ok(None) => {
                // No matching snapshot found — the merge crashed before
                // the snapshot was actually written, OR the snapshot
                // was already cleaned up by `gc_branches`.  Either way,
                // there is nothing to restore; clear the intent and
                // record it as orphaned so the operator can audit.
                tracing::warn!(
                    source_branch = %intent.source_branch,
                    target_branch = ?intent.target_branch,
                    started_at = %intent.started_at,
                    "recover_orphan_merges: no matching snapshot found — \
                     intent cleared without rollback (run `root health` to \
                     audit graph state)"
                );
                report.orphaned_intents_cleared.push(intent.clone());
            }
            Err(e) => {
                // Bubble up — a partial recovery shouldn't silently
                // leave the workspace in an inconsistent state.
                return Err(e);
            }
        }
    }

    // All intents successfully processed: remove the file.  Atomic via
    // remove (single inode) — no torn state.
    let path = intents_path(&refs_dir);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| Error::io_path(&path, e))?;
    }
    Ok(report)
}

/// Locate the most recent `graph.db.<prefix>-<slug>-<ts>` matching
/// `intent` and copy it over the target's `graph.db`.  Returns the
/// snapshot path that was restored, or `None` when no matching
/// snapshot exists.
fn restore_one(root_path: &Path, intent: &MergeIntent) -> Result<Option<PathBuf>> {
    let target_data_dir = resolve_data_dir(root_path, intent.target_branch.as_deref());
    let graph_dir = target_data_dir.join("graph");
    if !graph_dir.exists() {
        return Ok(None);
    }
    let slug = slugify(&intent.snapshot_subject);
    let prefix = format!("graph.db.{}-{}-", intent.snapshot_prefix, slug);

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&graph_dir)
        .map_err(|e| Error::io_path(&graph_dir, e))?
        .filter_map(|entry| entry.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect();

    if candidates.is_empty() {
        return Ok(None);
    }

    candidates.sort();
    let snapshot = candidates
        .last()
        .expect("non-empty after filter")
        .clone();

    let live = graph_dir.join("graph.db");
    std::fs::copy(&snapshot, &live).map_err(|e| Error::io_path(&live, e))?;
    tracing::info!(
        source_branch = %intent.source_branch,
        target_branch = ?intent.target_branch,
        snapshot = %snapshot.display(),
        "recover_orphan_merges: restored graph.db from pre-merge snapshot"
    );
    Ok(Some(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_intent(source: &str) -> MergeIntent {
        MergeIntent {
            source_branch: source.into(),
            target_branch: None,
            started_at: Utc::now(),
            snapshot_subject: source.into(),
            snapshot_prefix: "pre-merge".into(),
        }
    }

    #[test]
    fn recover_on_clean_workspace_is_noop() {
        let dir = tempdir().unwrap();
        let report = recover_orphan_merges(dir.path()).unwrap();
        assert_eq!(report.recovered.len(), 0);
        assert_eq!(report.orphaned_intents_cleared.len(), 0);
    }

    #[test]
    fn write_then_clear_round_trip() {
        let dir = tempdir().unwrap();
        let refs_dir = dir.path().join(".thinkingroot-refs");
        std::fs::create_dir_all(&refs_dir).unwrap();

        let intent = make_intent("feature/x");
        write_merge_intent(&refs_dir, &intent).unwrap();
        assert!(intents_path(&refs_dir).exists());

        // Read back: one intent.
        let file = read_intents(&refs_dir).unwrap();
        assert_eq!(file.intents.len(), 1);
        assert_eq!(file.intents[0].source_branch, "feature/x");

        // Clear by (source, started_at) — file is removed when empty.
        clear_merge_intent(&refs_dir, "feature/x", intent.started_at).unwrap();
        assert!(!intents_path(&refs_dir).exists(),
            "intents file should be removed when last intent cleared");
    }

    #[test]
    fn clear_specific_intent_keeps_others() {
        let dir = tempdir().unwrap();
        let refs_dir = dir.path().join(".thinkingroot-refs");
        std::fs::create_dir_all(&refs_dir).unwrap();

        let i1 = make_intent("feature/a");
        let i2 = make_intent("feature/b");
        write_merge_intent(&refs_dir, &i1).unwrap();
        write_merge_intent(&refs_dir, &i2).unwrap();

        clear_merge_intent(&refs_dir, "feature/a", i1.started_at).unwrap();

        let file = read_intents(&refs_dir).unwrap();
        assert_eq!(file.intents.len(), 1);
        assert_eq!(file.intents[0].source_branch, "feature/b");
    }

    #[test]
    fn recover_restores_main_from_pre_merge_snapshot() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let refs_dir = root.join(".thinkingroot-refs");
        let main_graph_dir = root.join(".thinkingroot").join("graph");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::create_dir_all(&main_graph_dir).unwrap();

        // Pre-existing live graph.db with corrupt content (simulating
        // mid-merge state) and a clean pre-merge snapshot.
        let live = main_graph_dir.join("graph.db");
        let snapshot = main_graph_dir.join("graph.db.pre-merge-feature-x-1234567");
        std::fs::write(&live, b"corrupt-mid-merge-state").unwrap();
        std::fs::write(&snapshot, b"clean-pre-merge-state").unwrap();

        let intent = MergeIntent {
            source_branch: "feature/x".into(),
            target_branch: None,
            started_at: Utc::now(),
            snapshot_subject: "feature/x".into(),
            snapshot_prefix: "pre-merge".into(),
        };
        write_merge_intent(&refs_dir, &intent).unwrap();

        let report = recover_orphan_merges(root).unwrap();
        assert_eq!(report.recovered.len(), 1);
        assert_eq!(report.recovered[0].source_branch, "feature/x");
        assert_eq!(report.orphaned_intents_cleared.len(), 0);

        // Live graph.db now matches the snapshot; intents file gone.
        let restored = std::fs::read(&live).unwrap();
        assert_eq!(restored, b"clean-pre-merge-state");
        assert!(
            !intents_path(&refs_dir).exists(),
            "intents file must be removed after successful recovery"
        );
    }

    #[test]
    fn recover_clears_intent_when_snapshot_missing() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let refs_dir = root.join(".thinkingroot-refs");
        let main_graph_dir = root.join(".thinkingroot").join("graph");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::create_dir_all(&main_graph_dir).unwrap();

        let live = main_graph_dir.join("graph.db");
        std::fs::write(&live, b"original-state").unwrap();
        // No snapshot file exists.

        let intent = MergeIntent {
            source_branch: "feature/lost".into(),
            target_branch: None,
            started_at: Utc::now(),
            snapshot_subject: "feature/lost".into(),
            snapshot_prefix: "pre-merge".into(),
        };
        write_merge_intent(&refs_dir, &intent).unwrap();

        let report = recover_orphan_merges(root).unwrap();
        assert_eq!(report.recovered.len(), 0);
        assert_eq!(report.orphaned_intents_cleared.len(), 1);
        assert_eq!(
            report.orphaned_intents_cleared[0].source_branch,
            "feature/lost"
        );

        // Live graph.db unchanged when no snapshot to restore from.
        let unchanged = std::fs::read(&live).unwrap();
        assert_eq!(unchanged, b"original-state");
        assert!(!intents_path(&refs_dir).exists());
    }
}
