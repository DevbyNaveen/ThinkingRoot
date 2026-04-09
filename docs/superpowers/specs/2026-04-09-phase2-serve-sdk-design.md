# ThinkingRoot Phase 2 Design Spec — Serve & SDK

**Date:** 2026-04-09
**Status:** Approved
**Author:** Naveen + Claude (CTO pair)

---

## Overview

Phase 2 makes compiled knowledge **queryable**. Phase 1 compiles docs/code/git into a knowledge graph with 8 artifact types. Phase 2 exposes that graph through three interfaces: REST API, MCP Server, and Python SDK.

**Architecture:** Shared Query Engine with transport adapters.

```
                        ┌─ REST API (Axum, JSON)
QueryEngine ────────────┼─ MCP HTTP/SSE (Axum, JSON-RPC 2.0)
  (shared core)         ├─ MCP stdio (tokio stdin/stdout)
                        └─ PyO3 (direct Rust calls)
                              │
                        Python HTTP Client (httpx → REST API)
```

All transports call the same `QueryEngine` methods. No logic duplication.

---

## 1. Query Engine

The shared brain. Lives in `thinkingroot-serve/src/engine.rs`.

### Data Structures

```rust
pub struct QueryEngine {
    workspaces: HashMap<String, WorkspaceHandle>,
}

pub struct WorkspaceHandle {
    name: String,
    root_path: PathBuf,
    storage: StorageEngine,  // graph (CozoDB) + vector (fastembed)
    config: Config,
}
```

### Operations

| Category | Method | Input | Returns |
|----------|--------|-------|---------|
| **Workspace** | `list_workspaces()` | — | Vec of (name, path, entity_count, claim_count) |
| **Workspace** | `mount_workspace(name, path)` | name, root_path | Result<()> |
| **Workspace** | `unmount_workspace(name)` | name | Result<()> |
| **Entities** | `list_entities(ws)` | workspace name | Vec of (id, name, type, claim_count) |
| **Entities** | `get_entity(ws, name)` | workspace, entity name | Entity page: claims, relations, aliases |
| **Entities** | `search_entities(ws, query, top_k)` | workspace, query text, limit | Ranked entity results with relevance |
| **Claims** | `list_claims(ws, filter)` | workspace, ClaimFilter (type, entity, min_confidence) | Filtered claims with source URIs |
| **Claims** | `search_claims(ws, query, top_k)` | workspace, query text, limit | Ranked claim results with relevance |
| **Relations** | `get_relations(ws, entity)` | workspace, entity name | Relations for one entity |
| **Relations** | `get_all_relations(ws)` | workspace | Full relation graph |
| **Artifacts** | `list_artifacts(ws)` | workspace | Available artifact types + last compiled |
| **Artifacts** | `get_artifact(ws, artifact_type)` | workspace, type name | Artifact content (markdown string) |
| **Health** | `health(ws)` | workspace | HealthScore + VerificationResult + warnings |
| **Search** | `search(ws, query, top_k)` | workspace, query text, limit | Unified: entities + claims ranked together |
| **Pipeline** | `compile(ws)` | workspace | PipelineResult (counts + health score). Requires LLM credentials (runs full extraction pipeline). |
| **Pipeline** | `verify(ws)` | workspace | VerificationResult (no LLM needed — graph queries only) |

### ClaimFilter

```rust
pub struct ClaimFilter {
    pub claim_type: Option<String>,       // "Decision", "Fact", etc.
    pub entity_name: Option<String>,      // filter by entity
    pub min_confidence: Option<f64>,      // minimum confidence threshold
    pub limit: Option<usize>,            // max results (default: 100)
    pub offset: Option<usize>,           // pagination offset
}
```

### SearchResult

```rust
pub struct SearchResult {
    pub entities: Vec<EntityResult>,
    pub claims: Vec<ClaimResult>,
}

pub struct EntityResult {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
    pub relevance: f32,
}

pub struct ClaimResult {
    pub id: String,
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub source_uri: String,
    pub relevance: f32,
}
```

### Search Strategy

1. **Vector search first** — embed query via fastembed, cosine similarity against all stored embeddings
2. **Keyword fallback** — if vector search returns < `top_k` results, supplement with CozoDB `regex_matches` keyword search
3. **Merge + dedup** — combine both result sets, deduplicate by ID, sort by relevance descending
4. **Filter** — drop results with relevance < 0.1

---

## 2. REST API

Axum server. JSON responses. Optional API key auth.

### Routes

```
GET  /api/v1/workspaces                                    → list mounted workspaces
GET  /api/v1/ws/{ws}/entities                               → list entities
GET  /api/v1/ws/{ws}/entities/{name}                        → entity page (claims, relations, aliases)
GET  /api/v1/ws/{ws}/claims?type=Decision&min_confidence=0.8&entity=PostgreSQL&limit=50&offset=0
                                                            → filtered claims
GET  /api/v1/ws/{ws}/relations                              → all relations
GET  /api/v1/ws/{ws}/relations/{entity}                     → relations for one entity
GET  /api/v1/ws/{ws}/artifacts                              → list artifact types
GET  /api/v1/ws/{ws}/artifacts/{type}                       → artifact content
GET  /api/v1/ws/{ws}/health                                 → health score + warnings
GET  /api/v1/ws/{ws}/search?q=payment+processing&top_k=10  → unified search
POST /api/v1/ws/{ws}/compile                                → trigger recompilation
POST /api/v1/ws/{ws}/verify                                 → run verification only
```

### Response Envelope

All JSON responses use a consistent envelope:

```json
{
  "ok": true,
  "data": { ... },
  "error": null
}
```

Error responses:

```json
{
  "ok": false,
  "data": null,
  "error": { "code": "NOT_FOUND", "message": "Entity 'Foo' not found" }
}
```

### HTTP Status Codes

| Code | When |
|------|------|
| 200 | Success |
| 400 | Invalid query parameters |
| 401 | Missing/wrong API key |
| 404 | Workspace/entity/artifact not found |
| 500 | Internal error (graph query failure, etc.) |

### Content Negotiation

`GET /api/v1/ws/{ws}/artifacts/{type}`:
- `Accept: application/json` (default) → JSON envelope with content field
- `Accept: text/markdown` → raw markdown body

### Authentication Middleware

- No `--api-key` flag → all routes open (localhost use)
- `--api-key SECRET` flag → requires `Authorization: Bearer SECRET` header
- Middleware checks before route handlers
- 401 response with `{ "ok": false, "error": { "code": "UNAUTHORIZED", "message": "Invalid or missing API key" } }`

### CORS

Enabled via `tower-http` CorsLayer:
- `Access-Control-Allow-Origin: *` (configurable in future)
- Allow GET, POST, OPTIONS
- Allow headers: Authorization, Content-Type, Accept

---

## 3. MCP Server

Model Context Protocol (JSON-RPC 2.0). Two transports.

### 3.1 stdio Transport

```
root serve --mcp-stdio --path ./repo
```

- Launched as subprocess by MCP clients (Claude Code, Cursor, Windsurf)
- Reads JSON-RPC from stdin, writes to stdout
- stderr for logging (does not interfere with protocol)
- Single workspace (path argument)
- Graceful shutdown on stdin EOF

### 3.2 HTTP/SSE Transport

Mounted on the same Axum server as REST API at `/mcp`.

- `POST /mcp` — client sends JSON-RPC requests
- `GET /mcp/sse` — server pushes events via Server-Sent Events
- Multi-workspace (all mounted workspaces accessible)
- Multiple concurrent agent connections

### MCP Handshake

```
Client → Server: { "method": "initialize", "params": { "protocolVersion": "2024-11-05", "clientInfo": {...} } }
Server → Client: { "result": { "protocolVersion": "2024-11-05", "serverInfo": { "name": "thinkingroot", "version": "0.1.0" }, "capabilities": { "resources": {}, "tools": {} } } }
Client → Server: { "method": "notifications/initialized" }
```

### MCP Resources

| URI | Description | MIME Type |
|-----|-------------|-----------|
| `thinkingroot://{ws}/entities` | JSON list of all entities | application/json |
| `thinkingroot://{ws}/entities/{name}` | Entity page with claims + relations | application/json |
| `thinkingroot://{ws}/artifacts/{type}` | Compiled artifact | text/markdown |
| `thinkingroot://{ws}/health` | Health score + warnings | application/json |
| `thinkingroot://{ws}/contradictions` | Unresolved contradictions | application/json |

Resources support `resources/list` and `resources/read` methods.

### MCP Tools

| Tool Name | Parameters | Description |
|-----------|-----------|-------------|
| `search` | `query: string`, `top_k: int` (default 10), `workspace: string` | Semantic search across entities + claims |
| `query_claims` | `type: string?`, `entity: string?`, `min_confidence: float?`, `workspace: string` | Filter claims |
| `get_relations` | `entity: string`, `workspace: string` | Graph traversal — all relations from entity |
| `compile` | `workspace: string` | Trigger full pipeline recompilation |
| `health_check` | `workspace: string` | Run verification, return health score |

Tools support `tools/list` and `tools/call` methods.

### MCP Error Codes

Standard JSON-RPC 2.0 error codes:
- `-32600` — Invalid request
- `-32601` — Method not found
- `-32602` — Invalid params
- `-32603` — Internal error

---

## 4. CLI: `root serve` Command

### Subcommand Definition

```
root serve [OPTIONS]

Options:
  --port <PORT>          Port to bind (default: 3000)
  --host <HOST>          Host to bind (default: 127.0.0.1)
  --api-key <KEY>        Optional API key for auth
  --path <PATH>          Workspace path (repeatable for multi-workspace)
  --mcp-stdio            Run as MCP stdio server (single workspace, no HTTP)
  --no-rest              Disable REST API (MCP only via HTTP/SSE)
  --no-mcp               Disable MCP endpoints (REST only)
```

### Behavior

- Default: `root serve --path .` → starts REST + MCP HTTP/SSE on localhost:3000
- `root serve --mcp-stdio --path ./repo` → stdio mode, no HTTP server
- `root serve --path ./repo1 --path ./repo2` → multi-workspace
- Workspace names derived from directory names (e.g., `./my-repo` → workspace `my-repo`). On name collision, append `-2`, `-3`, etc.
- Ctrl+C → graceful shutdown (finish in-flight requests, close connections)
- Banner printed on startup:

```
ThinkingRoot v0.1.0
  REST API:  http://127.0.0.1:3000/api/v1/
  MCP SSE:   http://127.0.0.1:3000/mcp/sse
  Workspaces: my-repo (142 entities, 387 claims)
  Auth:       API key required
```

---

## 5. Python SDK

Single PyPI package: `pip install thinkingroot`

### 5.1 Native Bindings (PyO3)

Compiled via maturin. Exposes Rust functions directly to Python.

**Module:** `thinkingroot` (native extension)

```python
import thinkingroot

# --- Full pipeline ---
result = thinkingroot.compile("./repo")
# result.files_parsed: int
# result.claims_count: int
# result.entities_count: int
# result.relations_count: int
# result.contradictions_count: int
# result.artifacts_count: int
# result.health_score: int (0-100)

# --- Individual stages ---
docs = thinkingroot.parse_directory("./repo")
# docs: list[DocumentIR]
# doc.uri, doc.source_type, doc.content_hash, doc.chunks

doc = thinkingroot.parse_file("./docs/arch.md")
# Single file parse

# --- Graph access (open existing .thinkingroot/) ---
engine = thinkingroot.open("./repo")

entities = engine.get_entities()
# list[dict] with id, name, type, claim_count

entity = engine.get_entity("PostgreSQL")
# dict with claims, relations, aliases

claims = engine.get_claims(type="Decision", min_confidence=0.8)
# list[dict] with id, statement, type, confidence, source_uri

relations = engine.get_relations("PaymentService")
# list[dict] with target, relation_type, strength

results = engine.search("payment processing", top_k=10)
# dict with entities: list, claims: list, each with relevance

health = engine.health()
# dict with overall, freshness, consistency, coverage, provenance, warnings

result = engine.verify()
# dict with health_score, stale_claims, contradictions, orphaned_claims, warnings

# --- Direct graph queries ---
all_relations = engine.get_all_relations()
sources = engine.get_sources()
contradictions = engine.get_contradictions()
```

**Error handling:** All functions raise `thinkingroot.ThinkingRootError` on failure with descriptive messages.

### 5.2 HTTP Client (Pure Python)

No Rust compilation required. Uses `httpx`.

**Module:** `thinkingroot.client`

```python
from thinkingroot import Client

client = Client("http://localhost:3000", api_key="optional-key")

# --- Query operations ---
workspaces = client.workspaces()
entities = client.entities(workspace="my-repo")
entity = client.entity("PostgreSQL", workspace="my-repo")
claims = client.claims(workspace="my-repo", type="Decision", min_confidence=0.8)
relations = client.relations("PaymentService", workspace="my-repo")
all_relations = client.all_relations(workspace="my-repo")
artifacts = client.artifacts(workspace="my-repo")
artifact = client.artifact("agent-brief", workspace="my-repo")
health = client.health(workspace="my-repo")
results = client.search("payment processing", workspace="my-repo", top_k=10)

# --- Actions ---
result = client.compile(workspace="my-repo")
result = client.verify(workspace="my-repo")
```

**Error handling:** Raises `thinkingroot.APIError` with status code + error message from server.

**Default workspace:** If only one workspace is mounted, the `workspace` parameter is optional.

### 5.3 Package Structure

```
thinkingroot-python/
  Cargo.toml              # PyO3 crate config
  pyproject.toml           # maturin + package metadata
  src/
    lib.rs                 # PyO3 bindings
  python/
    thinkingroot/
      __init__.py          # re-exports: compile, parse_directory, parse_file, open, Client
      client.py            # HTTP client class
      _thinkingroot.pyi    # type stubs for native module
```

**pyproject.toml:**
- Build backend: maturin
- Requires-python: >= 3.9
- Dependencies: httpx >= 0.27
- Dev dependencies: pytest, pytest-asyncio

---

## 6. File Layout

### Modified Crates

```
crates/thinkingroot-serve/
  Cargo.toml                  # add: axum, tower, tower-http, tokio-stream, serde_json + all stage crates
  src/
    lib.rs                    # pub mod engine, rest, mcp
    engine.rs                 # QueryEngine — shared query core
    rest.rs                   # Axum routes, middleware, JSON types
    mcp/
      mod.rs                  # MCP protocol types, JSON-RPC dispatch
      stdio.rs                # stdin/stdout transport
      sse.rs                  # HTTP/SSE transport
      resources.rs            # MCP resource handlers
      tools.rs                # MCP tool handlers

crates/thinkingroot-cli/
  Cargo.toml                  # add: thinkingroot-serve dependency
  src/
    main.rs                   # add Serve subcommand
    serve.rs                  # NEW: launch server logic
```

### New Crate

```
thinkingroot-python/
  Cargo.toml
  pyproject.toml
  src/lib.rs
  python/thinkingroot/__init__.py
  python/thinkingroot/client.py
  python/thinkingroot/_thinkingroot.pyi
```

### Dependency Changes

**thinkingroot-serve Cargo.toml:**
```toml
[dependencies]
thinkingroot-core = { workspace = true }
thinkingroot-graph = { workspace = true }
thinkingroot-parse = { workspace = true }
thinkingroot-extract = { workspace = true }
thinkingroot-link = { workspace = true }
thinkingroot-compile = { workspace = true }
thinkingroot-verify = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
chrono = { workspace = true }
axum = { workspace = true }
tower = { workspace = true }
tower-http = { workspace = true }
tokio-stream = "0.1"
anyhow = { workspace = true }

[dev-dependencies]
tempfile = "3"
reqwest = { version = "0.12", features = ["json"] }
```

**thinkingroot-cli Cargo.toml:** add `thinkingroot-serve = { workspace = true }` to dependencies.

**Workspace Cargo.toml:** add `thinkingroot-serve` to workspace dependencies, add `tokio-stream = "0.1"`.

**thinkingroot-python Cargo.toml:**
```toml
[package]
name = "thinkingroot-python"
version = "0.1.0"
edition = "2024"

[lib]
name = "_thinkingroot"
crate-type = ["cdylib"]

[dependencies]
thinkingroot-core = { path = "../crates/thinkingroot-core" }
thinkingroot-graph = { path = "../crates/thinkingroot-graph" }
thinkingroot-parse = { path = "../crates/thinkingroot-parse" }
thinkingroot-extract = { path = "../crates/thinkingroot-extract" }
thinkingroot-link = { path = "../crates/thinkingroot-link" }
thinkingroot-compile = { path = "../crates/thinkingroot-compile" }
thinkingroot-verify = { path = "../crates/thinkingroot-verify" }
thinkingroot-serve = { path = "../crates/thinkingroot-serve" }
pyo3 = { version = "0.23", features = ["extension-module"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

---

## 7. Build Order

| Step | What | Depends On | Milestone |
|------|------|-----------|-----------|
| 1 | Query Engine | Phase 1 crates | `QueryEngine` unit tests pass |
| 2 | REST API | Step 1 | `curl /api/v1/ws/test/entities` returns JSON |
| 3 | `root serve` command | Step 2 | `root serve --path ./repo` starts, REST works E2E |
| 4 | MCP stdio | Step 1 | Claude Code connects via `root serve --mcp-stdio` |
| 5 | MCP HTTP/SSE | Steps 2, 4 | Multiple agents connect to `/mcp/sse` |
| 6 | Python PyO3 | Phase 1 crates | `python -c "import thinkingroot; thinkingroot.compile('./repo')"` |
| 7 | Python HTTP client | Step 2 | `from thinkingroot import Client` works |
| 8 | Integration tests | All above | E2E: compile → serve → query via REST/MCP/Python |

---

## 8. Testing Strategy

### Unit Tests (per module)
- `engine.rs` — QueryEngine with in-memory graph, test all operations
- `rest.rs` — Axum test helpers (`axum::test`), verify routes + status codes + auth
- `mcp/` — JSON-RPC request/response serialization, tool dispatch
- Python — pytest for both native bindings and HTTP client

### Integration Tests
- **REST E2E:** compile test fixture → start server → hit every endpoint → verify responses
- **MCP stdio E2E:** spawn `root serve --mcp-stdio`, send JSON-RPC via pipe, verify responses
- **MCP SSE E2E:** start server, connect SSE client, invoke tools
- **Multi-workspace:** mount two workspaces, query each independently
- **Auth:** verify 401 without key, 200 with key, 401 with wrong key
- **Python native:** compile → open → query all graph methods
- **Python client:** start server → Client → hit every method

### Test Fixtures
Reuse existing `tests/fixtures/sample-repo/` from Phase 1.

---

## 9. Non-Goals (Phase 2)

- No user accounts or JWT auth (Phase 4)
- No write API for claims/entities (Phase 3 safety layer first)
- No webhook notifications (Phase 4)
- No dashboard UI (Phase 4)
- No real-time compilation watching / file system events (Phase 3+)
- No agent registry or quarantine pipeline (Phase 3)
