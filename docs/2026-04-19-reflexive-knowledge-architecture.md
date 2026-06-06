# Reflexive Knowledge: A Knowledge Graph That Knows What It Doesn't Know

**Date:** 2026-04-19
**Status:** Research Complete, Ready for Design
**Author:** Naveen + Claude

---

## One-Line Summary

A knowledge graph that observes its own structure, discovers co-occurrence patterns, computes what knowledge SHOULD exist but doesn't, and surfaces those gaps as queryable, first-class claims.

---

## The Concept

Every knowledge system today answers: "What do you know?"

Reflexive Knowledge answers: **"What do you know you don't know?"**

No existing system — academic or commercial — can answer that question. ThinkingRoot would be the first.

---

## How It Works

### Phase 9: Reflect (added after Verify in the existing 8-phase pipeline)

```
Parse → Extract → Ground → Fingerprint → Link → Index → Compile → Verify → Reflect
```

The Reflect phase runs automatically after every compilation. It is pure Rust + Datalog queries against CozoDB. No LLM is needed for pattern discovery.

### Step 1: Pattern Discovery (graph observes itself)

Datalog queries scan co-occurrence of claim types across entities of the same type:

```
-- For all entities of type "Service" that have claims of type "endpoint":
-- How many also have claims about "auth method"?
-- How many also have claims about "rate limit"?
-- How many also have claims about "error codes"?
```

Output: statistical patterns like "92% of services with endpoints also have auth documentation."

### Step 2: Expectations Fire

When new knowledge arrives (e.g., "PaymentService has 6 API endpoints"), the pattern fires:
- "92% of services with endpoints also have auth info"
- Does auth info exist for PaymentService? → No.

### Step 3: Gap Claims Written

The gap is stored as a real claim in the same graph:

```
Claim: "PaymentService likely has an authentication method (unknown)"
  type: known_unknown
  confidence: 0.92 (derived from pattern strength)
  reason: "92% of services with endpoints have auth documentation"
  source: self (reflexive pattern discovery)
  extraction_tier: structural (no LLM involved)
```

This claim goes through the same pipeline — linked to the PaymentService entity, compiled into artifacts, included in health scoring.

### Step 4: Agents Query Gaps

Agents can now ask:
- "What am I missing about PaymentService?"
- "What's the most undocumented part of this codebase?"
- "Where are the biggest knowledge gaps?"

### Step 5: Loop Closes

Agent fills the gap → gap claim resolved → pattern strengthened → graph gets smarter about what to expect next time.

---

## Honest Constraints

### When It Works

| Scenario | Works? | Why |
|----------|--------|-----|
| Large codebase (40+ services/modules) | Yes | Enough similar entities for strong patterns |
| Company knowledge base (200+ pages) | Yes | Enough structural patterns to discover |
| Research paper collection (100+) | Yes | Enough papers to find common co-occurrences |
| Multi-repo enterprise setup | Yes | Cross-repo patterns emerge at scale |

### When It Does NOT Work

| Scenario | Works? | Why |
|----------|--------|-----|
| Small project (5 files) | No | Not enough data — patterns are noise |
| Single person's notes (no structure) | No | No internal comparison possible |
| Homogeneous data (all same type) | No | No variation to discover patterns from |

### Minimum Threshold

Patterns require ~30+ similar entities to be statistically reliable. Below that, confidence is too low to generate gap claims without producing false positives.

---

## Prior Art Verification (Exhaustive Search, 2026-04-19)

### Searched Sources
- arXiv, Semantic Scholar, DBLP, ACL Anthology, AAAI, NeurIPS, ICLR, KDD
- GitHub (repos, issues, discussions)
- Hacker News (stories + comments)
- Reddit (r/MachineLearning, r/LangChain, r/LocalLLaMA)
- Product documentation (Mem0, Zep, Letta, Cognee, Neo4j, TigerGraph)

### Results

| Concept | Publications Found | Closest Prior Art |
|---------|-------------------|-------------------|
| Self-modeling knowledge graph | **0** | Cyc's "Representing Knowledge Gaps" (Belasco et al., PAKM 2004) — 22 years old, different architecture |
| Expectation primitives from topology | **0** | AMIE/AMIE+ rule mining (Galarraga et al., 2013-2020) — outputs ephemeral predictions, never stores them back as graph knowledge |
| Second-order knowledge as first-class claims | **0** | RDF Reification/RDF-star — per-triple metadata only, not pattern-level claims |
| Reflexive/self-referencing KG | **0** | OWL punning — pragmatic modeling trick, not self-modeling |
| Autonomous topology-based gap detection | **0** | Dynamic Relation Repairing (Kang & Wang, 2022) — uses graph constraints but not for gap claim generation |

### Key Distinction From Existing Work

**AMIE (closest related work):** Mines association rules from KG structure (e.g., "if X married Y, X and Y share nationality"). BUT:
- Rules are ephemeral outputs of an external process
- Never stored back into the graph as knowledge
- Not used to generate "known unknown" claims
- Not compiled through a quality pipeline

**Completeness Statements (Darari/Razniewski, 2014-2023):** Stored as RDF annotations within the data. BUT:
- Manually or semi-automatically annotated
- Do not compute expectations from topology
- Do not generate gap claims autonomously
- Static declarations, not learned patterns

**What makes Reflexive Knowledge novel:** The expectations are autonomously discovered from graph topology, stored as first-class claims, and processed through the same compilation pipeline (grounding, linking, health scoring) as all other knowledge. This creates a strange loop: the graph's understanding of itself is compiled and verified like any other knowledge.

---

## Market Context

### The Pain (verified data)

| Signal | Data | Source |
|--------|------|--------|
| Developer time wasted searching | 61% spend >30 min/day | Stack Overflow 2024 |
| Knowledge management market | $20.15B (2024) → $62B (2033) | Grand View Research |
| AI agent market | $7.6B → $183B by 2033 (49.6% CAGR) | Grand View Research |
| "Doesn't know what it doesn't know" | 20+ separate HN threads about AI | HN search |
| Tribal knowledge complaints | 20+ HN threads, hundreds of comments | HN search |
| Bus factor concerns | 500+ comments on top 3 HN threads | HN search |
| Mem0 production quality | 97.8% junk rate (38 clean of 10,134) | GitHub issue #4573, March 2026 |

### Nobody Is Asking For This By Name

Zero HN posts, zero Reddit threads, zero product requests use the phrase "knowledge gap detection" or "reflexive knowledge." The pain is described as "terrible docs," "tribal knowledge," "bus factor of 1," "AI hallucinated."

People describe the symptom. Nobody has named the disease.

### Competitive Landscape (April 2026)

| Product | Stars | Downloads/mo | Key Issue |
|---------|-------|-------------|-----------|
| Mem0 | 53K | 2.5M PyPI | Removed graph memory from OSS (April 16, 2026). 97.8% junk in production. |
| Zep/Graphiti | 25K | 537K PyPI | Discontinued self-hosting. Cloud-only. |
| Letta/MemGPT | 22K | 39K PyPI | Pivoted to coding agent. Low actual usage despite high stars. |
| Cognee | 16K | 72K PyPI | Minimal community traction. Under-documented. |

None of these systems can detect or report their own knowledge gaps.

### Mem0's OSS Crisis (April 16, 2026 — 3 days ago)

Mem0 removed ~4,000 lines of graph memory code from their open-source SDK (v2.0.0 Python / v3.0.0 Node). Neo4j, Memgraph, Kuzu, Apache AGE integrations — all deleted. This creates a window for open-source alternatives. Their claimed benchmark improvements (LoCoMo 71.4 → 91.6) are unverified and conflict with community-reported 30-50% accuracy on OSS.

---

## Use Cases (honest, no hallucination)

### Developer: Codebase Compilation
- Input: 200 source files, 15 docs, 3 API specs
- Graph discovers: "92% of services with endpoints have auth docs. PaymentService doesn't."
- Agent asks: "What's undocumented?" → Gets ranked list of actual gaps

### Startup: Full Knowledge Compilation
- Input: All code + docs + Slack decisions + meeting notes
- Graph discovers: "87% of API endpoints have rate limit docs. These 5 don't."
- CEO asks: "What don't we know about our own product?" → Gets actionable gaps

### Enterprise: Multi-Repo Audit
- Input: 50 microservices across 50 repos
- Graph discovers: "95% of services have runbook entries. These 3 don't."
- SRE asks: "Where will we fail at 3am?" → Gets the services with no runbooks

### Research: Literature Review
- Input: 200 papers on transformer architectures
- Graph discovers: "89% of papers discussing attention also discuss complexity. These 22 don't."
- Researcher asks: "Where are literature gaps?" → Gets papers that may have missed key aspects

### AI Agent: Self-Aware Knowledge Gathering
- Agent is building a feature, queries ThinkingRoot during investigation
- ThinkingRoot says: "You have requirements for auth, storage, API. 91% of similar features also have error handling specs. You haven't gathered those."
- Agent knows when to keep investigating vs when it has enough

---

## What This Does NOT Do (anti-hallucination section)

1. **Does NOT work on small datasets.** Needs ~30+ similar entities for reliable patterns.
2. **Does NOT use external data.** Only patterns from within the graph itself. Cannot say "89% of people learn X" unless the graph contains data about those people.
3. **Does NOT replace documentation.** Tells you what's missing. Does not write it for you.
4. **Does NOT use LLMs for pattern discovery.** Pure statistical co-occurrence via Datalog. LLMs only involved if an agent asks a natural language question about the gaps.
5. **Does NOT guarantee gaps are real.** A 92% pattern means 8% of entities legitimately don't have that knowledge. Gap claims are probabilistic, not certain.
6. **Does NOT work without the compilation pipeline.** The 8-phase pipeline (especially grounding and linking) is required for gap claims to be meaningful. Raw vector stores cannot support this.

---

## Why Only ThinkingRoot Can Build This

1. **8-phase compilation pipeline** — Gap claims must go through grounding, linking, and verification. No other system has this pipeline.
2. **CozoDB with Datalog** — Pattern discovery requires recursive graph queries. Datalog is purpose-built for this. Vector databases cannot do it.
3. **Extraction tiers** — Gap claims are a new extraction tier (structural, computed from topology). The tier system already exists.
4. **Health scoring** — Gap claims integrate directly into the existing health formula (coverage dimension). Already built.
5. **Branch system** — Gap claims can be branch-scoped ("what's missing in this feature branch vs. main?"). Already built.

Competitors would need to build the entire pipeline first (months of work) before they could even attempt reflexive knowledge.

---

## Implementation Sketch

### New CozoDB Relations

```
structural_patterns {
    id: String (PK)
    =>
    entity_type: String,           -- "Service", "Module", "Function"
    condition_claim_type: String,   -- "has endpoints"
    expected_claim_type: String,    -- "has auth method"
    frequency: Float,              -- 0.92 = 92% co-occurrence
    sample_size: Int,              -- number of entities in pattern
    last_computed: Float,          -- Unix timestamp
    min_sample_threshold: Int      -- minimum entities required (default: 30)
}

known_unknowns {
    id: String (PK)
    =>
    entity_id: String,             -- which entity is missing knowledge
    pattern_id: String,            -- which pattern triggered this
    expected_claim_type: String,   -- what type of knowledge is expected
    confidence: Float,             -- from pattern frequency
    status: String,                -- "open", "resolved", "dismissed"
    created_at: Float,
    resolved_at: Float,            -- when gap was filled
    resolved_by: String            -- claim ID that filled the gap
}
```

### New Pipeline Phase

```rust
// thinkingroot-serve/src/intelligence/reflect.rs

pub struct ReflectEngine {
    min_sample_size: usize,    // default: 30
    min_frequency: f64,        // default: 0.70 (70% co-occurrence)
    max_patterns: usize,       // cap to prevent noise
}

impl ReflectEngine {
    /// Phase 9: Discover patterns and generate known-unknown claims.
    /// Runs after Verify. Pure graph queries, no LLM.
    pub async fn reflect(&self, graph: &KnowledgeGraph, ws: &str) -> ReflectResult {
        // 1. For each entity_type with enough instances:
        //    Count co-occurrence of claim_type pairs
        //    Store as structural_patterns
        
        // 2. For each pattern above min_frequency:
        //    Find entities matching condition but missing expected
        //    Generate known_unknown claims
        
        // 3. Resolve any previously-open known_unknowns
        //    that now have matching claims
        
        // 4. Return summary for health scoring
    }
}
```

### New MCP Tool

```
Tool: "gaps"
Description: "What knowledge is missing? Returns ranked known-unknowns."
Parameters:
  - workspace: String (required)
  - entity: String (optional — scope to specific entity)
  - min_confidence: Float (optional — default 0.70)
Returns:
  - List of known_unknowns with entity, expected type, confidence, and reason
```

### Health Score Integration

The existing health formula:
```
health = (freshness + consistency + coverage + provenance) / 4
```

Coverage dimension enhanced:
```
coverage = 1.0 - (open_known_unknowns / total_expected_claims).min(1.0)
```

This means: the more gaps the graph knows about but hasn't filled, the lower the health score. Filling gaps directly improves health.

---

## Recommended Build Sequence

| Priority | What | Effort | Impact |
|----------|------|--------|--------|
| P0 | `structural_patterns` CozoDB relation + Datalog pattern discovery | 1-2 days | Core mechanism |
| P0 | `known_unknowns` relation + gap claim generation | 1-2 days | The feature itself |
| P1 | `gaps` MCP tool | 4-5 hours | Agent-facing interface |
| P1 | Health score integration (coverage enhancement) | 2-3 hours | Gap claims affect health |
| P2 | Pattern dashboard in compiled artifacts | 1 day | Human-readable gap reports |
| P3 | Branch-scoped gap analysis ("what's missing in this branch vs main?") | 1 day | Knowledge PR quality gates |

**Total estimated effort: ~1 week for core, ~2 weeks for full feature.**

---

## Related Research Worth Reading

| Paper | Year | Relevance |
|-------|------|-----------|
| Belasco et al., "Representing Knowledge Gaps Effectively" (PAKM 2004) | 2004 | Only prior work on gap representation in a KB (Cyc). 22 years old, different architecture. |
| Galarraga et al., AMIE/AMIE+/AMIE3 (VLDB/WWW) | 2013-2020 | Rule mining from KG structure. Closest to pattern discovery, but output is ephemeral. |
| Darari, Razniewski et al., "Completeness Statements" (ISWC 2014) | 2014-2023 | Storing completeness annotations in RDF. Manual, not autonomous. |
| Razniewski et al., "Completeness, Recall, and Negation in Open-World KBs" (ACM Computing Surveys) | 2023 | Best survey of KG completeness approaches. Confirms no autonomous gap detection exists. |

---

## Open Questions

1. **Pattern granularity:** Should patterns be computed per-entity-type only, or also per-entity-type + relation-type? (e.g., "services that depend on Redis" as a sub-pattern)
2. **Pattern decay:** Should patterns weaken over time if the graph structure changes, or are they recomputed fresh each cycle?
3. **Human override:** Should users be able to dismiss gap claims ("this service legitimately has no auth — it's internal only")?
4. **Confidence calibration:** The pattern frequency (e.g., 92%) becomes the gap claim confidence. Should there be a secondary calibration based on how often gap claims turn out to be real vs. false positives?
5. **Cross-workspace patterns:** In multi-workspace deployments, should patterns from one workspace inform gap detection in another?
