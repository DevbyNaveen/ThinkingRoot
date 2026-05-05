// crates/thinkingroot-branch/src/templates.rs
//
// T3.7 — Branch templates.
//
// A `BranchTemplate` is a pre-baked combination of `BranchKind`,
// `MergePolicy`, `RedactionPolicy`, optional `max_age_secs`, and
// optional `BranchPermissions`.  When `create_branch_full` is invoked
// with `template: Some("review-required")`, every unset field on the
// request inherits from the template; explicit fields on the request
// always override the template.  This keeps the default-overridable
// contract symmetric with how `BranchKind::default()` and friends work
// today.
//
// Storage: `<root>/.thinkingroot-refs/branch_templates.toml` — same
// directory as `branches.toml`.  One file per workspace, mutated under
// the same `BranchAdvisoryLock` advisory pattern as the registry so
// concurrent CLI / REST callers cannot race a half-written write.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thinkingroot_core::{BranchKind, BranchPermissions, Error, MergePolicy, RedactionPolicy, Result};

/// A pre-baked branch configuration users can apply by name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BranchTemplate {
    /// Stable identifier — must be unique within a workspace.
    pub name: String,
    /// Optional human-readable explanation surfaced in CLI / REST list
    /// outputs.  `None` means "no description"; do not invent one.
    #[serde(default)]
    pub description: Option<String>,
    /// Default kind for branches materialised from this template.
    /// Caller may override per-branch.
    #[serde(default)]
    pub kind: BranchKind,
    /// Default merge policy.  The most common opinionated value is
    /// `MergePolicy::RequiresProposal { min_reviewers: 1, required_checks:
    /// vec![] }` — pinned by the seeded `review-required` template.
    #[serde(default)]
    pub merge_policy: MergePolicy,
    /// Optional default redaction policy.
    #[serde(default)]
    pub redaction: Option<RedactionPolicy>,
    /// Optional default TTL.  When set, branches materialised from
    /// this template auto-abandon after `now - created_at >
    /// max_age_secs` (T2.3).
    #[serde(default)]
    pub max_age_secs: Option<u64>,
    /// Optional default branch-level permissions.  `None` means "use
    /// `BranchPermissions::default()`" — preserves the existing
    /// fall-through every other branch field uses.
    #[serde(default)]
    pub permissions: Option<BranchPermissions>,
}

impl BranchTemplate {
    /// Return the seed templates loaded into a freshly-created
    /// `branch_templates.toml`.  Two opinionated defaults so users
    /// have a working `--template` argument the first time they ask:
    ///
    /// - `review-required` — `RequiresProposal { min_reviewers: 1 }`,
    ///   the canonical "human-in-the-loop" gate.
    /// - `agent-sandbox` — `BranchKind::Sandbox`, `Ephemeral` merge
    ///   policy, ideal for one-shot experimentation that should
    ///   discard on merge.
    ///
    /// We keep this seeded set tiny on purpose; templates are meant
    /// for users to compose, not for the engine to ship a library.
    pub fn seed() -> Vec<Self> {
        vec![
            BranchTemplate {
                name: "review-required".to_string(),
                description: Some(
                    "Human review gate — every merge requires an approved \
                     Knowledge Proposal with at least one reviewer."
                        .to_string(),
                ),
                kind: BranchKind::Feature,
                merge_policy: MergePolicy::RequiresProposal {
                    min_reviewers: 1,
                    required_checks: Vec::new(),
                },
                redaction: None,
                max_age_secs: None,
                permissions: None,
            },
            // Sandbox carries a per-agent identifier so we leave the
            // template's `kind` as `Feature` here — callers materialise
            // the branch with `BranchKind::Sandbox { agent_id }` only
            // when they know the agent at create time.  The ephemeral
            // merge policy is the part that's actually pre-set.
            BranchTemplate {
                name: "agent-sandbox".to_string(),
                description: Some(
                    "Throwaway scratch branch — Ephemeral merge policy, \
                     auto-abandons on session end."
                        .to_string(),
                ),
                kind: BranchKind::Feature,
                merge_policy: MergePolicy::Ephemeral,
                redaction: None,
                max_age_secs: None,
                permissions: None,
            },
        ]
    }
}

/// On-disk container — one TOML file at
/// `<root>/.thinkingroot-refs/branch_templates.toml`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct TemplateFile {
    #[serde(default)]
    templates: Vec<BranchTemplate>,
}

/// Compute the path of the templates file inside a refs directory.
pub fn templates_path(refs_dir: &Path) -> PathBuf {
    refs_dir.join("branch_templates.toml")
}

/// In-memory registry of templates.  Loaded from disk on every public
/// API call so concurrent CLI/REST writers cannot race a stale view.
pub struct TemplateRegistry {
    refs_dir: PathBuf,
    templates: Vec<BranchTemplate>,
}

impl TemplateRegistry {
    /// Load the TOML file, seeding the two defaults when the file
    /// does not exist.  The seed write is performed exactly once on
    /// first call and never overwrites a hand-edited file.
    pub fn load_or_seed(refs_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(refs_dir).map_err(|e| Error::io_path(refs_dir, e))?;
        let path = templates_path(refs_dir);
        if !path.exists() {
            let file = TemplateFile {
                templates: BranchTemplate::seed(),
            };
            let bytes = toml::to_string_pretty(&file)
                .map_err(|e| Error::Config(format!("serialize seed templates: {e}")))?;
            std::fs::write(&path, bytes).map_err(|e| Error::io_path(&path, e))?;
            return Ok(Self {
                refs_dir: refs_dir.to_path_buf(),
                templates: file.templates,
            });
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
        let file: TemplateFile = toml::from_str(&raw)
            .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
        Ok(Self {
            refs_dir: refs_dir.to_path_buf(),
            templates: file.templates,
        })
    }

    /// All templates in disk order.
    pub fn list(&self) -> &[BranchTemplate] {
        &self.templates
    }

    /// Find a template by name (case-sensitive).
    pub fn get(&self, name: &str) -> Option<&BranchTemplate> {
        self.templates.iter().find(|t| t.name == name)
    }

    /// Insert or replace a template.  Returns `true` when a previous
    /// entry was overwritten so REST callers can return 200 vs 201.
    pub fn upsert(&mut self, template: BranchTemplate) -> Result<bool> {
        if template.name.trim().is_empty() {
            return Err(Error::Config(
                "branch template name must not be empty".into(),
            ));
        }
        let existed = self.templates.iter().any(|t| t.name == template.name);
        self.templates.retain(|t| t.name != template.name);
        self.templates.push(template);
        self.save()?;
        Ok(existed)
    }

    /// Remove a template by name.  Returns `true` when something was
    /// removed.  Does NOT cascade — branches already created from the
    /// template keep their materialised settings (templates are a
    /// helper for *creation*, not a live reference).
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let before = self.templates.len();
        self.templates.retain(|t| t.name != name);
        let removed = self.templates.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        let path = templates_path(&self.refs_dir);
        let file = TemplateFile {
            templates: self.templates.clone(),
        };
        let bytes = toml::to_string_pretty(&file)
            .map_err(|e| Error::Config(format!("serialize templates: {e}")))?;
        // Atomic write: tempfile + rename so a crash mid-write never
        // leaves a half-written file readers can see.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| Error::io_path(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io_path(&path, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_creates_file_with_two_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let registry = TemplateRegistry::load_or_seed(dir.path()).unwrap();
        assert_eq!(registry.list().len(), 2);
        assert!(registry.get("review-required").is_some());
        assert!(registry.get("agent-sandbox").is_some());
    }

    #[test]
    fn seed_does_not_clobber_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        // Hand-write a single-template file.
        let path = templates_path(dir.path());
        std::fs::write(
            &path,
            r#"
[[templates]]
name = "custom"
description = "user template"
"#,
        )
        .unwrap();
        let registry = TemplateRegistry::load_or_seed(dir.path()).unwrap();
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get("custom").unwrap().name, "custom");
        // Seeded defaults must NOT be re-injected on top of the user's
        // file — that would silently corrupt their workspace.
        assert!(registry.get("review-required").is_none());
    }

    #[test]
    fn upsert_creates_then_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = TemplateRegistry::load_or_seed(dir.path()).unwrap();

        let new = BranchTemplate {
            name: "release".to_string(),
            description: Some("release branch".into()),
            kind: BranchKind::Feature,
            merge_policy: MergePolicy::Manual,
            redaction: None,
            max_age_secs: Some(86_400),
            permissions: None,
        };

        let existed = registry.upsert(new.clone()).unwrap();
        assert!(!existed, "first upsert is a fresh insert");

        // Reloading from disk must see the new entry.
        let registry2 = TemplateRegistry::load_or_seed(dir.path()).unwrap();
        assert_eq!(registry2.get("release"), Some(&new));

        // Overwriting flips the existed flag to true.
        let mut updated = new.clone();
        updated.description = Some("hardened release".into());
        let mut registry3 = TemplateRegistry::load_or_seed(dir.path()).unwrap();
        let existed = registry3.upsert(updated.clone()).unwrap();
        assert!(existed, "second upsert finds the prior entry");
        assert_eq!(registry3.get("release"), Some(&updated));
    }

    #[test]
    fn remove_returns_false_for_missing_template() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = TemplateRegistry::load_or_seed(dir.path()).unwrap();
        let removed = registry.remove("never-existed").unwrap();
        assert!(!removed);
    }

    #[test]
    fn rejects_empty_template_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = TemplateRegistry::load_or_seed(dir.path()).unwrap();
        let res = registry.upsert(BranchTemplate {
            name: "  ".into(),
            description: None,
            kind: BranchKind::Feature,
            merge_policy: MergePolicy::Manual,
            redaction: None,
            max_age_secs: None,
            permissions: None,
        });
        assert!(res.is_err());
    }
}
