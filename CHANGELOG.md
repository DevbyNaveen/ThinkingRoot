# Changelog

All notable changes to ThinkingRoot are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).  
Versioning follows [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

---

## [0.2.0] — 2026-04-11

### Added

#### Phase 3 — Onboarding + Provider Expansion
- **11 LLM providers** — AWS Bedrock, OpenAI, Anthropic, Ollama, Groq, DeepSeek, Azure, Together, Mistral, Perplexity, custom OpenAI-compatible endpoints; switch with one config line
- **Global config hierarchy** — `~/.config/thinkingroot/config.toml` for user-wide defaults; workspace config in `.thinkingroot/config.toml` overrides per-project; `Config::load_merged` resolves both
- **`root setup`** — Interactive 5-step wizard: LLM provider selection, API key validation, workspace registration, MCP auto-wiring, first compile
- **`root connect`** — Auto-wires MCP server into Claude Desktop, Cursor, VS Code, Zed config files; `--tool` to target specific client; `--dry-run` to preview without writing; `--remove` to unwire
- **`root workspace`** — Registry subcommands: `add <path>` (auto-assigns port), `list`, `remove <name>`; `root serve` with no `--path` reads registry and mounts all registered workspaces
- **`root serve --install-service`** — Generates and installs OS-native autostart: `launchd` plist on macOS, systemd user unit on Linux, PowerShell `sc.exe` script on Windows
- **`WorkspaceRegistry`** — Global workspace registry at `~/.config/thinkingroot/workspaces.toml`; auto-increments port assignments starting at 3000

#### Phase 3.5 — Knowledge Version Control (KVC)
- **`thinkingroot-branch`** crate — Complete KVC engine: branch registry (`branch.rs`), semantic diff (`diff.rs`), merge engine (`merge.rs`), snapshot isolation (`snapshot.rs`), advisory lock (`lock.rs`)
- **`root branch <name>`** — Create an isolated knowledge branch (copies `graph.db`, symlinks `models/` and `cache/` from parent to avoid duplicating ~300 MB)
- **`root branch --list`** — List all active branches with current HEAD marker
- **`root branch --delete <name>`** — Soft-delete a branch (marks Abandoned; data dir kept)
- **`root branch --purge <name>`** — Hard-delete: marks Abandoned AND removes `.thinkingroot-{slug}/` data directory
- **`root branch --gc`** — Garbage-collect all abandoned branches; removes all their data directories in one pass
- **`root checkout <name>`** — Set active HEAD branch (writes `.thinkingroot-refs/HEAD`)
- **`root diff <branch>`** — Semantic Knowledge PR: shows new claims with entity context, new entities, new relations, auto-resolved contradictions with winner + delta, unresolved contradictions, health score before/after, merge-allowed gate with blocking reasons
- **`root merge <branch>`** — Merge branch into main; runs health CI gate; `--force` bypasses gate; `--propagate-deletions` applies claim deletions; `--rollback` restores main to its pre-merge state
- **`root status`** — Show current HEAD branch and all active branches
- **`root snapshot <name>`** — Create an immutable named snapshot of the current branch
- **`root serve --branch <name>`** — Serve a specific branch's knowledge graph instead of main
- **Semantic diff engine** — Three-layer contradiction detection: (1) BLAKE3 statement hash deduplication, (2) negation-pair keyword heuristic (10 patterns: "is/is not", "uses/does not use", etc.), (3) Jaccard token similarity second pass (flags claims with >60% overlap and shared entity context not caught by negation pairs)
- **Relation diffing** — `get_all_relations()` key-set diff by `(from_name, to_name, relation_type)` triple; new relations shown in `root diff` output
- **Relation merging** — `find_entity_id_by_name` + `link_entities` in `execute_merge`; new cross-branch entity relations are properly wired in main after merge
- **`DiffRelation` type** — Redesigned to carry `from_name`, `to_name`, `relation_type`, `strength` directly; usable for both terminal display and merge without secondary graph lookup
- **Pre-merge snapshot** — Before any mutation, `execute_merge` copies `graph.db` to `graph.db.pre-merge-{slug}-{timestamp}`; `root merge --rollback <branch>` finds the most recent backup and restores it
- **Advisory merge lock** — `fs2::FileExt::try_lock_exclusive` on `.thinkingroot-refs/merge.lock`; a concurrent `root merge` on the same workspace returns an immediate error instead of silently racing on `graph.db`
- **Cross-platform snapshot layout** — `create_branch_layout` uses Unix symlinks (`#[cfg(unix)]`) and a `copy_dir_all` recursive copy fallback (`#[cfg(windows)]`) for `models/` and `cache/`
- **Decision-type claims in Architecture Map** — `compile_architecture_map` now queries `graph.get_claims_by_type("Decision")` instead of returning an empty list
- **REST branch API** — Seven branch endpoints under `/api/v1/`:
  - `GET  /api/v1/branches` — list all active branches
  - `POST /api/v1/branches` — create a branch (`{ name, parent?, description? }`)
  - `GET  /api/v1/branches/{branch}/diff` — compute semantic diff against main
  - `POST /api/v1/branches/{branch}/merge` — merge into main (`{ force? }`)
  - `POST /api/v1/branches/{branch}/checkout` — set HEAD
  - `DELETE /api/v1/branches/{branch}` — soft-delete (abandon)
  - `GET  /api/v1/head` — get current HEAD branch name
- **MCP KVC tools** — `create_branch`, `diff_branch`, `merge_branch` exposed in MCP server (both SSE and stdio transports)
- **`mount_with_data_dir`** on `QueryEngine` — takes an explicit `data_dir` path; used by `root serve --branch` to mount branch-scoped data directories
- **`AppState::new_with_root`** — constructor variant that records `workspace_root` for branch REST handlers

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

[Unreleased]: https://github.com/thinkingroot/thinkingroot/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/thinkingroot/thinkingroot/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/thinkingroot/thinkingroot/releases/tag/v0.1.0
