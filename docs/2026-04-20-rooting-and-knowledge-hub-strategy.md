# Rooting + Knowledge Hub: Complete Strategy & Research Record

**Date:** 2026-04-20
**Status:** Strategic research complete. Ready for spec + build.
**Author:** Naveen + Claude (CTO session)
**Scope:** Captures the full arc from Phase 4/5 dashboard analysis through the discovery of Rooting as an atomic novel primitive, with all research evidence preserved.

---

## 0. Executive Summary

- **The "GitHub for knowledge" hub alone is NOT revolutionary.** It's a sound productization pattern (HuggingFace + GitHub + npm analog). Table stakes.
- **Rooting is revolutionary.** It is an atomic new operation on knowledge graphs with zero verified prior art.
- **Hub + Rooting together** is the moat: every pack carries a deterministic, verifiable quality certificate no competitor can produce.
- **Architecture:** local-first (`root compile` runs Rooting locally) → push compiled claims + certificates to cloud dashboard via `root sync`. Source never leaves the machine.
- **Build path:** 6 weeks, 1 engineer, inserts as Phase 3.5 into the existing 9-phase pipeline.
- **Novelty risk residual:** 4 papers still to read defensively; USPTO/EPO patent sweep not yet performed.

---

## 1. Phase 4/5 Dashboard Plan (User's Initial Proposal)

### The Dashboard Feature Set

**1. Global Knowledge Hub (Discovery)**
- Search engine across all public KnowledgePacks
- Trending graphs showing pack popularity via agent connections
- Verified badges for official packs (e.g., `django/official`)

**2. Personal Dashboard (My Space)**
- Knowledge Library of private/public repos
- Sync status for local-compiled pushes vs. cloud sources
- Agent Connections (Claude, ChatGPT, Perplexity OAuth list)

**3. KnowledgePack View (Repo Page)**
- Auto-rendered `knowledge.card.md`
- Semantic Tree Browser by Entity Type (Services, Hooks, People)
- 3D Graph Explorer for fact relationships
- Connect Button with CLI command and Web Connector links

**4. Knowledge PR (Semantic Diff)**
- Fact Diffs, not code diffs (`+ New Claim`, `~ Modified Claim`)
- Conflict resolution for contradictions
- Visual highlights of contradictions

**5. Agent Forge (One-Click Setup)**
- One-click Connect to Claude for Desktop/Claude.ai
- Custom SDK Endpoints for Python/JS agents

### The "Sync to Agent" Flow

1. **Discover** — Dave searches "React 19", finds `sarah/react-19-patterns` (98% Health Score)
2. **Connect** — Developer: `root hub connect sarah/react-19` (zero-latency bridge); SaaS: "Add to Claude" OAuth
3. **Use** — Dave asks Claude "how do I handle form actions?"; Claude pings ThinkingRoot Hub; hub returns Pure Gold claim with citation
4. **Contribute** — Dave forks Sarah's pack, fixes a claim, opens Knowledge PR; Sarah reviews Fact Diff, merges

### CTO Critique: Where Dashboard Plan Was Right

1. Fact Diffs already designed (`root diff --branch` + `--sync` posts Knowledge PRs per phase-4-cloud-cli-spec)
2. Agent Connections map cleanly to `mcp_config.rs` + OAuth extension
3. Semantic Tree Browser maps to CozoDB schema (`entities` relation has `type: String`)
4. Health score badges — 4-dimension formula (freshness 30% / consistency 30% / coverage 20% / provenance 20%) already runs in `thinkingroot-verify`
5. Separation of Knowledge Architect vs. Agent Developer personas is correct

### CTO Critique: Where to Push Back

1. **Reflexive Knowledge (the only un-copiable feature) was buried.** Zero prior art verified. Must be a top-level dashboard concept, not hidden. Every pack page should show Coverage ring, Known Unknowns list, "Fill this gap" CTA.

2. **GTM sequencing contradicted own stored strategy.** Memory says "Embedded SDK first (0.117ms) → self-hosted → cloud → edge cache. Don't chase remote first." Leading with central hub pitch pulls against this. Headline metric should be "Your graph, your latency" — showing embedded=0.117ms, self-hosted=5ms, cloud=100ms tiers.

3. **`thinkingroot-hub` crate needs scoping discipline.** OSS portion must be a thin client (~2K LOC):
   - `HubClient` — HTTP client to `api.thinkingroot.dev`
   - `PackDescriptor` — metadata types
   - `KnowledgeFormat` — wire format
   - `OAuthCallbackServer` — ephemeral localhost for `root login`
   - `SearchClient` — proxy to cloud search
   - Out of scope (private repo): pack storage, trending/ranking, webhooks, billing, web UI

4. **Fork/PR flow has unaddressed conflict resolution at scale.** Current `thinkingroot-branch/src/merge.rs` handles single-author KVC at confidence delta > 0.15. Marketplace scale needs: claim-level merge queue, contradiction-as-blocker policy, trust-weighted auto-merge (maintainer vs. drive-by).

5. **"Verified Badges" is a governance rabbit hole.** Who decides `django/official`? Recommendation: **attested provenance** instead — cryptographic signatures via DNS TXT or `.well-known`. Extends existing `thinkingroot-ground` phase.

### Reflexive Knowledge CTO Assessment

- Pure Datalog pattern discovery (no LLM in hot path) = correct engineering call
- Minimum sample threshold (30 entities, 70% co-occurrence) is appropriately conservative
- Integration into existing health formula (coverage dimension) = excellent, no new mental model
- Open questions flagged:
  - **Pattern decay** → recommendation: patterns recompute fresh each cycle, with `pattern_history` relation tracking drift
  - **Confidence calibration** → add `false_positive_rate` field updated by human dismissals; true confidence = `pattern_frequency × (1 - fp_rate)`
  - **MCP tool naming** → rename `gaps` to `known_unknowns` or `what_am_i_missing` for better agent ergonomics
- Recommended ordering: Reflect (2 weeks) → Phase 4 cloud CLI (3 weeks) → Web dashboard MVP (4 weeks) → Hub/marketplace (Phase 4.5)

---

## 2. The Honesty Pivot: Creation vs. Combining

User's challenge: *"i dont wnat just combining ..you more capable than this read all evrything use all you skills and agnets..create revaltinary world first thak makes evryone love and duse"*

User's deeper challenge: *"is creation or combining answer hsort?"*

Claude's honest answer on the first proposal (Crucible): **80% combining, 20% creation.** The ingredients existed (Popper falsification 1959, Necula proof-carrying code 1996, PRISM 2025 narrow-domain certificates, SHACL W3C 2017, TDD / property testing). The one genuinely new atom was "per-claim auto-generated executable predicates that re-run for LLM-extracted memory."

**Honest framing:** True atoms in AI are rare — one per ~decade (Attention 2014, Transformer 2017, Diffusion 2020). Most "revolutionary" products are novel arrangements. A single-session brainstorm is extremely unlikely to produce a true atom.

---

## 3. Research Evidence — 4 Parallel Agents

### Agent 1: Academic Frontier 2024-2026 (Unproductized Primitives)

Top 15 unproductized primitives found:

| # | Primitive | Paper | Key Innovation | Why Not Productized |
|---|---|---|---|---|
| 1 | Reasoning Zones on Signed Belief Graphs | [arXiv 2510.10042](https://arxiv.org/abs/2510.10042) | Contractive confidence-propagation with maximal balanced high-confidence subgraphs | Pure theory, no released code |
| 2 | Counterfactual Validation Loop on Causal KG | [arXiv 2509.14435](https://arxiv.org/abs/2509.14435) | Programmatic negation + downstream effect simulation | Requires pre-built causal (not correlational) KG; 5× slower |
| 3 | Tensor Logic (Datalog ≡ Einsum) | [arXiv 2510.12269](https://arxiv.org/abs/2510.12269) + [2601.17188](https://arxiv.org/abs/2601.17188) | Every Datalog rule body = tensor-join + project, GPU-native | Needs new compiler/runtime; only 2K-node demos |
| 4 | Sleep-Time Consolidation with Gist Extraction | [arXiv 2601.09913](https://arxiv.org/abs/2601.09913), [LightMem arXiv 2510.18866](https://arxiv.org/abs/2510.18866) | Offline "sleep" replays episodes, extracts gists, decays details (117× token reduction) | Research code only, no vendor implementation |
| 5 | SYNAPSE Spreading-Activation + Lateral Inhibition | [arXiv 2601.02744](https://arxiv.org/abs/2601.02744) | Dual-layer graph dynamics with temporal decay (+7.2 F1 on LoCoMo, -95% tokens) | Iterative graph dynamics engine needed at query time |
| 6 | Scale-Free Self-Organizing KG via Criticality | [arXiv 2503.18852](https://arxiv.org/abs/2503.18852) | Agentic loop organizes into scale-free state with emergent hubs | Materials-science only, 3.8K nodes |
| 7 | Episodic Memory as Single-Exposure Primitive | [arXiv 2502.06975](https://arxiv.org/abs/2502.06975) | Event-bound, context-tuple indexed (not embedding) | Position paper only |
| 8 | Proof-Carrying Artifacts (PRISM) | [arXiv 2510.25890](https://arxiv.org/abs/2510.25890) | Stratified constraint certificates linking facts to KG triples | Heavy theorem-prover integration; automotive AUTOSAR only |
| 9 | Adaptive (Generative) Hopfield Associative Memory | [arXiv 2511.20609](https://arxiv.org/abs/2511.20609) | Queries as generative variants with learned similarity | Tabular/image only, no text-RAG |
| 10 | Intent-Aligned Retrieval + Missing-Slot Filter (MemGuide) | [arXiv 2505.20231](https://arxiv.org/abs/2505.20231) | Retrieval conditioned on parsed task-slot schema (+11% MS-TOD) | "Code upon acceptance" — not public |
| 11 | Bayesian Weighted-Authority Belief Propagation (BEWA) | [arXiv 2506.16015](https://arxiv.org/abs/2506.16015) | Claims as Bayesian variables with author-credibility edges | No released code, no author-reputation graph exists |
| 12 | N-ary Hyperedge Claims (HyperGraphRAG) | [arXiv 2503.21322](https://arxiv.org/abs/2503.21322) | Facts as hyperedges over n entities, bipartite storage | 2× storage; Neo4j lacks native hyperedges |
| 13 | Function-Token-Gated KV Memory | [arXiv 2510.08203](https://arxiv.org/abs/2510.08203) | Function tokens gate majority of feature activations | Mechanistic paper, no runtime |
| 14 | Retroactive-Interference-Aware Forgetting | [arXiv 2603.00270](https://arxiv.org/abs/2603.00270) | LLMs have opposite forgetting curve vs humans (primacy protection) | Brand new finding, no framework |
| 15 | Expected-Free-Energy Retrieval (Active Inference) | [arXiv 2504.14898](https://arxiv.org/abs/2504.14898), [2508.05619](https://arxiv.org/abs/2508.05619) | Retrieval minimizes expected free energy = info gain + goal utility | Generative world model needed; robotics/RL only |

**Top 3 build candidates for ThinkingRoot (CozoDB fit):** Reasoning Zones, Sleep-Time Consolidation with Gist Extraction, Counterfactual Validation Loop.

### Agent 2: Competitive Pain Audit (April 2026)

Exhaustive review of 17 shipping systems.

**Per-system headline complaints:**
- **Mem0** — 97.8% junk rate ([issue #4573](https://github.com/mem0ai/mem0/issues/4573), 38 clean of 10,134 audited); graph memory removed from OSS April 16 2026; [issue #4248](https://github.com/mem0ai/mem0/issues/4248) graph writes blocked; [issue #3944](https://github.com/mem0ai/mem0/issues/3944) LOCOMO benchmark unreproducible
- **Zep/Graphiti** — LongMemEval 71.2% (GPT-4o); [Community Edition killed April 2, 2025](https://blog.getzep.com/announcing-a-new-direction-for-zeps-open-source-strategy/); 3-system minimum self-host; $50-200 per 10K docs
- **Letta/MemGPT** — [Context-window reset bug LET-7991](https://github.com/letta-ai/letta/releases) (fixed v0.16.7 Mar 31 2026); [Docker phones home #2444](https://github.com/letta-ai/letta/issues/2444); "too complicated to integrate" #480, #2347
- **Cognee** — [cognify() hangs forever #2119](https://github.com/topoteretes/cognee/issues/2119); [search endpoint hangs #2456](https://github.com/topoteretes/cognee/issues/2456); [Docker broken #2274](https://github.com/topoteretes/cognee/issues/2274)
- **LangMem** — p95 = 59.82s per third-party benchmark; [GRAPH_RECURSION_LIMIT #133](https://github.com/langchain-ai/langmem/issues/133); `ConversationBufferMemory` deprecated
- **Supermemory** — [Mar 6 2026 outage](https://blog.supermemory.ai/incident-report-march-6-2026/); [dashboard + Claude Code plugin broken #801](https://github.com/supermemoryai/supermemory/issues/801)
- **Chroma** — [silent data loss in Docker #6654](https://github.com/chroma-core/chroma/issues/6654); [memory leak #5843](https://github.com/chroma-core/chroma/issues/5843)
- **Pinecone** — 200-800ms cold-starts; "5-7 vendor relationships and months of engineering" to assemble agent memory
- **Neo4j GraphRAG** — [$50-200 per 10K-doc indexing cost](https://www.paperclipped.de/en/blog/graph-rag-production/); [latency thread at 100GB](https://community.neo4j.com/t/how-to-get-accurate-retrieval-process-with-less-latency-with-huge-network-in-neo4j/76231)

**The SINGLE problem every shipping system fails at: WRITE QUALITY.**

Evidence:
- Mem0 97.8% junk after 32 days
- [Weaviate's own blog "Limit in the Loop"](https://weaviate.io/blog/limit-in-the-loop) admits: "naive memory decays — requires write control, deduplication, reconciliation, amendment, purposeful forgetting"
- [Chanl.ai memory silent failure mode](https://www.chanl.ai/blog/memory-silent-failure-mode): "agents don't express uncertainty when retrieval fails — output looks the same whether retrieved or fabricated"

Everyone solved *storage*. Everyone punts on *write-gate quality*. The extractor is a prompt. The prompt lies. There is no feedback loop.

**What everyone has silently given up on:** Self-hosted OSS at feature parity with managed product. Zep CE killed. Mem0 paywalled graph. Letta Docker phones home. Supermemory cloud-only.

**What makes developers roll their own:**
1. Assembly tax (5-7 services wired together)
2. Silent-wrong writes (confident garbage)
3. Vendor exit / licensing whiplash

### Agent 3: Unused Theoretical Foundations

Top 3 ranked by revolutionary × buildable-in-6-weeks:

1. **Modern Hopfield cleanup layer (Ramsauer 2020)** — score 80. Drop-in after vector retrieval. One file of CUDA. Provable exponential capacity. Zero production implementations as explicit memory stage.

2. **HRR / VSA structured-binding substrate (Tony Plate 1995)** — score 72. Every claim `(subject, predicate, object, source, confidence)` as single composed HRR vector. Queries are unbinding operations. The "Transformer attention of agent memory."

3. **Defeasible logic over CozoDB Datalog** — score 63. Rule-priority + defeater relation. Contradictions stratify instead of overwrite.

**Hybrid thesis (strong candidate):** HRR-composed claim vectors + modern Hopfield cleanup + defeasible Datalog truth layer. Write-time: compose. Read-time: unbind → Hopfield cleanup → Datalog verification/stratification.

Other notable frameworks with novelty but higher barriers:
- Pearl's do-calculus / SCMs — 9/10 revolutionary but 4/10 buildable (causal DAG discovery from LLM-extracted facts is unsolved)
- Structure-Mapping (Gentner) — 8/10 revolutionary, 7/10 buildable (retrieval by structural not cosine similarity)
- Persistent Homology / TDA — 8/10 revolutionary, 6/10 buildable (epistemic communities as H0, contradictions as H1)

### Agent 4: Developer Pain Deep Scan

Top 20 complaints with citations. Selected headlines:

1. **Claude Code zero cross-session memory** — [issue #14227 Dec 2025](https://github.com/anthropics/claude-code/issues/14227)
2. **Memory files silently wiped on upgrade** — [issue #38459 Mar 2026](https://github.com/anthropics/claude-code/issues/38459)
3. **ChatGPT memory hallucinates/duplicates** — [OpenAI Community thread](https://community.openai.com/t/chatgpt-memory-broken-at-the-moment/1108272)
4. **Catastrophic memory wipes unacknowledged** — [OpenAI critical data loss](https://community.openai.com/t/critical-chatgpt-data-loss-engineering-fix-urgently-needed/1360675)
5. **LangGraph checkpointer breaks in parallel** — [issue #3380](https://github.com/langchain-ai/langgraph/issues/3380)
7. **Context rot empirically measured** — [Chroma Research](https://www.trychroma.com/research/context-rot): performance collapse 300 → 113K tokens across 18 models
11. **Hallucination detectors fail on real outputs** — [arXiv 2512.15068](https://arxiv.org/abs/2512.15068): 100% FPR on HaluEval
16. **Cross-user memory leakage in multi-tenant** — [arXiv 2505.18279](https://arxiv.org/html/2505.18279v1)
20. **Karpathy explicitly names unbuilt primitive:** *"conversations flow into daily logs, daily logs get compiled into a wiki, and the wiki gets injected back into the next session, so agents build their own knowledge base over time"* — [Year in Review 2025](https://karpathy.bearblog.dev/year-in-review-2025/)

**Top 3 open problems (HIGH freq + HIGH severity + ZERO good solution):**

1. **Durable, corruption-resistant cross-session memory for coding agents** (issues #1, #2, #4 are the same gap)
2. **Memory layer that survives context rot — log → compiled knowledge → injected snippet** (Karpathy-named, empirically proven problem)
3. **Shared + access-controlled multi-agent / multi-user memory** (#15 duplicate work + #16 tenancy leaks)

---

## 4. Intermediate Proposal: "Crucible" (Then Honestly Retracted)

Initial synthesis: deterministic adversarial write-gate with 5 probes (provenance, contradiction, predicate, topology, temporal). Every claim must survive before admission. Survival status stored as cryptographic certificate.

**Honest retrospective grade:** 80% combining, 20% creation.

Prior-art components:
- Popper's falsification → 1959
- Proof-carrying code → Necula 1996 (OSDI)
- PRISM proof-certificates → 2025 (automotive-narrow)
- SHACL validators → W3C 2017
- TDD / property testing → decades old

The one genuinely new atom: per-claim auto-generated executable predicates that re-run for LLM-extracted memory. But that was a small part of the composite.

User's sharpening: "go again and actually hunt for a *new atom*, not a new arrangement?"

---

## 5. The Atomic Hunt — Oneiric Graph

Candidate atomic primitive: **Oneiric Graph / Dream-Verify Loop**

Definition: A knowledge graph operation where:
1. The graph autonomously SAMPLES combinations of existing claims (sharing entities)
2. PROPOSES new hypothetical claims from those samples (templates or LLM composition)
3. Runs proposed claims through DETERMINISTIC probes (provenance match, contradiction vs. high-confidence, executable predicate against source corpus, temporal consistency)
4. ADMITS survivors as persistent first-class "derived" claims
5. Graph grows without new source documents

### Prior Art Search Verdict

**Oneiric Graph as composite: ZERO verified full prior art.**
**3 of 4 components: anticipated.**
**1 component: genuinely atomic and new.**

| Component | Prior art |
|---|---|
| Sampling existing claims sharing an entity | SciAgents (2024), NELL, AMIE rule-matching |
| Compositional generation of new hypothetical claims | NELL Horn-clause inference (2010-present), Buehler Agentic Deep Graph Reasoning |
| Admission of survivors as persistent graph members | NELL Knowledge Integrator, OpenCog PLN, Datalog materialization |

**Closest complete system: [NELL (CMU, 2010-present)](https://www.cs.cmu.edu/~tom/pubs/NELL_aaai15.pdf)** — independently hits 3 of 4 operations. Knowledge Integrator receives candidate beliefs from reading + inference modules, assigns confidences, promotes to persistent KB members.

**What NELL is missing vs. Oneiric Graph:**
- No executable-predicate re-evaluation against ORIGINAL source corpus
- No explicit contradiction check against high-confidence claims as deterministic gate
- No explicit temporal consistency probe on derived beliefs
- Sampling is implicit (rule matching), not a policy

**Other close-but-distinct systems:**

| System | Distinguishing gap |
|---|---|
| [Graph-PReFLexOR / Agentic Deep Graph Reasoning (Buehler, MIT)](https://arxiv.org/html/2502.13025v1) | No deterministic probes — LLM-native coherence only |
| [SciAgents (Ghafarollahi & Buehler, 2024)](https://arxiv.org/abs/2409.05556) | Verification is LLM Critic agent; outputs are research proposals not graph-admitted claims |
| [Generative Logic (arXiv 2508.00017)](https://arxiv.org/html/2508.00017v4) | Domain is formal math axioms, not document-sourced KG |
| [DeepDive / Snorkel](http://deepdive.stanford.edu/) | Probabilistic factor-graph inference, not deterministic probes; no source re-query |
| Cyc | Abductive hypotheses stored as problems-in-progress, not promoted via deterministic gate |
| OpenCog PLN | Forward-chains with probabilistic truth-value fusion, not deterministic probes |
| Microsoft GraphRAG | No composition of existing claims into new claims |
| YAGO / DBpedia / Wikidata | OWL-DL type reasoning only; no temporal/contradiction/source probes |

---

## 6. Rooting — The Atomic Novelty

**Genuinely new atomic operation with zero verified prior art:**

> **Rooting:** When a new claim D is derived from existing claims A and B, execute D's predicate against the *original source corpus* from which A and B were extracted. Admit D only if it survives this deterministic re-query.

### Why This Is Atomic

Every other system's knowledge flow is one-way: documents → extraction → graph. Rooting makes it **bidirectional** — the graph must continuously prove each derived claim projects back to its documentary origin.

This is the "new operation" test — not decomposable into prior primitives:
- NELL verifies via rule+pattern confidence fusion — no web text re-query
- DeepDive consumes source once, never re-queries
- Nemori's "Predict-Calibrate" checks predictions against source but outputs summaries, not admission-gated derived claims
- OpenCog PLN uses probabilistic fusion — no deterministic source re-execution
- Generative Logic verifies theorems deterministically — but "corpus" = axioms

### Why Possible on ThinkingRoot Specifically

ThinkingRoot's Phase 3 (Ground) already stores `source_span` as byte ranges — first-class metadata. Every other system treats provenance as audit-trail; ThinkingRoot can treat it as a **re-executable address**. That existing architectural decision enables the atom.

### What Rooting Unlocks

1. **Derived knowledge with grounding guarantees** — inferred claims carry same provenance contract as extracted claims
2. **Defensible against counterfactual generation** — LLM can propose plausible derived claim; Rooting asks "point to bytes in source." No bytes → claim dies.
3. **Time-decayable derivations** — periodic re-runs catch drift; refactored source auto-demotes stale claims
4. **The only operation competitors cannot copy quickly** — requires deterministic predicates + byte-addressable sources + re-execution harness. ThinkingRoot's Phase 1-3 gives the second for free.

---

## 7. Novelty Verification — 2 Papers Read In Full

### Paper 1: [Complex Logical Hypothesis Generation (arXiv 2312.15643v3)](https://arxiv.org/abs/2312.15643)

**What it does:** Generates first-order logical hypotheses in DNF over KG relations. Example: `Occupation(V, Actor) ∧ BornIn(V, LosAngeles)`.

**What it lacks vs. Rooting:**
- Hypotheses are **ephemeral query-time artifacts** (never admitted back to graph)
- Verification is pure **graph Jaccard**: `r(h, o) = |[[H]]_G ∩ O| / |[[H]]_G ∪ O|`
- Paper never mentions "source documents", "text corpus", "provenance", or "re-grounding"
- Explicit quote: *"We adopt the open-world assumption of knowledge graphs"*

**Verdict: DOES NOT FORECLOSE.**

### Paper 2: [Graph-based Agent Memory Survey (arXiv 2602.05665v1)](https://arxiv.org/html/2602.05665v1)

**What it does:** Taxonomic map of graph-based agent memory. Section VII ("Memory Evolution") categorizes mechanisms into:
1. Internal Self-Evolving — introspective refinement via consolidation/reorganization
2. External Self-Exploration — environment-grounding

Systems surveyed: Zep, Mem0, TReMu, MemoTime, HyperGraphRAG, Graphiti, AriGraph, GraphRAG, LightRAG, MemTree, SGMEM, Optimus-1, KG-Agent, H-MEM, G-Memory, LiCoMemory.

**Per-system re-execution check:** None of the 16 systems use deterministic source-corpus re-execution. Mechanisms are all: structural/temporal, embedding/similarity, LLM self-consistency, or environment-feedback.

**Rooting is a fifth paradigm that's a blank spot on the survey's map.**

**Verdict: DOES NOT FORECLOSE — actually strengthens novelty claim.**

### Combined Verdict

**Rooting remains novel against both verified papers.** Zero foreclosure.

### Adjacent Concepts Worth Citing

- **TReMu** — neuro-symbolic Python on graph-resident timelines. Close in spirit (deterministic code as verification) but wrong substrate (graph projection, not source spans)
- **Bi-temporal invalidation (Zep/Graphiti)** — admission/retirement by time, not source re-projection
- **Memory consolidation / schema induction** — abstraction over similar events, not derivation of new facts requiring re-grounding

### Residual Risks (Not Yet Closed)

1. **USPTO/EPO patent sweep not performed.** Requires counsel (~$2-5K). Mandatory before public novelty claim.
2. **4 defensive reads recommended:** TReMu, MemoTime, AriGraph, Mem0 (primary sources, not just survey)
3. **Stealth products unverified:** Graphlit, Hawksight Semantica claim abductive hypothesis generation in marketing. Could not verify from repos.

---

## 8. Rooting Inside ThinkingRoot SaaS

Local-first architecture preserved. `root compile` runs Rooting locally. `root sync` pushes compiled claims + Rooting certificates to cloud dashboard. Source code never leaves local machine.

### 5 Entry Points

**1. Compile-Time Derivation (OSS + SaaS)**

After Phase 9 (Reflect) identifies a structural pattern with a gap, the pipeline proposes a derived claim and Rooting gates admission.

```
Extract claim A: "AuthService issues JWT tokens"  (source: auth.rs:12-28)
Extract claim B: "PaymentService calls AuthService before transactions"  (source: payment.rs:45-67)
─────────────────────────────────────────────────
Reflect phase proposes derived claim D:
  "PaymentService transactions require a valid JWT"
  parents: [A, B]
  predicate: { lang: "rust-ast", query: "call_expression ∧ precedes(jwt_check)", scope: ["auth.rs", "payment.rs"] }
─────────────────────────────────────────────────
Rooting phase:
  → execute predicate against source_span(A) ∪ source_span(B)
  → match found at payment.rs:48 (validate_token call)
  → admission_tier: Rooted
  → certificate: blake3:a3f12d…
```

**2. Agent Write-Back (SaaS Feature)**

When Claude/Cursor/Cline writes via `contribute` MCP tool:

```
POST /api/v1/contribute
  claim: "Rate limit is 100 req/sec per tenant"
  predicate: { lang: "regex", query: "rate_limit\s*=\s*100", scope: ["config.yaml"] }
  proposed_parents: [claim_id_of_ratelimiter_service]

→ Rooting executes regex against config.yaml:17-19
→ Match found
→ Admitted with agent attribution
→ Response: { admission_tier: "Rooted", certificate: "...", latency_ms: 12 }
```

**3. Knowledge PR Admission (SaaS Dashboard)**

Existing Knowledge PR flow gains Rooting column:

```
Knowledge PR #14: sarah → main
  + 28 new extracted claims (all native provenance)
  + 12 new derived claims:
      10 Rooted (certificates verified)
       2 Quarantined (predicate failed — review required)
  Rooting survival rate: 83%  (threshold: 80% ✓)

  [ Merge ]  [ Re-run Rooting ]  [ View proofs ]
```

CI gate configurable per-pack via existing `[merge]` config.

**4. Continuous Re-Rooting (SaaS Background Job)**

Daily sweep re-executes every derived claim's predicate against current source corpus:

| Outcome | Action |
|---|---|
| Predicate still matches | `last_rooted_at` timestamp updated |
| Predicate no longer matches | Claim auto-demoted to Stale; notification to pack maintainer |
| Source document deleted | Claim auto-demoted to Orphaned; appears in pack health report |

**5. Hub / Marketplace (Phase 4.5)**

Public pack metadata includes Rooting Certificate:

```
sarah/react-19-patterns                    ★ 2.3k
Knowledge Pack · MIT · Rust+React
─────────────────────────────────────────
Health:     96%   ●●●●●●●●●○
Rooting:    94%   ●●●●●●●●●○   ← new
Freshness:  1h ago
Claims:     3,412 (2,891 rooted, 412 extracted, 109 quarantined)
```

### Code Changes

| Layer | Change | OSS/Private |
|---|---|---|
| `thinkingroot-core/src/types/claim.rs` | Add `derivation_proof`, `admission_tier`, `predicate`, `last_rooted_at` | OSS |
| `thinkingroot-crucible/` (new crate ~2500 LOC) | Trial orchestrator + 5 probes + certificate generation | OSS |
| `thinkingroot-extract/src/focused_prompts.rs` | Extend LLM prompts to emit executable predicates | OSS |
| `thinkingroot-serve/src/engine.rs` | Query API gains `trust_tier` and `include_proof` flags | OSS |
| `thinkingroot-serve/src/mcp/` | New tools: `query_rooted`, `rooting_report` | OSS |
| Private Phase 4 repo | Re-Rooting background worker, Rooting dashboard UI, hub Rooting scores | Private |

---

## 9. Audience Love Analysis — Why It Spreads

### The AI Startup Engineer (Dave — Mem0/Zep refugee)
- **Pain:** 97.8% junk rate, confident hallucination, silent wipes
- **Love moment:** First time they inspect a quarantined claim and see reason: *"predicate `stripe_import_regex` returned no matches against config/payment.toml:12-18"*
- **Conversion:** Replace Mem0 with ThinkingRoot, compare Rooting scores. Mem0 can't produce a score. Over.

### The Knowledge Architect (Sarah — pack author)
- **Pain:** No metric to prove pack quality
- **Love moment:** Her pack hits 99% Rooting, tweets screenshot, traction because nobody else has a number to show

### The Enterprise AI / Compliance Lead (CISO)
- **Pain:** "Prove your AI memory isn't making things up" — no vendor can
- **Love moment:** Auditor asks "how do you know this fact is still true?" — click "Re-run Rooting" → deterministic verification in 12ms. Meeting ends early.

### The Researcher / Academic
- **Pain:** Memory systems all benchmark on recall; nobody benchmarks write quality
- **Love moment:** First arXiv submission citing Rooting survives peer review. Pattern propagates.

### The Developer Tooling Audience (HN/Twitter/Reddit)
- **Pain:** RAG hallucinates, Graph RAG slow, agents confidently reference missing facts
- **Love moment:** Someone tweets *"ThinkingRoot added tests to facts"* — cognitively adhesive, 4-word explanation

### The Procurement / Buyer
- **Pain:** Every vendor claims quality, no way to compare
- **Love moment:** First enterprise RFP requiring "Rooting score ≥ 90% over rolling 30 days" — Mem0/Zep/Letta out, ThinkingRoot in by default

---

## 10. The Viral Narrative

### Tagline Options (ranked)

1. **"Every fact in ThinkingRoot has roots. Not maybe — provably."** (brand-consistent)
2. **"TDD for AI memory."** (4 words, borrowed credibility)
3. **"The first AI memory with a credit score."** (procurement-friendly)
4. **"Mem0 stores what the AI says. ThinkingRoot stores what the AI can prove."** (competitor-direct)
5. **"Hallucination is a choice. We chose not to."** (provocative)

**Recommendation:** #2 for developer virality, #3 for enterprise, #1 for product taglines.

### The 40-Second Demo Video

| Second | Frame |
|---|---|
| 0-5 | Claude agent answers: "PaymentService uses Stripe for card processing." (Mem0 logo, green checkmark) |
| 6-10 | Developer refactors payment.rs — replaces Stripe with Adyen. Git commit. |
| 11-15 | Same question asked again. Mem0 still says Stripe. Agent confidently repeats stale fact. |
| 16-20 | Cut to ThinkingRoot. Same question. Dashboard shows claim auto-demoted to Stale. |
| 21-25 | Drill into proof: "predicate `stripe_import` ran against payment.rs:47 — no match at 2026-04-20T09:14:00Z." |
| 26-30 | Re-rooting triggered. New claim Rooted: "PaymentService uses Adyen." Certificate chain shown. |
| 31-40 | Tagline: *"Every fact has roots. Or it doesn't stay."* |

### What It Changes About the Category

| Before | After |
|---|---|
| "Which memory system has the best recall?" | "Which memory system has verifiable write quality?" |

Recall benchmarks (LongMemEval, LoCoMo) favor whoever optimizes last. Write-quality benchmarks with source-re-execution favor the system that architected it from day one. That's the moat.

---

## 11. Implementation Path

### Week-by-Week (6 weeks, 1 engineer)

| Week | Work |
|---|---|
| 1 | Probe schemas, trial orchestrator skeleton, CozoDB relations (`trial_verdicts`, `verification_certificates`, `rejected_claims`) |
| 2 | Provenance + Contradiction + Temporal probes (deterministic, easy wins) |
| 3 | Predicate generator (LLM extension) + rust-ast predicate runner |
| 4 | Topology probe (reuse Reflexive pattern code) + admission policy engine |
| 5 | Certificate hashing + MCP tool + CLI (`root crucible report` / `root rooting report`) |
| 6 | LongMemEval re-run with Rooting on/off (prove survival rate lift) + buffer |

### Defensive Actions (in parallel)

- **Week 1-2:** Primary-source reads of TReMu, MemoTime, AriGraph, Mem0 (close the remaining 4 papers)
- **Week 2-3:** Commission patent attorney USPTO/EPO/WIPO sweep; if clean, file provisional (~$3K for 12-month priority)
- **Week 4+:** Draft NeurIPS 2026 / SIGMOD technical abstract as academic defense

### `.claim` Wire Format (Portable Verification Certificate)

```json
{
  "statement": "PaymentService uses Stripe for card processing",
  "entities": ["PaymentService", "Stripe"],
  "source": { "uri": "payment_service.rs", "span": [47, 62], "hash": "blake3:..." },
  "predicate": { "lang": "rust-ast", "query": "...", "last_eval": "2026-04-20T09:14:00Z", "last_result": "pass" },
  "trial": { "provenance": "pass", "contradiction": "pass", "predicate": "pass", "topology": "pass", "temporal": "pass" },
  "certificate": "blake3:a3f..."
}
```

`.claimpack` = signed archive of `.claim` bundles. This is the actual "PDF for AI" — except every byte is verifiable.

---

## 12. What This Is NOT (Anti-Hallucination Guarantees)

1. **Does NOT eliminate hallucination.** Eliminates *unverified* hallucination. Makes hallucination loud instead of silent.
2. **Does NOT work without the 9-phase pipeline.** Contradiction/Topology probes require grounded, linked, reflexively-analyzed claims.
3. **Does NOT beat pure-LLM on easy benchmarks.** Admission tax is overhead on small clean datasets. Win curve kicks in at ~10K+ claims.
4. **Does NOT prevent predicate generation weaknesses.** Weak predicate → weak admission. Mitigation: log predicate diversity, flag always-pass predicates as suspicious.
5. **Does NOT handle subjective claims well.** "User prefers concise responses" has no deterministic probe. Stays in Attested tier.
6. **Does NOT beat Anthropic/OpenAI built-in memory on convenience.** DX is "slower than Mem0, more trustworthy than Mem0." Explicit trade-off.

---

## 13. Revolutionary Assessment (Final)

| Claim | Verdict |
|---|---|
| Dashboard / "GitHub for knowledge" alone is revolutionary | **No** — sound productization pattern, table stakes |
| Rooting alone is revolutionary | **Yes** — atomic new operation, zero verified prior art |
| Hub + Rooting together is revolutionary | **Yes** — first knowledge marketplace with deterministic verifiable quality scores per pack |

The revolution lives in **Rooting**. The hub is the distribution channel that makes Rooting visible. Without Rooting, the hub is me-too. Without the hub, Rooting is a technical curiosity.

Ship both. Know which one is the moat.

---

## 14. Immediate Next Steps

Three paths, ranked:

1. **Move 1 — Finish defensive reads (2 hours, free):** TReMu, MemoTime, AriGraph, Mem0 full-PDF reads. Close the loop before public novelty claim.
2. **Move 2 — Patent sweep (1 week, $2-5K):** USPTO/EPO/WIPO with counsel. File provisional if clean. 12-month priority.
3. **Move 3 — Start building (6 weeks, 1 engineer):** Ship Rooting as Phase 3.5. Measure derived-claim survival rate on real workspace. Own the benchmark nobody else can produce.

**Recommended order:** Move 1 (tonight) → Move 2 + Move 3 in parallel (Monday).

---

## 15. Local → Dashboard Flow (User-Confirmed Architecture)

```
LOCAL (every developer machine)                CLOUD (dashboard, Phase 4 private)
───────────────────────────────               ──────────────────────────────────
root compile ./my-repo                  →
  Phases 1-9 run                              
  Rooting gates derived claims           
  .thinkingroot/graph.db updated               
  Rooting certificates generated               

root sync                                →      Claims + certificates uploaded
                                                (source code NEVER uploaded)
                                                Cloud graph merged
                                                Dashboard updated
                                                Hub listing refreshed

                                         ←      Background re-rooting worker
                                                Daily predicate re-execution
                                                Staleness notifications
```

This is the existing open-core architecture (`docs/2026-04-10-open-core-architecture.md`) with Rooting added as an OSS-native capability. The dashboard is the *visualization layer* for what happens locally. The verification itself is local-first, cloud-amplified.

---

## 16. Citations Index

### Papers (verified)
- Belief Graphs / Reasoning Zones — [arXiv 2510.10042](https://arxiv.org/abs/2510.10042)
- Causal-Counterfactual RAG — [arXiv 2509.14435](https://arxiv.org/abs/2509.14435)
- CausalRAG — [arXiv 2503.19878](https://arxiv.org/abs/2503.19878) / [GitHub](https://github.com/hippoley/CausalRAG)
- Tensor Logic — [arXiv 2510.12269](https://arxiv.org/abs/2510.12269)
- Continuum Memory Architectures — [arXiv 2601.09913](https://arxiv.org/abs/2601.09913)
- LightMem — [arXiv 2510.18866](https://arxiv.org/abs/2510.18866)
- SYNAPSE — [arXiv 2601.02744](https://arxiv.org/abs/2601.02744)
- Self-Organizing Critical KG (Buehler) — [arXiv 2503.18852](https://arxiv.org/abs/2503.18852)
- Agentic Deep Graph Reasoning — [arXiv 2502.13025](https://arxiv.org/abs/2502.13025)
- SciAgents — [arXiv 2409.05556](https://arxiv.org/abs/2409.05556)
- Episodic Memory Missing Piece — [arXiv 2502.06975](https://arxiv.org/abs/2502.06975)
- PRISM Proof-Carrying — [arXiv 2510.25890](https://arxiv.org/abs/2510.25890)
- Adaptive Hopfield — [arXiv 2511.20609](https://arxiv.org/abs/2511.20609)
- MemGuide — [arXiv 2505.20231](https://arxiv.org/abs/2505.20231)
- BEWA — [arXiv 2506.16015](https://arxiv.org/abs/2506.16015)
- HyperGraphRAG — [arXiv 2503.21322](https://arxiv.org/abs/2503.21322)
- Function Tokens Memory — [arXiv 2510.08203](https://arxiv.org/abs/2510.08203)
- Transformers Remember First — [arXiv 2603.00270](https://arxiv.org/abs/2603.00270)
- EFE Planning — [arXiv 2504.14898](https://arxiv.org/abs/2504.14898)
- Complex Logical Hypothesis Generation — [arXiv 2312.15643](https://arxiv.org/abs/2312.15643)
- Graph-based Agent Memory Survey — [arXiv 2602.05665](https://arxiv.org/html/2602.05665v1)
- Generative Logic — [arXiv 2508.00017](https://arxiv.org/html/2508.00017v4)
- NELL — [AAAI 2015](https://www.cs.cmu.edu/~tom/pubs/NELL_aaai15.pdf) / [CACM 2018](https://dl.acm.org/doi/10.1145/3191513)
- DeepDive — [VLDB 2015](http://www.vldb.org/pvldb/vol8/p1310-shin.pdf)
- AMIE — [Paper](https://resources.mpi-inf.mpg.de/yago-naga/amie/amie.pdf)
- Collaborative Memory — [arXiv 2505.18279](https://arxiv.org/html/2505.18279v1)
- Semantic Illusion — [arXiv 2512.15068](https://arxiv.org/abs/2512.15068)
- When to use Graphs in RAG — [arXiv 2506.05690](https://arxiv.org/html/2506.05690v2)

### Production System Sources
- [Mem0 issue #4573 — 97.8% junk audit](https://github.com/mem0ai/mem0/issues/4573)
- [Mem0 issue #4248 — graph writes blocked](https://github.com/mem0ai/mem0/issues/4248)
- [Mem0 issue #3944 — LOCOMO unreproducible](https://github.com/mem0ai/mem0/issues/3944)
- [Zep OSS direction announcement](https://blog.getzep.com/announcing-a-new-direction-for-zeps-open-source-strategy/)
- [Letta issue #2444 — Docker phones home](https://github.com/letta-ai/letta/issues/2444)
- [Cognee issue #2119 — cognify hangs](https://github.com/topoteretes/cognee/issues/2119)
- [LangMem issue #133](https://github.com/langchain-ai/langmem/issues/133)
- [Supermemory Mar 6 2026 incident](https://blog.supermemory.ai/incident-report-march-6-2026/)
- [Chroma issue #6654 — silent data loss](https://github.com/chroma-core/chroma/issues/6654)
- [Weaviate — Limit in the Loop](https://weaviate.io/blog/limit-in-the-loop)
- [Neo4j GraphRAG latency thread](https://community.neo4j.com/t/how-to-get-accurate-retrieval-process-with-less-latency-with-huge-network-in-neo4j/76231)
- [Claude Code issue #14227 — zero memory](https://github.com/anthropics/claude-code/issues/14227)
- [Claude Code issue #38459 — memory wiped](https://github.com/anthropics/claude-code/issues/38459)
- [OpenAI Community — ChatGPT memory broken](https://community.openai.com/t/chatgpt-memory-broken-at-the-moment/1108272)
- [LangGraph issue #3380 — parallel breaks](https://github.com/langchain-ai/langgraph/issues/3380)
- [LangGraph issue #5790 — dev hardcoded memory](https://github.com/langchain-ai/langgraph/issues/5790)
- [Chroma Research — Context Rot](https://www.trychroma.com/research/context-rot)
- [Elastic — Context Poisoning](https://www.elastic.co/search-labs/blog/context-poisoning-llm)
- [HN 45439997 — The RAG Obituary](https://news.ycombinator.com/item?id=45439997)
- [HN 44701172 — Are we pretending RAG is ready?](https://news.ycombinator.com/item?id=44701172)
- [HN 46923543 — Coding agents replaced every framework](https://news.ycombinator.com/item?id=46923543)
- [Karpathy — Year in Review 2025](https://karpathy.bearblog.dev/year-in-review-2025/)
- [Agent Memory Wars (Medium, Jan 2026)](https://medium.com/@nraman.n6/agent-memory-wars-why-your-multi-agent-system-forgets-what-matters-and-how-to-fix-it-a9a1901df0d9)
- [Dev.to — Why LLM Memory Still Fails](https://dev.to/isaachagoel/why-llm-memory-still-fails-a-field-guide-for-builders-3d78)
- [Chanl.ai — Memory silent failure mode](https://www.chanl.ai/blog/memory-silent-failure-mode)

---

## 17. Related Docs In This Repo

- `docs/2026-04-19-reflexive-knowledge-architecture.md` — Reflect phase spec (Phase 9). Rooting integrates with Reflect (gap-filling derived claims go through Rooting before admission).
- `docs/2026-04-10-phase4-cloud-cli-spec.md` — `root login` / `root sync` / `root connect --webhook`. Rooting certificates travel on the sync wire.
- `docs/2026-04-10-open-core-architecture.md` — OSS vs SaaS feature split. Rooting core is OSS; continuous Re-Rooting worker is SaaS.
- `docs/2026-04-10-oss-vs-saas-feature-comparison.md` — complete feature matrix. Rooting adds a new row.
- `compile-retrieve-architecture.md` — 9-phase pipeline. Rooting inserts between Ground and Link as conditional Phase 3.5.

---

**End of record.** This document preserves the complete strategic analysis, research evidence, novelty verification, product integration plan, and go-to-market positioning from the 2026-04-20 CTO session. Nothing has been omitted.
