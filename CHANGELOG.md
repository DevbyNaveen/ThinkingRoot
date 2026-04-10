# Changelog

All notable changes to ThinkingRoot are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).  
Versioning follows [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

---

## [0.1.0] — 2026-04-10

### Added

#### Phase 1 — Core Engine
- **6-stage compilation pipeline:** Parse → Extract → Link → Compile → Verify → Serve
- **`thinkingroot-core`** — Type-safe domain model: Source, Claim, Entity, Relation, Contradiction, Artifact, Workspace with ULID-based IDs
- **`thinkingroot-parse`** — Parsers for Markdown, code (Rust/Python/TypeScript/JavaScript/Go via tree-sitter), PDFs, git commits
- **`thinkingroot-graph`** — CozoDB (Datalog, embedded SQLite) graph storage + fastembed AllMiniLML6V2 vector index
- **`thinkingroot-extract`** — LLM extraction of claims, entities, and relations; multi-provider: AWS Bedrock, OpenAI, Anthropic, Ollama, Groq, DeepSeek
- **`thinkingroot-link`** — Entity resolution (exact + fuzzy), alias merging, contradiction detection, temporal ordering
- **`thinkingroot-compile`** — Artifact generation: Entity Pages, Architecture Maps, Decision Logs, Runbooks, Task Packs, Contradiction Reports, Health Reports
- **`thinkingroot-verify`** — 7 verification checks: staleness, contradiction, orphan, confidence decay, poisoning, leakage, coverage; Knowledge Health Score
- **`thinkingroot-safety`** — Safety engine scaffold (trust levels, sensitivity labels)
- **`thinkingroot-cli`** — `root` binary with `compile`, `health`, `init`, `query`, `serve` commands
- **Incremental compilation** — BLAKE3 content hashing; only recompiles changed sources
- **`.thinkingroot/config.toml`** — Hierarchical config with `root init`

#### Phase 2 — Serve + SDK
- **`thinkingroot-serve`** — Axum REST API with multi-workspace support, bearer auth, JSON response envelope
- **MCP Server** — Model Context Protocol 2024-11-05 with SSE + stdio transports; tools: search, query_claims, get_relations, compile, health_check
- **Python SDK** (`thinkingroot-python`) — PyO3 native bindings + async HTTP client; `ThinkingRootError` exception type; optional workspace parameter
- **Entity alias persistence** — Aliases stored and queryable via graph API
- **Vector feature flag** — fastembed optional (`default = ["vector"]`); no-op stub when disabled for lightweight builds
- **`AppState::new()`** constructor — Clean initialization with `SseSessionMap`

### Architecture
- Rust edition 2024, rust-version 1.85
- Cargo workspace with `default-members` excluding `thinkingroot-python` (requires maturin)
- Feature resolution: no explicit `features = ["vector"]` in dep declarations
- MIT OR Apache-2.0 dual license

[Unreleased]: https://github.com/thinkingroot/thinkingroot/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/thinkingroot/thinkingroot/releases/tag/v0.1.0
