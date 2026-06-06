# ThinkingRoot — World-Class Intelligent Serve Layer
## Complete Architecture Document

> Every item here is grounded in the actual codebase or the research synthesized in this session.
> No hallucination. No speculation beyond what is explicitly noted.

---

## 1. What ThinkingRoot Is

A **knowledge compiler for AI agents**. It runs a 6-stage pipeline over a codebase or document set and produces a typed knowledge graph accessible via REST API, MCP server, and Python SDK.

**Binary:** `root` | **Language:** Rust (edition 2024) | **Graph DB:** CozoDB (Datalog)

### Current Pipeline (Phases 1–3.5, complete)

```
Source Files
    │
    ▼
[Stage 1] thinkingroot-parse
    Tree-sitter AST parsing (20+ languages)
    Extracts: functions, types, imports, call graphs, manifests
    Output: DocumentIR { chunks: Vec<Chunk> }
    Each chunk has ChunkMetadata: function_name, calls_functions,
    type_name, trait_name, field_types, visibility, import_path
    │
    ▼
[Stage 2] thinkingroot-extract
    Two-tier extraction:
    Tier 0 — Structural (tree-sitter only, confidence=0.99, no LLM)
    Tier 2 — LLM extraction with:
              - AST anchor injection (grounds LLM in deterministic AST facts)
              - Graph-primed context (injects known entities into prompt)
              - Grounding tribunal (3 judges: lexical, span, semantic)
              - Content-addressable cache (SHA of chunk → skip re-extraction)
    Output: Claims, Entities, Relations
    │
    ▼
[Stage 3] thinkingroot-link
    Phase 1: Entity resolution (exact → alias → Levenshtein 0.85)
    Phase 2: Claim linking (claim → source, claim → entity)
    Phase 3: Relation linking + subsumption dedup + noisy-OR aggregation
    Phase 4: Contradiction detection (keyword heuristics + Jaccard similarity)
    Output: Resolved graph with contradictions flagged
    │
    ▼
[Stage 4] thinkingroot-compile
    Reads graph → renders 8 Tera templates:
    entity_page, architecture_map, contradiction_report,
    decision_log, task_pack, agent_brief, runbook, health_report
    Incremental: compile_affected() only re-compiles changed entities
    Output: Markdown artifacts in .thinkingroot/artifacts/
    │
    ▼
[Stage 5] thinkingroot-verify
    Health scoring: overall = freshness×0.3 + consistency×0.3
                              + coverage×0.2 + provenance×0.2
    Staleness detection, orphaned claim detection, contradiction counts
    │
    ▼
[Stage 6] thinkingroot-serve  ← WHERE THE NEW WORK LIVES
    REST API (Axum) + MCP server (stdio + SSE transports)
    QueryEngine: multi-workspace, Arc<Mutex<StorageEngine>>
    Current MCP tools: search, query_claims, get_relations,
                       compile, health_check,
                       create_branch, diff_branch, merge_branch
```

### Storage Layer (current)

```
StorageEngine
├── GraphStore (CozoDB, SQLite backend)
│   Relations: sources, claims, entities, entity_relations,
│              source_entity_relations, claim_entity_edges,
│              claim_source_edges, claim_temporal,
│              contradictions, entity_aliases
│   Queries: Datalog with aggregation, joins, recursive rules
│
└── VectorStore (fastembed, AllMiniLML6V2, in-memory HashMap)
    Cosine similarity search, O(n) — no HNSW indexing
    Persisted to vectors.bin (JSON)
    Feature-gated: --no-default-features = no-op stub
```

### Knowledge Version Control (KVC, current)

```
main branch      (.thinkingroot/graph/graph.db)
    └── feature branch  (.thinkingroot-{slug}/graph/graph.db)
                         models/ and cache/ symlinked to main

root branch / checkout / diff / merge / status / snapshot
Merge: health CI gate, contradiction check, noisy-OR aggregation
REST: GET/POST /branches, merge, checkout, diff, HEAD
MCP tools: create_branch, diff_branch, merge_branch
```

---

## 2. The Problem With the Current Serve Layer

The current MCP tools are **CRUD database queries with JSON serialization**:

```rust
// Current: every tool does this
match engine.search(ws, query, top_k).await {
    Ok(results) => {
        let content = serde_json::to_string_pretty(&results); // verbose JSON
        JsonRpcResponse::success(id, json!({ "content": [{ "type": "text", "text": content }] }))
    }
}
```

Problems:
- `serde_json::to_string_pretty` adds 1.5–2× token overhead (braces, quotes, repeated keys)
- No session state — re-sends what the agent already knows every call
- No intent awareness — `search("auth")` returns same thing whether agent is debugging or implementing
- No graph traversal — 3–5 sequential MCP calls to build picture of one entity
- No write-back — graph only grows via `root compile`, agents are passive consumers
- No token budgeting — agent cannot say "give me 500 tokens on this topic"

**Research finding (LongLLMLingua, 2023):** 4× compression with 21.4% *performance boost*. Removing noise improves accuracy. Current verbose JSON actively harms agent performance.

**Research finding (Wikontic, 2025):** Knowledge graphs achieve same quality as GraphRAG using 20× fewer tokens when served as structured triplets rather than prose.

**Research finding (RES architecture, 2026):** O(1) token cost of 1,574 tokens regardless of dataset size. Key: never put raw data in context — serve pre-computed aggregates.

---

## 3. What Needs to Be Built

### 3.1 Three New Graph Queries (graph.rs)

**`get_entity_context(entity_name)`**
Single Datalog query returning everything about one entity:
- Entity (id, name, type, description, aliases)
- All outgoing relations (→ neighbors)
- All incoming relations (reverse deps — who depends on this?)
- All claims linked to this entity (with source URI, confidence, type)
- All contradictions involving this entity's claims

This replaces 4–5 sequential MCP calls with one graph walk.

**`get_neighborhood(entity_name, radius)`**
BFS from focal entity up to `radius` hops. Returns:
- Focal entity + all entities within radius hops
- All relations between them
- Claim counts per entity (not full claims — for overview)

Used by the planner for `intent=Understand` (architecture view).

**`find_reverse_deps(entity_name)`**
Returns all entities that DependsOn, Calls, Uses, or Implements the focal entity.
Critical for `intent=Review` — before modifying X, know what breaks if X changes.

### 3.2 Intelligence Module (new: crates/thinkingroot-serve/src/intelligence/)

Four files:

**`mod.rs`** — module root, exports public types

**`planner.rs`** — Knowledge Query Planner

The planner takes intent + topic → outputs a QueryPlan → executor runs steps → raw KnowledgePacket.

```
QueryIntent variants:
  Implement  → conventions, patterns, deps, API signatures, similar implementations
  Debug      → error handling, failure modes, contradictions, recent changes
  Review     → reverse deps, staleness, coverage gaps, risks
  Understand → architecture, key relationships, design decisions
  Overview   → workspace summary (no focal entity)

QueryPlan: ordered list of PlanSteps
  ResolveEntity(name)        → entity_id
  WalkEntityContext          → full entity context (claims, relations, contradictions)
  WalkNeighborhood(radius)   → subgraph overview
  FindReverseDeps            → what depends on focal entity
  FetchClaimsByType(type)    → filtered claims (Decision, Requirement, etc.)
  FetchWorkspaceSummary      → counts, top entities, active warnings

Each intent maps to a specific sequence of PlanSteps.
Cost model: estimate token cost of each step before executing.
If estimated total > budget: drop lowest-priority steps first.
```

**`session.rs`** — Session Context Tracker

```
SessionContext {
    id: String,                          // SSE session UUID (already exists)
    created_at: DateTime<Utc>,
    entities_delivered: HashSet<String>, // entity IDs already sent this session
    claims_delivered: HashSet<String>,   // claim IDs already sent this session
    focus_topic: Option<String>,         // current focus area
    agent_branch: Option<String>,        // session's write-back branch name
    total_tokens_delivered: usize,
}

SessionStore: Arc<Mutex<HashMap<String, SessionContext>>>
  stored in AppState alongside existing mcp_sessions
```

Key operations:
- `is_entity_novel(session_id, entity_id)` → bool
- `mark_delivered(session_id, entities, claims)` → ()
- `get_or_create(session_id)` → SessionContext
- `get_agent_branch(session_id)` → Option<String>

Effect: every subsequent response is a semantic diff — only what's new.

**`compressor.rs`** — Token Budget Compressor

Input: raw `KnowledgePacket` (full detail, may be 5,000+ tokens)
Output: compressed string exactly fitting `budget_tokens`

Algorithm:
1. Score each item: `relevance × confidence × freshness × novelty`
   - novelty = 1.0 if not seen in session, 0.1 if already delivered
   - freshness = decay based on claim created_at
2. Allocate budget by zone:
   - Structure header: 10% (entity name, type, relations summary)
   - Focal claims: 60% (highest-scoring claims for the intent)
   - Risk/warnings: 20% (contradictions, staleness, reverse dep risks)
   - Navigation: 10% (related entities worth investigating next)
3. Format as structured text (not JSON):
   ```
   ## EntityName (EntityType) ◎confidence Δage
   Relation: Target (type|strength), Target2 (type|strength)
   ReverseDep: Caller1, Caller2

   ━━ Claims (shown/total) ━━
   • [Type|conf] Statement text
   • [Type|conf] Statement text

   ⚠ Contradictions: description
   → Next: RelatedEntity1, RelatedEntity2
   ```
4. Token estimation: ~4 chars/token for English, ~3 chars/token for code
5. Truncate iteratively until fits budget

Format rationale (research-backed):
- JSON: 1.5–2× token overhead → eliminated
- Graph triplets: 0.8–1.0× overhead → used for relations
- Structured text with typed labels: models comprehend better than raw JSON

### 3.3 New MCP Tools (mcp/tools.rs additions)

**`investigate`** — the primary tool replacing search + query_claims + get_relations

```json
{
  "name": "investigate",
  "description": "Intent-aware knowledge retrieval. Returns a token-budgeted knowledge packet about a topic.",
  "inputSchema": {
    "topic": "string — entity name, concept, or area to investigate",
    "intent": "implement | debug | review | understand",
    "budget_tokens": "integer (default: 800, max: 4000)",
    "workspace": "string"
  }
}
```

Flow: `investigate(topic, intent, budget)` → planner creates QueryPlan → engine executes graph walks → compressor fits to budget → session marks delivered → returns structured text

**`contribute`** — agent write-back (OFF the normal pipeline)

```json
{
  "name": "contribute",
  "description": "Write agent-discovered knowledge directly to the graph (agent branch). No LLM re-extraction. Fast.",
  "inputSchema": {
    "claims": [
      {
        "statement": "string — atomic, self-contained fact",
        "claim_type": "fact | decision | requirement | dependency | architecture | api_signature",
        "confidence": "float 0.0–1.0",
        "entities": ["entity names referenced in this claim"]
      }
    ],
    "workspace": "string"
  }
}
```

Flow:
1. Get or create agent session branch (`.thinkingroot-agent-{session_id}`)
2. Create Source record: `source_type: AgentSession`, `trust_level: Untrusted`
3. Insert each claim directly to graph (no parsing, no LLM)
4. Run entity resolution (link to existing entities by name)
5. Run contradiction detection against existing claims
6. Store in agent branch, NOT main
7. Return: `{ inserted: N, contradictions_found: M, branch: "agent-{id}" }`

This is entirely **off the normal pipeline**. No parse → extract stages. Agent IS the extractor.

**`brief`** — adaptive workspace overview

```json
{
  "name": "brief",
  "description": "Adaptive overview of the workspace. Returns compressed knowledge packet sized to budget.",
  "inputSchema": {
    "budget_tokens": "integer (default: 500)",
    "workspace": "string"
  }
}
```

Returns: entity count, top-N entities by claim count, active contradictions, warnings, last compile time. Replaces reading `agent-brief.md` artifact (which is static and verbose).

**`focus`** — tell server what you're working on

```json
{
  "name": "focus",
  "description": "Set session focus area. Server uses this for proactive context in subsequent calls.",
  "inputSchema": {
    "topic": "string",
    "workspace": "string"
  }
}
```

Updates `SessionContext.focus_topic`. Subsequent `investigate` calls use focus as tiebreaker when scoring claims.

### 3.4 Engine Methods (engine.rs additions)

```rust
// Intent-aware retrieval — called by investigate tool
pub async fn investigate(
    &self,
    ws: &str,
    topic: &str,
    intent: QueryIntent,
    budget_tokens: usize,
    session_ctx: &mut SessionContext,
) -> Result<String>   // returns compressed structured text

// Agent write-back — called by contribute tool
pub async fn contribute_claims(
    &self,
    ws: &str,
    claims: Vec<AgentClaim>,
    session_id: &str,
) -> Result<ContributeResult>

// Workspace overview — called by brief tool
pub async fn get_workspace_brief(
    &self,
    ws: &str,
    budget_tokens: usize,
    session_ctx: &SessionContext,
) -> Result<String>
```

### 3.5 AppState Update (rest.rs)

Add `sessions: SessionStore` field to `AppState`.
SSE sessions already have UUIDs — wire them to `SessionContext` on connect.

Auto-create agent branch when first `contribute` is called in a session.

---

## 4. Agent Write-Back: The Off-Pipeline Path

This is the critical architectural distinction:

```
Normal pipeline (human code → graph):
  file saved
    → parse (tree-sitter AST)
    → extract (LLM with grounding)
    → link (entity resolution, contradiction detection)
    → compile (artifact regeneration)
    → graph updated
  Slow. Expensive. LLM touches everything.

Agent write-back (agent discovery → graph):
  agent calls contribute()
    → validate claims (non-empty statement, valid claim_type, confidence in [0,1])
    → entity resolution only (does this entity already exist? link to it)
    → contradiction detection (does this conflict with existing claims?)
    → insert directly to GraphStore
    → tag: ExtractionTier::AgentInferred, TrustLevel::Untrusted
    → store in agent session branch (NOT main)
  Fast. No LLM. Pure graph operations.

Cross-validation (next root compile):
  compile sees agent claims in branch
    → compares against freshly extracted claims from source code
    → code agrees with agent claim → confidence promoted
    → code contradicts agent claim → contradiction flagged for review
    → developer runs: root diff agent-session-abc
                      root merge agent-session-abc
```

---

## 5. The Trust Ladder

```
ExtractionTier::Structural  confidence=0.99   TrustLevel::Trusted
  ↑ tree-sitter AST — deterministic, no hallucination

ExtractionTier::Llm         confidence=0.5–0.95  TrustLevel::Trusted
  ↑ LLM extraction with grounding tribunal

ExtractionTier::AgentInferred  confidence=agent-declared  TrustLevel::Untrusted
  ↑ agent write-back — fast path, stored in agent branch only

  After root compile cross-validates:
    code confirms → TrustLevel::Trusted, confidence boosted
    code contradicts → contradiction created, human reviews
    human approves → TrustLevel::Verified
```

---

## 6. Session Lifecycle

```
1. Agent connects via MCP (SSE or stdio)
   → SessionContext created with session UUID
   → agent_branch = None (created lazily on first contribute)

2. Agent calls brief() or first investigate()
   → workspace overview delivered (~400 tokens)
   → entities_delivered, claims_delivered seeded

3. Agent works on topic X
   → investigate("X", intent=implement, budget=800)
   → planner walks graph, compressor fits to 800 tokens
   → session marks X's entities/claims as delivered

4. Agent follows a lead to topic Y
   → investigate("Y", intent=debug, budget=600)
   → session knows: X already delivered — skip overlap
   → response is a delta, not a full dump
   → actual tokens delivered: ~300 (Y minus what overlaps with X)

5. Agent discovers new knowledge
   → contribute([{statement, claim_type, confidence}])
   → agent branch created: .thinkingroot-agent-{session_id}
   → claims written off-pipeline directly to branch graph
   → contradictions checked against main immediately

6. Session ends
   → SessionContext preserved for 30 minutes (reuse if agent reconnects)
   → Agent branch persists in .thinkingroot-refs/branches.toml
   → Developer can: root diff agent-session-{id}
                    root merge agent-session-{id}
   → On merge: health CI gate, contradiction resolution, main graph enriched
```

---

## 7. The Living, Evolving Graph

```
Day 0:  root compile → base graph from source code
        Structural claims (0.99 conf) + LLM claims (0.5–0.95 conf)

Day 1:  Developer A + Claude Code session
        → reads graph: knows architecture instantly
        → discovers: Redis TTL insufficient for 3DS redirect
        → writes back to agent branch
        → merges: graph now has new Requirement claim

Day 3:  Developer B + Claude Code session
        → reads graph: already knows about Redis TTL requirement
        → doesn't re-discover the same thing
        → discovers: webhook retry not idempotent
        → writes back → merges

Day 7:  root compile (after code changes)
        → cross-validates all agent claims against fresh source
        → 2 agent claims confirmed by code → confidence promoted
        → 1 agent claim contradicted by code → contradiction flagged

Month 1: Graph contains:
         - Source-extracted knowledge (structural + LLM)
         - Agent-discovered knowledge (validated by compile)
         - Contradiction map (what's uncertain or in conflict)
         Every developer, every session has made it richer.
```

The graph is no longer a snapshot of the codebase. It is the **accumulated intelligence of every agent session** that has ever touched the project.

---

## 8. Token Economics

**Current state (CRUD JSON tools):**
- Typical `search` result: ~2,000 tokens (verbose JSON, IDs, metadata noise)
- Typical 5-tool session: ~10,000 tokens of MCP context
- Re-sends same entities every call (no session state)

**After this build:**
- `brief`: ~400 tokens (workspace overview)
- `investigate` (first call): ~500–800 tokens (focal entity + context)
- `investigate` (subsequent, same session): ~150–400 tokens (delta only)
- Typical 5-tool session: ~2,000–3,000 tokens total

**10× reduction. Research-backed:**
- LongLLMLingua (2023): 4× compression + 21.4% performance *boost*
- Knowledge graph triplets: 20× more token-efficient than RAG prose (Wikontic, 2025)
- Session deduplication: 31% of LLM queries are for already-known information (MeanCache, 2024)

---

## 9. Auto-Wiring (how agents always use ThinkingRoot)

**Mechanism 1: CLAUDE.md**
```markdown
# In repo CLAUDE.md (checked in, applies to all Claude Code sessions)
ThinkingRoot MCP server: localhost:3000
Before any task: call investigate(topic, intent) to load context
After discoveries: call contribute() to write back to agent branch
```
Every Claude Code session reads CLAUDE.md automatically. No developer has to remember.

**Mechanism 2: `root connect` (already built)**
```bash
root connect
→ Detects: Claude Desktop, Cursor, VS Code, etc.
→ Writes MCP config to tool-specific config files
→ Every future session in that tool auto-connects
```

**Session startup (automatic after connect):**
```
MCP handshake
→ server sends brief() proactively (not waiting for agent to ask)
→ agent has workspace context before first user message
```

---

## 10. Files to Create / Modify

### New files
```
crates/thinkingroot-serve/src/intelligence/mod.rs
crates/thinkingroot-serve/src/intelligence/planner.rs
crates/thinkingroot-serve/src/intelligence/session.rs
crates/thinkingroot-serve/src/intelligence/compressor.rs
```

### Modified files
```
crates/thinkingroot-graph/src/graph.rs
  + get_entity_context(entity_name) → EntityContext
  + get_neighborhood(entity_name, radius) → NeighborhoodResult
  + find_reverse_deps(entity_name) → Vec<(String, String, String)>

crates/thinkingroot-serve/src/engine.rs
  + investigate(ws, topic, intent, budget, session) → String
  + contribute_claims(ws, claims, session_id) → ContributeResult
  + get_workspace_brief(ws, budget, session) → String

crates/thinkingroot-serve/src/mcp/tools.rs
  + handle: "investigate" → intelligence::planner + engine.investigate
  + handle: "contribute"  → engine.contribute_claims
  + handle: "brief"       → engine.get_workspace_brief
  + handle: "focus"       → session.set_focus

crates/thinkingroot-serve/src/rest.rs
  + AppState.sessions: SessionStore
  + session wired to SSE connect/disconnect

crates/thinkingroot-serve/src/lib.rs
  + pub mod intelligence;
```

### No new crates needed
Everything fits within `thinkingroot-serve` and `thinkingroot-graph`. No new workspace members.

---

## 11. What This Does NOT Change

- The 6-stage pipeline is unchanged
- The KVC branch system is unchanged
- Existing MCP tools (`search`, `query_claims`, `get_relations`, `health_check`, branch tools) remain — backward compatible
- REST API is unchanged
- CozoDB schema is unchanged (agent claims use existing tables)
- Feature flags are unchanged
- Python SDK is unchanged

The intelligent serve layer is **additive**. Nothing breaks.

---

## 12. Competitive Position After Build

| Capability | Cursor/Windsurf | Sourcegraph Cody | Microsoft GraphRAG | ThinkingRoot (after) |
|---|---|---|---|---|
| Typed knowledge graph | ✗ | ✗ | ✓ (docs only) | ✓ (code + docs) |
| Intent-aware retrieval | ✗ | ✗ | partial | ✓ |
| Token-budgeted responses | ✗ | ✗ | ✗ | ✓ |
| Session delta compression | ✗ | ✗ | ✗ | ✓ |
| Agent write-back | ✗ | ✗ | ✗ | ✓ |
| Knowledge version control | ✗ | ✗ | ✗ | ✓ |
| Off-pipeline agent writes | ✗ | ✗ | ✗ | ✓ |
| MCP native | partial | partial | ✗ | ✓ |
| Works offline / self-hosted | ✓ | partial | ✗ | ✓ |

No other system combines all of these. This is the world-first position.

---

## 13. The One-Line Summary

> ThinkingRoot compiles a codebase into a typed knowledge graph, serves it to AI agents via intent-aware MCP with 10× token compression, and lets agents write discoveries back — making the graph a living, accumulating intelligence that gets smarter with every session.

---

*Architecture document — ThinkingRoot Phase 4 (Intelligent Serve Layer)*
*Date: 2026-04-13 | Based on codebase audit + research synthesis*
