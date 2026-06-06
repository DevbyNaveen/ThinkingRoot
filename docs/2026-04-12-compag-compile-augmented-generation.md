# CompAG: Compile-Augmented Generation

**Date:** 2026-04-12
**Status:** Paradigm definition — ThinkingRoot is the reference implementation
**Prior art check:** arXiv full-text search, April 2026 — zero results for "Compile-Augmented Generation" or "CompAG"

---

## The Claim

**CompAG (Compile-Augmented Generation)** is a new paradigm for giving AI agents access to knowledge. It treats knowledge preparation as a *compilation problem*, not a retrieval problem.

ThinkingRoot is the first CompAG system.

---

## The Problem with RAG

Retrieval-Augmented Generation (RAG) is the dominant approach today. Its model is:

```
Query time:
  raw text chunks → embed → similarity search → dump into context → LLM figures it out
```

RAG pushes all understanding to the LLM at runtime, from unverified raw text. Every query re-does the same work. The LLM must resolve contradictions, assess staleness, infer types, and reconstruct relationships — all inside a single context window, under token pressure, with no guarantee of correctness.

The deeper problem: RAG is an interpreted approach. The source material is never transformed. It goes in raw, and whatever hallucinations or contradictions exist in the source material get passed directly to the model.

---

## The CompAG Approach

CompAG moves the hard work to compile time — before any query, before any agent session:

```
Compile time (once, offline):
  raw sources → parse → extract → verify → type → link → deduplicate → health-score → serve

Query time (fast, cheap, reliable):
  pre-verified typed claim + confidence + grounding evidence → 2K tokens, not 50K
```

By the time an agent asks a question, the knowledge is already:
- **Typed** — every claim has a `ClaimType`, every relation has a `RelationType`
- **Verified** — grounded against source text by up to 4 independent judges
- **Deduplicated** — entity resolution has merged aliases and variants
- **Linked** — relations between entities are explicit, not inferred
- **Contradiction-resolved** — conflicts detected and surfaced before serving
- **Health-scored** — freshness, consistency, coverage, provenance all measured

The LLM receives finished goods. Not raw material.

---

## Why "Compile" Is the Right Word

The analogy holds at every level:

| Classical compiler | CompAG (ThinkingRoot) |
|---|---|
| Source code | Raw docs, code, PDFs, git history |
| Lexer / parser | `thinkingroot-parse` — tree-sitter, Markdown, PDF |
| Type system | `ClaimType` (10 types), `EntityType` (13), `RelationType` (13) |
| Type checker | Grounding Tribunal — rejects claims that don't match source |
| Optimizer | Deduplication, entity linking, confidence scoring |
| Dead code elimination | Tribunal rejects claims with grounding score < 0.25 |
| Linker | `thinkingroot-link` — resolves entities across files |
| Output binary | Typed knowledge graph (CozoDB) + compiled artifacts |
| Incremental compilation | BLAKE3 content hashes — only reprocess changed files |
| Compiler warnings | Staleness warnings, orphaned claims, low-confidence flags |
| Static analysis | Contradiction detection across all sources |
| Runtime | REST API + MCP server — agents query compiled graph |
| Debugger | `root verify`, health score, contradiction report |

RAG has no equivalent for the middle column. It goes from source directly to runtime.

---

## Two Extraction Tiers — The Hallucination Firewall

CompAG handles hallucination structurally, not probabilistically.

**Tier 0 — Structural (zero LLM, zero hallucination):**

Functions, type definitions, and imports are extracted deterministically from the AST via tree-sitter. The parser reads `function_name` from the parse tree directly. No LLM is involved. Confidence is fixed at `0.99`. This tier cannot hallucinate because it is not inferring anything — it is reading.

**Tier 2 — LLM (grounded, not trusted):**

Prose, comments, and documentation go through the LLM for semantic extraction, then immediately through the Grounding Tribunal before entering the graph.

The Tribunal runs up to 4 independent judges on every LLM-produced claim:

```
Judge 1 — Lexical:    Key words in the claim appear in source text?
Judge 2 — Span:       LLM's cited source_quote exists verbatim in source text?
Judge 3 — Semantic:   Claim embedding is cosine-close to source embedding?
Judge 4 — NLI:        Source text logically entails the claim? (weight: 0.40)
```

Combined score below `0.25` → claim deleted.
Combined score below `0.50` → claim survives but confidence multiplied down.

The LLM must provide a verbatim `source_quote` for every claim. If it invents a quote that doesn't appear in the source, Judge 2 fails. The claim does not enter the graph.

This is structurally different from RAG, where the LLM's output is trusted unconditionally and hallucinations propagate silently into answers.

---

## Comparison with Existing Paradigms

| Paradigm | Acronym | Core idea | Verified? | Typed? | Versioned? |
|---|---|---|---|---|---|
| Retrieval-Augmented Generation | RAG | Retrieve raw chunks at query time | No | No | No |
| Cache-Augmented Generation | CAG | Preload docs into context, cache KV state | No | No | No |
| Graph RAG (Microsoft) | GraphRAG | Build a KG, retrieve from it at query time | No | Partial | No |
| Knowledge Graph Completion | KGC | Fill missing edges in an existing KG | Partial | Yes | No |
| **Compile-Augmented Generation** | **CompAG** | **Compile knowledge offline, serve verified typed graph** | **Yes** | **Yes** | **Yes** |

The critical distinction between CompAG and Graph RAG: Graph RAG still retrieves at query time from an unverified graph. CompAG compiles (verifies, types, health-scores) before any query.

The critical distinction between CompAG and CAG (Cache): caching puts raw text into memory faster. Compilation transforms raw text into a different, better representation. Cache ≠ Compile.

---

## Knowledge Version Control — Git for Knowledge

CompAG introduces a property that neither RAG nor any graph-based retrieval system has: **knowledge is versioned**.

```
main branch          ← production knowledge (shared, health-gated)
    ├── feature/auth-refactor  ← branch for proposed knowledge changes
    └── agent/alice            ← private branch for an AI agent
```

Operations:
- `root diff branch` — semantic diff showing new claims, new entities, new contradictions
- `root merge branch` — health-gated merge (blocked if health score drops or contradictions unresolved)
- `root snapshot name` — immutable named snapshot
- `root rollback` — restore to pre-merge state

This enables knowledge pull requests: an agent or human proposes changes to a knowledge branch, a reviewer inspects the diff, the health CI gate runs, the merge happens or is rejected. No equivalent exists in RAG.

---

## What CompAG Enables That RAG Cannot

**1. Agents that cost 2K tokens instead of 50K**

A compiled entity page for `AuthService` contains pre-extracted, pre-linked, pre-verified facts. An agent reads 2K tokens of finished knowledge. A RAG agent reads 50K tokens of raw source hoping to reconstruct the same picture.

**2. Knowledge that self-reports its health**

```
HealthScore {
    overall:     0.82,
    freshness:   0.91,   // 91% of claims are within staleness threshold
    consistency: 0.78,   // some unresolved contradictions
    coverage:    0.80,   // most entities have sufficient claims
    provenance:  0.88,   // most claims have verified sources
}
```

RAG has no health model. The agent cannot know whether what it retrieved is stale or contradictory.

**3. Contradiction detection before serving**

When two sources disagree, CompAG detects it at compile time and surfaces it in the ContradictionReport. The agent can be told "these two claims conflict — confidence reduced." RAG passes both conflicting claims to the agent as equal-weight context.

**4. Trust levels per source**

```
TrustLevel: Quarantined → Untrusted → Unknown → Trusted → Verified
```

A source can be quarantined (e.g., user-submitted content) or verified (e.g., checked-in code). Claims from quarantined sources are not served to agents without explicit permission. RAG has no trust model.

**5. Works on any corpus**

CompAG is not codebase-specific. The same pipeline runs on:
- Research papers (PDF)
- Course materials (Markdown)
- Documentation (Markdown, text)
- Codebases (Rust, Python, JavaScript, TypeScript, Go)
- Git history
- Any plain-text format

---

## The Paradigm in One Sentence

> RAG gives agents raw material. CompAG gives agents finished goods.

The compilation step is the work that turns unreliable raw text into reliable typed knowledge. It runs once, offline, before any agent ever connects. Every agent that queries the compiled graph benefits from that work without paying for it again.

This is why compiled languages outperform interpreted ones at scale. The same principle applies to knowledge.

---

## Naming and Prior Art

- **"Compile-Augmented Generation (CompAG)"** — confirmed no prior art on arXiv as of April 2026
- **"Cache-Augmented Generation (CAG)"** — arXiv:2412.15605 (December 2024, WWW '25) — distinct concept, cache ≠ compile
- **"Knowledge Augmented Generation (KAG)"** — existing uses in literature, distinct scope
- **"CompAG"** chosen over "CAG" to avoid collision with Cache-Augmented Generation and to make "Compile" unambiguous

---

*Originated: ThinkingRoot, April 2026*
*Reference implementation: this repository*
