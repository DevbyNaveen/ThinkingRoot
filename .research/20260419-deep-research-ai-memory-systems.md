# Deep Research: AI Memory Systems, Knowledge Graphs, and Neuroscience-Inspired Computing
**Date:** 2026-04-19
**Scope:** State of the art as of early 2026, based on published papers, shipped specs, and real codebases

---

## 1. MAGMA (arXiv:2601.03236) -- Multi-Graph Agentic Memory Architecture

**Status:** Published Jan 6, 2026. Revised Apr 16, 2026. **Accepted at ACL 2026 Main Conference.**

**Authors:** Dongming Jiang, Yi Li, Guanpeng Li, Bingzhe Li

**What it actually proposes:** MAGMA represents each memory item across four orthogonal graph structures:
1. **Semantic graph** -- meaning/topic relationships
2. **Temporal graph** -- time-based ordering and proximity
3. **Causal graph** -- cause-effect chains
4. **Entity graph** -- who/what relationships

The key insight is that existing Memory-Augmented Generation (MAG) systems conflate temporal, causal, and entity information into a single unified semantic store. MAGMA decouples memory representation from retrieval logic and formulates retrieval as **policy-guided traversal** over these relational views, enabling query-adaptive selection and structured context construction.

**Benchmarks:** LoCoMo and LongMemEval. The abstract claims "consistently outperforms state-of-the-art agentic memory systems" but specific numbers require the full paper PDF (not available via abstract page).

**Assessment:** This is a real, peer-reviewed contribution at a top NLP venue. The four-graph decomposition is architecturally sound and directly relevant to ThinkingRoot's approach. However, the "policy-guided traversal" mechanism's actual implementation details need the full paper.

---

## 2. Predictive Coding / Active Inference in Knowledge Systems

**Bottom line: Nobody has done this yet for knowledge graphs.**

Extensive arXiv search for:
- "active inference" + "knowledge graph" -- **zero results**
- "free energy principle" + "knowledge" + "retrieval" -- **zero results**
- "predictive coding" + "knowledge management" -- **zero results**

**What does exist:**

- **Fountas et al. (2603.04688), "Why the Brain Consolidates: Predictive Forgetting for Optimal Generalisation"** (March 2026) -- Proposes that biological consolidation uses "predictive forgetting" to selectively retain information that predicts future outcomes. Validated on autoencoders, predictive coding networks, and Transformers. Shows "outcome-conditioned compression optimises the retention-generalisation trade-off." This is the closest thing to predictive coding for knowledge, but it applies to neural network weights, not knowledge graphs.

- Karl Friston's active inference framework remains confined to motor control, perception, and planning in robotics/neuroscience. **No one has applied it to knowledge graph pre-computation or predictive retrieval.**

**Opportunity:** A knowledge graph that uses prediction error to pre-compute likely queries is genuinely novel. There is no published work in this space.

---

## 3. Hierarchical Temporal Memory (HTM) / Numenta / Thousand Brains Project

**Status:** Numenta pivoted from HTM to the **Thousand Brains Project** in 2024-2025.

- The Thousand Brains Project is now a **non-profit** (as of 2025), partially funded by the Gates Foundation
- The technical implementation is called **"Monty"** (named after Vernon Mountcastle), available at github.com/thousandbrainsproject/tbp.monty
- **Explicitly "not production-ready code"** -- early beta, under active development
- Core principles: sensorimotor learning, reference frames, modularity
- Directed by Viviane Clay

**HTM's connection to knowledge graphs:** None. Nobody has applied HTM principles to knowledge graphs. The Thousand Brains Project focuses on sensorimotor learning in simulated environments, not information retrieval or knowledge management.

**Has anyone applied HTM to KGs?** No published work found. HTM's sequence learning and temporal patterns are theoretically compatible with temporal KG reasoning, but this remains unexplored territory.

---

## 4. Memory Consolidation in AI

**This is now an active research area with multiple 2025-2026 papers. Real systems exist.**

### Key Papers:

**a) SleepGate (2603.14517) -- March 2026**
- Author: Ying Xie
- "Learning to Forget: Sleep-Inspired Memory Consolidation for Resolving Proactive Interference in LLMs"
- Proposes periodic "sleep micro-cycles" over the KV cache with: conflict-aware temporal tagger, forgetting gate, and consolidation module that merges surviving entries into compact summaries
- **Results:** 99.5% retrieval accuracy at interference depth 5, 97% at depth 10. All baselines (full KV cache, sliding window, H2O, StreamingLLM) below 18%
- Claims O(log n) interference horizon vs O(n)
- **Caveat:** Tested on tiny 4-layer, 793K parameter model. Unclear if it scales.

**b) TiMem (2601.02845) -- January 2026**
- "Temporal-Hierarchical Memory Consolidation for Long-Horizon Conversational Agents"
- Uses a **Temporal Memory Tree (TMT)** structure with semantic-guided consolidation and complexity-aware recall
- **Results:** 75.30% on LoCoMo (52.20% reduction in recalled memory length), 76.88% on LongMemEval-S
- Treats temporal continuity as a first-class organizing principle

**c) GAM (2604.12285) -- April 2026**
- "Hierarchical Graph-based Agentic Memory for LLM Agents"
- **Explicitly decouples memory encoding from consolidation**: isolates ongoing dialogue in an event progression graph, integrates into a topic associative network only upon semantic shifts
- Outperforms SOTA on LoCoMo and LongDialQA (specific numbers not in abstract)

**d) D-MEM (2603.14597) -- March 2026**
- "Dopamine-Gated Agentic Memory via Reward Prediction Error Routing"
- Uses a lightweight Critic Router with Fast/Slow paths: low-RPE inputs get O(1) buffer, high-RPE inputs (factual contradictions, preference shifts) trigger O(N) cognitive restructuring
- **Results:** 80%+ token reduction, eliminated O(N^2) bottlenecks on LoCoMo-Noise

**e) DeltaMem (2604.01560) -- April 2026**
- Uses RL for memory management with a novel Memory-based Levenshtein Distance metric
- Both training-free and RL-trained versions outperform product-level baselines on LoCoMo, HaluMem, PersonaMem

**f) SSGM Framework (2603.11768) -- March 2026**
- Identifies two corruption risks: topology-induced knowledge leakage and semantic drift through iterative summarization
- Proposes consistency verification, temporal decay modeling, dynamic access control

**g) Memory Retrieval and Consolidation through Function Tokens (2510.08203) -- Oct 2025**
- Theoretical: proposes that linguistic function words (punctuation, articles, prepositions) act as memory retrieval/consolidation triggers in LLMs
- Interesting theory but not an engineering system

**Assessment:** Memory consolidation is real and happening. The key distinction: SleepGate and TiMem do actual consolidation (merging fragmented memories). GAM does proper encoding/consolidation decoupling. D-MEM does selective gating. These go beyond simple pruning.

---

## 5. MCP Specification Status

### Stable Spec: 2025-11-25
The current stable MCP specification (version 2025-11-25) includes:

**Server Features:**
- Resources (context and data)
- Prompts (templated messages)
- Tools (executable functions)

**Client Features:**
- Sampling (server-initiated LLM interactions) -- **STABLE**
- Roots (filesystem boundaries) -- **STABLE**
- Elicitation (server-initiated user input requests) -- **STABLE in 2025-11-25**
  - Two modes: Form (structured data collection) and URL (out-of-band for sensitive data like OAuth)
  - URL mode is explicitly noted as "New feature: introduced in the 2025-11-25 version"

### MCP Tasks: IN THE DRAFT SCHEMA, NOT YET IN STABLE SPEC

Tasks ARE defined in the draft TypeScript schema (`schema/draft/schema.ts`) with full type definitions:

**Task States:**
```
"working" | "input_required" | "completed" | "failed" | "cancelled"
```

**Task Structure:** taskId, status, statusMessage, createdAt, lastUpdatedAt, ttl, pollInterval

**Operations:**
- `tasks/result` -- retrieve result of a task-augmented request
- `tasks/cancel` -- cancel a running task
- `tasks/list` -- paginated task listing

**How Tasks work:** Any request can be "task-augmented" -- instead of blocking, it returns a CreateTaskResult immediately, and the actual result is retrieved later via `tasks/result`. This enables async/long-running operations.

**Active development:**
- SEP-2557: "Adapt Tasks for Stateless & Sessionless Protocol" (draft PR, 8/9 complete)
- SEP-2549: "TTL for List Results" (draft PR)
- SEP-1686 issues about task notifications and in-progress results
- Multiple closed PRs: SEP-2339 (Task Continuity), SEP-2229 (Unsolicited Tasks)

### MCP Triggers: NOT FOUND
No "Trigger" types exist in the draft schema. No Trigger specification found anywhere. **This appears to be either a community wishlist item or confused with another concept.**

**Summary:**
| Feature | Status |
|---------|--------|
| Resources, Prompts, Tools | Stable (2025-11-25) |
| Sampling, Roots | Stable (2025-11-25) |
| Elicitation (Form + URL) | Stable (2025-11-25) |
| Tasks | Draft schema, active PRs, NOT in stable spec |
| Triggers | Does not exist in any spec or draft |

---

## 6. Mem0, Zep/Graphiti, Letta/MemGPT, Cognee -- Actual Architectures

### Mem0

**Architecture:**
- Vector store + entity linking (Qdrant default, supports 20 vector DBs)
- Default embedding dimension: 1536
- **CRITICAL: Graph memory has been REMOVED from the open-source SDK.** ~4000 lines of graph code deleted.
- Previously supported Neo4j, Memgraph, Kuzu, Apache AGE, Neptune
- Replaced with "built-in entity linking" that extracts entities (proper nouns, quoted text, compound noun phrases) and stores them in a parallel vector collection
- The old `relations` field on search results is no longer populated

**Real Limitations:**
- No longer has graph relationships -- just vector similarity + entity boosting
- Entity linking is purely extraction-based, not reasoned
- No temporal reasoning
- Cloud-first model; OSS SDK is limited compared to managed service

### Zep / Graphiti

**Architecture (from paper arXiv:2501.13956):**
- Temporal knowledge graph with three components: Entities (nodes), Facts/Relationships (edges with temporal validity windows), Episodes (provenance/raw data)
- **Bi-temporal tracking**: facts have validity windows; contradictions invalidate but don't delete old facts
- Hybrid retrieval: semantic embeddings + BM25 keyword + graph traversal
- Multi-backend: Neo4j (default), FalkorDB, Kuzu, Amazon Neptune
- Requires LLM with structured output support (OpenAI, Gemini recommended)

**Benchmark Results:** 94.8% on Deep Memory Retrieval (vs MemGPT 93.4%), up to 18.5% improvement on LongMemEval with 90% latency reduction

**Real Limitations:**
- Self-hosted only; infrastructure responsibility on developers
- Low default concurrency (limit 10) to prevent rate-limiting
- LLM-dependent for entity/relationship extraction -- quality varies with model
- No consolidation mechanism -- graph only grows

### Letta (formerly MemGPT)

**Architecture:**
- OS-inspired virtual memory management for LLMs
- Three memory tiers:
  1. **Core Memory** -- in-context, managed via Block objects with labels (persona, human). Character-limited. This IS the context window.
  2. **Archival Memory** -- external long-term store with search, tagging, timestamps
  3. **Recall Memory** -- additional external memory layer
- Also supports File Blocks for attached file content
- Agents manage their own memory via tool calls (self-editing memory)
- Model-agnostic

**Real Limitations:**
- Core memory is literally just string blocks in the system prompt -- crude
- The "virtual memory" metaphor is mostly marketing; it's tool-based retrieval
- Self-management means the LLM decides what to remember -- unreliable
- No structured knowledge representation (no graph, no schema)
- Memory quality degrades with smaller/cheaper models

### Cognee

**Architecture:**
- Pipeline: remember (add + cognify + improve), recall, forget
- Combines vector search + graph databases
- Supports Neo4j for graphs
- "Ontology grounding" and multimodal ingestion
- Auto-routing for recall (selects between search strategies)

**Real Limitations:**
- Python 3.10-3.13 only
- Requires LLM API key for all operations
- Actual graph construction algorithms undocumented
- Conflict resolution undocumented
- Small community, less battle-tested than alternatives

---

## 7. Temporal Knowledge Graphs -- State of the Art

The TKG field is very active in 2025-2026. Key papers:

### DynaGen (2512.12669) -- December 2025
- Unifies interpolation (filling historical gaps) and extrapolation (predicting future)
- Dynamic entity-centric subgraphs + conditional diffusion for generalization
- SOTA: +2.61 MRR on interpolation, +1.45 MRR on extrapolation across 6 benchmarks

### MemoTime (2510.13614) -- October 2025, Accepted WWW 2026
- "Tree of Time" hierarchical decomposition for multi-entity temporal synchronization
- Operator-aware reasoning enforcing monotonic timestamps
- Qwen3-4B with MemoTime matches GPT-4-Turbo performance
- Up to 24% improvement over strong baselines

### EvoReasoner (2509.15464) -- September 2025
- EvoKG for "temporally shifting knowledge" with confidence-based contradiction resolution
- Handles knowledge that changes over time

### RTQA (2509.03995) -- September 2025
- Recursive decomposition for complex temporal queries
- Multi-path answer aggregation

**Key trend:** The field is moving from pure embedding-based TKG methods toward LLM-augmented approaches that use knowledge graphs to ground LLM temporal reasoning. The combination of structured temporal graphs + LLM reasoning is the current frontier.

---

## 8. Event Sourcing + CRDT for Knowledge

**Has anyone applied CRDTs to knowledge graph merging?**

**No.** Extensive searching found:
- "CRDT" + "knowledge graph" -- zero arXiv results
- "conflict-free replicated" + "knowledge" -- zero arXiv results
- "event sourcing" + "knowledge graph" -- one irrelevant 2018 paper about Twitter

**The closest related work:**
- Graphiti's bi-temporal tracking is essentially event sourcing for facts (facts are never deleted, only invalidated)
- RDF has named graphs which can support provenance tracking
- Martin Kleppmann (of CRDT fame) has not published on knowledge graph CRDTs

**Why this gap exists:**
1. Knowledge graphs have schema/ontology constraints that complicate CRDT merge semantics
2. Triple merging requires semantic understanding (is "Alice works at Google" the same as "Alice is employed by Alphabet"?)
3. CRDTs guarantee eventual consistency for syntactic data; knowledge requires semantic consistency
4. The "last writer wins" or "multi-value register" CRDT strategies don't map cleanly to knowledge assertions that can be true/false/uncertain

**Real challenges if you attempted this:**
- Defining a lattice for knowledge assertions (partial ordering of truth values)
- Handling contradictions (CRDTs avoid conflicts; knowledge has inherent conflicts)
- Maintaining referential integrity across distributed graphs
- Temporal validity merging (when two nodes disagree about when a fact became true)

**Assessment:** This is genuinely unexplored territory. An event-sourced knowledge graph with CRDT-like merge semantics would be novel research.

---

## 9. Genuinely Novel Approaches to Agent Memory (2025-2026)

Beyond store-and-retrieve:

### GAM -- Encoding/Consolidation Decoupling (April 2026)
Separates active dialogue tracking from long-term knowledge integration. Only consolidates when semantic shifts are detected. This mimics how biological memory works (hippocampal encoding vs. cortical consolidation).

### D-MEM -- Dopamine-Gated Memory (March 2026)
Uses reward prediction error to decide whether to update memory. Routine inputs bypass the memory pipeline entirely (O(1)); surprising or contradictory inputs trigger full cognitive restructuring. This is the closest thing to predictive coding for memory management.

### MemCollab -- Cross-Agent Memory Sharing (March 2026)
Contrastive trajectory distillation creates "agent-agnostic memory" that transfers across heterogeneous models. Compares how different agents solve the same problem to extract model-independent reasoning patterns.

### MAGMA -- Multi-Graph Traversal Policy (January 2026)
Retrieval as policy-guided traversal over orthogonal graph views. The agent learns which graph to traverse based on query characteristics.

### SleepGate -- Actual Sleep Cycles (March 2026)
Periodic inference-time consolidation with entropy-triggered "micro-sleep" cycles. Genuine biological analogy, not just pruning.

### TiMem -- Temporal Memory Trees (January 2026)
Progressive abstraction from raw conversations to refined persona models through temporal hierarchy. Complexity-aware recall balances accuracy vs. compute.

### Memoria -- Weighted Knowledge Graphs for Personalization (December 2025)
Incrementally captures user traits as weighted entity-relationship structures alongside dynamic session summarization.

### What's actually new vs. incremental:
- **Genuinely new:** D-MEM's RPE gating, MemCollab's cross-agent transfer, GAM's encoding/consolidation split
- **Smart engineering:** MAGMA's multi-graph, TiMem's temporal trees
- **Incremental:** Most "memory" papers still do vector-store + summarization + retrieval

---

## 10. Compiled Knowledge

### Knowledge Compilation (AI Planning Sense)

**NeSyPr (2510.19429) -- Accepted NeurIPS 2025**
- "Neurosymbolic Proceduralization for Efficient Embodied Reasoning"
- Compiles symbolic plans into procedural representations that integrate into LM inference
- Three steps: symbolic planner generates plans -> transform to composable procedures -> encode as production rules for LM
- Enables "single-step LM inference" for what previously required multi-step symbolic reasoning
- Tested on PDDLGym, VirtualHome, ALFWorld

**Counting and Reasoning with Plans (2502.00145) -- February 2025**
- Transforms planning tasks into propositional formulas, uses knowledge compilation to count and reason about different plans
- Traditional AI planning knowledge compilation, not LLM-related

### Knowledge Distillation for Agents
- **TIP (2604.14084)** -- Token Importance in On-Policy Distillation. Entropy-based token selection for efficient model distillation.
- **Nemotron-Cascade 2 (2603.19220)** -- Multi-domain on-policy distillation across reasoning and agentic domains.
- These are about model distillation (teacher-student), not knowledge graph compilation.

### What "Compiled Knowledge" Could Mean:
1. **AI Planning sense (DONE):** Compiling declarative knowledge into efficient procedural form (NeSyPr does this)
2. **Knowledge graph sense (NOT DONE):** Pre-computing inference closures, materializing derived facts, pre-answering likely queries
3. **Cognitive science sense (PARTIAL):** D-MEM and GAM's consolidation can be viewed as "compiling" episodic memory into semantic knowledge

**Assessment:** Knowledge compilation in the planning sense is well-established. Knowledge graph compilation (pre-computing traversals, materializing views, compiling query patterns into lookup tables) has NO published work. This is an open niche.

---

## Summary: What's Real vs. Hype

| Claim | Reality |
|-------|---------|
| Multi-graph memory architectures | REAL -- MAGMA at ACL 2026 |
| Memory consolidation in AI | REAL -- Multiple 2026 papers with results |
| Predictive coding for knowledge graphs | DOES NOT EXIST -- completely open |
| HTM for knowledge graphs | DOES NOT EXIST -- Numenta pivoted to Thousand Brains |
| MCP Tasks | REAL but DRAFT -- in schema, active development |
| MCP Triggers | DOES NOT EXIST in any spec |
| MCP Elicitation | REAL and STABLE since 2025-11-25 |
| Mem0 graph memory | REMOVED from OSS -- now vector-only |
| CRDTs for knowledge | DOES NOT EXIST -- zero publications |
| Event sourcing for KG | Graphiti does bi-temporal; formal event sourcing absent |
| Temporal KG reasoning | VERY ACTIVE -- DynaGen, MemoTime leading |
| Compiled knowledge graphs | DOES NOT EXIST as a field |
| Cross-agent memory transfer | EMERGING -- MemCollab (March 2026) |
| Dopamine-gated memory | REAL -- D-MEM (March 2026) |

### Biggest Open Opportunities (No Published Work Exists):
1. Predictive coding / active inference for knowledge retrieval
2. CRDT-based knowledge graph merging
3. Compiled knowledge graphs (pre-computed inference, materialized views)
4. HTM-inspired temporal patterns in knowledge graphs
5. Knowledge graph compilation in the AI planning sense (NeSyPr-style but for KGs not embodied agents)
