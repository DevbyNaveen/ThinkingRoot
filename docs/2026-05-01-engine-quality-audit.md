# 2026-05-01 — engine quality audit

## What landed

A 16-finding audit landed on `main` across 12 atomic commits between
`28ca624` and `79ecf6c`. The pass covered three architectural surfaces:

- **secrets handling** — C1, M9
- **pipeline correctness + resilience** — C2, C3, C4, C5, C6, M1, M2, M5, M6
- **desktop UI flow** — H1–H7, M3, M4, M8, M10

Workspace tests grew from **929 → 956** (every finding ships with a
regression test). Desktop tests: 8/8. `BrainGraph.tsx` is now clean
under TypeScript strict mode. No features removed.

## TL;DR — what users notice

| Before | After |
|---|---|
| `config.toml` could persist API keys to disk in plaintext | `Config::save` strips them; permissions chmod 0600 on Unix |
| Compile silently dropped failed batches and reported "ok" | `failed_batches` + `failed_chunk_ranges` surface end-to-end as a yellow CLI warning + non-fatal desktop toast |
| A stale API key burned the full retry budget | `Error::is_permanent()` short-circuits in 1 attempt for HTTP 401/403/404 + missing config + unsupported file type |
| No way to stop a compile mid-run; killing the desktop lost everything | `CancellationToken` plumbed through `run_pipeline_with_options`; desktop Stop button wires it; partial state on disk is preserved |
| Killed compile re-ran every batch on restart | Per-batch JSONL checkpoint at `<data_dir>/checkpoints/in_flight.jsonl`; restart resumes |
| Compiling froze the desktop's Brain view | Compile runs in the sidecar; desktop is a read-only consumer |
| Brain view stayed stale after compile until workspace switch | Desktop drops `MountedMemory` after `cache_dirty` compile so the next read remounts fresh |
| Brain graph capped at 500 claims silently | `LIMIT` removed; the d3 graph sees every claim |
| Search input reinitialised the canvas on every keystroke | `searchQuery` / `hovered` / `isolated` now in refs; canvas-init effect deps shrink to `[nodes, links, neighborMap, size]` |
| Idle 1–3 % CPU in the Brain view forever | `simulation.on("tick", draw)` instead of manual `requestAnimationFrame` — d3 stops on `alpha < alphaMin` |
| Click-to-zoom threw `TypeError` (broken `d3-transition`) | Instant recentre — strict improvement over the broken path |
| Sidecar `shutdown()` was a no-op | Drops stdin → waits 2 s → SIGKILL → reaps |

## Findings, by ID

### Critical (C1–C6)

| ID | Finding | Fix | Commit |
|---|---|---|---|
| **C1** | Workspace `config.toml` persisted `api_key` plaintext on save | `Config::without_keys()` strips field on write + chmod 0600 on Unix | `28ca624` |
| **C2** | `claims.source_path` column always empty (never copied from sources table) | `insert_claim` / `insert_claims_batch` populate `source_path`; `find_source_uri_by_id` accessor + 4 regression tests | `0557ff0` |
| **C3** | 90 s hardcoded `reqwest` timeout overrode user's `request_timeout_secs` | Configurable `timeout_secs` in all four LLM provider constructors; outer timeout = `2× inner`, floored at 60 s | `766e2e9` |
| **C4** | Failed LLM batches silently dropped — compile reported "ok" on incomplete graph | `ExtractionOutput.failed_batches` + `failed_chunk_ranges`; CLI prints yellow warning, desktop renders toast, flows to `PipelineResult` | `9e6fd35` |
| **C5** | No way to cancel a running compile; killing the desktop lost extraction work | `tokio_util::sync::CancellationToken` through `run_pipeline_with_options`; checked at every phase boundary; surfaces `Error::Cancelled`; desktop `workspace_compile_stop` Tauri command | `5dc1c00`, `bd467bc` |
| **C6** | A killed compile with 80 % of batches done re-ran every batch on restart | `InFlightCheckpoint` JSONL log at `<data_dir>/checkpoints/in_flight.jsonl`; O_APPEND record per completed batch; `load_completed_batches()` on restart | `230ce9b` |

### High (H1–H7)

| ID | Finding | Fix | Commit |
|---|---|---|---|
| **H1** | BrainGraph `O(N×M×P)` triple-nested loop deriving per-entity best semantic type | Single regex pass: longest-first alternation regex + `matchAll` per claim + O(1) `TYPE_RANK` Map | `78474b5` |
| **H2** | Every search keystroke tore down and rebuilt the canvas pipeline | `searchQuery` / `hovered` / `isolated` in refs; render-effect deps shrunk to `[nodes, links, neighborMap, size]` | `78474b5` |
| **H3** | Hardcoded `LIMIT 500` in `brain_load` / `memory_list` silently truncated graph | `limit: None` — d3 graph needs every claim for correct counts and link weights | `78474b5` |
| **H4** | Sidecar `shutdown()` was a no-op (Child consumed by detached `wait()` task) | Child lives in `Arc<Mutex<Option<Child>>>`; shutdown drops stdin → waits 2 s → SIGKILL → reaps | `9046028` |
| **H5** | Desktop process owned `graph.db` for both reads and writes during compile; CozoDB internal lock froze Brain view | New `POST /api/v1/ws/{ws}/compile/stream` SSE route; desktop POSTs via `reqwest` + `eventsource-stream`; sidecar is single writer; in-process fallback if sidecar absent | `2bcf60d` |
| **H6** | Brain view stayed stale after compile until workspace switch (engine cache held pre-compile views) | Desktop drops `MountedMemory` when `PipelineResult.cache_dirty` is true; next read remounts fresh `QueryEngine` | `2bcf60d` |
| **H7** | Walker's `if let Ok(meta) = ... && meta.len() > limit` short-circuited `Err(meta)` to "include the file" — permission-denied stat could push oversized file into graph | Explicit `match` treats `Err` as skip | `9046028` |

### Medium (M1–M10)

| ID | Finding | Fix | Commit |
|---|---|---|---|
| **M1** | `serde_json::to_vec` errors during fingerprinting swallowed by `unwrap_or_default()` (empty fingerprint always looked "changed") | Errors propagate as `Error::Serialization` | `9d30a1a` |
| **M2** | `list_rooted_claims` errors `unwrap_or_default()`-swallowed → all rooted claims silently misclassified | Errors surface to desktop UI with workspace name | `9d30a1a` |
| **M3** | `unsafe { std::env::set_var("TR_ROOTING_DISABLED", "1") }` in CLI startup — race hazard | Replaced with explicit `PipelineOptions { no_rooting: bool, .. }` plumbing | `9d30a1a` |
| **M4** | Structural extraction always ran alongside LLM extraction, doubling cost | New `ExtractionConfig.structural_plus_llm: bool` toggle; off by default | `9d30a1a` |
| **M5** | Stale API key (HTTP 401) burned the full retry budget | `Error::is_permanent()` recognises 401/403/404 + missing config + unsupported file type; LLM retry loop short-circuits in 1 attempt | `9d30a1a` |
| **M6** | `parse_directory` was a sequential loop — slow on large workspaces | Parallelised via `rayon::par_iter`; 2 new regression tests | `9046028` |
| **M8** | Manual `requestAnimationFrame` ran forever at 60 Hz (1–3 % idle CPU) | `simulation.on("tick", draw)` + `on("end", draw)` — d3-force manages its own timer and stops on `alpha < alphaMin` | `78474b5` |
| **M9** | A stale `export X=…` in user's shell silently shadowed `credentials.toml` | Sidecar startup compares env vs `credentials.toml` and warns with the exact `unset` command | `9d30a1a` |
| **M10** | `source_path` schema invariant undocumented | Documented in graph crate's schema comments | `0557ff0` |

(M7 was reclassified as a duplicate of M2 during the audit and folded
into that fix.)

## Architectural shape changes that future PRs must respect

These are the load-bearing invariants the audit established. The
inline doc comments inside the relevant modules carry the same rules
in tighter form — this section is for human readers.

### 1. Compile runs in the sidecar, not in the desktop process

The desktop's `workspace_compile` Tauri command POSTs to
`http://127.0.0.1:<sidecar_port>/api/v1/ws/{ws}/compile/stream`, an
SSE endpoint in `thinkingroot-serve::rest::compile_stream`. The
desktop only falls back to in-process compile when
`agent_runtime_subprocess::spawn` couldn't resolve the bundled
`root` binary at startup (no bundle, no `$PATH`).

**Why:** pre-fix the desktop process opened `graph.db` for both
reads (Brain view, claim browser) and writes (the in-process
pipeline). CozoDB's internal lock serialised them, so the Brain
view froze for the entire compile.

**Don't:** re-introduce a default in-process compile path in the
desktop. The sidecar split is what makes the UI stay responsive.

### 2. Cancellation = client disconnect

The SSE compile handler binds a `CancellationToken::drop_guard()`
inside the `async_stream::stream!` body. When the desktop drops the
response (Stop button, modal close, network drop), the guard fires
the token and the pipeline exits at the next phase boundary with
`Error::Cancelled`.

**Why:** no separate "cancel by id" route required; the disconnect
*is* the cancel signal.

**Don't:** add a parallel cancel-by-id route. The desktop registers
its cancel token in `AppState.active_compile` for the Stop button
to find — that's the only state needed.

### 3. Per-batch checkpoints persist across runs

`thinkingroot-extract::checkpoint::InFlightCheckpoint` writes a
JSONL log at `<data_dir>/checkpoints/in_flight.jsonl`. Each
completed batch flushes a single record via `O_APPEND` (concurrent-
safe). On restart, `load_completed_batches()` reads the set and
the extractor skips chunks already done. Cleared by the pipeline
after Phase 7 succeeds.

**Don't:** add a parallel resume mechanism. This is the resume
contract.

### 4. `ProgressEvent` and `PipelineResult` are wire types

Both derive `Serialize` + `Deserialize` (tag = `kind`,
`rename_all = "snake_case"`). The SSE compile route JSON-encodes
them; the desktop deserialises into the same enum and runs the
same `map_progress` it's always used.

**Adding fields:** mark with `#[serde(default)]` on the consumer
side so a newer server can roll forward against an older client.

**Renaming variants:** wire-format break — coordinate with the
desktop release.

### 5. Desktop drops `MountedMemory` after `cache_dirty` compile

When `PipelineResult.cache_dirty` is true the desktop sets
`state.memory.lock().await = None` so the next `memory_list` /
`brain_load` remounts a fresh `QueryEngine`. Noop compiles (every
file fingerprint-identical) leave the cache intact — avoids
unnecessary remount cost.

**Don't:** re-introduce a stale-cache path that survives a
writing compile.

### 6. `PipelineOptions`, not env vars, gate compile behaviour

The legacy `TR_ROOTING_DISABLED=1` shortcut is gone (it sat behind
`unsafe { std::env::set_var(...) }` — race hazard with concurrent
thread reads). Use `PipelineOptions { no_rooting: true, .. }`
instead. The CLI's `--no-rooting` and the desktop's compile-stream
request body both plumb through to this struct.

**Adding new pipeline knobs:** fields on `PipelineOptions`. Never
new global state.

### 7. `Error::is_permanent()` short-circuits the LLM retry loop

HTTP 401/403/404, missing config, and unsupported file type fail
in 1 attempt instead of `max_retries`. Implementation in
`thinkingroot-core::error::Error::is_permanent`; consumed by the
retry loop in `thinkingroot-extract`.

**Don't:** add new retry budgets that bypass `is_permanent()`. A
stale API key costs quota that doesn't get refunded.

### 8. Brain UI: refs for transient state, d3 drives rendering

`searchQuery`, `hovered`, `isolated` live in refs in
`BrainGraph.tsx`. The canvas-init effect's deps are stable
(`[nodes, links, neighborMap, size]`). The render loop hooks into
`simulation.on("tick", draw)` + `on("end", draw)` — no manual
`requestAnimationFrame`.

**Don't:** put transient UI state back in the dep list (keystrokes
will tear down the canvas pipeline again). Don't add a manual rAF
loop (idle CPU goes back to 1–3 %).

## Verification

| Check | Result |
|---|---|
| `cargo test --workspace` | 956 passed, 0 failed (was 929 — added 27 regression tests) |
| `cargo test` (`apps/thinkingroot-desktop/src-tauri/`) | 8 passed, 0 failed |
| `cargo check --workspace` | clean |
| `cargo clippy` (changed crates) | no new warnings |
| `bun run tsc --noEmit` (UI) | `BrainGraph.tsx` clean; remaining 8 errors live in `BrainTable.tsx` / `IconRail.tsx` / `LiveAgentsPanel.tsx` — pre-existing, unrelated |

## Commits

In execution order (every commit ships its own test):

| Commit | Subject |
|---|---|
| `1dff398` | chore(stabilize): pre-Phase-1 groundwork — UI null-guard, install_tr stub, llm timeouts |
| `28ca624` | fix(core): strip API keys from workspace `config.toml` on save (C1) |
| `0557ff0` | fix(graph): populate `claims.source_path` from sources table on insert (C2) |
| `766e2e9` | fix(extract): wire `LlmConfig.request_timeout_secs` through every provider (C3) |
| `5dc1c00` | feat(serve, extract, core): cancellation token through the pipeline (C5 — part 1) |
| `9e6fd35` | feat(extract, serve, cli, desktop): surface `failed_batches` end-to-end (C4) |
| `230ce9b` | feat(extract, serve): in-flight per-batch checkpoint log (C6) |
| `bd467bc` | feat(desktop): `workspace_compile_stop` wires the C5 cancellation token (P3.4) |
| `9046028` | fix(parse, desktop): graceful sidecar shutdown + walker safety + rayon parse (P5) |
| `9d30a1a` | fix(core, extract, serve, cli, desktop): M-series cleanups (P7 — M1, M2, M3, M4, M5, M9, M10) |
| `2bcf60d` (rewritten as `111ab4d`) | feat(serve, desktop): route compile through sidecar SSE (P4 — H5, H6) |
| `78474b5` (rewritten as `79ecf6c`) | perf(desktop): BrainGraph perf rewrite + drop 500-claim cap (P6) |

## Files touched (summary)

**Engine (`crates/`):**

- `thinkingroot-core/src/error.rs` — `Cancelled` variant, `is_permanent()`, `is_rate_limited()`
- `thinkingroot-core/src/config.rs` — `without_keys()`, chmod 0600, `structural_plus_llm`
- `thinkingroot-graph/src/graph.rs` — `source_path` population, accessor methods
- `thinkingroot-extract/src/llm.rs` — configurable timeouts, permanent-error short-circuit
- `thinkingroot-extract/src/extractor.rs` — cancel + checkpoint hooks, `failed_batches`
- `thinkingroot-extract/src/checkpoint.rs` — **new module** (~280 lines)
- `thinkingroot-serve/src/pipeline.rs` — `PipelineOptions`, cancel boundaries, serde
- `thinkingroot-serve/src/rest.rs` — **`POST /api/v1/ws/{ws}/compile/stream`**
- `thinkingroot-parse/src/walker.rs` + `lib.rs` — error-handling fix + rayon

**CLI (`crates/thinkingroot-cli/`):**

- `main.rs`, `progress.rs`, `setup.rs`, `pipeline.rs` — Stop button surface, `no_rooting` plumbing, partial-failure rendering

**Desktop (`apps/thinkingroot-desktop/`):**

- `src-tauri/src/agent_runtime_subprocess.rs` — graceful sidecar shutdown
- `src-tauri/src/state.rs` — `active_compile`, `CompileHandle`
- `src-tauri/src/commands/workspaces.rs` — sidecar SSE driver, in-process fallback, Stop command, status command
- `src-tauri/src/commands/memory.rs` — 500-cap removed, error surfacing
- `src-tauri/src/lib.rs` — new commands registered
- `ui/src/components/brain/BrainGraph.tsx` — full perf rewrite
- `ui/src/lib/tauri.ts` — Stop / status bindings, `CompileProgress` extended
