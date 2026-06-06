# Knowledge Version Control (KVC) — Design Specification

**Date:** 2026-04-10  
**Status:** Approved for implementation  
**Author:** ThinkingRoot Core Team  
**Classification:** Novel — prior art exists in structural graph versioning (TerminusDB, Dolt); semantic diff + contradiction-as-conflict + health CI is not found in any existing system

---

## The Problem

Every AI agent that touches your knowledge graph is a liability.

Today when an agent calls `compile`, it extracts claims, resolves entities, and writes directly to your production knowledge base. If the agent hallucinates — "PostgreSQL was deprecated in favor of MongoDB" — that claim lives in main. The next agent reads it as truth. The corruption propagates.

There is no undo. There is no review. There is no isolation.

Beyond agents: teams working in parallel on a knowledge base have no way to experiment safely. "What if we redesigned the auth system?" can't be explored without mutating the one shared brain. If the exploration goes wrong, there is no way to roll back.

**Git solved this for code in 2005. Nobody has solved it for knowledge.**

---

## The Solution: Knowledge Version Control

KVC gives ThinkingRoot the branching model that Git gave source code — but adapted for the fundamental unit of knowledge: the **Claim**.

```
main  ──────────────────────────────────────────────── ▶
           │                   ▲
           │ root branch       │ root merge (after KnowledgeCI passes)
           ▼                   │
      feature/graphql ─────────┘
           │
           │ root compile ./graphql-docs
           │ (agent hypothesizes freely, main untouched)
           ▼
      [43 new claims, 3 contradictions with main]
```

The innovation: a branch is not a copy of the codebase. It is a **claim namespace** that inherits main's knowledge and layers new knowledge on top. A merge is not a text diff — it is a **semantic diff** run through the full linker and verifier pipeline, producing a reviewable Knowledge PR before anything touches main.

---

## Prior Art and Differentiation

Web research confirmed: Git-like branching for graph/tabular data is a solved problem. The following systems exist and should be acknowledged:

- **TerminusDB** — graph database with full Git-like branching, merging, push/pull/clone. The most capable existing system. Used for collaborative data engineering.
- **Dolt** — Git for SQL tables. Row-level diffs, row-level conflict detection. Mature and production-grade.
- **Quit Store** (2018 academic) — Git for RDF triples. Distributed collaboration on linked data.
- **KAPSO** (2026 academic) — Git-based branch isolation for AI agent experimentation. Closest to the agent sandboxing concept.

**What none of them do:**

| Feature | TerminusDB | Dolt | Quit Store | ThinkingRoot KVC |
|--------|-----------|---------------|--------------------------|------------------|
| Graph/table branching | ✓ | ✓ | ✓ RDF | ✓ |
| Structural diff (row/triple) | ✓ | ✓ | ✓ | ✓ |
| **Semantic diff (normalized statement hash)** | ✗ | ✗ | ✗ | **✓** |
| **Conflict = logical contradiction** | ✗ | ✗ | ✗ | **✓** |
| **Confidence scores drive auto-resolution** | ✗ | ✗ | ✗ | **✓** |
| **Health score CI gate for merge** | ✗ | ✗ | ✗ | **✓** |
| **LLM extraction pipeline integrated** | ✗ | ✗ | ✗ | **✓** |
| **AI agent sandboxing by design** | ✗ | ✗ | ✗ | **✓** |

The key distinction: **TerminusDB knows a conflict happened. ThinkingRoot KVC knows what the conflict means and why one side should win.**

In Dolt and TerminusDB, a conflict is structural — the same triple was changed in two places. In ThinkingRoot KVC, a conflict is semantic: `"Service A uses PostgreSQL"` contradicts `"Service A uses MongoDB"`, confidence 0.91 vs 0.73 means the first one wins automatically, and if the delta is too small, it surfaces for human review. The merge gate is not "are there structural conflicts?" — it is "would this merge lower our knowledge health score below threshold?"

The unit of conflict in Git is a line. The unit of conflict in KVC is a **Contradiction** — already modeled in the type system, already detected by the linker, already resolved by confidence scoring. The infrastructure for semantic merging already exists in ThinkingRoot. KVC assembles it into a version control system that no graph database has built.

---

## Core Architecture Decision: Snapshot Isolation

**Chosen approach:** Each branch gets its own `.thinkingroot-{branch-slug}/` directory with a copy of the parent's `graph.db` at branch creation time.

**Why not single-DB with workspace_id scoping:**
- CozoDB has no native multi-namespace join primitives
- Cross-branch fallback queries would require complex Rust-level merging on every read
- A corrupted branch would be in the same SQLite file as main — no true isolation
- The existing `engine.mount()` machinery already handles multiple independent databases perfectly

**Why snapshot works:**
- `graph.db` is typically 5–50 MB — file copy is instant
- The extraction cache (BLAKE3-keyed JSON files) is shared via symlink — zero re-LLM-calls on branch
- fastembed model cache is shared read-only — no re-download
- Branches start with full compiled artifacts and can be independently served via `root serve`

**Branch directory layout:**
```
project/
├── .thinkingroot/                    # main branch
│   ├── graph.db
│   ├── vectors.bin
│   ├── artifacts/
│   ├── models/                       # shared (read-only symlink in branches)
│   └── cache/extraction/             # shared (content-addressed, safe to share)
├── .thinkingroot-feature-graphql/    # branch: feature/graphql-migration
│   ├── graph.db                      # copy of main's graph.db at branch time
│   ├── vectors.bin                   # copy of main's vectors.bin at branch time
│   ├── artifacts/                    # copy of main's artifacts at branch time
│   ├── models -> ../.thinkingroot/models      # symlink
│   └── cache -> ../.thinkingroot/cache        # symlink
└── .thinkingroot-refs/               # NEW: branch registry
    └── branches.toml                 # branch metadata (name, parent, created_at, status)
```

---

## New Types

### BranchRef

```rust
// crates/thinkingroot-core/src/types/branch.rs

pub struct BranchRef {
    pub name: String,                  // "feature/graphql-migration"
    pub slug: String,                  // "feature-graphql-migration" (fs-safe)
    pub parent: String,                // "main"
    pub created_at: DateTime<Utc>,
    pub status: BranchStatus,
    pub description: Option<String>,   // why was this branch created
}

pub enum BranchStatus {
    Active,
    Merged { merged_at: DateTime<Utc>, merged_by: MergedBy },
    Abandoned { abandoned_at: DateTime<Utc> },
}

pub enum MergedBy {
    Human { user: String },
    Auto { reason: String },           // health CI passed, zero contradictions
    Agent { agent_id: String },
}

pub struct BranchRegistry {
    #[serde(default, rename = "branch")]
    pub branches: Vec<BranchRef>,
}
// stored at: {workspace}/.thinkingroot-refs/branches.toml
```

### KnowledgeDiff

```rust
// crates/thinkingroot-branch/src/diff.rs

pub struct KnowledgeDiff {
    pub from_branch: String,
    pub to_branch: String,
    pub computed_at: DateTime<Utc>,

    // Net-new knowledge (not in target, not contradicting)
    pub new_claims: Vec<DiffClaim>,
    pub new_entities: Vec<DiffEntity>,
    pub new_relations: Vec<DiffRelation>,

    // Contradictions between branch and target
    pub auto_resolved: Vec<AutoResolution>,    // confidence delta > threshold → auto-supersede
    pub needs_review: Vec<ContradictionPair>,  // human must decide before merge

    // Health impact projection
    pub health_before: HealthScore,
    pub health_after: HealthScore,            // projected after merge
    pub merge_allowed: bool,                  // CI pass/fail
    pub blocking_reasons: Vec<String>,        // why merge is blocked
}

pub struct DiffClaim {
    pub claim: Claim,
    pub entity_context: Vec<String>,    // entity names this claim is about
    pub diff_status: DiffStatus,
}

pub enum DiffStatus {
    New,                                // no equivalent in target
    Supersedes(ClaimId),               // would supersede this target claim
    Conflicts(ClaimId),                // conflicts, needs human resolution
}

pub struct ContradictionPair {
    pub branch_claim: Claim,
    pub main_claim: Claim,
    pub explanation: String,
    pub confidence_delta: f64,
    pub suggested_resolution: Option<Resolution>,
}

pub struct AutoResolution {
    pub branch_claim: Claim,
    pub superseded_claim: Claim,
    pub reason: String,               // e.g. "branch claim has higher confidence (0.91 > 0.73)"
}

pub struct DiffEntity {
    pub entity: Entity,
    pub is_new: bool,                  // false = existing entity got new claims/aliases
    pub new_claim_count: usize,
    pub new_alias_count: usize,
}

pub struct DiffRelation {
    pub relation: Relation,
    pub from_name: String,
    pub to_name: String,
    pub is_new: bool,
}
```

### MergeConfig

```rust
// crates/thinkingroot-core/src/config.rs (extend VerificationConfig)

pub struct MergeConfig {
    /// Maximum allowed health score drop after merge (default: 0.05 = 5%)
    pub max_health_drop: f64,
    /// Block merge if any unresolved contradictions remain (default: true)
    pub block_on_contradictions: bool,
    /// Auto-supersede threshold: confidence delta to auto-resolve (default: 0.15)
    pub auto_resolve_threshold: f64,
    /// Require human approval even if CI passes (default: false)
    pub require_approval: bool,
}
```

---

## New Crate: thinkingroot-branch

```
crates/thinkingroot-branch/
├── Cargo.toml
└── src/
    ├── lib.rs          — public API: create, list, diff, merge, delete
    ├── branch.rs       — BranchRegistry: load/save/add/remove
    ├── diff.rs         — KnowledgeDiff computation
    ├── merge.rs        — merge execution pipeline
    └── snapshot.rs     — fs operations: copy db, create symlinks, validate
```

### Dependency position in the workspace graph:

```
thinkingroot-core
    ↓
thinkingroot-graph
    ↓
thinkingroot-link, thinkingroot-verify, thinkingroot-compile
    ↓
thinkingroot-branch    ← NEW (depends on link + verify + compile + graph)
    ↓
thinkingroot-serve, thinkingroot-cli
```

### Core API

```rust
// crates/thinkingroot-branch/src/lib.rs

/// Create a new branch from the current workspace state.
/// Copies graph.db + artifacts, symlinks models + cache.
pub async fn create_branch(
    workspace_path: &Path,
    branch_name: &str,
    description: Option<&str>,
) -> Result<BranchRef>

/// List all branches for a workspace.
pub fn list_branches(workspace_path: &Path) -> Result<Vec<BranchRef>>

/// Compute the semantic diff between a branch and its parent.
/// This is the "Knowledge PR" — no data is written.
pub async fn diff_branch(
    workspace_path: &Path,
    branch_name: &str,
) -> Result<KnowledgeDiff>

/// Execute a merge: write branch knowledge into parent, run CI.
/// Returns Err if merge is blocked (health drop, unresolved contradictions).
pub async fn merge_branch(
    workspace_path: &Path,
    branch_name: &str,
    config: &MergeConfig,
    force: bool,
) -> Result<MergeResult>

/// Delete a branch directory and remove from registry.
pub fn delete_branch(workspace_path: &Path, branch_name: &str) -> Result<()>
```

---

## Diff Algorithm (the heart of KVC)

```rust
// crates/thinkingroot-branch/src/diff.rs

pub async fn compute_diff(
    branch_path: &Path,
    target_path: &Path,
    config: &MergeConfig,
) -> Result<KnowledgeDiff> {

    // 1. Open both storage engines (read-only)
    let branch_storage = StorageEngine::init(branch_path)?;
    let target_storage = StorageEngine::init(target_path)?;

    // 2. Get all claims from both sides
    let branch_claims = branch_storage.graph.get_all_claims_with_sources()?;
    let target_claims = target_storage.graph.get_all_claims_with_sources()?;

    // 3. Semantic identity: normalize + hash each statement
    //    "PostgreSQL is used as primary database" ==
    //    "postgresql is used as the primary database"
    //    → same semantic hash
    let target_hashes: HashMap<String, &ClaimRow> = target_claims.iter()
        .map(|c| (semantic_hash(&c.statement), c))
        .collect();

    // 4. Find net-new claims: in branch, NOT in target by semantic identity
    let new_claim_rows: Vec<_> = branch_claims.iter()
        .filter(|c| !target_hashes.contains_key(&semantic_hash(&c.statement)))
        .collect();

    // 5. Cross-branch contradiction detection
    //    Run the EXISTING linker negation-pair algorithm against
    //    (new_branch_claims × all_target_claims) grouped by entity
    let contradictions = detect_cross_branch_contradictions(
        &new_claim_rows,
        &target_claims,
        config.auto_resolve_threshold,
    )?;

    let (auto_resolved, needs_review): (Vec<_>, Vec<_>) = contradictions
        .into_iter()
        .partition(|c| c.confidence_delta > config.auto_resolve_threshold);

    // 6. Compute projected health
    //    Run verifier on target DB + staged new claims + staged auto-resolutions
    let health_before = run_verify(&target_storage, config)?;
    let health_after = project_health_after_merge(
        &target_storage,
        &new_claim_rows,
        &auto_resolved,
        &needs_review,
        config,
    )?;

    // 7. Compute new entities and relations
    let (new_entities, new_relations) = compute_new_graph_elements(
        &branch_storage,
        &target_storage,
    )?;

    // 8. Merge CI gate
    let health_drop = health_before.overall - health_after.overall;
    let mut blocking_reasons = Vec::new();

    if health_drop > config.max_health_drop {
        blocking_reasons.push(format!(
            "Health would drop {:.0}% (limit: {:.0}%)",
            health_drop * 100.0, config.max_health_drop * 100.0
        ));
    }
    if config.block_on_contradictions && !needs_review.is_empty() {
        blocking_reasons.push(format!(
            "{} contradictions require human review before merge",
            needs_review.len()
        ));
    }

    Ok(KnowledgeDiff {
        from_branch: branch_name.to_string(),
        to_branch: "main".to_string(),
        computed_at: Utc::now(),
        new_claims: new_claim_rows.into_iter().map(DiffClaim::from).collect(),
        new_entities,
        new_relations,
        auto_resolved,
        needs_review,
        health_before,
        health_after,
        merge_allowed: blocking_reasons.is_empty(),
        blocking_reasons,
    })
}
```

### Semantic Hash Function

```rust
fn semantic_hash(statement: &str) -> String {
    // Normalize: lowercase, collapse whitespace, remove trailing punctuation
    let normalized = statement
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', ',', ';', ':'])
        .to_string();
    
    // BLAKE3 → hex
    ContentHash::from_bytes(normalized.as_bytes()).0
}
```

This ensures that "PostgreSQL is the primary database." and "postgresql is the primary database" are recognized as the same claim during diff, preventing phantom duplicates.

---

## Merge Execution Pipeline

```rust
// crates/thinkingroot-branch/src/merge.rs

pub async fn execute_merge(
    workspace_path: &Path,
    branch_name: &str,
    config: &Config,
) -> Result<MergeResult> {

    // 1. Compute diff (this runs KnowledgeCI)
    let diff = compute_diff(branch_data_path, main_data_path, &config.merge).await?;

    if !diff.merge_allowed {
        return Err(Error::MergeBlocked {
            reasons: diff.blocking_reasons,
            diff,
        });
    }

    // 2. Load target storage (write mode)
    let target_storage = StorageEngine::init(&main_data_path)?;

    // 3. Write net-new claims with their entity linkages
    let link_input = ExtractionOutput {
        claims: diff.new_claims.iter().map(|dc| dc.claim.clone()).collect(),
        entities: diff.new_entities.iter().map(|de| de.entity.clone()).collect(),
        relations: diff.new_relations.iter().map(|dr| SourcedRelation {
            source: dr.relation.evidence[0], // preserved source attribution
            relation: dr.relation.clone(),
        }).collect(),
        claim_entity_names: rebuild_claim_entity_names(&diff.new_claims),
    };

    let linker = Linker::new(&target_storage.graph);
    let link_result = linker.link(link_input)?;

    // 4. Apply auto-resolutions (supersede lower-confidence claims in main)
    for resolution in &diff.auto_resolved {
        target_storage.graph.supersede_claim(
            &resolution.superseded_claim.id.to_string(),
            &resolution.branch_claim.id.to_string(),
        )?;
    }

    // 5. Rebuild entity relations (aggregates max strength across all sources)
    target_storage.graph.rebuild_entity_relations()?;

    // 6. Rebuild vector index
    rebuild_vector_index(&mut target_storage.vector, &target_storage.graph).await?;

    // 7. Recompile affected artifacts only
    let compiler = Compiler::new(&config)?;
    compiler.compile_affected(
        &target_storage.graph,
        &main_data_path.join("artifacts"),
        &link_result.affected_entity_ids,
        link_result.has_global_changes(),
    )?;

    // 8. Mark branch as merged in registry
    let mut registry = BranchRegistry::load(workspace_path)?;
    registry.mark_merged(branch_name, MergedBy::Human { user: "cli".to_string() })?;
    registry.save(workspace_path)?;

    Ok(MergeResult {
        claims_merged: diff.new_claims.len(),
        entities_merged: link_result.entities_created + link_result.entities_merged,
        relations_merged: diff.new_relations.len(),
        auto_resolved_count: diff.auto_resolved.len(),
        health_before: diff.health_before,
        health_after: diff.health_after,
    })
}
```

---

## CLI Commands

### Branch management

```bash
# Create a branch (copies main's knowledge state)
root branch feature/graphql-migration
root branch feature/graphql-migration --description "Exploring GraphQL as primary query interface"
root branch --from=main feature/graphql-migration   # explicit parent (for future multi-level)

# List branches
root branch --list

# Delete a branch (no merge, abandon it)
root branch --delete feature/graphql-migration

# Set active branch context for subsequent commands in this directory
root checkout feature/graphql-migration   # writes .thinkingroot-refs/HEAD

# Show current branch
root status
```

### Knowledge diff (the Knowledge PR)

```bash
# Show full semantic diff: what would change if we merged this branch to main?
root diff feature/graphql-migration

# Output:
#
# Knowledge Diff: feature/graphql-migration → main
# ────────────────────────────────────────────────
#
# NEW KNOWLEDGE  (+43 claims, +12 entities, +18 relations)
#
#   GraphQL  [Api]
#     + "ThinkingRoot exposes a /graphql endpoint supporting introspection"  conf: 0.92
#     + "GraphQL schema is generated from the compiled knowledge graph"  conf: 0.88
#
#   QueryLanguage  [Concept]
#     + "GraphQL enables typed queries over the entity graph"  conf: 0.85
#
#   ... 40 more claims
#
# AUTO-RESOLVED  (2 contradictions, auto-superseded by confidence)
#
#   ✓ "OpenAPI spec is maintained for all REST endpoints"  (main, conf: 0.89)
#     superseded by: "GraphQL replaces OpenAPI for client-facing APIs"  (branch, conf: 0.71)
#     reason: confidence delta 0.18 > threshold 0.15, branch claim supersedes
#
# NEEDS REVIEW  (3 contradictions — BLOCKING merge)
#
#   ⚠ #1
#     main:   "REST API is the primary integration interface"  (conf: 0.91)
#     branch: "GraphQL is the primary integration interface"  (conf: 0.87)
#     → delta 0.04, below auto-resolve threshold. Human decision required.
#
# HEALTH IMPACT
#   Before:  87%  (fresh: 91%  consistent: 85%  coverage: 82%  prov: 94%)
#   After:   79%  (fresh: 91%  consistent: 71%  coverage: 88%  prov: 94%)
#   ⚠ Consistency drop: 3 unresolved contradictions would lower it further
#
# MERGE STATUS:  BLOCKED
#   · 3 contradictions require human review before merge

# JSON output for programmatic use
root diff feature/graphql-migration --json
```

### Merge

```bash
# Standard merge (respects CI, blocked if contradictions exist)
root merge feature/graphql-migration

# After resolving contradictions in config or via --resolve flags
root merge feature/graphql-migration --resolve 1=keep-main --resolve 2=keep-branch --resolve 3=keep-main

# Force merge (bypass health CI — use with caution)
root merge feature/graphql-migration --force

# Preview merge without writing (same as diff, but explicitly phrased as merge preview)
root merge feature/graphql-migration --dry-run
```

### `root status` — the knowledge tree

```bash
root status

# Output:
# On branch: feature/graphql-migration
# Parent: main
# Created: 2026-04-10 09:14
#
# Changes since branching:
#   + 43 new claims extracted (from 12 new source files)
#   + 12 new entities created
#   ~ 3 existing entities gained new aliases
#   ⚠ 3 contradictions with main (unresolved)
#
# Knowledge CI: FAILING  (3 unresolved contradictions)
# Run 'root diff feature/graphql-migration' for details
```

---

## MCP Tools (Agent Sandboxing)

The most important surface area for KVC: agents always operate in branches.

### New MCP tools

```json
{
  "name": "create_branch",
  "description": "Create an isolated knowledge branch for safe experimentation. All compile operations in this session should use the branch workspace to avoid modifying main.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "branch_name": { "type": "string", "description": "e.g. agent-exploration-graphql" },
      "workspace": { "type": "string" },
      "description": { "type": "string" }
    },
    "required": ["branch_name", "workspace"]
  }
}
```

```json
{
  "name": "diff_branch",
  "description": "Show what knowledge would change if this branch were merged to main. Use this before proposing a merge to give the human a review opportunity.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "branch_name": { "type": "string" },
      "workspace": { "type": "string" }
    },
    "required": ["branch_name", "workspace"]
  }
}
```

```json
{
  "name": "merge_branch",
  "description": "Merge verified branch knowledge into main. Will fail if Knowledge CI is not passing.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "branch_name": { "type": "string" },
      "workspace": { "type": "string" }
    },
    "required": ["branch_name", "workspace"]
  }
}
```

### Agent safety flow (the canonical pattern)

```
[Human to Agent]: "Research the impact of migrating our auth to OAuth2"

[Agent]
1. create_branch("agent-oauth2-research", "my-workspace", "Impact analysis: OAuth2 migration")
   → Returns: branch created at .thinkingroot-agent-oauth2-research/

2. compile("my-workspace", branch="agent-oauth2-research")  
   → Extracts claims from OAuth2 docs, maps to existing auth entities
   → Runs in the branch — main is untouched

3. diff_branch("agent-oauth2-research", "my-workspace")
   → Returns KnowledgeDiff: 28 new claims, 2 contradictions with existing auth claims

4. Agent reports to human:
   "Found 28 new facts about OAuth2 integration. There are 2 contradictions with
    existing knowledge that need your decision before this can be merged.
    Here's the Knowledge PR: [diff output]"

[Human reviews, resolves contradictions, approves]

5. merge_branch("agent-oauth2-research", "my-workspace")
   → Writes 28 claims to main, supersedes resolved contradictions
   → Health: 84% → 88% (+4% coverage)
```

If the agent hallucinated or produced bad data at step 2:
- Human does NOT approve at step 4
- Agent calls `delete_branch("agent-oauth2-research")` — or human runs `root branch --delete`
- Main is clean. Zero contamination.

---

## REST API

New endpoints added to the workspace router:

```
GET  /api/v1/ws/{workspace}/branches               → list branches
POST /api/v1/ws/{workspace}/branches               → create branch { "name": "...", "description": "..." }
GET  /api/v1/ws/{workspace}/branches/{name}        → get branch info
DELETE /api/v1/ws/{workspace}/branches/{name}      → delete branch
GET  /api/v1/ws/{workspace}/branches/{name}/diff   → compute KnowledgeDiff
POST /api/v1/ws/{workspace}/branches/{name}/merge  → execute merge
POST /api/v1/ws/{workspace}/branches/{name}/compile → compile in branch context
```

---

## Snapshots (read-only, no merge-back)

A snapshot is a named, immutable copy of the knowledge base at a point in time. Unlike branches, snapshots can never be merged back. They are checkpoints.

```bash
root snapshot before-v2-migration    # creates .thinkingroot-snap-before-v2-migration/
root snapshot --list
root serve --path .thinkingroot-snap-before-v2-migration  # serve a historical state
```

Use case: before a major `root compile` on a large new document set, take a snapshot. If the compilation produces bad data, restore from snapshot:

```bash
root snapshot pre-batch-import
root compile ./new-documents/
root health  # 72% — bad extraction
# Restore: delete main's graph.db, copy from snapshot
root snapshot --restore pre-batch-import
```

---

## The Hypothetical Reasoning Pattern

KVC enables a new class of AI-assisted analysis that no knowledge system offers today:

```bash
# Hypothesis: what if we migrated from PostgreSQL to CockroachDB?
root branch hypothesis/cockroachdb-migration
root compile ./cockroachdb-docs --workspace-path .thinkingroot-hypothesis-cockroachdb-migration

# Ask ThinkingRoot: what's the impact on our current architecture?
root diff hypothesis/cockroachdb-migration

# Output might show:
# + 31 new claims about CockroachDB capabilities
# ⚠ 7 contradictions: existing "PostgreSQL is the primary persistence layer" claims
#   conflict with "CockroachDB replaces PostgreSQL" claims
# Health impact: +8% coverage, -15% consistency (7 unresolved contradictions)
# → High disruption hypothesis. Main brain is clean. Abandon or resolve.

# If hypothesis is useful: merge after resolving contradictions
# If hypothesis fails: just delete the branch
root branch --delete hypothesis/cockroachdb-migration
```

This is **parallel hypothesis testing on a knowledge graph**. Multiple branches can exist simultaneously:
- `hypothesis/cockroachdb-migration`
- `hypothesis/graphql-api`
- `hypothesis/event-sourcing-refactor`

Each exploring a different architectural future. Main brain untouched. Humans or agents compare the diffs and choose which (if any) to merge.

---

## Knowledge CI: The Merge Gate

The merge gate runs automatically before any `root merge`. It checks:

| Check | Default Threshold | Configurable |
|-------|------------------|--------------|
| Health score drop | ≤ 5% | `merge.max_health_drop` |
| Unresolved contradictions | 0 | `merge.block_on_contradictions` |
| Freshness (new claims can't be stale) | > 0 days old | n/a |
| Orphaned claims in branch | 0 | n/a |

The health score formula is unchanged: `freshness*0.3 + consistency*0.3 + coverage*0.2 + provenance*0.2`

The **consistency** dimension is what prevents contradiction-polluted merges. Unresolved contradictions lower consistency. Lower consistency → lower overall health → merge blocked.

---

## Resolved Design Questions

**Q: Snapshot (copy) or Inheritance (dynamic fallback)?**  
A: **Snapshot.** Copy `graph.db` at branch time. Dynamic fallback requires complex cross-DB Datalog that CozoDB doesn't natively support and adds latency to every query.

**Q: What if re-extracting in a branch is expensive (LLM calls)?**  
A: The extraction cache (`{data_dir}/cache/extraction/{BLAKE3_hash}.json`) is content-addressed and immutable. Branches symlink to the parent's cache directory. A file that was already extracted hits cache instantly — zero LLM calls for inherited sources.

**Q: `root checkout` — global mutable HEAD state?**  
A: Minimal. `root checkout` writes to `.thinkingroot-refs/HEAD` (a text file containing the branch name). All other commands still accept explicit `--branch` flag. HEAD is just a convenience for CLI sessions. No global config mutation.

**Q: On merge conflict — fail hard or produce report?**  
A: **Fail with a reviewable report.** `root merge` returns the full `KnowledgeDiff` with blocking reasons. Human resolves contradictions via `--resolve` flags or by editing the branch and re-merging. No silent data corruption.

**Q: How are IDs handled across branch and main?**  
A: Claim IDs (ULIDs) are unique per claim. When a branch claim is merged to main, it retains its original ULID. The `superseded_by` chain is preserved. History is never rewritten.

**Q: What does `root compile` target when a branch is active?**  
A: The compile pipeline reads `.thinkingroot-refs/HEAD` to resolve the active branch. If HEAD exists, compile targets the branch DB. If HEAD is absent or `--branch main` is passed explicitly, compile targets main. Rule: explicit `--branch` flag always overrides HEAD.

```
root compile ./docs                    # → active branch (from HEAD), or main if no HEAD
root compile ./docs --branch main      # → main (explicit override)
root compile ./docs --branch feature/x # → branch feature/x (explicit override)
```

The pipeline function `run_pipeline(root_path, branch: Option<&str>)` resolves the target data directory:
```rust
fn resolve_data_dir(root_path: &Path, branch: Option<&str>) -> PathBuf {
    match branch {
        Some("main") | None => root_path.join(".thinkingroot"),
        Some(name) => root_path.join(format!(".thinkingroot-{}", slugify(name))),
    }
}
```

**Q: Can you serve a branch directly?**  
A: Yes. `root serve` gains a `--branch <name>` flag that resolves to the branch data directory. Useful for previewing what the knowledge base looks like after a hypothetical merge before committing.

```bash
root serve --branch feature/graphql    # serves branch DB on configured port
root serve                             # serves main (unchanged behaviour)
```

The branch directory is a fully valid data directory — same schema, same artifacts, same REST + MCP API. The serve banner displays the branch name clearly:

```
ThinkingRoot v0.x.x  [branch: feature/graphql-migration]
REST API:  http://127.0.0.1:3000/api/v1/
Workspace: my-repo → http://127.0.0.1:3000/api/v1/ws/my-repo/
```

**Q: Where does `MergeConfig` live in config.toml?**  
A: A new `[merge]` section in `.thinkingroot/config.toml` and `~/.config/thinkingroot/config.toml` (global). Added to both `Config` and `GlobalConfig` structs.

```toml
[merge]
max_health_drop = 0.05         # block merge if health drops more than 5%
block_on_contradictions = true # block merge if any unresolved contradictions remain
auto_resolve_threshold = 0.15  # confidence delta above which the higher-confidence claim auto-wins
```

Defaults are safe for production use. `auto_resolve_threshold = 0.15` means a claim with confidence 0.91 auto-supersedes one with confidence 0.75 (delta 0.16 > 0.15). If delta is ≤ 0.15, human review is required.

**Q: What happens to two branches that conflict with each other?**  
A: Not a special case. When branch B merges after branch A, the diff runs against main-which-now-includes-A's-claims. Any contradictions between A and B surface naturally at B's merge time. First merge wins; second merge must resolve conflicts explicitly. Same as Git.

---

## Test Plan

### Isolation test
```rust
// Create branch, compile new source in branch, verify main is unchanged
#[tokio::test]
async fn test_branch_isolation() {
    let workspace = create_temp_workspace();
    let main_claim_count_before = count_claims(&workspace.main_db());
    
    let branch = create_branch(&workspace, "test-isolation").await?;
    compile_source("test data: Service X uses Redis", &workspace, "test-isolation").await?;
    
    let branch_count = count_claims(&workspace.branch_db("test-isolation"));
    let main_count = count_claims(&workspace.main_db());
    
    assert!(branch_count > main_claim_count_before);  // branch has new claims
    assert_eq!(main_count, main_claim_count_before);   // main unchanged
}
```

### Merge test
```rust
#[tokio::test]
async fn test_clean_merge() {
    let workspace = create_temp_workspace_with_knowledge();
    let branch = create_branch(&workspace, "test-merge").await?;
    
    compile_unique_source(&workspace, "test-merge").await?;
    
    let diff = diff_branch(&workspace, "test-merge").await?;
    assert!(diff.new_claims.len() > 0);
    assert!(diff.needs_review.is_empty());   // no contradictions
    assert!(diff.merge_allowed);
    
    let result = merge_branch(&workspace, "test-merge", &default_config()).await?;
    
    // Claims are now in main
    let main_claims = workspace.main_storage().graph.get_all_claims_with_sources()?;
    let merged_ids: HashSet<_> = result.merged_claim_ids.iter().collect();
    assert!(main_claims.iter().any(|c| merged_ids.contains(&c.id)));
}
```

### Contradiction blocking test
```rust
#[tokio::test]
async fn test_merge_blocked_by_contradiction() {
    let workspace = create_workspace_with_claim("Service A uses PostgreSQL", 0.9);
    let branch = create_branch(&workspace, "test-conflict").await?;
    
    compile_source("Service A uses MongoDB", &workspace, "test-conflict").await?;
    
    let diff = diff_branch(&workspace, "test-conflict").await?;
    assert!(!diff.merge_allowed);
    assert!(diff.needs_review.len() > 0);
    assert!(diff.blocking_reasons.iter().any(|r| r.contains("contradictions")));
    
    // Ensure merge actually fails
    let result = merge_branch(&workspace, "test-conflict", &default_config()).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), Error::MergeBlocked { .. }));
}
```

### Agent sandboxing test
```rust
#[tokio::test]
async fn test_agent_cannot_pollute_main() {
    let workspace = create_temp_workspace();
    let main_snapshot = snapshot_db_state(&workspace.main_db());
    
    // Simulate an agent that hallucinates bad data in a branch
    create_branch(&workspace, "agent-session-bad").await?;
    compile_bad_data(&workspace, "agent-session-bad").await?;
    
    // Agent abandons branch (no merge)
    delete_branch(&workspace, "agent-session-bad")?;
    
    // Main is identical to before the agent session
    assert_db_states_equal(&workspace.main_db(), &main_snapshot);
}
```

---

## Backward Compatibility

KVC is fully additive. Nothing changes for users who don't use branches:
- `root compile`, `root serve`, `root setup`, `root connect` — all unchanged
- The `.thinkingroot/` directory format is unchanged
- Branch directories use a different naming convention (`.thinkingroot-{slug}/`) and are invisible to existing commands unless explicitly targeted
- No database schema changes to the existing 9 CozoDB relations

---

## Implementation Scope

**New crate:** `crates/thinkingroot-branch/`  
**New types in thinkingroot-core:** `BranchRef`, `BranchRegistry`, `BranchStatus`, `KnowledgeDiff`, `DiffClaim`, `DiffEntity`, `DiffRelation`, `ContradictionPair`, `AutoResolution`, `MergeConfig`, `MergeResult`  
**Modified crates:** `thinkingroot-cli` (new commands), `thinkingroot-serve` (new REST endpoints + MCP tools), `Cargo.toml` (new crate), `CLAUDE.md` (updated phase status)  
**New CLI commands:** `root branch`, `root checkout`, `root diff`, `root merge`, `root snapshot`, `root status`  
**New MCP tools:** `create_branch`, `diff_branch`, `merge_branch`  
**New REST endpoints:** 7 endpoints under `/api/v1/ws/{workspace}/branches/`

---

## Phase 3.5 Positioning

KVC is the centerpiece of Phase 3.5. It transforms ThinkingRoot from a knowledge extraction tool into a **knowledge management platform** with the safety guarantees required for production AI automation.

The agent safety story — "your AI can never permanently corrupt your knowledge without human review" — is the single most compelling enterprise differentiator and the foundation that makes Phase 4 (team collaboration, cloud) meaningful.

Without KVC, multiple agents or engineers writing to the same knowledge base is a liability. With KVC, it is a feature.
