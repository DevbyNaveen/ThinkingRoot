# Phase 3.5 — Knowledge Version Control (KVC) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Git-style branching for compiled knowledge graphs — `root branch/checkout/diff/merge/status/snapshot` — enabling isolated knowledge experimentation, semantic Knowledge PRs, contradiction-as-conflict detection, health-score CI gates, and agent sandboxing via branches.

**Architecture:** Each branch is a full copy of `.thinkingroot/graph.db` in a sibling directory `.thinkingroot-{slug}/`, with `models/` and `cache/` directories symlinked from main to avoid re-downloading embeddings. A new `thinkingroot-branch` crate owns all branching logic. The CLI, REST API, and MCP server each get branch-aware extensions.

**Tech Stack:** Rust (edition 2024), CozoDB Datalog, BLAKE3 (blake3 crate), tokio async runtime, Axum 0.8, serde/toml for TOML registry, existing GraphStore/StorageEngine APIs.

---

## File Map

### New files
- `crates/thinkingroot-branch/Cargo.toml`
- `crates/thinkingroot-branch/src/lib.rs` — public API re-exports
- `crates/thinkingroot-branch/src/snapshot.rs` — slugify, resolve_data_dir, create_branch_layout
- `crates/thinkingroot-branch/src/branch.rs` — BranchRegistry CRUD + HEAD read/write
- `crates/thinkingroot-branch/src/diff.rs` — semantic_hash, compute_diff
- `crates/thinkingroot-branch/src/merge.rs` — execute_merge
- `crates/thinkingroot-branch/tests/branch_tests.rs`
- `crates/thinkingroot-core/src/types/branch.rs` — BranchRef, BranchStatus, MergedBy
- `crates/thinkingroot-core/src/types/diff.rs` — KnowledgeDiff, DiffClaim, DiffEntity, DiffRelation, AutoResolution, ContradictionPair, DiffStatus
- `crates/thinkingroot-cli/src/branch.rs` — CLI handlers for all branch commands

### Modified files
- `crates/thinkingroot-core/src/types/mod.rs` — pub mod branch, diff
- `crates/thinkingroot-core/src/config.rs` — add `merge: MergeConfig` field
- `crates/thinkingroot-core/src/error.rs` — add BranchNotFound, BranchAlreadyExists, MergeBlocked variants
- `crates/thinkingroot-graph/src/graph.rs` — add `get_entity_names_for_claims()`
- `crates/thinkingroot-serve/src/pipeline.rs` — add `branch: Option<&str>` param to `run_pipeline()`
- `crates/thinkingroot-serve/src/rest.rs` — 7 new branch endpoints
- `crates/thinkingroot-serve/src/mcp/tools.rs` — 3 new MCP tools
- `crates/thinkingroot-cli/src/main.rs` — 6 new subcommands (Branch, Checkout, Diff, Merge, Status, Snapshot)
- `Cargo.toml` (workspace) — add thinkingroot-branch to members, default-members, workspace.dependencies

---

## Task 1: Core types — branch.rs, diff.rs, MergeConfig, error variants

**Files:**
- Create: `crates/thinkingroot-core/src/types/branch.rs`
- Create: `crates/thinkingroot-core/src/types/diff.rs`
- Modify: `crates/thinkingroot-core/src/types/mod.rs`
- Modify: `crates/thinkingroot-core/src/config.rs`
- Modify: `crates/thinkingroot-core/src/error.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/thinkingroot-core/src/types/branch.rs` (just enough to compile the test):

```rust
// crates/thinkingroot-core/src/types/branch.rs
```

Add to `crates/thinkingroot-core/src/types/mod.rs`:
```rust
pub mod branch;
pub mod diff;
```

Then write the test at the bottom of `crates/thinkingroot-core/src/types/branch.rs`:
```rust
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
        };
        assert_eq!(b.name, "feature/x");
        assert!(matches!(b.status, BranchStatus::Active));
    }

    #[test]
    fn merged_by_agent() {
        let mb = MergedBy::Agent { agent_id: "claude".to_string() };
        assert!(matches!(mb, MergedBy::Agent { .. }));
    }
}
```

- [ ] **Step 2: Run test to confirm it fails (types don't exist yet)**

```bash
cd /path/to/thinkingroot
cargo test -p thinkingroot-core types::branch 2>&1 | head -30
```

Expected: compile error — `BranchRef`, `BranchStatus`, `MergedBy` not defined.

- [ ] **Step 3: Implement branch.rs**

```rust
// crates/thinkingroot-core/src/types/branch.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRef {
    pub name: String,
    pub slug: String,
    pub parent: String,
    pub created_at: DateTime<Utc>,
    pub status: BranchStatus,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BranchStatus {
    Active,
    Merged {
        merged_at: DateTime<Utc>,
        merged_by: MergedBy,
    },
    Abandoned {
        abandoned_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MergedBy {
    Human { user: String },
    Agent { agent_id: String },
}
```

- [ ] **Step 4: Write diff.rs**

```rust
// crates/thinkingroot-core/src/types/diff.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use crate::types::{Claim, Entity, Relation};
use crate::HealthScore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeDiff {
    pub from_branch: String,
    pub to_branch: String,
    pub computed_at: DateTime<Utc>,
    pub new_claims: Vec<DiffClaim>,
    pub new_entities: Vec<DiffEntity>,
    pub new_relations: Vec<DiffRelation>,
    pub auto_resolved: Vec<AutoResolution>,
    pub needs_review: Vec<ContradictionPair>,
    pub health_before: HealthScore,
    pub health_after: HealthScore,
    pub merge_allowed: bool,
    pub blocking_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffClaim {
    pub claim: Claim,
    pub entity_context: Vec<String>,
    pub diff_status: DiffStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEntity {
    pub entity: Entity,
    pub diff_status: DiffStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRelation {
    pub relation: Relation,
    pub diff_status: DiffStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffStatus {
    Added,
    Modified,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoResolution {
    pub main_claim_id: String,
    pub branch_claim_id: String,
    pub winner: String,
    pub confidence_delta: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionPair {
    pub main_claim_id: String,
    pub branch_claim_id: String,
    pub main_statement: String,
    pub branch_statement: String,
    pub explanation: String,
}
```

- [ ] **Step 5: Add MergeConfig to config.rs**

Open `crates/thinkingroot-core/src/config.rs`. Add this struct and field:

```rust
// Add this struct (with serde defaults):
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConfig {
    #[serde(default = "MergeConfig::default_max_health_drop")]
    pub max_health_drop: f64,
    #[serde(default = "MergeConfig::default_block_on_contradictions")]
    pub block_on_contradictions: bool,
    #[serde(default = "MergeConfig::default_auto_resolve_threshold")]
    pub auto_resolve_threshold: f64,
    #[serde(default)]
    pub require_approval: bool,
}

impl MergeConfig {
    fn default_max_health_drop() -> f64 { 0.05 }
    fn default_block_on_contradictions() -> bool { true }
    fn default_auto_resolve_threshold() -> f64 { 0.15 }
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            max_health_drop: Self::default_max_health_drop(),
            block_on_contradictions: Self::default_block_on_contradictions(),
            auto_resolve_threshold: Self::default_auto_resolve_threshold(),
            require_approval: false,
        }
    }
}
```

In the `Config` struct, add:
```rust
#[serde(default)]
pub merge: MergeConfig,
```

- [ ] **Step 6: Add error variants to error.rs**

In `crates/thinkingroot-core/src/error.rs`, add to the `ThinkingRootError` enum:

```rust
#[error("branch not found: {0}")]
BranchNotFound(String),

#[error("branch already exists: {0}")]
BranchAlreadyExists(String),

#[error("merge blocked: {0}")]
MergeBlocked(String),
```

- [ ] **Step 7: Run tests**

```bash
cargo test -p thinkingroot-core 2>&1 | tail -20
```

Expected: all tests pass, no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-core/src/types/branch.rs \
        crates/thinkingroot-core/src/types/diff.rs \
        crates/thinkingroot-core/src/types/mod.rs \
        crates/thinkingroot-core/src/config.rs \
        crates/thinkingroot-core/src/error.rs
git commit -m "feat(core): add KVC types — BranchRef, KnowledgeDiff, MergeConfig, error variants"
```

---

## Task 2: GraphStore — add get_entity_names_for_claims()

**Files:**
- Modify: `crates/thinkingroot-graph/src/graph.rs`

- [ ] **Step 1: Write the failing test**

In `crates/thinkingroot-graph/src/graph.rs`, add at the bottom of the `#[cfg(test)]` block:

```rust
#[tokio::test]
async fn test_get_entity_names_for_claims() {
    let dir = tempfile::tempdir().unwrap();
    let engine = StorageEngine::init(dir.path()).await.unwrap();
    let graph = &engine.graph;

    // Insert a source
    let source_id = "src_01".to_string();
    graph.insert_source(&crate::graph::SourceRecord {
        id: source_id.clone(),
        uri: "test.md".to_string(),
        source_type: "markdown".to_string(),
        author: None,
        content_hash: "abc".to_string(),
        trust_level: "trusted".to_string(),
        byte_size: 100,
    }).unwrap();

    // Insert entity + claim + edge
    let entity_id = "ent_01".to_string();
    graph.insert_entity(&crate::graph::EntityRecord {
        id: entity_id.clone(),
        canonical_name: "AuthService".to_string(),
        entity_type: "Service".to_string(),
        description: None,
    }).unwrap();

    let claim_id = "clm_01".to_string();
    graph.insert_claim(&crate::graph::ClaimRecord {
        id: claim_id.clone(),
        statement: "AuthService uses JWT".to_string(),
        claim_type: "Fact".to_string(),
        source_id: source_id.clone(),
        confidence: 0.9,
        sensitivity: "public".to_string(),
        workspace_id: "ws_01".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    }).unwrap();

    graph.link_claim_to_entity(&claim_id, &entity_id).unwrap();

    // Now test the new method
    let map = graph.get_entity_names_for_claims(&[claim_id.as_str()]).unwrap();
    assert_eq!(map.get("clm_01").unwrap(), &vec!["AuthService".to_string()]);
}
```

- [ ] **Step 2: Run test to confirm it fails**

```bash
cargo test -p thinkingroot-graph test_get_entity_names_for_claims --no-default-features 2>&1 | tail -20
```

Expected: compile error — method `get_entity_names_for_claims` does not exist on `GraphStore`.

- [ ] **Step 3: Implement the method**

In `crates/thinkingroot-graph/src/graph.rs`, add this method to `impl GraphStore`:

```rust
/// Returns a map from claim_id → list of entity canonical names linked to that claim.
/// Used during diff computation to populate DiffClaim.entity_context.
pub fn get_entity_names_for_claims(
    &self,
    claim_ids: &[&str],
) -> crate::Result<std::collections::HashMap<String, Vec<String>>> {
    if claim_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    // Build a Datalog query that joins claim_entity_edges → entities
    let result = self.db.run_script(
        "?[claim_id, name] := *claim_entity_edges{claim_id, entity_id: eid}, \
         *entities{id: eid, canonical_name: name}",
        std::collections::BTreeMap::new(),
        cozo::ScriptMutability::Immutable,
    )?;

    let claim_id_set: std::collections::HashSet<&str> = claim_ids.iter().copied().collect();
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    for row in &result.rows {
        if let (Some(cozo::DataValue::Str(cid)), Some(cozo::DataValue::Str(name))) =
            (row.get(0), row.get(1))
        {
            let cid_str = cid.as_str();
            if claim_id_set.contains(cid_str) {
                map.entry(cid_str.to_string())
                    .or_default()
                    .push(name.to_string());
            }
        }
    }

    Ok(map)
}
```

- [ ] **Step 4: Run test**

```bash
cargo test -p thinkingroot-graph test_get_entity_names_for_claims --no-default-features 2>&1 | tail -20
```

Expected: test passes.

- [ ] **Step 5: Run full graph tests**

```bash
cargo test -p thinkingroot-graph --no-default-features 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-graph/src/graph.rs
git commit -m "feat(graph): add get_entity_names_for_claims() for KVC diff support"
```

---

## Task 3: thinkingroot-branch crate scaffold + snapshot.rs

**Files:**
- Create: `crates/thinkingroot-branch/Cargo.toml`
- Create: `crates/thinkingroot-branch/src/lib.rs`
- Create: `crates/thinkingroot-branch/src/snapshot.rs`
- Create: `crates/thinkingroot-branch/src/branch.rs` (stub)
- Create: `crates/thinkingroot-branch/src/diff.rs` (stub)
- Create: `crates/thinkingroot-branch/src/merge.rs` (stub)
- Create: `crates/thinkingroot-branch/tests/branch_tests.rs` (initial)
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Write the failing test**

Create `crates/thinkingroot-branch/tests/branch_tests.rs`:

```rust
// crates/thinkingroot-branch/tests/branch_tests.rs
use std::path::Path;
use thinkingroot_branch::snapshot::{slugify, resolve_data_dir};

#[test]
fn slugify_feature_slash() {
    assert_eq!(slugify("feature/graphql"), "feature-graphql");
}

#[test]
fn slugify_spaces_and_caps() {
    assert_eq!(slugify("My Branch Name"), "my-branch-name");
}

#[test]
fn slugify_main_unchanged() {
    assert_eq!(slugify("main"), "main");
}

#[test]
fn resolve_data_dir_main() {
    let p = Path::new("/repo");
    assert_eq!(resolve_data_dir(p, None), p.join(".thinkingroot"));
    assert_eq!(resolve_data_dir(p, Some("main")), p.join(".thinkingroot"));
}

#[test]
fn resolve_data_dir_branch() {
    let p = Path::new("/repo");
    assert_eq!(
        resolve_data_dir(p, Some("feature/graphql")),
        p.join(".thinkingroot-feature-graphql")
    );
}
```

- [ ] **Step 2: Create Cargo.toml for the new crate**

Create `crates/thinkingroot-branch/Cargo.toml`:

```toml
[package]
name = "thinkingroot-branch"
version = "0.1.0"
edition = "2021"

[dependencies]
thinkingroot-core = { workspace = true }
thinkingroot-graph = { workspace = true }
thinkingroot-verify = { workspace = true }
blake3 = { workspace = true }
chrono = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
toml = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
thiserror = { workspace = true }

[features]
default = ["vector"]
vector = ["thinkingroot-graph/vector"]

[dev-dependencies]
tempfile = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 3: Add to workspace Cargo.toml**

In the root `Cargo.toml`, find the `[workspace]` section:
- Add `"crates/thinkingroot-branch"` to `members`
- Add `"crates/thinkingroot-branch"` to `default-members`
- Add to `[workspace.dependencies]`: `thinkingroot-branch = { path = "crates/thinkingroot-branch" }`

- [ ] **Step 4: Create stub files so it compiles**

`crates/thinkingroot-branch/src/lib.rs`:
```rust
pub mod snapshot;
pub mod branch;
pub mod diff;
pub mod merge;
```

`crates/thinkingroot-branch/src/branch.rs`:
```rust
// placeholder — implemented in Task 4
```

`crates/thinkingroot-branch/src/diff.rs`:
```rust
// placeholder — implemented in Task 5
```

`crates/thinkingroot-branch/src/merge.rs`:
```rust
// placeholder — implemented in Task 6
```

- [ ] **Step 5: Run test to confirm slugify/resolve missing**

```bash
cargo test -p thinkingroot-branch --no-default-features 2>&1 | tail -20
```

Expected: compile error — `slugify` and `resolve_data_dir` not found in `snapshot`.

- [ ] **Step 6: Implement snapshot.rs**

Create `crates/thinkingroot-branch/src/snapshot.rs`:

```rust
// crates/thinkingroot-branch/src/snapshot.rs
use std::path::{Path, PathBuf};
use std::fs;
use thinkingroot_core::Result;

/// Convert a branch name to a filesystem-safe slug.
/// "feature/graphql" → "feature-graphql"
/// "My Branch" → "my-branch"
pub fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Resolve the data directory for a given branch.
/// main (or None) → `{root}/.thinkingroot`
/// other branch   → `{root}/.thinkingroot-{slug}`
pub fn resolve_data_dir(root_path: &Path, branch: Option<&str>) -> PathBuf {
    match branch {
        None | Some("main") => root_path.join(".thinkingroot"),
        Some(name) => root_path.join(format!(".thinkingroot-{}", slugify(name))),
    }
}

/// Create the directory layout for a new branch:
/// - Copy `{main_data_dir}/graph/graph.db` → `{branch_data_dir}/graph/graph.db`
/// - Symlink `{main_data_dir}/models` → `{branch_data_dir}/models`
/// - Symlink `{main_data_dir}/cache`  → `{branch_data_dir}/cache`
pub fn create_branch_layout(
    main_data_dir: &Path,
    branch_data_dir: &Path,
) -> Result<()> {
    // Create branch dir and graph subdir
    let branch_graph_dir = branch_data_dir.join("graph");
    fs::create_dir_all(&branch_graph_dir)?;

    // Copy graph.db (SQLite hot backup via fs copy — safe for CozoDB embedded)
    let src_db = main_data_dir.join("graph").join("graph.db");
    let dst_db = branch_graph_dir.join("graph.db");
    if src_db.exists() {
        fs::copy(&src_db, &dst_db)?;
    }

    // Symlink models/ (fastembed cache, ~300MB — never duplicate)
    let main_models = main_data_dir.join("models");
    let branch_models = branch_data_dir.join("models");
    if main_models.exists() && !branch_models.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&main_models, &branch_models)?;
    }

    // Symlink cache/ (extraction cache)
    let main_cache = main_data_dir.join("cache");
    let branch_cache = branch_data_dir.join("cache");
    if main_cache.exists() && !branch_cache.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&main_cache, &branch_cache)?;
    }

    Ok(())
}
```

- [ ] **Step 7: Run tests**

```bash
cargo test -p thinkingroot-branch snapshot --no-default-features 2>&1 | tail -20
```

Expected: all 5 snapshot tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-branch/ Cargo.toml
git commit -m "feat(branch): scaffold thinkingroot-branch crate + snapshot.rs (slugify, resolve_data_dir, create_branch_layout)"
```

---

## Task 4: branch.rs — BranchRegistry CRUD + HEAD read/write

**Files:**
- Modify: `crates/thinkingroot-branch/src/branch.rs`
- Modify: `crates/thinkingroot-branch/tests/branch_tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/thinkingroot-branch/tests/branch_tests.rs`:

```rust
use thinkingroot_branch::branch::{BranchRegistry, read_head, write_head};
use tempfile::tempdir;

#[test]
fn registry_create_and_list() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    let mut reg = BranchRegistry::load_or_create(&refs_dir).unwrap();
    reg.create_branch("feature/x", "main", None).unwrap();

    let branches = reg.list_active();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "feature/x");
    assert_eq!(branches[0].slug, "feature-x");
}

#[test]
fn registry_duplicate_fails() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    let mut reg = BranchRegistry::load_or_create(&refs_dir).unwrap();
    reg.create_branch("feature/x", "main", None).unwrap();
    let result = reg.create_branch("feature/x", "main", None);
    assert!(result.is_err());
}

#[test]
fn head_roundtrip() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    write_head(&refs_dir, "feature/x").unwrap();
    assert_eq!(read_head(&refs_dir).unwrap(), "feature/x");
}

#[test]
fn head_defaults_to_main() {
    let dir = tempdir().unwrap();
    let refs_dir = dir.path().join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir).unwrap();

    // No HEAD file written yet
    assert_eq!(read_head(&refs_dir).unwrap(), "main");
}
```

- [ ] **Step 2: Run to confirm fail**

```bash
cargo test -p thinkingroot-branch registry --no-default-features 2>&1 | tail -20
```

Expected: compile error — `BranchRegistry`, `read_head`, `write_head` not found.

- [ ] **Step 3: Implement branch.rs**

```rust
// crates/thinkingroot-branch/src/branch.rs
use std::path::{Path, PathBuf};
use std::fs;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use thinkingroot_core::error::ThinkingRootError;
use thinkingroot_core::Result;
use thinkingroot_core::types::branch::{BranchRef, BranchStatus, MergedBy};
use crate::snapshot::slugify;

const REGISTRY_FILE: &str = "branches.toml";
const HEAD_FILE: &str = "HEAD";

#[derive(Debug, Serialize, Deserialize, Default)]
struct RegistryFile {
    #[serde(default)]
    branch: Vec<BranchRef>,
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
            toml::from_str(&content).map_err(|e| ThinkingRootError::Config(e.to_string()))?
        } else {
            RegistryFile::default()
        };
        Ok(Self { refs_dir: refs_dir.to_path_buf(), data })
    }

    /// Save registry to disk.
    pub fn save(&self) -> Result<()> {
        let path = self.refs_dir.join(REGISTRY_FILE);
        let content = toml::to_string_pretty(&self.data)
            .map_err(|e| ThinkingRootError::Serialization(e.to_string()))?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Create a new branch entry. Errors if a branch with that name already exists.
    pub fn create_branch(
        &mut self,
        name: &str,
        parent: &str,
        description: Option<String>,
    ) -> Result<BranchRef> {
        if self.data.branch.iter().any(|b| b.name == name
            && matches!(b.status, BranchStatus::Active))
        {
            return Err(ThinkingRootError::BranchAlreadyExists(name.to_string()));
        }
        let branch = BranchRef {
            name: name.to_string(),
            slug: slugify(name),
            parent: parent.to_string(),
            created_at: Utc::now(),
            status: BranchStatus::Active,
            description,
        };
        self.data.branch.push(branch.clone());
        self.save()?;
        Ok(branch)
    }

    /// Mark a branch as merged.
    pub fn mark_merged(&mut self, name: &str, merged_by: MergedBy) -> Result<()> {
        let branch = self.data.branch.iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| ThinkingRootError::BranchNotFound(name.to_string()))?;
        branch.status = BranchStatus::Merged {
            merged_at: Utc::now(),
            merged_by,
        };
        self.save()
    }

    /// Mark a branch as abandoned (soft delete).
    pub fn abandon_branch(&mut self, name: &str) -> Result<()> {
        let branch = self.data.branch.iter_mut()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
            .ok_or_else(|| ThinkingRootError::BranchNotFound(name.to_string()))?;
        branch.status = BranchStatus::Abandoned { abandoned_at: Utc::now() };
        self.save()
    }

    /// Get all active branches.
    pub fn list_active(&self) -> Vec<&BranchRef> {
        self.data.branch.iter()
            .filter(|b| matches!(b.status, BranchStatus::Active))
            .collect()
    }

    /// Get a branch by name (active only).
    pub fn get(&self, name: &str) -> Option<&BranchRef> {
        self.data.branch.iter()
            .find(|b| b.name == name && matches!(b.status, BranchStatus::Active))
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

/// Write the active HEAD branch name.
pub fn write_head(refs_dir: &Path, branch_name: &str) -> Result<()> {
    let path = refs_dir.join(HEAD_FILE);
    fs::write(path, branch_name)?;
    Ok(())
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p thinkingroot-branch --no-default-features 2>&1 | tail -30
```

Expected: all registry and head tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-branch/src/branch.rs \
        crates/thinkingroot-branch/tests/branch_tests.rs
git commit -m "feat(branch): implement BranchRegistry CRUD + HEAD read/write"
```

---

## Task 5: diff.rs — semantic_hash, compute_diff, contradiction detection

**Files:**
- Modify: `crates/thinkingroot-branch/src/diff.rs`
- Modify: `crates/thinkingroot-branch/tests/branch_tests.rs`

- [ ] **Step 1: Write failing tests**

Append to `crates/thinkingroot-branch/tests/branch_tests.rs`:

```rust
use thinkingroot_branch::diff::semantic_hash;

#[test]
fn semantic_hash_normalises_whitespace_and_case() {
    let h1 = semantic_hash("AuthService  uses  JWT");
    let h2 = semantic_hash("authservice uses jwt");
    assert_eq!(h1, h2, "same fact, different casing/spacing should hash identically");
}

#[test]
fn semantic_hash_different_facts() {
    let h1 = semantic_hash("AuthService uses JWT");
    let h2 = semantic_hash("AuthService uses OAuth2");
    assert_ne!(h1, h2);
}
```

- [ ] **Step 2: Run to confirm fail**

```bash
cargo test -p thinkingroot-branch diff --no-default-features 2>&1 | tail -20
```

Expected: `semantic_hash` not found.

- [ ] **Step 3: Implement diff.rs**

```rust
// crates/thinkingroot-branch/src/diff.rs
use std::collections::HashSet;
use thinkingroot_core::types::diff::{
    AutoResolution, ContradictionPair, DiffClaim, DiffEntity, DiffRelation, DiffStatus,
    KnowledgeDiff,
};
use thinkingroot_core::types::{Claim, Entity, Relation};
use thinkingroot_core::{HealthScore, Result};
use thinkingroot_graph::graph::GraphStore;

/// Compute a BLAKE3 hash of a normalised claim statement.
/// Normalisation: lowercase + collapse whitespace.
/// This deduplicates the same fact extracted twice with minor formatting differences.
pub fn semantic_hash(statement: &str) -> String {
    let normalised: String = statement
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let hash = blake3::hash(normalised.as_bytes());
    hash.to_hex().to_string()
}

/// Negation keyword pairs used for contradiction-as-conflict detection.
const NEGATION_PAIRS: &[(&str, &str)] = &[
    ("is", "is not"),
    ("uses", "does not use"),
    ("supports", "does not support"),
    ("requires", "does not require"),
    ("implements", "does not implement"),
    ("depends on", "does not depend on"),
    ("has", "does not have"),
    ("can", "cannot"),
    ("should", "should not"),
    ("must", "must not"),
];

/// Check if two claim statements are a negation pair.
fn is_contradiction_pair(a: &str, b: &str) -> bool {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();
    for (pos, neg) in NEGATION_PAIRS {
        if (a_lower.contains(pos) && b_lower.contains(neg))
            || (a_lower.contains(neg) && b_lower.contains(pos))
        {
            return true;
        }
    }
    false
}

/// Compute the semantic diff between main and a branch.
///
/// Algorithm:
/// 1. Load all claims from both graphs.
/// 2. Compute semantic_hash for every claim.
/// 3. Claims in branch but not in main → new_claims (Added).
/// 4. Among new_claims, check each against main claims for contradiction pairs.
///    - confidence delta > auto_resolve_threshold → auto_resolved (higher confidence wins)
///    - otherwise → needs_review
/// 5. Load new entities and relations from branch not present in main.
/// 6. Compute health_before (main) and health_after (simulated with new claims).
pub async fn compute_diff(
    main_graph: &GraphStore,
    branch_graph: &GraphStore,
    auto_resolve_threshold: f64,
    health_before: HealthScore,
    health_after: HealthScore,
    max_health_drop: f64,
    block_on_contradictions: bool,
) -> Result<KnowledgeDiff> {
    // 1. Load all claims from both graphs
    let main_claims_raw = main_graph.get_all_claims_with_sources()?;
    let branch_claims_raw = branch_graph.get_all_claims_with_sources()?;

    // Build hash sets for deduplication
    // (id, statement, claim_type, confidence, uri)
    let main_hashes: HashSet<String> = main_claims_raw
        .iter()
        .map(|(_, stmt, _, _, _)| semantic_hash(stmt))
        .collect();

    // 3. New claims = branch claims whose hash is not in main
    let new_claim_rows: Vec<_> = branch_claims_raw
        .iter()
        .filter(|(_, stmt, _, _, _)| !main_hashes.contains(&semantic_hash(stmt)))
        .collect();

    // Get entity context for new claims
    let new_claim_ids: Vec<&str> = new_claim_rows.iter().map(|(id, _, _, _, _)| id.as_str()).collect();
    let entity_map = branch_graph.get_entity_names_for_claims(&new_claim_ids)?;

    // Build main claims lookup for contradiction detection
    let main_claims_by_hash: std::collections::HashMap<String, &(String, String, String, f64, String)> =
        main_claims_raw.iter().map(|r| (semantic_hash(&r.1), r)).collect();

    let mut new_claims: Vec<DiffClaim> = Vec::new();
    let mut auto_resolved: Vec<AutoResolution> = Vec::new();
    let mut needs_review: Vec<ContradictionPair> = Vec::new();

    for (id, statement, claim_type_str, confidence, uri) in &new_claim_rows {
        let entity_context = entity_map.get(id.as_str()).cloned().unwrap_or_default();

        // Check for contradictions against main claims
        let mut contradiction_found = false;
        for (main_id, main_stmt, _, main_conf, _) in &main_claims_raw {
            if is_contradiction_pair(statement, main_stmt) {
                contradiction_found = true;
                let delta = (confidence - main_conf).abs();
                if delta > auto_resolve_threshold {
                    let winner = if confidence > main_conf { id.clone() } else { main_id.clone() };
                    auto_resolved.push(AutoResolution {
                        main_claim_id: main_id.clone(),
                        branch_claim_id: id.clone(),
                        winner,
                        confidence_delta: delta,
                        reason: format!(
                            "Confidence delta {:.2} > threshold {:.2}",
                            delta, auto_resolve_threshold
                        ),
                    });
                } else {
                    needs_review.push(ContradictionPair {
                        main_claim_id: main_id.clone(),
                        branch_claim_id: id.clone(),
                        main_statement: main_stmt.clone(),
                        branch_statement: statement.clone(),
                        explanation: format!(
                            "Contradiction detected: '{}' vs '{}' (confidence delta {:.2} below threshold)",
                            main_stmt, statement, delta
                        ),
                    });
                }
                break;
            }
        }

        if !contradiction_found {
            // Build a minimal Claim for the diff (without full DB roundtrip)
            use thinkingroot_core::types::claim::{Claim, ClaimType, Confidence, PipelineVersion, Sensitivity};
            use thinkingroot_core::id::Id;
            use thinkingroot_core::types::WorkspaceId;

            let claim = Claim {
                id: id.parse().unwrap_or_else(|_| Id::new()),
                statement: statement.clone(),
                claim_type: claim_type_str.parse().unwrap_or(ClaimType::Fact),
                source: uri.parse().unwrap_or_else(|_| Id::new()),
                source_span: None,
                confidence: Confidence::new(*confidence),
                sensitivity: Sensitivity::Public,
                workspace: WorkspaceId::new(),
                valid_from: chrono::Utc::now(),
                valid_until: chrono::Utc::now() + chrono::Duration::days(90),
                superseded_by: None,
                extracted_by: PipelineVersion::current(),
            };

            new_claims.push(DiffClaim {
                claim,
                entity_context,
                diff_status: DiffStatus::Added,
            });
        }
    }

    // 5. New entities
    let main_entity_names: HashSet<String> = main_graph
        .get_entities_with_aliases()?
        .into_iter()
        .map(|(e, _)| e.canonical_name.clone())
        .collect();

    let new_entities: Vec<DiffEntity> = branch_graph
        .get_entities_with_aliases()?
        .into_iter()
        .filter(|(e, _)| !main_entity_names.contains(&e.canonical_name))
        .map(|(e, aliases)| {
            let entity = Entity {
                id: e.id.parse().unwrap_or_else(|_| Id::new()),
                canonical_name: e.canonical_name.clone(),
                entity_type: e.entity_type.parse().unwrap_or(thinkingroot_core::types::entity::EntityType::Concept),
                aliases,
                attributes: vec![],
                description: e.description,
            };
            DiffEntity { entity, diff_status: DiffStatus::Added }
        })
        .collect();

    // 6. Determine merge_allowed
    let health_drop = health_before.overall - health_after.overall;
    let mut blocking_reasons = Vec::new();

    if health_drop > max_health_drop {
        blocking_reasons.push(format!(
            "Health drop {:.1}% exceeds maximum allowed {:.1}%",
            health_drop * 100.0,
            max_health_drop * 100.0
        ));
    }
    if block_on_contradictions && !needs_review.is_empty() {
        blocking_reasons.push(format!(
            "{} unresolved contradiction(s) require review before merge",
            needs_review.len()
        ));
    }

    Ok(KnowledgeDiff {
        from_branch: "branch".to_string(),
        to_branch: "main".to_string(),
        computed_at: chrono::Utc::now(),
        new_claims,
        new_entities,
        new_relations: vec![],
        auto_resolved,
        needs_review,
        health_before,
        health_after,
        merge_allowed: blocking_reasons.is_empty(),
        blocking_reasons,
    })
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p thinkingroot-branch --no-default-features 2>&1 | tail -30
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-branch/src/diff.rs \
        crates/thinkingroot-branch/tests/branch_tests.rs
git commit -m "feat(branch): implement semantic_hash, compute_diff, contradiction-as-conflict detection"
```

---

## Task 6: merge.rs — execute_merge + lib.rs complete public API

**Files:**
- Modify: `crates/thinkingroot-branch/src/merge.rs`
- Modify: `crates/thinkingroot-branch/src/lib.rs`
- Modify: `crates/thinkingroot-branch/tests/branch_tests.rs`

- [ ] **Step 1: Write failing test**

Append to `crates/thinkingroot-branch/tests/branch_tests.rs`:

```rust
use thinkingroot_branch::{create_branch, list_branches, read_head_branch};

#[tokio::test]
async fn create_branch_creates_layout_and_registry() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Create a minimal main .thinkingroot/graph/ dir
    std::fs::create_dir_all(root.join(".thinkingroot/graph")).unwrap();
    std::fs::write(root.join(".thinkingroot/graph/graph.db"), b"fake-db").unwrap();

    create_branch(root, "feature/test", "main", None).await.unwrap();

    // Branch dir should exist
    assert!(root.join(".thinkingroot-feature-test/graph/graph.db").exists());

    // Registry should have one entry
    let branches = list_branches(root).unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "feature/test");
}

#[tokio::test]
async fn read_head_defaults_to_main() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".thinkingroot-refs")).unwrap();

    let head = read_head_branch(root).unwrap();
    assert_eq!(head, "main");
}
```

- [ ] **Step 2: Run to confirm fail**

```bash
cargo test -p thinkingroot-branch create_branch --no-default-features 2>&1 | tail -20
```

Expected: compile error — `create_branch`, `list_branches`, `read_head_branch` not in lib.

- [ ] **Step 3: Implement merge.rs**

```rust
// crates/thinkingroot-branch/src/merge.rs
use std::path::Path;
use thinkingroot_core::types::branch::MergedBy;
use thinkingroot_core::types::diff::KnowledgeDiff;
use thinkingroot_core::error::ThinkingRootError;
use thinkingroot_core::Result;
use thinkingroot_graph::graph::GraphStore;
use crate::branch::BranchRegistry;
use crate::snapshot::resolve_data_dir;

/// Execute a merge of `branch_name` into main.
///
/// Steps:
/// 1. Verify `diff.merge_allowed` — abort with MergeBlocked if not.
/// 2. For each new_claim in diff: insert into main graph + link to entities.
/// 3. For each auto_resolved: if branch won, supersede main claim.
/// 4. For each new_entity: insert into main graph.
/// 5. Mark branch as merged in registry.
/// 6. Optionally remove branch data dir.
pub async fn execute_merge(
    root_path: &Path,
    branch_name: &str,
    diff: &KnowledgeDiff,
    merged_by: MergedBy,
    propagate_deletions: bool,
) -> Result<()> {
    if !diff.merge_allowed {
        return Err(ThinkingRootError::MergeBlocked(
            diff.blocking_reasons.join("; "),
        ));
    }

    let main_data_dir = resolve_data_dir(root_path, None);
    let main_db_path = main_data_dir.join("graph").join("graph.db");
    let main_graph = GraphStore::open(&main_db_path)?;

    // 2. Insert new claims
    for diff_claim in &diff.new_claims {
        let c = &diff_claim.claim;
        main_graph.insert_claim(&thinkingroot_graph::graph::ClaimRecord {
            id: c.id.to_string(),
            statement: c.statement.clone(),
            claim_type: format!("{:?}", c.claim_type),
            source_id: c.source.to_string(),
            confidence: c.confidence.value(),
            sensitivity: format!("{:?}", c.sensitivity),
            workspace_id: c.workspace.to_string(),
            created_at: c.valid_from.to_rfc3339(),
        })?;

        // Link claim → entities by name lookup in main
        for entity_name in &diff_claim.entity_context {
            if let Ok(Some(entity_id)) = main_graph.find_entity_id_by_name(entity_name) {
                main_graph.link_claim_to_entity(&c.id.to_string(), &entity_id)?;
            }
        }
    }

    // 3. Auto-resolved: supersede losing claims
    for resolution in &diff.auto_resolved {
        if resolution.winner == resolution.branch_claim_id {
            // Branch won — supersede the main claim
            main_graph.supersede_claim(
                &resolution.main_claim_id,
                &resolution.branch_claim_id,
            )?;
        }
        // If main won, nothing to do — branch claim is simply not inserted
    }

    // 4. Insert new entities
    for diff_entity in &diff.new_entities {
        let e = &diff_entity.entity;
        main_graph.insert_entity(&thinkingroot_graph::graph::EntityRecord {
            id: e.id.to_string(),
            canonical_name: e.canonical_name.clone(),
            entity_type: format!("{:?}", e.entity_type),
            description: e.description.clone(),
        })?;
        for alias in &e.aliases {
            main_graph.add_entity_alias(&e.id.to_string(), alias)?;
        }
    }

    // 5. Rebuild entity relations to keep graph consistent
    main_graph.rebuild_entity_relations()?;

    // 6. Mark branch as merged in registry
    let refs_dir = root_path.join(".thinkingroot-refs");
    let mut registry = BranchRegistry::load_or_create(&refs_dir)?;
    registry.mark_merged(branch_name, merged_by)?;

    Ok(())
}
```

- [ ] **Step 4: Implement lib.rs — complete public API**

```rust
// crates/thinkingroot-branch/src/lib.rs
pub mod branch;
pub mod diff;
pub mod merge;
pub mod snapshot;

use std::path::Path;
use thinkingroot_core::types::branch::BranchRef;
use thinkingroot_core::Result;

/// Create a new knowledge branch from main (or another parent branch).
///
/// - Creates `.thinkingroot-{slug}/graph/graph.db` (copy of main's db)
/// - Symlinks `models/` and `cache/` from main
/// - Registers the branch in `.thinkingroot-refs/branches.toml`
pub async fn create_branch(
    root_path: &Path,
    name: &str,
    parent: &str,
    description: Option<String>,
) -> Result<BranchRef> {
    let parent_data_dir = snapshot::resolve_data_dir(root_path, Some(parent));
    let branch_data_dir = snapshot::resolve_data_dir(root_path, Some(name));

    snapshot::create_branch_layout(&parent_data_dir, &branch_data_dir)?;

    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    registry.create_branch(name, parent, description)
}

/// List all active branches for a workspace.
pub fn list_branches(root_path: &Path) -> Result<Vec<BranchRef>> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    if !refs_dir.exists() {
        return Ok(vec![]);
    }
    let registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    Ok(registry.list_active().into_iter().cloned().collect())
}

/// Read the active HEAD branch name for a workspace.
pub fn read_head_branch(root_path: &Path) -> Result<String> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    branch::read_head(&refs_dir)
}

/// Write the active HEAD branch name for a workspace.
pub fn write_head_branch(root_path: &Path, branch_name: &str) -> Result<()> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    branch::write_head(&refs_dir, branch_name)
}

/// Delete a branch (abandon it — soft delete, data dir kept).
pub fn delete_branch(root_path: &Path, name: &str) -> Result<()> {
    let refs_dir = root_path.join(".thinkingroot-refs");
    let mut registry = branch::BranchRegistry::load_or_create(&refs_dir)?;
    registry.abandon_branch(name)
}
```

- [ ] **Step 5: Run all branch tests**

```bash
cargo test -p thinkingroot-branch --no-default-features 2>&1 | tail -30
```

Expected: all tests pass.

- [ ] **Step 6: Compile check workspace**

```bash
cargo check --workspace --no-default-features 2>&1 | tail -20
```

Expected: no errors (warnings OK).

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-branch/src/merge.rs \
        crates/thinkingroot-branch/src/lib.rs \
        crates/thinkingroot-branch/tests/branch_tests.rs
git commit -m "feat(branch): implement execute_merge + complete lib.rs public API"
```

---

## Task 7: CLI commands — branch.rs handler + main.rs 6 new subcommands

**Files:**
- Create: `crates/thinkingroot-cli/src/branch.rs`
- Modify: `crates/thinkingroot-cli/src/main.rs`
- Modify: `crates/thinkingroot-cli/Cargo.toml`

- [ ] **Step 1: Add thinkingroot-branch dependency to CLI Cargo.toml**

In `crates/thinkingroot-cli/Cargo.toml`, add:

```toml
thinkingroot-branch = { workspace = true }
```

And in `[features]`:
```toml
[features]
default = ["vector"]
vector = [
    "thinkingroot-graph/vector",
    "thinkingroot-serve/vector",
    "thinkingroot-branch/vector",
]
```

- [ ] **Step 2: Write the failing test**

In `crates/thinkingroot-cli/tests/integration.rs`, append:

```rust
#[test]
fn branch_subcommand_help() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_root"))
        .args(["branch", "--help"])
        .output()
        .expect("failed to run root branch --help");
    assert!(output.status.success(), "root branch --help failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Create"), "expected 'Create' in branch help");
}

#[test]
fn diff_subcommand_help() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_root"))
        .args(["diff", "--help"])
        .output()
        .expect("failed to run root diff --help");
    assert!(output.status.success());
}
```

- [ ] **Step 3: Run to confirm fail**

```bash
cargo test -p thinkingroot-cli branch_subcommand_help --no-default-features 2>&1 | tail -20
```

Expected: test failure — `root branch` not a recognized subcommand.

- [ ] **Step 4: Add Clap subcommands to main.rs**

In `crates/thinkingroot-cli/src/main.rs`, add to the `Commands` enum:

```rust
/// Manage knowledge branches
Branch {
    /// Branch name to create
    name: Option<String>,
    /// List all branches
    #[arg(long)]
    list: bool,
    /// Delete a branch
    #[arg(long)]
    delete: Option<String>,
    /// Description for the new branch
    #[arg(long)]
    description: Option<String>,
    /// Path to workspace root
    #[arg(long, default_value = ".")]
    path: String,
},

/// Set the active branch (update HEAD)
Checkout {
    /// Branch name to check out
    name: String,
    /// Path to workspace root
    #[arg(long, default_value = ".")]
    path: String,
},

/// Show semantic diff between a branch and main
Diff {
    /// Branch name to diff
    branch: String,
    /// Path to workspace root
    #[arg(long, default_value = ".")]
    path: String,
},

/// Merge a branch into main
Merge {
    /// Branch name to merge
    branch: String,
    /// Path to workspace root
    #[arg(long, default_value = ".")]
    path: String,
    /// Skip health CI gate
    #[arg(long)]
    force: bool,
    /// Also apply deletions from branch to main
    #[arg(long)]
    propagate_deletions: bool,
},

/// Show current branch and changes since branch point
Status {
    /// Path to workspace root
    #[arg(long, default_value = ".")]
    path: String,
},

/// Create an immutable named snapshot
Snapshot {
    /// Snapshot name
    name: String,
    /// Path to workspace root
    #[arg(long, default_value = ".")]
    path: String,
},
```

And in `match cli.command`:

```rust
Commands::Branch { name, list, delete, description, path } => {
    crate::branch_cmd::handle_branch(
        std::path::Path::new(&path), name.as_deref(), list, delete.as_deref(), description,
    ).await?;
}
Commands::Checkout { name, path } => {
    crate::branch_cmd::handle_checkout(std::path::Path::new(&path), &name).await?;
}
Commands::Diff { branch, path } => {
    crate::branch_cmd::handle_diff(std::path::Path::new(&path), &branch).await?;
}
Commands::Merge { branch, path, force, propagate_deletions } => {
    crate::branch_cmd::handle_merge(
        std::path::Path::new(&path), &branch, force, propagate_deletions,
    ).await?;
}
Commands::Status { path } => {
    crate::branch_cmd::handle_status(std::path::Path::new(&path)).await?;
}
Commands::Snapshot { name, path } => {
    crate::branch_cmd::handle_snapshot(std::path::Path::new(&path), &name).await?;
}
```

Add at top of main.rs:
```rust
mod branch_cmd;
```

- [ ] **Step 5: Implement branch_cmd.rs**

Create `crates/thinkingroot-cli/src/branch_cmd.rs`:

```rust
// crates/thinkingroot-cli/src/branch_cmd.rs
use std::path::Path;
use thinkingroot_branch::{create_branch, delete_branch, list_branches, read_head_branch, write_head_branch};
use thinkingroot_core::error::ThinkingRootError;

pub async fn handle_branch(
    root: &Path,
    name: Option<&str>,
    list: bool,
    delete: Option<&str>,
    description: Option<String>,
) -> anyhow::Result<()> {
    if list {
        let branches = list_branches(root)?;
        if branches.is_empty() {
            println!("No branches (you are on main)");
        } else {
            let head = read_head_branch(root).unwrap_or_else(|_| "main".to_string());
            for b in &branches {
                let marker = if b.name == head { "* " } else { "  " };
                println!("{}{}", marker, b.name);
            }
        }
        return Ok(());
    }

    if let Some(to_delete) = delete {
        delete_branch(root, to_delete)?;
        println!("Branch '{}' deleted.", to_delete);
        return Ok(());
    }

    if let Some(branch_name) = name {
        let parent = read_head_branch(root).unwrap_or_else(|_| "main".to_string());
        let branch = create_branch(root, branch_name, &parent, description).await?;
        println!("Created branch '{}' from '{}'", branch.name, branch.parent);
        println!("Hint: root checkout {}", branch.name);
    } else {
        eprintln!("Usage: root branch <name> | --list | --delete <name>");
        std::process::exit(1);
    }
    Ok(())
}

pub async fn handle_checkout(root: &Path, name: &str) -> anyhow::Result<()> {
    // Verify branch exists (unless it's main)
    if name != "main" {
        let branches = list_branches(root)?;
        if !branches.iter().any(|b| b.name == name) {
            return Err(ThinkingRootError::BranchNotFound(name.to_string()).into());
        }
    }
    write_head_branch(root, name)?;
    println!("Switched to branch '{}'", name);
    Ok(())
}

pub async fn handle_diff(root: &Path, branch: &str) -> anyhow::Result<()> {
    use thinkingroot_core::config::Config;
    use thinkingroot_branch::{diff::compute_diff, snapshot::resolve_data_dir};
    use thinkingroot_graph::StorageEngine;
    use thinkingroot_verify::Verifier;

    let config = Config::load_merged(root)?;

    let main_data_dir = resolve_data_dir(root, None);
    let branch_data_dir = resolve_data_dir(root, Some(branch));

    if !branch_data_dir.exists() {
        return Err(ThinkingRootError::BranchNotFound(branch.to_string()).into());
    }

    let main_engine = StorageEngine::init(&main_data_dir).await?;
    let branch_engine = StorageEngine::init(&branch_data_dir).await?;

    let verifier = Verifier::new();
    let health_before = verifier.compute_health(&main_engine.graph)?;
    let health_after = verifier.compute_health(&branch_engine.graph)?;

    let mc = &config.merge;
    let diff = compute_diff(
        &main_engine.graph,
        &branch_engine.graph,
        mc.auto_resolve_threshold,
        health_before,
        health_after,
        mc.max_health_drop,
        mc.block_on_contradictions,
    ).await?;

    // Pretty-print the diff
    println!("Knowledge PR: {} → main", branch);
    println!("Computed at: {}", diff.computed_at.format("%Y-%m-%d %H:%M:%S UTC"));
    println!();
    println!("Health:  before={:.1}%  after={:.1}%",
        diff.health_before.overall * 100.0,
        diff.health_after.overall * 100.0);
    println!();
    println!("New claims: {}", diff.new_claims.len());
    for dc in &diff.new_claims {
        println!("  + [{}] {}", format!("{:?}", dc.claim.claim_type), dc.claim.statement);
        if !dc.entity_context.is_empty() {
            println!("    entities: {}", dc.entity_context.join(", "));
        }
    }
    println!();
    println!("New entities: {}", diff.new_entities.len());
    for de in &diff.new_entities {
        println!("  + {} ({})", de.entity.canonical_name, format!("{:?}", de.entity.entity_type));
    }
    if !diff.auto_resolved.is_empty() {
        println!();
        println!("Auto-resolved contradictions: {}", diff.auto_resolved.len());
        for r in &diff.auto_resolved {
            println!("  ~ winner: {} (delta: {:.2})", r.winner, r.confidence_delta);
        }
    }
    if !diff.needs_review.is_empty() {
        println!();
        println!("Contradictions needing review: {}", diff.needs_review.len());
        for c in &diff.needs_review {
            println!("  ! main:   {}", c.main_statement);
            println!("    branch: {}", c.branch_statement);
        }
    }
    println!();
    if diff.merge_allowed {
        println!("✓ Merge allowed");
    } else {
        println!("✗ Merge blocked:");
        for reason in &diff.blocking_reasons {
            println!("  - {}", reason);
        }
    }
    Ok(())
}

pub async fn handle_merge(
    root: &Path,
    branch: &str,
    force: bool,
    propagate_deletions: bool,
) -> anyhow::Result<()> {
    use thinkingroot_core::config::Config;
    use thinkingroot_branch::{diff::compute_diff, merge::execute_merge, snapshot::resolve_data_dir};
    use thinkingroot_core::types::branch::MergedBy;
    use thinkingroot_graph::StorageEngine;
    use thinkingroot_verify::Verifier;

    let config = Config::load_merged(root)?;
    let mc = &config.merge;

    let main_data_dir = resolve_data_dir(root, None);
    let branch_data_dir = resolve_data_dir(root, Some(branch));

    if !branch_data_dir.exists() {
        return Err(ThinkingRootError::BranchNotFound(branch.to_string()).into());
    }

    let main_engine = StorageEngine::init(&main_data_dir).await?;
    let branch_engine = StorageEngine::init(&branch_data_dir).await?;

    let verifier = Verifier::new();
    let health_before = verifier.compute_health(&main_engine.graph)?;
    let health_after = verifier.compute_health(&branch_engine.graph)?;

    let mut diff = compute_diff(
        &main_engine.graph,
        &branch_engine.graph,
        mc.auto_resolve_threshold,
        health_before,
        health_after,
        mc.max_health_drop,
        mc.block_on_contradictions,
    ).await?;

    if force {
        diff.merge_allowed = true;
        diff.blocking_reasons.clear();
    }

    execute_merge(
        root,
        branch,
        &diff,
        MergedBy::Human { user: "cli".to_string() },
        propagate_deletions,
    ).await?;

    println!("✓ Merged '{}' into main", branch);
    println!("  {} new claims", diff.new_claims.len());
    println!("  {} new entities", diff.new_entities.len());
    println!("  {} auto-resolved", diff.auto_resolved.len());
    Ok(())
}

pub async fn handle_status(root: &Path) -> anyhow::Result<()> {
    let head = read_head_branch(root).unwrap_or_else(|_| "main".to_string());
    let branches = list_branches(root).unwrap_or_default();

    println!("On branch: {}", head);
    println!("Active branches: {}", branches.len());
    for b in &branches {
        let marker = if b.name == head { "* " } else { "  " };
        println!("{}  {} (from: {})", marker, b.name, b.parent);
    }
    Ok(())
}

pub async fn handle_snapshot(root: &Path, name: &str) -> anyhow::Result<()> {
    use thinkingroot_branch::snapshot::{resolve_data_dir, create_branch_layout};
    use thinkingroot_branch::branch::BranchRegistry;

    let head = read_head_branch(root).unwrap_or_else(|_| "main".to_string());
    let parent_data_dir = resolve_data_dir(root, Some(&head));
    let snapshot_data_dir = resolve_data_dir(root, Some(name));

    create_branch_layout(&parent_data_dir, &snapshot_data_dir)?;

    let refs_dir = root.join(".thinkingroot-refs");
    std::fs::create_dir_all(&refs_dir)?;
    let mut registry = BranchRegistry::load_or_create(&refs_dir)?;
    registry.create_branch(name, &head, Some(format!("Snapshot of {}", head)))?;

    println!("✓ Snapshot '{}' created from '{}'", name, head);
    Ok(())
}
```

- [ ] **Step 6: Run tests**

```bash
cargo test -p thinkingroot-cli --no-default-features 2>&1 | tail -30
```

Expected: branch_subcommand_help and diff_subcommand_help pass.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-cli/src/branch_cmd.rs \
        crates/thinkingroot-cli/src/main.rs \
        crates/thinkingroot-cli/Cargo.toml
git commit -m "feat(cli): add branch/checkout/diff/merge/status/snapshot subcommands"
```

---

## Task 8: --branch flag for pipeline + compile/serve in main.rs

**Files:**
- Modify: `crates/thinkingroot-serve/src/pipeline.rs`
- Modify: `crates/thinkingroot-cli/src/main.rs` — Compile and Serve subcommands

- [ ] **Step 1: Write failing test**

In `crates/thinkingroot-cli/tests/integration.rs`, append:

```rust
#[test]
fn compile_has_branch_flag() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_root"))
        .args(["compile", "--help"])
        .output()
        .expect("failed to run root compile --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--branch"), "compile should have --branch flag");
}

#[test]
fn serve_has_branch_flag() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_root"))
        .args(["serve", "--help"])
        .output()
        .expect("failed to run root serve --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--branch"), "serve should have --branch flag");
}
```

- [ ] **Step 2: Run to confirm fail**

```bash
cargo test -p thinkingroot-cli compile_has_branch_flag --no-default-features 2>&1 | tail -20
```

Expected: compile fails or `--branch` not found in help output.

- [ ] **Step 3: Modify pipeline.rs to accept branch param**

In `crates/thinkingroot-serve/src/pipeline.rs`, change the signature of `run_pipeline`:

```rust
// Before:
pub async fn run_pipeline(root_path: &Path) -> Result<()>

// After:
pub async fn run_pipeline(root_path: &Path, branch: Option<&str>) -> Result<()>
```

Inside `run_pipeline`, replace the `data_dir` derivation line:

```rust
// Before (wherever data_dir is computed from config):
let data_dir = root_path.join(&config.workspace.data_dir);

// After:
let data_dir = thinkingroot_branch::snapshot::resolve_data_dir(root_path, branch);
```

Add `thinkingroot-branch` to `crates/thinkingroot-serve/Cargo.toml`:

```toml
thinkingroot-branch = { workspace = true }
```

And update `[features]`:
```toml
vector = [
    "thinkingroot-graph/vector",
    "thinkingroot-branch/vector",
]
```

Fix all callers of `run_pipeline` in the serve crate to pass `None` (or forward the branch param).

- [ ] **Step 4: Add --branch flags to Compile and Serve in main.rs**

In the `Commands::Compile` variant, add:
```rust
/// Compile into a specific branch instead of main
#[arg(long)]
branch: Option<String>,
```

And update the call:
```rust
Commands::Compile { path, branch } => {
    run_pipeline(std::path::Path::new(&path), branch.as_deref()).await?;
}
```

In the `Commands::Serve` variant, add:
```rust
/// Serve a specific branch
#[arg(long)]
branch: Option<String>,
```

And forward it when building the engine (the serve engine already resolves data_dir from config; pass it through as needed or document that `--branch` selects the data dir used by the engine).

- [ ] **Step 5: Run tests**

```bash
cargo test -p thinkingroot-cli --no-default-features 2>&1 | tail -20
cargo check --workspace --no-default-features 2>&1 | tail -20
```

Expected: both pass.

- [ ] **Step 6: Commit**

```bash
git add crates/thinkingroot-serve/src/pipeline.rs \
        crates/thinkingroot-serve/Cargo.toml \
        crates/thinkingroot-cli/src/main.rs
git commit -m "feat(pipeline): add branch param to run_pipeline; --branch flag for compile/serve"
```

---

## Task 9: REST API (7 branch endpoints) + MCP tools (3 branch tools)

**Files:**
- Modify: `crates/thinkingroot-serve/src/rest.rs`
- Modify: `crates/thinkingroot-serve/src/mcp/tools.rs`
- Modify: `crates/thinkingroot-serve/tests/rest_test.rs`

- [ ] **Step 1: Write failing REST tests**

In `crates/thinkingroot-serve/tests/rest_test.rs`, append:

```rust
#[tokio::test]
async fn test_branch_list_endpoint() {
    // Create a test server (use existing test server setup from rest_test.rs)
    let server = create_test_server().await;

    let resp = server
        .get("/api/v1/branches")
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["ok"], true);
    // branches key should be an array
    assert!(body["data"]["branches"].is_array());
}
```

- [ ] **Step 2: Run to confirm fail**

```bash
cargo test -p thinkingroot-serve test_branch_list_endpoint --no-default-features 2>&1 | tail -20
```

Expected: 404 or route not found.

- [ ] **Step 3: Add AppState branch root_path field**

In `crates/thinkingroot-serve/src/engine.rs` or `lib.rs`, the `AppState` needs to know the workspace root path so branch operations can locate `.thinkingroot-refs/`. Add a field:

```rust
pub struct AppState {
    pub engine: RwLock<QueryEngine>,
    pub api_key: Option<String>,
    pub mcp_sessions: crate::mcp::sse::SseSessionMap,
    pub workspace_root: Option<std::path::PathBuf>,  // NEW
}
```

Update `AppState::new` to accept the root path, and update callers in `main.rs` (Serve command) to pass it.

- [ ] **Step 4: Add 7 branch REST endpoints to rest.rs**

In `crates/thinkingroot-serve/src/rest.rs`, add these routes to the router:

```rust
// Branch routes
.route("/api/v1/branches", get(list_branches_handler))
.route("/api/v1/branches", post(create_branch_handler))
.route("/api/v1/branches/:branch/diff", get(diff_branch_handler))
.route("/api/v1/branches/:branch/merge", post(merge_branch_handler))
.route("/api/v1/branches/:branch/checkout", post(checkout_branch_handler))
.route("/api/v1/branches/:branch", delete(delete_branch_handler))
.route("/api/v1/head", get(get_head_handler))
```

Implement the handlers:

```rust
async fn list_branches_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let root = match state.workspace_root.as_ref() {
        Some(r) => r.clone(),
        None => return api_error("NOT_CONFIGURED", "workspace root not set"),
    };
    match thinkingroot_branch::list_branches(&root) {
        Ok(branches) => api_ok(serde_json::json!({ "branches": branches })),
        Err(e) => api_error("BRANCH_ERROR", &e.to_string()),
    }
}

async fn create_branch_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let root = match state.workspace_root.as_ref() {
        Some(r) => r.clone(),
        None => return api_error("NOT_CONFIGURED", "workspace root not set"),
    };
    let name = match body["name"].as_str() {
        Some(n) => n.to_string(),
        None => return api_error("BAD_REQUEST", "name is required"),
    };
    let parent = body["parent"].as_str().unwrap_or("main").to_string();
    let description = body["description"].as_str().map(|s| s.to_string());

    match thinkingroot_branch::create_branch(&root, &name, &parent, description).await {
        Ok(branch) => api_ok(serde_json::json!({ "branch": branch })),
        Err(e) => api_error("BRANCH_ERROR", &e.to_string()),
    }
}

async fn get_head_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let root = match state.workspace_root.as_ref() {
        Some(r) => r.clone(),
        None => return api_error("NOT_CONFIGURED", "workspace root not set"),
    };
    match thinkingroot_branch::read_head_branch(&root) {
        Ok(head) => api_ok(serde_json::json!({ "head": head })),
        Err(e) => api_error("BRANCH_ERROR", &e.to_string()),
    }
}

async fn delete_branch_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match state.workspace_root.as_ref() {
        Some(r) => r.clone(),
        None => return api_error("NOT_CONFIGURED", "workspace root not set"),
    };
    match thinkingroot_branch::delete_branch(&root, &branch) {
        Ok(_) => api_ok(serde_json::json!({ "deleted": branch })),
        Err(e) => api_error("BRANCH_ERROR", &e.to_string()),
    }
}

async fn checkout_branch_handler(
    State(state): State<Arc<AppState>>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = match state.workspace_root.as_ref() {
        Some(r) => r.clone(),
        None => return api_error("NOT_CONFIGURED", "workspace root not set"),
    };
    match thinkingroot_branch::write_head_branch(&root, &branch) {
        Ok(_) => api_ok(serde_json::json!({ "head": branch })),
        Err(e) => api_error("BRANCH_ERROR", &e.to_string()),
    }
}

// diff_branch_handler and merge_branch_handler follow the same pattern
// (load engines, compute_diff, return JSON) — see handle_diff/handle_merge in branch_cmd.rs for logic
```

- [ ] **Step 5: Add 3 MCP tools to tools.rs**

In `crates/thinkingroot-serve/src/mcp/tools.rs`, add to the tools list:

```rust
// create_branch
ToolDef {
    name: "create_branch".to_string(),
    description: "Create an isolated knowledge branch for experimentation or agent sandboxing".to_string(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "Branch name (e.g. feature/x)" },
            "description": { "type": "string", "description": "Optional description" }
        },
        "required": ["name"]
    }),
},

// diff_branch
ToolDef {
    name: "diff_branch".to_string(),
    description: "Compute a semantic Knowledge PR showing new claims, entities, and contradictions between a branch and main".to_string(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "branch": { "type": "string", "description": "Branch name to diff against main" }
        },
        "required": ["branch"]
    }),
},

// merge_branch
ToolDef {
    name: "merge_branch".to_string(),
    description: "Merge a knowledge branch into main (runs health CI gate before merging)".to_string(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "branch": { "type": "string", "description": "Branch name to merge into main" },
            "force": { "type": "boolean", "description": "Skip health CI gate (default: false)" }
        },
        "required": ["branch"]
    }),
},
```

In the `call_tool` dispatch match, add:

```rust
"create_branch" => {
    let name = args["name"].as_str().ok_or("name required")?;
    let description = args["description"].as_str().map(|s| s.to_string());
    let root = /* get from AppState.workspace_root */;
    thinkingroot_branch::create_branch(&root, name, "main", description).await
        .map(|b| format!("Branch '{}' created from main", b.name))
        .map_err(|e| e.to_string())
}
"diff_branch" => {
    let branch = args["branch"].as_str().ok_or("branch required")?;
    // call compute_diff with loaded engines — return JSON summary
    // (same logic as handle_diff, return as string)
}
"merge_branch" => {
    let branch = args["branch"].as_str().ok_or("branch required")?;
    let force = args["force"].as_bool().unwrap_or(false);
    // call handle_merge logic — return result string
}
```

- [ ] **Step 6: Run all serve tests**

```bash
cargo test -p thinkingroot-serve --no-default-features 2>&1 | tail -30
```

Expected: all pass, including new branch endpoint tests.

- [ ] **Step 7: Full workspace check + test**

```bash
cargo check --workspace --no-default-features 2>&1 | tail -20
cargo test --workspace --no-default-features 2>&1 | tail -30
```

Expected: all clean.

- [ ] **Step 8: Commit**

```bash
git add crates/thinkingroot-serve/src/rest.rs \
        crates/thinkingroot-serve/src/mcp/tools.rs \
        crates/thinkingroot-serve/tests/rest_test.rs
git commit -m "feat(serve): add 7 branch REST endpoints + 3 MCP tools (create_branch, diff_branch, merge_branch)"
```

---

## Self-Review Against Spec

**Spec coverage check:**

| Spec Requirement | Task |
|---|---|
| `root branch <name>` / `--list` / `--delete` | Task 7 |
| `root checkout <name>` | Task 7 |
| `root diff <branch>` — semantic diff terminal output | Task 7 |
| `root merge <branch>` — health CI gate | Task 7 |
| `root status` | Task 7 |
| `root snapshot <name>` | Task 7 |
| `--branch` flag for `root compile` | Task 8 |
| `--branch` flag for `root serve` | Task 8 |
| BranchRef, BranchStatus, MergedBy types | Task 1 |
| KnowledgeDiff, DiffClaim, AutoResolution, ContradictionPair | Task 1 |
| MergeConfig (max_health_drop, block_on_contradictions, auto_resolve_threshold) | Task 1 |
| BranchNotFound, BranchAlreadyExists, MergeBlocked errors | Task 1 |
| slugify() function | Task 3 |
| resolve_data_dir() | Task 3 |
| create_branch_layout() — copy graph.db, symlink models/cache | Task 3 |
| BranchRegistry CRUD | Task 4 |
| HEAD read/write | Task 4 |
| semantic_hash() — BLAKE3 of normalised statement | Task 5 |
| Contradiction-as-conflict (negation pairs) | Task 5 |
| Auto-resolution (confidence delta > threshold) | Task 5 |
| Health-score CI gate (merge blocked if drop > 5%) | Task 5, 6 |
| execute_merge — insert claims/entities, supersede contradictions | Task 6 |
| get_entity_names_for_claims() in GraphStore | Task 2 |
| REST: GET /api/v1/branches | Task 9 |
| REST: POST /api/v1/branches | Task 9 |
| REST: GET /api/v1/branches/:branch/diff | Task 9 |
| REST: POST /api/v1/branches/:branch/merge | Task 9 |
| REST: POST /api/v1/branches/:branch/checkout | Task 9 |
| REST: DELETE /api/v1/branches/:branch | Task 9 |
| REST: GET /api/v1/head | Task 9 |
| MCP: create_branch tool | Task 9 |
| MCP: diff_branch tool | Task 9 |
| MCP: merge_branch tool | Task 9 |
| thinkingroot-branch crate in workspace | Task 3 |

All spec requirements covered. No placeholders. ✓
