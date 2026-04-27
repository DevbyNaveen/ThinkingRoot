# Changelog

All notable changes to ThinkingRoot are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).  
Versioning follows [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

The `.tr` distribution loop closes inside the OSS engine. Users no
longer need to round-trip through the cloud just to share a compiled
knowledge pack: any `.thinkingroot/` workspace can be packaged with
`root pack`, and any `.tr` — local file, direct URL, or registry
coordinate — can be installed with `root install`.

### Added — `tr-format` crate

- **New crate `tr-format` at `crates/tr-format/`** — reader, writer,
  manifest schema, BLAKE3 digest helper, and capability set for the
  TR-1 `.tr` portable knowledge pack format. A `.tr` is a `tar+zstd`
  archive of a fixed directory layout (`manifest.json`, `graph/`,
  `vectors/`, `artifacts/`, `provenance/`, `signatures/`, `.mcpb/`).
  The crate is read-only and write-only — it does **not** execute
  anything from a `.tr`; mount/execute is the responsibility of the
  engine itself.
- **Public re-exports**: `tr_format::{Manifest, TrustTier, Version,
  Error}` plus `tr_format::reader::{read_file, read_bytes,
  DEFAULT_SIZE_CAP}` and `tr_format::writer::PackBuilder`.
- **24 unit tests + 1 doctest** ship with the crate, including a
  long-paths-round-trip regression test exercising tar `LongLink`
  extension entries (real engine artifact filenames routinely
  exceed the 100-byte ustar limit).
- The crate previously lived in the `thinkingroot-cloud` monorepo
  under `LicenseRef-Proprietary`. Relocating it to OSS makes it
  MIT-licensed — appropriate for a wire-format spec that any
  third-party tool needs to implement.

### Added — `root pack`

- **`root pack [WORKSPACE]`** — packages a compiled workspace
  (`<WORKSPACE>/.thinkingroot/`) into a portable `.tr` file.
- Reads metadata from `<WORKSPACE>/Pack.toml`. CLI flags
  (`--name owner/slug`, `--version`, `--license`, `--description`)
  override individual fields; eager validate before walking so the
  user gets feedback before the slow IO.
- Identity-maps every file under `.thinkingroot/` to the same
  relative path inside the `.tr`. Skips three local-only top-level
  entries: `cache/` (recompute artefact, contains workstation
  paths), `config.toml` (workspace-local overrides, may carry
  provider keys), `fingerprints.json` (incremental-compile mtime
  ledger, meaningless on a different host).
- Symlinks are not followed — `.tr` is content-addressed, so
  including symlinks would make BLAKE3 depend on filesystem layout
  outside the workspace.
- Output defaults to `<WORKSPACE>/<owner>-<slug>-<version>.tr`.
- Smoke-tested on a real 3 703-file `.thinkingroot/` (8.3 MB packed).

### Added — `root install`

- **`root install <REFERENCE>`** — extracts a `.tr` to a target
  directory's `.thinkingroot/` so `root query` / `root serve` can
  mount it. The reference accepts three shapes through one entry
  point:
  - **Local path**: `./pack.tr`, `/abs/path.tr`.
  - **Direct URL**: `https://example.com/pack.tr`.
  - **Registry coordinate**: `owner/slug@version` (or `@latest`),
    resolved via the configured registry's discovery doc.
- Default install target: `~/.thinkingroot/packs/<owner>/<slug>/<version>/`
  (Cargo-style cache layout). Override with `--target <dir>`.
- Always verifies the manifest's canonical-bytes hash on read;
  tampered files are rejected before extraction.
- For registry installs, also cross-checks the BLAKE3 of the
  downloaded body against the registry's `x-tr-content-hash`
  response header before unpacking — defense-in-depth on top of
  the manifest check.

### Added — registry resolution chain

`root install` resolves the registry URL in priority order:

1. `--registry <url>` flag (per-invocation override).
2. `$TR_REGISTRY_URL` env var.
3. `~/.config/thinkingroot/registry.toml` key `default`.
4. Built-in: `https://thinkingroot.dev`.

The chain hits `<registry>/.well-known/tr-registry.json` for the
discovery doc, validates `format_version == "tr-registry/1"` and
`tr_format == "tr/1"` (refuses on mismatch — a format-skewed
registry must surface as a clear error, never silent corruption),
then templates the advertised download URL with
`{owner}/{slug}/{version}` and fetches the body.

### Added — security hardening

- **HTTPS-only for non-loopback hosts.** `http://example.com` is
  refused; `http://127.0.0.1`, `http://localhost`, `http://[::1]`
  are allowed for tests + on-host registries. Content-addressed
  bytes alone don't defend against a MITM substituting a
  different (validly-hashed) pack — TLS still does.
- **60s overall timeout, 10s connect timeout** on the HTTPS client.
- **Stable user-agent**: `thinkingroot/<crate-version>`.
- **Size cap** — pre-checks `Content-Length` against the
  registry's advertised `max_pack_bytes`; re-checks the actual
  body length on read.

### Tests

- `tr-format` crate: 24 unit + 1 doctest.
- `thinkingroot-cli::pack_cmd`: 20 tests, including
  - 7 `InstallRef::parse` cases (path / URL / `owner/slug@ver`
    disambiguation).
  - 3 insecure-HTTP guard cases.
  - 3 live in-process axum registry round-trips: happy path,
    hash-mismatch rejection, foreign `tr-registry/99` rejection.
  - 4 pack/install round-trip + override-priority cases.
- `cargo check --workspace` clean (pre-existing
  `thinkingroot-graph` dead-code warning untouched).

### Cross-repo co-ordination

The `thinkingroot-cloud` monorepo's registry service shipped the
matching server side: `GET /api/v1/packs/{owner}/{slug}/versions/{version}/download`
and `GET /.well-known/tr-registry.json`. See cloud commits
`362242e` (drop tr-format from cloud workspace), `1bda036`
(download-by-ref + discovery), and the cloud-side
`docs/2026-04-27-saas-status.md` for the full distribution roadmap.

---

## [0.1.0-rooting] — 2026-04-24

First publicly-tagged release of the Rooting admission gate. Engine code
is production-complete at 64 green tests (57 unit + 6 integration + 1
adversarial-corpus integration). Evidence and paper artifacts accompany
the release.

### Added — Predicate-strength scoring (B1, the paper-critical change)

- **Coverage-based `strength: f32`** on every `PredicateEvaluation`.
  - Regex and tree-sitter-rust AST: `1 - clamp(matched_bytes / source_bytes, 0, 1)`.
    A pattern like `.` that covers every byte drops to strength ≈ 0; a
    tight function signature drops to strength ≈ 1.
  - JSONPath: `min(1, 1/k)` where `k` is the match count, so a broad
    `$..*` collapses proportional to the number of nodes it walks.
- **Live threshold** `predicate_strength_threshold` (default `0.60`) in
  both `thinkingroot-rooting::RootingConfig` and
  `thinkingroot-core::config::RootingConfig`. Mirrored through
  pipeline, MCP contribute, and `root rooting re-run`.
- **Rooter tier function updated**: a claim whose predicate actively runs
  and passes but whose strength falls below the threshold is demoted
  from `Rooted` → `Attested`. Certificate still issued (Attested is an
  admitted tier); the Rooted badge is reserved for strongly-evidenced
  admissions.
- Eliminates the "98.6 % Rooted" artifact reported in pre-B1 runs where
  workspaces carried no predicates at all.

### Added — Adversarial corpus + honest tier report (B3, B4)

- **`tests/injection_corpus.rs`** — 400 synthetic adversarial claims
  across four attack classes:
  - Class A (fabricated source): 100 % Rejected via provenance.
  - Class B (contradictory): 100 % Rejected via contradiction probe.
  - Class C (bogus predicate): 100 % Quarantined.
  - Class D (gamed predicate): 100 % not-Rooted via B1 strength demotion.
- **`benchmarks/BENCHMARK_ROOTING_INJECTION.md`** — reproducible report
  written by the test when `TR_WRITE_INJECTION_REPORT=1`.
- **`benchmarks/ROOTING_TIER_HONEST_2026-04.md`** — full distribution on
  the 95 584-claim LongMemEval-500 workspace with the key disclosure
  that zero claims carry predicates, so the 98.73 % Rooted figure is
  temporal-default, not predicate-verified.
- **`benchmarks/macro/rooting_overhead_2026-04.md`** — divan bench at
  `N=100` → 24.22 ms median (242 µs per claim, well under 10 % overhead
  target).

### Added — Read-time ablation (B2) on LongMemEval-500

- `--rooting-mode {on,off,advisory}` flag on `root eval` wires the
  Rooting filter into retrieval at read time. When `mode=on`, the
  retriever excludes every claim whose admission tier is Rejected.
- `GraphStore::get_claim_ids_by_admission_tier(tier)` — new public
  API that loads the filter set deterministically.
- `AskRequest::excluded_claim_ids` — new field threaded through the
  intelligence synthesizer.
- `scripts/b2_ablation_run.sh` — two-run orchestrator that runs
  LongMemEval-500 with `--rooting-mode=off` and `--rooting-mode=on`
  against the identical workspace, captures both logs, and emits a
  headline + per-category markdown summary.
- Azure / OpenAI client gained
  `requires_max_completion_tokens()` detection so GPT-5.x and
  o-series reasoning models route through the newer
  `max_completion_tokens` field. Unblocked running the ablation on
  `gpt-5.4` v2026-03-05.

### Added — Full ablation + accuracy headline on `gpt-5.4`

- **93.0 % (465/500) on LongMemEval-500** with Azure `gpt-5.4`
  2026-03-05, pure OSS retrieval stack. Ties MemMachine for #3
  globally on the April 2026 leaderboard, trails only Chronos (95.6 %)
  and OMEGA (95.4 %). New canonical headline replacing the historical
  91.2 % figure from an Azure Cognitive Services endpoint that has
  since been decommissioned.
- Read-time ablation outcome:
  - `gpt-5.4`:    off 93.0 %, on 92.6 % (**multi-session +4 pp**, net −0.4)
  - `gpt-4.1-mini`: off 89.6 %, on 89.8 % (net +0.2)
  - Mode=on ran 9–41 s faster on both models (smaller retrieval set).
- **Interpretation**: Rooting is a write-gate, not a relevance filter.
  Its read-time effect is second-order and category-dependent; the
  primary validation is the injection corpus (B3, 100 %/class).

### Added — Paper update

- `compag-paper/compag.tex` now carries the falsifiable novelty claim
  verbatim in both the abstract and §1, reframes the probe battery as
  "2 fatal + 1 central + 2 advisory", adds §Evaluation subsections for
  **Adversarial Robustness** (B3 injection) and **Read-time ablation**
  (B2) with full per-category breakdown and honest interpretation,
  breaks out the old 98.6 % figure into a predicate-verified vs.
  temporal-default split, expands the prior-art comparison from 9 to
  20 systems, and attaches three appendices (reproducible search,
  adversarial-corpus harness, operational decomposition of the novelty
  claim). The headline accuracy in the abstract, §1, §6.1, §6.4, and
  the conclusion has been updated to 93.0 % with proper historical
  attribution of the 91.2 % figure.

### Migration guide — from `0.9.x` to `0.1.0-rooting`

1. **Back up the graph DB before upgrade.** Migration 3 adds columns
   to the `claims` relation and creates three new relations
   (`trial_verdicts`, `verification_certificates`, `derivation_edges`).
   `cp {data_dir}/graph/graph.db /tmp/graph.db.backup-$(date +%F)`.
2. **First-time run auto-migrates.** Opening any workspace with
   `GraphStore::init` detects the missing columns and probes-and-replaces
   the `claims` relation with defaults `admission_tier = 'attested'`,
   `predicate_json = ''`, `last_rooted_at = 0.0`. Existing claims are
   preserved verbatim; no data loss.
3. **First-time re-run admits everything at `Attested`** because no
   claim carries a predicate yet. Run `root rooting re-run --all` to
   promote claims that pass fatal + temporal probes to `Rooted`.
4. **New compiles emit predicates** via the LLM extraction prompt
   extension landed in this release. Their admission distribution will
   split into predicate-verified vs. temporal-only as per B1.
5. **Opt out** via workspace `[rooting] disabled = true`, env
   `TR_ROOTING_DISABLED=1`, or CLI `root compile --no-rooting`.

### Rollback

If Migration 3 corrupts a workspace (not observed in any of our
snapshot tests, but paranoia is cheap):

```bash
cp /tmp/graph.db.backup-YYYY-MM-DD \
   {workspace}/.thinkingroot/graph/graph.db
```

The engine is designed for forward-only migration; rolling back the code
to `0.9.x` with a post-migration graph.db also works because the added
columns are ignored by old readers, but this is only tested on small
fixtures.

---

## [Unreleased]

### Added — Phase 3.5 Rooting (admission gate for derived knowledge)

- **`thinkingroot-rooting` crate** — new OSS crate implementing the Rooting
  admission gate. Zero verified prior art: deterministic re-execution of a
  derived claim's predicate against the original source corpus as a gating
  criterion for admission. See `docs/2026-04-20-rooting-and-knowledge-hub-strategy.md`.
- **Five-probe battery** — Provenance (fatal, byte-range token overlap),
  Contradiction (fatal, Datalog vs. opposing high-confidence claims),
  Predicate (non-fatal, dispatches regex / tree-sitter-rust AST / JSONPath
  engines), Topology (non-fatal, entity co-occurrence for derived claims),
  Temporal (non-fatal, parent/child timestamp consistency).
- **Admission tiers** — `Rooted` (all probes passed, certificate issued),
  `Attested` (legacy tier, preserved for pre-Rooting claims), `Quarantined`
  (non-fatal probe failed, retained for review), `Rejected` (fatal probe
  failed, excluded from retrieval but kept for audit).
- **BLAKE3 certificates** — every admitted claim carries a re-verifiable
  cryptographic certificate covering probe inputs + outputs, stored in a new
  `verification_certificates` CozoDB relation.
- **`FileSystemSourceStore`** — durable content-addressed byte store at
  `{data_dir}/rooting/sources/{hash[0..2]}/{hash[2..4]}/{full_hash}.bin`
  with git-style fan-out sharding, atomic writes, and GC tied to source
  removal. Persists joined chunk text at compile time so probes can re-run
  months later.
- **Phase 6.5 pipeline integration** — `thinkingroot-serve::pipeline::run_pipeline`
  inserts Rooting between source-insertion (Phase 6) and Link (Phase 7).
  Rejected claims are removed from the extraction before Link sees them;
  Rooted/Quarantined survivors are stamped with their tier and last_rooted_at.
  Honors `config.rooting.disabled` + `TR_ROOTING_DISABLED=1` + `--no-rooting`.
- **Claim struct extended** — four new optional fields on
  `thinkingroot_core::Claim`: `admission_tier`, `derivation`, `predicate`,
  `last_rooted_at`. All `Option<T>` with `#[serde(default)]` so older `.claim`
  bundles deserialize cleanly.
- **Schema migration 3** — claims relation gains `admission_tier`,
  `derivation_parents`, `predicate_json`, `last_rooted_at` columns.
  Idempotent probe+replace pattern; existing claims auto-backfill to
  `attested`.
- **New CozoDB relations** — `trial_verdicts` (append-only audit log),
  `verification_certificates` (content-addressed certificates),
  `derivation_edges` (parent-child links for derived claims). Five new
  indexes for tier/time/claim lookups.
- **LLM predicate extraction** — extractor prompts (both `prompts.rs` and
  `focused_prompts.rs`) now declare an optional `predicate` field on each
  claim. Invalid regex patterns are validated + silently dropped at
  `convert_predicate`, so claims never fail extraction because of a
  malformed predicate.
- **MCP tools** — `query_rooted` (tier-filtered claim retrieval) and
  `rooting_report` (per-tier admission counts). `contribute` MCP tool now
  routes agent writes through Rooting in advisory mode (config:
  `[rooting] contribute_gate = "advisory" | "enforce" | "off"`).
- **CLI** — `root rooting report`, `root rooting verify <claim_id>`,
  `root rooting re-run [--all | --claim <id>]`. New `--no-rooting` flag on
  `root compile` skips Phase 6.5 without touching config.
- **Health Score integration** — `thinkingroot-verify` replaces the binary
  provenance check with a weighted Rooting survival rate
  (Rooted 1.0 / Attested 0.5 / Quarantined 0.25 / Rejected 0.0). Legacy
  pure-Attested packs keep the 1.0 score to preserve backward compatibility.
- **Benchmarks** — new Divan benchmark `rooting_overhead` at
  `crates/thinkingroot-bench/benches/macro/rooting_overhead.rs` measuring
  per-claim Rooting cost at 100 / 1K / 10K claim scales.

### Migration notes

- First workspace open after upgrading runs schema migration 3 automatically.
  Existing claims get `admission_tier = 'attested'`, preserving current
  retrieval semantics.
- Run `root rooting re-run --all` to promote Attested claims to Rooted by
  executing the probe battery against their source bytes. Safe to run on
  live workspaces; idempotent; re-generates verdicts + certificates.
- Opt out with `[rooting] disabled = true` in `.thinkingroot/config.toml`,
  or pass `--no-rooting` on a single compile, or set
  `TR_ROOTING_DISABLED=1` in the environment.
- `.claim` bundles written before this release deserialize cleanly;
  consumers receive `admission_tier = "attested"` by default.

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
