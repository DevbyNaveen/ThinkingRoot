// crates/thinkingroot-branch/src/branch.rs
use crate::snapshot::slugify;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thinkingroot_core::Result;
use thinkingroot_core::error::Error;
use thinkingroot_core::{BranchPermissions, BranchRef, BranchStatus, MergedBy};

const REGISTRY_FILE: &str = "branches.toml";
const HEAD_FILE: &str = "HEAD";

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
        let path = refs_dir.join(REGISTRY_FILE);
        let data = if path.exists() {
            let content = fs::read_to_string(&path)?;
            toml::from_str(&content).map_err(|e| Error::Config(e.to_string()))?
        } else {
            RegistryFile::default()
        };
        Ok(Self {
            refs_dir: refs_dir.to_path_buf(),
            data,
        })
    }

    /// Save registry to disk atomically (tmp + rename).
    ///
    /// Concurrent `root branch create` / `root merge` invocations
    /// against the same workspace previously raced on a non-atomic
    /// write that could leave an empty registry.toml on a crash.
    /// The rename here makes the file change visible all-or-nothing;
    /// the read-modify-write window between two concurrent callers
    /// is still last-writer-wins, but neither produces a corrupt
    /// file.
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
    pub fn create_branch_with_owner(
        &mut self,
        name: &str,
        parent: &str,
        description: Option<String>,
        owner: Option<String>,
        permissions: BranchPermissions,
    ) -> Result<BranchRef> {
        if self
            .data
            .branches
            .iter()
            .any(|b| b.name == name && matches!(b.status, BranchStatus::Active))
        {
            return Err(Error::BranchAlreadyExists(name.to_string()));
        }
        let branch = BranchRef {
            name: name.to_string(),
            slug: slugify(name),
            parent: parent.to_string(),
            created_at: Utc::now(),
            status: BranchStatus::Active,
            description,
            owner,
            permissions,
        };
        self.data.branches.push(branch.clone());
        self.save()?;
        Ok(branch)
    }

    /// Mark a branch as merged.
    pub fn mark_merged(&mut self, name: &str, merged_by: MergedBy) -> Result<()> {
        let branch = self
            .data
            .branches
            .iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
        branch.status = BranchStatus::Merged {
            merged_at: Utc::now(),
            merged_by,
        };
        self.save()
    }

    /// Mark a branch as abandoned (soft delete — data dir kept).
    pub fn abandon_branch(&mut self, name: &str) -> Result<()> {
        let branch = self
            .data
            .branches
            .iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
        branch.status = BranchStatus::Abandoned {
            abandoned_at: Utc::now(),
        };
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
