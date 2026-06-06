# HelloRoot — Design Specification

**Date:** 2026-04-21
**Status:** Awaiting approval
**Author:** ThinkingRoot Core Team
**Classification:** Novel — prior art exists in multi-channel agents (OpenClaw, OpenFang, ZeroClaw, Spacebot); compiled-KG memory + CRDT peer sync + deterministic replay is absent from every existing system.

---

## The Problem

The 2026 personal-AI-agent landscape has three failure modes.

**OpenClaw (361k★, the target):** ships on Node 22+, installs 250–298 MB, idles at 145 MB RAM, cold-starts in ~1.25 s, hard-crashes below 2 GB. Skills are Markdown instructions the LLM executes via shell — the docs literally say *"Treat third-party skills as untrusted code."* Documented incidents: *ClawJacked* (malicious sites hijack local agents over WebSocket, ~1000 public installs exposed), prompt-injection exfiltration (Kaspersky), and memory "dreaming" bugs still being fixed as of v2026.4.12.

**Existing Rust alternatives (ZeroClaw 30.4k★, OpenFang 16.8k★, Spacebot 2.1k★):** each solves a slice. ZeroClaw wins on footprint (8.8 MB binary, <10 ms cold start). OpenFang has the cleanest multi-crate layout and exposes MCP server. Spacebot has the best in-session memory via LanceDB + `rmcp`. None of them combine a **compiled knowledge-graph memory**, a **CRDT peer-to-peer sync**, or a **deterministic replay harness**. OpenFang's Wasm sandbox is `RuntimeNotAvailable` in the source (not yet implemented). ZeroClaw's `knowledge_graph.rs` is a user-entered notebook (Pattern/Decision/Lesson nodes), not an extraction pipeline.

**ThinkingRoot:** ships a production-grade knowledge compiler (91.2% LongMemEval, 0.117 ms p95 retrieval) with no end-user agent surface. The engine is best-in-class; the distribution vehicle is missing.

**The opportunity:** assemble a lightweight, multi-channel, Wasm-sandboxed Rust agent on top of thinkingroot's compiled-KG memory — giving OpenClaw's users 95%+ of its capability at <10% of its footprint, with three genuine kill-features nobody else ships.

---

## The Solution: HelloRoot

`helloroot` is a Rust-native **multi-agent** personal AI system that runs on your devices, speaks on the channels you already use, and remembers through a compiled knowledge graph. An orchestrator decomposes complex tasks and spawns specialist agents (coder, researcher, writer, debugger, security-auditor, …); every spawn, send, and kill is hash-chained and signed.

```
┌──────────────────────────────────────────────────────────────────┐
│                            HelloRoot                              │
│                                                                   │
│  Channels ──┐                                                     │
│  (telegram, │     ┌───────────────────────────────────────┐       │
│   slack,    │     │             Multi-Agent Bus           │       │
│   discord,  │     │                                       │       │
│   matrix,   ├───► │  ┌─────────────┐    ┌─────────────┐   │       │
│   imessage) │     │  │Orchestrator │◄──►│  Coder      │   │       │
│             │     │  └──────┬──────┘    └─────────────┘   │       │
│             │     │         │                             │       │
│             │     │         ▼                             │       │
│             │     │  ┌──────────┐    ┌──────────────┐     │       │
│             │     │  │Researcher│    │ ...other 7   │     │       │
│             │     │  └──────────┘    └──────────────┘     │       │
│             │     └──────────────┬────────────────────────┘       │
│             │                    │                                 │
│             │     ┌──────────────┼────────────┐                    │
│             │     │              ▼            │                    │
│             │     │      ┌─────────────┐      │                    │
│             │     │      │   Skills    │      │                    │
│             │     │      │   (Wasm)    │      │                    │
│             │     │      └─────────────┘      │                    │
│             │     │                           │                    │
│             │     │      ┌─────────────┐      │                    │
│             │     │      │ MCP client  │      │                    │
│             │     │      │ (ext. tools)│      │                    │
│             │     │      └─────────────┘      │                    │
│             │     │                           │                    │
│             │     │      ┌─────────────┐      │                    │
│             │     │      │   Trace     │ ──► hash-chained, signed  │
│             │     │      │  (signed)   │     per-agent sessions    │
│             │     │      └─────────────┘                           │
│             │     │                                                │
│             │     │      ┌────────────────────┐                    │
│             │     │      │ thinkingroot KG    │◄── MCP server      │
│             │     │      │ (shared brain,     │    (external       │
│             │     │      │  0.117 ms p95)     │     agents query)  │
│             │     │      └────────────────────┘                    │
│             │     │                                                │
│             │     │      ┌─────────────┐                           │
│             │     │      │ A2A Card    │◄── /.well-known/          │
│             │     │      │             │    agent.json             │
│             │     │      └─────────────┘                           │
└──────────────────────────────────────────────────────────────────┘
```

Five load-bearing design choices:

1. **Memory is a compiled knowledge graph** (thinkingroot), not vector RAG, not markdown files, not a user-entered notebook. Every incoming message and LLM reply can emit claims that flow through extract → link → verify before they enter retrieval. Every recall is a 0.117 ms p95 query against a typed graph with confidence scores and contradiction flags. **All agents in the spawn tree read/write the same KG — it's the shared brain.**

2. **Skills are Wasm modules with declared capabilities.** Not Markdown instructions the LLM executes via shell. A skill declares `tools`, `network`, `memory_read`, `memory_write` in its manifest; the Extism host enforces those declarations at runtime. A compromised skill cannot exfiltrate — the sandbox denies the syscall.

3. **MCP is bidirectional; A2A is the cross-framework bridge; inter-agent inside HelloRoot is native Rust.** HelloRoot calls any MCP server (inherits the 97M-download MCP ecosystem, replaces the ClawHub dependency) *and* exposes itself as an MCP server to other frameworks (the federation play). Internal agent-to-agent messaging goes through an `AgentBus` trait with 3 backends (in-process Rust channel ≪ subprocess MCP stdio ≪ cross-framework A2A HTTP) — the right transport per case, ≥100× faster than MCP-for-everything, and matches what OpenClaw's ACP and OpenFang's internal types actually do in production. Google's A2A protocol is layered on for cross-framework interop (Agent Cards at `/.well-known/agent.json`).

4. **Multi-agent is first-class, not an extension.** The `helloroot-agents` crate ships 10 bundled roles (orchestrator, planner, researcher, coder, reviewer, writer, analyst, debugger, security-auditor, test-engineer) in v1. Parents spawn children via `agent_spawn` with capability allowlists; children inherit a strict subset of parent capabilities. In-process or subprocess, isolated traces, parent-issued signing subkeys.

5. **Every step is a hash-chained trace event — including every spawn, send, and kill.** Signed, append-only, Merkle-linked across parent-child agent boundaries. v1 gives you a verifiable spawn tree and audit bundle export; v2 gives you deterministic multi-agent replay (reseeded RNG + stubbed LLM/tool outputs + re-instantiated children from `manifest_hash` → rerun any past workflow bit-for-bit); v2 also gives you CRDT peer-to-peer state sync via Iroh+Automerge (laptop ↔ phone ↔ server without a cloud).

---

## Prior Art and Differentiation

| Feature | OpenClaw | OpenFang | ZeroClaw | Spacebot | nanobot | **HelloRoot** |
|---|---|---|---|---|---|---|
| Language | TS/Node 22+ | Rust 1.75 | Rust 1.87 | Rust ed.2024 | Python | Rust ed.2024 |
| Install size | 298 MB | 32 MB | 8.8 MB | ~30 MB | 68 MB repo | **~200 MB full (w/ bundled ONNX embeddings) · ~80 MB lean (API embeddings)** |
| Idle RAM (cold) | 145 MB | unpub | <5 MB | unpub | Python base | **40–70 MB (cold) · 130–200 MB (embed model warm)** |
| Active RAM (processing a request) | 500 MB – 2 GB | unpub | unpub | unpub | Python | **180–300 MB typical · 350–500 MB peak (multi-agent + Wasm + embed + LLM)** |
| Cold start | 1,250 ms | <200 ms | <10 ms | unpub | Python start | **<100 ms target (full binary; <50 ms lean)** |
| Channel count | 25+ | 40 | 20+ | 8 | 12 | **5 (v1) → 10 (v2)** |
| Wasm sandbox | ❌ "untrusted" | ❌ stub only | ✅ Extism | ❌ prompt only | ❌ | **✅ Extism → WASI 0.2** |
| Capability-scoped skills | ❌ | ✅ declared | ⚠️ signature only | ⚠️ | ❌ | **✅ enforced by Extism host** |
| MCP client | via mcporter | ✅ | ✅ | ✅ rmcp | ✅ | **✅ rmcp** |
| **MCP server** | ❌ | ✅ | ❌ | ❌ | ❌ | **✅ (thinkingroot tools)** |
| **Compiled KG memory** | ❌ markdown | ❌ SQLite+vec | ❌ notebook KG | ❌ LanceDB vec | ❌ token-based | **✅ (thinkingroot only)** |
| Hash-chained trace | ❌ | ✅ audit.rs | ✅ audit.rs | ⚠️ OTel | ❌ | **✅ (signed)** |
| **CRDT P2P state sync** | ❌ | ❌ | ❌ | ⚠️ Iroh files | ❌ | **✅ v2 (Iroh+Automerge)** |
| **Deterministic replay** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v2** |
| **Deterministic multi-agent replay** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v2** |
| Multi-agent spawn | ✅ ACP `acp-spawn.ts` | ✅ `agent_spawn` host fn | ⚠️ `delegate.rs` | ❌ | ❌ | **✅ v1 (`AgentBus`: in-process Rust + MCP stdio + A2A)** |
| Pre-built agent roles | via extensions | **31 in `agents/`** | via tools | — | — | **10 bundled v1** |
| Cross-framework agent protocol | ACP (proprietary) | ✅ A2A (Google std) | ❌ | ❌ | ❌ | **✅ A2A + MCP v1** |
| Signed agent-to-agent trace | ❌ | ⚠️ audit only | ⚠️ audit | ❌ | ❌ | **✅ v1 (hash-chained + Ed25519)** |
| Compiled KG as shared multi-agent brain | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v1** |
| **Encryption at rest (default)** | ❌ plaintext | ❌ | ❌ | ❌ | ❌ | **✅ v0.1 (D-3)** |
| **Dry-run + approval gates (default on destructive)** | ⚠️ optional | ⚠️ optional | ⚠️ optional | ❌ | ❌ | **✅ v0.1 (D-4)** |
| **Built-in eval harness over replay** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.2 (D-1)** |
| **Cost + budget primitives (per-agent)** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.1.1 (D-2)** |
| **Confidence-aware meta-cognition** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.1.1 (D-5)** |
| **Self-learning agents (reflection to KG)** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.2 (D-6)** |
| **CompAG paradigm (not RAG)** | ❌ RAG | ❌ RAG | ❌ RAG | ❌ RAG | ❌ RAG | **✅ v0.1 (R-1, zero arXiv prior art)** |
| **Admission tiers surfaced to user** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.1 (R-2)** |
| **Disaggregated neurosymbolic controller** | ❌ ReAct-style | ❌ ReAct-style | ❌ ReAct-style | ❌ | ❌ | **✅ v0.1 (R-4)** |
| **Biscuit-per-tool-call capability attenuation** | ❌ ambient keys | ❌ | ❌ | ❌ | ❌ | **✅ v0.1 (R-8)** |
| **Inspectable/editable/portable memory UI** | ⚠️ partial | ❌ | ❌ | ❌ | ❌ | **✅ v0.1 (R-11)** |
| **True-undo Action Capsules** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.1 (R-12)** |
| **Agent Covenant (signed user–agent contract)** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.1 (R-13)** |
| **Reflexive queries / blindspots** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.1.1 (R-3, zero prior art)** |
| **Personality Pin across model upgrades** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.1.1 (R-14)** |
| **Bitemporal recall (valid + tx time)** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.2 (R-6)** |
| **Self-healing predicates (claims re-verify)** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.2 (R-7)** |
| **Personal transparency log (non-membership proofs)** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.2 (R-9)** |
| **PSI inter-agent knowledge handshake** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.2 (R-10)** |
| **Knowledge branches (memory fork/merge)** | ❌ | ❌ | ❌ | ❌ | ❌ | **✅ v0.2 (R-15)** |
| On-device LLM | via Ollama | via Ollama | via Ollama | via Ollama | via vLLM | **✅ mistral.rs native** |

**Six columns where HelloRoot is the only positive:** compiled KG memory, MCP server mode, CRDT P2P sync (v2), deterministic replay (v2), deterministic multi-agent replay (v2), signed agent-to-agent trace (v1), compiled KG as shared multi-agent brain (v1). These are defensible because (a) the KG requires thinkingroot's 2-year pipeline, (b) CRDT + agent state is an unsolved problem ("AI agents generate ops 25–100× faster than humans, breaking traditional CRDTs"), (c) deterministic replay of a *multi-agent* graph requires hash-chaining the parent-child spawn tree plus stubbed LLM/tool outputs — neither OpenClaw's ACP nor OpenFang's A2A hash-chain across agent boundaries.

---

## Core Architecture Decision: Separate Repo, Tight Coupling

**Chosen approach (locked 2026-04-21):** HelloRoot lives in its **own GitHub repository** (`github.com/<org>/helloroot`), separate from `thinkingroot`. The two repos are tightly coupled at the source level via a Cargo workspace dependency (`thinkingroot-*` crates referenced by `git+rev` pin or shared parent workspace).

Rationale:
- **Independent product identity.** HelloRoot has its own README, releases, issues, contributor base, license file, brand. Easier to attract HelloRoot-specific contributors who don't care about the knowledge compiler.
- **Independent release cadence.** thinkingroot can ship pipeline improvements without forcing a HelloRoot release; HelloRoot can ship UX fixes without bumping thinkingroot.
- **Cleaner public surface.** When users install HelloRoot, they get an agent; thinkingroot remains a separate, optional, lower-level product for users who only want the knowledge compiler.

How tight coupling is preserved:
- HelloRoot's `Cargo.toml` pulls thinkingroot crates as `git` dependencies pinned to a specific rev (or as a workspace-shared path during local dev via `git submodule`).
- Shared types live in `thinkingroot-core` (already designed for this — no `helloroot::` dependency).
- HelloRoot's CI matrix tests against the **pinned** thinkingroot rev + the latest `main` (so we catch drift early).
- Local dev workflow: `git clone helloroot && cd helloroot && git submodule update --init` brings thinkingroot in for hot iteration.

Cost added vs in-workspace: ~1 week of Phase 0 to set up CI matrix, dependency wiring, submodule conventions, and release coordination. Worth it for the brand and lifecycle independence.

**Out-of-scope alternative considered:** publishing thinkingroot crates to crates.io. Defer to v1.0 once the API is stable; until then, git+rev pin avoids premature semver lock-in.

---

## Crate Layout

**Two repos:**

```
github.com/<org>/thinkingroot/        # existing (knowledge compiler)
├── crates/
│   ├── thinkingroot-core/
│   ├── thinkingroot-graph/
│   ├── thinkingroot-extract/
│   ├── thinkingroot-link/
│   ├── thinkingroot-compile/
│   ├── thinkingroot-verify/
│   ├── thinkingroot-serve/        # MCP tools live here
│   ├── thinkingroot-branch/        # KVC (built)
│   ├── thinkingroot-rooting/       # Rooting gate (Phase 0)
│   └── ... (other thinkingroot crates)


github.com/<org>/helloroot/          # NEW REPO (the agent)
├── Cargo.toml                       # workspace; depends on thinkingroot via git+rev
├── crates/
│   ├── helloroot-types/             # Session, TraceEvent, AgentManifest, Capability, ChannelMsg
│   ├── helloroot-trace/             # append-only hash-chained log, signing
│   ├── helloroot-skills/            # manifest parsing + Extism loader + capability gate
│   ├── helloroot-providers/         # LLM driver trait + Anthropic/OpenAI/Ollama/mistral.rs
│   ├── helloroot-channels/          # Channel trait + telegram/slack/discord/matrix/imessage
│   ├── helloroot-agents/            # multi-agent registry, spawner, A2A protocol, orchestrator
│   ├── helloroot-attachments/       # (O-7) multimodal routing — image/PDF/audio
│   ├── helloroot-update/            # (O-3) self-update + minisign verify + schema migration
│   ├── helloroot-onboard/           # (O-2) 7-step wizard
│   ├── helloroot-watchdog/          # (O-11) supervisor tree + heartbeats + AI-aware signals
│   ├── helloroot-runtime/           # agent loop, tool dispatcher, thinkingroot adapter, BG tasks
│   ├── helloroot-sync/              # (v2) Iroh + Automerge CRDT adapter
│   └── helloroot-cli/               # binary entrypoint `helloroot`
└── README.md, LICENSE, CONTRIBUTING.md, etc.   # own brand, own docs
```

**Cargo dependency from helloroot to thinkingroot:**

```toml
# helloroot/Cargo.toml (workspace deps)
[workspace.dependencies]
thinkingroot-core    = { git = "https://github.com/<org>/thinkingroot", rev = "<sha>" }
thinkingroot-serve   = { git = "https://github.com/<org>/thinkingroot", rev = "<sha>" }
thinkingroot-branch  = { git = "https://github.com/<org>/thinkingroot", rev = "<sha>" }
# Local dev path override via [patch.crates-io] in user's ~/.cargo/config.toml
```

**Local dev convenience:** `git submodule add ../thinkingroot vendor/thinkingroot` so contributors can iterate across both repos without leaving the helloroot tree.

### Dependency order (top = no internal deps)

```
helloroot-types
    ↓
helloroot-trace, helloroot-providers, helloroot-skills
    ↓
helloroot-channels, helloroot-agents
    ↓
helloroot-runtime (depends on all above + thinkingroot-serve)
    ↓
helloroot-cli
```

`helloroot-sync` is v2 and hooks into `helloroot-trace` (Automerge wraps the trace log).

---

## Data Model

### Session

```rust
pub struct Session {
    pub id: SessionId,              // UUIDv7 (time-ordered)
    pub created_at: DateTime<Utc>,
    pub channel: ChannelRef,        // which channel + thread the session lives in
    pub head: TraceHash,            // hash of the latest trace event
    pub identity: IdentityRef,      // which user owns this session
    pub workspace: WorkspaceRef,    // which thinkingroot workspace feeds memory
}
```

### TraceEvent (append-only, hash-chained)

```rust
pub struct TraceEvent {
    pub seq: u64,                   // monotonic within session
    pub prev: TraceHash,            // parent hash (Merkle)
    pub timestamp: DateTime<Utc>,
    pub kind: TraceKind,
    pub sig: Signature,             // Ed25519 over canonical bytes
}

pub enum TraceKind {
    UserMessage     { channel: ChannelRef, content: Content },
    LlmRequest      { model: ModelId, request_hash: Blake3 },
    LlmResponse     { model: ModelId, response: Content, tokens: TokenUsage },
    ToolCall        { tool: ToolId, input: Value, skill: Option<SkillRef> },
    ToolResult      { call_seq: u64, output: Value, error: Option<ErrorInfo> },
    MemoryRead      { query: String, claims: Vec<ClaimId>, latency_us: u64 },
    MemoryWrite     { claim: Claim, verified: VerifyResult },
    ChannelReply    { channel: ChannelRef, content: Content },
    Checkpoint      { note: String }, // user-annotated waypoint

    // Multi-agent — every spawn, send, receive, kill is hash-chained and
    // linked to the parent agent's trace session, forming a spawn tree that
    // can be verified and (v2) deterministically replayed end-to-end.
    AgentSpawned    { child: AgentId, role: AgentRole, parent: AgentId, manifest_hash: TraceHash },
    AgentSent       { from: AgentId, to: AgentId, message: Content },
    AgentReceived   { from: AgentId, to: AgentId, message: Content },
    AgentKilled     { agent: AgentId, reason: String },
    WorkflowStep    { step: u32, agent: AgentId, input: Value, output: Option<Value> },
}

pub type TraceHash = Blake3;        // 32-byte content hash
```

**Canonicalization rule:** `TraceEvent` bytes are canonicalized via `serde_json` with sorted keys + fixed numeric precision before hashing. Signatures are over the canonical bytes, not the event struct. Prompt-cache-safe (deterministic ordering).

### Skill Manifest (on-disk at `skills/<name>/SKILL.md` + `module.wasm`)

OpenClaw-compatible frontmatter + HelloRoot extension block:

```markdown
---
name: weather
description: "Fetch forecasts from NOAA and OpenMeteo. Use when: (1) user asks about weather, (2) planning outdoor activities."
metadata:
  openclaw:
    emoji: "☀️"
    requires: { bins: [] }
  helloroot:
    version: "0.1.0"
    module: "module.wasm"
    module_sha256: "a1b2c3..."
    signing_key: "did:key:z6Mk..."
    capabilities:
      tools: ["http_get"]
      network: ["api.weather.gov", "api.open-meteo.com"]
      memory_read: []
      memory_write: ["self.*"]
      clock: true
      random: false
    resources:
      max_wall_ms: 5000
      max_mem_mb: 32
---

# Weather

Instructions for the LLM here (same as OpenClaw markdown body).
```

**Compatibility:** strips-to OpenClaw format if `helloroot` block is removed. HelloRoot skills import OpenClaw skills as **prompt-only** (no Wasm execution) with a capability gate that blocks everything by default.

### Capability Model

Enforced by the Extism host (helloroot-skills). Denied syscalls return `Err(Denied)` to the Wasm module; the skill cannot catch this to retry under a different name.

```rust
pub struct Capabilities {
    pub tools: Vec<String>,               // allowlist of tool IDs
    pub network: NetworkPolicy,           // "none" | "allowlist(Vec<host>)" | "any"
    pub memory_read: MemoryScope,         // "none" | "self" | "workspace" | "all"
    pub memory_write: MemoryScope,        // "none" | "self" | "workspace"
    pub clock: bool,
    pub random: bool,
    pub env: Vec<String>,                 // allowlisted env var names
    pub resources: ResourceLimits,
}
```

---

## Agent Loop State Machine (helloroot-runtime)

```
            ┌─────────────────┐
            │      Idle       │
            └────────┬────────┘
                     │ ChannelEvent
                     ▼
            ┌─────────────────┐
            │    Perceive     │  ← trace: UserMessage
            └────────┬────────┘
                     │
                     ▼
            ┌─────────────────┐
            │     Recall      │  ← MCP call: thinkingroot.brief
            │  (thinkingroot) │     trace: MemoryRead
            └────────┬────────┘
                     │
                     ▼
            ┌─────────────────┐
            │      Plan       │  ← LLM call
            │     (LLM)       │     trace: LlmRequest + LlmResponse
            └────────┬────────┘
                     │
                     ▼
            ┌─────────────────┐
      ┌────►│      Act        │  ← tool or skill call
      │     │ (tools/skills)  │     trace: ToolCall + ToolResult
      │     └────────┬────────┘
      │              │
      │              ▼
      │     ┌─────────────────┐
      │     │    Observe      │  ← integrate result
      │     └────────┬────────┘
      │              │
      │      more?   │  ← loop guard: max 20 turns, context-budget check
      └──────────────┤
                     │ done
                     ▼
            ┌─────────────────┐
            │     Compose     │  ← synthesize reply
            └────────┬────────┘
                     │
                     ▼
            ┌─────────────────┐
            │    Contribute   │  ← MCP call: thinkingroot.contribute
            │  (thinkingroot) │     trace: MemoryWrite (with verify result)
            └────────┬────────┘
                     │
                     ▼
            ┌─────────────────┐
            │     Reply       │  ← channel.send
            │   (channel)     │     trace: ChannelReply
            └────────┬────────┘
                     │
                     ▼
            ┌─────────────────┐
            │      Idle       │
            └─────────────────┘
```

**Invariants:**
- Every state transition emits exactly one `TraceEvent`.
- `Contribute` is **off the reply hot path** — it runs after `Reply` is sent (user doesn't wait on extraction).
- `Recall` is bounded by thinkingroot's p95 (0.117 ms embedded).
- Loop guard kills runaway tool-calling at 20 turns.
- All tool/skill calls are dispatched through the capability gate.

---

## Channel Adapter (helloroot-channels)

```rust
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    fn id(&self) -> ChannelId;
    async fn connect(&mut self, cfg: ChannelConfig) -> Result<(), ChannelError>;
    async fn stream(&mut self) -> Pin<Box<dyn Stream<Item = Result<ChannelEvent>> + Send>>;
    async fn send(&mut self, reply: ChannelReply) -> Result<MessageId, ChannelError>;
    async fn disconnect(&mut self) -> Result<(), ChannelError>;
}
```

### v1 channel set (5, with crates already verified as production-grade)

| Channel | Crate | License | Notes |
|---|---|---|---|
| Telegram | `teloxide` 0.17 | MIT | Flagship; webhooks + long-polling |
| Slack | `slack-morphism` 2.19 | Apache-2.0 | Web API + Events API + Socket Mode |
| Discord | `serenity` | ISC | Gateway + REST + slash; voice via `songbird` |
| Matrix | `matrix-rust-sdk` | Apache-2.0 | Element-maintained; E2EE |
| iMessage | `imessage-rs` (BlueBubbles bridge) | per-repo | Requires macOS host w/ SIP disabled |

### v2 channel set (5 more, deferred for v0.2)

Feishu (`lark-rs`), LINE (`line-bot-sdk-rust`), Mattermost (`mattermost_api`), Microsoft Teams (custom wrapper on `graph-rs-sdk`), WhatsApp (official Business Cloud API wrapper — **not** the ToS-violating `whatsapp-rust` scraper).

### Explicitly skipped

- **Signal** — `presage` is AGPL-3.0 (viral); incompatible with our MIT/Apache dual license. Users needing Signal can run the bridge externally and send via webhook.
- **WeChat** — ecosystem fragmented, Tencent ToS landmines, low US/EU ROI. Revisit only for a dedicated China build.

---

## Skill System (helloroot-skills)

### Loader pipeline

```
SKILL.md parse ──► Manifest validate ──► Signature verify (Ed25519)
                                              │
                                              ▼
                                   Capability check against policy
                                              │
                                              ▼
                                     Load module.wasm via Extism
                                              │
                                              ▼
                          Register with Host Functions (tools, memory, net)
                                              │
                                              ▼
                                          Ready to invoke
```

### Host functions exposed to Wasm (Extism ABI)

Each host function checks capabilities before executing. A skill with `network: none` that tries `host_http_get` receives `Err(CapabilityDenied)` synchronously.

```rust
// Tools
host_call_tool(tool_id: &str, input_json: &[u8]) -> Result<Vec<u8>>

// Memory (via thinkingroot)
host_memory_query(query: &str) -> Result<Vec<u8>>
host_memory_contribute(claim_json: &[u8]) -> Result<Vec<u8>>

// Network
host_http_request(req_json: &[u8]) -> Result<Vec<u8>>   // method/url/headers/body

// Clock / Random
host_now_ms() -> u64
host_random_bytes(n: u32) -> Vec<u8>

// Logging (always allowed)
host_log(level: u32, msg: &str)
```

### Why Extism and not raw Wasmtime/WASI 0.2 Components

- **Extism 1.21 is production-proven** (ZeroClaw ships it; ~1k production users).
- **Simpler ABI** (byte-in/byte-out) vs WASI 0.2's component types → skill authors can ship in Rust/TS/Go/Python with minimal scaffolding.
- **Upgrade path to WASI 0.2** is clean — the host-function surface is the stable interface; we can swap the runtime in v0.3 without breaking skills.

v2 may add WASI 0.2 Component Model as a second runtime alongside Extism, once the component ecosystem matures.

---

## Multi-Agent System (helloroot-agents)

HelloRoot is a **multi-agent system from v1**, not a single-agent loop. An agent can spawn child agents, delegate work, and coordinate results. This matches OpenClaw's ACP and OpenFang's 31-agent orchestration — and surpasses both by making the entire spawn tree hash-chained, signed, and (in v2) deterministically replayable.

### 1. Agent Manifest (`agent.toml`, OpenFang-compatible)

```toml
# ~/.helloroot/agents/orchestrator/agent.toml
name = "orchestrator"
role = "orchestrator"
version = "0.1.0"
description = "Meta-agent that decomposes tasks and delegates to specialists."
author = "helloroot"

[model]
provider = "anthropic"
model = "claude-sonnet-4-6"
max_tokens = 8192
temperature = 0.3
system_prompt = """You are the Orchestrator. Decompose complex tasks, delegate to specialists via agent_send, synthesize results."""

[capabilities]
tools = ["web_fetch", "github"]
network = "any"
memory_read = "workspace"
memory_write = "self"
agent_spawn = ["coder", "researcher", "writer", "debugger"]   # allowlist of roles
resources = { max_wall_ms = 60000, max_mem_mb = 128 }

[[fallback_models]]
provider = "openai"
model = "gpt-5.4"
```

### 2. Core types (`helloroot-types`)

```rust
pub struct AgentId(String);            // "agent-<uuidv7>"
pub struct AgentRole(String);          // "orchestrator" | "coder" | ... user-defined ok

pub struct AgentManifest {
    pub name: String,
    pub role: AgentRole,
    pub version: String,
    pub description: String,
    pub model: ModelConfig,
    pub system_prompt: String,
    pub capabilities: AgentCapabilities,
    pub fallback_models: Vec<ModelConfig>,
}

pub struct AgentCapabilities {
    pub tools: Vec<String>,
    pub network: NetworkPolicy,
    pub memory_read: MemoryScope,
    pub memory_write: MemoryScope,
    pub agent_spawn: SpawnPolicy,         // None | Allowlist(Vec<AgentRole>) | Any
    pub resources: ResourceLimits,
}

pub struct AgentHandle {
    pub id: AgentId,
    pub role: AgentRole,
    pub parent: Option<AgentId>,
    pub trace_session: SessionId,         // each agent has its own trace session
    pub manifest_hash: TraceHash,         // what manifest spawned it
}

pub enum SpawnPolicy {
    None,
    Allowlist(Vec<AgentRole>),
    Any,
}
```

### 3. Spawner (`helloroot-agents::Spawner`)

Two spawn modes:

```rust
pub enum SpawnMode {
    /// Child runs as a tokio task in the same process.
    /// Shares trace log backend, signing key, memory client.
    /// Lightest overhead; appropriate for trusted specialist agents.
    InProcess,

    /// Child runs as a separate process with stdio MCP transport.
    /// Has its own trace session (signed by parent-issued subkey).
    /// Use for third-party or untrusted agent code.
    Subprocess { bin: PathBuf, args: Vec<String> },
}

#[async_trait]
pub trait Spawner: Send + Sync + 'static {
    async fn spawn(
        &self,
        manifest: &AgentManifest,
        parent: AgentId,
        mode: SpawnMode,
    ) -> Result<AgentHandle, SpawnError>;

    async fn send(&self, from: AgentId, to: AgentId, msg: Content) -> Result<Content, SendError>;
    async fn list(&self) -> Vec<AgentHandle>;
    async fn kill(&self, agent: AgentId, reason: &str) -> Result<(), KillError>;
}
```

Every spawn emits `TraceKind::AgentSpawned`. Every send emits `AgentSent` + `AgentReceived` on both sides. Every kill emits `AgentKilled`. The chain links parent-child across trace sessions via `manifest_hash` and `parent`.

### 4. Agent-to-agent transport: hybrid (4 protocols, right tool per case)

**Architectural correction (2026-04-21 review):** we originally wrote "MCP for all agent-to-agent" — this was wrong. Competitor source-code audit confirmed nobody uses MCP as their internal inter-agent bus. OpenClaw has proprietary ACP (`src/acp/*.ts`, 9+ files) for rich lifecycle semantics MCP can't express. OpenFang uses native Rust types internally and A2A only for cross-framework. Using MCP for in-process agent-to-agent adds ~100–1000× latency overhead (JSON serialization, transport) over native Rust trait calls, for zero benefit.

**The right architecture: an `AgentBus` trait with four backends, chosen per destination:**

| Destination of target agent | Protocol | Transport | p50 latency target |
|---|---|---|---|
| **In-process** (same binary — orchestrator → coder) | native Rust trait + `tokio::mpsc` | in-memory channel, typed `AgentMessage` | **<100 μs** |
| **Subprocess** (sandboxed child w/ parent-issued subkey) | MCP over stdio | `rmcp` stdio transport | **<5 ms** |
| **Cross-framework** (HelloRoot ↔ OpenFang/LangGraph/etc.) | Google A2A | HTTPS + JSON Agent Cards | **<50 ms** |
| **Inbound from external agent** (Claude Code queries our KG) | MCP server | Streamable HTTP or stdio | served by `thinkingroot-serve` |

The `AgentBus` trait:

```rust
#[async_trait]
pub trait AgentBus: Send + Sync + 'static {
    /// Fire-and-forget send to another agent.
    async fn send(&self, from: AgentId, to: AgentId, msg: AgentMessage) -> Result<()>;

    /// Synchronous request-reply.
    async fn request(&self, from: AgentId, to: AgentId, msg: AgentMessage)
        -> Result<AgentReply>;

    /// Subscribe to messages addressed to an agent.
    async fn subscribe(&self, id: AgentId) -> mpsc::Receiver<AgentMessage>;

    /// Describe the transport (for trace + debug).
    fn transport(&self) -> BusTransport;
}

pub enum BusTransport {
    InProcess,                     // tokio::mpsc, typed AgentMessage
    McpStdio  { pid: u32 },        // subprocess child
    A2aHttp   { endpoint: Url },   // cross-framework peer
}
```

Three implementations in `helloroot-agents`:

```rust
// 1. InProcessBus — the default, fastest path. Used for all bundled roles.
pub struct InProcessBus {
    channels: DashMap<AgentId, mpsc::Sender<AgentMessage>>,
    trace: Arc<TraceLog>,
}

// 2. McpStdioBus — for subprocess agents (untrusted code, parent-issued subkey).
//    Wraps `rmcp` stdio client; exposes send/request via MCP tool calls.
pub struct McpStdioBus {
    child: Arc<Mutex<rmcp::RunningService<rmcp::RoleClient, _>>>,
    trace: Arc<TraceLog>,
}

// 3. A2aBus — for talking to external framework agents (OpenFang, LangGraph).
//    Discovers peer via /.well-known/agent.json; exchanges A2aTask objects.
pub struct A2aBus {
    http: reqwest::Client,
    peer_card: AgentCard,
    trace: Arc<TraceLog>,
}
```

**The orchestrator calls `bus.request(child_agent, msg)` without caring which backend is used.** The Spawner picks the right `AgentBus` impl when it creates the child, based on `SpawnMode`:

```rust
match spawn_mode {
    SpawnMode::InProcess               => InProcessBus::register(child_id, rx),
    SpawnMode::Subprocess { bin, .. }  => McpStdioBus::spawn(child_id, bin).await?,
}
// A2A peers are registered explicitly via `helloroot peer add <url>`
```

**MCP server mode (inbound) is unrelated to the AgentBus.** It exposes our thinkingroot-compiled-KG tools (`ask`, `brief`, `investigate`, `focus`, `contribute`, `search`) to *external* callers — the federation play. It is not how *our* agents talk to each other internally.

**A2A Agent Card (per Google spec)** — published at `http://<host>:<port>/.well-known/agent.json` for cross-framework discovery:

```rust
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    pub capabilities: A2aCapabilities,
    pub skills: Vec<A2aSkillDescriptor>,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
}
```

**Every message on any bus is a hash-chained trace event** (`AgentSent` + `AgentReceived`), so transport choice doesn't fragment observability.

### 5. Host tools exposed to every agent

Standard orchestration toolset (OpenFang-compatible names, so their agents can call ours and vice versa):

| Tool | Purpose |
|---|---|
| `agent_list` | See all running agents + roles + parent IDs |
| `agent_spawn(role, manifest_override?)` | Spawn a new specialist; enforces parent's `agent_spawn` allowlist |
| `agent_send(to, message) → reply` | Synchronous send (blocks for reply) |
| `agent_send_async(to, message)` | Fire-and-forget; result arrives as an event |
| `agent_kill(agent_id, reason)` | Terminate |
| `memory_query(query)` | Read shared compiled KG (via thinkingroot MCP) |
| `memory_contribute(claim)` | Write to shared KG (verified) |
| `workflow_step(step, input)` | Annotate a workflow position in the trace |

### 6. Bundled v1 agent library (10 roles)

Ships as `helloroot-agents/agents/<role>/agent.toml` + `SKILL.md` pairs — pure config, shares the runtime:

1. **orchestrator** — decomposer/delegator (OpenFang-style prompt)
2. **planner** — long-horizon task planning
3. **researcher** — web + memory retrieval + synthesis
4. **coder** — delegates to Claude Code / Codex via MCP or subprocess
5. **reviewer** — code review with citations
6. **writer** — long-form content
7. **analyst** — data + metrics interpretation
8. **debugger** — bug investigation + root cause
9. **security-auditor** — security review
10. **test-engineer** — test design + execution

Each is customizable by copying into `~/.helloroot/agents/<role>/` and editing the manifest.

### 7. Shared memory across the spawn tree

All agents read/write the **same thinkingroot compiled KG** through the shared `helloroot-runtime` memory client. This is the "shared brain" — orchestrator sees child results through `MemoryRead` events just like any other claim. Agent isolation is via `memory_write` scope:

- `MemoryScope::SelfOnly` — agent writes into `self.<agent_id>.*` namespace; other agents can read but not overwrite
- `MemoryScope::Workspace` — agent writes into shared workspace namespace (default for orchestrator)
- `MemoryScope::None` — agent cannot write memory (read-only specialists)

Reads are cheap (0.117 ms p95), so every agent can query the full workspace KG without duplicating data — unlike RAG-per-agent approaches in other systems.

### 8. Workflows

**v1 — orchestrator-driven.** Simple pattern: orchestrator decomposes, delegates via `agent_send`, synthesizes. Every step traced. Matches OpenFang's orchestrator + OpenClaw's ACP delegate flow. Implementation: just the agent's system prompt + spawn/send primitives.

**v2 — declarative workflow DSL.** A workflow is a typed graph of (`agent`, `task`, `depends_on`, `on_error`) nodes. Parallel branches execute concurrently. The whole graph serializes into the trace and can be replayed or forked. Provides type-safe workflow composition beyond ad-hoc orchestrator prompts.

```rust
// v2 sketch
pub struct Workflow {
    pub id: WorkflowId,
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
}

pub struct WorkflowNode {
    pub id: NodeId,
    pub agent: AgentRole,
    pub task: String,
    pub capability_override: Option<AgentCapabilities>,
    pub on_error: ErrorPolicy,
}

pub struct WorkflowEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub condition: Option<Condition>,  // e.g., "if output.score > 0.8"
}
```

### 9. Security invariants added by multi-agent

**M-1.** A parent cannot spawn a role outside its `agent_spawn` allowlist. Enforced at `Spawner::spawn` before any child state is created.

**M-2.** Child agents inherit a *strict subset* of the parent's capabilities. No privilege escalation through spawning.

**M-3.** Subprocess agents sign their trace with a **parent-issued subkey** (derived Ed25519 keypair). The parent can revoke by rotating the subkey root.

**M-4.** `agent_send` payloads are traced in full on both sides; replay reconstructs the exact message bytes.

**M-5.** Resource limits compose: sum of child `max_wall_ms` cannot exceed parent's remaining budget.

---

## World-Class Differentiators

Six capabilities that compound HelloRoot's core primitives (trace, thinkingroot KG, multi-agent, Wasm sandbox) into user-facing features no competitor ships. Each is cheap because it reuses existing infrastructure; each is a world-first when combined with the rest.

### D-1. Built-in Eval Harness (via Deterministic Replay)

**Goal:** the first agent framework with automated regression testing built into the runtime, not bolted on.

**How it works.** Every session's hash-chained trace is replayable (v2). On each new release of an agent manifest, skill, or system prompt, `helloroot eval` replays the last N real user sessions in stubbed mode and diffs outputs against the original responses. Regressions surface as a report: *"session 8234 now produces answer X, previously Y, confidence delta -0.14"*.

**Implementation.** A new CLI command + a fixed-size replay sampler over existing trace log + a diff reporter. Lands alongside v2 deterministic replay.

```rust
pub struct EvalSuite {
    pub sessions: Vec<SessionId>,         // sampled real sessions
    pub baseline: HashMap<SessionId, ReplaySnapshot>,
}

impl EvalSuite {
    pub async fn run_against(&self, build: &AgentBuild) -> EvalReport;
}

pub struct EvalReport {
    pub regressions: Vec<SessionDiff>,
    pub confidence_shifts: Vec<ConfidenceDelta>,
    pub tool_call_divergence: Vec<ToolDivergence>,
}
```

**Target phase:** v0.2 (requires deterministic replay from Phase 10).

**Why world-first:** LangSmith/Braintrust/Helicone do observation. We do regression detection on the multi-agent graph — no competitor has this.

### D-2. Cost & Budget as First-Class Primitives

**Goal:** every agent knows its own cost in dollars, respects a budget, and shows the user real-time spend.

**How it works.** Every `LlmResponse` trace event already carries `TokenUsage`. We extend it with `cost_usd` computed from a static provider pricing table (maintained as JSON, updated monthly). An agent's `AgentCapabilities` gains a `budget_usd: Option<f64>` field. Orchestrator enforces budgets: at 80%, switches to cheapest-capable provider; at 100%, refuses further spend without user confirmation.

`helloroot cost` CLI shows last day / week / month by agent role + provider + workflow.

```rust
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
    pub cost_usd: f64,    // NEW: computed from pricing table
}

pub struct AgentCapabilities {
    // ... existing fields ...
    pub budget_usd: Option<BudgetPolicy>,
}

pub enum BudgetPolicy {
    Hard { limit_usd: f64, period: BudgetPeriod },
    Soft { limit_usd: f64, notify_at_pct: u8 },
}
```

**Target phase:** v0.1.1 (lands ~2–3 weeks after v0.1 release).

**Why world-first:** OpenClaw, OpenFang, ZeroClaw don't expose cost accounting. Enterprise buyers care; transparency appeals to everyone.

### D-3. Encryption at Rest (Default)

**Goal:** a stolen laptop cannot reveal your conversations, memory, or credentials.

**How it works.** All data under `~/.helloroot/` except public keys is encrypted with **XChaCha20-Poly1305**. Key is held in the OS keychain (macOS Keychain / Linux Secret Service / Windows Credential Manager) via the `keyring` crate. Passphrase derivation uses `argon2` for migration/backup flows. No custom crypto — standard audited Rust crates (`chacha20poly1305`, `argon2`, `keyring`).

Storage layout tagged with per-file encryption header so rotation is incremental:

```
~/.helloroot/
├── sessions/
│   └── <uuid>/
│       ├── trace.log           # encrypted (XChaCha20-Poly1305)
│       └── trace.idx           # encrypted
├── memory/
│   └── ...                     # encrypted (thinkingroot KG files)
├── credentials/                # encrypted (double-layer; already mode 0700)
└── agent.key                   # encrypted (derived key in keychain)
```

Default on. Config `encrypt_at_rest = false` opt-out (not recommended).

**Target phase:** v0.1 (ships at launch — we don't want users storing plaintext on day 1).

**Why world-first:** OpenClaw stores conversations, credentials, and memory in plaintext on disk. HelloRoot is the first personal AI agent where data privacy is a filesystem-layer guarantee, not a marketing promise.

### D-4. Dry-Run + Graduated Approval Gates

**Goal:** trust. The user sees what the agent is about to do before it does it.

**How it works.** Every capability-touching action (tool call, skill invoke, channel send, memory write) flows through an `ApprovalGate`. Three modes per capability:

- `Auto` — execute without asking
- `Approve` — show the plan, wait for user consent
- `DryRun` — log what *would* happen but don't execute

Defaults are sane:
- Read-only operations (`web-fetch`, `memory_query`, `file-ops` read) → `Auto`
- Writes to user data (`email send`, `calendar write`, `file-ops write`) → `Approve`
- Destructive (`rm`, `drop`, `delete`, shell without allowlist) → `Approve` **always**, cannot be downgraded
- In subprocess agents, parent can only *widen* child defaults, never narrow destructive ones below `Approve`

Before any multi-step workflow, the orchestrator assembles a plan and presents it:
```
Plan:
  1. researcher: search web for "quantum computing papers 2026"
  2. researcher: fetch 3 URLs, extract abstracts       [auto]
  3. writer: compose 8-page summary                    [auto]
  4. pdf-generate: render to /tmp/quantum.pdf          [approve file-write]
  5. telegram.send: attach pdf to your chat            [approve send]

Approve [a]ll / [s]elect / [c]ancel / [d]ry-run?
```

Every gate decision is a trace event (`ApprovalRequested`, `ApprovalGranted`, `ApprovalDenied`) — auditable.

**Target phase:** v0.1 (trust is a launch-day feature, not a patch).

**Why world-first:** OpenClaw and competitors run actions then log them. HelloRoot is the first agent where the user sees and approves the plan *before* execution by default.

### D-5. Meta-Cognition — Confidence-Aware Agents

**Goal:** the first agent that knows what it doesn't know and says so.

**How it works.** thinkingroot already stores confidence scores on every claim (from the verify pipeline). We surface this to agents at recall time: each retrieved claim in the context window carries its confidence. The system prompt (for every bundled agent) instructs: *"If a load-bearing fact has confidence < 0.75, acknowledge uncertainty and offer to investigate further."*

Agents can call a new host tool:

```rust
host_investigate(claim: &str) -> InvestigationResult
```

which triggers a targeted deep-dive: researcher sub-agent, additional sources, contradiction check. The original orchestrator gets a verification result and a revised claim with higher confidence (or explicit "unable to verify").

User experience: *"I'm 62% confident this is the current pricing — would you like me to check?"* instead of *"The price is $X"* (false confidence).

**Target phase:** v0.1.1 (depends on thinkingroot's existing confidence scores; just needs exposure + prompt changes).

**Why world-first:** every agent in 2026 answers with uniform confidence. HelloRoot is the first that distinguishes *"I'm sure"* from *"this is my best guess"* in the reply itself.

### D-6. Self-Learning Agents (Reflection into thinkingroot)

**Goal:** agents that improve per-user over time, locally, without any fine-tuning.

**How it works.** At the end of every session, each agent that participated runs a one-shot reflection: *"What worked? What didn't? What should I do differently next time with this user?"* The output is 2–5 claims written into a reserved namespace in the thinkingroot KG (`self.<agent_role>.learned.*`). On the next session, the agent's recall step includes its own prior reflections scoped to this user.

Because thinkingroot stores typed claims with contradictions, reflections that disagree with later evidence get flagged and superseded — the agent corrects its own learned lessons over time.

```rust
pub enum MemoryScope {
    None,
    SelfOnly,            // <agent_id>.*
    SelfLearned,         // NEW: <agent_role>.learned.* — persistent reflection space
    Workspace,
    All,
}
```

**Target phase:** v0.2 (benefits from CRDT sync so reflections persist across devices).

**Why world-first:** other agents use RAG over user-authored notes. HelloRoot agents write their *own* typed, verified reflections into the same KG they recall from — enabling measurable, auditable self-improvement without fine-tuning.

### Summary of world-first differentiators

| # | Feature | Phase | World-first? | Depends on |
|---|---|---|---|---|
| D-1 | Eval harness via replay | v0.2 | ✅ | Deterministic replay (Phase 10) |
| D-2 | Cost + budget primitives | v0.1.1 | ✅ | Existing `TokenUsage` trace |
| D-3 | Encryption at rest | **v0.1** | ✅ | Storage layer |
| D-4 | Dry-run + approval gates | **v0.1** | ✅ | Capability gate + trace |
| D-5 | Meta-cognition (confidence) | v0.1.1 | ✅ | thinkingroot confidence scores |
| D-6 | Self-learning agents | v0.2 | ✅ | CRDT sync + thinkingroot KG |

---

## World-First Primitives (Research-Backed)

Seventeen primitives sourced from (a) deep reading of thinkingroot's own source, which revealed world-first capabilities not yet exposed to end users, (b) 2025–2026 agent research literature (arXiv), (c) user pain-point synthesis. These are **in addition to** the six World-Class Differentiators (D-1..D-6). Where D-section items are iterative improvements, R-section items are paradigm-level inventions or expositions.

### Summary table

| ID | Primitive | Lands | Grounding |
|---|---|---|---|
| R-1 | CompAG paradigm exposure | v0.1 | thinkingroot `docs/2026-04-12-compag-compile-augmented-generation.md` — zero arXiv prior art |
| R-2 | Admission-tier-aware answers | v0.1 | thinkingroot `AdmissionTier` (Rooted/Attested/Quarantined/Rejected) in `claim.rs` |
| R-3 | Reflexive queries ("blindspots") | v0.1.1 | thinkingroot Phase 9 Reflect; zero prior art per internal audit |
| R-4 | Disaggregated neurosymbolic controller | v0.1 | [arXiv:2601.17915](https://arxiv.org/html/2601.17915v1) — ReAct has >50% abandonment; separation fixes it |
| R-5 | Calibration head (trained, not prompted) | v0.3 | [arXiv:2406.08391](https://arxiv.org/abs/2406.08391) — prompting alone is insufficient |
| R-6 | Bitemporal recall | v0.2 | thinkingroot `claim.valid_from/valid_until/event_date` already bitemporal; Zep/XTDB pattern |
| R-7 | Self-healing memory (executable predicates) | v0.2 | thinkingroot `claim.predicate` field ready |
| R-8 | Biscuit-per-tool-call (capability attenuation) | v0.1 | [biscuit-auth](https://github.com/biscuit-auth/biscuit-rust) (Eclipse, production Rust) |
| R-9 | Personal Transparency Log (Rekor-for-one) | v0.2 | Sigstore Rekor model; [IACR 2016/683](https://eprint.iacr.org/2016/683.pdf) sparse-Merkle non-membership |
| R-10 | PSI inter-agent knowledge handshake | v0.2 | [OpenMined PSI](https://github.com/OpenMined/PSI) Rust bindings |
| R-11 | Inspectable/editable/portable memory UI | v0.1 | User wish #1 per research synthesis |
| R-12 | Action Capsules (true undo) | v0.1 | User wish #3; addresses destructive-action trust-break pattern |
| R-13 | Agent Covenant (signed user–agent contract) | v0.1 | Novel; addresses trust-break pattern identified in research |
| R-14 | Personality Pin (across model upgrades) | v0.1.1 | Emotionally critical per research; addresses "lobotomized update" pain |
| R-15 | Knowledge branches (memory fork/merge) | **v0.1.1** | thinkingroot `thinkingroot-branch` crate **already shipped** (1,161 LOC + MCP tools); 8 integration gaps close in Phase 0 |
| R-16 | Compiled agents via verified IR | v0.3 | [arXiv:2603.27299](https://arxiv.org/abs/2603.27299) + DbC contracts [arXiv:2508.03665](https://arxiv.org/pdf/2508.03665) |
| R-17 | Failure-first memory | v0.3 | [arXiv:2602.22406](https://arxiv.org/html/2602.22406) — capability signal is in failures |

### R-1. CompAG Paradigm Exposure (v0.1)

**What.** HelloRoot positions itself not as a RAG agent but as a **CompAG (Compile-Augmented Generation)** agent. Every answer is backed by pre-verified, typed, tribunal-grounded claims — not raw text chunks dumped into context.

**Why world-first.** thinkingroot's internal prior-art check (arXiv full-text, April 2026) found zero results for "Compile-Augmented Generation" or "CompAG." This is a new paradigm thinkingroot invented; HelloRoot is its user-facing reference implementation.

**Implementation.** Prose-only in v0.1 — positioning, README, docs. The capability already exists in thinkingroot-serve's MCP tools (`ask`, `brief`, `investigate`). We name it and lean into it.

### R-2. Admission-Tier-Aware Answers (v0.1)

**What.** thinkingroot's Rooting gate assigns every claim an `AdmissionTier`: `Rooted` (passed all probes), `Attested` (source-backed by grounding only), `Quarantined` (non-fatal probe failed), `Rejected` (fatal probe failed). HelloRoot surfaces this to users:

- Every answer displays tier distribution: *"backed by 3 Rooted claims, 1 Attested"*.
- Agents support a `--trust rooted` flag: only use claims that passed all probes.
- Low-trust answers are explicitly hedged: *"I'm drawing on an Attested claim that hasn't been re-verified since March 14 — want me to re-root it?"*

**Why world-first.** No agent in 2026 exposes admission tiers. Every existing system presents all memory with uniform confidence.

**Implementation.** `thinkingroot-serve` returns `admission_tier` in claim responses (already does internally). HelloRoot's runtime threads it into system-prompt context + reply formatting. ~2 days of work.

### R-4. Disaggregated Neurosymbolic Controller (v0.1)

**What.** Ground-truth finding: current ReAct-style agents conflate "what to investigate next" with "how to invoke tools," causing >50% task abandonment even on frontier models (arXiv:2601.17915, Jan 2026). HelloRoot's `helloroot-runtime` separates the two:

- **Symbolic controller (Rust):** owns workflow traversal, termination conditions, belief bookkeeping, error recovery, loop guards, budget enforcement, approval gates.
- **LLM:** does only bounded local reasoning — "given this context, what is the next step?" — never controls traversal or termination.

This is a restructure of the agent loop state machine already in the spec, not an addition. It means the agent loop is *not* a prompt template with a tool-call parser; it's a typed state machine where the LLM is called as a pure function.

**Why world-first.** No shipped agent (OpenClaw, OpenFang, AutoGen, LangGraph, Claude Code, Cursor) does this cleanly. Even those that claim it leak control logic into prompts.

**Implementation.** Reshape `helloroot-runtime::Agent` so all state transitions are Rust code; LLM is invoked for bounded reasoning steps. Builds on existing 8-state loop.

### R-8. Biscuit-Per-Tool-Call Capability Attenuation (v0.1)

**What.** No ambient authority. Every tool invocation requires a freshly-attenuated **biscuit token** (Eclipse biscuit-auth): Ed25519-signed, Datalog-scoped, time-bounded, offline-verifiable.

Example: the user grants `calendar.read` for the next 5 minutes with max 20 events. The agent derives a biscuit carrying that scope and passes it to every sub-agent and tool invocation. The `calendar` skill's host function verifies the biscuit before executing — no env vars, no ambient keys, no standing permissions.

```rust
pub struct Biscuit {
    pub root_signature: Ed25519Sig,
    pub blocks: Vec<BiscuitBlock>,   // each block can attenuate, not widen
    pub revocation_id: RevocationId,
}

pub trait ToolHost {
    async fn call(&self, tool: &str, input: Value, biscuit: &Biscuit) -> Result<Value>;
    // ^ verifies biscuit.check(Datalog { tool, input, now() }) before executing
}
```

**Why world-first.** No personal AI uses capability-attenuated tokens per tool call today. OpenClaw and every competitor use ambient API keys in env or config. Biscuits give cryptographically scoped, time-bounded, offline-revocable permissions.

**Implementation.** Integrate the `biscuit-auth` crate (production-mature, sub-ms verification, 300-byte tokens). Every `host_call_tool`, `host_http_request`, `host_memory_*` extends to accept + verify a biscuit. ~1 week.

### R-11. Inspectable/Editable/Portable Memory UI (v0.1)

**What.** User's #1 wish per research: *"show me what you remember, let me delete a row, let me export it."* HelloRoot ships:

- `helloroot memory browse` — TUI showing the typed KG; user navigates entities, sees claims with confidence + admission tier + source
- `helloroot memory delete <claim_id>` — hard delete + trace event
- `helloroot memory edit <claim_id>` — supersede with a user-authored claim (trace preserves both)
- `helloroot memory export [--format json|md|ttl]` — portable export
- `helloroot memory import <file>` — re-import (versioned under the user's signing key)

**Why world-first.** Memory across current agents is either invisible (ChatGPT "Memory" panel), creepy (Pi), or buried in chat logs. Nobody gives users a real, editable, portable view of what the agent knows.

**Implementation.** TUI built with `ratatui`; export pipes thinkingroot's existing claim store; delete/edit wraps existing graph ops. ~1.5 weeks.

### R-12. Action Capsules — True Undo (v0.1)

**What.** Every destructive action (file write, email send, calendar modify, shell exec, memory delete) is wrapped in a **Capsule** that records: (a) declared pre-state hash, (b) inverse operation, (c) side-effect descriptor, (d) grace period (default 60 s). The user sees a receipt:

> *"Sent email to sarah@example.com — ref #a3f. Undo within 60 s? [y/N]"*

`helloroot undo <capsule_id>` replays the inverse. Capsules record in the trace log; after the grace period, the capsule is sealed (still referenceable for audit) but no longer auto-undoable from the receipt — user can still run `helloroot action reverse <id>` with approval gate.

```rust
pub struct ActionCapsule {
    pub id: CapsuleId,
    pub action: ActionDescriptor,
    pub pre_state_hash: TraceHash,           // what we committed we saw
    pub inverse: InverseOp,                   // how to undo
    pub side_effect: SideEffectKind,          // Network | File | Memory | Skill
    pub grace_period: Duration,
    pub sealed_at: Option<DateTime<Utc>>,
}

pub enum InverseOp {
    DeleteFile { path: PathBuf },
    RestoreFile { path: PathBuf, content_hash: TraceHash },
    SendFollowup { to: String, subject: String, body: String },  // e.g., apology/retract
    ClaimSupersede { old: ClaimId, new: Option<ClaimId> },
    CustomScript { capsule: Vec<u8> },        // Wasm-sandboxed inverse
}
```

**Why world-first.** OpenClaw, Claude Code, Cursor have no real undo — `git` is the only safety net, and only for file ops. HelloRoot extends transactional undo across email, calendar, memory, shell, even network side effects (via compensating actions).

**Implementation.** New `helloroot-capsule` crate. Every bundled skill declares its inverse operation in its SKILL.md manifest. Runtime wraps skill invocations in capsules. ~2 weeks.

### R-13. Agent Covenant — Signed User–Agent Contract (v0.1)

**What.** At install, HelloRoot signs a **Covenant** with the user — a human-readable, machine-parseable document listing commitments:

```yaml
covenant:
  version: "1.0.0"
  signed_by: "helloroot-agent-key-<fingerprint>"
  commitments:
    - id: never_destructive_without_approval
      text: "I will never delete files, send emails, post to channels, or make irreversible changes without explicit approval, unless you've set the policy to Auto."
    - id: always_show_confidence
      text: "When I answer a factual question, I will always indicate my confidence level and cite sources when available."
    - id: never_change_persona_silently
      text: "If my underlying model changes, I will not change my personality or voice unless you explicitly opt in."
    - id: keep_trace_verifiable
      text: "Every action I take is recorded in a hash-chained signed trace. I will never delete or modify past trace entries."
    - id: memory_is_yours
      text: "Your memory is yours. You can browse, edit, delete, and export it at any time. I will not store memory in any form that you cannot see."
  violations:
    - logged_to_trace: true
    - user_notified: true
    - requires_user_acknowledgment: true
```

The covenant is a claim in the user's KG. Violations emit a `CovenantViolation` trace event + user notification; the agent cannot proceed until the user acknowledges or revises the covenant.

**Why world-first.** No personal AI formalizes its behavioral commitments as a signed, enforceable contract. This transforms trust from a marketing claim into an auditable invariant.

**Implementation.** Covenant file + validator in `helloroot-runtime`; hooks into capability gate and trace log. ~3 days.

---

### Other primitives — concise definitions

**R-3. Reflexive queries (v0.1.1).** Exposes thinkingroot's Phase 9 Reflect as user-facing tools: `helloroot blindspots <topic>` returns known-unknown claims ("92% of similar entities have X; you don't"). Agent system prompts gain: *"When the user asks about an entity, offer to check for blindspots."*

**R-5. Calibration head (v0.3).** Train a small ONNX head (≤10 MB) on ~1000 graded HelloRoot answers to predict calibrated confidence per response. Drives ask-vs-act decisions. Requires user-opt-in telemetry or a synthetic dataset.

**R-6. Bitemporal recall (v0.2).** User queries: *"what did you know about X on March 1?"* vs *"what's true about X now?"* — thinkingroot's `valid_from/valid_until/event_date` already stores both axes. HelloRoot CLI exposes `--as-of <date>` on recall tools.

**R-7. Self-healing predicates (v0.2).** Claims with `predicate` (executable assertion against source bytes) re-verify on a daily sweep or on-demand. Failed predicates auto-transition Rooted→Quarantined. User receives notification: *"Previously-verified claim X is now stale — source file changed."*

**R-9. Personal Transparency Log (v0.2).** Every agent action emits a signed entry to a local append-only Merkle log (Rekor-style). User can selectively reveal: *"prove my agent never touched email this month"* — a signed non-membership proof. Uses sparse Merkle trees ([IACR 2016/683](https://eprint.iacr.org/2016/683.pdf)).

**R-10. PSI inter-agent handshake (v0.2).** Two HelloRoots compute intersection of claim hashes via OpenMined's PSI without revealing non-overlap. Enables: *"Alice's HelloRoot confirms she attended meeting M without revealing her other meetings."* Rust bindings production-ready.

**R-14. Personality Pin (v0.1.1).** User locks personality (system prompt fragments, tone, opinions, refusal stance) at install. Model upgrades preserve pinned personality; any change requires explicit opt-in. Addresses "lobotomized update" trauma.

**R-15. Knowledge branches (v0.1.1).** `helloroot memory fork feature/auth-redesign` → hypothesize; `helloroot memory merge` with semantic conflict resolution via thinkingroot-branch. KVC is already built (`thinkingroot-branch` 1,161 LOC + `create_branch`/`checkout_branch`/`diff_branch`/`merge_branch` MCP tools shipped); HelloRoot just exposes them as user-facing CLI once Phase 0 closes branch-aware reads (Gap 1) and per-branch vector index (Gap 2).

**R-16. Compiled agents via verified IR (v0.3).** User describes a recurring workflow in natural language. HelloRoot compiles to a typed Rust trait impl + LTL contracts (pre/post/invariants). Subsequent runs are deterministic + model-checked. Ambitious; dependent on frontier-model quality for compilation.

**R-17. Failure-first memory (v0.3).** Every failed action or user correction is written as a high-salience claim. Per [arXiv:2602.22406](https://arxiv.org/html/2602.22406): the signal for capability expansion is in failures, not successes. Agents that learn-from-mistakes eventually outperform agents that remember-successes.

---

## LLM Provider Trait (helloroot-providers)

```rust
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    fn supports(&self) -> ProviderCaps;   // streaming, tools, vision, etc.
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, ProviderError>;
    async fn stream(&self, req: CompleteRequest) -> Result<Pin<Box<dyn Stream<Item = TokenDelta> + Send>>, ProviderError>;
}
```

### v1 providers

| Provider | Transport | Notes |
|---|---|---|
| Anthropic | HTTPS | Claude Opus 4.7, Sonnet 4.6, Haiku 4.5 |
| OpenAI | HTTPS | GPT-5.4, GPT-5.3-Codex |
| Ollama | local HTTP | runs any GGUF; user manages models |
| `mistral.rs` | embedded Rust | feature-flagged (`--features local-llm`); Metal + CUDA + CPU |

### Model routing

A lightweight routing policy decides which provider to use per call:

- **Default:** user-configured preferred provider.
- **Cost-aware:** small intents (classify, summarize) → local/Haiku; long-form reasoning → Opus.
- **Fallback chain:** if preferred provider 5xx's twice, try the next-cheapest. Traced.

---

## MCP Surface (helloroot-runtime + thinkingroot-serve)

### Client side (outbound)

HelloRoot discovers and calls arbitrary MCP servers configured by the user (`~/.helloroot/mcp/*.toml`). Uses `rmcp` (official Rust MCP SDK) over stdio or Streamable HTTP. Each tool call becomes a `ToolCall`/`ToolResult` trace event pair.

### Server side (inbound) — **the under-served slot**

HelloRoot embeds `thinkingroot-serve`'s MCP server and exposes:

- `ask(question) → answer` — full RAG pipeline over the compiled KG
- `brief(topic) → briefing` — structured summary with citations
- `investigate(claim) → verification` — check a claim against the KG
- `focus(entity) → focused context` — pull all claims about one entity
- `contribute(claim) → verify_result` — extract-link-verify pipeline write
- `search(query) → results` — raw graph query
- `get_relations(entity) → relation_map` — entity relation traversal
- `query_claims(pattern) → claims` — structured claim query

**Result:** any other agent (Claude Code, Cursor, Codex, OpenFang, ZeroClaw) can point their MCP client at a running HelloRoot and get compiled-KG memory for free. This is a federation play — HelloRoot becomes infrastructure, not just an endpoint.

---

## Trace Log Format (helloroot-trace)

### Storage

Append-only binary log at `~/.helloroot/sessions/<session_id>/trace.log`. Each record:

```
[ u32 length ][ canonical CBOR bytes of TraceEvent ][ 32-byte Blake3 hash ][ 64-byte Ed25519 sig ]
```

Monotonic `seq` and `prev` linkage give the Merkle chain. An index file (`trace.idx`) maps `seq → file_offset` for O(1) lookup.

### Public API

```rust
impl TraceLog {
    pub async fn append(&mut self, kind: TraceKind) -> Result<TraceHash>;
    pub async fn get(&self, seq: u64) -> Result<TraceEvent>;
    pub async fn verify_chain(&self) -> Result<(), ChainError>;
    pub async fn export_bundle(&self, range: Range<u64>) -> Result<EvidenceBundle>;
}
```

### Why this matters in v1 (before replay)

- **Audit:** a user can `HelloRoot trace export <session>` and hand the bundle to a CISO, auditor, or regulator.
- **Tamper-evident:** any mutation to past events breaks the hash chain.
- **Debug:** when something goes wrong, you have the exact sequence.

### Deterministic replay (v2)

With the trace in place, v2 adds:
- **Input stubs:** every `LlmResponse` is cached by `request_hash`. Replay feeds the stored response instead of calling the provider.
- **Seeded RNG:** `host_random_bytes` is seeded from `session_id || seq` during replay.
- **Clock pinning:** `host_now_ms` returns the recorded timestamp.
- **Tool-result pinning:** every tool call's result is recorded and replayed.

Result: `HelloRoot replay <session>` reproduces the session bit-for-bit. Any divergence is in *our code*, not model variance — making agent regressions finally debuggable.

---

## CRDT Peer Sync (helloroot-sync, v2)

The trace log is wrapped in an Automerge document; peer sync runs over Iroh QUIC.

### Data model

```rust
// Automerge document per session
pub struct SessionDoc {
    pub session: Session,
    pub events: List<TraceEvent>,        // append-only, CRDT-list
    pub pins: Map<Annotation>,           // user annotations on specific hashes
}
```

Events are append-only, so the CRDT merge is trivial (list union by `seq` + `prev` chain). Conflicts can only arise on `pins` (user annotations), where last-writer-wins + merge-friendly data structures handle it.

### Transport

Iroh QUIC with `iroh-docs` or direct `iroh-gossip`. Peers discover each other via:
1. Shared Iroh ticket (user pastes on second device)
2. LAN mDNS discovery (private network)
3. User-hosted relay (Tailscale-style)

No cloud account required. Encryption is Iroh's default (Noise + TLS).

### Why v2 and not v1

- Adds ~3 MB to binary (Iroh + Automerge deps).
- Introduces a new consistency mental model users must understand.
- Channels + skills + trace must be stable first.

---

## Operational Design (Production Hygiene)

Six gap areas filled with research-grounded designs. Sources cited inline; full bibliography at end of section.

### O-1. Failure Modes & Recovery

**Architectural commitment: the trace log IS the journal.** Every spawn / MCP call / tool invocation / approval / channel send is a journaled entry with an idempotency key. On any crash, `helloroot serve` replays from the last completed entry. This is the [Temporal](https://docs.temporal.io/workflow-execution) / [Restate](https://www.restate.dev/blog/solving-durable-executions-immutability-problem) durable-execution pattern, applied to our existing hash-chained log — no new infrastructure required.

**Crash-only software** ([Candea & Fox, HotOS IX 2003](https://dslab.epfl.ch/pubs/crashonly.pdf)): no graceful shutdown path. SIGKILL and SIGTERM take the same code path. Forces the recovery code to be correct.

**Per-failure-mode patterns** (cited):

| Failure | Pattern | Crate / Source |
|---|---|---|
| LLM provider 5xx / 429 / network | Layered retry: SDK retry (inner) → `Router` fallback chain Anthropic→OpenAI→Ollama (middle) → circuit-breaker cooldown (outer). Honor `Retry-After` headers. | `backoff`, `reqwest-retry`; LiteLLM Router pattern |
| Channel WebSocket disconnect mid-reply | Persist `(channel_id, inbound_message_id, turn_id)` BEFORE generating; replay unacked outbound with idempotency key per chunk; dedupe inbound by channel-native ID (`update_id` for Telegram, `envelope_id` for Slack Socket Mode, opcode-6 RESUME for Discord) | `serenity` Shard auto-RESUME, `slack-morphism`, `teloxide` |
| CozoDB / SQLite corruption | Startup runs `PRAGMA integrity_check`; on fail, fall back to newest daily `VACUUM INTO` snapshot. WAL mode + `synchronous=NORMAL` default; trace log uses `synchronous=FULL` | `rusqlite` bundled, `cozo` |
| Disk full mid-trace-write | Frame format `len ‖ CBOR ‖ blake3 ‖ ed25519_sig`; on startup scan from last signed checkpoint, truncate at first frame with bad length/hash. Chain integrity preserved because prev-hash is inside next frame. | Mirrors [Rekor v2 / RFC 6962](https://blog.sigstore.dev/rekor-v2-ga/) |
| Wasm skill panic / hang | One fresh `Store` per skill call + epoch deadline + memory cap (32 MB default) + 5 s wall clock. Drop-on-trap eliminates host leaks, sidesteps [CVE-2026-27195](https://github.com/bytecodealliance/wasmtime/security/advisories) (async future drop reentrance bug, patched ≥40.0.4). | `wasmtime` epoch interruption, `extism` ≥1.21 |
| Multi-agent network drop | Replay journal until last completed spawn/send/kill; resume orchestrator from there. Children re-spawned from `manifest_hash`. | In-house, mirrors Temporal saga pattern |
| User SIGKILL mid-trace-write | `write_all` then `fsync(file)` + `fsync(dir)` every N=10 frames or M=200 ms; verifier truncates trailing partial frame on boot | `rustix::fs::fsync`; SQLite atomic-commit pattern |
| Loop guard exceeded (>20 turns) | Hard cap 20 tool-turns + USD budget + repeated tool-call hash early-stop signal. Emit `StopReason::LoopGuard`; surface recap + "continue / stop / new plan" prompt. | Mirrors Claude Code `max_turns` |

### O-2. First-Run Onboarding (`helloroot onboard`)

**Hard target: 5 minutes to first agent reply** ([Amplitude time-to-value research](https://amplitude.com/blog/time-to-value-drives-user-retention) — first 5-minute improvements drive ~50% LTV lift).

**Consent legally must be discrete + readable + granular** ([Italy Garante €5M Replika fine](https://captaincompliance.com/education/replikas-e5-million-gdpr-fine-key-takeaways-for-ai-developers/), Feb 2023). The Agent Covenant signing is the discrete consent moment.

**7 steps, target 4–5 min total:**

```
[1/7] Welcome (5s)              — "memory + traces stay local"
[2/7] Agent Covenant (45s)      — 12-line covenant; sign with generated Ed25519 key
[3/7] Personality pin (15s)     — Concise / Warm / Analyst / Debug / Custom-later
[4/7] Model provider (30s)      — autodetect ANTHROPIC_API_KEY / OPENAI_API_KEY in env
[5/7] Connect 1 channel (90s)   — Telegram first (token paste, fastest); OAuth ones deferred
[6/7] Memory baseline (20s)     — if ~/.openclaw/ detected, offer import; else empty
[7/7] First interaction (30s)   — real reply via the connected channel = the aha moment
```

Defaults chosen for speed (generated keypair, one channel, autodetect provider). Skippable: extra channels, peer sync, custom personality, daemon install (prompted at end). Channels added later via `helloroot connect <channel>`.

### O-3. Self-Update & Schema Migration

**Update mechanism:** the [Astral `uv` pattern](https://github.com/astral-sh/uv) — `self-replace` crate (atomic `rename(2)` swap) + downloader + minisign signature verification.

| Concern | Choice | Rationale |
|---|---|---|
| Atomic binary swap | `self-replace` (mitsuhiko) | Used by Astral `uv`, Zed; battle-tested |
| Signature verification | `minisign-verify` (Ed25519, no OpenSSL) | Used by Tauri updater + cargo-binstall; pure Rust |
| Update channels | `stable` + `nightly` only (skip `beta` per `uv` precedent) | YAGNI — two channels cover all real cases |
| When to apply | Async check on startup (24 h cache) → non-blocking notice → user runs `helloroot self update` | uv / gh / rustup pattern; never silent-update a CLI (trust) |
| Rollback | Keep `helloroot.old` after swap; `helloroot self rollback` restores it | Standard pattern; no auto-fallback (complexity > value) |
| SQLite migrations | `rusqlite_migration` (lightweight, SQLite-only, Rust-closure migrations) | Smaller than `refinery` for embedded use |
| Config schema migrations | Versioned JSON with explicit `schema_version` + Rust migrator functions | VSCode pattern; CozoDB has no formal migration framework |

CI signs every release with an offline minisign key; pubkey is embedded in the binary at build time.

### O-4. Proactive Notification Model

**Research finding:** *user-authored triggers win; model-authored triggers backfire.* Apple Intelligence Notification Summaries rolled back after fabricated headlines ([BBC News, Jan 16 2025](https://www.bbc.co.uk/news/articles/c0jp4ndr3lvo)); Replika "clingy" complaints (r/replika ongoing); Meta AI unsolicited Instagram DMs widely called creepy ([404 Media, Apr 2025](https://www.404media.co/meta-ai-chatbots-on-instagram-and-facebook-are-initiating-conversations-out-of-the-blue/)); Snapchat My AI pinning itself review-bombed.

**HelloRoot defaults: reactive only.** Opt-in to proactive per-channel via `helloroot channel allow-proactive <id>`.

**Per-channel config** at `~/.helloroot/channels.toml`:
```toml
[telegram.personal]
proactive = "allow"           # allow | mention-only | never
quiet_hours = "22:00-08:00"
max_per_hour = 2

[slack.work]
proactive = "mention-only"

[matrix.private]
proactive = "never"
```

**Trigger taxonomy** (default-on subset is conservative):
- `time` (cron) — **on** when proactive allowed
- `calendar` (event in N min) — **on**
- `event-rule` (user-defined "when email matches X") — **on**
- `state-inferred` (laptop opened after 8 pm, agent idle) — **off** (Apple Intelligence backlash)
- `reflexive` (KG gap or contradiction detected) — **off** (`--experimental` opt-in)

**OS Focus integration:** macOS reads `defaults read com.apple.ncprefs` Focus state; Linux respects `XDG_SESSION_IDLE` + systemd inhibitors. Hard-suppress during Focus unless channel marked `break-through`.

**Cooldowns:** global 4/hour, 10/day; per-channel override; exponential backoff (halve rate) if user doesn't reply within 30 min.

**Trace events:** every proactive send emits `proactive.interrupt` (trigger id, channel, cooldown state, Focus state) — auditable, user-inspectable via `helloroot trace tail`.

### O-5. Background Tasks & Long-Running Agents

**Pattern:** ChatGPT Tasks ([Jan 2025](https://openai.com/index/introducing-tasks/), 10-active cap) + Devin's session model (running/blocked/stopped/finished) + Cursor cloud agents (per-task isolation), grounded on Tokio's `CancellationToken`.

**Architecture:** same process, dedicated `tokio` runtime, task registry keyed by `task_id` holding a `CancellationToken`. State persists via the existing trace log:
```rust
pub enum TraceKind {
    // ... existing ...
    TaskStarted   { id: TaskId, prompt: Content, channel: ChannelRef, parent: Option<AgentId> },
    TaskProgress  { id: TaskId, step: u32, summary: String },
    TaskCompleted { id: TaskId, result: Content },
    TaskFailed    { id: TaskId, reason: String },
    TaskCancelled { id: TaskId, reason: CancellationReason },
}
```

**CLI surface:**
- `helloroot tasks list` — running + recent terminal
- `helloroot tasks status <id>` — single task detail
- `helloroot tasks logs <id> --tail` — stream
- `helloroot tasks cancel <id> [--force]` — graceful (5s drain) or SIGKILL worker

**Limits** (matching ChatGPT Tasks philosophy): max 3 concurrent tasks per user, 10-min cooldown between identical prompts (hash-based dedupe).

**Result delivery:** original channel + durable inbox (`helloroot inbox`) so user always finds the result even if channel was offline.

### O-6. Conversation Interruption

**User types mid-orchestrator-execution. The handler:**

1. Suspend orchestrator at the next `.await` checkpoint (LangGraph-style `interrupt()`)
2. Snapshot in-flight state hash
3. Emit trace `Interrupted { state_hash, step, in_flight_capsules }`
4. Prompt user via channel: *"I was doing X (step 3/5). Your new message says Y. [c]ontinue / [r]eplan / [a]bandon?"*
5. Default `c` after 10 s idle (preserves work)

**Approval gates are NEVER silently dropped on interrupt.** A pending approval becomes `Denied { reason: UserInterrupt }`; logged; in-flight Action Capsules are sealed without execution.

### O-7. Multimodal Inputs

**New crate: `helloroot-attachments`** — handles all non-text inputs from channels.

```rust
pub struct Attachment {
    pub mime: Mime,
    pub bytes: Vec<u8>,
    pub source_uri: Option<Url>,
}

pub enum AttachmentRoute {
    VisionLLM      { resized_to: (u32, u32), encoding: ImageEncoding },
    PdfExtract     { text: String, layout: Option<LayoutInfo>, ingest_to_kg: bool },
    AudioTranscribe{ text: String, lang: String },
}
```

**Routing** by MIME:
- `image/*` → resize to 1568px max edge (`image` 0.25), base64-encode → vision LLM (Claude `image` block, OpenAI `image_url`)
- `application/pdf` → `pdf-extract` (text) or `pdfium-render` (layout) → chunk → ingest as claims via `thinkingroot.contribute`
- `audio/*` → `whisper-rs` (offline whisper.cpp) OR provider STT (`openai /audio/transcriptions`) → treat as user text

**UX example:** User on WhatsApp sends a receipt photo. HelloRoot replies: *"Logged $42 coffee expense, tagged work/travel. Added to your Expenses workspace. Undo? [y/N]"*

### O-8. Migration from OpenClaw

**`helloroot import openclaw` tool** — verified against actual OpenClaw layout in `openclawResearch/openclaw/`:

| Source path | Action | Notes |
|---|---|---|
| `~/.openclaw/memory/<agent>.sqlite` | Skip (derived index) | SQLite is a derived FTS5 index over markdown |
| `~/.openclaw/MEMORY.md`, `~/.openclaw/memory/*.md` | Re-ingest into thinkingroot KG via extractor | `source_uri = openclaw://<agent>/<file>`. Best-effort, lossy (no bitemporal provenance for old data). |
| `~/.openclaw/skills/<name>/SKILL.md` | Copy as **prompt-only** skill in HelloRoot registry | LLM executes via our sandboxed shell tools, NOT OpenClaw's free shell. Tool bindings re-wired through MCP. |
| `~/.openclaw/credentials/*` | **Never** migrate secrets | Emit `~/.helloroot/credentials-todo.md` listing each provider/channel; user runs `helloroot connect` to re-auth. |
| `~/.openclaw/agents/<id>/agent/auth-profiles.json` | Skip | Same — re-auth required |

### O-9. Multi-Profile (Work / Personal / Family)

Pattern: `gh auth switch`, VSCode profiles, git `includeIf`.

**Layout:**
```
~/.helloroot/
├── active                          # symlink → profiles/<current>
└── profiles/
    ├── personal/
    │   ├── config.toml
    │   ├── thinkingroot.cozo       # isolated KG per profile
    │   ├── credentials/
    │   ├── covenant.sig
    │   └── channels/
    ├── work/
    └── family/
```

**CLI:**
- `helloroot profile create work`
- `helloroot profile use personal`
- `helloroot --profile work ask "..."`
- Env: `HELLOROOT_PROFILE=work helloroot serve`

**Channels are bound per profile** — work Slack ≠ personal WhatsApp ≠ family iMessage. **Memory is per profile** — no leakage. **Each profile has its own Agent Covenant** — different commitments per context.

### O-12. Prompt Injection Defense (The 2026 Open Problem)

**Threat model.** Indirect prompt injection — foundational paper [Greshake et al., "Not what you've signed up for", arXiv 2302.12173, AISec'23](https://arxiv.org/abs/2302.12173) — is the #1 attack class against AI agents in 2025-2026. Real 2025 incidents we design against:
- **EchoLeak ([CVE-2025-32711](https://labs.aim-intelligence.ai/echoleak))** — zero-click exfiltration in Microsoft 365 Copilot via email prompt injection, disclosed Jun 2025
- **Invariant Labs GitHub MCP exploit** — poisoned issue leaked private repo through Claude, [May 2025](https://invariantlabs.ai/blog/mcp-github-vulnerability)
- **ChatGPT Atlas omnibar attack** — hidden instructions executed from URL bar, Oct 2025
- **Anthropic AI-orchestrated cyber-espionage** — nation-state used Claude Code with injected tool outputs, [disclosed Nov 2025](https://www.anthropic.com/news/disrupting-AI-espionage)

**Simon Willison's lethal trifecta ([Jun 2025](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/))**: breach requires (a) untrusted input + (b) private data access + (c) external communication. **Break any leg = prevent breach.**

**State-of-the-art defense — CaMeL ([DeepMind, arXiv 2503.18813](https://arxiv.org/abs/2503.18813), Mar 2025)**: privileged planner LLM emits a restricted program with explicit capabilities; quarantined reader LLM only fills data slots. Drops attack success rate on [AgentDojo benchmark](https://arxiv.org/abs/2406.13352) from **47% → ~0%**. This is the *only* defense with deterministic guarantees; spotlighting / instruction hierarchy / training-time defenses (StruQ, SecAlign) are all probabilistic.

**HelloRoot's primitives already align with CaMeL's trust model** — a massive coincidence that lets us ship the SOTA defense without redesigning. Admission tiers ≅ CaMeL trust levels; biscuit tokens ≅ CaMeL's attenuable capabilities; approval gates ≅ CaMeL's human-in-loop escalation.

#### Four additions to the spec

**G-1a. SourceTrust enum on every context fragment (v0.1)**
```rust
#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceTrust {
    Privileged,      // system prompt, Agent Covenant
    User,            // the human user's own current input
    Quarantined,     // everything else: channel messages from others, tool outputs,
                     // retrieved memory claims (regardless of admission tier),
                     // PDFs, webpages, images, audio transcripts
}

pub struct ContextFragment {
    pub content: String,
    pub source: SourceRef,
    pub trust: SourceTrust,
    pub admission_tier: Option<AdmissionTier>,  // if from memory
}
```
Every byte flowing into the LLM's context window carries `SourceTrust`. Propagated through `helloroot-runtime` context assembly + `helloroot-channels` message ingestion + MCP tool outputs. ~1 week of mechanical refactor.

**G-1b. Spotlighting wrapper at channel ingress (v0.1)**
Per [Hines et al. (Microsoft), arXiv 2403.14720](https://arxiv.org/abs/2403.14720): wrap `Quarantined` fragments in `<untrusted src="telegram:chat:42">…</untrusted>` with Unicode tagging so the LLM can distinguish its instructions from data. Reduces ASR ~50% on benchmark. **Cheap first line — not sufficient alone.** ~2 days.

**G-1c. Dual-model planner/executor split — CaMeL-inspired (v0.1.1)**
```
┌──────────────────────────────────────────────────────┐
│  Orchestrator (PRIVILEGED MODEL)                      │
│  - Sees: Privileged + User tokens ONLY                │
│  - Output: plan + biscuit attenuations + tool schema  │
│  - NEVER sees Quarantined content                     │
└───────────────────┬──────────────────────────────────┘
                    │ dispatches
                    ▼
┌──────────────────────────────────────────────────────┐
│  Reader (QUARANTINED MODEL — can be a smaller LLM)    │
│  - Sees: Privileged instructions + Quarantined data   │
│  - Output: TYPED DATA ONLY (no tool calls)            │
│  - Cannot mint biscuits, cannot invoke capabilities   │
└──────────────────────────────────────────────────────┘
```
Attacker-controlled text reaches only the Reader; Reader can't escalate capabilities. **Biggest win — neutralizes the lethal trifecta architecturally.** ~3–4 weeks; requires refactor of agent loop. Based on CaMeL arXiv 2503.18813 — provably breaks the injection chain.

**G-1d. Trifecta-aware approval gate (v0.1)**
Policy engine in `helloroot-policy` computes per action:
```
breach_risk := has_quarantined_input ∧ reads_private_data ∧ writes_externally
```
If all 3 → REQUIRE APPROVAL (upgrades from capability's default mode). ~1 week. Directly cites Willison's Jun 2025 trifecta analysis + Invariant Labs' GitHub-MCP postmortem.

#### Open research we honestly don't solve

- **Multimodal invisible-text defense** — [arXiv 2307.10490](https://arxiv.org/abs/2307.10490) shows image-based injection has no deterministic fix. We rely on `image-caption` skill outputting to `Quarantined` trust level and downstream trifecta gate.
- **Cross-agent trust transitivity** — AgentDojo still shows non-zero ASR with CaMeL in adversarial agent chains. We acknowledge as residual risk in THREAT_MODEL.md.

### O-13. Cryptographically-Proven Deletion + GDPR Portability

**Goal.** When a user deletes a claim (or invokes right-to-be-forgotten), produce a **signed cryptographic receipt** proving the claim was removed — verifiable to a third party (auditor, regulator) without revealing the content. No AI agent ships this in 2026. We uniquely can, because our trace log is already a Merkle chain.

**Regulatory backdrop (cited):**
- [GDPR Art. 17](https://gdpr-info.eu/art-17-gdpr/) erasure within 1 month (extensible to 3) per EDPB Guidelines 01/2022
- [GDPR Art. 20](https://gdpr-info.eu/art-20-gdpr/) portability in "structured, commonly used, machine-readable format"
- [Italy Garante: OpenAI €15M fine (Dec 2024)](https://www.garanteprivacy.it/web/guest/home/docweb/-/docweb-display/docweb/10085455) for inadequate erasure
- [EU AI Act Art. 10(5)](https://eur-lex.europa.eu/eli/reg/2024/1689) requires deletion logs for training data in high-risk systems
- [CNIL 2025 AI recommendations](https://www.cnil.fr/en/ai-how-sheets): "proof of effective deletion"

#### Architecture

**Technical primitives (cited):**
- Sparse Merkle Trees for non-membership proofs — [Dahlberg et al., IACR 2016/683](https://eprint.iacr.org/2016/683.pdf)
- Append-only logs have no deletion by design — [RFC 9162 Certificate Transparency](https://www.rfc-editor.org/rfc/rfc9162.html); you tombstone + re-root
- Crypto-shredding — [NIST SP 800-88 Rev.1 §2.5](https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-88r1.pdf): destroy the key, leave ciphertext unreadable
- Forward-secure signatures — [Bellare-Miner CRYPTO'99](https://cseweb.ucsd.edu/~mihir/papers/fsig.html)

**Delete pipeline:**
```rust
pub async fn delete(&mut self, claim_id: ClaimId, reason: &str) -> Result<DeletionReceipt> {
    // 1. Walk derivation DAG via thinkingroot-core::DerivationProof
    //    Collect transitive closure C of all claims derived from this one.
    let closure = self.thinkingroot.derivation_closure(claim_id)?;

    // 2. For each c ∈ C: append signed Tombstone to trace log
    for c in &closure {
        self.trace.append(TraceKind::Tombstone {
            claim_hash: c.hash, reason: reason.into(), ts: Utc::now()
        }).await?;
    }

    // 3. Crypto-shred: destroy the per-claim AES-256-GCM encryption key
    //    Ciphertext remains on disk but is mathematically inaccessible.
    for c in &closure { self.keystore.shred(c.key_id).await?; }

    // 4. Remove from Sparse Merkle Tree index keyed by claim hash
    //    Subsequent queries for these hashes return non-membership witnesses
    let witnesses: Vec<NonMembershipProof> = closure.iter()
        .map(|c| self.smt.remove(c.hash)).collect();

    // 5. Sign new tree root with forward-secure epoch key
    let prev_root = self.smt.previous_root();
    let new_root = self.smt.root();
    let sig = self.epoch_key.sign(&canonical(&new_root));

    // 6. Emit signed receipt
    Ok(DeletionReceipt {
        deleted_at: Utc::now(),
        claim_hashes: closure.iter().map(|c| c.hash).collect(),
        reason: reason.into(),
        prev_tree_root: prev_root, new_tree_root: new_root,
        non_membership_proofs: witnesses,
        signature: sig,
    })
    // 7. Broadcast tombstone as CRDT op (v2 — propagates to synced devices)
}
```

**Crates:** `sparse-merkle-tree` (Nervos, non-membership witnesses), `rs-merkle`, `ed25519-dalek` v2, `aes-gcm`, `argon2` (key derivation), `blake3` (already used for trace hashing).

**Where it lives:** a new module `helloroot-trace::deletion`; no new top-level crate.

#### GDPR Art. 20 export format

**JSON-LD bundle** — [json-ld.org spec](https://json-ld.org/spec/), satisfies Art. 20's "commonly used, machine-readable" test; richer than CSV, better tooling than Turtle:
```
helloroot-export-<timestamp>.tar.gz
├── export.jsonld        # claims, trace events, config, skills, derivation edges
├── tree_head.sig        # ed25519 signature over final Merkle root
├── manifest.json        # SHA-256 of every file + epoch pubkey
└── README.md            # import instructions
```
Importable into another HelloRoot instance (`helloroot import --verify <bundle>`) or any JSON-LD tool. Mirrors Google Takeout's signed-manifest pattern.

#### Phasing

- **v0.1:** basic deletion (tombstone + crypto-shred + key destroy), JSON-LD export, deletion receipt without SMT proof
- **v0.1.1:** Sparse Merkle Tree + non-membership proofs (full receipt)
- **v0.2:** CRDT tombstone propagation across peer devices

### O-14. Supply Chain Security (SLSA Level 3)

**The xz-utils lesson** ([CVE-2024-3094](https://nvd.nist.gov/vuln/detail/CVE-2024-3094), Mar 2024, [Russ Cox timeline](https://research.swtch.com/xz-timeline)): single-maintainer critical dep; 2-year social-engineering; backdoor shipped only in release tarball (not git); discovered accidentally via 500 ms SSH latency. For a local binary users install via `curl | sh`, we have to do better than hope.

**[SLSA v1.0](https://slsa.dev/spec/v1.0/levels) Build Tracks:**
- **L1:** provenance exists
- **L2:** signed provenance from a hosted build service
- **L3:** hardened, isolated/hermetic builder; non-forgeable provenance
- **L4:** deferred in v1.0 (reproducible builds + two-party review — aspirational)

**HelloRoot targets SLSA L3 at v0.1.**

#### Build pipeline (cited tooling)

| Step | Tool | Source |
|---|---|---|
| Hosted hermetic builder | GitHub-hosted runners (isolation primitive) | |
| Rust release builder | [`cargo-dist`](https://github.com/axodotdev/cargo-dist) (Axo; integrates GitHub Attestations v0.14+) | |
| Signed provenance (in-toto SLSA v1.0 predicate) | [GitHub Artifact Attestations](https://github.blog/2024-05-02-introducing-artifact-attestations-now-in-public-beta/) (GA Jan 2024) | |
| Binary signing | [`cosign`](https://docs.sigstore.dev/cosign/overview/) keyless (Fulcio OIDC + Rekor transparency) | |
| SBOM generation | [`cargo-cyclonedx`](https://github.com/CycloneDX/cargo-cyclonedx) producing CycloneDX 1.6 | |
| Dependency audit gates | [`cargo-vet`](https://mozilla.github.io/cargo-vet/) (Mozilla audit sets) + [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) + [`cargo-audit`](https://github.com/rustsec/rustsec) | Required CI gates |
| Reproducibility CI job | `cargo build --locked --frozen` + `--remap-path-prefix` + pinned toolchain; diff binaries byte-for-byte across two runs | Best-effort; honest status: not bit-identical guaranteed in 2026 |

#### The `helloroot verify` command

User verifies their running binary is what we claim to have built:
```
$ helloroot verify
Checking binary: /usr/local/bin/helloroot (SHA-256: abc123...)
Fetching attestation from GitHub Releases + Sigstore Rekor...
Signed by: github.com/<org>/helloroot @ v0.1.0 (commit def456)
In-toto provenance: ✅ matches running binary
SLSA v1.0 Level 3 attestation verified.
```

Uses [`sigstore-rs`](https://github.com/sigstore/sigstore-rs) ≥ 0.9 (stable). Ships in v0.1 (~3 days of work).

#### Phasing

- **v0.1:** SLSA L2 (GitHub Attestations + cargo-dist), SBOM, all dep-audit gates, `helloroot verify` command
- **v0.1.1:** SLSA L3 (hermetic isolation claims documented), reproducibility CI job reporting bit-diff
- **v0.2:** aspirational SLSA L4 (two-party review + full reproducibility)

### O-15. Formal Threat Model (STRIDE × AI-specific)

**Shipped artifact: [`THREAT_MODEL.md`](./) at the repo root.** No competitor in personal AI publishes a formal threat model. Claude Code has only SECURITY.md; Cursor has none; OpenFang has none. [OpenAI Operator System Card](https://openai.com/index/operator-system-card/) is the closest public example and is the template we'll mirror.

#### STRIDE × Asset matrix (preview — full doc ships in `THREAT_MODEL.md`)

| Asset | S Spoofing | T Tampering | R Repudiation | I Info Disclosure | D DoS | E Elevation |
|---|---|---|---|---|---|---|
| **User memory (KG)** | biscuit-auth caller identity | thinkingroot Rooting gate + contradiction detector; Merkle trace log | signed trace records every write | encryption at rest + MCP localhost-only | rate-limit skill memory_write | CaMeL trust levels + Quarantined inputs can't mint biscuits |
| **Credentials** | OS keychain binding | keychain-sealed; never in env or config | n/a | 0700 mode + encrypt-at-rest + never in trace | n/a | biscuit scope gates every tool call |
| **Trace log** | Ed25519 per-install key | hash-chain + forward-secure epoch signatures | **this IS non-repudiation** | encryption at rest; selective reveal only | fsync + disk-full watchdog | append-only by construction |
| **Signing key** | OS keychain entry | keychain sealed | n/a | keychain sealed | n/a | separate per-install |
| **Skill manifests** | Ed25519 signature + TOFU | signature verification at load | signed | signature includes Wasm SHA-256 | skill resource limits | Wasm sandbox + capability allowlist |
| **Channel messages** | channel-native auth (bot token) | TLS + channel dedup | signed trace entry | per-channel config | per-channel circuit breaker (O-11) | Quarantined trust level |
| **Covenant** | signed at install | hash-chained | yes | public document (no secrets) | n/a | breaking covenant = traced violation |

#### MITRE ATLAS mapping

Each [MITRE ATLAS](https://atlas.mitre.org/) technique applicable to personal AI, with our mitigation:

| ATLAS ID | Technique | HelloRoot mitigation |
|---|---|---|
| AML.T0051 | LLM Prompt Injection | O-12 full stack (SourceTrust + Spotlighting + CaMeL dual-model + trifecta gate) |
| AML.T0054 | LLM Jailbreak | system prompts + Covenant + provider-level safety layer |
| AML.T0057 | LLM Data Leakage | O-12 trifecta gate; O-13 crypto-proven deletion; memory encryption |
| AML.T0043 | Craft Adversarial Data | calibration head (D-5) + admission tiers block low-confidence |
| AML.T0049 | Exploit Public-Facing Application | MCP localhost-only by default; explicit opt-in for WAN |

#### OWASP LLM Top 10 (2025) coverage

| OWASP | HelloRoot mitigation |
|---|---|
| LLM01 Prompt Injection | O-12 full stack |
| LLM02 Sensitive Info Disclosure | O-13 + encryption + trifecta gate |
| LLM05 Improper Output Handling | spotlighting + capability gate on outputs |
| LLM06 Excessive Agency | biscuit attenuation + approval gate + loop/budget guards (O-11) |
| LLM08 Vector/Embedding Weakness | thinkingroot Rooting gate (Quarantined tier) + contradiction detection |

#### Residual risks (honestly documented, not mitigated)

- **Local sudo attacker** — OS is the root of trust; if compromised, all bets off. Mitigation: rely on OS disk encryption (FileVault, LUKS, BitLocker). Documented.
- **Compromised LLM provider** — model could return adversarial responses. Mitigation: provider-agnostic + local fallback + eval harness regression (D-1). Partial.
- **Legal coercion** — gag-order compliance may require disclosing private memory. Mitigation: client-side encryption means provider can't comply on our behalf; user can crypto-shred. Documented as residual user risk.
- **Nation-state actor** — out of scope. Documented.
- **Multimodal invisible injection** — no deterministic defense exists in 2026. Tracked as open research.

### O-11. Watchdog & Self-Healing

**Gap context:** 2025–2026 saw multi-hour outages at every major LLM provider — Anthropic's Aug–Sep 2025 postmortem documented 3 infra bugs degrading up to 16% of requests; AWS us-east-1 Oct 20 2025 DNS race cascade took 140+ services down for 15 h; OpenAI June 2025 OOM cascade ran 34 h; Claude outage Apr 15 2026. LangGraph's default `recursion_limit=25` exists because agent tool-ping-pong was the #1 cause of runaway cost (documented cases: $12 burned in 15 min vs $0.08 baseline). HelloRoot needs a **unified watchdog subsystem** that catches both generic daemon failures and AI-specific runaway modes.

**Architectural commitment:** a single `helloroot-watchdog` crate running a supervisor on a **dedicated OS thread with its own small tokio runtime** (bulkhead — a stalled main runtime cannot silence it). All subsystems register with the supervisor; missed heartbeats trigger declared recovery policies; every supervisor decision is a trace event (auditable + replayable).

#### Supervisor tree (OTP-style, cited from [Erlang/OTP supervisor docs](https://www.erlang.org/doc/system/sup_princ.html))

```
root (one_for_all)
├── daemon-core (one_for_one)
│   ├── trace-log-writer
│   ├── memory-client (thinkingroot MCP)
│   └── config-watcher
├── channel-adapters (one_for_one)    ← one fails, only it restarts
│   ├── telegram
│   ├── slack
│   ├── discord
│   ├── matrix
│   └── imessage (macOS-gated)
├── orchestrator (rest_for_one)       ← orchestrator dies → restart planner+executor+memory-writer after it
│   ├── planner
│   ├── executor
│   └── memory-writer
├── skill-pool (simple_one_for_one)   ← dynamic pool of identical Wasm invocation workers
└── mcp-server (one_for_one)
```

#### Heartbeat API (Temporal-style progress tokens, cited from [Temporal activity-heartbeat docs](https://docs.temporal.io/activities#activity-heartbeat))

```rust
pub struct Heartbeat {
    supervisor: Arc<Supervisor>,
    subsystem_id: SubsystemId,
    progress: AtomicU64,              // monotonic; distinguishes "stuck" from "crashed"
}

impl Heartbeat {
    pub fn alive(&self);                                 // I'm responsive
    pub fn progress(&self, token: u64);                  // I advanced; token distinguishes real progress from busy-looping
    pub fn status(&self, msg: &str);                     // optional human-readable state
}

pub enum RecoveryPolicy {
    Retry       { max: u32, backoff: ExponentialBackoff },
    Fallback    { f: Box<dyn Fn() -> Result<()> + Send + Sync> },
    CircuitBreak{ state: failsafe::StateMachine },
    Restart     { strategy: RestartStrategy },           // one_for_one | rest_for_one | one_for_all
    Escalate,                                            // up-tree to parent supervisor
}

impl Supervisor {
    pub fn register(&self, id: SubsystemId, policy: RecoveryPolicy, deadline_ms: u64) -> Heartbeat;
}
```

**Missed-heartbeat detection:** supervisor scans every 1 s; if `now - last_alive > deadline_ms` OR `progress` unchanged for `stuck_threshold_ms`, the declared `RecoveryPolicy` fires. Panics are caught via `catch_unwind` at subsystem entry and reported as `Crashed`. Every decision emits a `SupervisorEvent` trace entry (so restart history is replayable under our journal model).

#### Kubernetes-style 3-probe model ([K8s liveness/readiness/startup probes](https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/))

Each subsystem exposes three internal probes on the MCP admin channel:
- `startup` — initialization gate (long grace window; no restart while starting)
- `live` — responsive; missed → supervisor restarts
- `ready` — accepting work; missed → traffic redirected away (channel adapters stop pulling new messages)

#### AI-specific watchdog signals (beyond generic liveness)

| Signal | Trip condition | Action | Precedent |
|---|---|---|---|
| **Token-rate stall** | SSE stream idle `> 5 min` (configurable) | Abort + retry non-streaming | Claude Code `CLAUDE_STREAM_IDLE_TIMEOUT_MS` + `CLAUDE_ENABLE_BYTE_WATCHDOG` (GH issues #25979, #33949) |
| **Tool-call loop** | SHA-256 of `(tool_name, canonical_args)` repeated 3× within N steps | Halt + force replan via orchestrator | Codieshub / agentpatterns.tech documented heuristic |
| **Plan divergence** | Agent's structured progress ledger unchanged for K steps | Halt + surface recap to user | Modexa "agent loop" pattern |
| **Budget guard** | ANY of `max_steps` / `max_tokens` / `max_usd` exceeded | Stop workflow, require user re-approval | ZenML "hard caps are non-negotiable"; LangGraph `recursion_limit=25` |
| **Zombie subagent** | Parent `CancellationToken` dropped but child still running | Reap via `tokio-util::task::TaskTracker` | Designed from first principles; no vendor precedent |
| **LLM provider outage** | Per-provider error rate > threshold in rolling window | Circuit break + fall back Sonnet → Haiku → GPT-5.4 → local Ollama | `failsafe` crate; buildmvpfast fallback chain |
| **Channel outage** | Per-channel circuit breaker; isolate so one dead transport doesn't fail whole agent | Mark channel degraded; keep other channels live; surface banner to user | Generic bulkhead pattern |

#### OS integration

- **Linux** — on boot, detect `$NOTIFY_SOCKET` + `$WATCHDOG_USEC`; if present, supervisor pings `sd_notify(WATCHDOG=1)` at `WATCHDOG_USEC/2` ONLY while root subsystem tree is healthy. Missed root heartbeat → systemd restarts per `Restart=on-watchdog`. ([sd_notify docs](https://www.freedesktop.org/software/systemd/man/sd_notify.html))
- **macOS** — ship a `.plist` with `KeepAlive.Crashed=true` + `ThrottleInterval=10`; on unrecoverable root failure, supervisor calls `std::process::abort()` so launchd observes non-zero exit and restarts. ([launchd.plist(5)](https://www.manpagez.com/man/5/launchd.plist/))
- **Windows** — via `windows-service` crate; Service Control Manager handles restart per `SERVICE_FAILURE_ACTIONS`.
- **Optional self-hosted server mode** — `--hardware-watchdog /dev/watchdog` for extreme reliability on Linux servers.

#### Observability

- **Compile-time feature `--features debug-console`** — enables `tokio-console` + `parking_lot::deadlock_detection` (both have overhead; production builds skip)
- **CLI:** `helloroot watchdog status` (subsystem tree + heartbeat latencies), `helloroot watchdog history` (recent restarts)
- **Trace events:** `SupervisorEvent { subsystem, kind: Registered|Heartbeat|Missed|Crashed|Restarted|Escalated }`

#### Bill of materials (verified crates.io versions, 2026-04)

```toml
[dependencies]
tokio                   = "1"
tokio-util              = "0.7.18"      # TaskTracker for zombie reaping
tokio-graceful-shutdown = "0.19.3"      # hierarchical cancellation tree
dashmap                 = "6.1"          # concurrent heartbeat map (DIY liveness)
sysinfo                 = "0.38.4"       # CPU/RSS/disk cross-platform
backon                  = "1.6.0"        # async exponential backoff
failsafe                = "1.3.0"        # circuit breaker (Netflix Hystrix pattern)
governor                = "0.10.4"       # rate-limit restart storms

[target.'cfg(target_os = "linux")'.dependencies]
sd-notify               = "0.5.0"        # WATCHDOG=1 / READY=1 / STATUS=

[target.'cfg(windows)'.dependencies]
windows-service         = "0.8.0"        # SCM integration
```

No dedicated `tokio-supervisor` crate exists (verified 2026-04); the per-task restart policy layer is ~80 LOC built on `tokio-graceful-shutdown` + our own `Supervisor { restart_policy, backoff }` loop around `tokio::spawn`.

#### Composition with existing primitives

- **Trace log is the journal (O-1)** → every `SupervisorEvent` is journaled; restart history is replayable + auditable
- **Crash-only software (O-1)** → supervisor's "restart" path IS our start path; no separate graceful-shutdown bug surface
- **Approval gates (D-4)** → when a supervisor restarts a subsystem mid-workflow, any in-flight approval becomes `Denied{SupervisorRestart}` and logged
- **Action Capsules (R-12)** → capsule grace periods survive restarts (persisted in trace log) so undo still works across supervisor events
- **Multi-agent (R-4 + Multi-Agent Section)** → spawned subagents register with the same supervisor tree; parent's `CancellationToken` reaps children on crash

### O-16. Competitor-Source-Code Adoptions (from 2026-04-21 audit)

Direct source-file audit of `/Users/naveen/Desktop/src/` (Claude Code) + `openclawResearch/` (OpenClaw, OpenFang, ZeroClaw, Spacebot). Findings in `openclawResearch/COMPETITOR_AUDIT.md`. Tier 1 items land in v0.1; Tier 2 in v0.1.1.

#### T-1. Extended task types (v0.1) — from Claude Code `Task.ts:7-13`

Expand `TraceKind::*` with two new task variants Claude Code has that we lack:
```rust
pub enum TaskKind {
    // existing: UserMessage, LlmRequest/Response, ToolCall, MemoryRead, etc.
    // NEW:
    DreamTask   { target: Target, trigger: DreamTrigger },  // background ingestion, off reply path
    MonitorMcp  { server: McpServerId, event_filter: EventFilter },  // event-driven MCP watcher
}

pub enum DreamTrigger {
    Idle { after_seconds: u64 },
    Schedule { cron: String },
    UserRequest,
}
```

**Why `dream`:** matches OpenClaw's "dreaming" concept + our self-learning D-6. Background agents that run during idle / nightly / on-demand, writing to `self.<role>.learned.*` namespace. Orchestrator dispatches; trace records; bounded by R-7 self-healing predicates.

**Why `monitor_mcp`:** our MCP client is pull-based today. A `monitor_mcp` task subscribes to an MCP server's notifications (tool list changes, resource updates) and spawns an agent when conditions fire. Turns HelloRoot into a reactive agent across the MCP ecosystem.

#### T-2. Symlink-attack-resistant IDs (v0.1) — from `Task.ts:94-106`

All `SessionId` / `TaskId` / `AgentId` / `CapsuleId` use 8-char random suffix from 36-char alphabet (`0-9a-z`, lowercase) + single-char type prefix. 36⁸ ≈ 2.8 trillion combinations; resistant to brute-force symlink attacks on on-disk task output files.

```rust
pub fn generate_id(prefix: char) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).unwrap();
    let mut s = String::with_capacity(9);
    s.push(prefix);
    for b in bytes { s.push(ALPHABET[(b as usize) % 36] as char); }
    s
}
```

Replaces UUIDv7 for filesystem-facing IDs (UUIDs stay for internal cross-process refs where predictability doesn't matter). Case-insensitive alphabet plays nicely with macOS HFS+ case-insensitivity.

#### T-3. Denial tracking (v0.1) — from Claude Code `Tool.ts:59`

When user denies a permission via approval gate, record the denial in `DenialTrackingState` within the trace log. Agent's context-assembly includes recent denials so it doesn't re-ask the same question in the same session.

```rust
pub struct DenialRecord {
    pub action_hash: TraceHash,       // what was asked (canonical form)
    pub scope: DenialScope,           // Session | Day | Permanent
    pub reason: Option<String>,
    pub denied_at: DateTime<Utc>,
}

pub enum DenialScope {
    Session,    // default; forgotten on new session
    Day,        // user said "stop asking today"
    Permanent,  // user said "never"
}
```

Orchestrator checks denials before proposing the same action again. Covenant commitment: "I will not ask twice in the same session about something you just denied."

#### T-4. File history snapshots (v0.1) — from Claude Code `utils/fileHistory`

Complement Action Capsules (action-inverse) with per-file *state* snapshots. Before any write, snapshot the file's prior state in content-addressed storage (`~/.helloroot/file-history/<sha256>`). Two-layer undo:
- **Action capsule** undoes the agent's logical action (e.g., "send email")
- **File snapshot** undoes the specific file state change (e.g., "revert this config file")

Store by Blake3 hash; dedupe naturally. Retention policy: keep 30 days or 10 generations, whichever is longer. `helloroot file restore <path> --as-of <time>` uses snapshots directly.

#### T-5. `SetCwd` + workspace binding (v0.1 — incidental find)

Claude Code's `utils/Shell.setCwd` binds agent to working directory explicitly. Our agent loop implicitly uses whatever directory `helloroot` was launched in. Make it explicit: each session has a bound `workspace_path`, visible in trace, displayable in prompts.

#### T-6. (v0.1.1) Commit attribution — from Claude Code `commitAttribution.ts`

Any git operations performed by a HelloRoot agent append to the commit body:
```
Co-Authored-By: HelloRoot <agent-key-fingerprint@helloroot>
```
Fingerprint = first 16 chars of the agent's Ed25519 public-key hash. Ties commit history to the signed trace (same key signs both), making git blame meaningful and auditable.

#### T-7. (v0.1.1) Fast-mode toggle — from Claude Code `fastMode.ts`

User-facing `/fast` slash command and config flag toggles between normal and fast models per session:
- Normal: user's preferred provider + model
- Fast: cheapest-capable fallback (Haiku / Ollama small model)

Explicit cost lever; pairs with D-2 budget primitives.

#### T-8. (v0.1.1) Structured output enforcement — from Claude Code `hookHelpers.registerStructuredOutputEnforcement`

Runtime validation: when an MCP tool declares a JSON schema for its output, validate the actual response before propagating. Use `schemars` + `jsonschema` Rust crates. Invalid outputs become `ToolResult { error: SchemaViolation }` with structured error.

Prevents garbage tool responses from corrupting downstream reasoning. Cheap; ~2 days.

#### T-9. (v0.1.1) Bridge / remote-session — from Claude Code `bridge/` (33 files)

Parallel to CRDT sync (v2), add a **remote session bridge** (v0.1.1 experimental): your phone opens a secure tunnel to your laptop's HelloRoot. Architecture:

- Server side (laptop): `helloroot serve --bridge --bind 127.0.0.1:xxxx` + reverse tunnel (Tailscale, Iroh relay, ngrok user-controlled)
- Client side (phone/tablet/other laptop): `helloroot bridge connect <ticket>`
- Auth: JWT signed by laptop's agent key; client presents `trusted_device_id` + `work_secret`
- Transport: WebSocket; messages are the same MCP protocol we already serve

**Difference from CRDT sync:** CRDT sync replicates state across devices (each device runs its own agent). Bridge connects to one authoritative agent remotely.

#### T-10. (v0.2) `in_process_teammate` mode — from Claude Code `Task.ts:10`

Collaborative pair-mode: user and agent work on the same task simultaneously, agent responds to micro-prompts while user continues typing. Different UX surface from reactive assistant. Defer to v0.2 after core UX stabilizes.

### O-10. i18n + Time Zones

**i18n stack:**
- CLI strings: `rust-i18n` + `locales/*.yml` (lightweight, compile-time macros)
- Channel templates with plurals/gender: `fluent-rs` (Mozilla Project Fluent — handles CLDR correctly)
- Error messages: `thiserror` + fluent keys
- Agent system prompts: append `User locale: <BCP47>; respond in this language` — LLM handles natural-language localization automatically

**Locale priority order:** CLI flag > profile config > `LANG` env > system default > `en-US` fallback. Initial v0.1.1 ships English + Spanish + Mandarin + Hindi + Japanese (covers ~50% of global users); community PRs add the rest.

**Time zones:**
- Crate: `chrono-tz` (mature, IANA tzdb compiled in) — already used by thinkingroot
- Storage: every `valid_time` / `transaction_time` / `event_date` is `DateTime<Utc>` (already true in `claim.rs`)
- Display: per-channel `display_tz` config; CLI/channel formatter converts UTC → local at render
- Parsing: user-supplied times parsed via `chrono-tz` using profile's default
- Provenance: claim records original tz string for round-tripping

**UX:** Personal profile (`America/Los_Angeles`): *"remind me tomorrow 9am"* → stored UTC 17:00; family Slack channel set to `Asia/Kolkata` sees *"9:30 PM IST"*. `helloroot config set timezone Europe/Berlin --profile work`.

---

## Build Sequence (Milestones)

Target: **v0.1 in 20 weeks (incl. 4 weeks thinkingroot foundation closure); v0.2 at week 30.**

### Why a Phase 0 exists

Source audit (2026-04-21) of thinkingroot revealed: **KVC core is fully built** (`thinkingroot-branch` 1,161 LOC + branch-aware MCP tools + REST), **bitemporal claim model is built**, **admission tier data structure is built**. But three foundation pieces HelloRoot depends on are not yet shipped:

1. **8 documented KVC integration gaps** (per `docs/2026-04-14-stream-branches-spec.md`) — branch-aware reads, vector index per branch, auto-session branch on MCP init, stream-branch cleanup, etc. HelloRoot needs all 8 to ride on KVC for agent sessions.
2. **Rooting gate runtime** — crate `thinkingroot-rooting/` exists but probe execution + admission decisions need to land. HelloRoot's R-2 (admission tiers) depends on this.
3. **Reflexive Knowledge (Phase 9 Reflect)** — research complete, not yet coded. HelloRoot's R-3 (blindspots, v0.1.1) depends on this.

Phase 0 closes those before HelloRoot Phase 1 starts. **Sequential ordering, not parallel** — HelloRoot is useless without thinkingroot stable.

### Phase 0 — Thinkingroot Foundation Closure (weeks 1–4)

This phase is **thinkingroot work, not HelloRoot work** — but HelloRoot's v0.1 release bar depends on it. Three streams in parallel:

**Stream A — Close 8 KVC integration gaps** (per `docs/2026-04-14-stream-branches-spec.md` "Implementation Status" table):
- Gap 1: Branch-aware reads on `search` / `investigate` / `brief`
- Gap 2: Vector index copied on branch creation + updated on `contribute`
- Gap 3: `delete_branch` / `list_branches` / `rollback_merge` MCP tools
- Gap 4: Auto-session branch on MCP `initialize`
- Gap 5: Per-branch delta cache
- Gap 6: Branch engine connection pool
- Gap 7: Python SDK branch methods (deferrable to post-v0.1 if scope pressures)
- Gap 8: Stream branch cleanup on session expiry

**Stream B — Finish Rooting gate runtime** (`thinkingroot-rooting/` crate exists):
- Wire all 5 probes (provenance, contradiction, predicate, topology, temporal)
- Admission decisions writing back to `claim.admission_tier`
- Daily re-rooting sweep
- Re-rooting on source change

**Stream C — Build Reflexive Knowledge (Phase 9 Reflect)** (per `docs/2026-04-19-reflexive-knowledge-architecture.md`):
- Datalog pattern discovery queries against CozoDB
- Gap claim generation with `known_unknown` claim type
- Integration into compile pipeline as Phase 9
- Health scoring includes gap density

**Exit criteria:**
- All 8 KVC gaps closed; integration test of stream-branch agent session passes
- Rooting tier transitions verified via tests
- Reflexive Phase 9 produces gap claims on a 50-entity test corpus

Phase 0 owners: thinkingroot core team. HelloRoot work waits.

### Phase 1 — HelloRoot Skeleton (weeks 5–6)
- `helloroot-types` crate with Session, TraceEvent, Capability, ChannelMsg, AgentManifest
- `helloroot-cli` crate with `helloroot init`, `helloroot serve`, `helloroot status`
- Wire into workspace Cargo.toml; CI passes.

### Phase 2 — Core loop + first channel (weeks 7–9)
- `helloroot-runtime` with the 8-state agent loop (Idle → Perceive → Recall → Plan → Act → Observe → Compose → Contribute → Reply → Idle)
- `helloroot-providers` with Anthropic + OpenAI HTTPS drivers (Ollama + `mistral.rs` deferred to Phase 6)
- `helloroot-trace` with append-only hash-chained log + verify
- `helloroot-channels` with Telegram (`teloxide`) as the single Phase-2 adapter
- End-to-end smoke: user types in Telegram → agent replies via Claude Sonnet, trace log contains 7 expected events

### Phase 3 — Memory integration (weeks 10–11)
- `helloroot-runtime` calls `thinkingroot-serve` MCP tools (`brief`, `contribute`)
- Recall on every Perceive, Contribute after every Reply
- Benchmark: memory path stays under 5 ms round-trip

### Phase 4 — Skills (weeks 12–14)
- `helloroot-skills` with Extism loader, manifest parser, signature verify
- Capability gate with `network`, `tools`, `memory_*` enforcement
- Import OpenClaw skill format (prompt-only mode) + native Wasm skills
- Ship **17 bundled skills** covering the 6 top-use categories (email, calendar, research, writing, task-mgmt, meeting-summary) + dev essentials:
  - **Tier 1 — everyday (8):** `web-search` (DuckDuckGo + SearXNG, no API key), `web-fetch`, `email` (IMAP/SMTP), `calendar` (CalDAV), `contacts`, `pdf-read`, `pdf-generate`, `scheduler` (cron + delayed actions)
  - **Tier 2 — content (4):** `transcribe` (whisper.cpp offline), `summarize`, `markdown-render`, `image-caption` (vision API)
  - **Tier 3 — developer (3):** `shell-safe` (capability-gated), `file-ops` (scoped read/write/list/search), `github`
  - **Tier 4 — workspace (2):** `git`, `memory` (exposes thinkingroot `brief`/`contribute`/`investigate` as discoverable tool)
- Binary impact: +10–25 MB for the 17 Wasm skills (compressed at rest). Total install: **full build ~180–220 MB** (HelloRoot core + thinkingroot w/ fastembed ONNX ~100 MB + 17 skills), **lean build ~60–110 MB** (API-only embeddings, `--no-default-features`)

### Phase 5 — Remaining v1 channels (weeks 15–16)
- Slack (`slack-morphism`), Discord (`serenity`), Matrix (`matrix-rust-sdk`), iMessage (`imessage-rs`)
- Shared auth management in `~/.helloroot/credentials/`

### Phase 6 — Multi-agent core (weeks 17–18)
- `helloroot-agents` crate with `AgentManifest`, `Spawner` trait, in-process + subprocess modes
- `TraceKind` agent variants (`AgentSpawned`, `AgentSent`, `AgentReceived`, `AgentKilled`, `WorkflowStep`) plumbed end-to-end
- Host tools: `agent_list`, `agent_spawn`, `agent_send`, `agent_send_async`, `agent_kill`
- A2A protocol: serve `AgentCard` at `/.well-known/agent.json`; client-side discovery
- Ship 10 bundled agent manifests: orchestrator, planner, researcher, coder, reviewer, writer, analyst, debugger, security-auditor, test-engineer
- Parent-issued Ed25519 subkeys for subprocess agents
- Smoke test: orchestrator decomposes "research X, code Y, review Z" → spawns researcher + coder + reviewer → returns synthesized result → full spawn tree verifies via `trace verify`

### Phase 7 — MCP server + encryption + approval gates + revolutionary primitives + operational hygiene + polish (week 19)
- Mount thinkingroot-serve's MCP tools under `helloroot mcp serve`
- MCP server exposes per-agent tool surfaces (federation-ready)
- Ollama provider; `mistral.rs` feature flag
- **D-3 Encryption at rest:** XChaCha20-Poly1305 wrapping for `sessions/`, `memory/`, `credentials/`; `keyring` integration for macOS/Linux/Windows
- **D-4 Dry-run + approval gates:** `ApprovalGate` with Auto/Approve/DryRun modes; destructive ops force Approve; workflow plan preview before orchestrator execution
- **R-1 CompAG positioning:** README, docs, marketing all lead with Compile-Augmented Generation
- **R-2 Admission-tier-aware answers:** serve layer threads `admission_tier` into replies; `--trust rooted` flag
- **R-4 Disaggregated controller:** `helloroot-runtime` restructured so Rust symbolic core owns all state transitions; LLM invoked as pure function for bounded local reasoning only
- **R-8 Biscuit-per-tool-call:** `biscuit-auth` integration; every host function verifies attenuated biscuit; no ambient authority anywhere
- **R-11 Inspectable/editable/portable memory UI:** `helloroot memory {browse, delete, edit, export, import}` with `ratatui` TUI
- **R-12 Action Capsules:** `helloroot-capsule` crate; every bundled skill declares inverse op in manifest; 60 s grace period receipt UI; `helloroot undo <capsule>` replays inverse
- **R-13 Agent Covenant:** signed covenant file at install; covenant violations emit trace event + user notification; cannot proceed without acknowledgment
- **O-1 Failure & recovery:** trace-log-as-journal replay on restart; layered LLM resilience (SDK retry → Router fallback → circuit breaker); per-Wasm-call Store + epoch deadline; loop-guard at 20 turns + budget cap
- **O-2 Onboarding wizard:** `helloroot-onboard` crate; 7-step flow, <5 min target; Telegram-first channel, OpenClaw memory import if `~/.openclaw/` detected
- **O-3 Self-update:** `helloroot-update` crate; `self-replace` + `minisign-verify`; check-and-notify (no auto-apply); `helloroot self update` + `helloroot self rollback`
- **O-4 Proactive notifications:** default reactive-only; per-channel `~/.helloroot/channels.toml`; macOS Focus + Linux XDG idle integration; cooldowns 4/hr, 10/day
- **O-5 Background tasks:** task registry + `CancellationToken`; trace events `TaskStarted/Progress/Completed/Failed/Cancelled`; CLI `helloroot tasks list|status|cancel`; max 3 concurrent
- **O-6 Conversation interruption:** suspend at next await + snapshot + prompt continue/replan/abandon; pending approvals → `Denied{UserInterrupt}`
- **O-7 Multimodal:** `helloroot-attachments` crate; image/PDF/audio routing
- **O-8 OpenClaw migration:** `helloroot import openclaw` — memory re-ingest from MD source, skills as prompt-only, credentials NEVER (re-auth flow)
- **O-9 Multi-profile:** `~/.helloroot/profiles/<name>/` + `helloroot --profile <name>`; isolated KG/credentials/covenant/channels per profile
- **O-11 Watchdog + Self-Healing:** `helloroot-watchdog` crate; OTP-style supervisor tree on dedicated tokio runtime (bulkhead); per-subsystem heartbeat + progress tokens; 7 AI-specific signals (token-rate stall, tool-call loop, plan divergence, budget guard, zombie subagent, provider outage, channel outage); systemd/launchd/SCM integration; Kubernetes-style 3-probe model
- **O-12 Prompt injection defense (partial v1):** `SourceTrust` enum propagated through context assembly; spotlighting wrapper at channel ingress; trifecta-aware approval gate (policy engine computes `untrusted ∧ sensitive ∧ external` → require approval). CaMeL dual-model planner/executor split deferred to v0.1.1.
- **O-13 Crypto-proven deletion (partial v1):** tombstone + crypto-shred on delete; ed25519-signed deletion receipt; JSON-LD GDPR Art. 20 export bundle. Sparse-Merkle non-membership proofs deferred to v0.1.1.
- **O-14 Supply chain (SLSA L2 at v0.1, L3 at v0.1.1):** `cargo-dist` + GitHub Artifact Attestations + CycloneDX SBOM + `cosign` signatures; `cargo-vet`/`cargo-deny`/`cargo-audit` as CI gates; `helloroot verify` command (uses `sigstore-rs`)
- **O-15 Formal threat model (v0.1):** public `THREAT_MODEL.md` with STRIDE × asset matrix, MITRE ATLAS mapping, OWASP LLM Top 10 mitigation table, residual risks
- **O-16 Competitor-source-code adoptions (v0.1 Tier 1):** T-1 `dream` + `monitor_mcp` task types; T-2 symlink-attack-resistant 36⁸-alphabet IDs; T-3 denial tracking; T-4 file history snapshots; T-5 explicit session workspace binding
- Docs + example skills repo + example multi-agent workflows

### Phase 8 — v0.1 Release (week 20)
- **Release bar (full build):** ~180–220 MB install (HelloRoot core + thinkingroot with bundled fastembed ONNX embeddings + 17 Wasm skills), <100 ms cold start, 40–70 MB idle-cold RAM / 130–200 MB idle-warm RAM, 5 channels working, **17 skills sandboxed and functional**, MCP client+server live, **10 bundled agent roles functional with orchestrator-driven workflows**, trace log produces exportable audit bundle across multi-agent spawn trees, **encryption at rest on by default (D-3)**, **approval gates enforced for destructive ops (D-4)**. Lean build (`--no-default-features`, API-only embeddings): ~60–110 MB, <50 ms cold start, 25–50 MB idle.
- HN launch: *"HelloRoot — a multi-agent personal AI with compiled knowledge-graph memory, encrypted by default, every action reviewable. 20 MB core. Open source."*

### Phase 8.5 — v0.1.1 (week 21 — parallel with Phase 9)
- **D-2 Cost & budget primitives:** extend `TokenUsage` with `cost_usd`; provider pricing table (static JSON); `AgentCapabilities::budget_usd` field; `helloroot cost` CLI; orchestrator enforcement at 80%/100% thresholds
- **D-5 Meta-cognition:** surface `Claim.confidence` at recall; `host_investigate` tool; prompt updates to all 10 bundled agents
- **R-3 Reflexive queries:** `helloroot blindspots <topic>` CLI + MCP tool; agent prompts gain blindspot-awareness; exposes thinkingroot Phase 9 Reflect to users (built in Phase 0 Stream C)
- **R-14 Personality Pin:** user locks personality at install; model upgrades preserve unless explicit opt-in; address "lobotomized update" trauma
- **R-15 Knowledge branches (memory fork/merge):** `helloroot memory fork <name>` + `helloroot memory merge` exposing the already-shipped KVC + Phase 0 gap closures
- **O-10 i18n (English + ES + ZH + HI + JA):** `rust-i18n` for CLI; `fluent-rs` for templates; LLM responses already multilingual via locale-aware system prompt
- **O-10 Time zones:** `chrono-tz` integration; per-channel `display_tz`; UTC storage stays
- **O-12 CaMeL dual-model split:** privileged planner + quarantined reader architecture; attacker-controlled content can't mint biscuits (arXiv 2503.18813)
- **O-13 Sparse Merkle non-membership proofs:** upgrade deletion receipts to full crypto-proven non-membership
- **O-14 SLSA L3:** hermetic build isolation claims documented; reproducibility CI job reports bit-diff
- **O-16 Tier 2 adoptions:** T-6 commit attribution; T-7 `/fast` mode toggle; T-8 structured output enforcement; T-9 remote-session bridge (experimental)
- Bug fixes from v0.1 feedback
- Ships ~1 week after v0.1

### Phase 9 — CRDT sync (weeks 21–24)
- `helloroot-sync` with Iroh + Automerge
- Pairing flow: `helloroot pair` generates ticket, second device joins
- Per-agent session sync (orchestrator + child specialists converge across devices)
- Conflict UX for pinned annotations

### Phase 10 — Deterministic replay + eval harness + bitemporal + transparency log + PSI (weeks 25–27)
- Input stubs, seeded RNG, clock pinning, tool-result pinning in `helloroot-trace`
- Agent-spawn replay: re-instantiate children from `manifest_hash`, feed stubbed inputs
- `helloroot replay <session>` + `helloroot fork <hash>` + `helloroot diff <a> <b>` commands
- **D-1 Eval harness:** `helloroot eval` replays sampled historical sessions against a new build, produces `EvalReport` with regression diffs + confidence shifts + tool divergence
- **D-6 Self-learning:** `MemoryScope::SelfLearned` namespace; end-of-session reflection writes 2–5 typed claims; recall includes prior reflections
- **R-6 Bitemporal recall:** `--as-of <date>` flag on all recall tools; distinct valid-time vs transaction-time queries
- **R-7 Self-healing predicates:** daily sweep re-runs `claim.predicate` against source bytes; failed → Quarantined + user notification
- **R-9 Personal Transparency Log:** append-only Merkle log via `rs-merkle`; selective reveal with Ed25519 signed checkpoints; sparse-Merkle non-membership proofs
- **R-10 PSI inter-agent handshake:** `helloroot peer confirm <claim_hash>` uses OpenMined PSI to check shared context without revealing non-overlap
  *(R-15 Knowledge branches moved to v0.1.1 — KVC core already shipped, only needs Phase 0 gap closure)*
- Validation: 100-event single-agent session replay bit-for-bit; 50-event orchestrator + 3-specialist workflow replay bit-for-bit; eval harness detects ≥1 intentional regression across 10 test sessions

### Phase 11 — Declarative workflow DSL + v2 channels + voice (weeks 28–30)
- `Workflow` / `WorkflowNode` types in `helloroot-agents`
- Parallel workflow execution with typed edges + conditions
- Feishu, LINE, Teams, Mattermost, WhatsApp Business channels
- Voice channel (LiveKit) for hands-free agent use

### Phase 12 — v0.2 Release (end of week 30)
- **Release bar:** CRDT P2P sync working across laptop + phone + server, deterministic replay proven on 50-event multi-agent workflows, declarative workflow DSL in tree, 10 channels, voice, A2A interop tested against OpenFang.
- HN launch: *"Replayable, forkable, peer-synced multi-agent AI — the first agent system you can debug."*

---

## Performance Targets

Grounded in measured competitor numbers:

| Metric | OpenClaw | OpenFang | ZeroClaw | **HelloRoot v0.1** |
|---|---|---|---|---|
| Install size (full: HelloRoot + thinkingroot + fastembed ONNX + 17 skills) | 298 MB | 32 MB | 8.8 MB | **~180–220 MB** |
| Install size (lean: API-only embeddings, no ONNX) | — | — | — | **~60–110 MB** |
| Idle RAM — cold (no embedding model loaded) | 145 MB | unpub | <5 MB | **40–70 MB** |
| Idle RAM — warm (embedding model mmap'd after first use) | — | — | — | **130–200 MB** |
| Active RAM (one request in flight) | 500 MB – 2 GB | unpub | unpub | **180–300 MB** |
| Peak RAM (multi-agent + Wasm skill + embed + LLM together) | unpub | unpub | unpub | **350–500 MB** |
| Cold start (full build) | 1,250 ms | <200 ms | <10 ms | **<100 ms** |
| Cold start (lean build) | — | — | — | **<50 ms** |
| RAM floor (minimum to run) | 2 GB | unpub | unpub | **512 MB (lean) · 1 GB (full) · 8 GB (+local LLM)** |
| Memory recall p95 | unpub (slow) | unpub | unpub | **<5 ms** (thinkingroot: 0.117 ms + MCP overhead) |
| Trace append p99 | n/a | n/a | n/a | **<1 ms** |
| First reply (Telegram) | ~3 s | ~1 s | <500 ms | **<700 ms** |

**Honest tradeoff:** we will *not* beat ZeroClaw on pure binary size (they're at 8.8 MB with a flat memory model; we're 2× because thinkingroot's compiler is real code). We trade 15 MB of binary for a categorically better memory layer and MCP server mode. Every other metric matches or beats ZeroClaw.

---

## Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| **Phase 0 thinkingroot work slips** | Medium | High | 4-week budget includes contingency. Stream A (KVC gaps) is well-specced. Stream B (Rooting runtime) has crate already created. Stream C (Reflexive) has research-complete design. If Stream C slips, R-3 moves to v0.1.2 — does not block v0.1. |
| **Subsystem deadlock or hang in production** | Medium | High | O-11 unified watchdog catches stuck tokio tasks, hung Wasm calls, dead channel sockets. Bulkhead runtime ensures supervisor survives main-runtime stall. `tokio-console` + `parking_lot::deadlock_detection` gated behind debug feature for investigation. |
| **Runaway LLM cost from agent loop** | High | High | O-11 budget guard trips on whichever of `max_steps` / `max_tokens` / `max_usd` fires first (ZenML hard-cap principle). Tool-call loop detector halts on 3× repeat of same hash. LangGraph `recursion_limit=25` precedent. |
| **LLM provider multi-hour outage** | Medium (happens ~quarterly) | Medium | Provider circuit breaker + fallback chain Sonnet → Haiku → GPT-5.4 → local Ollama. User sees "degraded mode" banner; agent continues with warnings. Cited precedent: Anthropic Aug-Sep 2025, OpenAI Jun 2025, Claude Apr 15 2026 outages. |
| **Prompt injection leaks user data** | High (industry-wide in 2025-26) | Critical | O-12 full stack: SourceTrust propagation + spotlighting + trifecta gate (v0.1) → CaMeL dual-model split (v0.1.1) drops AgentDojo ASR ~47%→0%. Residual: multimodal invisible-text (open research, documented). |
| **Supply chain compromise (xz-style)** | Low (rare but catastrophic) | Critical | SLSA L3 at v0.1.1 (hermetic builds + attestations); CI-gated `cargo-vet`/`cargo-deny`/`cargo-audit`; `helloroot verify` empowers users to detect tampering. Full mitigation against xz-class: SBOM + dependency review + attestation. |
| **Regulator audit demands proof of deletion** | Medium (GDPR enforcement growing) | High | O-13 crypto-proven deletion with signed `DeletionReceipt`; JSON-LD portable export. Italy Garante €15M OpenAI fine (Dec 2024) is the motivating precedent. |
| OpenFang ships real Wasm sandbox before we do | Medium | Medium | They've had scaffolding since v0.6; execution lag is real. Our Extism v1 + WASI-0.2 v2 path leapfrogs them if they catch up. |
| OpenFang ships CRDT sync first | Low | High | No evidence in their codebase; would require Iroh/Automerge dep they don't currently have. Monitor their commits. |
| Extism upstream breaks compat | Medium | Medium | Pin to `extism = "=1.21"`; maintain a capability-gate shim layer so runtime is swappable. |
| thinkingroot's MCP latency too high for real-time chat | Low | High | Pre-benchmark phase 3; if MCP round-trip exceeds 5 ms, use in-process bindings instead (same Rust types). |
| iMessage requires SIP-disabled macOS host | High | Low | Gate iMessage behind an explicit opt-in flag + install-time warning. |
| `whatsapp-rust` crate lures us into ToS violation | Medium | High | Hard-ban it in CODEOWNERS/deny-list; v2 WhatsApp only via Business Cloud API. |
| OpenClaw pivots to Rust rewrite | Low | High | Even if they do, they can't replicate thinkingroot's 2-year KG pipeline in 6 months. Our moat is the memory layer, not the binary size. |
| Peter Steinberger publicly disputes positioning | Medium | Low | We're additive — HelloRoot interops via OpenClaw skill format. Acknowledge inspiration in README. |

---

## Out of Scope for v1

- **UI (desktop, web, mobile)** — CLI-first; community can build frontends on the MCP surface.
- **Enterprise SSO / RBAC** — single-user v1. Multi-user is v3.
- **Cloud-hosted service** — all deployments are local or self-hosted.
- **Fine-tuning pipeline** — out of scope; use Anthropic/OpenAI/Ollama for models.
- **Voice (inbound/outbound)** — deferred to v2 (LiveKit integration).
- **Browser automation** — OpenClaw has a browser-use layer; we skip until there's a clear MCP-browser-tool standard (one is emerging in Q3 2026).
- **Any channel not in the v1 list of 5** — Slack/Discord/Telegram/Matrix/iMessage only.

---

## Naming & Brand

- **Product:** HelloRoot
- **Binary:** `helloroot` (separate from thinkingroot's `root` — keeps agent surface distinct from KG surface)
- **Crates:** `helloroot-*`
- **Config path:** `~/.helloroot/`
- **MCP server name:** `helloroot` (with `implementation.name = "HelloRoot"`, version from Cargo)
- **Tagline:** *"Hello — the personal AI that remembers."*
- **Audience:** Everyone who wants a private, fast, trustworthy personal AI assistant. Same audience as OpenClaw — no niche gating.

Why HelloRoot: "Hello" is a universal greeting borrowed into nearly every language — signals the product is for everyone, not a niche developer tool. "Root" ties the agent to the thinkingroot knowledge lineage ("thinkingroot thinks, helloroot does"). Together they read as an inviting first contact: say hello, it remembers you forever after.

Why not extend `root` directly (no new binary): muddies the product pitch. `root` = knowledge compiler for agents. `helloroot` = the agent that lives on top of it. Two binaries, one family.

---

## Locked Decisions (2026-04-21)

All 5 prior open questions resolved. Recording the final calls + the reasoning so future contributors don't re-litigate.

### LD-1. OpenClaw skill compatibility — **YES, ship in v0.1**

OpenClaw skills are imported as **prompt-only** (the SKILL.md frontmatter + body is treated as agent instructions; the LLM uses HelloRoot's own sandboxed tools — Wasm + biscuit + capability gate — to execute, NOT OpenClaw's free shell). This taps into the 13,729+ OpenClaw skill ecosystem on day 1 without inheriting their security model. Loader lives in `helloroot-skills` with format compat shim. **Trade-off:** prompt-only mode means OpenClaw skills lose any executable-Node-code paths they had — they become LLM-readable instructions only. Agent uses our Wasm tools to fulfill them.

### LD-2. iMessage — **always-on with runtime detection**

No compile-time feature flag (no `--features imessage`). Single binary ships with `imessage-rs` always linked. At runtime:
- macOS host detected → check for BlueBubbles bridge running → enable iMessage channel automatically
- BlueBubbles not detected → show one-time setup instructions during `helloroot onboard` (link to BlueBubbles install + SIP-disable docs); skip if user declines
- Non-macOS host → channel marked unavailable (silently); no error

Why not feature flag: forces rebuild for one channel; users hate `cargo install --features` complexity. Why detection vs prompt: respects user setup choice without nagging.

### LD-3. MCP server mode — **default ON, localhost-only**

`helloroot serve` exposes the MCP server on `127.0.0.1:<port>` by default. Other agents on the same machine (Claude Code, Cursor, Codex) discover it via standard MCP client config. **Never** binds to `0.0.0.0` or any non-loopback interface unless user runs `helloroot serve --bind 0.0.0.0:<port>` (warning printed; logged as `MCPExposed` trace event; covenant-flagged).

Why default-on: HelloRoot becomes immediately useful as memory infrastructure for the user's other AI tools — biggest single product moment. Why localhost-only: zero risk of surprise WAN exposure; the network-exposure decision is a deliberate user choice, not a default.

### LD-4. Signing — **self-signed in v1, web-of-trust in v2**

Per-install Ed25519 keypair generated during `helloroot onboard`, stored in OS keychain (`keyring` crate). All artifacts signed by this key:
- Trace events (already specified)
- Agent Covenant (already specified)
- User's own bundled skills' manifests
- Action Capsule signatures
- Outbound MCP server identity

Third-party skills installed from external sources show their own signing key fingerprint to the user at install time (TOFU — trust-on-first-use, like SSH host keys). v2 layers a **web-of-trust** model: skills can carry endorsements from other keys; HelloRoot displays the trust path; user configures min-endorsement policy.

Why not full PKI in v1: complexity vs benefit ratio. TOFU + per-user keypair gets us 90% of the security with 10% of the complexity. v2 graduates to web-of-trust if the ecosystem grows.

### LD-5. Repo strategy — **separate repo for HelloRoot**

See "Core Architecture Decision: Separate Repo, Tight Coupling" section above. `github.com/<org>/helloroot` standalone; `thinkingroot` referenced as `git+rev` Cargo dependency. Local dev via submodule. Adds ~1 week to Phase 0 for CI + dependency wiring. Worth it for brand independence + release-cadence independence.

---

## Definition of Done (v0.1)

- [ ] All crates compile on stable Rust, edition 2024, rust-version 1.85
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] Static binary + 17 bundled Wasm skills bundle: full build ≤220 MB (includes fastembed ONNX ~100 MB) / lean build ≤110 MB (`--no-default-features`, API embeddings)
- [ ] Cold start (binary invocation → ready-to-receive channel events): <100 ms full build / <50 ms lean build, measured on M2 MacBook Air
- [ ] Idle RAM (RSS) measured after 60 s of no activity with all 5 channels connected: 40–70 MB cold (before first embedding call), 130–200 MB warm (after embedding model loaded); active-state <300 MB for a typical request
- [ ] Install size budget: full build ≤220 MB (HelloRoot + thinkingroot + fastembed ONNX + 17 Wasm skills); lean build ≤110 MB (API-only embeddings)
- [ ] All 5 v1 channels send + receive end-to-end
- [ ] 17 bundled skills execute in Wasm sandbox with capability enforcement (web-search, web-fetch, email, calendar, contacts, pdf-read, pdf-generate, scheduler, transcribe, summarize, markdown-render, image-caption, shell-safe, file-ops, github, git, memory)
- [ ] 10 bundled agent roles load from `agent.toml` manifests and pass a smoke workflow
- [ ] Orchestrator agent spawns ≥3 specialists, exchanges messages, synthesizes output
- [ ] Subprocess-mode agent launches with parent-issued Ed25519 subkey; trace verifies end-to-end across process boundary
- [ ] A2A `AgentCard` served at `/.well-known/agent.json`; external A2A client can invoke a skill
- [ ] MCP client connects to a live MCP server; MCP server responds to external client
- [ ] Trace log verifies chain on a 1000-event session including `AgentSpawned`/`AgentSent`/`AgentReceived` events
- [ ] `helloroot replay` command exists (v1 dumps trace + parent-child spawn tree; full deterministic replay is v2)
- [ ] End-to-end smoke test: Telegram → orchestrator → spawn researcher → thinkingroot recall → spawn writer → reply → thinkingroot contribute → verified claim in graph
- [ ] **D-3:** encryption at rest enabled by default; keychain round-trip works on macOS + Linux (CI gates); opt-out via `encrypt_at_rest = false` documented but discouraged
- [ ] **D-4:** approval gate enforces default policy (read=auto, write=approve, destructive=always-approve); workflow plan preview rendered before any multi-step orchestration; every gate decision emits an `ApprovalRequested`/`ApprovalGranted`/`ApprovalDenied` trace event
- [ ] **R-2:** every answer response includes admission tier distribution (rooted/attested/quarantined counts); `--trust rooted` filter flag supported
- [ ] **R-4:** `helloroot-runtime::Agent` passes a disaggregation test — no control flow decisions (terminate/branch/retry) originate in LLM output; all state transitions are in Rust code with LLM as pure function
- [ ] **R-8:** every host function (`host_call_tool`, `host_http_request`, `host_memory_*`) verifies a biscuit before executing; biscuit attenuation tested via integration test (parent issues Scope A, child tries Scope A+B → denied)
- [ ] **R-11:** `helloroot memory browse` TUI functional; delete + edit + export (JSON/MD/TTL) + import round-trip works
- [ ] **R-12:** 5+ bundled skills declare inverse operations; `helloroot undo <capsule>` replays inverse for email-send, file-write, memory-write, calendar-create, github-issue-create; grace period UI shows receipt
- [ ] **R-13:** Agent Covenant signed at install; 5 core commitments enforced; covenant violation emits `CovenantViolation` trace event + user notification + acknowledgment required before continuing
- [ ] **O-1:** crash-recovery test passes — kill -9 mid-orchestrator-workflow + restart resumes from last journaled entry; trace chain still verifies; no duplicate side effects
- [ ] **O-1:** LLM provider failover test passes — primary returns 503, router falls back to secondary, traced
- [ ] **O-2:** `helloroot onboard` completes happy path in <5 min on M2 MacBook Air with no prior config; Covenant signed; Telegram connected; first reply received
- [ ] **O-3:** `helloroot self update` downloads, verifies minisign signature, atomically swaps; `helloroot self rollback` restores `.old` binary
- [ ] **O-4:** proactive notification respects per-channel `proactive`, `quiet_hours`, `max_per_hour`; macOS Focus state honored; cooldown after no-reply enforced
- [ ] **O-5:** `helloroot tasks` lifecycle test: spawn long task → check status → cancel gracefully; trace contains all task events
- [ ] **O-6:** mid-workflow interrupt prompts user; pending approval becomes `Denied{UserInterrupt}`
- [ ] **O-7:** multimodal smoke — send image to Telegram → vision LLM call returns description; send PDF → text extracted + ingested into KG
- [ ] **O-8:** `helloroot import openclaw` ingests `~/.openclaw/MEMORY.md` claims; emits credential re-auth TODO list
- [ ] **O-9:** `helloroot --profile work` and `helloroot --profile personal` show fully isolated memory + channels
- [ ] **O-11:** watchdog integration test passes — (a) killed channel adapter is restarted per `one_for_one` policy, (b) hung Wasm skill call (>5 s past epoch deadline) is reaped + Store dropped, (c) stuck background task triggers supervisor after missed heartbeats, (d) tool-call loop (3× same tool+args hash) halts orchestrator, (e) budget guard stops workflow at `max_usd` cap, (f) on Linux, systemd `WATCHDOG=1` ping cadence correct; on macOS, `.plist` restart on abort verified
- [ ] **AgentBus latency gates** (per Multi-Agent Section 4): in-process agent-to-agent request-reply p50 <100 μs (InProcessBus; native `tokio::mpsc`); subprocess p50 <5 ms (McpStdioBus); cross-framework A2A p50 <50 ms (A2aBus; localhost peer test)
- [ ] **O-12:** prompt-injection defense tests pass — (a) `SourceTrust` enum propagated through all context assembly paths, (b) quarantined content wrapped in spotlighting markers at channel ingress, (c) AgentDojo subset (at least 10 canonical attacks) run in CI — all mitigated by trifecta gate forcing approval or by dual-model split (if v0.1.1+) or by admission tier rejection
- [ ] **O-13:** deletion test passes — (a) `helloroot memory delete <claim>` walks derivation closure, (b) cryptographic key is shredded (decryption fails post-delete), (c) signed `DeletionReceipt` returned with previous+new tree root, (d) `helloroot export --format jsonld` produces valid signed bundle importable via `helloroot import --verify`
- [ ] **O-14:** release pipeline produces SLSA v1.0 provenance + CycloneDX SBOM + cosign signature; `helloroot verify` command against own binary succeeds; `cargo-vet`/`cargo-deny`/`cargo-audit` CI gates green
- [ ] **O-15:** `THREAT_MODEL.md` published at repo root; STRIDE × asset matrix complete; MITRE ATLAS + OWASP LLM Top 10 mappings present; residual risks honestly enumerated
- [ ] README, install script, 3 example skills, 2 example multi-agent workflows, quickstart docs, **covenant document**, **CompAG positioning doc**, **operational guide (failure recovery, profiles, updates)**
