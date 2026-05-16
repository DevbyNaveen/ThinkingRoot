// crates/thinkingroot-branch/src/branch.rs
use crate::snapshot::slugify;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thinkingroot_core::Result;
use thinkingroot_core::error::Error;
use thinkingroot_core::{
    BranchKind, BranchPermissions, BranchRef, BranchStatus, MergePolicy, MergedBy, RedactionPolicy,
};

const REGISTRY_FILE: &str = "branches.toml";
const HEAD_FILE: &str = "HEAD";

/// Project a [`MergedBy`] to the canonical "actor" string used in
/// [`thinkingroot_core::BranchEvent`] audit-log entries.  Mirrors
/// `Principal::identity()` shape so consumers (audit tail, lineage
/// DAG) can join on the same key set without a second lookup.
fn merger_identity(merged_by: &MergedBy) -> String {
    match merged_by {
        MergedBy::Human { user } => user.clone(),
        MergedBy::Agent { agent_id } => agent_id.clone(),
        MergedBy::Connector {
            connector_id,
            install_id,
        } => format!("{connector_id}:{install_id}"),
        MergedBy::System => "system".to_string(),
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct RegistryFile {
    #[serde(default, rename = "branch")]
    branches: Vec<BranchRef>,
}

/// Manages the `.thinkingroot-refs/branches.toml` registry.
pub struct BranchRegistry {
    refs_dir: PathBuf,
    data: RegistryFile,
}

impl BranchRegistry {
    /// Load registry from disk, or create an empty one if it doesn't exist.
    pub fn load_or_create(refs_dir: &Path) -> Result<Self> {
        let data = Self::read_registry_file(refs_dir)?;
        Ok(Self {
            refs_dir: refs_dir.to_path_buf(),
            data,
        })
    }

    /// Read `branches.toml` from disk, returning an empty registry if
    /// the file is absent.  Used both by [`Self::load_or_create`] and
    /// by every mutating method to refresh the in-memory copy *inside*
    /// the registry lock — so concurrent processes / threads always
    /// observe the latest persisted state before they mutate.
    fn read_registry_file(refs_dir: &Path) -> Result<RegistryFile> {
        let path = refs_dir.join(REGISTRY_FILE);
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            toml::from_str(&content).map_err(|e| Error::Config(e.to_string()))
        } else {
            Ok(RegistryFile::default())
        }
    }

    /// Save registry to disk atomically (tmp + rename).
    ///
    /// Atomicity at the file-system level (the rename) gives readers
    /// an all-or-nothing view; cross-process and cross-thread write
    /// safety is provided by [`crate::lock::RegistryLock`], which
    /// every mutating method on this struct acquires before reloading
    /// + saving.  Calling `save()` directly without holding the lock
    /// will not corrupt the file but can lose concurrent writes — use
    /// the higher-level mutating methods instead.
    pub fn save(&self) -> Result<()> {
        let path = self.refs_dir.join(REGISTRY_FILE);
        let content =
            toml::to_string_pretty(&self.data).map_err(|e| Error::Serialization(e.to_string()))?;
        thinkingroot_core::atomic_write(&path, content.as_bytes(), None)?;
        Ok(())
    }

    /// Create a new branch entry. Errors if an active branch with that name already exists.
    pub fn create_branch(
        &mut self,
        name: &str,
        parent: &str,
        description: Option<String>,
    ) -> Result<BranchRef> {
        self.create_branch_with_owner(
            name,
            parent,
            description,
            None,
            BranchPermissions::default(),
        )
    }

    /// Create a new branch entry with optional owner + explicit permissions.
    /// Kind defaults to [`BranchKind::Feature`] and merge policy defaults to
    /// [`MergePolicy::Manual`] — call [`Self::create_branch_full`] when
    /// either needs a non-default value (e.g. `Stream` branches created
    /// by `mcp/mod.rs::ensure_session_branch` or `Sandbox` branches
    /// created by an agent contribution path).
    pub fn create_branch_with_owner(
        &mut self,
        name: &str,
        parent: &str,
        description: Option<String>,
        owner: Option<String>,
        permissions: BranchPermissions,
    ) -> Result<BranchRef> {
        self.create_branch_full(
            name,
            parent,
            description,
            owner,
            permissions,
            BranchKind::default(),
            MergePolicy::default(),
            None,
        )
    }

    /// Create a new branch entry, threading the full T0.6 attribute set
    /// (kind + merge_policy) plus the T2.6 redaction policy.
    ///
    /// Callers that don't care about kind/policy/redaction should keep
    /// using [`Self::create_branch_with_owner`] — the defaults match
    /// the historical behaviour.
    ///
    /// Cross-process / cross-thread safe: acquires
    /// [`crate::lock::RegistryLock`] before reloading the registry from
    /// disk.  Two concurrent callers each see the other's prior writes
    /// and never lose a branch to a load-modify-save race.
    #[allow(clippy::too_many_arguments)]
    pub fn create_branch_full(
        &mut self,
        name: &str,
        parent: &str,
        description: Option<String>,
        owner: Option<String>,
        permissions: BranchPermissions,
        kind: BranchKind,
        merge_policy: MergePolicy,
        redaction: Option<RedactionPolicy>,
    ) -> Result<BranchRef> {
        let _lock = crate::lock::RegistryLock::acquire(&self.refs_dir)?;
        self.data = Self::read_registry_file(&self.refs_dir)?;

        if self
            .data
            .branches
            .iter()
            .any(|b| b.name == name && matches!(b.status, BranchStatus::Active))
        {
            return Err(Error::BranchAlreadyExists(name.to_string()));
        }
        let now = Utc::now();
        let mut branch = BranchRef {
            name: name.to_string(),
            slug: slugify(name),
            parent: parent.to_string(),
            created_at: now,
            status: BranchStatus::Active,
            description,
            owner: owner.clone(),
            permissions,
            kind,
            merge_policy,
            redaction,
            // T0.5 — set separately by lib.rs::create_branch_full via
            // `set_parent_commit_hash` after the parent's graph.db has
            // been hashed.  Defaults to None so legacy callers (and
            // tests) continue to compile without LCA tracking.
            parent_commit_hash: None,
            // T2.3 — opt-in TTL.  `None` preserves pre-T2.3 default
            // ("never expire on age").  Callers that want a TTL on a
            // brand-new branch use [`Self::set_max_age_secs`] right
            // after creation.
            max_age_secs: None,
            // T1.3 — audit log starts with a single Created entry so
            // the lineage DAG (T1.7) can answer "when was this branch
            // forked?" without consulting `created_at` separately.
            events: Vec::new(),
        };
        branch.append_event(thinkingroot_core::BranchEvent::Created {
            at: now,
            actor: owner.unwrap_or_else(|| "system".to_string()),
            parent: parent.to_string(),
        });
        self.data.branches.push(branch.clone());
        self.save()?;
        Ok(branch)
    }

    /// Set the T2.3 TTL on an existing active branch.  `None` clears
    /// the TTL.  Returns the updated branch.  Lock-protected.
    pub fn set_max_age_secs(
        &mut self,
        name: &str,
        max_age_secs: Option<u64>,
    ) -> Result<BranchRef> {
        let _lock = crate::lock::RegistryLock::acquire(&self.refs_dir)?;
        self.data = Self::read_registry_file(&self.refs_dir)?;

        let branch = self
            .data
            .branches
            .iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
        branch.max_age_secs = max_age_secs;
        let updated = branch.clone();
        self.save()?;
        Ok(updated)
    }

    /// Set the T0.5 LCA pointer on an existing active branch.
    ///
    /// Called from `lib.rs::create_branch_full` immediately after the
    /// parent's `graph.db` has been BLAKE3-hashed and copied to the
    /// branch's `graph.db.parent-at-fork`.  Splitting this from the
    /// main create path keeps `BranchRegistry::create_branch_full`'s
    /// signature small (no 10th argument) and lets legacy create
    /// paths leave `parent_commit_hash = None`.
    ///
    /// Lock-protected: acquires [`crate::lock::RegistryLock`] and
    /// reloads the on-disk state before mutating, mirroring every
    /// other mutating method on this struct.
    pub fn set_parent_commit_hash(
        &mut self,
        name: &str,
        hash: String,
    ) -> Result<()> {
        let _lock = crate::lock::RegistryLock::acquire(&self.refs_dir)?;
        self.data = Self::read_registry_file(&self.refs_dir)?;

        let branch = self
            .data
            .branches
            .iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
        branch.parent_commit_hash = Some(hash);
        self.save()
    }

    /// Update the human-readable `description` on an existing active
    /// branch and persist. `None` clears the description.
    ///
    /// Phase B.1 (2026-05-17): drives the topic-branch title flow —
    /// the first user message of a chat session is persisted on the
    /// stream branch's description, and `maintenance::cleanup_once`
    /// propagates that description onto the auto-created topic branch
    /// at merge time so the UI can surface a meaningful title.
    ///
    /// Lock-protected: acquires [`crate::lock::RegistryLock`] and
    /// reloads the on-disk state before mutating.
    pub fn set_description(
        &mut self,
        name: &str,
        description: Option<String>,
    ) -> Result<BranchRef> {
        let _lock = crate::lock::RegistryLock::acquire(&self.refs_dir)?;
        self.data = Self::read_registry_file(&self.refs_dir)?;

        let branch = self
            .data
            .branches
            .iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
        branch.description = description;
        let updated = branch.clone();
        self.save()?;
        Ok(updated)
    }

    /// Update the redaction policy on an existing active branch and
    /// persist. Returns the updated branch.
    ///
    /// Lock-protected: acquires [`crate::lock::RegistryLock`] and
    /// reloads the on-disk state before mutating.
    pub fn set_redaction(
        &mut self,
        name: &str,
        policy: Option<RedactionPolicy>,
    ) -> Result<BranchRef> {
        let _lock = crate::lock::RegistryLock::acquire(&self.refs_dir)?;
        self.data = Self::read_registry_file(&self.refs_dir)?;

        let branch = self
            .data
            .branches
            .iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
        let now = Utc::now();
        let enabled = policy.is_some();
        branch.redaction = policy;
        branch.append_event(thinkingroot_core::BranchEvent::RedactionUpdated {
            at: now,
            actor: "system".into(),
            enabled,
        });
        let updated = branch.clone();
        self.save()?;
        Ok(updated)
    }

    /// Mark a branch as merged.
    ///
    /// Lock-protected: acquires [`crate::lock::RegistryLock`] and
    /// reloads the on-disk state before mutating.
    pub fn mark_merged(&mut self, name: &str, merged_by: MergedBy) -> Result<()> {
        self.mark_merged_into(name, merged_by, None)
    }

    /// Like [`Self::mark_merged`] but threads through an
    /// `authorising_proposal_id` so the audit log captures the T0.4
    /// proposal that authorised this merge (when one was used).
    pub fn mark_merged_into(
        &mut self,
        name: &str,
        merged_by: MergedBy,
        authorising_proposal_id: Option<String>,
    ) -> Result<()> {
        let _lock = crate::lock::RegistryLock::acquire(&self.refs_dir)?;
        self.data = Self::read_registry_file(&self.refs_dir)?;

        let now = Utc::now();
        let actor = merger_identity(&merged_by);
        let parent = {
            let parent_branch = self
                .data
                .branches
                .iter()
                .find(|b| b.name == name)
                .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
            parent_branch.parent.clone()
        };
        let branch = self
            .data
            .branches
            .iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
        branch.append_event(thinkingroot_core::BranchEvent::Merged {
            at: now,
            actor,
            into: parent,
            authorising_proposal_id,
        });
        branch.status = BranchStatus::Merged {
            merged_at: now,
            merged_by,
        };
        self.save()
    }

    /// Mark a branch as abandoned (soft delete — data dir kept).
    ///
    /// Lock-protected: acquires [`crate::lock::RegistryLock`] and
    /// reloads the on-disk state before mutating.
    pub fn abandon_branch(&mut self, name: &str) -> Result<()> {
        let _lock = crate::lock::RegistryLock::acquire(&self.refs_dir)?;
        self.data = Self::read_registry_file(&self.refs_dir)?;

        let branch = self
            .data
            .branches
            .iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
        let now = Utc::now();
        branch.append_event(thinkingroot_core::BranchEvent::Abandoned {
            at: now,
            actor: "system".into(),
        });
        branch.status = BranchStatus::Abandoned { abandoned_at: now };
        self.save()
    }

    /// Get all active branches.
    pub fn list_active(&self) -> Vec<&BranchRef> {
        self.data
            .branches
            .iter()
            .filter(|b| matches!(b.status, BranchStatus::Active))
            .collect()
    }

    /// Get every branch in the registry (active + merged + abandoned).
    /// Used by audit-log + lineage views that need to walk historical
    /// state, not just live branches.
    pub fn all(&self) -> Vec<&BranchRef> {
        self.data.branches.iter().collect()
    }

    /// Get a branch by name (active only).
    pub fn get(&self, name: &str) -> Option<&BranchRef> {
        self.data
            .branches
            .iter()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
    }

    /// Get all abandoned branches.
    pub fn list_abandoned(&self) -> Vec<&BranchRef> {
        self.data
            .branches
            .iter()
            .filter(|b| matches!(b.status, BranchStatus::Abandoned { .. }))
            .collect()
    }
}

/// Read the active HEAD branch name.
/// Returns "main" if no HEAD file exists.
pub fn read_head(refs_dir: &Path) -> Result<String> {
    let path = refs_dir.join(HEAD_FILE);
    if path.exists() {
        let content = fs::read_to_string(&path)?;
        Ok(content.trim().to_string())
    } else {
        Ok("main".to_string())
    }
}

/// Write the active HEAD branch name atomically.  Validates the
/// branch name first so a malformed value (`..`, control chars,
/// NULs, leading `.`) cannot leave HEAD pointing at a path no
/// registry entry matches and cannot escape the refs directory.
///
/// Allows `/` and `-` — a `feature/x` style hierarchy is the same
/// convention git uses, and the registry's TOML keys handle it.
pub fn write_head(refs_dir: &Path, branch_name: &str) -> Result<()> {
    validate_branch_name(branch_name)
        .map_err(|msg| Error::BranchNotFound(format!("invalid branch name: {msg}")))?;
    let path = refs_dir.join(HEAD_FILE);
    thinkingroot_core::atomic_write(&path, branch_name.as_bytes(), None)?;
    Ok(())
}

/// Validate a branch name. Stricter than [`thinkingroot_core::validate_id`]
/// in some ways (no leading dot, no `..` segment) but allows `/` so
/// `feature/x` style hierarchies work.
///
/// Rejects: empty, > 255 bytes, NUL byte, backslash, control
/// characters, path-traversal segments (`.` or `..` between
/// slashes), names starting with `.` or `-`.
fn validate_branch_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("name is empty".into());
    }
    if name.len() > 255 {
        return Err(format!("name exceeds 255 chars: {} chars", name.len()));
    }
    if name.starts_with('.') || name.starts_with('-') || name.starts_with('/') {
        return Err(format!("name starts with `{}`", &name[..1]));
    }
    if name.ends_with('/') {
        return Err("name ends with `/`".into());
    }
    if name.contains("//") {
        return Err("name contains `//`".into());
    }
    for ch in name.chars() {
        match ch {
            '\\' | '\0' => return Err(format!("forbidden character `{ch}`")),
            c if c.is_control() => {
                return Err("contains control character".into());
            }
            _ => {}
        }
    }
    for segment in name.split('/') {
        if segment == "." || segment == ".." {
            return Err(format!("path-traversal segment `{segment}`"));
        }
    }
    Ok(())
}
