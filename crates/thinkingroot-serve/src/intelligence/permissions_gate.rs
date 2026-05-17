//! Phase D Wave 1 (2026-05-17) — `PermissionsGate` wraps the
//! existing [`ApprovalGate`] chain with rule-based path and
//! command authorisation.
//!
//! ## Layering
//!
//! The agent's write-tool gate becomes:
//!
//! ```text
//!     PermissionsGate
//!        ├─→ evaluate canonical paths against PermissionStore
//!        │       ├─→ Allow  → return Approved (skip prompt)
//!        │       ├─→ Deny   → return Rejected with reason
//!        │       └─→ Ask    → delegate to inner ApprovalGate
//!        │                    (existing UI prompt flow)
//!        │
//!        └─→ evaluate shell_exec command strings the same way
//! ```
//!
//! The inner gate is typed as `Arc<dyn ApprovalGate>` so production
//! callers wire `ToolApprovalRouter` (the existing SSE-bridge
//! prompt flow) and tests plug `AutoApprove` / `DenyAll` for
//! determinism.
//!
//! ## Canonicalisation invariant
//!
//! Every path subject is resolved via
//! [`safe_path::canonicalize_for_policy`] BEFORE evaluation. This
//! is the single load-bearing security guarantee — it closes the
//! symlink-cover-name attack (LLM passes `./notes/id_rsa` where
//! `./notes -> ~/.ssh`; without canonicalisation the literal
//! `~/.ssh/**` deny rule never fires).
//!
//! For tools that write to a path that doesn't yet exist (the
//! common `file_write`/`file_edit` case), canonicalisation falls
//! through to the path's PARENT directory — the directory MUST
//! exist and pass the permission check. The eventual write
//! target is the canonical parent joined with the leaf name.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use thinkingroot_core::permissions::{Decision, PermissionStore};
use thinkingroot_core::safe_path::{canonicalize_for_policy, PathPolicyError};
use tokio::sync::RwLock;

use crate::intelligence::approval::{ApprovalDecision, ApprovalGate};

/// What the gate is being asked to authorise.  Extracted from the
/// tool input once at the top of `check` so we don't re-parse the
/// JSON on every rule evaluation.
#[derive(Debug)]
enum Subject {
    /// A filesystem path the tool wants to read or write.
    /// `allow_nonexistent` is true for tools that can legitimately
    /// target a path that doesn't exist yet (file_write creates
    /// new files).  When canonicalisation of the leaf fails with
    /// `NotFound`, the gate falls through to canonicalising the
    /// parent directory.
    Path {
        raw: String,
        allow_nonexistent: bool,
    },
    /// A shell command the tool wants to execute. Evaluated
    /// against command-kind rules in the store.
    Command(String),
}

pub struct PermissionsGate {
    store: Arc<RwLock<PermissionStore>>,
    inner: Arc<dyn ApprovalGate>,
}

impl PermissionsGate {
    pub fn new(store: Arc<RwLock<PermissionStore>>, inner: Arc<dyn ApprovalGate>) -> Self {
        Self { store, inner }
    }

    /// Per-tool argument extraction.  Keep this exhaustive — adding a
    /// new Phase D tool means adding a match arm here.  Tools not
    /// listed have an empty subject vector and bypass the gate (e.g.
    /// `clipboard_read`/`clipboard_write` operate on the system
    /// clipboard, not on a path, so there is nothing to canonicalise).
    fn extract_subjects(tool_name: &str, input: &serde_json::Value) -> Vec<Subject> {
        let path_subject = |raw: &str, allow_nonexistent: bool| -> Subject {
            Subject::Path {
                raw: raw.to_string(),
                allow_nonexistent,
            }
        };
        match tool_name {
            "file_read" | "file_edit" => input
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| vec![path_subject(s, false)])
                .unwrap_or_default(),

            // `open_in_default` accepts EITHER a filesystem path OR a
            // URL under the same `path_or_url` field (see tool schema
            // in `mcp/tools.rs::open_in_default`). The earlier shared
            // arm above read `input.get("path")`, which the schema
            // never populates — every call yielded an empty subject
            // vec, silently bypassing DEFAULT_DENY for file targets.
            //
            // Path targets route through `Subject::Path` so the
            // canonical-check fires (~/.ssh refused without prompt).
            // URL targets (`http://`, `https://`, `mailto:`, `ftp://`)
            // route through `Subject::Command` so the existing
            // command-policy machinery decides — that's the closest
            // existing primitive without introducing a new variant.
            "open_in_default" => input
                .get("path_or_url")
                .and_then(|v| v.as_str())
                .map(|raw| {
                    let trimmed = raw.trim();
                    let lower = trimmed.to_ascii_lowercase();
                    if lower.starts_with("http://")
                        || lower.starts_with("https://")
                        || lower.starts_with("ftp://")
                        || lower.starts_with("mailto:")
                    {
                        vec![Subject::Command(trimmed.to_string())]
                    } else if let Some(stripped) = trimmed.strip_prefix("file://") {
                        vec![path_subject(stripped, false)]
                    } else {
                        vec![path_subject(trimmed, false)]
                    }
                })
                .unwrap_or_default(),

            "file_write" => input
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| vec![path_subject(s, true)])
                .unwrap_or_default(),

            "glob" | "grep" => input
                .get("base")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())
                .map(|s| vec![path_subject(s, false)])
                .unwrap_or_default(),

            "trash" => input
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| path_subject(s, false))
                        .collect()
                })
                .unwrap_or_default(),

            "shell_exec" => {
                let mut subjects = Vec::new();
                if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                    subjects.push(Subject::Command(cmd.to_string()));
                }
                if let Some(cwd) = input.get("cwd").and_then(|v| v.as_str()) {
                    subjects.push(path_subject(cwd, false));
                }
                subjects
            }

            // Clipboard tools operate on system clipboard, not on a
            // path — nothing to gate at the path/command level.
            // (They're still write-tools so the UI shows an
            // approval; that's the inner gate's job.)
            "clipboard_read" | "clipboard_write" => Vec::new(),

            // Unknown tool → empty subjects → fall through to inner
            // gate. Defensive: a future tool added to BRIDGE_WRITE_NAMES
            // without a matching arm here still gets the standard
            // approval prompt instead of silently bypassing.
            _ => Vec::new(),
        }
    }

    /// Canonicalise a path subject. For `allow_nonexistent`, if the
    /// leaf doesn't exist the parent is canonicalised and the leaf
    /// is appended literally — this is the file-creation path.
    fn canonicalize_subject(raw: &str, allow_nonexistent: bool) -> Result<std::path::PathBuf, ApprovalDecision> {
        let p = Path::new(raw);
        match canonicalize_for_policy(p) {
            Ok(canonical) => Ok(canonical),
            Err(PathPolicyError::NotFound { .. }) if allow_nonexistent => {
                // file_write to a not-yet-existing path. Canonicalise
                // the parent. If the parent also doesn't exist, the
                // user is trying to write somewhere that requires
                // mkdir -p — refuse rather than silently surprise.
                let parent = p
                    .parent()
                    .ok_or_else(|| ApprovalDecision::Rejected {
                        reason: format!(
                            "permission policy: path `{raw}` has no parent directory"
                        ),
                    })?;
                let parent_canonical = canonicalize_for_policy(parent).map_err(|e| {
                    ApprovalDecision::Rejected {
                        reason: format!(
                            "permission policy: parent of `{raw}` cannot be canonicalised: {e}"
                        ),
                    }
                })?;
                let leaf = p.file_name().ok_or_else(|| ApprovalDecision::Rejected {
                    reason: format!("permission policy: path `{raw}` has no leaf component"),
                })?;
                Ok(parent_canonical.join(leaf))
            }
            Err(e) => Err(ApprovalDecision::Rejected {
                reason: format!("permission policy: cannot canonicalise `{raw}`: {e}"),
            }),
        }
    }
}

#[async_trait]
impl ApprovalGate for PermissionsGate {
    async fn check(&self, tool_name: &str, input: &serde_json::Value) -> ApprovalDecision {
        let subjects = Self::extract_subjects(tool_name, input);
        if subjects.is_empty() {
            // No subjects: this tool isn't a path/command-typed one;
            // fall through to the inner gate so the standard
            // write-tool approval flow still runs.
            return self.inner.check(tool_name, input).await;
        }

        let store = self.store.read().await;
        let mut any_ask = false;

        for subject in subjects {
            match subject {
                Subject::Path {
                    raw,
                    allow_nonexistent,
                } => {
                    let canonical = match Self::canonicalize_subject(&raw, allow_nonexistent) {
                        Ok(c) => c,
                        Err(decision) => return decision,
                    };
                    match store.evaluate_path(&canonical) {
                        Decision::Allow => continue,
                        Decision::Deny => {
                            return ApprovalDecision::Rejected {
                                reason: format!(
                                    "permission policy denies access to `{}`",
                                    canonical.display()
                                ),
                            };
                        }
                        Decision::Ask => any_ask = true,
                    }
                }
                Subject::Command(cmd) => match store.evaluate_command(&cmd) {
                    Decision::Allow => continue,
                    Decision::Deny => {
                        return ApprovalDecision::Rejected {
                            reason: format!("permission policy denies command: `{cmd}`"),
                        };
                    }
                    Decision::Ask => any_ask = true,
                },
            }
        }

        drop(store);

        if any_ask {
            // Delegate to inner gate (UI prompt via the existing SSE
            // `approval_requested` flow). The inner gate sees the
            // tool_name + input as-is.
            self.inner.check(tool_name, input).await
        } else {
            // Every subject evaluated to Allow.
            ApprovalDecision::Approved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::approval::{AutoApprove, DenyAll};
    use serde_json::json;
    use thinkingroot_core::permissions::{Decision, Rule, RuleKind};

    fn store_with_rule(pattern: &str, decision: Decision, kind: RuleKind) -> Arc<RwLock<PermissionStore>> {
        let mut s = PermissionStore::empty();
        s.insert_rule(Rule {
            kind,
            pattern: pattern.to_string(),
            decision,
            created_at: chrono::Utc::now(),
            created_by: "test".to_string(),
        })
        .expect("rule must insert in test (DEFAULT_DENY check passes for test patterns)");
        Arc::new(RwLock::new(s))
    }

    fn empty_store() -> Arc<RwLock<PermissionStore>> {
        Arc::new(RwLock::new(PermissionStore::empty()))
    }

    #[tokio::test]
    async fn no_subjects_falls_through_to_inner_gate() {
        // clipboard_read has no subjects → must delegate to inner.
        // Inner is AutoApprove → check returns Approved.
        let gate = PermissionsGate::new(empty_store(), Arc::new(AutoApprove));
        let d = gate.check("clipboard_read", &json!({})).await;
        assert!(d.is_approved());
    }

    #[tokio::test]
    async fn no_subjects_delegates_rejection_too() {
        // Same path but inner is DenyAll → gate must surface the rejection.
        let gate = PermissionsGate::new(empty_store(), Arc::new(DenyAll));
        let d = gate.check("clipboard_read", &json!({})).await;
        assert!(!d.is_approved());
    }

    #[tokio::test]
    async fn file_read_inside_workspace_with_explicit_allow_skips_inner() {
        // Build a path inside the OS temp directory + add an allow rule
        // for that prefix.  PermissionsGate must return Approved
        // WITHOUT delegating to inner (DenyAll proves the bypass).
        // Use canonicalized tmp path so the rule pattern matches the
        // canonicalized form `PermissionsGate` evaluates against
        // (on macOS `/var/folders/...` canonicalizes to
        // `/private/var/folders/...`).
        let tmp = tempfile::tempdir().unwrap();
        let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
        let f = canonical_tmp.join("readme.md");
        std::fs::write(&f, b"hello").unwrap();
        let pattern = format!("{}/**", canonical_tmp.display());
        let store = store_with_rule(&pattern, Decision::Allow, RuleKind::Path);
        let gate = PermissionsGate::new(store, Arc::new(DenyAll));
        let d = gate
            .check("file_read", &json!({ "path": f.display().to_string() }))
            .await;
        assert!(d.is_approved(), "explicit allow must skip inner gate");
    }

    #[tokio::test]
    async fn file_read_against_ssh_path_is_denied_by_default() {
        // No user rules; DEFAULT_DENY covers ~/.ssh/**.  Gate
        // returns Rejected without prompting (even though inner is
        // AutoApprove — DEFAULT_DENY always wins).
        let home = dirs::home_dir().unwrap();
        let ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).ok();
        let key = ssh_dir.join("id_rsa_test_phase_d_wave_1");
        std::fs::write(&key, b"fake").ok();

        let gate = PermissionsGate::new(empty_store(), Arc::new(AutoApprove));
        let d = gate
            .check("file_read", &json!({ "path": key.display().to_string() }))
            .await;
        // Cleanup before assertion.
        std::fs::remove_file(&key).ok();

        match d {
            ApprovalDecision::Rejected { reason } => {
                assert!(
                    reason.contains("permission policy denies"),
                    "rejection reason must indicate policy denial, got: {reason}"
                );
            }
            ApprovalDecision::Approved => {
                panic!("DEFAULT_DENY must override AutoApprove for ~/.ssh paths");
            }
        }
    }

    #[tokio::test]
    async fn file_read_missing_path_is_rejected() {
        let gate = PermissionsGate::new(empty_store(), Arc::new(AutoApprove));
        let d = gate
            .check(
                "file_read",
                &json!({ "path": "/this/path/definitely/does/not/exist/anywhere" }),
            )
            .await;
        match d {
            ApprovalDecision::Rejected { reason } => {
                assert!(reason.contains("canonicalise") || reason.contains("does not exist"));
            }
            other => panic!("expected Rejected for missing path, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_write_to_new_file_uses_parent_canonicalisation() {
        // file_write to a new (non-existent) path within an existing
        // directory must canonicalise the parent, evaluate against
        // the user's rules, and Approve when the parent is allowed.
        let tmp = tempfile::tempdir().unwrap();
        let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
        let new_file = canonical_tmp.join("new.txt");
        assert!(!new_file.exists(), "test setup: file must not exist yet");
        let pattern = format!("{}/**", canonical_tmp.display());
        let store = store_with_rule(&pattern, Decision::Allow, RuleKind::Path);
        let gate = PermissionsGate::new(store, Arc::new(DenyAll));
        let d = gate
            .check(
                "file_write",
                &json!({ "path": new_file.display().to_string(), "content": "hi" }),
            )
            .await;
        assert!(
            d.is_approved(),
            "file_write to allowed-parent must approve via parent canonicalisation"
        );
    }

    #[tokio::test]
    async fn shell_exec_command_rule_takes_effect() {
        // Allow `git *` commands; everything else falls through to
        // inner. With inner=AutoApprove all commands ultimately
        // succeed, but the policy-allow path must NOT prompt the
        // user — assertion is on the bypass behaviour.
        let mut s = PermissionStore::empty();
        s.insert_rule(Rule {
            kind: RuleKind::Command,
            pattern: "git *".to_string(),
            decision: Decision::Allow,
            created_at: chrono::Utc::now(),
            created_by: "test".to_string(),
        })
        .unwrap();
        let store = Arc::new(RwLock::new(s));

        // Allow rule on `git status` → policy short-circuits to Approved.
        let gate = PermissionsGate::new(store.clone(), Arc::new(DenyAll));
        let d = gate
            .check("shell_exec", &json!({ "command": "git status" }))
            .await;
        assert!(d.is_approved(), "Allow command rule must skip inner");

        // Different command → falls through to Ask → DenyAll inner → Rejected.
        let d2 = gate
            .check("shell_exec", &json!({ "command": "rm -rf /" }))
            .await;
        assert!(!d2.is_approved(), "Ask must delegate to inner DenyAll");
    }

    #[tokio::test]
    async fn shell_exec_command_deny_rule_blocks_without_prompt() {
        let mut s = PermissionStore::empty();
        s.insert_rule(Rule {
            kind: RuleKind::Command,
            pattern: "rm *".to_string(),
            decision: Decision::Deny,
            created_at: chrono::Utc::now(),
            created_by: "test".to_string(),
        })
        .unwrap();
        let store = Arc::new(RwLock::new(s));
        // Inner is AutoApprove — Deny rule MUST win.
        let gate = PermissionsGate::new(store, Arc::new(AutoApprove));
        let d = gate
            .check("shell_exec", &json!({ "command": "rm -rf /" }))
            .await;
        match d {
            ApprovalDecision::Rejected { reason } => {
                assert!(reason.contains("denies command"));
            }
            _ => panic!("Deny command rule must reject even against AutoApprove inner"),
        }
    }

    #[tokio::test]
    async fn trash_aggregates_multiple_paths() {
        // trash takes a paths array — every entry must pass.
        let tmp = tempfile::tempdir().unwrap();
        let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
        let f1 = canonical_tmp.join("a.txt");
        let f2 = canonical_tmp.join("b.txt");
        std::fs::write(&f1, b"a").unwrap();
        std::fs::write(&f2, b"b").unwrap();
        let pattern = format!("{}/**", canonical_tmp.display());
        let store = store_with_rule(&pattern, Decision::Allow, RuleKind::Path);
        let gate = PermissionsGate::new(store, Arc::new(DenyAll));
        let d = gate
            .check(
                "trash",
                &json!({
                    "paths": [
                        f1.display().to_string(),
                        f2.display().to_string()
                    ]
                }),
            )
            .await;
        assert!(d.is_approved(), "all-allow trash batch must approve");
    }
}
