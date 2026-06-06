# Incremental Compilation

**Date:** 2026-04-10
**Branch:** `phase-3/onboarding-providers`
**Status:** Shipped

---

## What This Is

ThinkingRoot compiles documents into a knowledge graph (claims, entities, relations) and then renders artifacts (entity pages, architecture maps, contradiction reports, etc.). Before this feature, every `root compile` run was a full rebuild — all three expensive stages ran unconditionally regardless of what changed.

This feature makes compilation incremental at four distinct levels. Only the work that is actually necessary is done.

---

## The Problem Before

Three functions performed full rebuilds on every run, even when only one file changed:

| Stage | Old behaviour | Cost |
|-------|--------------|------|
| Entity relation aggregation | `rebuild_entity_relations()` — cleared all `entity_relations` rows, re-aggregated from all `source_entity_relations` | O(all edges) |
| Vector index | `rebuild_vector_index()` — called `vector.reset()`, re-embedded every entity and claim | O(all entities + claims) |
| Artifact compilation | `compile_all()` — recompiled all 8 artifact types for every entity | O(all entities × artifact types) |
| LLM extraction | No cache — every chunk called the LLM on every run | 80–95% of total wall time |

On a repository with 100 files, changing a single file triggered the full cost of all 100.

---

## Four Levels of Incrementality

### Level 1 — Content-hash skip (pre-existing)

Already existed before this work. Each parsed document carries a content hash. At the start of `run_pipeline`, the hash is compared against what is stored in the graph:

```rust
// pipeline.rs:43-49
if existing_sources.len() == 1
    && !doc.content_hash.0.is_empty()
    && existing_sources[0].1 == doc.content_hash.0
{
    skipped += 1;
    continue;
}
```

Unchanged files are excluded from `new_documents` and never reach the LLM, linker, or compiler. The `early_cutoffs` field in `PipelineResult` is set to `skipped` so the CLI can show how many files were skipped.

### Level 2 — Incremental entity relation aggregation

**New in this work.** The `entity_relations` table is an aggregated view: for each `(from_entity, to_entity, relation_type)` triple, it stores the maximum `strength` across all sources that contributed that triple. Before, the only way to update it was a full clear-and-rebuild.

Two new methods on `GraphStore` make it incremental:

**`get_source_relation_triples(source_id: &str) -> Result<Vec<(String, String, String)>>`**
(`graph.rs:438`) — Returns all `(from_id, to_id, relation_type)` triples that a specific source contributed to `source_entity_relations`. Called *before* removing a source so we know which aggregated edges need to be re-evaluated.

**`update_entity_relations_for_triples(triples: &[(String, String, String)]) -> Result<()>`**
(`graph.rs:478`) — For each triple, deletes the stale aggregated edge from `entity_relations`, then queries `source_entity_relations` for the current max strength across all remaining sources. If any source still contributes the triple, re-inserts it with the new max. If no source does, the edge stays deleted.

The pipeline captures affected triples before any removal, then calls the update after:

```rust
// pipeline.rs:52-84 (simplified)
// Before removing a changed/deleted source:
affected_triples.extend(graph.get_source_relation_triples(source_id)?);
graph.remove_source_by_uri(&uri)?;

// After all removals:
affected_triples.sort_unstable();
affected_triples.dedup();        // dedup avoids redundant aggregation queries
graph.update_entity_relations_for_triples(&affected_triples)?;
```

The `dedup()` matters: multiple deleted sources can contribute to the same triple, and the re-aggregation query scans `source_entity_relations` on each call. Deduplication prevents redundant scans.

### Level 3 — Selective artifact compilation

**New in this work.** Two changes combine to make artifact compilation selective:

**`LinkOutput::affected_entity_ids: Vec<String>`** (`linker.rs:28`) — The linker now returns the IDs of every entity that was created or merged during linking. This gives the pipeline an exact list of what changed.

**`Compiler::compile_affected(graph, data_dir, affected_entity_ids, has_changes) -> Result<Vec<Artifact>>`** (`compiler.rs:158`) — Instead of recompiling every entity page, it filters to only the affected set using a `HashSet` for O(1) lookup. Global artifacts (architecture map, contradiction report, decision log, task pack, agent brief, runbook, health report — 7 total) are only regenerated when `has_changes` is `true`.

```rust
// pipeline.rs:177-183
let artifacts = compiler.compile_affected(
    &storage.graph,
    &data_dir,
    &link_output.affected_entity_ids,
    true,  // global artifacts regenerated since new documents were processed
)?;
```

### Level 4 — Content-addressable LLM extraction cache

**New in this work.** This is the highest-impact change because LLM extraction accounts for 80–95% of total compilation time.

**`ExtractionCache`** (`crates/thinkingroot-extract/src/cache.rs`) — A directory-backed key-value store.

- **Key:** `blake3(chunk_content + ":" + PROMPT_VERSION)` where `PROMPT_VERSION = "v1"`. The version suffix means bumping `PROMPT_VERSION` in the constant automatically invalidates the entire cache, forcing a fresh extraction pass after prompt changes.
- **Value:** JSON-serialised `ExtractionResult`, stored as `{data_dir}/cache/extraction/{hash_hex}.json`.
- **Invalidation:** Automatic. If the chunk content changes, its blake3 hash changes, producing a cache miss. No explicit cache management is needed.

The cache integrates via a builder method on `Extractor`:

```rust
// extractor.rs:60
pub fn with_cache_dir(mut self, data_dir: &std::path::Path) -> Self
```

In the extraction loop, cache hits are processed synchronously (no LLM task spawned). Cache misses spawn an LLM task as before, and the result is written to cache after collection:

```rust
// extractor.rs (simplified)
if let Some(cached_result) = cache.get(&content) {
    output.merge(convert(cached_result, ...));
    continue;  // no LLM call
}
// ... spawn LLM task, then after collection:
if let Err(e) = cache.put(&content, &result) {
    tracing::warn!("failed to write extraction cache entry: {e}");
}
```

The cache is enabled in the pipeline with:

```rust
// pipeline.rs:130-132
let extractor = thinkingroot_extract::Extractor::new(&config)
    .await?
    .with_cache_dir(&data_dir);
```

---

## The FingerprintStore (Early Cutoff Infrastructure)

**New in this work.** `FingerprintStore` (`crates/thinkingroot-serve/src/fingerprint.rs`) persists per-source blake3 fingerprints of extraction output to `{data_dir}/fingerprints.json`.

This is Salsa-inspired early cutoff: if a file's content changes (new content hash) but the *extracted knowledge* is identical to the previous run, the downstream pipeline (linking, compilation) can be skipped for that source.

The store has five public methods:

```rust
FingerprintStore::load(data_dir: &Path) -> Self        // load from disk or empty
FingerprintStore::compute(extraction_json: &[u8]) -> String  // blake3 hex of JSON bytes
is_unchanged(&self, uri: &str, new_fingerprint: &str) -> bool
update(&mut self, uri: &str, fingerprint: String)
remove(&mut self, uri: &str)                           // called when source deleted
save(&self) -> Result<()>                              // persist to fingerprints.json
```

**Current status:** The store is created and saved on every run, and fingerprints are removed when a source is deleted. The actual early-cutoff check (comparing fingerprints before dispatching to the linker) is not yet wired into the pipeline — it is infrastructure ready for the next iteration.

---

## Watch Mode

**New in this work.** `root watch <path>` (`crates/thinkingroot-cli/src/watch.rs`) watches a directory for file changes and triggers incremental compilation automatically.

Behaviour:
1. Performs an initial compile on startup.
2. Sets up a `notify-debouncer-mini` watcher with a **300ms debounce window**. Events within 300ms of each other are coalesced into a single compile trigger.
3. On each debounce batch, filters out events inside `.thinkingroot/` using exact path-component matching (not substring matching, which would false-positive on any parent directory whose name contains `.thinkingroot`).
4. Triggers `run_pipeline` for the remaining events, prints timing and stats.

The blocking `mpsc::Receiver::recv()` is moved off the tokio async executor via `tokio::task::spawn_blocking` to avoid occupying a worker thread:

```rust
// watch.rs:53-55
let rx_clone = Arc::clone(&rx);
let recv_result = tokio::task::spawn_blocking(move || rx_clone.lock().unwrap().recv())
    .await?;
```

Watcher errors (permission denied, inotify limits, etc.) are surfaced to stderr with the same `ERR` styling used for compile errors, not silently dropped to a log.

---

## Pipeline Execution Flow

The full pipeline after this work, from `run_pipeline`:

```
Phase 1: Identify potentially_changed (content hash diff — no removals yet)
  ├── For each parsed document:
  │     ├── content_hash unchanged AND already in graph? → skipped += 1
  │     └── changed? → potentially_changed
  └── Deleted sources: in graph but not in filesystem → deleted_sources

Early exit: potentially_changed.is_empty() && deleted_sources.is_empty()
  └── Return immediately, early_cutoffs = skipped

Phase 2: Extract potentially_changed (with LLM cache)
  └── Extractor::new(&config).with_cache_dir(&data_dir)
      extract_all(&potentially_changed, workspace_id)
      [per chunk: cache hit → skip LLM, cache_hits++; miss → LLM → write cache]

Phase 3: Fingerprint check (Salsa-style early cutoff)
  └── Per potentially_changed doc: serialize its extracted claims → blake3 hash
      if fingerprint unchanged → fingerprint_cutoffs++, skip (source stays in graph)
      else → fingerprints.update(), truly_changed.push(doc)

Phase 4: Remove truly_changed + deleted_sources from graph
  └── Capture stale_claim_vector_ids, stale_entity_candidate_ids before removal
      remove_source_by_uri for each; fingerprints.remove for deleted

Phase 5: Incremental entity relation update (removals)
  └── affected_triples.sort_unstable(); .dedup();
      update_entity_relations_for_triples(&affected_triples)

Early exit: truly_changed.is_empty() (fingerprint hits or deletion-only)
  └── Vector update (surgical or full) → compile_affected([], has_any_changes)
      → verify → fingerprints.save → config.save
      early_cutoffs = skipped + fingerprint_cutoffs

Phase 6: Insert sources for truly_changed into graph

Phase 7: Link filtered_extraction (only truly_changed sources)
  └── Linker::link(filtered_extraction) → LinkOutput { affected_entity_ids, added_claim_ids }

Phase 8: Incremental entity relation update (new sources)
  └── get_source_relation_triples per new doc → dedup → update_entity_relations_for_triples

Phase 9: Vector index update
  ├── deleted == 0 → surgical: remove stale IDs, upsert new entity + claim items
  └── deleted > 0 → update_vector_index_full (orphan detection is imprecise with deletions)

Phase 10: Selective compilation
  └── compile_affected(graph, data_dir, &link_output.affected_entity_ids, true)

Phase 11: Verify + persist
  └── verifier.verify → fingerprints.save → config.save
      early_cutoffs = skipped + fingerprint_cutoffs
```

---

## What Changed in Each Crate

| Crate | File | Change |
|-------|------|--------|
| `thinkingroot-graph` | `graph.rs` | Added `get_source_relation_triples`, `update_entity_relations_for_triples` |
| `thinkingroot-graph` | `vector.rs` | Added `remove_by_ids` to both real and no-op `VectorStore` impls |
| `thinkingroot-link` | `linker.rs` | Added `affected_entity_ids: Vec<String>` to `LinkOutput`; populated during entity resolution |
| `thinkingroot-compile` | `compiler.rs` | Added `compile_affected`, `compile_affected_count` |
| `thinkingroot-extract` | `cache.rs` | New file — `ExtractionCache` |
| `thinkingroot-extract` | `extractor.rs` | Added `cache` field, `with_cache_dir` builder, cache integration in `extract_all` |
| `thinkingroot-extract` | `Cargo.toml` | Added `blake3` dependency |
| `thinkingroot-serve` | `fingerprint.rs` | New file — `FingerprintStore` |
| `thinkingroot-serve` | `pipeline.rs` | Full rewrite of `run_pipeline`; renamed `rebuild_vector_index` → `update_vector_index_full`; new `PipelineResult` fields `cache_hits`, `early_cutoffs` |
| `thinkingroot-serve` | `Cargo.toml` | Added `blake3` dependency |
| `thinkingroot-cli` | `watch.rs` | New file — `run_watch` |
| `thinkingroot-cli` | `main.rs` | Added `Watch` subcommand, `mod watch`, CLI output for `cache_hits`/`early_cutoffs` |
| `thinkingroot-cli` | `Cargo.toml` | Added `notify`, `notify-debouncer-mini` dependencies |
| `Cargo.toml` (workspace) | — | Added `notify = "8"`, `notify-debouncer-mini = "0.6"` |

---

## `PipelineResult` Fields

```rust
pub struct PipelineResult {
    pub files_parsed: usize,        // total documents found by parser
    pub claims_count: usize,        // claims extracted in this run (0 if all skipped)
    pub entities_count: usize,      // entities extracted in this run
    pub relations_count: usize,     // relations extracted in this run
    pub contradictions_count: usize,
    pub artifacts_count: usize,
    pub health_score: u8,
    pub cache_hits: usize,          // chunks served from extraction cache (no LLM call)
    pub early_cutoffs: usize,       // files skipped (content-hash skip + fingerprint early cutoff)
}
```

---

## Known Limitations and TODOs

### `cache_hits` and `early_cutoffs` are only printed when > 0

The CLI only prints these lines conditionally:

```rust
if result.cache_hits > 0 { ... }
if result.early_cutoffs > 0 { ... }
```

This is intentional — on a first run or a full-change run there is nothing to report.

---

## Performance Targets

These are design targets from the plan. Actual measurements depend on repository size, LLM provider latency, and machine speed. They are not benchmarked yet.

| Scenario | Before | Target after |
|----------|--------|--------------|
| No changes (all skipped) | Full rebuild (~5s typical) | `< 100ms` (hash check only, early exit) |
| 1 file changed out of 100 | Full rebuild | `~1s` (extract 1 file + incremental link + selective compile) |
| 1 file changed, chunk seen before | Full rebuild | `< 500ms` (extraction cache hit, no LLM call) |
| Watch mode, save one file | Not available | `< 1s` to recompile |

---

## Usage

```bash
# One-shot incremental compile
root ./my-repo

# Watch mode — recompiles on every save
root watch ./my-repo
```

On the second `root ./my-repo` run with no file changes, the pipeline exits at the early-cutoff check and returns almost immediately. On a run where some files changed, only those files are extracted, only their affected entities get new pages, and global artifacts are regenerated once.
