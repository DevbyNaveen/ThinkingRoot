# ThinkingRoot Knowledge Hub — World-First Architecture

**Date:** 2026-04-13  
**Status:** Architecture Specification  
**Author:** Naveen  
**Classification:** World-first — no existing system does this

---

## What This Document Is

This is the complete architecture for a **global knowledge distribution protocol** — the first system that treats compiled, verified, source-cited knowledge as a distributable, versionable, forkable, composable package that AI agents can directly consume.

Every design decision below is grounded in precedent from systems that have proven to work at scale. Nothing is speculative. Where a design choice has no precedent, it is explicitly marked.

---

## Architectural Precedents Used (Not Invented — Borrowed)

| Precedent System | What It Proved | What We Take |
|---|---|---|
| **Git** | Content-addressable storage, Merkle DAGs, branching/forking at scale, distributed-first | Content-addressable knowledge objects, forking, branching |
| **npm / crates.io** | Sparse index protocol, versioned packages, dependency resolution, 3.1M packages at scale | Package format, versioning scheme, sparse index for discovery |
| **Docker/OCI** | Layered content-addressable artifacts, manifest + blob separation, multi-registry distribution | Manifest/layer architecture for knowledge packages |
| **Hugging Face Hub** | Model cards, community engagement, public+private repos, $130M ARR from hosting ML artifacts | Hub UX patterns, model card → knowledge card, community signals |
| **MCP (Model Context Protocol)** | 20,000+ servers indexed, standardized agent-tool interface, Linux Foundation governance | Native MCP serving of published knowledge, agent discoverability |
| **CozoDB / Datalog** | ThinkingRoot's existing graph engine, proven for knowledge storage | Direct reuse — published graphs use the same engine |

---

## Core Concept: The Knowledge Package (KnowledgePack)

A KnowledgePack is the atomic unit of distribution. It is what gets published, discovered, connected, forked, and served.

### What Is Inside a KnowledgePack

```
naveen/transformer-survey@1.2.0
├── manifest.json          # Package metadata + integrity hashes
├── graph.db               # CozoDB database (claims, entities, relations)
├── vectors.bin            # fastembed vector index (optional, can regenerate)
├── artifacts/             # Compiled markdown artifacts
│   ├── entity/            # Entity pages
│   ├── architecture.md    # Architecture map
│   ├── contradictions.md  # Contradiction report
│   ├── health.md          # Health report
│   └── ...
├── knowledge.card.md      # Human-readable summary (like HuggingFace model card)
└── provenance.json        # Full source-URI provenance chain (no raw content)
```

### What Is NOT Inside

- ❌ Raw source files (`.rs`, `.md`, `.py`) — never, by design
- ❌ LLM API keys or credentials
- ❌ User-specific configuration
- ❌ The extraction cache (per-machine optimization)

### Why This Structure

The separation mirrors OCI's manifest+blob pattern, proven at Docker Hub scale (billions of pulls/month):
- `manifest.json` is small, cacheable, content-addressed — registries index only this
- `graph.db` is the heavy payload — pulled only when connecting
- `artifacts/` are individually addressable — agents can pull specific entity pages without downloading the full graph
- `vectors.bin` is optional — can be regenerated locally from `graph.db` using fastembed

---

## The Manifest Format

```json
{
  "schema_version": "1.0.0",
  "name": "naveen/transformer-survey",
  "version": "1.2.0",
  "description": "Compiled knowledge from 200 transformer architecture papers (2020-2026)",
  "license": "CC-BY-4.0",
  
  "stats": {
    "claims": 47832,
    "entities": 2341,
    "relations": 8903,
    "contradictions": 127,
    "sources": 203,
    "artifacts": 2341
  },
  
  "health": {
    "overall": 0.87,
    "freshness": 0.82,
    "consistency": 0.91,
    "coverage": 0.85,
    "provenance": 0.90
  },
  
  "compiled": {
    "compiler_version": "0.9.0",
    "compiled_at": "2026-04-13T17:00:00Z",
    "pipeline_version": "v1",
    "extraction_model": "amazon.nova-micro-v1:0"
  },
  
  "content_hash": {
    "algorithm": "blake3",
    "manifest": "a1b2c3d4...",
    "graph": "e5f6g7h8...",
    "vectors": "i9j0k1l2...",
    "artifacts": "m3n4o5p6..."
  },
  
  "dependencies": [],
  
  "publisher": {
    "name": "naveen",
    "verified": true,
    "published_at": "2026-04-13T17:05:00Z"
  },
  
  "tags": ["ai", "transformers", "nlp", "research"],
  "domain": "research/machine-learning",
  
  "source_types": ["Document", "Code"],
  "entity_types": {
    "System": 45,
    "Concept": 892,
    "Person": 234,
    "Library": 156
  },
  
  "claim_types": {
    "Fact": 31204,
    "Decision": 4521,
    "Architecture": 3102,
    "ApiSignature": 8905
  }
}
```

### Why BLAKE3 Content Hashing (Not SHA-256)

ThinkingRoot already uses BLAKE3 throughout the pipeline (extraction cache, fingerprint store, content-hash skip). Using the same algorithm for package integrity means:
- Zero new dependencies
- Consistency with local compilation hashes
- BLAKE3 is faster than SHA-256 (hardware-parallelized) — matters for large graphs

---

## The Five Operations

Every registry needs exactly five operations. No more, no fewer.

### 1. PUBLISH — Push a compiled graph to the hub

```bash
root publish
root publish --name naveen/transformer-survey
root publish --version 1.2.0
root publish --visibility public|private|org
root publish --license CC-BY-4.0
root publish --tag ai,transformers
```

**What happens internally:**
```
1. Validate: graph.db exists, health score ≥ 0.5 (reject unhealthy graphs)
2. Generate: manifest.json from graph stats + content hashes
3. Generate: knowledge.card.md from compiled artifacts (auto-summary)
4. Generate: provenance.json from source URIs (never raw content)
5. Package: tar.zst (zstandard — same as cargo, 30% better than gzip)
6. Upload: PUT /api/v1/packages/{name}/{version}
7. Index: Hub extracts manifest, indexes for discovery
```

**Content-addressable deduplication:** If two publishers compile the same source material and get identical claims, the BLAKE3 graph hash is identical. The hub stores one copy. This is the same property that makes Git efficient.

### 2. DISCOVER — Find knowledge packages

```bash
root hub search "transformer architectures"
root hub search --domain research/machine-learning
root hub search --entity-type System --min-health 0.8
root hub browse --trending
root hub browse --domain law
```

**Discovery protocol (sparse index, borrowed from crates.io):**

The hub maintains a sparse HTTPS index. Clients fetch only the metadata they need:

```
GET https://hub.thinkingroot.dev/index/na/ve/naveen/transformer-survey
→ Returns: [manifest.json for each version]
```

The index layout mirrors crates.io:
```
index/
├── na/
│   └── ve/
│       └── naveen/
│           └── transformer-survey  ← JSON lines, one per version
├── dj/
│   └── an/
│       └── django/
│           └── official
```

**Why sparse index instead of full database query:**
- CDN-cacheable (each path is a static file)
- Works offline (client caches previously fetched metadata)
- Scales to millions of packages without query load on the hub
- Proven: crates.io serves 130K+ packages this way with near-zero server load

### 3. CONNECT — Mount a remote knowledge graph for agent use

```bash
root hub connect naveen/transformer-survey
root hub connect django/official@5.0
root hub connect --read-only mayo/cardiology-guidelines
```

**What happens internally:**
```
1. Fetch manifest from sparse index
2. Verify BLAKE3 content hash
3. Download graph.db + vectors.bin to ~/.thinkingroot/hub/{name}/{version}/
4. Register in WorkspaceRegistry as a "hub" workspace (read-only flag)
5. Agent MCP/REST queries now span local workspaces + connected hub graphs
```

**Federated query resolution:**

When an agent queries via MCP `search` tool:
```
Agent asks: "What is positional encoding?"

QueryEngine resolves:
  1. Search local workspace(s)         → 3 results
  2. Search connected hub graph(s)     → 12 results  
  3. Merge by relevance score          → ranked list
  4. Return with source attribution    → each result shows origin graph
```

The agent sees a single unified knowledge space. This is federated search — same pattern as `root serve --federated` in Phase 4, but extended to include public hub graphs alongside private org graphs.

### 4. FORK — Create a derivation of someone else's graph

```bash
root hub fork naveen/transformer-survey
# Creates: naveen-fork/transformer-survey in your account
# Downloads graph.db locally
# You can now: root compile ./my-additions → adds to forked graph
# Then: root publish --name my-org/transformer-survey-extended
```

**Why it works with existing infrastructure:**

Forking = `root branch` on a remote graph. ThinkingRoot's KVC system (Phase 3.5) already supports:
- Branch creation (SQLite hot backup of graph.db)
- Extraction cache sharing via symlink
- Semantic diff between branches
- Health-score CI gate
- Merge with contradiction detection

A fork is architecturally identical to a branch — the only difference is the parent is a remote hub graph instead of a local one.

**The fork tree:**
```
django/official@5.0                    ← canonical, published by Django team
    ├── naveen/django-ai-patterns       ← fork: adds AI-specific patterns
    │   └── student/django-ml-tutorial  ← fork-of-fork: adds ML examples
    └── company/django-internal         ← private fork: adds company conventions
```

Each fork maintains provenance: `provenance.json` records the parent graph hash. This is the Merkle DAG property from Git — you can trace any claim back through the fork tree to its original source.

### 5. UPDATE — Publish a new version of an existing graph

```bash
# Recompile with new sources
root compile ./my-sources

# Publish as new version
root publish --version 1.3.0

# Consumers auto-update (if configured)
root hub update                 # update all connected graphs
root hub update --check         # show what would update (dry run)
```

**Versioning scheme (SemVer for knowledge):**
- **MAJOR** (2.0.0): Breaking changes — entity schema changed, fundamental claims revised
- **MINOR** (1.3.0): New knowledge added — new entities, new claims, expanded coverage
- **PATCH** (1.2.1): Corrections — contradiction resolved, stale claims removed, health improved

**Incremental sync (delta protocol):**

On update, the client doesn't re-download the entire graph. The hub computes a delta:

```
Client: I have naveen/transformer-survey@1.2.0 (graph hash: abc123)
Hub:    Version 1.3.0 available (graph hash: def456)
        Delta: +342 claims, +28 entities, ~12 updated, -3 removed
        Delta size: 1.2 MB (vs full graph: 45 MB)
Client: Downloads delta, applies to local graph.db
```

This uses the same BLAKE3 content-hash mechanism that makes `root compile` incremental. Claims are content-addressed by their statement hash — unchanged claims don't transfer.

---

## Architecture Layers

```
┌─────────────────────────────────────────────────────────────┐
│                    AGENT LAYER                              │
│  Claude Desktop · Cursor · Windsurf · Zed · Custom Agents  │
│                    ↕ MCP / REST                             │
├─────────────────────────────────────────────────────────────┤
│                  QUERY ENGINE                               │
│  Federated search across:                                   │
│  [Local workspaces] + [Connected hub graphs] + [Cloud org]  │
│                    ↕                                        │
├─────────────┬───────────────────────┬───────────────────────┤
│  LOCAL      │  HUB (new)            │  CLOUD (Phase 4)      │
│  graph.db   │  ~/.thinkingroot/hub/ │  api.thinkingroot.dev │
│  per-repo   │  cached hub graphs    │  org-wide graph       │
├─────────────┴───────────────────────┴───────────────────────┤
│                  DISTRIBUTION PROTOCOL                      │
│  Sparse index · Content-addressed blobs · Delta sync        │
│  BLAKE3 integrity · zstd compression · ETag caching         │
│                    ↕ HTTPS                                  │
├─────────────────────────────────────────────────────────────┤
│                  HUB REGISTRY                               │
│  hub.thinkingroot.dev                                       │
│  Manifest index · Blob storage · Discovery API              │
│  User accounts · Org namespaces · Access control            │
│  Community signals (stars, forks, health badges)             │
├─────────────────────────────────────────────────────────────┤
│                  COMPILATION ENGINE                          │
│  ThinkingRoot core (Phases 1-3.5, this repo)                │
│  Parse → Extract → Link → Compile → Verify → Serve         │
└─────────────────────────────────────────────────────────────┘
```

### Why This Layering Matters

Each layer is independently useful:
- **Without Hub**: ThinkingRoot works exactly as it does today (local compilation + serve)
- **Without Cloud**: Hub works as a public registry (like crates.io without crates.io teams)
- **With everything**: Full platform — local compilation, public sharing, team collaboration, cross-org federation

No layer depends on a higher layer. This is the principle that made Git successful: Git works without GitHub. GitHub adds value on top.

---

## The Knowledge Card (knowledge.card.md)

Every published package gets a knowledge card — auto-generated from the compiled graph, manually editable by the publisher.

```markdown
# naveen/transformer-survey

> Compiled knowledge from 200 transformer architecture papers (2020-2026)

## Stats
- 47,832 claims across 203 sources
- 2,341 entities (892 Concepts, 234 People, 156 Libraries, 45 Systems)
- 8,903 relations mapped
- 127 contradictions detected (98 auto-resolved, 29 open)

## Health Score: 87%
| Metric | Score |
|--------|-------|
| Freshness | 82% |
| Consistency | 91% |
| Coverage | 85% |
| Provenance | 90% |

## Top Entities
1. Transformer (Concept) — 312 claims
2. Attention Mechanism (Concept) — 287 claims
3. BERT (System) — 198 claims
4. GPT (System) — 245 claims
5. Vaswani et al. (Person) — 89 claims

## Notable Contradictions
- FlashAttention v1 vs v2 positional encoding compatibility
- Layer normalization placement (pre-norm vs post-norm debate)

## Sources
This graph was compiled from:
- 147 arXiv papers (2020-2026)
- 31 blog posts from major AI labs
- 18 GitHub repository READMEs
- 7 conference proceedings

## How to Connect
```bash
root hub connect naveen/transformer-survey
```

## License
CC-BY-4.0
```

This is equivalent to Hugging Face's model card — proven to be the single most important factor in community trust and adoption.

---

## Trust and Safety Architecture

This is where most "sharing" systems fail. ThinkingRoot has unique advantages here.

### Trust Signals (visible on every package)

| Signal | Source | Why It Matters |
|---|---|---|
| **Health score** | Computed by ThinkingRoot verify engine | Low-health graphs are visibly flagged — stale, contradictory, or poorly sourced |
| **Provenance chain** | `provenance.json` | Every claim traces to a source URI — consumers can verify |
| **Publisher verification** | GitHub/email verified account | Prevents impersonation |
| **Fork count** | Hub metadata | Popular graphs get social proof — same as GitHub stars |
| **Downstream count** | How many agents/users are connected | Active use signal |
| **Compiler version** | `manifest.json` | Graphs compiled with old pipeline versions are flagged |
| **Contradiction ratio** | `stats.contradictions / stats.claims` | High contradiction ratios signal unresolved conflicts |

### Safety Mechanisms

**1. Publish-time validation:**
```
root publish →
  ✓ Health score ≥ 0.5 (reject low-quality graphs)
  ✓ No claims with Sensitivity = Restricted or Confidential
  ✓ Provenance chain complete (every claim has a source URI)
  ✓ No duplicate package name in same namespace
  ✓ BLAKE3 hashes verified
```

**2. Content scanning (hub-side):**
```
On upload, hub backend:
  ✓ Scans claim statements for known harmful patterns
  ✓ Checks entity names against abuse lists
  ✓ Rate-limits publishing (prevent spam flooding)
  ✓ Flags anomalous patterns (e.g., 100K claims from 1 source = suspicious)
```

**3. Consumer-side trust levels (already built):**

ThinkingRoot's existing `TrustLevel` system (Verified, Trusted, Unknown, Untrusted, Quarantined) maps directly to hub sources:

```rust
// When connecting a hub graph, claims inherit trust level from publisher
hub_claims.trust_level = match publisher.verified {
    true => TrustLevel::Trusted,
    false => TrustLevel::Unknown,
};

// Consumer can override:
root hub connect naveen/transformer-survey --trust-level Verified
```

**4. Poisoning defense:**

The poisoning scenario: A malicious publisher creates `react/official-docs` with subtly wrong claims about React. Agents connect and get bad information.

Defenses:
- **Namespace protection**: `react/` namespace reserved for verified React team (like npm scoped packages)
- **Community flagging**: Users can report packages — flagged packages get demoted in search
- **Contradiction detection**: If a consumer's local graph contradicts a hub graph, ThinkingRoot surfaces the conflict (existing belief revision engine)
- **Provenance verification**: Every claim cites a source URI. Consumers or automated systems can verify the source actually says what the claim says

---

## How Every Scenario Is Served

### Scenario Matrix (18 scenarios, zero gaps)

| # | Scenario | Operations Used | Trust Model | Offline? |
|---|---|---|---|---|
| 1 | Developer connects to framework docs | `connect` | Publisher verified | Yes (cached) |
| 2 | OSS maintainer publishes project knowledge | `publish` | Self-published | N/A |
| 3 | Student connects to course materials | `connect` | Professor verified | Yes (cached) |
| 4 | Professor publishes course pack | `publish` | Institutional verified | N/A |
| 5 | Researcher forks and extends a survey | `fork` + `publish` | Fork chain visible | N/A |
| 6 | Company publishes internal knowledge (private) | `publish --visibility org` | Org-scoped | N/A |
| 7 | New hire connects to company knowledge | `connect` (org auth) | Org trust | Yes (cached) |
| 8 | Multi-agent system shares context | `connect` × N agents | Single publisher | Yes (cached) |
| 9 | Legal AI connects to statute database | `connect` | Authority verified | Yes (cached) |
| 10 | Medical AI connects to treatment guidelines | `connect` | Institution verified | Yes (cached) |
| 11 | Community compiles Stack Overflow knowledge | `publish` | Community curated | N/A |
| 12 | Expert publishes domain expertise | `publish` | Expert verified | N/A |
| 13 | Government publishes public regulations | `publish` | Government verified | N/A |
| 14 | Team reviews knowledge PR before publish | `fork` + KVC diff | Team RBAC | N/A |
| 15 | Consumer gets update to connected graph | `update` (delta sync) | Same as initial connect | Yes (cached) |
| 16 | Consumer discovers graphs by domain | `discover` (sparse index) | Health-scored | Partial |
| 17 | Self-hosted enterprise runs private hub | Self-host all layers | Air-gapped | Full offline |
| 18 | Agent auto-discovers relevant knowledge | MCP `resources/list` | Agent-selected | Yes (cached) |

### Key Insight: Offline-First

Every connected graph is fully cached locally at `~/.thinkingroot/hub/{name}/{version}/`. Once connected, the agent works even if the hub is down. Updates are pulled when available, not required.

This is the Git property: `git clone` gives you the full repository. You don't need GitHub to be online to use Git.

---

## Hub Backend Architecture

### Components (Phase 4 private repo)

```
hub.thinkingroot.dev/
├── registry/                    
│   ├── index-server/            # Sparse index (Axum, serves static JSON)
│   ├── blob-store/              # S3-compatible (graph.db, vectors.bin, artifacts/)
│   ├── manifest-db/             # PostgreSQL (manifest metadata, search index)
│   └── delta-engine/            # Computes graph deltas between versions
│
├── api/
│   ├── publish/                 # Upload + validation + index update
│   ├── search/                  # Full-text + semantic search over manifests
│   ├── download/                # Content-addressed blob retrieval
│   ├── fork/                    # Clone graph to new namespace
│   └── community/               # Stars, flags, comments, fork count
│
├── auth/
│   ├── accounts/                # User + org accounts
│   ├── namespaces/              # Name reservation + verification
│   └── tokens/                  # API tokens for CLI publish/connect
│
├── web/                          
│   ├── browse/                  # Package discovery UI
│   ├── package-page/            # Knowledge card view
│   ├── search/                  # Search results UI
│   └── dashboard/               # Publisher analytics
│
└── workers/
    ├── validation-worker/       # Async publish validation
    ├── delta-worker/            # Pre-compute deltas for popular packages
    └── health-worker/           # Periodic re-verification of published graphs
```

### Storage Economics

| Component | Storage per Package | At 10K Packages | At 1M packages |
|---|---|---|---|
| Manifest index | ~2 KB | 20 MB | 2 GB |
| Graph (graph.db) | 1-100 MB typical | 100 GB - 1 TB | 10-100 TB |
| Vectors (vectors.bin) | 10-50 MB | 100-500 GB | 10-50 TB |
| Artifacts (markdown) | 1-10 MB | 10-100 GB | 1-10 TB |

At 10K packages (year 1-2 target), total storage is ~1-2 TB. This is trivially served by S3 at ~$23/month/TB.

At 1M packages (at-scale), storage is ~50-150 TB. This is still within standard infrastructure costs (~$1,500-3,500/month on S3).

For comparison: Docker Hub serves **billions** of pulls per month across petabytes. npm serves trillions of downloads per year. The storage economics of a knowledge registry are far smaller because knowledge graphs are compact (megabytes, not gigabytes like container images).

---

## CLI Integration (OSS Crate Changes)

### New Crate: `thinkingroot-hub` (in this OSS repo)

```rust
// crates/thinkingroot-hub/src/lib.rs

/// Client for interacting with a ThinkingRoot Knowledge Hub.
pub struct HubClient {
    endpoint: String,          // default: "https://hub.thinkingroot.dev"
    auth: Option<HubAuth>,     // API token for publish/private access
    cache_dir: PathBuf,        // ~/.thinkingroot/hub/
}

/// A published knowledge package.
pub struct KnowledgePack {
    pub manifest: Manifest,
    pub graph_path: PathBuf,
    pub vectors_path: Option<PathBuf>,
    pub artifacts_path: PathBuf,
    pub knowledge_card: String,
    pub provenance: Provenance,
}

/// Package manifest.
pub struct Manifest {
    pub schema_version: String,
    pub name: String,              // "naveen/transformer-survey"
    pub version: semver::Version,
    pub description: String,
    pub stats: GraphStats,
    pub health: HealthScore,
    pub content_hash: ContentHashes,
    pub publisher: PublisherInfo,
    pub tags: Vec<String>,
    pub domain: String,
    pub license: String,
    pub dependencies: Vec<Dependency>,
    pub compiled: CompilationInfo,
}

/// Provenance — source URIs only, never raw content.
pub struct Provenance {
    pub sources: Vec<ProvenanceEntry>,
    pub parent_graph: Option<String>,  // BLAKE3 hash of parent (if forked)
}

pub struct ProvenanceEntry {
    pub uri: String,           // e.g., "https://arxiv.org/abs/2306.12345"
    pub source_type: String,   // "Document", "Code", etc.
    pub claim_count: usize,
}
```

### New CLI Commands

```rust
// crates/thinkingroot-cli/src/main.rs (extend Commands enum)

#[derive(Subcommand)]
enum Hub {
    /// Search for knowledge packages on the hub.
    Search {
        query: String,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, default_value = "10")]
        limit: usize,
    },
    
    /// Connect a hub knowledge graph for agent use.
    Connect {
        /// Package name (e.g., "naveen/transformer-survey")
        package: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value = "Unknown")]
        trust_level: String,
    },
    
    /// Disconnect a previously connected hub graph.
    Disconnect { package: String },
    
    /// Fork a hub package into your namespace.
    Fork { package: String },
    
    /// Update all connected hub graphs to latest versions.
    Update {
        #[arg(long)]
        check: bool,  // dry run
    },
    
    /// List connected hub graphs.
    List,
    
    /// Browse trending/popular packages.
    Browse {
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        trending: bool,
    },
}
```

### Modified: `QueryEngine` (federated hub queries)

The existing `QueryEngine` in `thinkingroot-serve` gains awareness of hub graphs:

```rust
// Pseudocode — extends existing QueryEngine
impl QueryEngine {
    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<SearchResult>> {
        let mut results = Vec::new();
        
        // 1. Search local workspace graphs (existing behavior)
        for workspace in &self.local_workspaces {
            results.extend(workspace.search(query, top_k)?);
        }
        
        // 2. Search connected hub graphs (NEW)
        for hub_graph in &self.connected_hub_graphs {
            let hub_results = hub_graph.search(query, top_k)?;
            // Tag each result with its source graph for attribution
            for mut r in hub_results {
                r.source_graph = Some(hub_graph.package_name.clone());
                results.push(r);
            }
        }
        
        // 3. Rank by combined relevance score
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        results.truncate(top_k);
        
        Ok(results)
    }
}
```

---

## Dependency Graphs: Composable Knowledge

Knowledge packages can declare dependencies on other packages:

```json
{
  "dependencies": [
    {"name": "python/stdlib@3.12", "optional": false},
    {"name": "community/machine-learning-concepts", "optional": true}
  ]
}
```

When connecting a package with dependencies, the hub resolves the dependency tree:

```bash
root hub connect naveen/django-ml-tutorial
# Resolving dependencies...
#   naveen/django-ml-tutorial@1.0.0
#   ├── django/official@5.0.0 (required)
#   └── community/ml-concepts@2.1.0 (optional, included)
# 
# Connect 3 packages? (Y/n) › y
```

**Why dependencies matter:** Knowledge doesn't exist in isolation. A "Django ML Tutorial" graph depends on knowledge about Django itself and about ML concepts. Without dependencies, the graph has dangling entity references. With dependencies, the agent has full context.

**Dependency resolution uses the same algorithm as cargo:** Version constraints (^1.0, ~1.2, =1.2.3), conflict detection, and lockfile-style pinning.

---

## The Network Effect Flywheel

```
More published graphs
    → More agents connect
        → More developers see value
            → More developers compile and publish their own
                → More published graphs
```

**The critical insight:** Unlike code packages (npm) where publishing requires writing code, publishing a knowledge graph requires only having *sources*. A professor with a folder of PDFs can `root compile ./papers && root publish`. The barrier to publishing is dramatically lower than any existing package registry.

**Seeding strategy (year 1):**
1. ThinkingRoot team compiles and publishes the top 50 open source frameworks (Django, React, Rust stdlib, Node.js, etc.)
2. Partner with 5 universities to publish course packs
3. Partner with 3 open source foundations (CNCF, Apache, Linux Foundation) to publish official project knowledge
4. Launch with 100+ high-quality seed graphs
5. Community takes over publishing at ~500+ graphs

---

## Integration Points (How Agents Actually Use This)

### Via MCP (Primary — Zero Config)

When a hub graph is connected, it automatically appears in the MCP `resources/list`:

```json
{
  "resources": [
    {
      "uri": "thinkingroot://local/my-repo",
      "name": "my-repo",
      "description": "Local workspace"
    },
    {
      "uri": "thinkingroot://hub/django/official@5.0",
      "name": "django/official",
      "description": "Hub: Django framework knowledge (50K claims)"
    }
  ]
}
```

Agents (Claude Desktop, Cursor, etc.) see hub graphs as additional MCP resources. No agent-side configuration needed.

### Via REST API

```
GET /api/v1/hub/{package}/entities
GET /api/v1/hub/{package}/claims?type=Fact&min_confidence=0.8
GET /api/v1/hub/{package}/search?q=positional+encoding
GET /api/v1/hub/{package}/relations/{entity}
```

Same API shape as local workspaces. Same response envelope. Clients don't need separate code paths.

### Via Python SDK

```python
from thinkingroot.hub import HubClient

hub = HubClient()
hub.connect("naveen/transformer-survey")

# Now queries span local + hub graphs
engine = thinkingroot.open("./my-repo")
results = engine.search("attention mechanism")
# Returns results from both local and connected hub graphs
```

---

## What's Truly World-First Here

Let me be precise about what has never existed before:

| Property | Closest Precedent | Why This Is Different |
|---|---|---|
| Compiled knowledge as a package | Hugging Face (models) | Models are weights. This is compiled facts, entities, relations — queryable, not inferable |
| Content-addressed knowledge objects | Git (code objects) | Git stores code diffs. This stores knowledge diffs — claims added, entities merged, contradictions resolved |
| Forking a knowledge graph | GitHub (forking repos) | Forking code = copying files. Forking knowledge = branching a semantic graph with entity resolution |
| Federated knowledge queries | Google Knowledge Graph (internal) | Google's is monolithic and proprietary. This is open, distributed, user-controlled |
| Versioned knowledge with SemVer | npm (versioned packages) | npm versions code. This versions facts — MAJOR = facts changed, MINOR = facts added |
| Knowledge health as a quality signal | npm download counts | Download counts measure popularity. Health scores measure knowledge quality (freshness, consistency, coverage) |
| Agent-native knowledge distribution | MCP registries (tools) | MCP registries share tools (actions). This shares knowledge (facts). Different primitive entirely |
| Contradiction detection across graphs | Nothing | No distributed system detects when two independently published knowledge graphs contradict each other |

The last one — **cross-graph contradiction detection** — is genuinely novel. When a consumer connects two hub graphs that make conflicting claims, ThinkingRoot's belief revision engine can detect and surface the contradiction. No existing system does this.

---

## Timeline Mapping to Existing Roadmap

| When | What | Depends On |
|---|---|---|
| **Phase 3.5** (KVC) | Branch/fork infrastructure | Existing KVC work |
| **Phase 4a** (Cloud) | `root login`, `root sync`, cloud backend | Phase 3.5 |
| **Phase 4b** (Hub) | `root publish`, `root hub connect`, sparse index, hub registry | Phase 4a (auth + backend) |
| **Phase 4c** (Community) | Stars, forks, knowledge cards, search UI, trending | Phase 4b |
| **Phase 5** (Enterprise) | Private hub, self-hosted registry, air-gapped, SSO | Phase 4b |

Hub (Phase 4b) is a natural extension of Cloud (Phase 4a). The same auth system, the same graph sync protocol, the same backend infrastructure. The difference is visibility: Cloud = private org sync. Hub = public registry.

---

## One Sentence

**ThinkingRoot Knowledge Hub is the world's first open registry for compiled, verified, version-controlled, agent-queryable knowledge — built on proven infrastructure patterns from Git, npm, OCI, and Hugging Face, and powered by the only open-source knowledge compiler that produces the artifacts worth sharing.**
