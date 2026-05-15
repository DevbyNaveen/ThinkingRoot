# SHIPPED — ThinkingRoot OSS Engine Ledger

> **Living document.** Append a new section every time a major track
> ships; bump the sequencing chain + test totals at the top.
> Last updated **2026-05-14** with three same-day tracks:
> Witness Mesh polish cleanup (Track 14 — ~5,200 LOC deleted, rooting
> crate removed, `thinkingroot-llm` added), Universal install +
> auto-update + login-agent (Track 15 — curl-one-liner pattern with
> `dev.thinkingroot` login agent and `tauri-plugin-updater` signed
> updates), and River v1.0 — live merge feedback + symmetric
> stream-branch creation (Track 16 — REST chat auto-creates
> `stream/{conversation_id}` branches at parity with MCP path,
> diamond merge glyph + 800ms pulse on SSE merged events).

---

## At a glance

| Track | Date | Status | Tests |
|---|---|---|---|
| **1. Compile Completeness Contract (CCC)** — 33 tables, 4 invariants, Phase 9 byte-coverage audit | 2026-05-02 | ✅ shipped, CI-gated | 12.1–12.5 fixture tests + Phase 9 audit |
| **2. Active Engram Protocol v2 (AEP / RARP)** — 4 MCP tools, 12 Datalog rules, 31 of 33 tables | 2026-05-02 | ✅ shipped | +50 |
| **3. Hybrid Retrieval** — vector × Datalog × BLAKE3, 11-component score fusion, 7-layer pipeline | 2026-05-03 | ✅ shipped | +64 |
| **4. Cortex Protocol** — singleton-engine discovery, `cortex.lock`, attach-or-spawn | 2026-05-03 | ✅ shipped | +40 |
| **5. tr-mount + Python/TS SDKs** — secondary-brain plug, `Brain` facade, REST AEP endpoints | 2026-05-03 | ✅ shipped | +22 SDK + 7 mount |
| **6. Branch system T0.6 + T0.7 + T2.6** — `Principal` enum, connector idempotent bulk contribute, per-branch PII redaction | 2026-05-03 | ✅ shipped | +25 |
| **7. Water-Flow Incremental Compile (T1–T12, complete)** — cascade completeness, Phase 9 orphan audit, Phase 7e re-validation, `resolution_deps`, transactional per-source rebuild, `IncrementalSummary` + per-phase timing, CLI/desktop summary surfaces, `root compile --watch`, source-granular re-extract, p95<1000ms benchmark gate | 2026-05-05 | ✅ shipped, CI-gated (p95 = 98ms) | +72 |
| **8. Production-readiness sweep** — vector-error promotion, registry-write race fix, mount-trust regime rename, T2.7 orphan-merge auto-recovery, desktop chat error-event path + 8-segment status bar + capsule terminology cleanup | 2026-05-06 | ✅ shipped | +29 |
| **9. Branch v1.0 — T0.4 + T1.2 + T1.3 + T1.7 + T2.1 + T2.2 + T2.3 + T2.5** — Knowledge Proposal lifecycle (REST + MCP), per-branch stats, audit log via `BranchEvent`, lineage DAG, APFS clonefile / Linux FICLONE for O(1) branch create, protected branches, branch TTL auto-abandon, tag create + REST | 2026-05-06 | ✅ shipped | +10 |
| **10. Branch v1.0 finish — T1.1 + T1.4 + T1.5 + T1.6 + T2.4 + T3.2 + T3.6 + T3.7** — vector-embedding contradiction pass, branch-as-pack export/import, dry-run + cancel-in-flight merge, live SSE branch events, bitemporal as-of queries, cross-branch reflect, claim-migration registry, branch templates | 2026-05-06 | ✅ shipped | +26 |
| **11. Witness Mesh v1.0 (scaffold + partial cutover)** — `Witness` content-addressed primitive, 56-rule catalog with build-time grammar pinning, mesh assembler with dedup + SAFETY cross-check + deterministic sort, 4 mechanical extractors (comment-claims, doctags, test-assertions, lsp), 2 CozoDB tables + 6 indexes + 9 query methods on `GraphStore` (incl. `walk_mesh_from`), 4 REST endpoints + 2 MCP tools + 4 engine methods, `root migrate --to-witness-mesh` end-to-end. **Cutover progress:** Phase 6.5 (Rooting trial) + Phase 6.6 (verification certs) + Phase 2b (4-judge grounding tribunal) deleted from pipeline; 5 of 7 grounding-judge files deleted (`grounder.rs`, `nli.rs`, `semantic.rs`, `span.rs`, `dedup.rs`); rooting CLI subcommand + rooting_overhead bench deleted; `tr-format` bumped to 1.0.0; Rooting advisory pass in engine.rs deleted; **`Extractor::new` now unconditionally falls back to `LlmClient::new_structural_only()` — the LLM is never consulted at compile time regardless of provider config.** **Remaining (file-level deletions):** delete 18 LLM extraction files in `thinkingroot-extract/src/` + reroute `extract_all_inner` body, delete `thinkingroot-rooting` crate body, switch ~30 reader sites from claims to witnesses, write `tr/3.2` packs (`witnesses.cbor` + `rule_catalog.toml`), cloud-side one-liner. | 2026-05-11 | 🟡 scaffold complete + cutover ~70% done; LLM functionally disabled | +113 / -57 |
| **12. Install + Runtime Smoothness (slices A–F + PATH-fallback hotfix)** — 6-slice refactor eliminating the silent-fallback failure cluster on install + daemon lifecycle. **A** install-manifest substrate (atomic JSON + BLAKE3 + `setup_complete_at`); **B** `root doctor` substrate with 12 commit-locked check IDs (replaces 969-line legacy `doctor_cmd.rs`); **C** pure `core::cortex::decide()` unifying CLI + desktop spawn-vs-attach (+ new `thinkingroot-cortex-async` crate + `cortex.lock` write-before-mount crash-safety via RAII `LockfileGuard`); **D** EngineGate loud-blocking panel + watchdog `engine_status_changed` Tauri event + `THINKINGROOT_FORCE_IN_PROCESS=1` dev escape hatch; **E** onboarding collapsed into EngineGate wizard variant (deleted 650-line `OnboardingWizard.tsx` + `onboarding_status` Tauri command + `onboardingDismissed` store flag); **F** self-heal — `restart_state.rs` (exp backoff 0/500ms/2s/5s, 4-in-60s cap, 3-crash-signal cap, 5-min auto-clear breaker), `recovery_log.rs` (JSONL 10 MiB rotated), wedged-daemon SIGTERM+grace+SIGKILL cleanup, 3 new doctor checks (`binary.cli.runnable`, `binary.cli.checksum`, `daemon.restart.exhausted`), Unix signal capture via `ExitStatusExt::signal()`, restart banner + circuit-breaker reset UI; **Hotfix** PATH fallback (env override → `$PATH`) in both `cortex_client::load_preferred_manifest_binary` and `cortex_bridge::load_preferred_or_extant_binary` so `cargo install`-style installs (which don't write a manifest) work without `Decision::RepairNeeded`. | 2026-05-13 | ✅ all 6 slices on `main` + hotfix shipped | +~4,400 / -2,325 across 63 commits |
| **13. Compile Resilience + AI-Operator Compile** — 13-bug rollup closing the "compile sometimes works / queues / fails / Stop doesn't" cluster + giving the chat agent first-class compile capability. **Unified entry:** `rest.rs::run_unified_compile` extracted from `compile_stream`'s 400-LOC inline body; owns workspace remount + vector-index rebuild + `LlmProbed`/`MountSucceeded`/`CompileFinished` actor dispatch + **`EngramManager::invalidate_workspace` on `cache_dirty`** (silently skipped by the streaming path before — every agent-driven compile could return AEP probes against GC'd claim ids). **AI fast-path:** `mcp/sse.rs::compile_request_fastpath` intercepts `tools/call name="compile"` and routes through the same helper (drops the dispatch's engine read guard first, since `run_unified_compile` write-locks for remount). Stdio MCP keeps the legacy `engine.compile()` arm for editor MCP clients. **Compile-scoped breaker:** `thinkingroot-core::restart_state` schema v1→v2 with `CompileAttempt` + `compile_breaker_until` (back-compat via `#[serde(default)]`). 3 failures in 5-min window trips for 10 min; manual user clicks honour the breaker, doctor surface is the supported reset path. `record_compile_success` purges that workspace's prior `Failed` history (consecutive-failure semantics). **Auto-retry-once:** desktop spawns one cancel-aware retry on `Failed` with `compile_backoff_for_attempt(1) = 1s`; single user-visible Done/Failed per click. Recovery log records `compile_failed`/`compile_retry_scheduled`/`compile_recovered`/`compile_breaker_tripped`. **Wire fixes:** dynamic workspace alias (`resolve_compile_target` → registered name or `"_"` placeholder, replaces hardcoded `"desktop"`); `SIDECAR_BOOT_MAX_ATTEMPTS` 120 → 20 (60 s → 10 s, stale NLI/fastembed justification deleted); `CompileStatus.running: bool` (was `active`, but TS binding expected `running` since day one → silent no-op for every pre-flight check); `CompileHandle.task: JoinHandle<()>` for genuine `abort()` on force-clear; cancel-aware sleeps everywhere; `SSE_STALL_WATCHDOG = 60 s`. **Chat UI bridge:** `ChatView.tsx::compileToolWorkspace` map synthesises `CompileProgress::Started`/`Done`/`Failed`/`Cancelled` from the agent's tool-call lifecycle so the Right-Rail progress bar reflects AI-driven compile (start → end only — granular phases need a new chat-event type, out of scope). **Right-Rail polish:** pre-flight `workspaceCompileStatus()` check + "Compile started" toast (no fictional queue). | 2026-05-14 | ✅ shipped; AI compile fully wired through unified post-compile flow | +~1,200 / -380 across 4 crates + 2 UI files |
| **14. Witness Mesh polish cleanup (six-phase post-cutover)** — Phase 1 moves `SourceByteStore` from `thinkingroot-rooting` → `thinkingroot-graph`; Phase 2 extracts chat-time LLM into new `thinkingroot-llm` crate (8 files, 19 consumer sites switched, 10 unused deps pruned from `thinkingroot-extract`); Phase 3 rewrites `extractor.rs` 1,378→842 LOC and reduces `Extractor` to one honest field (`min_confidence`); Phase 4 lands the read-side bridge at `GraphStore::get_all_claims_with_sources` so synthesizer / brain UI / REST `/claims` transparently fall back to witnesses; Phase 5 deferred (claims-table drop needs AEP+hybrid+engram retargeting, ~3-4 weeks); Phase 6 **deletes `thinkingroot-rooting` crate entirely** (4,345 LOC). Net ~5,200 LOC deleted + 1 new crate + 1 deleted. | 2026-05-14 | ✅ shipped (8 commits on `main`, local-only); Phase 5 deferred | +~1,500 / -~6,700 |
| **15. Universal install + auto-update + login-agent** — 6-slice curl-one-liner ship: Slice 1 `service.rs` (495 LOC) that **actually runs** `launchctl bootstrap` / `systemctl --user enable` / `schtasks /Create /SC ONLOGON` (replacing legacy "print & exit"); Slice 2 `install.sh` universal (`install_desktop_macos`, `install_desktop_linux`, `register_login_agent` + 3 skip env vars); Slice 3 new `install.ps1` 350-LOC mirror; Slice 4 `tauri-plugin-updater` wired with new signing keypair + GitHub Releases `latest.json` endpoint; Slice 5 `release.yml` 3-job pipeline (`build-cli` 5-target → `build-desktop` 4-platform → `release`); Slice 6 landing-page `InstallSection`. Cleanups: `productName` "ThinkingRoot Desktop" → "ThinkingRoot" (no spaces, kills URL-encoding hazard); legacy `scripts/install.{sh,ps1}` stubs deleted; `serve.rs::install_service` → 4-LOC shim. Zero recurring fees, no Apple Developer / Microsoft signing certs. | 2026-05-14 | ✅ shipped (local-only); live-tested `root service install/uninstall` end-to-end | +~1,800 / -~200 across CLI + desktop + landing + CI |
| **16. River v1.0 — live merge feedback + symmetric stream-branch creation** — Engine: REST chat `/api/v1/ws/{ws}/ask/stream` now calls `auto_create_session_branch` at parity with MCP — desktop chat contributions no longer land on `main` when `streams.auto_session_branch = true`. UI: persistent diamond `mergeGlyph` at the spine join for merged-tone history rows (4-pixel polygon; outlives the pulse so users can re-see merge points days later); transient 800ms pulse ring on SSE `merged` events with timer cleanup on workspace switch + unmount. | 2026-05-14 | ✅ shipped | small (1 engine function + ~60 lines TSX + ~12 lines CSS) |
| **17. `.rootignore` privacy primitive** — Walker honours `.rootignore` in addition to `.gitignore` via `ignore::WalkBuilder::add_custom_ignore_filename(".rootignore")`. Precedence (`.rootignore` > `.gitignore` > global git excludes) matches `.dockerignore` / `.npmignore` semantics. `root init` writes a sensible-default `.rootignore` (secrets / personal / heavy binaries / build artefacts) on fresh workspaces, non-fatal if write fails. Keeps credentials and personal files out of compiled cognition independently of git tracking — first user-facing gate for the `$10/mo` student/researcher pitch. | 2026-05-14 | ✅ shipped (local-only, pending commit) | 4 walker tests + 1 CLI const (~70 LOC total) |
| **18. V3Pack v3.2 reader expansion + Living Paper / Witness Mesh pack inclusion** — Adds `witnesses_cbor: Option<Vec<u8>>`, `rule_catalog_toml: Option<Vec<u8>>`, `paper_md: Option<Vec<u8>>` to `tr_format::V3Pack`. `read_v3_pack_with_cap` now detects `witnesses.cbor` / `rule_catalog.toml` / new `paper.md` tar entries; refuses half-pair states (witnesses without catalog or vice versa). `V3Pack::recompute_pack_hash` chains v3.2 members into the canonical BLAKE3 (`manifest \|\| NUL \|\| source.tar.zst \|\| NUL \|\| claims.jsonl [\|\| NUL \|\| witnesses.cbor \|\| NUL \|\| rule_catalog.toml] [\|\| NUL \|\| paper.md]`) — tampering surfaces as hash divergence. `V3PackBuilder::with_paper(impl Into<String>)` stages the Living Paper body; `prepare_canonical` + `emit_outer_tar` carry it through. `is_v32()` now fires on paper-only packs. `tr_render::RenderedPreview` gains `witness_count`, `paper_preview_md` (with YAML frontmatter stripped), `has_witness_mesh`. Desktop `InstallPreview` plumbs all three to the install sheet. | 2026-05-14 | ✅ shipped (local-only, pending commit) | +5 reader_v3 tests + 3 render_smoke tests (60 + 8 totals, was 55 + 5) |
| **19. Phase 5 Witness Mesh cutover — bridge polish (5.4)** — `GraphStore::init(path)` now attaches a `FileSystemSourceStore` (re-using the workspace's `{path}/rooting/sources/` layout) so the witness→claims bridge can materialise **lossless** statement text. New helper `GraphStore::materialize_statement(source_id, byte_start, byte_end) -> Result<Option<String>>` resolves the source's `content_hash`, reads the byte range via the attached byte store, decodes as UTF-8 (lossy fallback for binary witnesses). `get_all_claims_with_sources` calls it transparently: chat citations + Brain UI + REST `/claims` now see real source bytes instead of the synthesised `[witness_type] symbol @byte_start..byte_end` form. Honest fallback when bytes aren't materialised (test fixtures, post-GC) preserves a structurally-distinct `[…]` prefix so the fallback is never confused for a quote. **Also:** `PipelineResult.claims_count` derives partially from witnesses persisted by Phase 6.45 — the CLI summary + desktop progress + REST compile response now report non-zero for witness-only workspaces (was always 0 pre-fix). **Remaining Phase 5 work** (deferred to a focused session): retarget 31 `*claims{...}` Datalog joins in `graph.rs` + `hybrid_queries.rs` (needs schema work on structural tables for `witness_id` columns); migrate `engine.rs::contribute` + `merge.rs::merge_into_branch` writers to write witnesses; remove the dormant claims-writer path in `linker.rs`. | 2026-05-14 | 🟡 partial — 5.4 bridge polish + `claims_count` derivation shipped; 5.1–5.3 deferred | +5 materialize_statement integration tests + ~120 LOC bridge polish |
| **20. Living Paper v1 — deterministic substrate** — New `thinkingroot-paper` crate (`crates/thinkingroot-paper/`) producing a per-compile `paper.md` artefact: YAML frontmatter spine (machine-readable: paper_version, workspace, compiled_at, witness/source/branch counts, rule_catalog_blake3, section index with per-section BLAKE3 input hashes) + markdown body with 5 deterministic sections (`at-a-glance`, `architecture`, `promises-it-keeps`, `how-it-is-tested`, `provenance`). Architecture section emits a Mermaid `graph LR` block with deterministic node + edge ordering — same witness vec across runs → byte-identical Mermaid source. Wired into the pipeline as **Phase 10b** (after the existing Phase 10 README synthesis): runs only on `main` / un-branched compiles, non-fatal on failure (a stale paper never aborts a compile). Atomic file write via `.tmp` + rename. **Pack integration:** `root pack` reads `.thinkingroot/paper.md` (if present) and attaches via `V3PackBuilder::with_paper` — the bytes chain into the canonical pack hash so tampering surfaces as a verifier divergence. AI narrative sections (Abstract, Key Ideas, How it fits together, Recent changes, How to use it) scaffold in `SectionId` but render as placeholder in v1; the LLM-cited layer ships in v1.1. | 2026-05-15 | ✅ shipped (substrate + Phase 10b + pack export); v1.1 AI narrative pending | +19 paper unit tests (frontmatter / sections / mermaid / synthesizer) + 1 new crate + Phase 10b in pipeline + paper-staging block in pack_cmd |
| **21. Image + Audio Witness Mesh extraction (catalog v1.1 / v1.2)** — Two new mechanical extractors under `thinkingroot-extract`: `image_rules.rs` (perceptual phash via inline 20-LOC mean-hash, color histogram, Sobel edge summary, EXIF via kamadak-exif, dominant-colors online quantisation) and `audio_rules.rs` (symphonia mono downmix + rustfft 2048-window Hann-FFT spectral fingerprint, duration metadata, honest-skipped on decode failure). Each rule emits whole-file-anchored witnesses (`spans[0] = (file_blake3, 0, len)`) — image content has no internal byte-range structure to chunk by. Per-file byte budgets: 32 MiB image, 256 MiB audio — oversized files emit a `{image,audio}::skipped@v1` witness with the reason in `symbol` instead of stalling compile. Parser side: new `image_meta.rs` + `audio_meta.rs` chunkless DocumentIR emitters; `parse_file` dispatches on extension. `CATALOG_VERSION = "1.2.0"`. | 2026-05-15 | ✅ shipped (commits `41d744e`, `ccbef6c`, `b5e91c0`) | +12 image rule unit tests + 10 audio rule unit tests |
| **22. Playground UI v1 — researcher surface (drop-zone → action toolbar → citation chips)** — Full Playground surface on the desktop icon rail (`FlaskConical` between Conversations and Knowledge). Composes: **DropZone** (Tauri drag-drop with `tr-file-opened` / `playground-files-dropped` event split, auto-compile after copy to `<ws>/inbox/`); **SourceLibrary** (left rail w=64, grouped by Text / Image / Audio / Other, per-source witness count badges via `GET /witnesses/by-source`); **PlaygroundView** Paper/Chat tab switcher embedding the existing `ChatView` (no fork); **SourceDetailPanel** (right slide-over w=80, witnesses grouped by `witness_type` with rule / confidence / byte range); **PaperPanel** renders `paper.md` via ReactMarkdown + remark-gfm with a **Regenerate** button calling the new `POST /api/v1/ws/{ws}/paper/regenerate` REST endpoint + `engine::regenerate_paper` (no full recompile); **PlaygroundActions** toolbar surfacing 7 inline-drawer verbs: save-note (writes `<ws>/notes/<slug>-<date>.md` with YAML frontmatter, atomic), open-proposal (POST `/branches/{br}/proposals`), branch-conversation (POST `/branches`), quiz (brain.investigate + JSON-array extraction with bounded `[`/`]` scanner), find-gaps (new `GET /api/v1/ws/{ws}/gaps` REST endpoint wrapping `list_gaps_branched`), export-tr (delegates to existing `pack_export` with `~/Downloads/<ws>.tr` default destination), handoff-url (`tr+mcp://` deep-link + paste-ready `mcp.json` snippet for Claude Code / Cursor / Codex). **Citation chips** (`CitationChip.tsx`): recursive children transformer scans React nodes coming out of ReactMarkdown's `p` / `li` / `td` renderers and replaces `[[witness:<id>]]` markers with clickable chips that open a popover. Wired into both PaperPanel + ChatView AI replies. No-marker text bypasses the splitter so prose layout is unchanged for un-cited replies. | 2026-05-15 | ✅ shipped (commits `25fde06`–`90c30da`, `8ab0e3a`, `21c7173`) | +~1,800 LOC across 7 React components + 8 new Tauri commands + 2 new REST endpoints + 1 new engine method |
| **23. Living Paper v1.1 — AI narrative + citation validation + cache** — Wires `LlmClient` to produce the 5 AI-narrative sections (`Abstract`, `KeyIdeas`, `HowItFitsTogether`, `RecentChanges`, `HowToUseIt`) on top of the v1 deterministic skeleton. Strict citation contract: every claim must carry a `[[witness:<id>]]` marker; `validate_citations` strips markers whose ids don't resolve to a real workspace witness; sections that lose ALL markers fall back to an honest stub ("couldn't ground a narrative for this section"); `RecentChanges` is exempt — an empty recent-changes section is honest, not a failure. **Cache:** per-section BLAKE3(prompt \|\| witness_digest) cache key persisted to `<ws>/.thinkingroot/paper-cache.json`; hits short-circuit the LLM call entirely. Cache + `paper.md` write atomically via tempfile+rename. Pipeline Phase 10b auto-selects: `LlmClient::new(config.llm)` → if Some, drives `synthesize_and_persist_with_llm`; if None (no provider / bad key), falls back to deterministic synthesis. `engine::regenerate_paper` picks the same branch using `handle.llm`. | 2026-05-15 | ✅ shipped (commit `f0f83a7`) | +11 narrate-module unit tests covering citation validation, cache round-trip, cache-key determinism, stub fallbacks (paper 19 → 30 total) |
| **24. Video extraction — MP4 demux, keyframes, scene-change (catalog v1.3)** — Third multimodal family: pure-Rust ISOBMFF demux via the `mp4` crate. Demux-only by design — per-keyframe perceptual hashing requires a pixel decoder (transitively pulls in heavy C/C++ deps), so v1 ships byte-anchored container metadata instead. **5 rules:** `video::duration@v1` (movie duration + track summary from mvhd), `video::keyframe@v1` (one witness per I-frame, byte-anchored when sample size known), `video::keyframe-overflow@v1` (capped-truncation summary, > 2000 keyframes/file), `video::scene-change@v1` (consecutive keyframes whose gap exceeds 5.0s → inferred scene cut, actual gap recorded in `symbol`), `video::skipped@v1` (WebM/MKV/AVI + parse-failure honest absence). **Wire-through:** `parse_file` dispatches mp4/mov/m4v/3gp/3gpp/webm/mkv/avi/flv/wmv/ogv to new `video_meta.rs`; `Extractor::collect_witnesses_from_documents` adds `is_video_document` branch; `.mp4` removed from `AUDIO_EXTENSIONS` so it routes to video (M4A remains the audio-only ISOBMFF extension). `CATALOG_VERSION = "1.3.0"`. 1 GiB byte budget per file. Per-keyframe `content_blake3` over the exact sample byte slice when in range, whole-file hash as fallback — CCC I-4 honesty. | 2026-05-15 | ✅ shipped (commit `dbd6462`) | +10 video_rules unit tests |
| **25. Phase 5.1a — bridge inverts to witness-primary** — `GraphStore::get_all_claims_with_sources` now reads from the Witness Mesh FIRST and falls back to the legacy `claims` table only when the workspace has no witnesses yet (pre-migration / hand-rolled test fixtures). Before this flip, every fresh workspace with both tables populated (post-2026-05-11) read from `claims` and never consulted witnesses — downstream consumers (rebuild_vector_index, compile/compiler.rs `claim_count`, graph_cache.rs hydration) were effectively claims-only despite the witness substrate being the authoritative one. **What's not in this commit:** the 31+ direct `*claims{...}` Datalog joins in `graph.rs` / `hybrid_queries.rs` / `aep_queries.rs` remain unchanged — those land incrementally through Phase 5.1b/c with full per-site test coverage; rewriting them in bulk is a silent-corruption hazard. Pipeline still dual-writes both tables (Phase 5.3 stops the claims write — gated on every direct-join site being cutover). | 2026-05-15 | 🟡 partial — bridge flipped; 31+ direct Datalog sites remain | 1,489 lib tests passing, zero regressions |
| **26. SOTA accuracy levers 1 + 2 + 3 — cross-encoder reranker, typed-edge graph, Observer/Reflector** — Three independent levers that together project a measurable lift over the 91.2% LongMemEval baseline towards the SOTA 95.4% (OMEGA) / 94.87% (Mastra) tier. **Lever 1 (reranker):** new `thinkingroot-graph::rerank` module wrapping fastembed v5's `TextRerank` with `JinaRerankerV1TurboEn` (137M params, ~280MB, ~120-200ms top-20 CPU). Lazy-loaded, shared OS cache. Wired as **Layer 6.5** of `intelligence/hybrid.rs::hybrid_retrieve` after the deterministic 11-component `fuse_score`; blended score = `weight * normalised_ce + (1 - weight) * fused`, default weight 0.7 (OMEGA's published coefficient). Opt-in via `ScoringProfile::use_cross_encoder = true` — default OFF because rerank exceeds the `<25ms p95` instant-retrieval budget. New `ScoringProfile::deep_mode()` preset + `compliance()` auto-enables. `ScoreBreakdown.cross_encoder: Option<f32>` exposes per-hit normalised score. **Lever 2 (typed-edge graph):** new Cozo table `witness_typed_edges (from_witness_id, to_witness_id, edge_type)` with three indexes + 5 graph helpers on `GraphStore` (`insert_witness_typed_edge[s_batch]`, `count_witness_typed_edges`, `list_witness_supersedes`, `list_witness_contradictions`, `walk_witness_typed_edges` with `mid != $start` cycle guard mirroring `Q_SUPERSESSION_CHAIN`). Edge alphabet locked to `Supersedes` / `Contradicts` / `Related` / `TemporalNext` — unknown edge types are dropped loudly at the insert site, never silently. Catalog 1.4.0 adds the 4 edge rules; `derive_all_typed_edges` mechanical extractor wired into pipeline Phase 6.45 after the witness persist (Related-from-shared-heading-text + TemporalNext-from-git-commit-chains are active; Supersedes + Contradicts deferred — need heading_path + quantity-value extraction wired through to the Witness payload). Also: `GraphStore::materialize_statement_verified` enforces CCC I-4 BLAKE3 re-anchor on probe-result return paths. **Lever 3 (Observer/Reflector):** new `intelligence/observer.rs` module — per-session in-memory buffer with mechanical condensation (no LLM) every 10 turns into a `StagedObservation`. Reflector materialises a `conversation::reflection@v1` Witness over ≥ 2 observations with `WitnessInput::WitnessRef` provenance chain. Catalog 1.5.0 adds `conversation::observation@v1` (conf 0.97) + `conversation::reflection@v1` (conf 0.95). `QueryEngine.observer() -> Arc<Observer>` shared handle; `QueryEngine.flush_observations(ws, session_id) -> Result<usize>` drains staged + (optionally) reflection to the witness substrate via `insert_witnesses_batch`; failed flushes re-stage (honest-incomplete, never silent-lossy). **MCP surface:** two new tools `observe_turn { session_id, turn_number, user_prompt?, assistant_reply? }` and `flush_observations { workspace, session_id, force_condense? }` let any MCP client (Claude Code, Cursor, Desktop chat) adopt the Mastra observational-memory pattern on the witness substrate without the engine prescribing a specific chat-path hook location. **Substrate sequencing:** all three levers share the same witness-substrate primitives — reranker reads `EnrichedCandidate.statement` (post Phase 5.1a bridge → real source bytes), typed edges sit on top of the existing `witnesses` table, observations are themselves witnesses. **What's not in this commit:** the Supersedes + Contradicts emitters (catalog rules registered, derivation deferred to a follow-up that wires heading_path / quantity-value through the Witness payload); chat-path auto-recording (Observer's hooks are opt-in via the new MCP tools — auto-recording would require per-path UX alignment on respond.rs / react.rs / agent_streaming.rs). | 2026-05-15 | ✅ shipped (substrate + emitters + MCP surface across 6 commits `d4df1fe`..`c532b26`); Supersedes/Contradicts emitters + chat-path auto-recording deferred | +33 unit tests across rerank / typed_edges / observer / MCP-tool-listing; whole workspace `cargo check` clean |

### Test scoreboard (2026-05-14 — post-Track-16)

| Surface | Count | Source |
|---|---|---|
| Cargo workspace | **1,425** (Track 14 dropped the `thinkingroot-rooting` crate's own ~48 unit tests with the crate itself; zero new regressions; Tracks 15 + 16 added 2 service tests + small UI bridge — no new lib-test rows) | `cargo test --workspace --lib --no-fail-fast` |
| `thinkingroot-core` lib | **209** | `cargo test -p thinkingroot-core --lib` |
| `thinkingroot-serve` lib | **467** | `cargo test -p thinkingroot-serve --lib` |
| `thinkingroot-desktop-app` lib | **27** | `cargo test --manifest-path apps/thinkingroot-desktop/src-tauri/Cargo.toml --lib` |
| `thinkingroot-cli` `service::` | **2** (Track 15) | `cargo test -p thinkingroot-cli --bin root service::` |
| TypeScript Vitest | **13** | `cd sdks/typescript && npm test` |
| Python pytest | **17** (+1 skipped on macOS XDG) | `cd thinkingroot-python && pytest` |
| Desktop UI tsc | **clean** | `cd apps/thinkingroot-desktop/ui && pnpm tsc --noEmit` |
| Landing page build | **clean** (Vite 8.0.12, 209 KB JS gzipped 65 KB) | `cd apps/thinkingroot-landing && pnpm build` |
| Benchmark gate | **PASS** (p95=98ms / gate=1000ms) | `cargo bench -p thinkingroot-serve --bench incremental_smoke` |
| **Grand total** | **~1,455** | 0 failures, 0 regressions across 16 tracks |

### Sequencing chain

```
✅ 1. Compile Completeness Contract       (2026-05-02, foundation)
✅ 2. Active Engram Protocol v2 / RARP    (2026-05-02, gated on CCC)
✅ 3. Hybrid Retrieval                    (2026-05-03, gated on AEP)
✅ 4. Cortex Protocol                     (2026-05-03, gated SDK)
✅ 5. tr-mount + Python/TS SDKs           (2026-05-03, gated on Cortex)
✅ 6. Branch T0.6 + T0.7 + T2.6           (2026-05-03, T2.6 gated on CCC)
✅ 7. Water-Flow Incremental T1–T12       (2026-05-05, closes CCC cascade gap + sub-second p95)
✅ 8. Production-readiness sweep          (2026-05-06, vector-error promotion + T2.7 + desktop polish)
✅ 9. Branch v1.0 (first half)            (2026-05-06, T0.4 + T1.2 + T1.3 + T1.7 + T2.1 + T2.2 + T2.3 + T2.5)
✅ 10. Branch v1.0 finish                 (2026-05-06, T1.1 + T1.4 + T1.5 + T1.6 + T2.4 + T3.2 + T3.6 + T3.7)
🟡 11. Witness Mesh v1.0 (scaffold)       (2026-05-11, deterministic substrate live alongside legacy claims; cutover pending)
✅ 12. Install + Runtime Smoothness A–F   (2026-05-13, loud-blocking EngineGate + auto-restart + circuit breaker + recovery log; PATH fallback hotfix closes cargo install gap)
✅ 13. Compile Resilience + AI-Operator   (2026-05-14, unified `run_unified_compile` shared by desktop + SSE + AI MCP fast-path; compile-scoped breaker; auto-retry-once; SSE stall watchdog; ChatView synthetic progress for agent-driven compile)
✅ 14. Witness Mesh polish cleanup        (2026-05-14, 6-phase post-cutover; `thinkingroot-rooting` deleted; new `thinkingroot-llm` crate; read-side bridge; Phase 5 deferred)
✅ 15. Universal install + auto-update    (2026-05-14, curl-one-liner pattern; `dev.thinkingroot` login agent on macOS/Linux/Windows; `tauri-plugin-updater` signed updates; release.yml 3-job pipeline)
✅ 16. River v1.0                          (2026-05-14, REST↔MCP parity for auto-branch creation; persistent diamond merge glyph + 800ms SSE merged pulse)
?  17. Stream G — RFC drafts (continuous, not blocking)
```

### Deferred (with honest reason)

- **Stream G open-standards RFC drafts** — `.tr` format RFC, embed-widget protocol spec, W3C DID-VC contributions via `tr-identity`. Continuous, never blocking.
- **Stream C trust-crate publishes** — `tr-verify`, `tr-render`, `tr-identity`, `tr-revocation`, `tr-sigstore`, `tr-transparency` to crates.io. Needs the user's crates.io credentials; cannot be done in-session.

The dependency arrows are real: every track gated on the previous
one. Compile Completeness shipped the typed substrate; AEP and
Hybrid couldn't exceed 30%/3% utilization until that landed.
Cortex shipped before SDKs because adding Python/TS clients to
the existing CLI/Desktop pair without singleton discovery would
have turned the silent CozoDB lock-conflict bug into a
multi-process race. T2.6 (per-branch PII) was doubly-blocked
before the CCC populated `claims.sensitivity`.

### In flight

- **World-Class OSS Completion plan** (2026-05-14, ~12 weeks). Plan:
  `~/.claude/plans/rippling-seeking-abelson.md`. Seven gaps to ship
  `root` as a `$10/mo` student/researcher cognition product with
  cloud-managed LLM. Hot path: (1) `.rootignore` ✅ shipped as
  Track 17; (2) V3Pack reader v3.2 field expansion (`witnesses.cbor`,
  `rule_catalog.toml`, `paper.md`); (3) Phase 5 Witness Mesh cutover
  (retarget 31 `*claims{}` Datalog sites → `*witnesses{}`, replace
  lossy `[witness_type] symbol @byte_start..byte_end` synthesis with
  `materialize_statement` over `SourceByteStore`; remove the
  `claims`-writer path from `pipeline.rs`); (4) Living Paper v1
  (new `thinkingroot-paper` crate, Phase 10b per-compile synthesis,
  9 sections deterministic+AI, YAML frontmatter for machine reads,
  section-level BLAKE3 caching); (5) mathematical multimodal
  extraction (pure-Rust image/audio/video rule families — phash,
  MFCC, keyframes — replacing the dropped LLM-multimodal plan);
  (6) Playground UI v1 (3-pane surface alongside Chats/Brain, 13
  Tauri commands, drop-zone + citation chips + Mermaid + Living
  Paper panel + quiz + .tr export); (7) first tagged release that
  exercises the universal-install + auto-update + signing pipeline
  end-to-end.

- **ThinkingTouch Protocol** (Cursor Hackathon Riga, 2026-05-08).
  Spec: `docs/superpowers/specs/2026-05-07-thinkingtouch-design.md`
  (~2,000 lines, v1.0 final design). Rule:
  `.claude/rules/thinkingtouch.md`. **ThinkingTouch (TTP)** is a
  sibling project — a vendor-neutral open protocol giving AI
  agents hands to drive any UI (web / native macOS+Win+Linux /
  Electron / canvas / games), MIT-licensed. Hackathon ship
  2026-05-08 → 2026-05-10 at Shipyard AI Riga (Anthropic + Lovable
  + Cursor + Magebit backed). Implementation lives at
  `github.com/thinkingtouch/thinkingtouch` — separate repo,
  separate npm scope (`@thinkingtouch/*`), separate crates.io
  prefix (`thinkingtouch-*`). **Why it touches this repo:** the
  post-hackathon 90-day plan reuses the ThinkingRoot OSS trust
  crates (`tr-format`, `tr-sigstore`, `tr-transparency`,
  `tr-revocation`) for the v0.2 app-profile registry. Co-launch
  with ThinkingRoot v0.1 is the hackathon dogfood story. **What's
  in the spec:** 7-verb wire format (JSON-RPC 2.0 over stdio +
  WebSocket, `tt/` namespace), 4-strategy grounding (vision
  primary, AX augment, coords fallback, attention reserved),
  11-code typed error catalog, 3 protocol invariants
  (origin-tagged perception, speculative chains, capability +
  permission negotiation), 3-market framing (QA test + AI agent +
  power-user automation), 24-hour hackathon plan with hour-by-hour
  LOC budget, 90-second pitch demo script, post-hackathon 90-day
  plan, monetization (5 revenue lanes), competitive landscape with
  30-day delta scan.

---

## 1. Compile Completeness Contract (CCC)

**Date shipped:** 2026-05-02 · **LOC:** ~5,720 · **Status:** ✅ CI-gated

### Scope

The data-fidelity contract that every compile PR must preserve.
Four load-bearing invariants enforced end-to-end:

- **I-1 — 100% Datalog-queryable structure.** Every distinct field
  in `ChunkMetadata` (`thinkingroot-core/src/ir.rs:194-258`) and
  every `ChunkType` variant lands in a typed CozoDB column —
  never stuffed into `claim.statement` text. Substrate target
  expanded from 17 → **33 typed tables** + 4 new `ExtractedClaim`
  fields (`sensitivity`, `expiration_signal`, `valid_until`,
  `quantities[]`) + 1 plumbing field (`symbol`).
- **I-2 — 100% byte-anchored.** Every row in every structural
  table carries `(source_id, byte_start, byte_end)` — no
  exceptions. Extends the v3 byte-range citation contract from
  `claims` to every relation.
- **I-3 — 100% byte coverage.** End-of-compile **Phase 9 audit**
  fails on any orphan byte. Every byte of every source maps to
  ≥1 structural row OR to `chunks_residual`.
- **I-4 — 100% per-row tamper evidence.** Every structural row
  carries `content_blake3` over its source byte slice.
  Re-verified at AEP probe time + hybrid retrieval return time.

### Pipeline shape

- **Phase 6.7 (Structural Persist)** — 16 emitters, one per new
  table, plus a gap-fill sweep that emits `chunks_residual` rows
  for whitespace / blank-line / inter-chunk gaps so Phase 9 can
  pass on real workspaces.
- **Phase 7e (Structural Resolution)** — resolves
  `function_calls.callee_claim_id`, `code_links.is_internal /
  target_source_id`, and builds `source_references`.
- **Phase 9 (Byte-Coverage Audit)** — fails compile via
  `Error::ByteCoverageBreach { sources_with_orphans,
  total_orphan_bytes, sample }`. Escape hatch:
  `TR_SKIP_BYTE_AUDIT=1` for local iteration only; CI keeps it on.

### Files

- All 16 new structural tables in
  `crates/thinkingroot-graph/src/graph.rs`: `function_calls`,
  `doc_tags`, `code_links`, `code_signatures`, `config_tree`,
  `data_rows`, `git_commits`, `headings`, `chunks_residual`,
  `quantities`, `source_annotations`, `source_references`,
  `code_markers`, `test_annotations`, `git_blame`, `code_metrics`.
- `claims.content_blake3` + `claims.symbol` columns + auto-migration.
- `Claim.row_blake3` field stamped pre-linker by Phase 6.7.
- `crates/thinkingroot-serve/src/structural_persist.rs` — Phase 6.7 driver.
- `crates/thinkingroot-link/src/structural_resolve.rs` — Phase 7e.
- 3 new extractors:
  `crates/thinkingroot-extract/src/{sensitivity,expiration,quantity}.rs`.
- Migration: `crates/thinkingroot-serve/src/backfill.rs` +
  `root migrate --to-completeness-contract` CLI subcommand +
  auto-trigger in `pipeline.rs` when `compile_schema_version != "2"`.

### Verification

- `crates/thinkingroot-serve/tests/contract_invariants.rs` —
  Tests 12.1–12.5 prove the four invariants hold end-to-end
  against `tests/fixtures/contract_canonical/`.
- AEP Datalog rules fixture at `graph.rs:6146-6198`:
  `rule_trust_gate`, `rule_temporal_collapse`, `rule_gap_scan`.
- Per-emitter unit tests under
  `crates/thinkingroot-serve/src/structural_persist/*.rs`.

### Reference

`docs/2026-05-02-compile-completeness-contract.md` — canonical spec.

---

## 2. Active Engram Protocol v2 (AEP / RARP)

**Date shipped:** 2026-05-02 · **LOC:** ~2,900 · **Status:** ✅ shipped

### Scope

The read-path Engram-mediated probe interface. Expands AEP from
the v1 prototype (3 Datalog rules / 3 MCP tools / 5 tables) to:

- **12 Datalog rules** stored as `pub const &'static str` in
  `crates/thinkingroot-graph/src/aep_queries.rs`. No `:create rule`
  — const strings + `db.run_script(QUERY, params,
  ScriptMutability::Immutable)`.
- **4 MCP tools**: `materialize_engram`, `probe_engram`,
  `list_engrams`, `expire_engram`.
- **31 of 33 tables** consumed by `EngramManager`.
- Typed shapes: `EngramSummary` and `ProbeAnswer` carry
  `trial_scores`, `certificate_hash`, `source_blake3s`,
  `derivation_root`, `git_blame`, `test_origin`, `quantities`,
  `superseded_by_chain`, `unresolved_contradictions`, `gaps`, and
  sensitivity-clearance redactions on every probe answer.

### Key invariants (load-bearing across the read path)

- **`EngramManager` holds `GraphStore` by clone, not via the
  outer `Arc<RwLock<QueryEngine>>` chain** — multi-rule
  materialise doesn't serialise on the storage mutex.
- **BLAKE3 verification is lazy + memoised** via
  `Engram.blake3_cache` keyed on
  `(content_hash_str, byte_start, byte_end) → bool`.
- **`ProbeCaveat`, never `Error`**, for clearance / staleness —
  `SensitivityRedaction`, `StaleRow`, `LowConfidence` surface as
  typed-result-with-caveats so the LLM doesn't misroute them as
  protocol failures.
- **Cache-dirty compile invalidates Engrams** — both REST
  `compile_handler` and MCP `"compile"` arm call
  `engram_manager.invalidate_workspace(ws).await` post-success.
- **`turn_provenance` is bounded** to the most recent 200 turns
  (`EngramConfig.turn_provenance_window = 200`); past the window
  emits `TurnRef::Unknown`.
- **`EngramPointer` is HMAC-derived** (16-bit pointer space,
  `0xXXXX`) keyed on a per-`EngramManager` `pointer_secret`.
- **Probe routing is regex-only at v1** with `LowConfidence`
  fallback below 0.5 — no LLM call in the classifier.

### Files

- `crates/thinkingroot-graph/src/aep_queries.rs` — 20 cluster +
  9 probe const Datalog queries with cycle guards
  (`Q_SUPERSESSION_CHAIN`, `Q_DERIVATION_ROOT`).
- `crates/thinkingroot-serve/src/engram.rs` — `EngramManager`,
  `Engram`, `ProbeAnswer`, lifecycle errors.
- `crates/thinkingroot-serve/src/mcp/tools.rs` — 4 new MCP tool
  registrations + dispatcher arms.

### Reference

`docs/active-engram-protocol.md` (v2 rewrite).

---

## 3. Hybrid Retrieval

**Date shipped:** 2026-05-03 · **LOC:** ~2,110 · **Status:** ✅ shipped

### Scope

Vector × Datalog × BLAKE3 retrieval, replacing the flat
vector-only retriever (`intelligence/retriever.rs:1-16`) with a
**7-layer pipeline**:

```
QueryParser → QueryPlanner → VectorRecall + DatalogFilters →
CandidateMerger → StructuralEnricher (touches all 33 tables) →
ScoreFusion (11-component breakdown) → ByteSpanStitcher →
ProvenanceVerifier (BLAKE3) → SensitivityFilter
```

Returns `RetrievalHit` with full provenance bundle and
11-component score breakdown. Composable with AEP via
`probe_engram(.., score_with_hybrid: true)`.

### Key invariants

- **Vector backend is fastembed in-memory, NOT Cozo HNSW.** The
  world-class part is the Datalog fan-in + score fusion +
  per-row BLAKE3 verification *downstream* of recall.
- **`From<RetrievalHit> for ClaimSearchHit` is a one-way shim** —
  trial-score average when present, otherwise admission-tier
  proxy (`Rooted=1.0, Attested=0.7, others=0.0`).
- **AEP × Hybrid composition lives at the MCP handler layer**
  (`mcp/tools.rs::handle_probe_engram` ~line 1712) — composition
  doesn't change `EngramManager::probe_engram`'s signature.
- **IEEE 754 score determinism via fixed source order** in
  `fuse_score` — never `iter().sum()`, never `mul_add`.
- **ProvenanceVerifier is eager on top-K**, not lazy. Per-call
  `HashMap<(content_hash, byte_start, byte_end), bool>`
  deduplicates within one call but not across.
- **Recursive Datalog cycle guards** in `hybrid_queries.rs`:
  `Q_HR_IN_CALL_GRAPH_OF` and `Q_HR_SUPERSEDES_CLAIM` carry
  `mid != self` predicates.
- **Routing under 500 claims forces Datalog-only** — small
  workspaces don't have enough vector-space variation for cosine
  to be informative.
- **REST is single-shot JSON, not SSE** — spec §10 targets
  <25ms p95 for top-50; SSE adds >5ms framing overhead.

### Files

- `crates/thinkingroot-serve/src/intelligence/{hybrid_types,hybrid,dsl,scoring_profiles,byte_span}.rs`
- `crates/thinkingroot-graph/src/hybrid_queries.rs` — 13 typed-predicate const queries.
- 11-component score fusion with 2 named profiles
  (`ScoringProfile::default()`, `::compliance()`).
- MCP `hybrid_retrieve` tool registered alongside the 4 RARP tools.
- `score_with_hybrid` field on `EngramScope` + per-call MCP
  `probe_engram` parameter for AEP composition.
- REST `POST /api/v1/ws/{ws}/search/hybrid` route.

### Reference

`docs/2026-05-02-hybrid-retrieval-spec.md`.

---

## 4. Cortex Protocol — Singleton Engine Discovery

**Date shipped:** 2026-05-03 · **LOC:** ~2,400 · **Status:** ✅ shipped

### Scope

Closed the silent-corruption class from concurrent CozoDB
writers across CLI + Desktop + future SDK surfaces.

- **`cortex.lock` is the single source of discovery** at
  `<dirs::config_dir()>/thinkingroot/cortex.lock` (honours
  `XDG_CONFIG_HOME`). JSON-encoded with `schema_version`, `pid`,
  `port`, `host`, `version`, `started_by`, `started_at`,
  `binary_path`. Atomic `tempfile + persist` (rename(2) on
  POSIX, `ReplaceFileW` on Windows).
- **`schema_version` is reader-bumped, not writer-bumped** — a
  reader on version N refuses to parse a lockfile with
  `schema_version > N`.
- **`process_alive` is sysinfo-backed** (cross-platform; treats
  zombies as dead).
- **`/livez` health check has a 1s timeout** — attach-or-spawn
  must feel instant.
- **CLI `serve` default port flipped 3000 → 31760** (the cortex
  canonical port).
- **Daemon spawn is detached + log-redirected** —
  `process_group(0)` on Unix, `CREATE_NEW_PROCESS_GROUP |
  DETACHED_PROCESS` on Windows. stdout/stderr → `serve.log`
  (mode 0o600 on Unix).
- **Desktop `spawn()` is attach-or-spawn, never SIGKILL-then-spawn**
  — `cortex_bridge::resolve_engine(EngineIntent::DesktopBoot)`
  installs a `SidecarHandle{ child: None, .. }` on attach.
  `shutdown()` honours `child: None` by leaving the daemon
  running.
- **`--in-process` is the escape hatch, not the default** — every
  cortex-routed CLI command accepts a global flag for hermetic CI
  + air-gapped scenarios.
- **MCP stdio bypasses cortex** —
  `EngineConnection::Stdio` returned without touching the
  filesystem.
- **Cancellation = client disconnect, end-to-end** — every
  stateful REST handler wires `CancellationToken + DropGuard`.
- **Lazy auth read, never restart-on-rotation** — handlers call
  `Credentials::load()` per request.
- **Module split: sync types in core, async wrappers in
  consumers** — `thinkingroot_core::cortex` is sync (no `tokio`,
  no `reqwest` deps); each consumer writes its own thin async
  `resolve_engine`.

### Files

- `crates/thinkingroot-core/src/cortex.rs` — sync types,
  lockfile I/O, `process_alive`. ~280 lines + 15 unit tests.
- Workspace `Cargo.toml` — `sysinfo`, `fs2`, `tempfile` added as
  workspace deps.
- `crates/thinkingroot-cli/src/cortex_client.rs` — async
  `resolve_engine`, `spawn_detached_daemon`, `health_check`.
- `crates/thinkingroot-cli/src/cortex_remote.rs` — HTTP-delegate
  paths for compile/query/ask/health/render/reflect.
- `crates/thinkingroot-cli/src/serve.rs` — pre-bind
  attach-detection, post-bind lockfile write, graceful-shutdown
  lockfile removal.
- `crates/thinkingroot-cli/src/main.rs` — `--in-process` global
  flag, `try_resolve_remote` helper, stateful subcommands wired.
- `apps/thinkingroot-desktop/src-tauri/src/cortex_bridge.rs` —
  desktop's async wrapper.
- `apps/thinkingroot-desktop/src-tauri/src/agent_runtime_subprocess.rs`
  — refactored to attach-or-spawn.
- `crates/thinkingroot-cli/tests/cortex_scenarios.rs` — 13
  integration tests covering all 12 spec scenarios + 1 wedged-
  daemon recovery bonus.

### Test count

40 cortex-specific tests (13 scenario + 15 core unit + 8
cortex_remote unit + 4 cortex_client unit), 100% green.

### Reference

`docs/2026-05-02-unified-singleton-runtime.md`.

---

## 5. tr-mount + Python/TS SDKs (Secondary Brain Plug)

**Date shipped:** 2026-05-03 · **LOC:** ~3,200 · **Status:** ✅ shipped

### Scope

The "60-second secondary brain" promise from
`docs/secondary-brain-concept.md` §7 — one CLI command turns a
`.tr` knowledge pack into a queryable workspace, with three
honest transports per SDK (in-process / remote / cortex-aware).

### Components

- **`root mount <pack.tr>`** — replay-by-default,
  recompile-by-flag. Synthesizes Sources, Entities, Claims, and
  `claim_entity_edges` from `claims.jsonl` + `source.tar.zst`
  without an LLM extraction pass. `--recompile` drives the full
  33-table pipeline.
- **AEP + workspace mount/unmount REST endpoints**:
  - `POST /api/v1/workspaces` / `DELETE /api/v1/workspaces/{name}`
  - `POST /api/v1/ws/{ws}/engrams`
  - `GET /api/v1/ws/{ws}/engrams`
  - `DELETE /api/v1/ws/{ws}/engrams/{ptr}`
  - `POST /api/v1/ws/{ws}/engrams/{ptr}/probe`
- **Python `Brain` facade** — `Brain.open(path)` /
  `Brain.remote(url)` / `Brain.connect()` /
  `Brain.mount(pack)`. Identical method surface across all four
  constructors.
- **TypeScript `Brain` facade** — pure-fetch (Node 18+), ESM-only,
  no native bindings. Vitest tests.

### Key invariants

- **`root mount` is replay-by-default, recompile-by-flag.** v3
  packs ship `claims.jsonl` + `source.tar.zst` but NOT the
  33-table structural substrate.
- **`root mount` registers via REST**, not by daemon restart —
  `POST /api/v1/workspaces` mounts under the engine write-lock.
  Both mount + unmount call
  `engram_manager.invalidate_workspace(name)`.
- **Source-id mapping is by relative path**, not by content hash.
- **Mount layout matches `root install`** —
  `~/.thinkingroot/mounts/<owner>/<slug>/<version>/` mirrors
  `~/.thinkingroot/packs/<...>`. `sanitize_path_component` strips
  chars outside `[A-Za-z0-9._-]` (tar-slip protection).
- **AEP REST endpoints require `X-TR-Session-Id` header** —
  missing returns 400 with `MISSING_SESSION` so SDKs catch it on
  first call.
- **`Brain` is a facade, not a transport** — same surface across
  PyO3 in-process, httpx remote, fetch remote.
- **`Brain.connect()` does NOT silently fall back to in-process**
  when `cortex.lock` is absent — raises `ConnectionError`.
  Spawning + opening would defeat Cortex's single-writer
  guarantee.
- **`Brain.mount(path)` shells out to `root mount`** — both SDKs
  spawn the binary, parse `MountSummary` JSON, return a
  `Brain.remote(...)`. No re-implemented replay logic in either
  SDK.
- **PyO3 `Engine` owns its own `EngramManager` + session_id**
  (BLAKE3-derived from `(workspace, pid, monotonic nanos)`).
- **TypeScript SDK is pure-fetch** — no `node-gyp`, ESM-only,
  Node 18+, strict TS (`noUncheckedIndexedAccess`,
  `noImplicitOverride`, `strict`).
- **Wire-shape parity across surfaces** — Python `Brain`, TS
  `Brain`, and Rust `engine::QueryEngine` return the same shapes
  for AEP probes, hybrid retrieval, and engram materialise.

### Files

- `crates/thinkingroot-cli/src/mount_cmd.rs` — `root mount`
  command + claim-replay synthesis (~480 LOC + 7 unit tests).
- `crates/thinkingroot-serve/src/rest.rs` — 6 new endpoints (~360 LOC).
- `crates/thinkingroot-serve/src/mcp/tools.rs` — bumped 4
  helpers from `pub(crate)` to `pub` (now SDK contract surface).
- `thinkingroot-python/src/lib.rs` — extended `Engine` with
  AEP + Hybrid methods (~300 LOC added).
- `thinkingroot-python/python/thinkingroot/brain.py` (~360 LOC).
- `thinkingroot-python/python/thinkingroot/cortex.py` (~120 LOC).
- `thinkingroot-python/tests/test_brain_facade.py` — 9 pytest
  tests (1 skipped on macOS XDG).
- `sdks/typescript/` — package.json, tsconfig, src/, test/, README.
  ~890 LOC + 13 vitest tests.
- `docs/2026-05-03-secondary-brain-quickstart.md` — 60-second
  walkthrough across CLI/Python/TS/raw HTTP.

### Reference

`docs/secondary-brain-concept.md` §5 +
`docs/2026-05-03-secondary-brain-quickstart.md`.

---

## 6. Branch System T0.6 + T0.7 + T2.6

**Date shipped:** 2026-05-03 · **LOC:** ~640 · **Status:** ✅ shipped

### Scope

Three Tier-0 + one Tier-2 items from
`docs/branch-system-improvements.md`, gated on the CCC for T2.6.

- **T0.6 — `BranchKind` + `MergePolicy` + `Principal`** —
  replaces the historical `BranchActor` enum + `stream/` name-
  prefix convention with typed discriminators.
- **T0.7 — Connector identity + idempotent bulk contribute** —
  `contribute_bulk` API with `connector_ingest_log` replay
  protection and optional `backfill` mode.
- **T2.6 — Per-branch PII redaction policy** — `RedactionPolicy`
  on `BranchRef` enforced at `list_claims_branched`,
  `search_branched`, and `get_workspace_brief_branched`
  outbound boundaries.

### Key invariants

(See CLAUDE.md "Branch system T0.6 + T0.7 + T2.6 invariants"
section for the full 13-rule set. Highlights:)

- **`Principal` is the actor model; `BranchActor` stays as a
  type alias** — every legacy call site keeps compiling.
- **Connector identity is `connector_id:install_id`** — the
  pair, never just `connector_id`. Same install of "github" by
  two different orgs = two distinct identities.
- **`BranchKind` is the typed discriminator; `stream/` name
  prefix is the migration shim** — fall back to prefix only for
  pre-T0.6 branches.
- **`MergePolicy::Ephemeral` short-circuits to abandon, never
  merges** — `execute_merge_into` reads the source branch's
  policy after the health gate.
- **`MergePolicy::RequiresProposal` blocks raw merges** until
  T0.4 ships the proposal layer.
- **`branches.toml` round-trips through `#[serde(default)]` for
  every new field** — pre-T0.6 user files keep mounting.
- **`contribute_bulk` requires `Principal::Connector`** for
  idempotency scoping. Non-connector principals rejected at
  entry.
- **Idempotent replay is per-target-graph, not workspace-global**
  — branch graph when branched, main graph otherwise.
- **Connector source URI is `connector://{cid}/{iid}/{key}`**
  (distinct from `mcp://agent/{session_id}` for interactive
  contributes).
- **`RedactionPolicy` stores raw regex strings, compiles on
  apply** — `regex::Regex` isn't `Serialize`. Invalid patterns
  are logged at WARN and skipped, never panic the outbound path.
- **Sensitivity gating is at-or-above the threshold; drop is
  the default** — `Public < Internal < Confidential <
  Restricted` (`Sensitivity` derives `Ord`).
- **Redaction applies before pagination** — page count must be
  stable across redaction.
- **Sensitivity is one batched extra query per outbound call**,
  via `GraphStore::get_sensitivities_for_claims`.

### Files

- `crates/thinkingroot-core/src/types/branch.rs` — extended
  with `BranchKind`, `MergePolicy`, `OutboundMode`,
  `RedactionPolicy` + 11 unit tests. Adds `kind`,
  `merge_policy`, `redaction` fields to `BranchRef` (all
  `#[serde(default)]`). `MergedBy` extended with `Connector` +
  `System` variants.
- `crates/thinkingroot-core/Cargo.toml` — `regex` workspace dep.
- `crates/thinkingroot-branch/src/{branch,lib,merge}.rs` —
  `create_branch_full` + `set_branch_redaction` exported
  helpers, Ephemeral + RequiresProposal merge gates.
- `crates/thinkingroot-branch/tests/branch_tests.rs` — +4
  scenario tests.
- `crates/thinkingroot-graph/src/graph.rs` — new
  `connector_ingest_log` relation + `lookup_connector_ingest`
  / `record_connector_ingest` / `get_sensitivities_for_claims`
  helpers + `ConnectorIngestRecord` shape.
- `crates/thinkingroot-serve/src/engine.rs` — `Principal` enum
  (replaces `BranchActor`), `contribute_bulk` +
  `contribute_with_source_override`, `branch_redaction_for`,
  `apply_redaction_to_claim_infos`,
  `apply_redaction_to_search_hits`, redaction wiring on 3
  outbound paths, Tag immutability in
  `ensure_branch_permission`.
- `crates/thinkingroot-serve/src/maintenance.rs` —
  kind-typed Stream filter with prefix fallback; Ephemeral
  always abandons.
- `crates/thinkingroot-serve/src/mcp/{mod,tools}.rs` —
  auto session branch uses `BranchKind::Stream` +
  `MergePolicy::AutoOnSessionEnd`; `contribute_bulk` MCP tool.
- `crates/thinkingroot-serve/src/rest.rs` — extended
  `CreateBranchRequest`, new `POST .../contribute-bulk` +
  `POST .../redaction` routes.
- `crates/thinkingroot-serve/tests/connector_bulk_test.rs` —
  11 integration tests.

### Reference

`docs/branch-system-improvements.md` §T0.6, §T0.7, §T2.6.

---

## 7. Water-Flow Incremental Compile (T1–T12, complete)

**Date shipped:** 2026-05-05 · **LOC:** ~5,800 across 14 commits ·
**Status:** ✅ all 12 tracks shipped; CI-gated p95 = 98ms (10× headroom)

### Scope

Closes the silent² cascade-gap bug from the CCC ship. Pre-water-flow,
`remove_source_by_id` cascaded `claims` + 8 legacy tables but ignored
the 16 new structural tables CCC added (`function_calls`, `headings`,
`doc_tags`, `code_links`, `code_signatures`, `config_tree`, `data_rows`,
`chunks_residual`, `quantities`, `source_annotations`,
`source_references`, `code_markers`, `test_annotations`, `git_blame`,
`git_commits`, `code_metrics`). Result: every file rename, function
delete, function move, or byte-shift edit left stale rows behind that
AEP and Hybrid joined against silently — the audit was blind to it,
the read path saw shrunken/dangling joins, no error fired.

The Option-B slice ships the four write-side / audit / resolution /
snapshot invariants. T8–T12 (observability + watch mode + p95 gate)
remain pending and will land in a follow-up.

### Pipeline shape

```
Phase 4 (source removal)
  ├── before remove: list_dependent_sources via resolution_deps:by_to
  ├──                 → resolution_dirty_sources (logged, used by T11)
  ├── remove_source_by_id
  │     └── cascade :rm for all 16 structural tables + resolution_deps (both dirs)
  │     └── via STRUCTURAL_TABLES registry + canonical pk_rm_script_for_table
  └──
Phase 6.7 (structural persist)
  └── flush_buckets per source
        └── transactional_rebuild_source(source_id, &PerSourceRows)
              └── ONE Cozo multi_transaction(write=true) boundary:
                    16 :rm cascades + per-table :put for new rows
              └── stats recorded only AFTER commit
Phase 7e (structural resolve)
  ├── revalidate EVERY function_calls.callee_claim_id and code_links.target_source_id
  ├── dangling pointers reset to "" or re-resolve
  └── populate resolution_deps for cross-source edges
Phase 9 (audit)
  ├── existing byte-coverage check (CCC)
  └── NEW: query_orphan_structural_rows
       └── fails compile via Error::OrphanStructuralRows { count, sample }
```

### Key invariants — load-bearing

- **I-W1 — Cascade completeness.** Every structural table in
  `STRUCTURAL_TABLES` registry participates in source removal. The
  `pk_rm_script_for_table` helper in
  `thinkingroot-core/src/structural_registry.rs` is the single source
  of truth — both `graph.rs` cascade and `backfill.rs` migration
  delegate through it.
- **I-W2 — Phase 9 detects deleted-source orphans.**
  `query_orphan_structural_rows` returns rows whose `source_id` ∉
  `sources`. Phase 9 fails via `Error::OrphanStructuralRows { count,
  sample }`. Error message points users at `root migrate --to-water-flow`.
- **I-W3 — Resolution re-validation per compile.** Phase 7e
  re-validates EVERY resolved `callee_claim_id` and `target_source_id`
  per compile, not just unresolved rows. `resolution_deps` table
  records cross-source edges with `:by_from` / `:by_to` indexes;
  cascaded in both directions. Phase 4 reads `:by_to` BEFORE source
  removal to collect a dirty-source set.
- **I-W4 — Snapshot-consistent per-source rebuild.**
  `transactional_rebuild_source` runs cascade + 16-table emit inside
  one Cozo `multi_transaction(write=true)`. Concurrent readers never
  see torn state. Rolls back on any failure.

### Migration

`compile_schema_version` "2" → "3". Auto-triggered on first compile
after engine upgrade; chained pre-v2 → v2 → v3.
`backfill_water_flow_v3` purges orphan structural rows, resets
dangling `function_calls.callee_claim_id` pointers to `""`, builds
`resolution_deps` from current resolved `function_calls` AND
`code_links`, bumps schema version. Idempotent.

Explicit invocation: `root migrate --to-water-flow [--dry-run]`.
Clap-level `requires = "to_water_flow"` on `--dry-run` prevents
silent acceptance with `--to-completeness-contract`.

### Files (new + modified)

**New (3):**

- `crates/thinkingroot-core/src/structural_registry.rs` — registry +
  canonical `pk_rm_script_for_table` (T1 commit `6b34e7a`).
- `crates/thinkingroot-graph/src/per_source_rows.rs` — `PerSourceRows`
  + `transactional_rebuild_source` + 16 `*_put_spec` helpers + 6 unit
  tests, ~1,099 lines (T7 commit `5c61618`).
- `crates/thinkingroot-serve/tests/incremental_concurrency_test.rs` —
  4 atomicity + concurrency tests (T7).

**Modified (10):**

- `crates/thinkingroot-core/src/error.rs` — `OrphanStructuralRows`
  variant.
- `crates/thinkingroot-core/src/lib.rs` — re-exports.
- `crates/thinkingroot-graph/src/graph.rs` — `resolution_deps` schema
  + indexes, two-direction cascade, fixed `insert_claim` to write
  `content_blake3` + `symbol`, delegates `pk_rm_script_for_table` to
  registry.
- `crates/thinkingroot-graph/src/structural_inserts.rs` —
  `query_orphan_structural_rows`, `record_resolution_dep`,
  `list_dependent_sources`, `get_claim_source_id`,
  `list_all_code_links`. (Removed `list_unresolved_*` dead code.)
- `crates/thinkingroot-graph/src/lib.rs` — register
  `per_source_rows` module.
- `crates/thinkingroot-link/src/structural_resolve.rs` — rewrote
  Phase 7e to revalidate every row + populate `resolution_deps`.
  `ResolutionStats.calls_updated` / `links_updated` (renamed from
  `*_resolved`).
- `crates/thinkingroot-link/src/linker.rs` — tracing log uses new
  field names.
- `crates/thinkingroot-serve/src/backfill.rs` —
  `backfill_water_flow_v3` + `_at_path` + migration step 3 builds
  `resolution_deps` for both `function_calls` and `code_links`.
- `crates/thinkingroot-serve/src/pipeline.rs` — chained pre-v2 → v2
  → v3 auto-migration; Phase 4 `resolution_dirty_sources` collection
  before remove; Phase 9 structural orphan check after byte audit.
- `crates/thinkingroot-serve/src/structural_persist.rs` —
  `flush_buckets` drives one `transactional_rebuild_source` per
  source; captures bucket lens before `std::mem::take` so stats are
  recorded only after the commit succeeds.
- `crates/thinkingroot-cli/src/main.rs` — `Migrate` variant extended
  with `to_water_flow` + `dry_run` (clap `requires`).

**Tests (5 files):**

- `crates/thinkingroot-serve/tests/incremental_cascade_test.rs` — 14
  cascade + revalidation + dep tests.
- `crates/thinkingroot-serve/tests/migration_v2_to_v3_test.rs` — 6
  migration tests.
- `crates/thinkingroot-serve/tests/incremental_concurrency_test.rs`
  — 4 concurrency / probe / partial-failure tests.
- 6 unit tests in `per_source_rows::tests` module.

### Commits

```
5fb61b1  fix(v3/incremental): T7 code-quality follow-ups
5c61618  feat(v3/incremental): per-source transactional rebuild
d01c418  feat(v3/incremental): resolution_deps schema + Phase 4 invalidator
9de5ba0  fix(v3/incremental): T4 code-quality follow-ups
7602a06  feat(v3/incremental): revalidate every Phase 7e resolution per compile
75fd24b  fix(v3/incremental): T3 code-quality follow-ups
572545f  feat(v3/incremental): migrate v2 → v3 with auto + explicit triggers
9057d56  feat(v3/incremental): widen Phase 9 to detect orphan structural rows
0793b52  feat(v3/incremental): cascade source delete to all 16 structural tables (pre-Option-B)
6b34e7a  feat(v3/incremental): add STRUCTURAL_TABLES registry as cascade source of truth (pre-Option-B)
```

### T8–T12 — observability + watch + sub-second p95 (shipped same day)

**T8+T9 — IncrementalSummary + per-phase timing** (commits `fe16cde`, `663a3b3`):
- `thinkingroot_core::IncrementalSummary` struct in `types/incremental.rs`. 16 fields covering source/claim/structural/extraction deltas, per-phase wall-clock, total elapsed.
- Stable `PHASE_NAMES` constant: 10 entries (`diff`, `extract`, `ground`, `fingerprint`, `remove_sources`, `entity_relations`, `link`, `structural_persist`, `audit`, `other`). Phase 7e (`structural_resolve`) is intentionally subsumed under `link`.
- `PipelineResult.incremental_summary: IncrementalSummary` (NON-Option). Every successful compile populates it — including the three early-return paths (nothing-changed, deletions-only, all-fingerprint-cutoffs).
- New SSE events: `ProgressEvent::PhaseDone { name, elapsed_ms }` per-phase, `ProgressEvent::IncrementalDone { summary }` once at end. Phase 5 + Phase 8 both emit `PhaseDone` for `entity_relations` (additive).
- `PipelineOptions.skip_byte_audit: bool` — typed replacement for `TR_SKIP_BYTE_AUDIT=1` env var. Closes a long-standing repo-wide invariant gap (the `unsafe set_var` race).
- 9 tests in `incremental_summary_test.rs`.

**T10 — CLI summary printer + desktop panel** (commits `68878cf`, `2b1fb5a`):
- `thinkingroot_cli::summary_printer::{render, print}`. Phase timings rendered in `PHASE_NAMES` order (NOT BTreeMap alphabetic — contract asserted via `windows(2)` loop).
- `--json` flag on `Compile` — emits one-line JSON, suppresses TTY progress bars to keep stdout clean for `jq` piping.
- `cortex_remote::run_compile_remote` captures `incremental_done` SSE via a `match` (not `if let Ok`) — failed deserialization logs via `tracing::warn!`, never silent (no-silent-failure invariant).
- `summary_printer::print` uses `print!` (not `write_all().expect()`) so `root compile | head -5` doesn't panic on SIGPIPE.
- Canonical `format_bytes` (B/KiB/MiB/GiB/TiB, `.2` precision) lives in `thinkingroot_core::types::incremental` — both summary_printer and any future telemetry surface delegate there.
- Desktop `CompileProgress::Done.incremental_summary: Option<IncrementalSummary>` (`#[serde(default)]`). Single construction site at `Ok(result)` covers both in-process and sidecar paths.
- 5 tests in `summary_printer_test.rs`.

**T11 — `root compile --watch`** (commit `d92d16b`):
- `thinkingroot_cli::watch::{WatchOptions, run_watch_loop, is_noise}`. notify-debouncer-mini (notify v8) backend.
- Flags: `--watch`, `--debounce <ms>` (default 200), `--no-incremental` (forces full re-extract).
- `is_noise()` filters `.git/`, `.thinkingroot/`, `target/`, dotfiles, vim/editor swap suffixes (`.swp`, `.swo`, `.swx`, `~`, `.tmp`, `.bak`, vim's `4913` probe).
- Single-writer guarantee from sequential `await` — no two compiles concurrent; edits during a running compile accumulate in `pending` and trigger the next debounce window.
- Tokio `mpsc::unbounded_channel` bridge from sync notify callback to async loop — no `spawn_blocking` thread.
- 9 tests (4 unit + 5 `#[serial]`-gated integration covering debounce collapse, noise filtering, single-writer serialization, error recovery, editor-swap filtering).

**T12 — Source-granular re-extract + benchmark gate** (commit `db56d3f`):
- `Extractor::extract_all` accepts `sources_to_extract: Option<HashSet<String>>`. Filter applies BEFORE cache lookup or LLM dispatch — dominant cost saver on small-edit-large-workspace scenarios.
- `pipeline.rs` Phase 2 builds the filter from `potentially_changed` (Phase 1 diff set). `PipelineOptions.no_incremental: bool` opts out.
- `Provider::StructuralOnly` + `LlmClient::new_structural_only()` — no-op LLM variant that lets pipelines run without `config.toml` configured. `Extractor::new` falls back gracefully.
- Phase 1 collateral perf win: replaced N `find_sources_by_uri` queries with one `get_sources_with_hashes()` call.
- `crates/thinkingroot-serve/benches/incremental_smoke.rs` — REAL fixture (no `thread::sleep` stub), 100 markdown sources, 5 trials of 1-line edit + incremental compile. Asserts `sources_truly_changed == 1` per trial AND p95 < 1000ms. Run via `cargo bench -p thinkingroot-serve --bench incremental_smoke`. Observed darwin/M-series: **p50=94ms, p95=98ms, max=98ms**.
- 3 unit tests on `extract_all` filter (None / matched-subset / empty).

### T8–T12 commits

```
db56d3f  feat(v3/incremental): T12 — source-granular re-extract + sub-second p95 benchmark gate
d92d16b  feat(v3/incremental): T11 — root compile --watch with notify-rs + 200ms debounce
2b1fb5a  fix(v3/incremental): T10 code-quality follow-ups (mod dedup, --json/progress race, silent IncrementalDone parse, SIGPIPE panic, canonical format_bytes)
68878cf  feat(v3/incremental): T10 — CLI summary printer + desktop incremental_summary panel
663a3b3  fix(v3/incremental): T8+T9 code-quality follow-ups (skip_byte_audit + PHASE_NAMES truthiness + Phase 8 PhaseDone)
fe16cde  feat(v3/incremental): T8+T9 — IncrementalSummary wire type + per-phase timing
```

### Reference

- Spec: `docs/superpowers/specs/2026-05-04-incremental-compile-water-flow-design.md`
- Plan: `docs/superpowers/plans/2026-05-04-incremental-compile-water-flow.md`
- Invariants: `.claude/rules/compile-completeness.md` "Water-flow incremental compile invariants" section

---

## How to update this file

When a new track ships:

1. **Bump the at-a-glance row** at the top with date + test delta.
2. **Update the test scoreboard** with the new totals.
3. **Append the new track to the sequencing chain** as a `✅` line.
4. **Add a new top-level section** following the existing template:
   - Date shipped · LOC · Status
   - Scope (one paragraph + bullet highlights)
   - Key invariants (the load-bearing rules — same shape as
     CLAUDE.md "invariants" sections)
   - Files (new + modified)
   - Reference (canonical spec doc path)
5. **Cross-update CLAUDE.md** — add the corresponding invariants
   block under the existing pattern (`## <Track> invariants
   (<date>, **shipped**)`) and bump the sequencing-chain comment
   at the top.

The two documents are paired: this file is the human-readable
ledger, CLAUDE.md is the rule book future Claude sessions use to
avoid re-introducing solved bugs. Keep them in sync.

---

## 8. Production-readiness sweep

**Date shipped:** 2026-05-06 · **Commit:** `4cf5b6d` · **Status:** ✅ shipped

### Scope

The 16-finding audit fold + desktop v0.1 polish + branch
correctness fixes that close the remaining honesty-rule gaps
before launch. No new features — every change is a tightening of
an existing path.

- **Vector-error promotion** in `merge.rs` — three previously-`tracing::warn!("(non-fatal)")` paths during deletion-propagation and reconciliation now return `Error::VectorStorage` so a corrupt-or-missing post-merge vector index surfaces loudly instead of silently corrupting hybrid retrieval. Pre-merge snapshot remains the recovery anchor; the error message points users at `root branch rollback`.
- **Registry-write race fix** in `branch.rs` — added `RegistryAdvisoryLock` (generalised from `MergeLock`) around the load → mutate → save window in `create_branch_full` and `set_redaction`. 32-thread concurrent-create test pins the no-loss invariant.
- **A1 mount-trust regime rename** at `mount_cmd.rs:272` — `verify_signature` (which was a bare `Ok(())`) renamed to `acknowledge_signed_bundle_in_local_trust_mode`. The function name now accurately describes what it does (records the regime in `tracing::info!` for audit, does NOT perform the strict Sigstore handshake — that's `root install`'s job per `.claude/rules/tr-mount-sdk.md`).
- **A4 desktop chat client builder** at `chat.rs:194-198` — replaced `.unwrap_or_default()` (which silently produced a no-timeout client on a bad system clock) with `?` propagation + Tauri `emit_error` event so a broken client surfaces as a UI toast instead of an infinite spinner.
- **A5 (T2.7 fold) orphan-merge auto-recovery** — new `crates/thinkingroot-branch/src/recovery.rs` with `recover_orphan_merges(workspace_root)`. Persists a `MergeIntent` to `.thinkingroot-refs/merges_in_flight.toml` BEFORE Step 1 of `execute_merge_into`; clears it after Step 10. Workspace-open hook calls `recover_orphan_merges` to roll the target back from the pre-merge snapshot when a crashed-mid-merge intent is found.
- **Desktop B1 BYOC vs cloud chat toggle** — `use_cloud_agent` setting; `chat.rs` branches the URL between local sidecar (`http://127.0.0.1:<port>`) and cloud hub (`https://hub.thinkingroot.dev`). Vitest covers both transports.
- **Desktop B2 8-segment status bar** — `StatusBar.tsx` config-array migration (sidecar, workspace, branch, jobs, rooting-health, credits, sync, errors). Last two hide when signed out — preserves Honesty Rule #6 (never claim something synced when it didn't).
- **Desktop B3 capsule/satellite terminology cleanup** — replaced legacy strings in `InstallTrSheet.tsx`, `LiveAgentsPanel.tsx`, `command-palette/catalog.ts`. Grep verified zero hits remain.

### Verification

- New tests: `merge_fails_loud_on_vector_save_error`, `create_branch_full_serialises_concurrent_writes`, `failed_merge_leaves_intent_and_recovers`, plus desktop Vitest tests for the BYOC toggle + 8-segment status-bar signed-in/out snapshots.
- Honesty audit: `grep -rn "unwrap_or_default\|tracing::warn.*non-fatal\|Ok(())"` across `crates/thinkingroot-branch crates/thinkingroot-serve crates/thinkingroot-cli/src` returned zero remaining hits on data-mutation paths.

---

## 9. Branch v1.0 (first half) — T0.4, T1.2, T1.3, T1.7, T2.1, T2.2, T2.3, T2.5

**Date shipped:** 2026-05-06 · **Commit:** `3d3034f` · **Status:** ✅ shipped

### Scope

Eight tracks of `docs/branch-system-improvements.md` cleared in a single slice — closes the user-visible 🔴 gap (Knowledge Proposals were Rust-API-only) and ships every Tier-1/Tier-2 item that doesn't require schema migration.

- **T0.4 Knowledge Proposal lifecycle** — full REST surface: `POST /branches/{branch}/proposals`, `GET /proposals[/{id}]`, `POST /proposals/{id}/reviews`, `POST /proposals/{id}/close`. Mirrored MCP tools: `open_proposal`, `review_proposal`, `list_proposals`, `close_proposal`. The `MergePolicy::RequiresProposal` gate at `merge.rs:336` becomes navigable — `find_approved_proposal(source, target)` consults the on-disk `.thinkingroot-refs/proposals/<ulid>.toml` rows. `X-TR-User` header required for opens / reviews / closes; missing returns 400 `MISSING_PRINCIPAL`.
- **T1.2 Branch stats** — `GET /branches/{branch}/stats` returns `{claim_count, entity_count, source_count, event_count, status}` via cheap GraphStore probes; no full diff.
- **T1.3 Branch audit log** — `BranchEvent` enum (`Created` / `Merged` / `Abandoned` / `RedactionUpdated` / `PermissionsUpdated` / `ContributeBulk`) on `BranchRef.events: Vec<BranchEvent>` (`#[serde(default)]` for old-TOML round-trip). `MAX_EVENTS = 1000` FIFO cap. `GET /branches/{branch}/events` reads from the on-disk registry.
- **T1.7 Branch lineage DAG** — `GET /branches/lineage` aggregates `(parent → child)` fork edges + `(child → into)` merge edges (with `authorising_proposal_id` when present) across every branch in the registry (active + merged + abandoned). Brain UI consumes this for the visual DAG.
- **T2.1 APFS clonefile + Linux FICLONE** — `clone_file_fast(src, dst)` in `snapshot.rs` uses `libc::clonefile` on macOS (HFS+/APFS reflinks) and `libc::ioctl(dst_fd, FICLONE, src_fd)` on Linux (btrfs/xfs/zfs). Falls back to `fs::copy` everywhere else. Three call sites in `create_branch_layout` switched. Branch creation on a 1GB graph.db drops from 1–3s → <10ms.
- **T2.2 Protected branches** — `MergeConfig.protected_branches: Vec<String>` (defaults to empty — opt-in). `execute_merge_into_with_options(force=false)` rejects merge into a protected target with `Error::MergeBlocked`; `force=true` bypasses (matches the existing health-score force semantics). Tag immutability is enforced separately at `engine::ensure_branch_permission` and is NOT bypassed by `force`.
- **T2.3 Branch TTL** — `BranchRef.max_age_secs: Option<u64>` (`#[serde(default)]`). New `set_branch_max_age_secs` helper. `maintenance::ttl_cleanup_once` walks every Active branch, computes `(now - created_at)`, abandons when over TTL. Tag branches are skipped (immutable). Drops the cached engine handle before abandoning.
- **T2.5 Tag create + REST** — `create_tag(root, name, ref_name, target, owner, description)` actually constructs `BranchKind::Tag { ref_name, target }`. `POST /tags`, `GET /tags`, `GET /tags/{name}`. The pre-existing immutability gate at `engine.rs:919-926` (shipped with T0.6) finally has live data to gate.

### Verification

- 14 unit tests in `thinkingroot-core` (BranchEvent round-trip, MAX_EVENTS cap, clone_file_fast byte-equal, etc.).
- 29 integration tests in `thinkingroot-branch` (was 26 + 3 new: TTL, tag round-trip, protected-target gate).
- 4 new integration tests in `thinkingroot-serve` covering the proposal lifecycle (open → review → close, missing-principal 400, invalid-id-shape 400, unknown-id 404).

---

## 10. Branch v1.0 finish — T1.1, T1.4, T1.5, T1.6, T2.4, T3.2, T3.6, T3.7

**Date shipped:** 2026-05-06 · **Commits:** `0815bd1`, `975ae0a`, `66de002`, `99d6334`, `2d1a463`, `55b9230` · **Status:** ✅ shipped

### Scope

The remaining eight branch-system tracks plus T2.4 bitemporal as-of and T3.6 schema-migration registry. Closes branch v1.0 to **~100% of `docs/branch-system-improvements.md`** spec coverage.

- **T1.1 Vector-embedding contradiction pass** — third pass in `diff.rs` after the negation-pair and Jaccard passes. `apply_vector_contradiction_pass` opens both branches' `vectors.bin`, queries cosine > 0.75 across shared entity context, and surfaces semantic conflicts that the earlier two passes miss (e.g. "uses JWT" vs "migrated to OAuth2"). Reuses the source-side stored embedding via new `VectorStore::search_by_vector` + `get_embedding` so the pass is **zero model inference per merge**. Gated on `vectors.bin` existing on both sides; falls back to vector-free diff cleanly on fresh workspaces.
- **T1.4 Branch-as-pack export + import** — `root pack --branch <name>` packs a branch's graph.db (engine_dir shifts to `<root>/.thinkingroot/branches/<slug>/`; source-byte store stays on main). `root branch-import <pack> <branch> [--no-verify]` is the round-trip pair: parses + integrity-verifies the pack, forks main via `create_branch_full`, replays claims/entities/sources into the branch's data dir. New `BranchImportSummary` JSON wire shape for SDK + automation parsing.
- **T1.5 Dry-run + cancel-in-flight merge** — `dry_run_merge_into` runs the same three-way / two-way diff chain `merge_into` would, but never touches the target graph or the registry. REST: `POST /branches/{branch}/merge?dry_run=true`. New `merge_into_branch_cancellable` + `execute_merge_into_cancellable` with phase-boundary `CancellationToken` checks (pre-protected-gate, pre-policy-gate, pre-intent-write, pre-apply-diff, late between apply and `mark_merged_into`). Past `mark_merged_into` the token is intentionally ignored — registry write must complete to keep on-disk state consistent. `AppState.active_merges: HashMap<String, CancellationToken>` mirrors `active_compile`. `POST /merges/{id}/cancel`.
- **T1.6 Live SSE branch events** — `AppState.branch_event_hub: HashMap<String, broadcast::Sender<BranchEvent>>` (capacity 64; lazy create + reuse per branch). Mutations call `publish_latest_branch_event` after success, dropping the engine read-lock before the broadcast so slow subscribers can't stall a write. SSE endpoint: `GET /branches/{branch}/events/stream` with `BroadcastStream` + `Lagged → "lagged"` event so slow consumers can resync via the polling endpoint.
- **T2.4 Bitemporal as-of queries** — `GraphStore::get_claims_with_sources_as_of(tx_time)` returns claims whose `created_at` ≤ tx_time. `engine::list_claims_as_of_branched(ws, branch, tx_time)` is the branch-aware entry point. REST: `GET /ws/{ws}/claims/as-of?as_of=<ISO-8601>[&branch=name]`. Honest scoping note: a dedicated `tx_time` column was deferred (would force every workspace through `compile_schema_version "3" → "4"`); for v0.1 the existing `created_at` IS the transaction time on every engine-inserted claim. Public API shape (`chrono::DateTime`, `tx_time` naming) is forward-compatible with the eventual column.
- **T3.2 Cross-branch reflect** — `engine::reflect_across_branches(ws, &branches)` walks branches sequentially calling `reflect_branched`, classifies each pattern id as present-in / absent-from per branch, and returns `CrossBranchReflectResult { workspace, branches, per_branch, divergent_patterns }` sorted by aggregate sample size. Deliberately omits aggregate_patterns — branches share substrate via copy-on-write, so aggregating would double-count fork-inherited claims. REST: `POST /ws/{ws}/reflect/across-branches`.
- **T3.6 Schema versioning + claim-migration registry** — new `thinkingroot_core::types::claim_migration` module with `ClaimMigration { from, to, name, apply: fn(&mut Claim) }` + `MigrationRegistry`. Process-global `OnceLock<RwLock<MigrationRegistry>>` populated via `register_migration` at consumer bootstrap. Contiguous-version chain (`from = N`, `to = N + 1`); duplicate + non-contiguous rejected on register. `migrate_claim` walks the chain in order with fail-fast error propagation — chain gap or failing migration aborts; we never ship a partially-migrated claim. `lib::merge_into_cancellable` calls `apply_claim_schema_migrations` between gate and execute, reading both sides' `workspace_meta.claim_schema_version` (defaults to `1` when absent — preserves pre-T3.6 behaviour). Migration applies to the in-memory `KnowledgeDiff`, not to the source graph on disk — preserves dry-run's no-mutation contract.
- **T3.7 Branch templates** — `thinkingroot_branch::templates` module with `BranchTemplate` struct + `TemplateRegistry` (atomic tempfile + rename save). `branch_templates.toml` lives in `.thinkingroot-refs/` next to `branches.toml`. Seeded with two opinionated defaults: `review-required` (`MergePolicy::RequiresProposal { min_reviewers: 1 }`) and `agent-sandbox` (`MergePolicy::Ephemeral`). `CreateBranchRequest` extended with optional `template`; explicit fields always win over template defaults. REST CRUD: `GET/POST /branch-templates`, `GET/DELETE /branch-templates/{name}`.

### Verification

- 26 new tests across the eight tracks:
    - 2 in `branch_tests.rs` for T1.1 (synthetic-embedding flagged-conflict + shared-entity-gate).
    - 3 in `dry_run_cancel_merge_test.rs` for T1.5.
    - 1 in `pack_cmd::tests` for T1.4 round-trip (workspace → pack_a → import as branch → re-pack → pack_b).
    - 3 in `branch_events_sse_test.rs` for T1.6.
    - 3 in `as_of_query_test.rs` for T2.4.
    - 2 in `cross_branch_reflect_test.rs` for T3.2.
    - 6 unit tests in `claim_migration::tests` + 3 integration tests in `schema_migration_test.rs` for T3.6.
    - 5 unit tests in `templates.rs` + 3 integration tests in `branch_templates_test.rs` for T3.7.
- Zero pre-existing test regressions across all six commits.

---

## 11. Witness Mesh v1.0 (scaffold)

**Date shipped:** 2026-05-11 · **LOC:** ~3,700 · **Status:** 🟡 substrate complete; destructive cutover pending

### Scope

The deterministic, byte-grounded replacement for LLM-extracted claims.
A **Witness** is a typed, content-addressed unit derived from primary
bytes via a named rule from a fixed catalog (`docs/superpowers/specs/
2026-05-10-witness-mesh-design.md` §2). The substrate is now functionally
complete and live alongside the legacy `claims` table; the destructive
cutover (delete LLM extraction + Rooting crate + 4-judge tribunal) is the
only remaining work.

### What landed (5 sessions of additive build-out)

**Core types** (`crates/thinkingroot-core/src/types/witness.rs`) —
`Witness`, `WitnessId`, `WitnessInput`, `WitnessSpan`, `WitnessMesh`.
Content-addressed id: `BLAKE3(rule || canonical_cbor(spans))` with
length-prefix collision-safety on both rule name and span list.
11 unit tests pin determinism, hex round-trip, derivation-distinction.

**Rule catalog** (`crates/thinkingroot-extract/src/rule_catalog.rs`) —
**56 rules** in a `phf::Map`, organised across 11 families
(tree-sitter, lsp, rustdoc, jsdoc, javadoc, markdown, cargo-test,
pytest, jest, junit, toml, json, yaml, csv, manifest, comment, git,
code, legacy). Catalog ships with deterministic-TOML serializer
(`rule_catalog_toml()`) whose output is byte-identical across
processes, BLAKE3-hashed for pack-level reproducibility. Grammar
versions pinned at build time from `Cargo.lock` via `build.rs` →
`$OUT_DIR/grammar_versions.rs`. 12 unit tests.

**Mesh assembler** (`crates/thinkingroot-extract/src/witness_mesh.rs`)
— typed-error dedup (UnknownRule, SafetyOrphan, EmptySpans,
EmptyInputs), SAFETY-rule cross-check against parent
`code::unsafe-region` Witnesses, deterministic output order
(witnesses sorted by hex id, edges sorted (parent, child)). 8 unit
tests including order-determinism + collision-safety.

**4 mechanical extractors:**

- `comment_claims.rs` — `@claim` / `@invariant` / `@owns` / `SAFETY:`
  across 7 comment styles (Rust `//` `///` `//!`, Python `#`, SQL
  `--`, multi-line `*`, Lisp/Elixir `;`). File-relative byte offsets
  preserved; content_blake3 over actual span bytes. 11 unit tests.
- `parse_doc_rules.rs` — rustdoc/jsdoc/javadoc doctag adapter +
  markdown heading/paragraph/list-item/code-block emission. 11
  unit tests covering language gating + skipped-on-empty-anchor.
- `test_assertions.rs` — cargo-test / pytest / jest / junit
  assertion miner with framework markers (`#[test]`, `def test_*`,
  `it(`, `@Test`) gating the assertion regex. 11 unit tests
  including cross-framework leakage check.
- `lsp_rules.rs` — rust-analyzer / tsserver / pyright backend
  detection via `$PATH` probing + `lsp::skipped@v1` honest absence
  emission. 12 unit tests. Real LSP subprocess protocol wires in
  Commit 2's `extractor.rs` rewrite.

**Wire format** (`crates/tr-format/src/witness.rs`) — `WitnessRecord`
type with CBOR-canonical sort for `witnesses.cbor` in the upcoming
`tr/3.2` pack format. Drops `stmt` (the LLM paraphrase field) by
design — span text is materialised from `source.tar.zst` at read
time, never duplicated. 8 round-trip + invariant tests.

**Anchor verifier** (`crates/thinkingroot-ground/src/witness_verifier.rs`)
— the surviving piece of the 4-judge tribunal. `verify_witness_anchor`
takes `(byte_start, byte_end, source_bytes, expected_blake3)` and
returns `AnchorVerdict::Verified | Mismatch { expected, actual }`.
~10µs per witness; replaces the 22KB grounder + 17KB NLI ONNX +
3.8KB lexical + 3.8KB semantic with one BLAKE3 comparison. 8 unit
tests.

**CozoDB substrate** (`crates/thinkingroot-graph/src/`):
- `graph.rs::create_schema()` — 2 new tables (`witnesses` 16-col,
  `witness_input_edges` DAG denormalisation) + 6 indexes.
- `witness_inserts.rs` — 8 methods on `GraphStore`: `insert_witness`,
  `insert_witnesses_batch`, `insert_witness_input_edges_batch`,
  `count_witnesses`, `get_witness`, `list_witnesses`,
  `list_witnesses_by_workspace`, `list_witnesses_by_source`.
  Datalog idiom encoded: constrain via `column = $param` predicate,
  not `{column: $param}` binding (Cozo head-symbol rule). 10 unit
  tests against fresh GraphStore instances.

**Pipeline integration** (`crates/thinkingroot-serve/src/pipeline.rs:1098-1162`)
— new Phase 6.45 between source-insert (Phase 6) and Rooting
(Phase 6.5). Reads `filtered_extraction.witnesses` via
`std::mem::take`, runs mesh assembly, batch-inserts via
`insert_witnesses_batch` + `insert_witness_input_edges_batch`.
Mesh-assembly errors are `tracing::warn!`-logged during the
dual-write transition (will become `?`-propagation post-cutover).
Per-source incremental filter preserved (I-W6).

**Migration tool** (`crates/thinkingroot-serve/src/backfill.rs::backfill_witness_mesh*`)
— scans legacy `claims` table, synthesises one `Witness { rule:
"legacy::claim@v1", ... }` per byte-anchored row, batch-inserts,
sets `workspace_meta.witness_schema_version = "2"`. Idempotent
(re-runs are no-ops). Wired into CLI as `root migrate
--to-witness-mesh [--dry-run]`. 4 unit tests including
end-to-end claim → Witness conversion.

**Cross-surface read API:**
- REST (`crates/thinkingroot-serve/src/rest.rs`) — 3 new endpoints:
  `GET /api/v1/ws/{ws}/witnesses?rule=…&limit=N`,
  `GET /api/v1/ws/{ws}/witnesses/{id}`,
  `GET /api/v1/ws/{ws}/witnesses/count`.
- MCP (`crates/thinkingroot-serve/src/mcp/tools.rs`) — `list_witnesses`
  tool with optional `rule` filter. 2 listing tests.
- Rust API (`crates/thinkingroot-serve/src/engine.rs:1253-1289`) —
  `QueryEngine::list_witnesses`, `get_witness`, `count_witnesses`.

### Reference doc

- `.claude/rules/witness-mesh.md` — load-bearing invariants (I-W8
  anchor verification, I-W9 DAG consistency, I-W10 rule pinning,
  I-W11 pack-catalog hash, I-W12 deterministic compile), wire-shape
  decisions, Datalog idiom, and the full Commit-2 remaining-work
  checklist.
- `docs/superpowers/specs/2026-05-10-witness-mesh-design.md` — the
  v1.0 design spec the implementation followed.
- `~/.claude/plans/okey-then-prepare-a-wiggly-reddy.md` — the
  3-commit cutover plan + crate-by-crate file inventory.

### Test scoreboard

- Pre-Witness-Mesh baseline: 1,363 lib tests
- Post-Witness-Mesh scaffold: **1,470 lib tests** (+107 new, 0 failures, 0 regressions)
- Distribution: 11 witness + 12 rule_catalog + 8 witness_mesh + 11
  comment_claims + 11 parse_doc_rules + 11 test_assertions + 12
  lsp_rules + 3 witness_collection_tests + 8 WitnessRecord + 8
  witness_verifier + 10 witness_inserts + 4 backfill_witness_mesh + 2
  mcp tool_listing + 6 from related tests.

### What's still pending (Commit 2 destructive cutover)

The substrate is functionally complete. The remaining work is
**pure subtraction** — removing the code paths the new substrate
has rendered unnecessary:

1. Delete `crates/thinkingroot-rooting/` (entire crate) — admission
   gate obviated by content-addressed Witnesses.
2. Delete 18 LLM-extraction files in `crates/thinkingroot-extract/src/`
   + rewrite `extractor.rs` body to use the rule catalog only.
3. Delete 5 grounding judges in `crates/thinkingroot-ground/`
   (`lexical.rs`, `semantic.rs`, `nli.rs`, `grounder.rs`, `dedup.rs`)
   + remove `pipeline.rs` Phase 2b + Phase 8.
4. Switch ~30 reader sites from `claims` to `witnesses` (synthesizer,
   compressor, reranker, builtin_tools, react, hybrid, intelligence/*,
   compile templates, Desktop UI).
5. Bump `tr-format` to 1.0.0; write `tr/3.2` packs with
   `witnesses.cbor` + `rule_catalog.toml`.
6. Cloud-side one-line path-dep → registry-dep switch in
   `thinkingroot-cloud/Cargo.toml`.

Each step has a coordinated multi-file cascade; the next focused
cutover session lands them as one big-bang commit.

---

## 12. Install + Runtime Smoothness (slices A–F + PATH-fallback hotfix)

**Dates shipped:** 2026-05-11 → 2026-05-13 · **Merge commits:** `4804285 → 6a65c1c → 93b6ba1 → 6ebb2a6 → 08e4d7f → 0deaa52` · **Hotfix:** `11845e2` · **Status:** ✅ shipped

**Source spec:** `docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md`
**Rule (invariants):** `.claude/rules/install-runtime-smoothness.md`

### Scope

6-slice refactor closing the install + daemon-lifecycle failure cluster ("sometimes daemon doesn't work, app silently falls back, onboarding has bugs, CLI fails"). Replaces silent in-process fallback with loud-blocking UI + self-healing daemon.

- **A — install-manifest substrate.** New `crates/thinkingroot-core/src/install_manifest.rs`: atomic JSON at `<config_dir>/thinkingroot/install-manifest.json`, BLAKE3 streaming verify, `register_or_update` with sentinel-locked RMW, reader-bumped `schema_version`. `install.sh` writes `cli-script` entry; desktop's `install_manifest_bridge::register_desktop_bundle` writes `desktop-bundle` entry idempotently on every launch. Hidden `root hash-file <path>` subcommand for install-time BLAKE3.
- **B — `root doctor` substrate.** New `crates/thinkingroot-cli/src/doctor/` module replacing 969-line legacy `doctor_cmd.rs`. 12 commit-locked check IDs (`binary.cli.{installed, on_path, runnable, checksum}`, `config.dir.writable`, `credentials.any_provider`, `daemon.{lockfile.parseable, reachable, restart.exhausted}`, `workspace.{registry.parseable, active.exists}`, `install.manifest.consistent`). `DoctorReport` JSON `schema_version=1`. Four modes (`default | --json | --quiet | --fix [--interactive]`). `root setup` collapses to a 3-line `doctor --fix --interactive` alias; legacy 706-line setup wizard deleted.
- **C — unified `decide()` + cortex.lock crash-safety.** Pure `core::cortex::decide(DecisionInputs) -> Decision` function with 5 variants (`Attach`, `Spawn`, `InProcess`, `Stdio`, `RepairNeeded`). New tiny `thinkingroot-cortex-async` crate (109 LOC) with `probe_livez` shared by CLI + desktop. Both `cortex_client::resolve_engine` (CLI) and `cortex_bridge::resolve_engine` (desktop) call `decide()` — only difference is CLI spawns detached vs desktop returns `SpawnRequired` so sidecar manager owns the `Child`. `serve.rs` flipped to bind → write_lock → mount → accept with RAII `LockfileGuard` (closes the crash-mid-mount race). 14 cortex_scenarios green.
- **D — loud-blocking EngineGate + Tauri event.** New `apps/thinkingroot-desktop/ui/src/components/engine/EngineGate.tsx` top-level wrapper with `EngineStatus` state machine. Watchdog at `agent_runtime_subprocess.rs` emits `engine_status_changed` Tauri event on every status transition (instead of silently clearing `state.sidecar = None`). New `doctor_check` / `doctor_apply_fix` Tauri commands. `THINKINGROOT_FORCE_IN_PROCESS=1` env var as dev escape hatch. CLI's `--fix --json` mode now actually emits JSON (was silently no-op due to if-chain ordering bug — caught + fixed).
- **E — onboarding collapse.** EngineGate gains `variant: 'standard' | 'wizard'` derived from `install-manifest.setup_complete_at == null` + all-failing-check-IDs-are-setup-relevant rule. Wizard auto-marks complete on `engineStatus → 'healthy'` transition (or on Skip-for-now). Deleted: 650-line `OnboardingWizard.tsx`, `onboarding_status` Tauri command, `OnboardingStatus` Rust type, `onboardingDismissed`/`onboardingOpen` store fields. Net `-325 LOC`.
- **F — self-heal.** New `crates/thinkingroot-core/src/restart_state.rs` (397 LOC): exponential backoff (0/500ms/2s/5s) capped at `MAX_ATTEMPTS=4` in 60-second `FAILURE_WINDOW`. Circuit breaker trips on EITHER 4 plain failures OR 3 crash-signal exits (SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGABRT on Unix as negative i32, STATUS_ACCESS_VIOLATION on Windows). `BREAKER_DURATION=5min` auto-clear via runtime check. Unix signal capture via `ExitStatusExt::signal()` → negated i32 (without this the crash-signal cap never fires on Unix). New `crates/thinkingroot-core/src/recovery_log.rs` (305 LOC): append-only JSONL audit at `<config_dir>/thinkingroot/recovery.log` (mode 0600, 10 MiB rotation to `.log.1`). 8 `RecoveryEvent` kinds covering respawn, stale-lock-cleanup, port-advance, manifest-rebuild, breaker trip/reset, binary-checksum-mismatch. Wedged-daemon SIGTERM (via `libc::kill`) + 2s grace polling `process_alive` + SIGKILL escalation before respawn (+ recovery-log entry). 3 new doctor checks. RestartBanner non-blocking overlay during attempts; CircuitBreakerSection with "Reset and try again" button calling `reset_circuit_breaker` Tauri command.
- **Hotfix `11845e2`** — `cortex_client::load_preferred_manifest_binary` and `cortex_bridge::load_preferred_or_extant_binary` fall back to `THINKINGROOT_ROOT_BINARY` env override → `$PATH` lookup when the install manifest is absent. Without this, `cargo install` users (who don't run `install.sh`, which is the only manifest writer for the CLI surface) saw `Decision::RepairNeeded` for every spawn intent even with `root` on `$PATH`.

### Verification

- **218 scoped tests passing** across affected crates: core lib 196 (+12 from this track), cli lib 35, cortex_scenarios 14/14, recovery_log 3, restart_state 10, doctor 32, cortex-async crate 2.
- `cargo check --workspace` clean. `cargo check` on desktop clean (modulo pre-existing `unused_mut` from another feature's WIP).
- `install.sh` smoke test (`tests/install_sh_manifest_smoke.sh`) PASS: writes manifest at sandboxed `XDG_CONFIG_HOME`, mode 0600, all 4 JSON-shape assertions hold.
- `tsc --noEmit` on desktop UI clean.
- Manual end-to-end smoke on macOS: `root doctor` reports 9 ok / 0 warn / 0 fail / 3 skipped (3 skipped because no daemon running + no install manifest yet on the test machine).

### Known residual (predates this work, out of scope)

`crates/thinkingroot-serve/tests/contract_invariants.rs:29` still imports the deleted `thinkingroot_extract::router` module from the Witness Mesh cutover. Blocks full `cargo test --workspace`; doesn't affect runtime. One-line fix at the next cleanup pass.

### Reference

`.claude/rules/install-runtime-smoothness.md` carries the full invariant catalogue — load-bearing for any edit to install.sh, the manifest substrate, the cortex decide() function, the EngineGate UI, the watchdog, or the self-heal flows.

---

## 13. Compile Resilience + AI-Operator Compile

**Date shipped:** 2026-05-14 · **Status:** ✅ shipped · **LOC:** +~1,200 / -380 across 4 crates + 2 UI files

**Source audit:** in-session 13-bug rollup driven by repeated user-reported "compile sometimes works, sometimes queues, sometimes fails, Stop sometimes doesn't" failure modes.
**Rule (invariants):** `.claude/rules/compile-resilience.md`

### Scope

13-bug rollup closing the long-standing compile UX failure cluster + promoting the chat agent to a first-class compile operator. Three coordinated pillars carry the weight, two narrowly-scoped fixes uncovered in passing, and a chat-side UI bridge so AI-triggered compile is visible in the right rail.

- **Pillar 1 — Single canonical compile entry.** `rest.rs::run_unified_compile` (pub(crate)) extracted from `compile_stream`'s 400-LOC inline body. Owns workspace remount, vector-index rebuild, `LlmProbed { Healthy }` stamp, `MountSucceeded` dispatch, terminal `CompileFinished`, and **`EngramManager::invalidate_workspace` when `result.cache_dirty`** (the streaming path silently skipped this — every agent-driven compile prior to today could return AEP probes against GC'd claim ids). `compile_stream` reduces to a ~25-line SSE shim. Post-compile reconciliation lives in one place; the legacy `engine.compile()` arm survives only for stdio MCP clients with no `AppState`.

- **Pillar 2 — AI-operator compile via SSE fast-path.** `mcp/sse.rs::compile_request_fastpath` intercepts `tools/call name="compile"` before normal `mcp::dispatch` runs. Reason: `sse.rs::handle_post` holds `state.engine.read().await` for the dispatch's lifetime; `run_unified_compile` write-locks the engine for remount, which deadlocks against the held read guard. The fast-path resolves the workspace + root_path under a brief read lock, drops it, then calls `run_unified_compile` with `Arc<AppState>`. Response shape matches `mcp_text_result` exactly (pretty-printed JSON in a single text content block). Cancellation flows through a local `CancellationToken::drop_guard()` — when the MCP dispatch future is dropped (chat turn cancel, transport disconnect), the guard fires and the pipeline aborts at the next phase boundary.

- **Pillar 3 — Compile-scoped circuit breaker + auto-retry-once.** `thinkingroot-core::restart_state` schema v1→v2 with `#[serde(default)]` back-compat. New types: `CompileAttempt`, `CompileAttemptOutcome`. New constants: `COMPILE_FAILURE_WINDOW = 5min`, `COMPILE_MAX_ATTEMPTS = 3`, `COMPILE_BREAKER_DURATION = 10min`, `compile_backoff_for_attempt(1) = 1s`. 9 new methods: `prune_compile_attempts`, `recent_compile_failure_count`, `compile_breaker_active`, `compile_should_trip`, `trip_compile_breaker`, `reset_compile_breaker`, `record_compile_failure`, `record_compile_cancellation`, `record_compile_success`. The success recorder purges that workspace's prior `Failed` history (consecutive-failure semantics; flaky-then-recovered providers don't accumulate). Cancellations are logged but don't count toward the breaker. Workspace_compile's Tauri command pre-checks the breaker and returns a loud `Err(...)` rather than queueing. Auto-retry-once fires when first attempt failed (not cancelled), breaker not active, and the cancel token isn't tripped; uses cancel-aware `tokio::select!` over the 1 s backoff sleep + retry await; emits a single user-visible Done/Failed per click. Recovery log records `compile_failed` / `compile_retry_scheduled` / `compile_recovered` / `compile_breaker_tripped` so the doctor surface and recovery-log tail both reflect compile health.

- **Wire fix — dynamic workspace alias.** `resolve_compile_target` looks up `args.target` in `WorkspaceRegistry` and returns the registered name; falls back to `"_"` (the `compile_stream`-recognised placeholder) for path-only inputs. Pre-fix this was hardcoded `"desktop"`, which happened to work because the engine matches by `root_path` regardless of alias — but produced misleading status-actor keys and would break any future per-alias routing.

- **Wire fix — `CompileStatus.running: bool`.** The TypeScript binding in `apps/thinkingroot-desktop/ui/src/lib/tauri.ts:489` has been `running: boolean` since day one. The Rust struct shipped `active: bool`, so every UI poll of `workspace_compile_status` silently returned `running: undefined` (truthy → falsy) and the Right-Rail's pre-flight check was a no-op. Rename closes a real silent bug uncovered while wiring the UI pre-flight.

- **Boot cap — 60 s → 10 s.** `SIDECAR_BOOT_MAX_ATTEMPTS = 20`. Pre-fix justification cited "large NLI ONNX model + first-run fastembed download" — both deps are gone (verified via `crates/thinkingroot-ground/Cargo.toml:17` and `crates/thinkingroot-extract/src/extractor.rs:76`). A healthy sidecar boots in 2–4 s today.

- **Force-clear discipline — `CompileHandle.task: JoinHandle<()>`.** `state.rs::CompileHandle` carries the spawned task's JoinHandle so the 5 s force-clear path can `task.abort()` after the cooperative `cancel.cancel()` failed to propagate. `Clone` derive removed (JoinHandle isn't Clone). Take-then-abort order eliminates the slot-clobber race that pre-fix let the old task overwrite the new compile's slot.

- **Cancel-aware waits everywhere.** Every `tokio::time::sleep` > 500 ms is interleaved with `cancel.cancelled()` via `tokio::select!`. Three pre-fix windows (supersede 5 s, reqwest-retry 10 s, sidecar boot 60 s) ignored Stop clicks; all three honoured now.

- **SSE stall watchdog — `SSE_STALL_WATCHDOG = 60 s`.** Wraps `stream.next()` in `tokio::time::timeout`. Server emits `KeepAlive` SSE comments every 15 s (4× headroom under the watchdog). A wedged stream that's silent that long fails loud rather than blocking the UI forever.

- **Right-Rail polish.** Pre-flight `workspaceCompileStatus()` check before invoking compile — stale-slot mismatches surface as inline warnings, not hard-error toasts. Toast text "Compile queued" → "Compile started" (no queue exists; the slot is exactly one in-flight).

- **Chat-side UI bridge.** `ChatView.tsx::compileToolWorkspace: Map<tool_use_id, workspace>` populated on `tool_call_proposed` (reads `ev.input.workspace`); consumed on `tool_call_executing` (emits synthetic `CompileProgress::Started`), `tool_call_finished` (parses serialized `PipelineResult` from `ev.content`, emits `CompileProgress::Done` or `Failed`), `tool_call_rejected` (emits `CompileProgress::Cancelled`). Cleared on `final` / `error` to bound memory. Start → end visibility only; granular phases need a new chat event type, out of scope for v1.

### Verification

- **`thinkingroot-core` lib: 209 passing** (was 200; +8 compile breaker tests + 4 recovery-event wire-format tests + 1 v1→v2 schema round-trip).
- **`thinkingroot-serve` lib: 467 passing** (zero regressions — the `compile_stream` refactor is mechanically equivalent; every behaviour the inline body had now exists in `finalize_successful_compile`).
- **`thinkingroot-desktop-app` lib: 27 passing** (no regressions across the workspace_compile rewrite + state.rs JoinHandle addition + CompileStatus rename).
- `cargo check` on all touched crates clean.
- `tsc --noEmit` on desktop UI clean.

### Known limit (honest, documented)

The Right-Rail Stop button does not cancel an AI-triggered compile. `workspace_compile_stop` only knows about the desktop's `AppState.active_compile` slot, which is empty for agent-driven runs. The supported way to cancel an AI-driven compile is to abort the chat turn (which drops the MCP transport future → `DropGuard` fires → pipeline aborts at next phase boundary). A future ship can bridge desktop Stop into the chat-turn lifecycle via a Tauri-side abort signal.

### Reference

`.claude/rules/compile-resilience.md` carries the full invariant catalogue — load-bearing for any edit to `workspaces.rs`, `state.rs::CompileHandle`, `rest.rs::run_unified_compile`, `mcp/sse.rs::compile_request_fastpath`, `restart_state.rs::compile_*`, `recovery_log.rs::Compile*`, `ChatView.tsx::compileToolWorkspace`, or `RightRail.tsx::CompilationProgressIndicator`.

---

## 14. Witness Mesh polish cleanup (six-phase post-cutover)

**Date shipped:** 2026-05-14 · **Status:** ✅ shipped (local-only per "don't push without asking") · **LOC:** ~5,200 deleted across the engine workspace · **Commits:** `36bee7b` → `802fa26` → `b7f81e0` → `941fd7a` → `2e9581c` → `02ef2ad` → `88f0f55` (8 on `main`)

**Rule (invariants):** `.claude/rules/witness-mesh.md`

### Scope

Six-phase cleanup that lands the structural residuals of the Witness Mesh cutover (Track 11). Phase 5 deferred — see "Residual debt" in the Witness Mesh rule for the substrate-retargeting work that still gates dropping the legacy `claims` table.

- **Phase 1 (`36bee7b`)** — `SourceByteStore` + `FileSystemSourceStore` moved from `thinkingroot-rooting` → `thinkingroot-graph` (354-line module + 6 unit tests). 9 production import sites switched. Unblocks Phase 6.
- **Phase 2 (`802fa26`)** — New workspace crate `thinkingroot-llm` holds the 8 chat-time LLM files (`llm.rs` 217 KB + `prompts`, `scheduler`, `citation`, `readme`, `graph_context`, `events`, `checkpoint`). 19 consumer sites in `thinkingroot-serve` / `cli` switched. 10 unused deps pruned from `thinkingroot-extract` (`reqwest`, `async-stream`, `aws-sdk-bedrockruntime`, etc). Crate-name `thinkingroot-extract` is now honest — mechanical Witness Mesh extraction only.
- **Phase 3 (`b7f81e0`)** — `extractor.rs` 1,378 → 842 LOC. `Extractor` struct down to **one** field (`min_confidence`) — honest reality, not the pre-cleanup three (`progress` + `cancel` were set by builder methods but never read). `#![allow(dead_code)]` removed. `ExtractionProgressEvent` enum deleted; wire-type `ProgressEvent::ExtractionStart` / `BatchStart` / `ChunkDone` variants kept stable for SSE deserialiser compat (never emitted post-cutover — same retired-but-wire-stable precedent as `GroundingStart` / `GroundingDone`).
- **Phase 4 (`941fd7a` + `2e9581c` + `02ef2ad`)** — Read-side bridge at the graph layer. `GraphStore::get_all_claims_with_sources` and `count_claims_by_admission_tier` fall back to witnesses transparently when the claims table is empty — wire shapes unchanged, downstream consumers (synthesizer / brain UI / REST `/claims`) work without modification. Pre-existing broken `contract_invariants.rs:29` import (the long-standing `thinkingroot_extract::router` reference) fixed. Phase 7e linker bridged. AEP / hybrid / engram readers documented as deferred to Commit-2 cutover (they join `claims` with `admission_tier` / `trial_scores` / `claim_temporal` etc., none populated by witnesses today).
- **Phase 5 — DEFERRED.** Dropping the `claims` table requires retargeting AEP probes (20 queries), hybrid retrieval (11-component fusion), engram cache, plus reworking `branch/merge.rs` claim-merge into witness-merge. ~3-4 weeks; out of session scope. Tracked in `.claude/rules/witness-mesh.md` "What's still pending" section.
- **Phase 6 (`88f0f55`)** — **`thinkingroot-rooting/` crate deleted.** Zero production callers outside the crate after Phase 1. 4,345 LOC removed. Workspace `members` / `default-members` / `workspace.dependencies` entries removed; `[dependencies]` lines removed from `cli` + `serve` + `ground` Cargo.tomls.

Net: ~5,200 LOC deleted across the engine workspace + 1 new crate (`thinkingroot-llm`) + 1 deleted (`thinkingroot-rooting`). Workspace crate count net unchanged.

### Verification

- `cargo check --workspace` — clean
- `cargo build --workspace --exclude thinkingroot-python` — clean
- `cargo test --workspace --lib --no-fail-fast` — **1,425 pass / 0 fail / 5 ignored** (drop from 1,473 baseline = rooting crate's own ~48 unit tests went with the crate, zero new regressions)
- Desktop UI `npm run typecheck` — clean

### Reference

Updated OSS plan `docs/2026-04-27-oss-final-plan.md` §4.4 rewritten — mechanical structural extraction via Witness Mesh rules (1 week of catalog rule families) replaces the heavy-binary-model approach. Total OSS launch effort: **~8-9 weeks** single engineer.

---

## 15. Universal install + auto-update + login-agent

**Date shipped:** 2026-05-14 · **Status:** ✅ shipped (local-only per "don't push without asking") · **LOC:** +~1,800 / -~200 across CLI + desktop + landing + CI

**Source plan:** in-conversation alignment "we need universal and simple plan now — like cursor and windsurf"
**Rule (invariants):** `.claude/rules/universal-install.md`

### Scope

Closes the "user downloads .app from thinkingroot.com → Gatekeeper/SmartScreen wall, daemon doesn't auto-start, no CLI on PATH, no auto-update" failure cluster with the curl-one-liner pattern (Rust / Ollama / Tailscale / uv / bun). Zero recurring fees, no Apple Developer / Microsoft signing certs required in this ship.

- **Slice 1 — Login agent substrate.** New `crates/thinkingroot-cli/src/service.rs` (495 LOC + 2 unit tests): `install()` / `uninstall()` / `print_outcome()` that **actually run** `launchctl bootstrap gui/$UID` (macOS), `systemctl --user enable --now` (Linux), `schtasks /Create /SC ONLOGON` (Windows) — replacing the legacy `serve::install_service` that just `println!`-printed the commands for the user to copy-paste. Typed `ServiceError` propagates loader failures honestly. macOS path uses modern `launchctl bootstrap` with `load -w` as fallback for restricted-TCC shells. New `Commands::Service { Install | Uninstall }` CLI subcommand; `--install-service` flag retained as alias.
- **Slice 2 — install.sh universal.** Extends the existing 557-line installer with `install_desktop_macos`, `install_desktop_linux`, `register_login_agent` helpers. Desktop bundle download is an honest skip (`say_dim` + return 0) when the release doesn't carry it — CLI is fully functional alone. Three opt-out env vars: `TR_SKIP_NLI=1`, `TR_SKIP_APP=1`, `TR_SKIP_SERVICE=1`. `xattr -dr com.apple.quarantine` defensive normalisation on the final .app path.
- **Slice 3 — install.ps1 universal.** New 350-LOC PowerShell mirror. Atomic-move install discipline (`.tr-installing` staging path); user-PATH update via `[Environment]::SetEnvironmentVariable(..., 'User')`; install manifest written with the same shape as install.sh; Task Scheduler ONLOGON registration via `root service install`. Honours the same three skip env vars.
- **Slice 4 — tauri-plugin-updater wired.** New signing keypair at `apps/thinkingroot-desktop/src-tauri/keys/updater.key{,.pub}` (public committed, private gitignored). `Cargo.toml` adds `tauri-plugin-updater = "2"`; `tauri.conf.json` gets `bundle.createUpdaterArtifacts: true` + `plugins.updater` with the pinned pubkey + GitHub Releases `latest.json` endpoint; `capabilities/default.json` gets `updater:default`; new `commands/updater.rs` (launch-time async check + `updater_check_now` invoke handler). Emits `update-installed` event on success.
- **Slice 5 — release.yml CI rewrite.** 3-job pipeline: `build-cli` (5-target matrix: 2× Linux, 2× macOS, 1× Windows) → `build-desktop` (4-platform matrix via `tauri-action@v0`, signed with `TAURI_SIGNING_PRIVATE_KEY` secret) → `release` (CLI checksums.txt + auto-pruning `latest.json` generator + GitHub Release publish to `DevbyNaveen/releases`). Deleted the `homebrew-bump` job per "no channels" direction. `install.sh` + `install.ps1` shipped as release assets so thinkingroot.com can 302-redirect to them.
- **Slice 6 — Landing page surface.** `apps/thinkingroot-landing/src/App.jsx` gets `InstallSection` component with macOS/Linux/Windows tabs + copy-to-clipboard button. `App.css` gets matching styles (`.install-section`, `.install-tabs`, `.install-command-wrapper`, `.install-copy`). Source of truth: three string constants in `INSTALL_COMMANDS = { macos, linux, windows }`. No telemetry, no install-time pings.

**Cleanups landed alongside:**

- `apps/thinkingroot-desktop/src-tauri/tauri.conf.json`: `productName` "ThinkingRoot Desktop" → **"ThinkingRoot"** (no spaces). Window title preserved via `app.windows[0].title`. Removes URL-encoding hazard in 3 separate consumers (install.sh, install.ps1, latest.json generator).
- Deleted `scripts/install.sh` (legacy 3.1 KB stub) and `scripts/install.ps1` (legacy 40-line stub) — release.yml now references the canonical root-level files. No remaining grep hits.
- `serve.rs::install_service` 130-LOC inline plist/unit/script body → 4-LOC delegation shim calling `service::install`.

### Verification (live-tested on this machine)

- `cargo check -p thinkingroot-cli` — clean
- `cargo test -p thinkingroot-cli --bin root service::` — **2 pass / 0 fail** (`service_labels_are_stable`, `outcome_is_clonable`)
- `cargo check` (desktop workspace) — clean (Tauri 2.10.3 + tauri-plugin-updater 2.10.1)
- `pnpm build` (landing page) — clean (Vite 8.0.12, 763ms, 209 KB JS + 14.7 KB CSS gzipped 65 + 3.9 KB)
- `bash -n install.sh` — clean
- `root service install` → wrote `~/Library/LaunchAgents/dev.thinkingroot.plist`, `launchctl bootstrap gui/501` succeeded, `launchctl print gui/501/dev.thinkingroot` confirms `state = running, pid = 90071`
- `root service uninstall` → `launchctl bootout` + plist removed, `launchctl print` confirms service gone, context-correct footer ("Login auto-start is now disabled")

### What requires the human operator (cannot be automated)

1. **2 GitHub Actions secrets:** `TAURI_SIGNING_PRIVATE_KEY` (contents of `apps/thinkingroot-desktop/src-tauri/keys/updater.key`) + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (empty — key has no password).
2. **DNS / redirect** for `thinkingroot.com/install.sh` and `thinkingroot.com/install.ps1` → latest-release download URLs on `DevbyNaveen/releases`.
3. **First tagged release** (`git tag v… && git push --tags`) to exercise the new pipeline end-to-end. First `latest.json` ships then; from then on, auto-update is live for every previously-installed app.

### Intentionally NOT in this ship

(per user direction "no channels"): Apple Developer ID + notarization, Microsoft Authenticode + Azure Trusted Signing, Homebrew tap, winget manifest, Flathub, AUR, Linux system-scoped daemon. Curl-installed binaries skip both Gatekeeper and SmartScreen because the quarantine / zone-internet bit is set by browsers, not by curl / PowerShell.

### Reference

`.claude/rules/universal-install.md` carries the full invariant catalogue — load-bearing for any edit to `install.sh`, `install.ps1`, `service.rs`, `tauri.conf.json::productName`, `release.yml`, `commands/updater.rs`, or `App.jsx::INSTALL_COMMANDS`.

---

## 16. River v1.0 — live merge feedback + symmetric stream-branch creation

**Date shipped:** 2026-05-14 · **Status:** ✅ shipped · **LOC:** small (single function in `mcp/mod.rs`, ~60 lines in `CompileBranchPipeline.tsx`, ~12 lines of CSS)

### Scope

Closes the last two gaps in the desktop's branch river surface. The per-branch row layout in `apps/thinkingroot-desktop/ui/src/components/shell/CompileBranchPipeline.tsx` stays as-shipped — names are more useful than counts at the cardinality real workspaces hit. Two narrow additions plus one engine-side fix:

- **Engine: REST chat auto-creates `stream/{conversation_id}` branches.** `crates/thinkingroot-serve/src/mcp/mod.rs::maybe_auto_create_branch` was MCP-only since T0.6; REST `POST /api/v1/ws/{ws}/ask/stream` never called it, so desktop chat contributions landed on `main` even with `streams.auto_session_branch = true`. Factored the workspace-resolved body into new `pub async fn auto_create_session_branch(workspace, engine, session_id, sessions)` and called it from `rest.rs::agent_stream_response` right after `conversation_id` is computed. The shared helper now uses `entry().or_insert_with(...)` so the SessionContext is minted on-demand — fixes a latent set_branch no-op in both paths when the session hadn't been touched by a tool yet. Idempotent against reconnected sessions (existing branch + set on session is a no-op).
- **UI: persistent diamond `mergeGlyph` at the spine join for merged-tone history rows** (`CompileBranchPipeline.tsx::BranchGraphSvg`). Renders a 4-pixel polygon next to the dot for any node whose tone is `merged`. The diamond outlives the pulse and lets the user re-see where a branch landed days later, without re-reading the row text.
- **UI: transient 800ms pulse ring on SSE `merged` events** (`CompileBranchPipeline.tsx::BranchResolutionRiver`, `apps/thinkingroot-desktop/ui/src/styles/globals.css`). Local `recentMerges: ReadonlySet<string>` state, populated by `extractMergedBranchName(envelope)` when a `branch-event` SSE frame carries `{ kind: "event", branch, event: { kind: "merged", ... } }` (defensive against the single-key serde shape Cozo's broadcast occasionally emits). Timers cleared on workspace switch + unmount; no leak when the user navigates away mid-pulse. New `@keyframes branch-merge-pulse` in `globals.css` — scale 1→2.4, opacity 1→0, `forwards` so it doesn't snap back before the React state evicts.

### Verification

- `cargo check -p thinkingroot-serve` — clean
- UI `tsc --noEmit` — clean

### Reference

Touches `.claude/rules/branch-system.md` (REST + MCP symmetry for `maybe_auto_create_branch`) — when adding new chat entry points, both MCP and REST paths must call `auto_create_session_branch`.
