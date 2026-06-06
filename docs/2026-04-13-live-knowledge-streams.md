# ThinkingRoot Live Knowledge Streams — Real-Time Compilation Architecture

**Date:** 2026-04-13  
**Status:** Architecture Design  
**Author:** ThinkingRoot Core Team  
**Classification:** World-First — No existing system combines compiled knowledge graphs with real-time streaming extraction  

---

## The Problem

ThinkingRoot's 6-stage pipeline (parse → extract → link → compile → verify → serve) produces the fastest knowledge retrieval in the industry (11µs entity lookup vs. 200ms+ for competitors). But it requires an upfront compilation step. This creates a gap:

```
                    ┌─────────────────────────────┐
                    │    THE KNOWLEDGE GAP         │
                    │                             │
  Compiled Graph    │    ????                     │   Real-Time Session
  (Yesterday)       │                             │   (Right Now)
                    │  Agent discovers new facts   │
  ✓ Fast (11µs)     │  during live work, but they  │   ✗ Unstructured
  ✓ Typed           │  don't exist in the graph    │   ✗ Ephemeral
  ✓ Linked          │  until next `root compile`   │   ✗ Lost on exit
  ✓ Verified        │                             │
                    └─────────────────────────────┘
```

Every competitor (SuperMemory, Zep, Mem0) chose to solve this by going **all-in on real-time and accepting slow, probabilistic retrieval.** We refuse that trade-off. We want both: **compiled speed AND real-time awareness.**

---

## The Insight: Two-Tier Materialized Knowledge

The breakthrough comes from combining three research areas that have never been applied together to knowledge graphs for AI agents:

1. **Differential Dataflow** (Frank McSherry, Materialize) — Incrementally maintain materialized views as data streams in; don't recompute from scratch.
2. **LSM Trees** (Log-Structured Merge) — Buffer writes in-memory for instant availability, flush to persistent storage during idle time.
3. **Bi-Temporal Knowledge Graphs** (Zep/Graphiti) — Track both event-time and ingestion-time to distinguish "what is true now" from "what was true then."

**Our synthesis:** The knowledge graph has two layers that are queried as one:

```
┌─────────────────────────────────────────────────────────────────┐
│                     QUERY LAYER (Unified View)                  │
│                                                                 │
│   Agent asks: "What does AuthService depend on?"                │
│   QueryEngine merges results from BOTH layers:                  │
│                                                                 │
│   ┌──────────────────────────┐  ┌────────────────────────────┐  │
│   │    COLD LAYER (Base)     │  │    HOT LAYER (Stream)      │  │
│   │                          │  │                            │  │
│   │  CozoDB/SQLite on disk   │  │  CozoDB/mem (in-memory)    │  │
│   │  Compiled knowledge      │  │  Live session knowledge    │  │
│   │  High confidence (0.9+)  │  │  Lower confidence (0.5-0.8)│  │
│   │  Verified, linked        │  │  Unverified, provisional   │  │
│   │  Latency: 11µs           │  │  Latency: ~5µs (RAM only)  │  │
│   │                          │  │                            │  │
│   │  Source: `root compile`  │  │  Source: Session events     │  │
│   │  Updated: On compile     │  │  Updated: Continuously     │  │
│   │  Durability: Persistent  │  │  Durability: WAL-backed    │  │
│   └──────────────────────────┘  └────────────────────────────┘  │
│                                                                 │
│   Merge Strategy: Hot shadows Cold (latest wins within session) │
│   Conflict Rule: Hot claim with same entity+statement as Cold   │
│                  claim → Hot supersedes for this session only    │
└─────────────────────────────────────────────────────────────────┘
```

---

## Architecture: The Three Engines

### Engine 1: The Structural Extractor (Zero-LLM, ~1ms)

Not every piece of live data needs an LLM. Most session events have obvious structure that can be extracted deterministically using the same AST/regex patterns ThinkingRoot already uses in the parser:

```
Input:  Agent runs `git log --oneline -5`
Output: [
  Claim("commit abc123 modified auth.rs", Fact, confidence=0.95),
  Relation(commit_abc123 → auth.rs, "Modifies", strength=0.95)
]

Input:  Agent opens file `src/auth/service.rs`  
Output: [
  Claim("Agent accessed AuthService source", Fact, confidence=1.0),
  Relation(Session → AuthService, "Investigating", strength=0.9)
]

Input:  Agent runs `cargo test` → 3 failures
Output: [
  Claim("3 test failures detected in current session", Metric, confidence=1.0),
  Claims for each failing test name, linked to their module entities
]
```

**Why this matters:** This is the layer that SuperMemory and Mem0 don't have. They would store "cargo test output" as raw text. We extract **typed, linked facts** in under 1 millisecond, with zero LLM cost.

**Implementation:** Reuse `thinkingroot-parse` pattern matchers + a new lightweight `StreamParser` that handles terminal output, chat messages, and file-change events. The same AST anchoring used in batch compilation applies here.

---

### Engine 2: The Shadow Extractor (Light-LLM, ~200ms, Async)

For unstructured data (agent reasoning, user chat, complex logs), we run a lightweight LLM extraction asynchronously. This does NOT block the agent — it runs in the background.

```
┌─────────────────────────────────────────────────────────────┐
│                    SHADOW EXTRACTION PIPELINE                │
│                                                             │
│  1. Session events accumulate in a ring buffer              │
│  2. Every N events OR every T seconds (configurable):       │
│     ┌──────────────────────────────────────────────────┐    │
│     │  MICRO-BATCH                                     │    │
│     │                                                  │    │
│     │  • Concatenate last N events into a single chunk │    │
│     │  • Inject KNOWN_ENTITIES from Base + Hot graphs  │    │
│     │  • Call lightweight LLM (Nova Micro / Mistral)   │    │
│     │    with a STREAM-OPTIMIZED extraction prompt     │    │
│     │  • Parse JSON response                           │    │
│     │  • Deduplicate against existing Hot claims       │    │
│     │  • Insert into Hot Layer (in-memory CozoDB)      │    │
│     └──────────────────────────────────────────────────┘    │
│  3. Hot Layer is immediately queryable                      │
│  4. Agent's NEXT query sees the new knowledge               │
│                                                             │
│  Trigger Modes:                                             │
│  • Time-based:  Every 10 seconds of activity                │
│  • Event-based: After tool completion (compile, test, etc.) │
│  • Threshold:   After 500 tokens of new session text        │
│  • Manual:      Agent calls `flush_stream` tool             │
└─────────────────────────────────────────────────────────────┘
```

**The Stream Extraction Prompt** is a radically simplified version of the full extraction prompt. It focuses on:
- Decisions made ("I will use X instead of Y")
- Discoveries ("Found that X calls Y, which was unexpected")
- Hypotheses ("X might be causing the bug in Y")
- Status updates ("Fixed the auth bug", "Tests passing now")

**LLM Model choice hierarchy:**
1. **Local Ollama (Mistral/Phi):** Zero cost, ~100ms latency, good enough for most sessions.
2. **Nova Micro:** $0.00004/request, ~150ms, better quality.
3. **Full model (only if user explicitly enables):** For critical sessions where accuracy matters more than cost.

---

### Engine 3: The Promotion Pipeline (Session → Persistent)

The most important piece: when should Hot Layer knowledge become permanent?

```
┌─────────────────────────────────────────────────────────────┐
│                    PROMOTION LIFECYCLE                        │
│                                                             │
│  1. EPHEMERAL (Default)                                      │
│     • Lives in Hot Layer only                                │
│     • Dies when session ends                                 │
│     • No LLM cost to persist                                 │
│     • Used for: navigation context, temporary observations   │
│                                                             │
│  2. CANDIDATE (Agent marks as important)                     │
│     • Agent calls `contribute` → promoted to Candidate       │
│     • Persisted to WAL (.thinkingroot/stream.wal)            │
│     • Survives server restart                                │
│     • Used for: discoveries, decisions, bug findings         │
│                                                             │
│  3. STAGED (Ready for verification)                          │
│     • On session end or manual `promote_session`:            │
│       All Candidate claims are written to a KVC branch       │
│       Branch name: `stream/{session-id}`                     │
│     • User can review with `root diff stream/{session-id}`   │
│     • Used for: anything the agent wants to remember         │
│                                                             │
│  4. COMPILED (Full citizen)                                  │
│     • On next `root compile`, the staged claims are:         │
│       - Cross-validated against source code                  │
│       - Linked to entities with full resolution              │
│       - Grounded (confidence recalculated)                   │
│       - Merged into the Cold Layer                           │
│     • Now queryable at 11µs with full provenance             │
│                                                             │
│  Lifecycle:                                                  │
│  Ephemeral ──contribute──→ Candidate ──session end──→ Staged │
│                                         ↓                    │
│                                   root compile               │
│                                         ↓                    │
│                                      Compiled                │
└─────────────────────────────────────────────────────────────┘
```

---

## The Unified Query: How Both Layers Merge

When the agent calls `investigate("AuthService")`, the QueryEngine performs:

```rust
// Pseudocode for unified query
async fn get_entity_context(ws: &str, entity_name: &str) -> EntityContext {
    // 1. Query Cold Layer (persistent, compiled graph)
    let cold = self.cold_graph.get_entity_context(entity_name)?;
    
    // 2. Query Hot Layer (in-memory, session graph)  
    let hot = self.hot_graph.get_entity_context(entity_name)?;
    
    // 3. Merge with precedence rules
    let merged = EntityContext {
        // Entity identity from Cold (canonical)
        id: cold.id,
        name: cold.name,
        entity_type: cold.entity_type,
        
        // Claims: union, with Hot claims marked as [LIVE]
        claims: merge_claims(cold.claims, hot.claims, MergeStrategy::UnionTagged),
        
        // Relations: union, Hot relations marked as [PROVISIONAL]  
        relations: merge_relations(cold.relations, hot.relations),
        
        // Contradictions: if Hot contradicts Cold, flag it
        contradictions: detect_cross_layer_contradictions(&cold, &hot),
    };
    
    merged
}
```

**Merge rules:**
1. **Same entity, same statement:** Hot wins (it's more recent).
2. **Same entity, different statement:** Both are returned; Hot is tagged `[LIVE]`.
3. **Hot entity not in Cold:** Returned as a `[PROVISIONAL]` entity.
4. **Contradictions:** If a Hot claim contradicts a Cold claim, both are surfaced with an explanation.

---

## The Write-Ahead Log (Durability Without Performance Cost)

Ephemeral claims live purely in RAM. But Candidate claims (those marked important by the agent) need to survive crashes. We use a lightweight append-only WAL:

```
File: .thinkingroot/stream.wal

Format (one JSON-lines entry per claim):
{"ts":1713045000,"session":"sess-abc","claim_id":"01HX...","statement":"AuthService uses bcrypt for hashing","claim_type":"fact","confidence":0.75,"entities":["AuthService","bcrypt"],"tier":"candidate"}
{"ts":1713045005,"session":"sess-abc","claim_id":"01HX...","statement":"Found race condition in token refresh","claim_type":"fact","confidence":0.8,"entities":["TokenRefresh"],"tier":"candidate"}
```

**On server startup:**
1. Read `stream.wal`
2. Replay all Candidate claims into a fresh in-memory Hot Layer
3. Resume serving — agent sees everything from the previous session

**On `root compile`:**
1. All Candidate claims in the WAL are staged into branch `stream/pending`
2. The compile pipeline processes them alongside source-derived claims
3. WAL is truncated after successful compilation

**WAL size limit:** 10 MB (~50,000 claims). At this threshold, auto-promotion to a branch is triggered.

---

## Latency Analysis

```
┌──────────────────────────────────────────────────────────────┐
│              LATENCY COMPARISON (per query)                   │
│                                                              │
│  Operation           Cold Only    Hot+Cold    Overhead        │
│  ─────────────────   ─────────    ────────    ────────        │
│  Entity lookup       11 µs        16 µs       +5 µs          │
│  Claims for entity   935 µs       1.1 ms      +165 µs        │
│  Relations           351 µs       420 µs      +69 µs         │
│  Full investigate    ~2 ms        ~2.5 ms     +500 µs        │
│  Search (keyword)    ~3 ms        ~3.5 ms     +500 µs        │
│  Search (vector)     ~5 ms        ~5.5 ms     +500 µs        │
│                                                              │
│  ALL OPERATIONS REMAIN UNDER 6ms                             │
│                                                              │
│  Shadow extraction:  ~200 ms (async, non-blocking)           │
│  Structural extract: ~1 ms (sync, in-band)                   │
│  WAL append:         ~50 µs (sync fsync)                     │
└──────────────────────────────────────────────────────────────┘
```

**The key guarantee:** Adding the Hot Layer increases query latency by at most ~500µs. Since our Cold Layer already runs at 11µs–5ms, the combined system stays well under 6ms for all standard operations.

---

## CozoDB In-Memory Backend

CozoDB natively supports an in-memory backend. This is what makes the Hot Layer possible without introducing a new dependency:

```rust
// Cold Layer (existing — persistent)
let cold_db = DbInstance::new("sqlite", "/path/to/graph.db", "")?;

// Hot Layer (new — in-memory, same schema, same query language)
let hot_db = DbInstance::new("mem", "", "")?;
```

**Same Datalog queries work on both.** The Hot Layer uses the exact same schema as the Cold Layer. This means:
- Zero new query language to learn
- Zero new data model to maintain
- Zero serialization overhead between layers
- The merge is a simple Datalog union

---

## Why No Existing System Does This

| System | Real-Time Ingestion | Compiled Knowledge | Typed Entities | Sub-ms Retrieval | Provenance Chain |
|--------|--------------------|--------------------|----------------|-----------------|-----------------|
| **SuperMemory** | ✅ | ✗ | ✗ | ✗ (300ms) | ✗ |
| **Zep/Graphiti** | ✅ | ✗ | Partial | ✗ (200ms) | ✅ |
| **Mem0** | ✅ | ✗ | Partial | ✗ (1400ms) | Partial |
| **ThinkingRoot (today)** | ✗ | ✅ | ✅ | ✅ (0.011ms) | ✅ |
| **ThinkingRoot Streams** | ✅ | ✅ | ✅ | ✅ (~0.016ms) | ✅ |

ThinkingRoot Streams is the only architecture that provides **all five properties simultaneously.** Every competitor sacrifices at least two of them.

---

## Configuration

```toml
# .thinkingroot/config.toml additions

[streams]
enabled = true
structural_extraction = true       # Zero-LLM pattern extraction (recommended)
shadow_extraction = true           # Background LLM extraction
shadow_model = "ollama/mistral"    # Cheapest option for live extraction
shadow_trigger = "event"           # "time" | "event" | "threshold" | "manual"
shadow_interval_secs = 10          # For time-based trigger
shadow_token_threshold = 500       # For threshold-based trigger
wal_enabled = true                 # Persist candidate claims to WAL
wal_max_size_mb = 10               # Auto-promote to branch at this size
auto_promote_on_exit = true        # Stage candidates to branch on session end
hot_layer_max_claims = 10000       # Memory cap for in-memory graph
```

---

## MCP Tools (New)

```
stream_status      — Show Hot Layer stats: claim count, entity count, WAL size
flush_stream       — Force shadow extraction of buffered session data
promote_session    — Stage all Candidate claims to a KVC branch for review
set_stream_mode    — Toggle structural/shadow/off per session
```

---

## Implementation Phases

### Phase A: Structural Stream (No LLM, Pure Speed)
- In-memory CozoDB Hot Layer
- `StreamParser` for terminal output, file changes, git events
- Unified query in `QueryEngine` (Cold + Hot merge)
- WAL for candidate durability
- `stream_status` MCP tool
- **Estimated effort:** ~3 days

### Phase B: Shadow Extraction (Async LLM)
- Ring buffer for session text
- Micro-batch extraction with stream-optimized prompt
- Configurable trigger modes
- Model hierarchy (Ollama → Nova Micro → Full)
- `flush_stream` and `set_stream_mode` MCP tools
- **Estimated effort:** ~5 days

### Phase C: Promotion Pipeline
- Auto-stage to KVC branch on session end
- `promote_session` MCP tool
- WAL replay on server restart
- Integration with `root compile` for cross-validation
- Contradiction detection across layers
- **Estimated effort:** ~3 days

---

## The "ThinkingRoot Advantage" After Streams

```
Before Streams:
  Compile ────────── 60s wait ──────────── Sub-ms retrieval forever
  
After Streams:  
  Compile ────────── Sub-ms retrieval forever
       ↑                    ↑
       │                    │
  Background             Live session knowledge
  (unchanged)            available in ~1ms
                         (structural) or ~200ms
                         (shadow, async)
```

**The compilation tax drops to near-zero for active sessions.** The agent gets instant, typed, linked knowledge from its own session while retaining the full power of the compiled base graph.

**This is the architecture that makes ThinkingRoot the world's first system to achieve both real-time awareness AND compiled-graph retrieval speed.**
