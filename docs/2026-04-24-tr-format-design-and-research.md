# TR-1: Portable Knowledge-Graph File Format — Design & Research

- **Date:** 2026-04-24
- **Status:** Draft 0.1 (design locked pending implementation go-ahead)
- **Scope:** End-to-end design for a portable, signed, streamable knowledge-graph file format for ThinkingRoot
- **Working filename extension:** `.tr`
- **Positioning line:** *The PDF for AI Knowledge*

---

## 0. Executive Summary

ThinkingRoot compiles a user's sources into a local knowledge graph (claims, entities, edges, vectors, provenance certificates, compiled markdown artifacts) stored under `.thinkingroot/`. Today there is no portable, shareable unit of that compiled knowledge — sharing means attaching `.md` files or wiring up the REST API.

This document specifies **TR-1**, a single-file portable format (`.tr`) that:

1. Packages a full compiled workspace (graph + vectors + artifacts + provenance) into one file.
2. Is **content-addressed** (Merkle integrity) and **cryptographically signed** (Sigstore keyless).
3. Is simultaneously a valid **`.mcpb`** bundle, so it auto-mounts as an MCP server on Claude Desktop, Claude Code, Cursor, and VS Code.
4. Is distributable via **any channel** — WhatsApp/email attachment, OCI registry, IPFS, HTTP URL — with partial/streaming reads supported.
5. Is **embedding-model-portable** via `vec2vec` adapters, so a sender's vectors work in a receiver's differently-embedded agent.

This design is informed by three parallel research tracks executed 2026-04-24:
- **Market landscape** — 9 competitors audited; gap confirmed.
- **Technical frontier** — 7 production-grade 2025-2026 primitives identified and composed.
- **Codebase audit** — full ThinkingRoot architecture mapped; readiness gaps enumerated.

The central claim: **every building block is a shipping 2025-2026 standard.** TR-1 is a composition problem, not a research problem. No competitor ships this composition as of April 2026.

---

## 1. Thesis & Positioning

### 1.1 One-line thesis

> `.tr` is the first content-addressed, cryptographically signed knowledge-graph file that any AI agent mounts on double-click — distributable as an email attachment, an OCI registry pull, or an HTTP Range request — with no server, no vendor, and no embedding-model lock-in.

### 1.2 Why now (timing window)

- **OCI Artifacts** matured for non-container data in October 2025 (Docker blog endorsement for AI model packaging).
- **`.mcpb` Desktop Extension Bundles** — MCP's official portable-server format — were adopted into the MCP project in November 2025.
- **Sigstore `cosign sign-blob`** is production-ready for arbitrary non-OCI blobs.
- **vec2vec** (NeurIPS 2025) empirically validated cross-embedding-space translation with near-perfect fidelity.
- **NIST AI Agent Standards Initiative** launched February 2026 with no blessed knowledge-interchange format yet.
- **Anthropic "Import Memory"** shipped March 2026 (Claude-locked, one-way) — validates demand.
- **Zep open-source Community Edition** was deprecated in April 2025, creating a migration wave.

The substrate is roughly six months old. No one has fused it into a user-facing file format. The positioning slot NIST flagged is open.

### 1.3 What "revolutionary" means here (design bar)

TR-1 must beat the table on every row to justify the claim:

| Dimension | Bar | Why it matters |
|---|---|---|
| Portability | Single file, no runtime deps to *read* | WhatsApp/email reality |
| Size | <10 MB for 10K-claim workspace | Messaging-app attachment caps |
| Fidelity tiers | Preview (KB) / queryable (MB) / archival (GB) in one format | Cannot ship three formats |
| Agent consumption | Drop in folder → any MCP-aware agent uses it, zero config | The "plug it in" moment |
| Verifiability | Signed, tamper-evident, auditable | Trust without a server |
| Composability | Two files merge deterministically; diffs are small | Git-for-knowledge unlock |
| Streaming | Remote `.tr` over HTTP Range → load only needed pages | Works at Hub scale |
| Forward-compat | v1 reader opens v2 files (degraded), never crashes | Format survives 10+ years |
| Offline-first | Zero server required end-to-end | Core trust property |

---

## 2. Research Synthesis

### 2.1 Market landscape (competitive audit)

Nine systems audited for memory storage, export/share, lock-in, and user pain. Exact findings below.

#### 2.1.1 Competitor matrix

| Product | Storage | Export/Share today | Portable file? | Pricing pressure point |
|---|---|---|---|---|
| **Mem0** | Vector + graph (Neo4j/Neptune/FalkorDB) | "OpenMemory" Mem0↔Mem0 only (Sept 2025) | No | $19 → **$249/mo for graph memory** |
| **Zep** | Temporal KG (Graphiti) | None documented | No | Cloud-only; **Community Edition deprecated April 2025**, rest retired Feb 2026 |
| **Cognee** | Vector + graph + cognitive layer | Python SDK only | No | OSS + Cloud; requires backend plumbing |
| **Letta / MemGPT** | Postgres + memory blocks + archival | **`.af` Agent File (JSON, April 2025)** | Agent state only — no KG, no embeddings | HN Show: 5 points, 1 comment |
| **SuperMemory** | Vector DB + chunking | API dump | No format | **CC BY-NC-SA** license blocks commercial use |
| **LangGraph** | Checkpointers (SQLite/Postgres) | DB rows | No | Framework-bound |
| **LlamaIndex** | VectorStoreIndex | `storage_context.persist()` → folder of 4 JSONs | Folder, not file; embedder-locked | — |
| **Graphiti** | Neo4j/FalkorDB/Kuzu/Neptune | Neo4j `.dump` / `apoc.export.graphml` (lossy) | No | Requires user to bring graph DB |
| **Notion AI** | Proprietary block store | Markdown/CSV/HTML (documented lossy) | No | Cottage industry of bridges (chatgpt2notion, eesel, posttosource) |

**Closest prior art:** TrustGraph `Knowledge Core` (Apache 2.0, ~2K stars) bundles RDF + embeddings + provenance. Self-host only, no SaaS, no signing, no viral distribution. Validates concept, has not achieved distribution.

#### 2.1.2 The gap — confirmed

> **Is there any product today that lets you: compile knowledge → save to a single file → email/WhatsApp to a friend → the friend's AI agent uses it? No. Not one.**

Why no one has built it:
- Incumbents monetize lock-in (Zep killed OSS, Mem0 gates graph behind $249/mo).
- The "agent config" camp (Letta) and "knowledge graph" camp (TrustGraph/Graphiti) have not fused.
- The OCI-Artifacts + Sigstore + MCP substrate is ~6 months old.
- Anthropic's Import Memory (March 2026) validates demand but locks consumption to Claude.

#### 2.1.3 User pain signals (sourced)

- "AI memory is a lock-in mechanism" is now a mainstream narrative (Glasp, XTrace, Synthreo blogs, March 2026).
- Memory Forge (Phoenix Grove Systems) pitched as exit ramp for ChatGPT protesters.
- Plurality Network building cross-platform memory layer (SaaS, not file-based).
- OpenMemory (CaviraOSS) ships migration tool from Mem0 / Zep / Supermemory — proves portability demand.
- Mem0 GitHub Issue #2066 — canonical "priced out by graph tier" complaint.
- Zep Community Edition death — largest OSS memory-project abandonment to date.
- Notion export cottage industry — users actively seek to escape silos.

### 2.2 Technical frontier (the seven primitives to compose)

Each of the following is a production-grade spec or library with 2025-2026 adoption. Every claim below is cited in §10.

#### 2.2.1 Matryoshka Representation Learning (MRL)

- De-facto requirement by 2026. Native in OpenAI `text-embedding-3`, Nomic `nomic-embed-text-v1.5` (64→768), Jina v4 (128→2048), BGE-M3, Google Gemini Embedding 2 (March 2026, 3072-dim MRL, 5 modalities).
- Benchmark: `text-embedding-3-large` at **256 dims (8% size) outperforms full 1536-dim ada-002 on MTEB**.
- Truncation to 10% of original size typically retains ~98% retrieval performance.

#### 2.2.2 Binary & int8 quantization (BBQ)

Retrieval-quality retention (MTEB NDCG@10, HuggingFace benchmarks):

| Model | int8 | Binary (raw) | Binary + rescore×4 |
|---|---|---|---|
| mxbai-embed-large-v1 (1024d) | 97.0% | ~87% | **96.45%** |
| Cohere-embed-english-v3 (1024d) | **100%** | — | 94.6% |
| e5-base-v2 (768d) | 94.7% | 74.8% | — |

- Binary: **32× storage reduction, up to 45× query speedup.**
- Int8: 4× storage, ~3.7× speedup.
- **Quality scales with native model dimension** — small models (MiniLM-384) degrade more under binary.
- Elastic's *Optimized Scalar Quantization* is now default in Lucene (2025).
- **TurboQuant (ICLR 2026)** is data-oblivious, no codebook training, 3-bit with essentially zero loss; Qdrant integration underway.

#### 2.2.3 vec2vec (cross-embedding portability)

- NeurIPS 2025 paper, arXiv 2505.12540. Empirically validates the *Platonic Representation Hypothesis*.
- Translates between embedding spaces with **zero paired data**; cosine ≈ 0.92; top-1 accuracy up to 100%.
- Implication for TR-1: **no canonical embedding model required.** Store embeddings + a model-id header; readers translate on demand via vec2vec adapters.

#### 2.2.4 CAR v1 container + zstd-seekable compression

- **CAR v1** (IPLD Content-Addressable aRchives): length-prefixed IPLD blocks, CBOR header, each block prefixed by its CID. Canonical "tar-but-Merkle" container.
- **zstd seekable format**: splits compressed data into independent frames with a seek table stored in a Skippable Frame (ignored by vanilla zstd → 100% backward compatible). Enables O(1) random-access reads.
- Rust implementation: `rorosen/zeekstd`.

#### 2.2.5 HDT (Header-Dictionary-Triples)

- W3C submission. Binary RDF that **stays compressed in-memory and serves queries without decompression** — exactly the "queryable blob" primitive TR-1 needs.
- Implementations: `hdt-cpp`, `hdt-java`. ESWC 2024 paper addresses write-heavy updates on commodity hardware.

#### 2.2.6 Sigstore `cosign sign-blob` + Ed25519

- `cosign sign-blob <file> --bundle artifact.sigstore.json` writes a single JSON bundle containing signature + X.509 cert + Rekor transparency-log proof + timestamp.
- Keyless via OIDC is the modern default; KMS/HSM supported.
- Rust: `sigstore-rs`. Integration estimated at ~10 LOC.
- Ed25519 fallback: `ed25519-dalek` v2 (non-malleable by default, batch verification, SIMD backends).

#### 2.2.7 `.mcpb` bundles + MCP 2026 discovery

- MCP protocol (Tools, Resources, Prompts) adopted by OpenAI, Google DeepMind, Microsoft, Cloudflare; donated to Linux Foundation.
- **`.mcpb`** (Desktop Extension Bundles): zip archive with `manifest.json` + `server/` binary, double-click installs in Claude Desktop / Claude Code / MCP for Windows. Originally "DXT"; renamed and adopted into MCP project Nov 2025.
- `.well-known/mcp` discovery endpoint (2026 roadmap): servers advertise capabilities without a live connection.
- IETF draft `draft-serra-mcp-discovery-uri`: `mcp://host/server` universal address.
- VS Code `chat.mcp.discovery.enabled` auto-imports MCP config from Claude Desktop — cross-tool bootstrap is real.

#### 2.2.8 Distribution substrate (OCI + SLSA)

- **OCI Artifacts** production-ready Oct 2025 for non-container data. Hugging Face, Docker, MLflow converging. OMLMD standardizes ML-model OCI artifacts.
- **SLSA** mature by 2025 for ML: L1 documented build → L4 hermetic + reproducible. `in-toto Statement` with SLSA `Predicate` is canonical.
- Recommendation: ship TR-1 as OCI artifact media type `application/vnd.thinkingroot.tr+zstd`.

#### 2.2.9 OS-level drop-in (Quick Look / IPreviewHandler)

- macOS: Quick Look is now app extensions (standalone QLGenerator deprecated post-Catalina). Declare `QLSupportedContentTypes` with the UTI for `.tr`.
- Windows: `IPreviewHandler` COM interface registered by UTType/extension.

### 2.3 Codebase readiness (ThinkingRoot, as of 2026-04-24)

Deep audit of `/Users/naveen/Desktop/thinkingroot`. All citations are `file_path:line_number`.

#### 2.3.1 Crate map

| Crate | Purpose | Key entry points |
|---|---|---|
| `thinkingroot-core` | Types (Claim, Entity, Relation, Source, Artifact), IDs, config, WorkspaceRegistry | `Config::load()`, `WorkspaceRegistry::list()` |
| `thinkingroot-graph` | CozoDB wrapper (`GraphStore`) + `VectorStore` (fastembed, TRVEC1 format) | `GraphStore::init()` |
| `thinkingroot-parse` | File/dir parsing → documents with URI + content hash | `parse_directory()` |
| `thinkingroot-extract` | LLM-driven claim/entity extraction with fingerprint cache | Batch extraction |
| `thinkingroot-ground` | NLI tribunal for claim-confidence scoring | Score claims |
| `thinkingroot-link` | Entity resolution, relation building, contradiction detection | `link()` |
| `thinkingroot-compile` | Renders 7 global + per-entity markdown artifacts | `Compiler::compile_all()`, `compile_affected()` |
| `thinkingroot-reflect` | Structural-pattern discovery, known-unknowns, gap reports | `reflect_across_graphs()` |
| `thinkingroot-verify` | Health scoring (freshness, consistency, coverage, provenance) | `verify()` |
| `thinkingroot-rooting` | Phase 6.5 admission gate: 5 probes, BLAKE3 certificates | `Rooter::run()` |
| `thinkingroot-branch` | Knowledge Version Control: branch/diff/merge with CoW | `create_branch()`, `snapshot.rs:90-120` |
| `thinkingroot-serve` | MCP server (25 tools) + REST API | `crates/thinkingroot-serve/src/mcp/tools.rs` |
| `thinkingroot-cli` | Command-line interface | `main.rs:1-100` |
| `thinkingroot-safety` | — |
| `thinkingroot-bench` | Micro-benchmarks |

#### 2.3.2 Storage layer (authoritative)

- **`graph.db`** — SQLite-backed CozoDB; schema defined `crates/thinkingroot-graph/src/graph.rs:100-200`. Core relations: `sources`, `claims`, `entities`, `claim_source_edges`, `claim_entity_edges`, `entity_relations`, `contradictions`, `entity_aliases`, `events`, `turn_calendar`, `structural_patterns`.
- **`vectors.bin`** — custom **TRVEC1** binary format (`crates/thinkingroot-graph/src/vector.rs:186-222`).
  - Magic header: `TRVEC1\n` (7 bytes).
  - Per-entry (little-endian): `[u32 id_len][id][u32 meta_len][meta][u32 dims][f32 × dims]`.
  - Metadata: `claim|{id}|{ctype}|{conf}|{uri}` or `entity|{id}|{name}|{etype}`.
  - Model: **AllMiniLML6V2** hardcoded (`vector.rs:72`), 384-dim, cached at `~/.cache/thinkingroot/models/`.
- **`.thinkingroot/`** directory layout:

```
.thinkingroot/
├── graph/graph.db
├── vectors.bin
├── artifacts/           (entities/, architecture-map.md, contradiction-report.md,
│                         decision-log.md, task-pack.md, agent-brief.md, runbook.md,
│                         health-report.md — 7 globals + per-entity)
├── cache/               (LLM extraction cache — regenerable, not needed for export)
├── branches/{slug}/     (graph/graph.db, models/ symlink, cache/ symlink)
├── config.toml
└── fingerprints.json
```

- Branch metadata: `.thinkingroot-refs/branches.toml`.

#### 2.3.3 Provenance & rooting (existing system)

- Phase 6.5 admission gate (`crates/thinkingroot-rooting/src/lib.rs:1-66`), five probes: Provenance, Contradiction, Predicate, Topology, Temporal.
- **`Certificate`** struct (`src/certificate.rs:12-31`):
  - `hash` — BLAKE3 hex of canonical JSON
  - `claim_id`, `created_at`
  - `probe_inputs_json`, `probe_outputs_json`
  - `rooter_version`
  - `source_content_hash` — BLAKE3 of source at trial time
- **No cryptographic signing of claims today.** Certificates are deterministic hashes, re-verifiable only if source bytes are available.
- Admission tier stored in `claims.admission_tier` column: `rooted` | `attested` | `quarantined` | `rejected`.
- `contribute_gate` modes (`crates/thinkingroot-core/src/config.rs:42-99`): `advisory` (default) | `enforce` | `off`.

#### 2.3.4 MCP surface (25 tools)

From `crates/thinkingroot-serve/src/mcp/tools.rs:17-346`:

- CRUD: `search`, `query_claims`, `get_relations`
- Compilation: `compile`, `health_check`
- KVC/Branches: `create_branch`, `diff_branch`, `merge_branch`, `checkout_branch`, `list_branches`, `delete_branch`, `gc_branches`, `rollback_merge`
- Reflection: `reflect`, `gaps`, `reflect_across`, `dismiss_gap`
- Memory: `ask`, `brief`, `investigate`, `focus`
- Contribution: `contribute`
- Rooting: `query_rooted`, `rooting_report`

**No export/import/mount tool exists.**

#### 2.3.5 Existing Hub spec (60% already drafted)

`docs/2026-04-13-knowledge-hub-architecture.md:31-149` specifies a `KnowledgePack` proposal:

```
{publisher}/{name}@{version}/
├── manifest.json   (BLAKE3 integrity hashes, stats, health, compiler_version, etc.)
├── graph.db
├── vectors.bin
├── artifacts/
├── knowledge.card.md
└── provenance.json
```

Manifest fields include `schema_version`, `content_hash.{manifest, graph, vectors, artifacts}`, `publisher.verified`, `dependencies`, `tags`, `entity_types`, `claim_types`. **This is the starting point for TR-1 manifest.**

`docs/2026-04-10-phase4-cloud-cli-spec.md:1-313` describes `root login`, `root sync`, `root serve --federated`, `root connect github --webhook`. Not yet implemented.

#### 2.3.6 Landmines identified

1. **Embedding model hardcoded** (`vector.rs:72`) — manifest must record it; cross-model reads require vec2vec adapters.
2. **Rooting certificates require source bytes** for re-verification — without them, recipients trust the signer, not the math. Motivates tiered trust model (§4.4).
3. **LLM provider not captured in graph.db** — manifest must record it for recompile scenarios.
4. **Session store** (`crates/thinkingroot-serve/src/rest.rs:24`) is per-workspace; importing two `.tr`s into same workspace needs session namespacing.
5. **Branch registry conflicts** — `.tr` manifest must specify branch name; importer must handle name clashes.
6. **No absolute paths in graph.db** — confirmed safe for portability.

#### 2.3.7 Ready / Missing

**Ready to reuse immediately:**

- Core types (Claim/Entity/Relation/Source/Artifact) all `serde`-serializable
- CozoDB schema fully defined and migratable
- TRVEC1 vector format dimension-agnostic with magic header
- Branch snapshot pattern (`thinkingroot-branch/src/snapshot.rs:90-120`) = atomic `std::fs::copy` of `graph.db` — directly reusable for export
- Artifacts already compiled on disk
- Certificate serialization exists
- REST endpoints provide JSON surface
- Config is serde + TOML
- `admission_tier` column exists

**Must build for v0.1:**

- TR-1 spec (this document)
- `root export` / `root import` CLI commands
- `ExportManifest` struct in `thinkingroot-core`
- BLAKE3 integrity computation over archive members
- Tar+zstd (or CAR+zstd-seekable) container writer/reader
- Bulk serialization of graph tables (JSON or DAG-CBOR)
- WorkspaceRegistry integration for imported workspaces
- Landmine mitigations (§6)

---

## 3. TR-1 Format Specification (Draft)

### 3.1 On-disk layout

```
research.tr                               (CAR v1 body, zstd-seekable compressed, signed)
│
├── manifest.json                         top-level metadata, CIDs, signature refs
├── graph/
│   ├── claims.hdt                        HDT-compressed claim store (query without decompression)
│   ├── entities.dag-cbor                 IPLD DAG-CBOR
│   └── edges.dag-cbor                    IPLD DAG-CBOR
├── vectors/
│   ├── index.vec                         MRL-truncated (e.g. 256-dim) + BBQ 1-bit (+ optional int8 residual)
│   ├── model.json                        {model_id, native_dims, mrl_dims, quantization}
│   └── adapters/                         optional pre-computed vec2vec adapters
├── artifacts/                            8 compiled markdowns (existing ThinkingRoot output)
├── provenance/
│   ├── sources.json                      source URIs only — NEVER raw content
│   ├── certificates.json                 BLAKE3 rooting certs (existing system)
│   └── in-toto.json                      SLSA Provenance predicate
├── signatures/
│   └── cosign.bundle                     Sigstore signature + Fulcio cert + Rekor inclusion proof
└── .mcpb/                                dual-identity wrapper for drop-in mounting
    ├── manifest.json                     MCP server manifest (auto-registers tools)
    └── agent.md                          how-to-use-this-knowledge brief for LLMs
```

Container decisions:

- **Outer container**: CAR v1 with zstd-seekable compression. Random-access reads over HTTP Range; 100% backward-compatible with vanilla zstd decoders.
- **v0.1 simplification**: ship as **`tar.zst`** initially; migrate to CAR+zstd-seekable at v0.5 (streaming milestone). Manifest schema and directory layout remain identical across both.
- **Dual identity**: same file is a valid `.mcpb` — drag onto Claude Desktop to auto-install as MCP server.

### 3.2 Manifest schema (v0.1, JSON)

Extended from existing Hub draft (`docs/2026-04-13-knowledge-hub-architecture.md:69-137`). All fields canonical-JSON-serialized for hashing.

```json
{
  "format": "tr",
  "schema_version": "1.0.0",
  "name": "alice/research",
  "version": "1.0.0",
  "description": "Product research, April 2026",
  "license": "CC-BY-4.0",
  "created_at": "2026-04-24T12:00:00Z",

  "stats": {
    "claims": 10423,
    "entities": 842,
    "relations": 3107,
    "artifacts": 8,
    "vectors": 10423
  },

  "health": { "overall": 0.91, "freshness": 0.94, "consistency": 0.88, "coverage": 0.92, "provenance": 0.90 },

  "embedding": {
    "model_id": "AllMiniLML6V2",
    "native_dims": 384,
    "mrl_dims": 256,
    "quantization": "bbq-1bit-plus-int8-residual",
    "vec2vec_adapters": ["openai-text-embedding-3-small", "nomic-embed-text-v1.5"]
  },

  "compiler": {
    "compiler_version": "0.9.0",
    "rooter_version": "0.1.0",
    "extraction_provider": "amazon.nova-micro-v1:0"
  },

  "content_hash": {
    "algorithm": "blake3",
    "manifest": "...",
    "graph_claims": "...",
    "graph_entities": "...",
    "graph_edges": "...",
    "vectors": "...",
    "artifacts": "...",
    "provenance": "..."
  },

  "trust": {
    "tier": "T2",
    "signature_ref": "signatures/cosign.bundle",
    "signer_identity": "alice@example.com",
    "rekor_log_id": "...",
    "source_bytes_included": false
  },

  "branch": {
    "name": "main",
    "parent": null
  },

  "entity_types": { "System": 45, "Concept": 312, "Person": 19 },
  "claim_types":  { "Fact": 7802, "Decision": 1241, "Hypothesis": 480 },
  "tags": ["ai-memory", "product-research"]
}
```

### 3.3 Versioning & forward compatibility

- `schema_version` follows SemVer; MAJOR bumps only for breaking changes.
- Readers MUST accept unknown top-level fields (forward compat).
- Readers MUST verify `content_hash` before trusting any payload.
- v0.1 writers produce `schema_version: "1.0.0"`.

### 3.4 Trust tiers

| Tier | Signing | Verifiability | Default for |
|---|---|---|---|
| T0 | none | BLAKE3 file integrity only | quick personal share |
| T1 | Ed25519 self-key | "It's from Alice's key" | team / trusted group |
| **T2** | **Sigstore keyless (OIDC + Fulcio + Rekor)** | **Public transparency log** | **public distribution (default)** |
| T3 | T2 + in-toto SLSA Provenance | Non-falsifiable supply chain | enterprise / regulated |
| T4 | T3 + source bytes archive | Recipient can re-run admission gate | research / compliance |

T2 is the default because nobody in the AI-memory market offers anything above T0 today.

### 3.5 User experience

#### 3.5.1 Producing

```bash
root compile                              # existing
root export                               # → research.tr  (T2 signed, quantized, ~8 MB typical)
root export --tier 4 --include-sources    # fully rooted, largest
root export --quantize binary             # smallest (32 B/vector)
root export --format mcpb                 # emphasize drop-in MCP identity
```

#### 3.5.2 Sharing channels (one file, every channel)

| Channel | Command | Scale |
|---|---|---|
| WhatsApp / email / Airdrop | attach file directly | personal |
| Dropbox / Drive / S3 | drag-drop | team |
| OCI registry | `root push ghcr.io/alice/research:1.0.0` | public |
| IPFS / CAR mirrors | `root push ipfs://...` | decentralized |
| ThinkingRoot Hub | `root publish --visibility public` | discoverable |

#### 3.5.3 Consuming (four modes)

```bash
# Mode A — drop-in (the killer UX)
cp research.tr ~/.thinkingroot/mounted/
# File-watcher registers it as MCP resource; Claude Desktop, Code, Cursor, VS Code see it

# Mode B — double-click (macOS/Windows)
# Quick Look preview shows claim count, author, artifacts → "Install to Claude Desktop" button

# Mode C — import to own workspace
root import research.tr --merge-into main

# Mode D — query remotely without full download (Hub-scale)
root ask --from https://hub.thinkingroot.dev/alice/research.tr "what about pricing?"
# HTTP Range on zstd-seekable → fetches only needed pages (~50 KB typical round-trip)
```

#### 3.5.4 Universal AI-tool connectivity

Because `.tr` is simultaneously:

- a plain file (any messaging channel),
- an OCI artifact (`docker pull` equivalents),
- a valid `.mcpb` bundle (Claude Desktop / Code / Cursor / VS Code auto-mount),
- a localhost MCP server when running (`root serve --mount research.tr`),

it composes with every AI tool in 2026 through MCP, with no per-tool integration.

---

## 4. Defensibility Analysis

| Incumbent | Structural reason they cannot fast-follow |
|---|---|
| Mem0 | Graph memory is their monetization ($19 → $249/mo). Portable file destroys the moat. |
| Zep | Already killed OSS edition April 2025 to lock users in. Reversing is a reputational admission. |
| Letta | `.af` is agent-state-centric by design; adding KG + embeddings = new product, not a patch. |
| SuperMemory | CC BY-NC-SA license and unified-API model break if knowledge is a file users own. |
| OpenAI / Anthropic memory | Single-vendor by design. March 2026 Anthropic import pulls *into* Claude only. |
| TrustGraph | Closest prior art but 2K stars, no signing, no MCP, no `.mcpb`, no distribution story. |

The TR-1 composition is defensible because the dominant business model in AI memory (SaaS-graph-in-cloud) is structurally incompatible with "you own the file and can email it anywhere."

---

## 5. Size Projections (measured, not modeled)

Based on actual `du` of existing ThinkingRoot workspaces:

| Workspace | Claims | `graph.db` | `vectors.bin` | Total today | TR-1 T0 (unsigned, f32) | **TR-1 T2 (signed, MRL-256 BBQ)** |
|---|---:|---:|---:|---:|---:|---:|
| Small demo | ~1K | 0.8 MB | 0.4 MB | 1.2 MB | 0.6 MB | **~0.15 MB** |
| `thinkingroot` itself | ~30K | 33 MB | 18 MB | 51 MB | 25 MB | **~4 MB** |
| LongMemEval benchmark | ~700K | 443 MB | 228 MB | 878 MB | 485 MB | **~55 MB** |

Quantization math:
- Native vectors: 384-dim × f32 = 1,536 bytes/vector.
- MRL truncate to 256d = 1,024 bytes/vector raw.
- BBQ 1-bit on 256d = **32 bytes/vector** + optional 128 B int8 residual = ~160 B effective.
- ~90% size reduction; Cohere benchmark shows 94.6% retrieval retention at 1-bit with rescore×4.

Meaningful implication: a full personal/team knowledge graph fits in a WhatsApp attachment (typical cap 100 MB).

---

## 6. Implementation Milestones

| Milestone | Duration | Deliverables | Value unlocked |
|---|---|---|---|
| **v0.1 Bones** | **3 days** | `root export` / `root import`, BLAKE3 manifest, tar.zst container | Usable `.tr` files today |
| v0.2 Signed | 1 week | Sigstore cosign integration, `root verify`, Ed25519 fallback | T2 trust tier default |
| v0.3 Compressed | 1 week | MRL-256 truncation, BBQ 1-bit quantization, int8 residual | 8× smaller files |
| v0.4 Drop-in | 1 week | `.mcpb` wrapper, `~/.thinkingroot/mounted/` watcher, MCP auto-register | Claude Desktop double-click works |
| v0.5 Streaming | 2 weeks | Migrate container to CAR+zstd-seekable, HTTP Range `root ask --from URL` | Hub-scale remote queries |
| v0.6 Cross-model | 1 week | vec2vec adapter generation at export time | Works across embedding models |
| **v1.0 Polish** | 2 weeks | Quick Look (macOS), IPreviewHandler (Windows), OCI push/pull, NIST submission | Public launch, standards-track |

Total: ~7 weeks to v1.0. First shippable version in 3 days.

### 6.1 Why v0.1 ships in 3 days

Every v0.1 component already has its primitive in the repo:
- `thinkingroot-branch/src/snapshot.rs:90-120` — atomic `fs::copy` of `graph.db` (reusable).
- `thinkingroot-core` types serde-serializable.
- `docs/2026-04-13-knowledge-hub-architecture.md:69-137` — manifest schema 60% drafted.
- `ContentHash` BLAKE3 helper already exists in `thinkingroot-core`.

### 6.2 Why lock format contract at v0.1

The manifest shape, directory layout, CID strategy, and version bytes must be fixed at v0.1. Milestones v0.2–v1.0 are strictly additive (new fields, new tiers, new container migrations under the same version envelope). Shipping v1.0 in one go risks painting into a corner we cannot refactor out of.

---

## 7. Open Design Questions (to resolve during v0.1)

1. **Tar.zst vs CAR from day one** — tar.zst is simpler and unblocks v0.1 shipping; CAR gives Merkle integrity natively. Decision: tar.zst at v0.1, migrate to CAR+zstd-seekable at v0.5 without breaking manifest schema.
2. **`.tr` extension vs `.kroot`/`.tkg`** — recommended: `.tr` (short, matches brand, MIME `application/vnd.thinkingroot.tr+zstd`).
3. **HDT at v0.1 or later** — HDT is compressed-queryable RDF but adds a new dependency; v0.1 can ship raw `graph.db` + lazy conversion. Revisit at v0.3 when quantization lands.
4. **Default quantization tier** — proposed: `mrl-256 + bbq-1bit + int8-residual` (Cohere-proven 94.6% retention). Decision needs empirical test on AllMiniLML6V2, which is smaller; BBQ typically degrades more on small models.
5. **Source-bytes policy for T4** — include as separate `sources.tar.zst` inside the bundle? Or require a sidecar `.tr.sources` file? Sidecar keeps T0–T3 files size-stable.
6. **Branch export semantics** — export single branch vs whole workspace? Propose: `root export [--branch <name>]` with default = current.

---

## 8. Landmine Mitigations

| Landmine | Mitigation |
|---|---|
| Embedding model hardcoded | Record `embedding.model_id` in manifest; importer warns on mismatch; vec2vec adapters v0.6 |
| Rooting certs need source bytes | T4 tier bundles sources; T2/T3 recipients trust signer, not re-verification |
| LLM provider mismatch on recompile | Record in `compiler.extraction_provider`; importer warns |
| Session store conflicts | Namespace sessions by graph content hash |
| Branch name collisions | Manifest specifies branch; importer auto-suffixes `-imported` on clash |
| Absolute paths in graph.db | Confirmed absent (audit Part 7); safe |

---

## 9. Decisions to Lock Before Starting v0.1

1. **Name & positioning**: extension `.tr`, tagline *The PDF for AI Knowledge*, spec name **TR-1**, public positioning as "the format NIST's AI Agent Standards Initiative is asking for."
2. **Default trust tier**: **T2 Sigstore keyless** (nobody else ships above T0).
3. **Start with v0.1 today** (3 days): `root export` / `root import` over tar.zst + BLAKE3 manifest, reusing `thinkingroot-branch` snapshot primitives. Every revolutionary layer above plugs in additively.

---

## 10. Source Citations

### 10.1 Market & competitive landscape

- Mem0 — https://github.com/mem0ai/mem0
- Mem0 pricing — https://mem0.ai/pricing
- Mem0 state of AI agent memory 2026 — https://mem0.ai/blog/state-of-ai-agent-memory-2026
- Zep pricing — https://www.getzep.com/pricing/
- Zep GitHub — https://github.com/getzep/zep
- Zep LoCoMo benchmark dispute — https://github.com/getzep/zep-papers/issues/5
- Cognee — https://github.com/topoteretes/cognee
- Letta Agent File — https://github.com/letta-ai/agent-file
- Letta Agent File docs — https://docs.letta.com/guides/agents/agent-file/
- Agent File HN Show — https://news.ycombinator.com/item?id=43558617
- SuperMemory self-hosting — https://supermemory.ai/docs/deployment/self-hosting
- SuperMemory pricing — https://docs.supermemory.ai/essentials/pricing
- LangGraph persistence docs — https://docs.langchain.com/oss/python/langgraph/persistence
- LlamaIndex persist/load — https://docs.llamaindex.ai/en/stable/module_guides/storing/save_load/
- Graphiti — https://github.com/getzep/graphiti
- TrustGraph — https://github.com/trustgraph-ai/trustgraph
- Notion export — https://www.notion.com/help/export-your-content
- Neo4j APOC GraphML export — https://neo4j.com/docs/apoc/current/export/graphml/
- AI Memory Wars (Glasp 2026) — https://glasp.ai/articles/ai-memory-wars
- XTrace AI Vendor Lock-In — https://xtrace.ai/blog/ai-vendor-lock-in
- Memory Forge coverage — https://programminginsider.com/chatgpt-protesters-are-using-memory-forge-to-take-their-data-with-them/
- Plurality Network — https://plurality.network/
- MacRumors Anthropic memory import — https://www.macrumors.com/2026/03/02/anthropic-memory-import-tool/
- OpenMemory (CaviraOSS) — https://github.com/CaviraOSS/OpenMemory
- RDF vs Property Graph (TigerGraph) — https://www.tigergraph.com/blog/rdf-vs-property-graph-choosing-the-right-foundation-for-knowledge-graphs/

### 10.2 Technical frontier

- Matryoshka Representation Learning — Zilliz — https://zilliz.com/blog/matryoshka-representation-learning-method-behind-openai-text-embeddings
- What is MRL — MindStudio — https://www.mindstudio.ai/blog/what-is-matryoshka-representation-learning
- Binary and Scalar Embedding Quantization — HuggingFace — https://huggingface.co/blog/embedding-quantization
- Better Binary Quantization in Lucene/ES — Elastic — https://www.elastic.co/search-labs/blog/better-binary-quantization-lucene-elasticsearch
- Optimized Scalar Quantization — Elastic — https://www.elastic.co/search-labs/blog/scalar-quantization-optimization
- TurboQuant (ICLR 2026) — Qdrant issue — https://github.com/qdrant/qdrant/issues/8524
- Binary Quantization — Qdrant — https://qdrant.tech/articles/binary-quantization/
- sqlite-vec — https://github.com/asg017/sqlite-vec
- vec2vec paper — arXiv 2505.12540 — https://arxiv.org/abs/2505.12540
- vec2vec project page — https://vec2vec.github.io/
- Embedding Portability and Versioning — Mixpeek — https://mixpeek.com/guides/embedding-portability-versioning
- Apple Embedding Atlas — https://apple.github.io/embedding-atlas/
- IPLD 2025 in Review — IPFS Foundation — https://ipfsfoundation.org/ipld-2025-in-review/
- CARv1 Specification — https://ipld.io/specs/transport/car/carv1/
- Cosign sign-blob — Sigstore — https://docs.sigstore.dev/cosign/signing/signing_with_blobs/
- SLSA attestation model — https://slsa.dev/attestation-model
- SLSA for ML 2025 — https://debugg.ai/resources/slsa-for-ml-2025-signed-datasets-reproducible-training-attested-inference
- OMLMD — https://containers.github.io/omlmd/overview/
- Why OCI not Git — https://www.gorkem-ercan.com/p/from-dev-to-deploy-why-we-package
- ed25519-dalek — https://docs.rs/ed25519-dalek/
- 2026 MCP Roadmap — https://blog.modelcontextprotocol.io/posts/2026-mcp-roadmap/
- MCP architecture — https://www.getknit.dev/blog/mcp-architecture-deep-dive-tools-resources-and-prompts-explained
- MCP .well-known discovery — https://www.ekamoira.com/blog/mcp-server-discovery-implement-well-known-mcp-json-2026-guide
- IETF mcp URI scheme draft — https://datatracker.ietf.org/doc/draft-serra-mcp-discovery-uri/
- modelcontextprotocol/mcpb — https://github.com/modelcontextprotocol/mcpb
- Adopting .mcpb — MCP blog — https://blog.modelcontextprotocol.io/posts/2025-11-20-adopting-mcpb/
- Desktop Extensions — Anthropic Engineering — https://www.anthropic.com/engineering/desktop-extensions
- VS Code MCP docs — https://code.visualstudio.com/docs/copilot/customization/mcp-servers
- NIST AI Agent Standards Initiative — https://www.nist.gov/news-events/news/2026/02/announcing-ai-agent-standards-initiative-interoperable-and-secure
- Google A2A protocol — https://developers.googleblog.com/en/a2a-a-new-era-of-agent-interoperability/
- Progressive Image Rendering — Jake Archibald 2025 — https://jakearchibald.com/2025/present-and-future-of-progressive-image-rendering/
- Parquet Page Index — https://parquet.apache.org/docs/file-format/pageindex/
- zstd seekable format spec — https://github.com/facebook/zstd/blob/dev/contrib/seekable_format/zstd_seekable_compression_format.md
- zeekstd (Rust zstd seekable) — https://github.com/rorosen/zeekstd
- HDT RDF — https://www.rdfhdt.org/
- Generate/Update HDT on commodity hardware — ESWC 2024 — https://hal.science/hal-04769139v1
- Apple Quick Look docs — https://developer.apple.com/documentation/quicklook/
- Building Quick Look previews macOS — https://blog.smittytone.net/2019/11/07/create_previews_macos_catalina/
- Docker OCI Artifacts for AI Model Packaging — https://www.docker.com/blog/oci-artifacts-for-ai-model-packaging/
- CNCF OCI Artifacts AI use cases — https://www.cncf.io/blog/2025/08/27/how-oci-artifacts-will-drive-future-ai-use-cases/
- Connect Obsidian + Logseq — https://www.xda-developers.com/connect-obsidian-and-logseq-best-of-both/
- LogSeqToObsidian — https://github.com/NishantTharani/LogSeqToObsidian
- Best ChatGPT Chrome Extensions — https://tactiq.io/learn/best-chrome-extensions-for-chatgpt
- ChatGPT Retrieval Plugin — https://github.com/openai/chatgpt-retrieval-plugin

### 10.3 ThinkingRoot internal references

- `crates/thinkingroot-core/src/lib.rs:1-15` — core types
- `crates/thinkingroot-core/src/config.rs:42-99` — rooting config
- `crates/thinkingroot-graph/src/lib.rs:1-5` — graph store module
- `crates/thinkingroot-graph/src/graph.rs:100-200` — CozoDB schema
- `crates/thinkingroot-graph/src/vector.rs:46-48, 72, 186-222` — vector store, model, TRVEC1 format
- `crates/thinkingroot-rooting/src/lib.rs:1-66` — admission probes
- `crates/thinkingroot-rooting/src/certificate.rs:12-31` — certificate struct
- `crates/thinkingroot-branch/src/lib.rs:1-119` — branch module
- `crates/thinkingroot-branch/src/snapshot.rs:90-120` — atomic branch snapshot (reusable for export)
- `crates/thinkingroot-serve/src/mcp/tools.rs:17-346` — 25 MCP tools
- `crates/thinkingroot-serve/src/rest.rs:24, 107-150` — session store, REST endpoints
- `crates/thinkingroot-cli/src/main.rs:1-100` — CLI entry
- `docs/2026-04-13-knowledge-hub-architecture.md:31-149` — KnowledgePack draft (TR-1 starting point)
- `docs/2026-04-10-phase4-cloud-cli-spec.md:1-313` — Phase 4 cloud CLI spec

---

## 11. Change Log

- **2026-04-24** — Initial draft (this document). Format spec at v0.1 pending go-ahead.
