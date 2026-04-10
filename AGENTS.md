# ThinkingRoot — AI Development Guide

ThinkingRoot is a **knowledge compiler for AI agents**. It runs a 6-stage pipeline over a codebase or document set — parse → extract (LLM) → link → compile → verify → serve — and produces a typed knowledge graph plus compiled artifacts (entity pages, architecture maps, decision logs, etc.) accessible via REST API, MCP server, and Python SDK.

**Binary name:** `root`  
**Primary language:** Rust (edition 2024, rust-version 1.85)  
**Workspace resolver:** 2

---

## Build Commands

```bash
# Type-check the entire workspace (includes Python cdylib — no linking)
cargo check --workspace

# Build all default members (excludes thinkingroot-python — see GOTCHAS)
cargo build
cargo build --release

# Build the CLI binary only
cargo build --release -p thinkingroot-cli
# Binary lands at: target/release/root

# Build WITHOUT vector/fastembed (no ONNX Runtime, fast, ~300MB less)
cargo build --no-default-features
cargo build --no-default-features -p thinkingroot-serve

# Run all tests (excludes Python)
cargo test
cargo test --no-default-features    # Faster — skips ONNX linking

# Run tests for a specific crate
cargo test -p thinkingroot-serve
cargo test -p thinkingroot-core
cargo test -p thinkingroot-graph

# Lint and format
cargo fmt --all
cargo clippy --workspace -- -D warnings

# Python binding — MUST use maturin, not cargo (see GOTCHAS)
cd thinkingroot-python
maturin develop --release           # Build + install into current Python env
maturin build --release             # Build wheel only
```

---

## Workspace Structure

```
thinkingroot/
├── Cargo.toml                          # Workspace root
├── crates/
│   ├── thinkingroot-core/              # Foundation: types, errors, config, IDs
│   ├── thinkingroot-parse/             # Stage 1: markdown, code, PDF, git → DocumentIR
│   ├── thinkingroot-extract/           # Stage 2: LLM extraction → Claims/Entities/Relations
│   ├── thinkingroot-link/              # Stage 3: entity resolution, contradiction detection
│   ├── thinkingroot-graph/             # Graph storage (CozoDB) + vector search (fastembed)
│   ├── thinkingroot-compile/           # Stage 4: knowledge graph → compiled artifacts
│   ├── thinkingroot-verify/            # Stage 5: health scoring, staleness, contradiction checks
│   ├── thinkingroot-serve/             # Stage 6: REST API (Axum) + MCP server
│   ├── thinkingroot-safety/            # Safety engine (access control, trust levels)
│   └── thinkingroot-cli/              # CLI binary (`root` command)
├── thinkingroot-python/                # PyO3 cdylib — Python SDK (maturin only)
└── docs/
    ├── 2026-04-08-engram-knowledge-compiler-design.md   # Product design doc
    └── superpowers/specs/2026-04-09-phase2-serve-sdk-design.md
```

### Dependency Order (build from top to bottom)

```
thinkingroot-core  (no internal deps)
    ↓
thinkingroot-graph, thinkingroot-parse
    ↓
thinkingroot-extract, thinkingroot-link
    ↓
thinkingroot-compile, thinkingroot-verify
    ↓
thinkingroot-serve  (aggregates all above)
    ↓
thinkingroot-cli    (uses serve + all stages)
    ↓
thinkingroot-python (PyO3 bindings over serve)
```

---

## Feature Flags

The `vector` feature controls whether fastembed + ONNX Runtime (~300 MB) is compiled in. It is **ON by default** in all crates that use it.

| Crate | Feature definition | What it gates |
|---|---|---|
| `thinkingroot-graph` | `default = ["vector"]` / `vector = ["dep:fastembed"]` | Real `VectorStore` with fastembed; disabled = no-op stub |
| `thinkingroot-serve` | `default = ["vector"]` / `vector = ["thinkingroot-graph/vector"]` | Chains to graph |
| `thinkingroot-cli` | `default = ["vector"]` / `vector = ["thinkingroot-graph/vector", "thinkingroot-serve/vector"]` | Chains to serve |
| `thinkingroot-python` | `default = ["vector"]` / `vector = ["thinkingroot-graph/vector", "thinkingroot-serve/vector"]` | Chains to serve |

**Rule:** When adding a new crate that depends on graph or serve, declare:
```toml
[features]
default = ["vector"]
vector = ["thinkingroot-graph/vector"]   # or thinkingroot-serve/vector

[dependencies]
thinkingroot-graph = { workspace = true }  # NO explicit features = ["vector"] here
```

Never write `features = ["vector"]` in a dependency declaration — use the crate's own `[features]` section instead. Explicit feature declarations in deps bypass `--no-default-features`.

---

## Core Types (thinkingroot-core)

All domain types live in `crates/thinkingroot-core/src/types/`.

### Type-Safe IDs
```rust
// All IDs are ULID-backed, parameterised by phantom marker type
pub type SourceId = Id<markers::SourceMarker>;
pub type ClaimId = Id<markers::ClaimMarker>;
pub type EntityId = Id<markers::EntityMarker>;
pub type RelationId = Id<markers::RelationMarker>;
pub type ContradictionId = Id<markers::ContradictionMarker>;
pub type ArtifactId = Id<markers::ArtifactMarker>;
pub type WorkspaceId = Id<markers::WorkspaceMarker>;
```
ULIDs are monotonic, URL-safe, sortable. Serialise as strings.

### Claim (fundamental unit)
```rust
pub struct Claim {
    pub id: ClaimId,
    pub statement: String,
    pub claim_type: ClaimType,
    pub source: SourceId,
    pub source_span: Option<SourceSpan>,
    pub confidence: Confidence,          // clamped [0.0, 1.0]
    pub sensitivity: Sensitivity,
    pub workspace: WorkspaceId,
    pub valid_from: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub superseded_by: Option<ClaimId>,
    pub extracted_by: PipelineVersion,
}
```
`ClaimType`: Fact, Decision, Opinion, Plan, Requirement, Metric, Definition, Dependency, ApiSignature, Architecture

### Entity
```rust
pub struct Entity {
    pub id: EntityId,
    pub canonical_name: String,
    pub entity_type: EntityType,
    pub aliases: Vec<String>,
    pub attributes: Vec<ClaimId>,
    pub description: Option<String>,
}
```
`EntityType`: Person, System, Service, Concept, Team, Api, Database, Library, File, Module, Function, Config, Organization

### Relation
```rust
pub struct Relation {
    pub id: RelationId,
    pub from: EntityId,
    pub to: EntityId,
    pub relation_type: RelationType,
    pub evidence: Vec<ClaimId>,
    pub strength: Strength,              // clamped [0.0, 1.0]
}
```
`RelationType`: DependsOn, OwnedBy, Replaces, Contradicts, Implements, Uses, Contains, CreatedBy, PartOf, RelatedTo, Calls, ConfiguredBy, TestedBy

### HealthScore
```rust
pub struct HealthScore {
    pub overall: f64,       // freshness*0.3 + consistency*0.3 + coverage*0.2 + provenance*0.2
    pub freshness: f64,
    pub consistency: f64,
    pub coverage: f64,
    pub provenance: f64,
}
```

### DocumentIR (parser output)
```rust
pub struct DocumentIR {
    pub source_id: SourceId,
    pub uri: String,
    pub source_type: SourceType,
    pub chunks: Vec<Chunk>,
    pub content_hash: ContentHash,      // BLAKE3
}
```
`ChunkType`: Heading, CodeBlock, Prose, ListItem, Table, Metadata

---

## Graph Storage (thinkingroot-graph)

**Database:** CozoDB community edition (cozo-ce 0.7.13-alpha.3), embedded SQLite backend.  
**Query language:** Datalog (NOT SQL). Schema uses `:create`, queries use `?[col] := *relation{...}`.  
**Database file:** `.thinkingroot/graph.db`

### CozoDB Schema (9 relations)
```
sources            { id: String => uri, source_type, author, content_hash, trust_level, byte_size }
claims             { id: String => statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at }
entities           { id: String => canonical_name, entity_type, description }
entity_aliases     { entity_id: String, alias: String }        -- many-to-many
entity_relations   { from_id: String, to_id: String, relation_type: String => strength }
claim_source_edges { claim_id: String, source_id: String }     -- many-to-many
claim_entity_edges { claim_id: String, entity_id: String }     -- many-to-many
claim_temporal     { claim_id: String => valid_from, valid_until, superseded_by }
contradictions     { id: String => claim_a, claim_b, explanation, status, detected_at }
```

### Datalog query pattern
```rust
let mut params = BTreeMap::new();
params.insert("name".into(), DataValue::Str("MyService".into()));
let result = self.db.run_script(
    "?[id, type] := *entities{id, canonical_name: $name, entity_type: type}",
    params,
    ScriptMutability::Immutable,
)?;
// result.rows: Vec<Vec<DataValue>>
```

---

## REST API (thinkingroot-serve)

**File:** `crates/thinkingroot-serve/src/rest.rs`  
**Framework:** Axum 0.8

### Response envelope
```json
{ "ok": true, "data": {...}, "error": null }
{ "ok": false, "data": null, "error": { "code": "NOT_FOUND", "message": "..." } }
```

### Endpoints
```
GET  /api/v1/workspaces
GET  /api/v1/ws/{workspace}/entities
GET  /api/v1/ws/{workspace}/entities/{name}
GET  /api/v1/ws/{workspace}/claims?type=&entity=&min_confidence=&limit=&offset=
GET  /api/v1/ws/{workspace}/relations
GET  /api/v1/ws/{workspace}/relations/{entity}
GET  /api/v1/ws/{workspace}/artifacts
GET  /api/v1/ws/{workspace}/artifacts/{type}
GET  /api/v1/ws/{workspace}/health
GET  /api/v1/ws/{workspace}/search?q=&top_k=10
POST /api/v1/ws/{workspace}/compile
POST /api/v1/ws/{workspace}/verify
```

### AppState
```rust
pub struct AppState {
    pub engine: RwLock<QueryEngine>,
    pub api_key: Option<String>,
    pub mcp_sessions: crate::mcp::sse::SseSessionMap,
}
// Always construct via:
let state = AppState::new(engine, api_key);
```

---

## MCP Server (thinkingroot-serve)

**Protocol:** MCP 2024-11-05  
**Transports:** stdio (`mcp/stdio.rs`) + HTTP SSE (`mcp/sse.rs`)

### SSE transport flow
1. `GET /mcp/sse` — create UUID session, return SSE stream, send `endpoint` event pointing to `/mcp?sessionId={id}`
2. `POST /mcp?sessionId={id}` — receive JSON-RPC request, dispatch, send response to session's SSE channel, return 202 Accepted
3. Notifications (id = null) return 202 immediately without dispatch

### MCP methods exposed
- `initialize`, `notifications/initialized`, `ping`
- `resources/list`, `resources/read` — knowledge as MCP resources
- `tools/list`, `tools/call` — tools: `search`, `query_claims`, `get_relations`, `compile`, `health_check`

---

## CLI Commands

**Binary:** `root`  
**Source:** `crates/thinkingroot-cli/src/`

```bash
root compile <path>              # Full 6-stage pipeline on a directory
root health [--path=.]           # Run verification, print health score
root init [--path=.]             # Create .thinkingroot/config.toml with defaults
root query <query>               # Semantic + keyword search
  --path=.                       # Path to compiled knowledge base
  --top-k=10                     # Max results
root serve                       # Start REST + MCP server
  --port=3000
  --host=127.0.0.1
  --api-key=<key>                # Optional bearer auth
  --path=<workspace>             # Repeatable (multi-workspace mount)
  --mcp-stdio                    # Use stdio transport instead of HTTP
  --no-rest                      # Disable REST routes
  --no-mcp                       # Disable MCP routes
```

---

## Configuration (.thinkingroot/config.toml)

```toml
[workspace]
name = "my-org"
data_dir = ".thinkingroot"

[llm]
default_provider = "bedrock"      # "bedrock" | "openai" | "anthropic" | "ollama" | "groq" | "deepseek"
extraction_model = "amazon.nova-micro-v1:0"
compilation_model = "amazon.nova-micro-v1:0"
max_concurrent_requests = 5
request_timeout_secs = 120

[llm.providers.bedrock]
region = "us-east-1"
profile = "default"

[llm.providers.openai]
api_key_env = "OPENAI_API_KEY"

[extraction]
max_chunk_tokens = 4000
min_confidence = 0.5
extract_relations = true
max_retries = 3

[compilation]
enabled_artifacts = ["entity_page", "architecture_map", "contradiction_report", "health_report"]
output_dir = "artifacts"

[verification]
staleness_days = 90
min_freshness = 0.5
auto_resolve = true

[parsers]
exclude_patterns = ["target/**", "node_modules/**", ".git/**", ".thinkingroot/**", "*.lock"]
respect_gitignore = true
max_file_size = 1048576
```

---

## Python SDK

**Install:**
```bash
cd thinkingroot-python && maturin develop --release
pip install thinkingroot   # once published to PyPI
```

**Native API:**
```python
import thinkingroot
thinkingroot.compile("./my-repo")
engine = thinkingroot.open("./my-repo")
```

**HTTP Client:**
```python
from thinkingroot.client import Client
c = Client(base_url="http://localhost:3000", api_key="optional")
c.workspaces()
c.entities()                      # auto-resolves single workspace
c.claims(type="Fact", min_confidence=0.8)
c.search("authentication flow")
```
Raises `APIError(status_code, code, message)` on failures.  
Raises `ThinkingRootError` (native) for Rust-side errors.

---

## Critical Gotchas

### 1. Python cdylib requires maturin — never use `cargo build`
```bash
# WRONG — missing Python headers, will fail with "symbol not found"
cargo build -p thinkingroot-python

# CORRECT
cd thinkingroot-python && maturin develop --release
```

### 2. thinkingroot-python is in `members` but NOT `default-members`
`cargo build` / `cargo test` skip it. `cargo check --workspace` still validates it.  
The Python crate is in `members` so Cargo resolves its dependencies; it's out of `default-members` so it isn't linked by bare `cargo build`.

### 3. Never put `features = ["vector"]` in a dep declaration
```toml
# WRONG — bypasses --no-default-features, always forces vector ON globally
thinkingroot-graph = { workspace = true, features = ["vector"] }

# CORRECT — use crate-level [features] section
[features]
default = ["vector"]
vector = ["thinkingroot-graph/vector"]
```
Cargo feature resolution is global: any crate that explicitly requests a feature forces it ON for every other crate sharing that dependency, even with `--no-default-features`.

### 4. Graph queries use Datalog, not SQL
CozoDB syntax: `?[col] := *relation{field: $param, other_field: col}`  
Parameters are `BTreeMap<String, DataValue>`.  
Use `ScriptMutability::Mutable` for `:put` / `:rm`, `Immutable` for reads.

### 5. All IDs are type-safe — don't cast between them
`SourceId` and `ClaimId` are different types. `.to_string()` converts to string for graph storage. Parse from string with `Id::from_str()` or `"xxx".parse::<ClaimId>()`.

### 6. LLM extraction requires credentials
Default provider is AWS Bedrock. Set `~/.aws/credentials` or switch provider in config.  
Extraction stage degrades gracefully (skips LLM, no claims extracted) if provider is unreachable.

### 7. Incremental compilation via BLAKE3 content hashes
The pipeline checks `graph.source_hash_exists(hash)` before re-parsing a file.  
Force full re-run by deleting `.thinkingroot/graph.db`.

### 8. Workspace = `.thinkingroot/` directory
Each compiled knowledge base lives in a `.thinkingroot/` directory at the repo root.  
`graph.db` = CozoDB, `artifacts/` = compiled markdown, `models/` = fastembed model cache.

---

## Adding New Features: Where to Put Code

| What | Where |
|---|---|
| New core domain type | `crates/thinkingroot-core/src/types/` |
| New parser (file format) | `crates/thinkingroot-parse/src/` |
| New LLM prompt | `crates/thinkingroot-extract/src/prompts.rs` |
| New graph relation/table | `crates/thinkingroot-graph/src/graph.rs` (schema + CRUD) |
| New artifact type | `crates/thinkingroot-compile/src/compiler.rs` + template |
| New REST endpoint | `crates/thinkingroot-serve/src/rest.rs` |
| New MCP tool | `crates/thinkingroot-serve/src/mcp/tools.rs` |
| New MCP resource | `crates/thinkingroot-serve/src/mcp/resources.rs` |
| New CLI command | `crates/thinkingroot-cli/src/main.rs` (Clap enum) |
| New Python binding | `thinkingroot-python/src/lib.rs` (PyO3 `#[pyfunction]`) |
| New verification check | `crates/thinkingroot-verify/src/verifier.rs` |

---

## Phase Status

| Phase | Status | What it includes |
|---|---|---|
| **Phase 1** (Core engine) | **COMPLETE** | All 6 crates, CLI, CozoDB schema, fastembed vectors |
| **Phase 2** (Serve + SDK) | **COMPLETE** | REST API, MCP SSE, Python SDK, AppState, entity aliases |
| **Phase 3** (Ecosystem) | NOT STARTED | TypeScript SDK, GitHub Action, VS Code extension, safety engine |
| **Phase 4** (Cloud platform) | NOT STARTED — **PRIVATE REPO** | SaaS dashboard, connectors, multi-tenant backend |
| **Phase 5** (Enterprise) | NOT STARTED — **PRIVATE REPO** | SSO, compliance, air-gapped deploy |

Phases 1–3 are open source (this repo). Phases 4–5 are in a separate private repo that imports Phase 1–3 as dependencies.

---

## Running End-to-End

```bash
# 1. Build
cargo build --release

# 2. Init a test workspace
./target/release/root init ./test-repo

# 3. Compile (requires LLM credentials for extraction stage)
./target/release/root compile ./test-repo

# 4. Check health
./target/release/root health --path ./test-repo

# 5. Start server
./target/release/root serve --port 3000 --path ./test-repo

# 6. Query
curl http://localhost:3000/api/v1/workspaces
curl "http://localhost:3000/api/v1/ws/test-repo/search?q=authentication"
```
