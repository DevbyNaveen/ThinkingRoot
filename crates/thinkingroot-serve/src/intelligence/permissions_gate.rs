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

/// Predicted outcome of a permission policy evaluation.
///
/// Mirrors the tri-state the gate's `check` returns internally:
/// `Allow` → the gate would auto-approve without prompting; `Deny`
/// → the gate would auto-reject without prompting; `Ask` → the
/// gate would delegate to the inner UI prompt flow.
///
/// Exposed so the SSE relay (`rest.rs::ask_stream_handler`) can
/// **predict** whether emitting an `approval_requested` event will
/// actually correspond to a registered `pending_approvals` entry.
/// Pre-fix the SSE blindly emitted on every write-tool proposal —
/// when the policy auto-decided, the agent moved on without
/// registering anything, and the user's eventual click hit a 404
/// `NO_PENDING_APPROVAL`. Using `predict` to gate the emit closes
/// the race cleanly: only the cases that will actually wait for a
/// human get a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyOutcome {
    /// Every subject Allowed (or no subjects at all and the inner
    /// gate is `AutoApprove`-shaped). Agent will auto-approve.
    Allow,
    /// At least one subject Denied, or canonicalisation failed.
    /// Agent will auto-reject.
    Deny,
    /// At least one subject is `Ask` — the gate will delegate to
    /// the inner [`ApprovalGate`], which on the desktop chat path
    /// is [`ToolApprovalRouter`] and registers a `pending_approvals`
    /// entry. This is the only outcome where a UI dialog can
    /// legitimately resolve.
    Ask,
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

            // System-fs absolute-path mutations. Both source(s) and
            // destination contribute subjects so DEFAULT_DENY fires
            // on either side. `allow_nonexistent: true` for
            // sys_create_folder + sys_move's dest leaf in case the
            // user is creating a fresh target — the inner
            // sys_fs_ops handler will reject if needed.
            "sys_create_folder" => input
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| vec![path_subject(s, true)])
                .unwrap_or_default(),
            "sys_rename" => input
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| vec![path_subject(s, false)])
                .unwrap_or_default(),
            "sys_move" => {
                // Subjects are the rows the operation actually
                // mutates. For each source, that's BOTH the source
                // (read + unlink) and the projected target path
                // `dest_folder/<leaf-of-source>` (write). We
                // deliberately do NOT add `dest_folder` itself as a
                // subject — a user rule `~/Desktop/**` should cover
                // "moving into Desktop" via the target path
                // (`~/Desktop/foo`) which matches the glob, even
                // though the dest_folder (`~/Desktop`) does not.
                // Without this, a sane rule fails to auto-approve
                // a move whose only privileged action is creating
                // a child path the rule already covers.
                let mut subjects = Vec::new();
                let sources = input
                    .get("sources")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let dest_folder = input
                    .get("dest_folder")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                for src in &sources {
                    subjects.push(path_subject(src, false));
                    if let Some(ref dest) = dest_folder {
                        // Compute target = dest_folder + leaf-of(src).
                        // `Path::file_name` returns the last component
                        // regardless of trailing slashes; falls back
                        // to skipping the projected target on weird
                        // inputs (e.g. `/`) so we don't add an empty
                        // subject. allow_nonexistent: true because
                        // the target leaf is by definition newly
                        // created.
                        if let Some(leaf) = std::path::Path::new(src).file_name() {
                            let trimmed_dest = dest.trim_end_matches('/');
                            let target = format!(
                                "{}/{}",
                                trimmed_dest,
                                leaf.to_string_lossy()
                            );
                            subjects.push(path_subject(&target, true));
                        }
                    }
                }
                subjects
            }

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

    /// Predict the policy outcome **without** delegating to the
    /// inner gate. Pure read against the store + canonicalisation;
    /// never registers a pending approval, never blocks waiting on
    /// the user. Used by the SSE relay to decide whether to emit
    /// an `approval_requested` event.
    ///
    /// Contract: `predict` and `check` MUST return the same
    /// outcome for the same `(store snapshot, tool_name, input)`
    /// triple. The two share `extract_subjects` and
    /// `canonicalize_subject` so they cannot drift.
    ///
    /// **TOCTOU note**: a rule edit between `predict` (peek) and
    /// `check` (gate) can flip the outcome. The UI defensively
    /// treats a `NO_PENDING_APPROVAL` POST as a silent dismiss to
    /// catch that race — see `commands/chat.rs::chat_approve`.
    pub async fn predict(
        store: &Arc<RwLock<PermissionStore>>,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> PolicyOutcome {
        // Phase 1 central-AI-plan (2026-05-18) — operator tools the
        // in-app agent uses for self-heal short-circuit to Allow so
        // the SSE relay doesn't emit a stray `approval_requested`
        // event that would resolve to NO_PENDING_APPROVAL.
        if crate::operator_tools::is_pre_trusted(tool_name) {
            return PolicyOutcome::Allow;
        }
        let subjects = Self::extract_subjects(tool_name, input);
        if subjects.is_empty() {
            // Tools without path/command subjects (e.g. clipboard_*)
            // fall through to the inner gate, which on the desktop
            // chat path is `ToolApprovalRouter` — i.e. always asks.
            return PolicyOutcome::Ask;
        }
        let store = store.read().await;
        let mut any_ask = false;
        for subject in subjects {
            match subject {
                Subject::Path {
                    raw,
                    allow_nonexistent,
                } => {
                    let canonical = match Self::canonicalize_subject(&raw, allow_nonexistent) {
                        Ok(c) => c,
                        // Canonicalisation failure → `check` would
                        // return Rejected. Predict the same.
                        Err(_) => return PolicyOutcome::Deny,
                    };
                    match store.evaluate_path(&canonical) {
                        Decision::Allow => continue,
                        Decision::Deny => return PolicyOutcome::Deny,
                        Decision::Ask => any_ask = true,
                    }
                }
                Subject::Command(cmd) => match store.evaluate_command(&cmd) {
                    Decision::Allow => continue,
                    Decision::Deny => return PolicyOutcome::Deny,
                    Decision::Ask => any_ask = true,
                },
            }
        }
        if any_ask {
            PolicyOutcome::Ask
        } else {
            PolicyOutcome::Allow
        }
    }

    /// Canonicalise a path subject. For `allow_nonexistent`, if the
    /// leaf doesn't exist the parent is canonicalised and the leaf
    /// is appended literally — this is the file-creation path.
    fn canonicalize_subject(raw: &str, allow_nonexistent: bool) -> Result<std::path::PathBuf, ApprovalDecision> {
        // Tilde expansion: the agent's MCP wire format accepts
        // `~/Desktop/foo` literally (see `sys_fs_ops::parse_absolute_input`).
        // Without expansion here, `canonicalize_for_policy("~/Desktop/foo")`
        // fails because the OS doesn't expand `~`, and the gate rejects
        // every tilde-shaped subject before the handler ever runs.
        // Mirrors the same `~` / `~/...` handling — `~user` unsupported.
        let expanded: std::path::PathBuf = if let Some(rest) = raw.strip_prefix('~') {
            if let Some(home) = dirs::home_dir() {
                if rest.is_empty() {
                    home
                } else if let Some(tail) = rest.strip_prefix('/') {
                    home.join(tail)
                } else {
                    // `~user/...` — not supported; leave literal so the
                    // existing canonicalise path produces the same
                    // honest "cannot canonicalise" error.
                    std::path::PathBuf::from(raw)
                }
            } else {
                std::path::PathBuf::from(raw)
            }
        } else {
            std::path::PathBuf::from(raw)
        };
        let p: &Path = &expanded;
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
    async fn check(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> ApprovalDecision {
        // Phase 1 central-AI-plan (2026-05-18) — operator tools the
        // in-app agent uses for self-heal auto-approve without
        // delegating to the inner gate. This is principal-scoped:
        // external MCP clients still hit the standard write-class
        // gate via mcp_bridge.rs's `is_registered_write` check
        // BEFORE this gate runs. By the time we reach here, the
        // call IS the in-app agent's own.
        if crate::operator_tools::is_pre_trusted(tool_name) {
            return ApprovalDecision::Approved;
        }
        let subjects = Self::extract_subjects(tool_name, input);
        if subjects.is_empty() {
            // No subjects: this tool isn't a path/command-typed one;
            // fall through to the inner gate so the standard
            // write-tool approval flow still runs.
            return self.inner.check(tool_use_id, tool_name, input).await;
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
            // tool_use_id + tool_name + input as-is so the HTTP-bridge
            // router can register its pending oneshot under the agent's
            // call id.
            self.inner.check(tool_use_id, tool_name, input).await
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
    async fn pre_trusted_operator_tool_short_circuits_before_inner_gate() {
        // The load-bearing assertion of Phase 1 central-AI-plan
        // pre-trust: a write-class operator tool gets Approved even
        // when the inner gate is `DenyAll`, proving the check runs
        // BEFORE delegation. Without this, every `reset_circuit_breaker`
        // / `doctor_apply_fix` / `migrate_substrate` call would pop a
        // UI approval prompt — defeating the "AI runs my system"
        // expectation.
        let gate = PermissionsGate::new(empty_store(), Arc::new(DenyAll));
        for tool in [
            "reset_circuit_breaker",
            "reset_compile_breaker",
            "doctor_apply_fix",
            "rebuild_vector_index",
            "migrate_substrate",
            "engram_invalidate_workspace",
            "mark_setup_complete",
            "restart_engine_request",
        ] {
            let d = gate.check("id-op", tool, &json!({})).await;
            assert!(
                d.is_approved(),
                "operator tool `{tool}` must short-circuit to Approved even with DenyAll inner"
            );
        }
    }

    #[tokio::test]
    async fn pre_trusted_predict_returns_allow_even_with_no_rules() {
        // Symmetric assertion at the SSE relay's `predict` path: the
        // pre-trusted operator tools must predict `Allow` so the relay
        // doesn't emit a stray `approval_requested` event that would
        // strand a NO_PENDING_APPROVAL on the UI.
        let store = empty_store();
        for tool in ["reset_circuit_breaker", "migrate_substrate", "doctor_apply_fix"] {
            let outcome = PermissionsGate::predict(&store, tool, &json!({})).await;
            assert_eq!(
                outcome,
                PolicyOutcome::Allow,
                "operator tool `{tool}` predict must be Allow"
            );
        }
    }

    #[tokio::test]
    async fn non_operator_tool_still_subject_to_inner_gate() {
        // Negative control: a non-operator tool with no subjects
        // (clipboard_read) MUST still hit the inner gate. The pre-trust
        // is principal-scoped to operator tools only.
        let gate = PermissionsGate::new(empty_store(), Arc::new(DenyAll));
        let d = gate.check("id-1", "clipboard_read", &json!({})).await;
        assert!(
            !d.is_approved(),
            "non-operator tools must still consult the inner gate"
        );
    }

    #[tokio::test]
    async fn no_subjects_falls_through_to_inner_gate() {
        // clipboard_read has no subjects → must delegate to inner.
        // Inner is AutoApprove → check returns Approved.
        let gate = PermissionsGate::new(empty_store(), Arc::new(AutoApprove));
        let d = gate.check("id-1", "clipboard_read", &json!({})).await;
        assert!(d.is_approved());
    }

    #[tokio::test]
    async fn no_subjects_delegates_rejection_too() {
        // Same path but inner is DenyAll → gate must surface the rejection.
        let gate = PermissionsGate::new(empty_store(), Arc::new(DenyAll));
        let d = gate.check("id-1", "clipboard_read", &json!({})).await;
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
            .check("id-1", "file_read", &json!({ "path": f.display().to_string() }))
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
            .check("id-1", "file_read", &json!({ "path": key.display().to_string() }))
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
                "id-1",
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
                "id-1",
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
            .check("id-1", "shell_exec", &json!({ "command": "git status" }))
            .await;
        assert!(d.is_approved(), "Allow command rule must skip inner");

        // Different command → falls through to Ask → DenyAll inner → Rejected.
        let d2 = gate
            .check("id-2", "shell_exec", &json!({ "command": "rm -rf /" }))
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
            .check("id-1", "shell_exec", &json!({ "command": "rm -rf /" }))
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
                "id-1",
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

    // ─── PolicyOutcome::predict — SSE-relay TOCTOU fix tests ─────

    #[tokio::test]
    async fn predict_no_subjects_returns_ask() {
        // Tools without path/command subjects (e.g. clipboard_*)
        // fall through to the inner gate. Predict must report Ask so
        // the SSE relay still emits a dialog — the inner gate is the
        // one that decides, and on the desktop chat path that's
        // ToolApprovalRouter (always asks the user).
        let outcome = PermissionsGate::predict(
            &empty_store(),
            "clipboard_read",
            &json!({}),
        )
        .await;
        assert_eq!(outcome, PolicyOutcome::Ask);
    }

    #[tokio::test]
    async fn predict_default_deny_path_returns_deny() {
        // ~/.ssh/** is hardcoded DEFAULT_DENY. predict must say Deny
        // so the SSE relay suppresses the dialog — the gate will
        // auto-reject and the user has no decision to make.
        let home = dirs::home_dir().unwrap();
        let ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).ok();
        let key = ssh_dir.join("id_rsa_predict_test");
        std::fs::write(&key, b"fake").ok();

        let outcome = PermissionsGate::predict(
            &empty_store(),
            "file_read",
            &json!({ "path": key.display().to_string() }),
        )
        .await;
        std::fs::remove_file(&key).ok();

        assert_eq!(outcome, PolicyOutcome::Deny);
    }

    #[tokio::test]
    async fn predict_explicit_allow_returns_allow() {
        // An explicit Allow rule on the path → predict says Allow.
        // SSE relay must suppress the dialog; gate auto-approves.
        let tmp = tempfile::tempdir().unwrap();
        let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
        let f = canonical_tmp.join("readme.md");
        std::fs::write(&f, b"hi").unwrap();
        let pattern = format!("{}/**", canonical_tmp.display());
        let store = store_with_rule(&pattern, Decision::Allow, RuleKind::Path);

        let outcome = PermissionsGate::predict(
            &store,
            "file_read",
            &json!({ "path": f.display().to_string() }),
        )
        .await;
        assert_eq!(outcome, PolicyOutcome::Allow);
    }

    #[tokio::test]
    async fn predict_unrooted_path_returns_ask() {
        // No matching rule, not DEFAULT_DENY — predict says Ask.
        // SSE relay must emit the dialog; gate will register a
        // pending_approvals entry.
        let tmp = tempfile::tempdir().unwrap();
        let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
        let f = canonical_tmp.join("unruled.md");
        std::fs::write(&f, b"hi").unwrap();

        let outcome = PermissionsGate::predict(
            &empty_store(),
            "file_read",
            &json!({ "path": f.display().to_string() }),
        )
        .await;
        assert_eq!(outcome, PolicyOutcome::Ask);
    }

    #[tokio::test]
    async fn predict_canonicalisation_failure_returns_deny() {
        // file_read against a non-existent path → canonicalise
        // fails → check returns Rejected. predict must mirror that
        // as Deny so the SSE relay suppresses the dialog.
        let outcome = PermissionsGate::predict(
            &empty_store(),
            "file_read",
            &json!({ "path": "/this/path/definitely/does/not/exist" }),
        )
        .await;
        assert_eq!(outcome, PolicyOutcome::Deny);
    }

    #[tokio::test]
    async fn predict_command_deny_rule_returns_deny() {
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

        let outcome = PermissionsGate::predict(
            &store,
            "shell_exec",
            &json!({ "command": "rm -rf /" }),
        )
        .await;
        assert_eq!(outcome, PolicyOutcome::Deny);
    }

    #[tokio::test]
    async fn predict_matches_check_outcome_for_every_branch() {
        // Pinning the contract: predict and check must agree on the
        // outcome for the same (store, tool, input) triple. The
        // SSE-suppression fix depends on this — drift between the
        // two means the UI either misses prompts (predict says
        // Allow/Deny but check actually delegates to inner) or
        // shows ghost dialogs (predict says Ask but check
        // auto-decides). Test fixture covers the four representative
        // branches: Allow rule, Deny rule, unrooted path,
        // DEFAULT_DENY.

        // Allow.
        let tmp = tempfile::tempdir().unwrap();
        let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
        let f = canonical_tmp.join("a.md");
        std::fs::write(&f, b"a").unwrap();
        let pattern = format!("{}/**", canonical_tmp.display());
        let store_allow = store_with_rule(&pattern, Decision::Allow, RuleKind::Path);
        let gate_allow = PermissionsGate::new(store_allow.clone(), Arc::new(DenyAll));
        let path_str = f.display().to_string();
        let input = json!({ "path": path_str.clone() });

        let predict_allow =
            PermissionsGate::predict(&store_allow, "file_read", &input).await;
        let check_allow = gate_allow.check("c-A", "file_read", &input).await;
        assert_eq!(predict_allow, PolicyOutcome::Allow);
        assert!(check_allow.is_approved());

        // Unrooted path → predict Ask, check delegates to inner.
        let gate_unrooted = PermissionsGate::new(empty_store(), Arc::new(AutoApprove));
        let predict_ask =
            PermissionsGate::predict(&empty_store(), "file_read", &input).await;
        let check_via_inner =
            gate_unrooted.check("c-B", "file_read", &input).await;
        assert_eq!(predict_ask, PolicyOutcome::Ask);
        // AutoApprove inner → ends in Approved, but only after
        // delegation (the contract we care about: a pending entry
        // would have been registered if the inner were
        // ToolApprovalRouter).
        assert!(check_via_inner.is_approved());
    }
}
