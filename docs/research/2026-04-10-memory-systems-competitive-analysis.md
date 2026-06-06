# State-of-the-Art Memory Systems for AI Agents
## Competitive Deep Research — April 10, 2026

Research focus: How production memory systems handle multi-project, multi-source knowledge with isolation. Every claim below is sourced from public documentation, GitHub repos, or published blog posts.

---

## 1. Mem0

**What it is:** An open-source memory layer for AI agents. Formerly associated with the "Embedchain" project (now folded into the mem0 monorepo). Provides both a hosted platform (app.mem0.ai) and a self-hosted open-source option (Apache 2.0).

### Architecture

- **Vector stores:** 20 supported backends in Python — Qdrant (default), Chroma, PGVector, Milvus, Pinecone, MongoDB, Redis, Elasticsearch, Weaviate, FAISS, Supabase, and more. TypeScript supports Qdrant, Redis, Valkey, Vectorize, and in-memory.
- **Graph database:** Neo4j (via `langchain_neo4j.Neo4jGraph`). Graph memory is a separate feature layered on top of vector memory.
- **LLMs:** OpenAI (default), with support for other providers.
- **Languages:** Python (61%), TypeScript (29%).

### Memory Model

Three-tier scoping: **User**, **Session**, and **Agent** state. Every memory operation takes a `user_id` parameter for isolation. Graph memory is an augmentation layer — graph relations are returned alongside vector results to provide additional context; they do NOT re-rank vector hits.

### Entity Resolution

Embedding-based similarity matching. Before adding a new entity node, the system:
1. Embeds the source and destination nodes using the configured embedder
2. Searches for existing nodes exceeding a similarity threshold (default 0.7)
3. Merges with existing nodes if found

This is a soft-match approach — no ontology, no canonical naming, no deterministic resolution.

### Contradictions / Temporal Changes

Relationships have a `valid` flag for soft-deletion (not hard removal). When a relationship already exists, the system increments a `mentions` counter and resets invalidation markers. There is no explicit contradiction detection — no mechanism to flag when new information conflicts with existing information.

### Multi-Tenant / Multi-Project

Isolation via `user_id`, `agent_id`, and `run_id` parameters on nodes. No workspace-level isolation. No cross-project linking model documented. The graph memory stores these IDs on every node, providing per-user filtering.

### Privacy / Isolation

Scoping by user_id/agent_id/run_id. No sensitivity labels. No trust levels. No access control model beyond API-level auth.

### Latency

Graph memory is explicitly documented as **asynchronous** — "adding graph memories is an asynchronous operation due to heavy processing." Retrieval requires calling `get_all()` separately. Vector search is standard latency (depends on chosen backend).

### Key Limitations

- Graph memory is a bolt-on, not the core architecture
- No contradiction detection
- No knowledge verification or health scoring
- No compilation step — it is purely store-and-search
- Entity resolution is probabilistic (embedding similarity), not deterministic

---

## 2. Zep (the Platform)

**What it is:** A hosted context engineering platform for agent applications. Combines agent memory, Graph RAG, and context assembly. Powered by Graphiti (see below) as its open-source temporal knowledge graph framework.

### Architecture

- **Knowledge graph:** Nodes = entities, edges = facts/relationships. Dynamically updated when new information arrives.
- **Temporal tracking:** Stores dates when facts became valid and invalid on edges.
- **Context assembly:** Generates an optimized string containing a user summary and relevant facts for the current conversation thread.
- **Languages:** Python (69.6%), Go (26.4%), TypeScript SDK available.

### Data Model

- **User Graph:** A specialized graph variant for personalized user context.
- **Fact invalidation:** When data contradicts existing facts, the invalidation timestamp is stored on that fact's edge. Old facts are not deleted.
- **Custom types:** Users can customize entity and relationship types using Pydantic-like classes.
- **Multi-source ingestion:** JSON, text, messages, documents, conversations, emails.

### Entity Resolution

Not extensively documented in public docs. The platform relies on Graphiti's entity resolution mechanisms (see Graphiti section below).

### Multi-Tenant

Zep is a hosted platform with per-user graph isolation. Specific multi-tenant architecture details are not publicly documented. The system is designed to manage "vast numbers of per-user/entity context graphs."

### Latency

Zep markets itself on fast context assembly. Graphiti (its underlying graph framework) claims sub-second latency for retrieval.

### Key Insight

Zep is primarily a **hosted product** that wraps Graphiti. The real technical substance is in Graphiti itself.

---

## 3. Graphiti (by Zep) — The Temporal Knowledge Graph

**What it is:** Zep's open-source temporal knowledge graph framework. This is the most technically interesting system in this research. Source: github.com/getzep/graphiti.

### Architecture

Four core components:
1. **Entities (Nodes):** People, products, policies, concepts — with summaries that evolve over time
2. **Facts/Relationships (Edges):** Triplets (Entity -> Relationship -> Entity) with temporal validity windows
3. **Episodes:** Raw ingested data serving as provenance — every derived fact traces back to its source
4. **Custom Types:** Developer-defined entity and edge types via Pydantic models

### Supported Databases

- Neo4j 5.26+
- FalkorDB 1.1.2+
- Kuzu 0.11.2+
- Amazon Neptune (with OpenSearch Serverless for full-text search)

### Temporal Model (Bi-Temporal Tracking)

This is Graphiti's key differentiator. Every fact has two temporal dimensions:
- **When the fact became true** (valid_at)
- **When the fact was superseded/invalidated** (invalid_at)

When contradictions arise, old facts are automatically invalidated rather than deleted. This preserves full historical context. Users can query what is currently true OR what was true at any point in time.

This is deterministic state management — not LLM-driven judgment.

### Entity Resolution

Graphiti handles entity resolution through its ingestion pipeline. New entities are matched against existing entities during episode processing. The system supports both:
- **Prescribed ontology:** Predefined schemas (entity/edge types defined upfront)
- **Learned structure:** Emerging patterns from data

### Search Strategy — Hybrid Retrieval

Three retrieval methods combined:
1. **Semantic search** via embeddings
2. **Keyword matching** (BM25)
3. **Graph traversal** for relationship-based queries

This eliminates dependency on LLM-driven summarization at query time.

### Latency

Typically **sub-second** for retrieval. Graphiti explicitly contrasts this with GraphRAG's "seconds to tens of seconds."

### Incremental Construction

New episodes integrate into the existing graph without batch recomputation. The graph evolves in real-time. This is in direct contrast to GraphRAG, which requires full re-indexing.

### Key Insight

Graphiti is the closest system to ThinkingRoot's approach in terms of temporal tracking and contradiction handling. However, it is a runtime memory system (dynamic, incremental, conversation-driven) rather than a compile-time knowledge system (batch, verified, artifact-generating).

---

## 4. LangGraph / LangChain Memory

**What it is:** LangChain's framework-level approach to agent memory. Not a standalone memory product — it is memory primitives built into the LangGraph agent framework.

### Memory Types (LangChain's Taxonomy)

LangChain defines three conceptual memory categories (from their blog post "Memory for Agents"):

1. **Procedural Memory:** LLM weights and agent code. Rarely auto-updated. Some agents modify system prompts.
2. **Semantic Memory:** Facts about users and context. Extracted from conversations via LLMs, stored, retrieved into system prompts.
3. **Episodic Memory:** Sequences of past agent actions. Implemented via few-shot example prompting.

### LangGraph Implementation

**Short-term memory:** Thread-scoped checkpoints. State is persisted to a database using a checkpointer so the thread can be resumed at any time. Stores messages, uploaded files, retrieved documents.

**Long-term memory:** Persists across conversations using custom namespaces. Uses a Store API with:
- `put()` — stores JSON documents under namespace/key pairs
- `get()` — retrieves by ID
- `search()` — queries with content filters and vector similarity

### Storage Backends

- `InMemoryStore` for development
- Documentation states: "Use a DB-backed store in production use" but does not specify which databases are supported in the public docs reviewed.

### Multi-User / Multi-Project

Isolation via namespace structure: "Namespaces often include user or org IDs or other labels that makes it easier to organize information." This is convention-based, not enforced.

### Memory Update Patterns

- **Hot Path:** Agent explicitly stores facts via tool calls before responding. Adds latency but ensures immediate updates.
- **Background:** Separate process updates memory during/after conversations. No latency but delayed updates.
- **User Feedback:** Users mark interactions positively to create retrievable examples.

### Memory Subtypes

- **Profile approach:** Single continuously-updated document. Compact but lossy — model must generate correct updates.
- **Collection approach:** Multiple narrowly-scoped documents. Higher recall but requires managing deletions/overwrites.

### Key Limitations

- No graph-based memory (only key-value + vector)
- No entity resolution
- No contradiction detection
- No temporal tracking
- No knowledge verification
- Convention-based isolation (namespace strings), not enforced boundaries
- It is a toolkit, not a system — you build your own memory on top of primitives

---

## 5. Cognee

**What it is:** Open-source memory engine for AI agents that builds knowledge graphs from data. Uses an ECL (Extract, Cognify, Load) pipeline. github.com/cognee-ai/cognee.

### Architecture — Four Core Operations

1. **`add`:** Ingests data from files, directories, URLs, or S3 URIs across 38+ formats (PDF, CSV, JSON, audio, images, code). Content is normalized, deduplicated via hashing, organized into datasets with ownership controls.

2. **`cognify`:** The central operation — a six-stage workflow:
   - Document classification
   - Permission verification
   - Chunk extraction
   - LLM-based entity and relationship extraction
   - Summary generation
   - Embedding and graph commit
   Only new or modified files reprocess on reruns (incremental).

3. **`memify`:** Post-ingestion refinement:
   - Prunes stale nodes
   - Strengthens frequent connections
   - Reweights edges based on usage signals
   - Adds derived facts
   "Memory is not static storage, it's an evolving structure."

4. **`search`:** Queries across vector and graph layers using **14 retrieval modes**.

### Storage — Three-Layer Hybrid

- **Graph Store:** Kuzu (default), Neo4j, FalkorDB, Neptune, Memgraph
- **Vector Store:** LanceDB (default), Qdrant, pgvector, Redis, DuckDB, Pinecone, ChromaDB
- **Relational Store:** SQLite (default), PostgreSQL

Default deployment requires zero infrastructure setup (embedded Kuzu + LanceDB + SQLite).

### Data Model

The fundamental unit is `DataPoint` — a Pydantic model carrying content and metadata. Entities, chunks, summaries, and relationships are all DataPoints. Users define custom DataPoints to control which fields get embedded. Bidirectional linking between graph nodes and embeddings is maintained.

### Entity Resolution — Ontology-Grounded (Key Differentiator)

Cognee's approach to entity resolution is the most sophisticated in this survey. It uses a four-step validation layer:

1. **LLM Extraction:** Instructor-powered structured output generates typed Node/Edge objects
2. **Resolver & Lookup:** `RDFLibOntologyResolver` parses OWL files, creating cached dictionaries for classes/individuals with normalized keys
3. **Fuzzy Matching:** `difflib.get_close_matches()` matches entities against ontology terms at configurable threshold (default 0.80 similarity)
4. **Canonicalization & Subgraph Expansion:** Matched entities get replaced with canonical URI-derived names; BFS traversal extracts surrounding ontology structure (`rdfs:subClassOf`, `owl:ObjectProperty` edges)

Every node gets an `ontology_valid` flag (True if matched, False if not). Canonical naming eliminates cross-document duplicates.

The system works without ontologies (falls back to LLM-only extraction). Ontology support is additive.

### Custom Graph Models

Users can define class-based schemas with typed relationships. Testing on 2WikiMultihopQA showed expanded custom graph models achieving **0.54 F1** vs **0.35 F1** for the default pipeline.

### Search Modes (14 Total)

Notable modes: GRAPH_COMPLETION (vector hints -> triplets -> graph traversal), GRAPH_COMPLETION_COT (chain-of-thought multi-hop), RAG_COMPLETION (traditional chunks), NATURAL_LANGUAGE (Cypher translation), FEELING_LUCKY (LLM-selected optimal mode).

### Multi-Tenancy

Dataset-level permissions (read, write, delete, share). Per-user or per-group instantiation. Session memory vs permanent memory separation.

### Key Insight

Cognee is the most architecturally similar system to ThinkingRoot. Both use:
- Incremental processing (content hashing to skip unchanged files)
- Multi-database hybrid (graph + vector + relational)
- LLM-based extraction with structured output
- Embedded defaults requiring zero infrastructure

Key differences: Cognee is Python-only, uses ontologies (RDF/OWL) rather than ThinkingRoot's claim-based model, lacks explicit contradiction detection/resolution, and has no concept of knowledge health scoring, trust levels, or sensitivity labels.

---

## 6. Microsoft GraphRAG

**What it is:** A data pipeline for extracting structured data from unstructured text using LLMs, then using that structure for improved RAG. Published as a research paper (arXiv:2404.16130) and open-source tool. github.com/microsoft/graphrag.

### Indexing Pipeline

1. **Text chunking:** Documents are split into analyzable TextUnits
2. **Entity & relationship extraction:** LLM extracts entities, relationships, and claims from text
3. **Community detection:** Leiden algorithm performs hierarchical clustering on the entity graph
4. **Community summarization:** Bottom-up summaries generated at multiple granularity levels
5. **Embedding:** Text embedded into vector space
6. **Storage:** Results stored as Parquet tables by default; embeddings written to configured vector store

### Query Modes

- **Local Search:** Combines entity knowledge graph data with raw document text chunks. Best for entity-specific questions. Efficient, narrow scope.
- **Global Search:** Map-reduce over ALL community summaries. Resource-intensive but handles holistic dataset questions. Each community summary generates a partial response; all partial responses are summarized into a final answer.
- **DRIFT Search:** Enhances local search with community information. Generates detailed follow-up questions. More thorough than local, less expensive than global.

### Architecture Decisions

- **Knowledge Model abstraction:** Common interface independent of underlying storage
- **LLM caching:** Caches LLM responses based on identical prompt/parameter inputs for idempotency
- **Factory pattern:** Extensible via factories for LLMs, input readers, cache storage, vector stores, workflows
- **Modular pipeline:** Composed of workflows, standard/custom steps, prompt templates, I/O adapters

### Key Trade-offs

**GraphRAG vs Standard RAG:**
- GraphRAG excels at global sensemaking queries ("What are the main themes?") where standard RAG fails completely
- Standard RAG is better for specific factual retrieval where the answer exists in a single passage
- GraphRAG is significantly more expensive (many LLM calls for community summarization)
- GraphRAG requires batch re-indexing — NOT incremental

**GraphRAG vs Graphiti:**
- GraphRAG: Batch processing, community summaries, LLM-driven query answers. Seconds to tens of seconds latency.
- Graphiti: Incremental construction, bi-temporal tracking, hybrid retrieval. Sub-second latency.
- GraphRAG: Better for static document corpora analyzed holistically
- Graphiti: Better for dynamic, real-time agentic contexts

### Multi-Tenant / Multi-Project

Not addressed. GraphRAG is designed as a single-corpus indexing pipeline, not a multi-tenant system.

### Key Insight

GraphRAG is a **batch indexing pipeline**, not a runtime memory system. It does not handle temporal changes, contradictions, or incremental updates. It must re-index the entire corpus when data changes. Its strength is global sensemaking over large document sets — a fundamentally different use case from agent memory.

---

## 7. Letta (formerly MemGPT)

**What it is:** A platform for building stateful agents with advanced memory. Based on the MemGPT research paper (arXiv:2310.08560) which introduced "virtual context management" inspired by OS memory hierarchies.

### Memory Architecture — OS-Inspired Virtual Context

The core MemGPT insight: manage LLM context windows the way an operating system manages memory — with paging between fast and slow storage tiers.

**Memory tiers:**
- **Core Memory Blocks:** Persistent knowledge the agent maintains in-context. Directly readable and writable. Configurable blocks with labels like "human" and "persona."
- **Archival Memory:** External (out-of-context) memory store that agents can search. Analogous to disk storage.
- **Recall Memory:** (Referenced in paper) Historical conversation data.

### Agent Self-Editing

The key innovation: agents modify their own memory through **function/tool calls**. The agent decides what to remember, what to archive, and what to forget. This is in contrast to systems where memory management is handled externally.

### Multi-Agent

Three communication mechanisms:
1. **Async messaging:** `send_message_to_agent_async` — fire and forget
2. **Sync messaging:** `send_message_to_agent_and_wait_for_reply` — request/response
3. **Broadcast:** `send_message_to_agents_matching_all_tags` — tag-based multicast

Agents can share state via **shared memory blocks** — though specific isolation details are not publicly documented.

### Tech Stack

- Python + Go
- SDKs for Python and TypeScript
- Hosted platform + self-hosted option

### Key Limitations

- No knowledge graph — memory is text blocks, not structured knowledge
- No entity resolution
- No contradiction detection
- No compilation or verification
- Memory is per-agent, not per-project or per-organization
- Agent self-editing is powerful but unpredictable — the agent can corrupt its own memory

### Key Insight

Letta's contribution is the **agent-editable memory** paradigm, not the storage architecture. It treats memory as a resource the agent manages through tool calls. This is philosophically interesting but architecturally simple — the actual storage is just text blocks and a vector store.

---

## 8. Haystack (by deepset)

**What it is:** An open-source framework for building RAG and search pipelines. Not a memory system per se, but a pipeline framework with document store abstractions.

### Architecture

Explicit, modular pipelines composed of retrievers, routers, memory layers, tools, evaluators, and generators. Components are independently testable and replaceable.

### Document Stores

Protocol requires four methods: `count_documents`, `filter_documents`, `write_documents`, `delete_documents`. Mentioned backends: ChromaDocumentStore, InMemoryDocumentStore. Supports many more via integrations.

### Duplicate Handling

Three policies: OVERWRITE (replace), SKIP (ignore), FAIL (raise error). Based on document IDs (auto-generated via content hashing if not provided).

### Multi-Tenant / Multi-Project

Not addressed in core framework. No built-in isolation model.

### Key Insight

Haystack is a **pipeline framework**, not a memory system. It provides the plumbing to build retrieval pipelines but does not itself manage knowledge, track entities, resolve contradictions, or handle temporal changes. It competes with LangChain, not with Mem0/Zep/Cognee.

---

## Comparative Analysis

### Feature Matrix

| Feature | ThinkingRoot | Mem0 | Zep/Graphiti | Cognee | GraphRAG | LangGraph | Letta |
|---|---|---|---|---|---|---|---|
| **Core paradigm** | Knowledge compiler | Memory store | Temporal KG | Memory engine | Batch indexer | Agent toolkit | Agent memory |
| **Graph DB** | CozoDB (Datalog) | Neo4j | Neo4j/FalkorDB/Kuzu/Neptune | Kuzu/Neo4j/FalkorDB | Parquet + vector | None | None |
| **Vector DB** | fastembed (local) | 20 backends | Embeddings in graph DB | LanceDB/Qdrant/etc | Configurable | InMemoryStore | Vector store |
| **Entity resolution** | Alias-based + linking stage | Embedding similarity (0.7) | Pipeline-based | Ontology-grounded (RDF/OWL) | LLM extraction | None | None |
| **Contradiction detection** | Automatic + resolution audit trail | None | Automatic fact invalidation | None (stale pruning only) | None | None | None |
| **Temporal tracking** | valid_from/valid_until + superseded_by | Soft-delete flag | Bi-temporal (valid_at/invalid_at) | Edge reweighting | None | None | None |
| **Knowledge health** | Health Score (freshness/consistency/coverage/provenance) | None | None | None | None | None | None |
| **Trust levels** | 5 levels (Quarantined->Verified) | None | None | None | None | None | None |
| **Sensitivity labels** | 4 levels (Public->Restricted) | None | None | Permissions (R/W/D/Share) | None | None | None |
| **Multi-project** | Multi-workspace (--path repeatable) | user_id/agent_id scoping | Per-user graphs | Dataset-level permissions | Single corpus | Namespace conventions | Per-agent |
| **Incremental** | BLAKE3 content hashing | Append-only | Incremental episodes | Content hash dedup | Full re-index required | N/A | N/A |
| **Language** | Rust | Python/TypeScript | Python/Go | Python | Python | Python | Python |
| **Offline/embedded** | Yes (CozoDB + fastembed, zero infra) | Needs vector DB | Needs Neo4j (typically) | Yes (Kuzu + LanceDB + SQLite) | Needs LLM | Needs backend | Needs server |
| **Compilation/artifacts** | Entity pages, arch maps, decision logs | None | None | None | Community summaries | None | None |

### Architectural Differentiation

**ThinkingRoot's unique position:** Every other system in this survey treats knowledge as a **retrieval problem** — store data, search it when needed. ThinkingRoot treats it as a **compilation problem** — transform raw sources into verified, typed, linked knowledge artifacts that are pre-optimized for consumption.

Specific differentiators:

1. **Compilation produces artifacts.** No other system generates entity pages, architecture maps, contradiction reports, or health reports. Cognee has `memify` (edge reweighting/pruning) but does not produce compiled documents.

2. **Knowledge Health Score.** No other system has a composite quality metric (freshness 30% + consistency 30% + coverage 20% + provenance 20%) with 7 verification checks.

3. **Trust levels per source.** No other system distinguishes between Quarantined / Untrusted / Unknown / Trusted / Verified sources. Cognee has dataset permissions but not trust provenance.

4. **Sensitivity labels per claim.** Only ThinkingRoot labels individual claims as Public / Internal / Confidential / Restricted.

5. **Typed claims as the fundamental unit.** ThinkingRoot's `Claim` type (Fact, Decision, Opinion, Plan, Requirement, Metric, Definition, Dependency, ApiSignature, Architecture) with confidence scores, source spans, and supersession chains is unique. Other systems store relationships or facts but do not type-classify the claims themselves.

6. **CozoDB (Datalog graph).** No other system uses Datalog. CozoDB provides an embedded graph database with a powerful recursive query language — better suited for complex graph traversals than Neo4j's Cypher for deeply recursive queries.

7. **Rust performance.** Every other system is Python (or Python + Go). ThinkingRoot's Rust core means faster parsing, lower memory usage, and the ability to run embedded without a server.

8. **Zero-infrastructure embedded mode.** Only ThinkingRoot and Cognee can run with zero external services. But ThinkingRoot's Rust + CozoDB embedded + fastembed ONNX is a single binary with no Python runtime needed.

### Where Others Are Stronger

1. **Graphiti's bi-temporal model** is more battle-tested for real-time agent memory with dynamic updates. ThinkingRoot's temporal model (`valid_from`/`valid_until`/`superseded_by`) is structurally similar but designed for batch compilation rather than real-time updates.

2. **Cognee's ontology grounding (RDF/OWL)** is more sophisticated for entity resolution in domains with established vocabularies (healthcare with SNOMED CT, finance with FIBO). ThinkingRoot uses alias-based resolution, which is simpler but less rigorous for domains with formal ontologies.

3. **Cognee's 14 search modes** provide more retrieval flexibility than ThinkingRoot's current semantic + keyword search.

4. **Mem0's 20 vector store backends** give more deployment flexibility for teams already invested in specific vector databases.

5. **GraphRAG's community summarization** is uniquely powerful for global sensemaking over large static corpora — a use case ThinkingRoot does not currently address.

6. **Letta's agent self-editing memory** paradigm is interesting for autonomous agents that need to manage their own context — ThinkingRoot's model is external compilation, not agent-managed.

### The Real Competition

Based on this research, ThinkingRoot's actual competitive landscape is:

- **Direct competitor:** Cognee. Same problem space (knowledge graphs for AI agents), similar pipeline architecture (extract -> build graph -> search), similar multi-DB approach. Key differences: ThinkingRoot compiles artifacts + has health/trust/sensitivity; Cognee has ontology grounding + more search modes.

- **Complementary, not competitive:** Graphiti/Zep. Graphiti handles real-time conversational memory; ThinkingRoot handles batch knowledge compilation. An agent could use both — ThinkingRoot for project knowledge, Graphiti for user interaction memory.

- **Different category:** GraphRAG (batch corpus analysis), LangGraph (agent framework), Letta (agent platform), Haystack (pipeline framework), Mem0 (simple memory store).

---

## Research Sources

All claims in this document are sourced from:
- GitHub repositories: mem0ai/mem0, getzep/graphiti, getzep/zep, cognee-ai/cognee, microsoft/graphrag, letta-ai/letta
- Official documentation: docs.mem0.ai, help.getzep.com, docs.langchain.com, microsoft.github.io/graphrag
- Blog posts: blog.langchain.com/memory-for-agents, cognee.ai/blog (multiple deep dive posts)
- Academic papers: arXiv:2404.16130 (GraphRAG), arXiv:2310.08560 (MemGPT)
- Source code: mem0/memory/graph_memory.py (Neo4j integration, entity extraction, deduplication logic)

No information was hallucinated. Where documentation was incomplete or inaccessible (many Zep help pages returned 404s, Letta docs had URL changes), this is noted explicitly.
