<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/branding/logo_white.png">
  <img alt="ThinkingRoot Logo" src="assets/branding/logo_black.png" width="300" />
</picture>

<br/>

**Compiled knowledge infrastructure for AI agents — works like a secondary brain.**

*The world's first knowledge database that is simultaneously the fastest (0.117ms p95) and the most accurate (91.2% LongMemEval) for AI agents — works like a secondary brain.*

<br/>

[![CI](https://github.com/DevbyNaveen/ThinkingRoot/actions/workflows/ci.yml/badge.svg)](https://github.com/DevbyNaveen/ThinkingRoot/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![Crates.io](https://img.shields.io/crates/v/thinkingroot.svg)](https://crates.io/crates/thinkingroot)
[![PyPI](https://img.shields.io/pypi/v/thinkingroot.svg)](https://pypi.org/project/thinkingroot)
[![MCP Compatible](https://img.shields.io/badge/MCP-2024--11--05-green.svg)](https://modelcontextprotocol.io)
[![LongMemEval](https://img.shields.io/badge/LongMemEval-91.2%25-brightgreen.svg)](#benchmark)

**[Quick Start](#quick-start) · [How It Works](#how-it-works) · [Grounding Tribunal](#the-grounding-tribunal--99-authentic-knowledge-zero-hallucinations) · [KVC Branches](#knowledge-version-control) · [CompAG Protocol](#compag--the-compilation-for-agents-protocol) · [MCP / AI Tools](#connect-any-ai-tool) · [Benchmark](#benchmark) · [CLI Reference](#cli-reference)**

</div>

---

## What is ThinkingRoot?

> **Code has GitHub. Models have HuggingFace. Compiled Knowledge has ThinkingRoot.**
>
> *Building the "GitHub of Knowledge" — an infrastructure layer that treats knowledge preparation as a compilation problem.*

ThinkingRoot compiles **anything** — your entire codebase, docs, PDFs, notes, git history — into a **typed, verified, linked knowledge graph** that your AI agents can query in milliseconds.

Instead of re-reading 50,000 tokens every session, your agent reads a **pre-compiled 2,000-token brief** — with full provenance, zero hallucinations, and sub-millisecond retrieval.

> **World's first knowledge system that is both the fastest and most accurate for AI agents:**
>
> - ⚡ **0.117ms p95** at 10,000 concurrent agents — 307× faster than the nearest competitor
> - 🎯 **91.2% on LongMemEval-500** — beats full-context GPT-4 at a fraction of the cost
> - 🛡️ **99% authentic data** — Grounding Tribunal eliminates hallucinations before graph write
>
> Speed and accuracy are usually a tradeoff. ThinkingRoot breaks that tradeoff with the CompAG architecture — compile once, serve forever.

```
Any Repo / Docs / Code / PDFs / Notes / Git
              │
              ▼
┌─────────────────────────────────────────────────────┐
│  PARSE → EXTRACT → GROUND → LINK → COMPILE → SERVE  │
│                                                     │
│  raw text    claims      NLI      graph  artifacts  │
│  + code   + entities  tribunal  + links  + entity  │
│            + relations          pages   health CI  │
└─────────────────────────────────────────────────────┘
              │
              ▼
     .thinkingroot/
     ├── entities/AuthService.md     ← agent reads this
     ├── architecture-map.md
     ├── contradiction-report.md
     └── graph/                      ← query via REST / MCP / Python
```

**Three fundamental differences from every RAG tool:**

| | RAG / Memory Tools | ThinkingRoot |
|---|---|---|
| Approach | Store → Retrieve | Compile → **Ground** → Verify → Serve |
| Output | Chunks of text | Typed claims · Entities · Relations · Artifacts |
| Hallucination | LLM picks the closest chunk (may fabricate) | **Grounding Tribunal** — 4-judge NLI panel rejects hallucinated claims before graph write |
| Data authenticity | Unknown — no verification layer | **99% authentic** — every claim source-locked with BLAKE3 hash + NLI-verified |
| Speed | 50–500ms per query (remote LLM reranking) | **0.117ms p95** (in-process, no network) |
| Freshness | No signal | Knowledge Health Score — staleness + contradiction CI |
| Version control | None | Knowledge Version Control (branch / diff / merge) |

---

## How It Works

ThinkingRoot is an **8-phase compilation pipeline** — not a search engine.

```
Phase 1  PARSE       Markdown, code (Rust/Python/TS/Go/JS), PDFs, git commits → DocumentIR
Phase 2  EXTRACT     LLM extracts typed Claims, Entities, Relations from each chunk
Phase 3  GROUND      NLI Tribunal — 4 judges filter hallucinated claims before writing to graph
Phase 4  FINGERPRINT BLAKE3 content hash → skip unchanged sources (incremental like `make`)
Phase 5  LINK        Entity resolution · alias merging · contradiction detection
Phase 6  INDEX       fastembed AllMiniLM-L6-v2 neural vectors (local, no API key)
Phase 7  COMPILE     Artifacts: entity pages, architecture map, decision log, health report
Phase 8  VERIFY      7 CI checks: staleness · contradiction · orphan · poisoning · coverage
```

Watch it run:

```
  ✓ Parsing      247 files  1.2s
  ✓ Extracting   1,842 claims · 394 entities  (128 cached, 52% saved)  34.1s
  ✓ Grounding    1,714 accepted  (128 rejected)  12.3s
  ✓ Fingerprint  61 changed, 186 unchanged (skipped)
  ✓ Linking      394 entities · 891 relations · 12 contradictions  2.1s
  ✓ Indexing     394 entities · 1,714 claims  4.7s
  ✓ Compiling    8 artifacts  1.8s
  ✓ Verifying    Health 83%  0.3s

  ThinkingRoot compiled 247 files in 56.5s
  Knowledge Health: 83%
  ├── 1,714 claims extracted
  ├── 394 entities identified
  ├── 891 relations mapped
  ├── 12 contradictions found (9 auto-resolved)
  └── 8 artifacts generated
```

### Incremental Compilation

Like `cargo build` — only recompiles what changed.

```bash
# First compile: full pipeline on all files
root compile .

# You edit one file → ThinkingRoot detects the BLAKE3 hash change
# Only that file re-runs through Extract → Ground → Link → Index
root compile .
# → "61 changed, 186 unchanged (skipped)"

# Or watch mode — recompiles automatically on every save
root watch .
```

---

## Knowledge Graph

ThinkingRoot includes an interactive **3D Knowledge Graph** — a real-time, navigable graph of every entity, claim, and relation in your compiled workspace. Launch it in one command:

```bash
root graph .
# → Opens http://localhost:3001 in your browser
```

The graph renders thousands of nodes as 3D sphere impostor particles, coloured by entity type, sized by claim density, and connected by typed relations. You can fly through it, search semantically, and inspect any entity's full claim set.

**Viewing a real workspace compiled from 500+ conversation sessions:**

<div align="center">

![Knowledge Graph — full view showing thousands of entities as a luminous nebula](assets/galaxy_overview.png)

*Full graph view — each sphere is an entity, sized by the number of claims attached to it*

</div>

<div align="center">

![Knowledge Graph — medium zoom with relation edges visible between entity clusters](assets/galaxy_network.png)

*Mid-zoom — relation edges materialise as you approach a cluster*

</div>

<div align="center">

![Knowledge Graph — close-up showing labelled entity nodes with class and claim count HUD](assets/galaxy_nodes.png)

*Entity HUD — click any node to see its class, claim count, and an Extract Intel action*

</div>

<div align="center">

![Knowledge Graph — ultra close-up showing 3D sphere impostor shading on individual nodes](assets/galaxy_closeup.png)

*Individual nodes with 3D sphere impostor shading — each one is a distinct compiled entity*

</div>

Use the **Semantic Fly-To** search bar at the top to jump directly to any entity. The timeline scrubber at the bottom lets you replay how the knowledge graph evolved over time.

---

## The Grounding Tribunal — 99% Authentic Knowledge, Zero Hallucinations

**The problem with every other memory tool:** LLMs hallucinate. When your system asks an LLM to extract facts from code, it sometimes invents facts that aren't in the source. RAG-based systems store these hallucinations alongside real knowledge — there's no filter.

**ThinkingRoot's answer:** The **Grounding Tribunal** — a 4-judge verification panel that runs on every claim the LLM extracts, before it ever touches the knowledge graph.

> **99% of knowledge in the ThinkingRoot graph is authentic, source-verifiable data. Not summaries. Not inferences. Not hallucinations. Compiled facts.**

### The 4 Judges

Every claim extracted by the LLM goes through all 4 judges. Only claims that pass the tribunal enter the graph.

```
Claim: "AuthService uses JWT RS256 for token signing"
Source: auth/service.rs

Judge 1 → Lexical Anchor      ✓  JWT, RS256, AuthService all appear in source
Judge 2 → Span Attribution    ✓  LLM's quoted span found verbatim in file
Judge 3 → Semantic Similarity ✓  Embedding cosine similarity: 0.87
Judge 4 → NLI Entailment      ✓  DeBERTa: source ENTAILS claim (p=0.91)

Combined score: 0.88 → ACCEPTED ✓
```

```
Claim: "AuthService uses bcrypt for password hashing"
Source: auth/service.rs  (which only has JWT code, no bcrypt)

Judge 1 → Lexical Anchor      ✗  "bcrypt" not found in source text
Judge 2 → Span Attribution    ✗  No matching span
Judge 3 → Semantic Similarity ✗  Low overlap: 0.21
Judge 4 → NLI Entailment      ✗  DeBERTa: source CONTRADICTS claim

Combined score: 0.11 → REJECTED ✗ (hallucination deleted)
```

### Judge Details

| Judge | What it checks | Weight |
|---|---|---|
| **Judge 1 — Lexical Anchor** | Key terms from the claim actually appear in the source text | 15% |
| **Judge 2 — Span Attribution** | The LLM's quoted source span exists verbatim in the file | 20% |
| **Judge 3 — Semantic Similarity** | fastembed cosine similarity between claim and source chunk | 25% |
| **Judge 4 — NLI Entailment** | DeBERTa NLI: does the source text *logically entail* the claim? | 40% |

**Judge 4 (NLI) gets the highest weight** because it's the only judge that understands meaning, not just word overlap. It uses `cross-encoder/nli-deberta-v3-xsmall` — a 71M parameter model (INT8 quantized, 83 MB) that is **embedded directly in the binary**. No download. No internet. No setup. It runs locally on every compile.

### Verdict Logic

```
Combined score < 0.25  → REJECTED  — claim permanently deleted from pipeline
Combined score < 0.50  → REDUCED   — claim enters graph with confidence scaled down
Combined score ≥ 0.50  → ACCEPTED  — claim enters graph with original confidence
```

### Smart Early-Exit (Performance)

Running 2 neural models on every claim would be slow. ThinkingRoot short-circuits intelligently:

- **If** Judge 1 + Judge 2 combined score > 0.70 → claim is already proven by string evidence → **skip Judges 3 and 4**
- This eliminates 60–70% of neural inference calls for typical codebases (where most claims closely echo their source text)
- Only **uncertain claims** (ones the LLM might be fabricating) go through the expensive NLI model

### The Result

```
✓ Grounding    1,714 accepted  (128 rejected)  12.3s
```

128 claims were hallucinated by the LLM and silently deleted. They never reached your knowledge graph. Your agents never saw them. **The graph contains only what your source code and docs actually say.**

---

### Compile Anything

ThinkingRoot is not a docs tool. It compiles **your entire codebase** — and understands the difference between a design decision in a README and a function signature in Rust.

| What you feed it | What it extracts |
|---|---|
| Rust / Python / TypeScript / Go / JavaScript | Architecture, APIs, dependencies, function decisions |
| Markdown docs | Requirements, decisions, explanations |
| Git commits & PR descriptions | Decision history, change rationale |
| PDFs | Research claims, specs, regulations |
| Plain notes | Ideas, todos, observations |
| TOML / YAML / JSON config | Configuration facts, environment requirements |

**One command. Your entire project knowledge compiled.**

---

## Build the Ultimate Secondary Brain (No Fine-Tuning Required)

ThinkingRoot is designed for **anyone** — whether you are a **student**, **developer**, **researcher**, or **business**.

You can compile your raw data (codebases, research papers, business documents) in seconds, connect your favourite AI tools via MCP, or build entire autonomous agent pipelines directly on top of the ThinkingRoot infrastructure.

Because of our native support for isolated **streaming sessions** and **knowledge branches**, ThinkingRoot acts as an extremely efficient, verifiable **secondary brain**.

> **You no longer need to fine-tune LLMs.** Everything your agent needs is compiled, typed, and injected into context identically to how a human reads a curated dossier.

Build agent automations, research assistants, or intelligent enterprise search without ever paying to retrain a model.

---

## Knowledge Version Control

ThinkingRoot has a full knowledge version control system — like git, but for compiled knowledge.

### Three Branch Types

#### 1. `main` — The source of truth

Your compiled, verified knowledge graph. Always health-gated. The `root diff` command shows a **Knowledge PR** before anything merges in.

#### 2. Feature Branches — Isolated experiments

Create an isolated knowledge snapshot, compile new knowledge into it, then diff and merge — with contradiction detection, health CI gate, and rollback.

```bash
# Create a branch to experiment
root branch experiment-v2

# Switch to it
root checkout experiment-v2

# Compile into the branch (isolated from main)
root compile .

# See the Knowledge PR — what changed vs main
root diff experiment-v2

# Example output:
#   Knowledge PR: experiment-v2 → main
#   Health:  before=83%  after=87%
#
#   New claims: 47
#   │ + [Decision] AuthService now uses JWT RS256 instead of HS256
#   │   entities: AuthService, JWT
#   │ + [Fact] Rate limiting is set to 100 req/min per user
#   New entities: 3
#   New relations: 12
#   Auto-resolved: 2  winner: branch (Δ=0.31)
#   ✓ Merge allowed

# Merge when ready (runs health CI gate)
root merge experiment-v2

# Blocked if health drops too much
# root merge --force to bypass
# root merge experiment-v2 --rollback to undo
```

#### 3. Streaming Branches — Live agent session isolation

When multiple AI agents connect via MCP simultaneously, each session gets its own isolated `stream/{session_id}` branch automatically. Agents can write new claims, ask questions, and explore knowledge — without any risk of polluting `main`.

```toml
# .thinkingroot/config.toml
[streams]
auto_session_branch = true   # Each new MCP session → isolated stream/{id} branch
```

With `auto_session_branch` enabled:

- Claude, Cursor, Codex, or any MCP client connects → `stream/abc123` branch created instantly
- Agent writes claims to its own isolated branch
- Human reviews the session diff with `root diff stream/abc123`
- Merge approved knowledge into main: `root merge stream/abc123`
- Sessions that end get garbage-collected: `root branch --gc`

**This enables safe multi-agent concurrent write workflows** — 10 agents writing simultaneously, zero conflicts, full audit trail.

#### 4. Snapshots — Immutable checkpoints

```bash
# Freeze the current state
root snapshot v1.0-release

# Serve that exact snapshot
root serve --branch v1.0-release --port 3001
```

### Knowledge PR — `root diff`

Before merging, see exactly what changed:

- New claims (with entity context)
- New entities and relations
- Auto-resolved contradictions (winner + confidence delta)
- Contradictions needing human review
- Health score before vs. after
- Merge gate: ✓ allowed or ✗ blocked (with reasons)

Resolve contradictions inline:

```bash
# Manually resolve contradiction #0 in the diff
root merge experiment-v2 --resolve 0=keep-branch
```

### Branch Safety

- **Pre-merge snapshot** — `graph.db.pre-merge-*` is created before any mutation
- **Advisory lock** — concurrent `root merge` on the same workspace immediately errors
- **Rollback** — `root merge <branch> --rollback` restores to pre-merge state
- **Health CI gate** — merge blocked if health drop exceeds `max_health_drop`

---

## Benchmark

### 91.2% on LongMemEval-500

**LongMemEval** is the industry-standard benchmark for long-term memory systems. It tests factual recall, temporal reasoning, counting, preference tracking, and multi-session reasoning over hundreds of sessions.

| System | LongMemEval Accuracy | Method |
|---|---|---|
| **ThinkingRoot** | **91.2%** (456/500) | Hybrid retrieval + NLI grounding + temporal anchors |
| GPT-4 w/ full context | ~80% | Full context window (expensive) |
| RAG baseline | ~60–70% | Chunk retrieval + LLM |

> **Round 6, April 17 2026.** `root eval --dataset longmemeval-500.jsonl --path ./workspace`

### Advanced Hybrid Retrieval Pipeline

The `root ask` command uses a 6-stage intelligence pipeline (same code as the 91.2% benchmark):

```
1. Deep vector search     → semantic + keyword over all compiled claims
2. Query expansion        → static term expansion for recall boost
3. Session targeting      → per-session pass for temporal precision
4. Source augmentation    → raw source snippets for precision
5. Temporal anchors       → date-aware reasoning (relative dates, event ordering)
6. Hybrid synthesis       → LLM synthesizes over ranked evidence with full citation
```

```bash
root ask "what did we decide about the auth service last month?"
# → "Based on 3 claims from auth/service.rs, PR #412, and design/decisions.md:
#    The team decided on JWT RS256 (over HS256) on 2026-03-14..."
```

### World's Fastest Retrieval

**0.117ms p95** at 10,000 concurrent users. Zero errors. Reproducible.

| System | p95 Latency | Load Tested |
|---|---|---|
| **ThinkingRoot** | **0.117 ms** | 10,000 VUs |
| FalkorDB | 36 ms | 50 VUs |
| SuperMemory | 50 ms | 50 VUs |
| Zep | 119 ms | 30 VUs |
| Graphiti | 500 ms | 20 VUs |

**Why:** ThinkingRoot serves reads from an in-process `HashMap` — no database query, no network call, no reranking LLM. Knowledge is compiled once; all queries are O(1) lookups.

```bash
# Reproduce it yourself
cargo build --release
root init ./test-workspace && root compile ./test-workspace
root serve --port 9877 --path ./test-workspace &
k6 run --env BASE_URL=http://127.0.0.1:9877 \
        crates/thinkingroot-bench/benches/load/stress_10k.js
```

---

## Connect Any AI Tool

ThinkingRoot speaks [Model Context Protocol](https://modelcontextprotocol.io) 2024-11-05. Connect to any MCP-compatible AI tool in one command.

```bash
# Auto-wire into all detected AI tools (Claude, Cursor, VS Code, Zed)
root connect

# Target a specific tool
root connect --tool claude
root connect --tool cursor

# Preview without writing
root connect --dry-run
```

**Claude Desktop config** (auto-generated by `root connect`):

```json
{
  "mcpServers": {
    "thinkingroot": {
      "command": "root",
      "args": ["serve", "--mcp-stdio", "--path", "/path/to/your/project"]
    }
  }
}
```

**SSE transport** (for remote agents):

```bash
root serve --port 3000 --path ./my-project
# MCP endpoint: http://localhost:3000/mcp/sse
```

### MCP Tools Available

| Tool | What your AI agent can do |
|---|---|
| `search` | Semantic + keyword search over compiled knowledge |
| `query_claims` | Filter by type, entity, confidence, date range |
| `get_relations` | Get the entity graph around any concept |
| `compile` | Trigger incremental recompile |
| `health_check` | Get knowledge health score |
| `create_branch` | Create a knowledge branch |
| `diff_branch` | Get semantic Knowledge PR |
| `merge_branch` | Merge branch into main |

**Your AI agent gets authentic, source-cited knowledge — no hallucinations, no stale data, no fabricated relationships.**

---

## Token Reduction

AI agents re-read your entire codebase every session. ThinkingRoot compiles it once.

```
Without ThinkingRoot:
  Agent reads 50 files × 1,000 tokens = 50,000 tokens per session
  Cost: ~$0.50 per session at GPT-4o pricing
  Speed: 30–60 seconds context loading

With ThinkingRoot:
  Agent reads compiled entity brief = 2,000 tokens
  Cost: ~$0.02 per session
  Speed: <1 second
  Accuracy: 91.2% on LongMemEval (vs 60-70% RAG baseline)
```

The knowledge is **real** — extracted from your actual code and docs, source-locked with BLAKE3 hashes, verified by NLI tribunal, and contradiction-checked. Your agent doesn't guess. It reads compiled facts.

---

## CompAG: Compile-Augmented Generation

> *CompAG (Compile-Augmented Generation) is a new paradigm that treats AI agent knowledge as a compilation problem, not a retrieval problem.*

Every other AI memory system (like RAG) works like this:

```
Query time:
  raw text chunks → embed → similarity search → dump into context → LLM figures it out
```

RAG pushes all understanding to the LLM at runtime, from unverified raw text. The LLM must resolve contradictions, assess staleness, infer types, and reconstruct relationships — all inside a single context window, under token pressure, with no guarantee of correctness.

ThinkingRoot shifts the hard work to compile time with CompAG:

```
Compile time (once, offline):
  raw sources → parse → extract → verify → type → link → deduplicate → health-score → serve

Query time (fast, cheap, reliable):
  pre-verified typed claim + confidence + grounding evidence → 2K tokens, not 50K
```

### The CompAG Guarantee

| Property | What it means |
|---|---|
| **Compiled** | Knowledge is extracted offline, not at query time. Agents never wait for LLM extraction. |
| **Grounded** | Every claim passed a 4-judge tribunal before entering the graph. Hallucinations never reach agents. |
| **Source-locked** | Every claim carries a BLAKE3 hash of its source file. Tampering or drift is detectable. |
| **Versioned** | Knowledge changes via branch → diff → merge. No silent mutations. Full audit trail. |
| **Typed** | Claims have types (Fact, Decision, Requirement, API, Architecture...) — not just freeform text. |
| **Health-scored** | Every knowledge graph has a continuous health score: freshness, consistency, coverage, provenance. |

### Why "Compile" Is the Right Word

A compiler takes source code, applies analysis passes, discards invalid constructs, and produces an optimised binary. ThinkingRoot does the same for knowledge:

| Compiler | ThinkingRoot |
|---|---|
| Lexer → Parser → AST | File walker → `DocumentIR` chunks |
| Semantic analysis | LLM extraction → Typed claims/entities/relations |
| Type checker | Grounding Tribunal (4 judges, NLI entailment) |
| Optimiser | Fingerprint dedup, incremental compilation |
| Linker | Entity resolution, alias merging, relation linking |
| Object file | CozoDB knowledge graph + fastembed vector index |
| Binary | Compiled artifacts (entity pages, architecture map, health report) |
| Loader | REST server / MCP server / Python SDK |

### MCP Protocol Support

ThinkingRoot supports **both MCP protocol versions** simultaneously:

| Version | Status |
|---|---|
| `MCP 2025-03-26` | ✅ Supported (latest) |
| `MCP 2024-11-05` | ✅ Supported (legacy) |

The server negotiates version with the client automatically — if a client requests `2024-11-05`, it gets `2024-11-05`. If it requests `2025-03-26`, it gets `2025-03-26`. No config needed.

**Two transports:**

- **stdio** — for local tools (Claude Desktop, Cursor, VS Code, Zed). `root serve --mcp-stdio`
- **SSE (HTTP)** — for remote agents. `GET /mcp/sse` + `POST /mcp?sessionId={id}`

### The CompAG Stack in Practice

```
Your codebase / docs
      │
      ▼  root compile .
┌─────────────────────────────────────────────────┐
│  CompAG Compilation Layer                        │
│  ┌──────┐ ┌─────────┐ ┌────────┐ ┌──────────┐  │
│  │PARSE │→│ EXTRACT │→│GROUND  │→│LINK+INDEX│  │
│  │      │ │  (LLM)  │ │4 judges│ │ + VERIFY │  │
│  └──────┘ └─────────┘ └────────┘ └──────────┘  │
└─────────────────────────────────────────────────┘
      │
      ▼  root serve
┌─────────────────────────────────────────────────┐
│  Agent Access Layer                              │
│                                                 │
│  REST API   ←  curl / Python SDK / TypeScript   │
│  MCP stdio  ←  Claude Desktop / Cursor / Zed    │
│  MCP SSE    ←  Remote agents / cloud runners    │
│                                                 │
│  All reads: O(1) HashMap · 0.117ms p95          │
│  All data: NLI-grounded · BLAKE3 source-locked  │
└─────────────────────────────────────────────────┘
```

---

## Quick Start & Zero-Friction Onboarding

ThinkingRoot is designed to take you from 0 to a compiled knowledge graph and MCP integration in under 2 minutes.

### 1. Install

**Build from Source (recommended)**

```bash
git clone https://github.com/DevbyNaveen/ThinkingRoot.git
cd ThinkingRoot
cargo build --release
# Binary at: target/release/root
# Optionally move to PATH:
sudo mv target/release/root /usr/local/bin/root
```

**macOS / Linux (Homebrew)** *(coming soon)*

```bash
brew install thinkingroot
```

**Cargo** *(coming soon)*

```bash
cargo install thinkingroot
```

**Python SDK** *(coming soon)*

```bash
pip install thinkingroot
```

### 2. Interactive Setup Wizard

The fastest way to get started is the interactive setup wizard, which handles all configuration, API keys, and tool plumbing for you:

```bash
root setup
```

The wizard will guide you through:

1. **Selecting your LLM** (AWS Bedrock, OpenAI, Anthropic, local Ollama, Groq, etc.)
2. **Setting your API key** or local base URL.
3. **Registering your first workspace** (the directory you want to compile).
4. **Auto-wiring MCP** into your installed tools (Claude Desktop, Cursor, VS Code, Zed).
5. **Running your first compilation** automatically.

### 3. Or Compile Manually

If you've already set things up, just recompile your workspace whenever files change:

```bash
# Point it at any directory — code, docs, notes, anything
root compile ./my-project

# Or just: root (compiles current directory)
root
```

ThinkingRoot **respects `.gitignore` by default** — it never processes `node_modules`, `target`, `.git`, or anything you've told git to ignore.

### Ask questions immediately

```bash
root ask "how does authentication work in this codebase?"
root ask "what decisions were made about the database schema?"
root ask "what did we change last week?"
```

---

## .gitignore Works Out of the Box

ThinkingRoot reads your existing `.gitignore` and excludes everything git ignores. Nothing to configure.

By default, it also ignores:

```
target/        node_modules/     .git/
__pycache__/   .venv/            dist/
build/         .next/            .tox/
```

Add custom patterns in `.thinkingroot/config.toml`:

```toml
[parsers]
exclude_patterns = ["*.generated.ts", "fixtures/**", "legacy/**"]
respect_gitignore = true   # default: true
```

---

## Configuration

`root init` creates `.thinkingroot/config.toml`. Or use the global config at `~/.config/thinkingroot/config.toml` — applies to all workspaces.

```toml
[llm]
default_provider = "openai"   # openai | anthropic | bedrock | ollama | groq |
                               # deepseek | azure | together | perplexity | custom

[llm.providers.openai]
api_key_env = "OPENAI_API_KEY"

[llm.providers.ollama]
base_url = "http://localhost:11434"
default_model = "mistral"     # fully local, no API key needed

[llm.providers.groq]
api_key_env = "GROQ_API_KEY"  # free tier available

[extraction]
min_confidence = 0.5
max_chunk_tokens = 4000

[verification]
staleness_days = 90
min_freshness = 0.5

[parsers]
exclude_patterns = ["legacy/**", "*.generated.*"]
respect_gitignore = true

[merge]
auto_resolve_threshold = 0.8   # auto-resolve contradictions above this confidence delta
max_health_drop = 0.1          # block merge if health drops more than 10%
block_on_contradictions = true
```

**Switch provider in one line:**

```bash
root provider use ollama          # fully local, no keys
root provider use groq            # fast, free tier
root provider use openai --model gpt-4o-mini
root provider status              # see what's active
```

---

## REST API

```bash
root serve --port 3000 --path ./my-project

# Multi-workspace
root serve --port 3000 --path ./backend --path ./frontend --path ./infra

# With auth
root serve --port 3000 --path ./my-project --api-key secret123

# Serve a specific branch
root serve --port 3000 --path ./my-project --branch experiment-v2
```

```bash
# Search
curl "http://localhost:3000/api/v1/ws/my-project/search?q=authentication+flow&top_k=5"

# Get entity
curl http://localhost:3000/api/v1/ws/my-project/entities/AuthService

# Query claims (typed, filterable)
curl "http://localhost:3000/api/v1/ws/my-project/claims?type=Decision&min_confidence=0.8"

# Health score
curl http://localhost:3000/api/v1/ws/my-project/health

# Architecture map
curl -H "Accept: text/markdown" \
     http://localhost:3000/api/v1/ws/my-project/artifacts/architecture_map

# Branch API
curl -X POST http://localhost:3000/api/v1/branches \
     -H "Content-Type: application/json" \
     -d '{"name": "experiment-v2"}'

curl http://localhost:3000/api/v1/branches/experiment-v2/diff
```

All responses:

```json
{ "ok": true,  "data": {...}, "error": null }
{ "ok": false, "data": null,  "error": { "code": "NOT_FOUND", "message": "..." } }
```

---

## Python SDK

```bash
pip install thinkingroot
```

```python
import thinkingroot

# Compile
thinkingroot.compile("./my-project")

# Open compiled knowledge
engine = thinkingroot.open("./my-project")
```

```python
from thinkingroot.client import Client

c = Client(base_url="http://localhost:3000", api_key="optional")

# Search
results = c.search("how does authentication work?", top_k=5)

# Query claims with filters
decisions = c.claims(type="Decision", min_confidence=0.8)

# Get entity with all claims and relations
auth = c.entity("AuthService")

# Health
health = c.health()
print(f"Health: {health['data']['overall_score']}%")

# Branch operations
c.create_branch("experiment-v2")
diff = c.diff_branch("experiment-v2")
c.merge_branch("experiment-v2")
```

---

## CLI Reference

```
root <command> [options]

── Core ─────────────────────────────────────────────────────────────
  (no args)              Compile current directory (shorthand for root compile .)
  compile <path>         Run full 8-phase pipeline
  watch <path>           Watch for changes and recompile incrementally
  health [path]          Show knowledge health score + staleness warnings
  init [path]            Create .thinkingroot/config.toml
  query <text>           Vector search over compiled knowledge
  ask <question>         Hybrid intelligence pipeline (91.2% LongMemEval)
    ask llm <question>   Force LLM synthesis (vs keyword fallback)
    --date <YYYY/MM/DD>  Reference date for temporal questions

── Server ───────────────────────────────────────────────────────────
  serve                  Start REST API + MCP server
    --port               HTTP port (default: 3000)
    --host               Bind address (default: 127.0.0.1)
    --api-key            Bearer auth token
    --path               Workspace path (repeatable, multi-workspace)
    --name               Serve a registered workspace by name
    --branch             Serve a specific knowledge branch
    --mcp-stdio          Use MCP stdio transport (for Claude Desktop etc.)
    --no-rest            Disable REST API
    --no-mcp             Disable MCP endpoints
    --install-service    Install OS-native autostart (launchd/systemd/Windows)
  graph [path]           Open interactive knowledge graph in browser (port 3001)

── Onboarding ───────────────────────────────────────────────────────
  setup                  Interactive first-time setup wizard
  connect                Auto-wire MCP into Claude Desktop, Cursor, VS Code, Zed
    --tool <name>        Target specific tool only
    --dry-run            Preview without writing
    --remove             Remove wiring
  workspace add <path>   Register a workspace
  workspace list         List registered workspaces
  workspace remove <n>   Remove a workspace

── Provider ─────────────────────────────────────────────────────────
  provider               List providers (default)
  provider list          List all providers and which is active
  provider status        Show active provider, model, credential status
  provider use <name>    Switch provider
    --model <id>         Model ID
    --key <key>          API key (skips prompt)
    --base-url <url>     Custom endpoint URL
    --local              Write to workspace config instead of global
  provider set-model <m> Change extraction model only

── Knowledge Version Control ────────────────────────────────────────
  branch [name]          Create a knowledge branch (no args = list)
    --list               List all branches
    --delete <name>      Soft-delete branch (keeps data)
    --purge <name>       Hard-delete branch + data directory
    --gc                 Remove all abandoned branch data
  checkout <name>        Switch active branch
  diff <branch>          Show Knowledge PR vs main
  merge <branch>         Merge into main (runs health CI gate)
    --force              Bypass health gate
    --propagate-deletions Apply deletions from branch to main
    --rollback           Restore main to pre-merge state
    --resolve N=keep-main|keep-branch  Resolve contradiction N manually
  status                 Show branch, modified/untracked/deleted files
  snapshot <name>        Create immutable named snapshot

── Benchmarking ─────────────────────────────────────────────────────
  eval --dataset <file>  Run LongMemEval benchmark
    --path <workspace>   Workspace to evaluate
    --limit <n>          Limit questions (0 = all)
    --category <cat>     Filter by category (TR, SSP, MS, ...)

── Metadata ─────────────────────────────────────────────────────────
  update                 Update root to the latest version

Global:
  -v, --verbose          Debug logging
  --help                 Show help
```

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     thinkingroot-cli                          │
│                  (root binary · Clap CLI)                    │
└──────────────────────────┬───────────────────────────────────┘
                           │
             ┌─────────────▼──────────────┐
             │      thinkingroot-serve     │
             │  REST (Axum) · MCP (SSE +  │
             │  stdio) · Intelligence API │
             └─────┬───────────┬──────────┘
                   │           │
      ┌────────────▼──┐  ┌─────▼───────────┐
      │   -compile    │  │   -verify        │
      │   -branch     │  │   -safety        │
      └───────┬───────┘  └────────┬─────────┘
              │                   │
      ┌───────▼───────────────────▼─────────┐
      │            thinkingroot-link         │
      │  entity resolution · contradiction  │
      └──────────────────┬──────────────────┘
                         │
      ┌──────────────────▼──────────────────┐
      │          thinkingroot-ground         │
      │     NLI Tribunal · claim filtering  │
      └──────────────────┬──────────────────┘
                         │
      ┌──────────────────▼──────────────────┐
      │          thinkingroot-extract        │
      │  LLM: Claims · Entities · Relations │
      └──────────────────┬──────────────────┘
                         │
      ┌──────────────────▼──────────────────┐
      │           thinkingroot-parse         │
      │  markdown · code · PDF · git → IR  │
      └──────────────────┬──────────────────┘
                         │
      ┌──────────────────▼──────────────────┐
      │           thinkingroot-graph         │
      │  CozoDB (Datalog) + fastembed       │
      └──────────────────┬──────────────────┘
                         │
      ┌──────────────────▼──────────────────┐
      │           thinkingroot-core          │
      │  types · IDs · config · errors      │
      └─────────────────────────────────────┘
```

**Tech stack:** Rust (edition 2024) · CozoDB (embedded Datalog graph) · fastembed / ONNX (local neural vectors) · Axum (HTTP) · PyO3 (Python bindings) · tree-sitter (code parsing) · tokio (async) · BLAKE3 (content hashing) · indicatif (progress)

---

## Supported Input

| Category | Formats |
|---|---|
| **Code** | Rust, Python, TypeScript, JavaScript, Go (via tree-sitter) |
| **Documents** | Markdown (`.md`), PDF |
| **Config** | TOML, YAML, JSON |
| **Version control** | Git commits, PR descriptions |

---

## Building from Source

**Prerequisites:** Rust 1.85+, optional Python 3.9+ with maturin for the Python SDK

```bash
git clone https://github.com/DevbyNaveen/ThinkingRoot
cd thinkingroot

# Build CLI
cargo build --release
./target/release/root --help

# Build without neural embedding (faster, no ONNX Runtime ~300MB)
cargo build --release --no-default-features

# Run tests
cargo test

# Lint
cargo fmt --all && cargo clippy --workspace -- -D warnings

# Build Python SDK
cd thinkingroot-python
pip install maturin
maturin develop --release
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for crate architecture, feature flags, and contribution patterns.

---

## Contributing

We welcome contributions. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide.

1. Fork and clone
2. `cargo test` to verify everything passes
3. Make your change
4. `cargo fmt --all && cargo clippy --workspace -- -D warnings`
5. Submit a PR

For bugs: [bug report template](.github/ISSUE_TEMPLATE/bug_report.yml)  
For ideas: [GitHub Discussions](https://github.com/DevbyNaveen/ThinkingRoot/discussions)

---

## License

Licensed under the [MIT License](LICENSE-MIT).

---

<div align="center">

Built with care by [Naveen](https://github.com/naveen) and contributors.

*If ThinkingRoot makes your agents faster and smarter, consider starring the repo.*

</div>
