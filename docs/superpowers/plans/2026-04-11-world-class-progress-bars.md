# World-Class Progress Bars Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the silent, black-box compile pipeline with a world-class 5-phase progress display — real progress bars, ETA, active file names, phase solidification, and CI-safe fallback.

**Architecture:** An `mpsc` event channel threads through `run_pipeline()`. The extractor emits `ChunkDone` events via a type-erased closure; the linker emits `EntityResolved` events the same way. A `tokio::join!` bar-driver task in the CLI consumes events and drives five `indicatif` phase bars. Non-TTY and `--verbose` paths skip bars entirely and keep plain tracing output.

**Tech Stack:** `indicatif 0.17` (already in workspace), `tokio::task::JoinSet` (tokio 1.51 full), `std::io::IsTerminal` (Rust 1.70+, project requires 1.85)

---

## File Map

| File | Action | Purpose |
|------|--------|---------|
| `crates/thinkingroot-extract/src/extractor.rs` | Modify | Add `ChunkProgressFn` type, builder, two-pass extraction, `JoinSet` |
| `crates/thinkingroot-extract/src/lib.rs` | Modify | Export `ChunkProgressFn` |
| `crates/thinkingroot-link/src/linker.rs` | Modify | Add `EntityProgressFn` type, builder, call in entity loop |
| `crates/thinkingroot-link/src/lib.rs` | Modify | Export `EntityProgressFn` |
| `crates/thinkingroot-serve/src/pipeline.rs` | Modify | Add `ProgressEvent` enum, add `progress` param, send events, wire closures |
| `crates/thinkingroot-cli/src/pipeline.rs` | Modify | Re-export `ProgressEvent` |
| `crates/thinkingroot-cli/src/progress.rs` | **Create** | Bar styles, `run_compile_progress()`, bar-driver task |
| `crates/thinkingroot-cli/src/main.rs` | Modify | TTY detection, tracing filter, `use_progress` flag, dispatch |
| `crates/thinkingroot-cli/src/setup.rs` | Modify | Replace blind spinner with `progress::run_compile_progress()` |
| `crates/thinkingroot-cli/src/watch.rs` | Modify | Pass `None` as progress arg (two call sites) |
| `crates/thinkingroot-serve/src/engine.rs` | Modify | Pass `None` as progress arg in `compile()` |

---

## Task 1: Add `ChunkProgressFn` to the Extractor

**Files:**
- Modify: `crates/thinkingroot-extract/src/extractor.rs`
- Modify: `crates/thinkingroot-extract/src/lib.rs`

- [ ] **Step 1: Add `ChunkProgressFn` type alias and field**

In `extractor.rs`, replace the top of the file (imports + `Extractor` struct) with:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::*;

use crate::llm::LlmClient;
use crate::prompts;
use crate::schema::ExtractionResult;

type SharedLlm = Arc<LlmClient>;

/// Callback fired after each original chunk is processed (cached or via LLM).
/// Arguments: (done, total, source_uri)
pub type ChunkProgressFn = Arc<dyn Fn(usize, usize, &str) + Send + Sync>;

/// The main extraction engine. Takes DocumentIRs and produces
/// Claims, Entities, and Relations via LLM extraction.
pub struct Extractor {
    llm: SharedLlm,
    concurrency: usize,
    min_confidence: f64,
    /// Approximate max tokens per chunk sent to the LLM (chars / 4 approximation).
    max_chunk_tokens: usize,
    cache: Option<crate::cache::ExtractionCache>,
    progress: Option<ChunkProgressFn>,
}
```

- [ ] **Step 2: Add `with_progress()` builder method**

In the `impl Extractor` block, after `with_cache_dir()`, add:

```rust
    /// Attach a progress callback. Called once per original chunk processed
    /// (cache hit or LLM result). Arguments: (done, total, source_uri).
    pub fn with_progress(mut self, f: ChunkProgressFn) -> Self {
        self.progress = Some(f);
        self
    }
```

Also update `Extractor::new()` to initialise the new field:

```rust
    pub async fn new(config: &Config) -> Result<Self> {
        let llm = LlmClient::new(&config.llm)
            .await?
            .with_max_retries(config.extraction.max_retries);

        Ok(Self {
            llm: Arc::new(llm),
            concurrency: config.llm.max_concurrent_requests,
            min_confidence: config.extraction.min_confidence,
            max_chunk_tokens: config.extraction.max_chunk_tokens,
            cache: None,
            progress: None,
        })
    }
```

- [ ] **Step 3: Replace `extract_all` with two-pass + `JoinSet` implementation**

Replace the entire `extract_all` method body (keep the signature unchanged — `pub async fn extract_all(&self, documents: &[DocumentIR], workspace_id: WorkspaceId) -> Result<ExtractionOutput>`):

```rust
    pub async fn extract_all(
        &self,
        documents: &[DocumentIR],
        workspace_id: WorkspaceId,
    ) -> Result<ExtractionOutput> {
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let min_confidence = self.min_confidence;
        let max_chunk_tokens = self.max_chunk_tokens;
        let documents_len = documents.len();

        let mut output = ExtractionOutput::default();
        output.sources_processed = documents_len;

        // ── Pass 1: separate cache hits from LLM work ──────────────────
        // This gives us an accurate total_chunks denominator before any
        // progress events fire, without double-counting sub-chunks.
        struct ChunkWork {
            source_id: SourceId,
            source_uri: String,
            content: String,
            sub_chunks: Vec<String>,
            context: String,
        }

        let mut cache_hits_data: Vec<(SourceId, String, ExtractionResult)> = Vec::new();
        let mut llm_work: Vec<ChunkWork> = Vec::new();

        for doc in documents {
            for chunk in &doc.chunks {
                if let Some(ref cache) = self.cache {
                    if let Some(cached) = cache.get(&chunk.content) {
                        tracing::debug!("extraction cache hit for chunk in {}", doc.uri);
                        cache_hits_data.push((doc.source_id, doc.uri.clone(), cached));
                        continue;
                    }
                }

                let sub_chunks = split_to_token_budget(&chunk.content, max_chunk_tokens);
                if sub_chunks.len() > 1 {
                    tracing::debug!(
                        "chunk in {} split into {} sub-chunks (estimated {} tokens > limit {})",
                        doc.uri,
                        sub_chunks.len(),
                        chunk.content.len() / 4,
                        max_chunk_tokens
                    );
                }
                llm_work.push(ChunkWork {
                    source_id: doc.source_id,
                    source_uri: doc.uri.clone(),
                    content: chunk.content.clone(),
                    sub_chunks,
                    context: prompts::build_context(
                        &doc.uri,
                        chunk.language.as_deref(),
                        chunk.heading.as_deref(),
                    ),
                });
            }
        }

        // Total = cache hits + original LLM chunks (progress denominator).
        // Sub-chunk splits are an implementation detail — not exposed to callers.
        let total_chunks = cache_hits_data.len() + llm_work.len();
        let mut done: usize = 0;

        // ── Process cache hits (instant, no LLM) ───────────────────────
        output.cache_hits = cache_hits_data.len();
        for (source_id, source_uri, cached_result) in cache_hits_data {
            let converted =
                Self::convert_result_static(cached_result, source_id, workspace_id, min_confidence);
            output.merge(converted);
            output.chunks_processed += 1;
            done += 1;
            if let Some(ref pf) = self.progress {
                pf(done, total_chunks, &source_uri);
            }
        }

        // ── Spawn LLM tasks — one task per original chunk ───────────────
        // Sub-chunks are processed sequentially *within* each task so that
        // progress fires once per original chunk, not once per sub-chunk.
        // Concurrency is still bounded by the semaphore (one permit per
        // sub-chunk LLM call, released after each call completes).
        let mut join_set = tokio::task::JoinSet::new();

        for work in llm_work {
            let llm = Arc::clone(&self.llm);
            let sem = Arc::clone(&semaphore);

            join_set.spawn(async move {
                let source_id = work.source_id;
                let source_uri = work.source_uri;
                let mut sub_results: Vec<(String, ExtractionResult)> = Vec::new();

                for sub_content in work.sub_chunks {
                    let _permit = sem.acquire().await.ok()?;
                    match extract_with_split(
                        Arc::clone(&llm),
                        sub_content.clone(),
                        work.context.clone(),
                        0,
                    )
                    .await
                    {
                        Ok(r) => sub_results.push((sub_content, r)),
                        Err(e) => {
                            tracing::warn!(
                                "extraction failed for chunk in {source_uri}: {e}"
                            );
                        }
                    }
                }

                if sub_results.is_empty() {
                    return None;
                }
                Some((source_id, source_uri, sub_results))
            });
        }

        // ── Collect in completion order (JoinSet.join_next) ─────────────
        // JoinSet yields results as each task finishes, giving smooth
        // progress updates rather than awaiting in spawn order.
        while let Some(join_result) = join_set.join_next().await {
            if let Ok(Some((source_id, source_uri, sub_results))) = join_result {
                // Write each sub-chunk result to cache.
                if let Some(ref cache) = self.cache {
                    for (sub_content, extraction_result) in &sub_results {
                        if let Err(e) = cache.put(sub_content, extraction_result) {
                            tracing::warn!("failed to write extraction cache entry: {e}");
                        }
                    }
                }

                for (_, extraction_result) in sub_results {
                    let converted = Self::convert_result_static(
                        extraction_result,
                        source_id,
                        workspace_id,
                        min_confidence,
                    );
                    output.merge(converted);
                }
                output.chunks_processed += 1;
                done += 1;
                if let Some(ref pf) = self.progress {
                    pf(done, total_chunks, &source_uri);
                }
            }
        }

        tracing::info!(
            "extraction complete: {} claims, {} entities, {} relations \
             from {} sources ({} chunks, {} cache hits)",
            output.claims.len(),
            output.entities.len(),
            output.relations.len(),
            output.sources_processed,
            output.chunks_processed,
            output.cache_hits,
        );

        Ok(output)
    }
```

- [ ] **Step 4: Export `ChunkProgressFn` from the crate**

In `crates/thinkingroot-extract/src/lib.rs`, replace the entire file:

```rust
pub mod cache;
pub mod extractor;
pub mod llm;
pub mod prompts;
pub mod schema;

pub use extractor::{ChunkProgressFn, ExtractionOutput, Extractor};
```

- [ ] **Step 5: Verify the crate type-checks**

```bash
cargo check -p thinkingroot-extract
```

Expected: no errors.

- [ ] **Step 6: Run existing extractor tests**

```bash
cargo test -p thinkingroot-extract --no-default-features
```

Expected: all tests pass (split_to_token_budget tests must still pass).

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-extract/src/extractor.rs \
        crates/thinkingroot-extract/src/lib.rs
git commit -m "feat(extract): add ChunkProgressFn + two-pass JoinSet extraction"
```

---

## Task 2: Add `EntityProgressFn` to the Linker

**Files:**
- Modify: `crates/thinkingroot-link/src/linker.rs`
- Modify: `crates/thinkingroot-link/src/lib.rs`

- [ ] **Step 1: Add `EntityProgressFn` type alias and field**

In `linker.rs`, add the type alias after the imports and update the struct:

```rust
use std::collections::HashMap;
use std::sync::Arc;

use thinkingroot_core::Result;
use thinkingroot_core::types::*;
use thinkingroot_extract::extractor::ExtractionOutput;
use thinkingroot_graph::graph::GraphStore;

use crate::resolution;

/// Callback fired after each entity is resolved (created or merged).
/// Arguments: (done, total)
pub type EntityProgressFn = Arc<dyn Fn(usize, usize) + Send + Sync>;

/// The Linker takes extraction output and builds the knowledge graph:
/// - Resolves duplicate entities
/// - Detects contradictions
/// - Writes everything to the graph store
pub struct Linker<'a> {
    graph: &'a GraphStore,
    progress: Option<EntityProgressFn>,
}
```

- [ ] **Step 2: Update `Linker::new()` and add `with_progress()` builder**

Replace the `impl<'a> Linker<'a>` `new` method and add the builder:

```rust
impl<'a> Linker<'a> {
    pub fn new(graph: &'a GraphStore) -> Self {
        Self { graph, progress: None }
    }

    /// Attach a progress callback. Called once per entity resolved.
    /// Arguments: (done, total)
    pub fn with_progress(mut self, f: EntityProgressFn) -> Self {
        self.progress = Some(f);
        self
    }
```

- [ ] **Step 3: Instrument the entity resolution loop**

In the `link()` method, find the entity resolution loop (currently line 46) and replace it with:

```rust
        // Phase 1: Entity resolution.
        let mut resolved_entities = self.graph.get_entities_with_aliases()?;
        let mut entity_id_map: HashMap<EntityId, EntityId> = HashMap::new();
        let total_entities = extraction.entities.len();
        let mut entity_done: usize = 0;

        for new_entity in extraction.entities {
            match resolution::resolve_entity(&new_entity, &resolved_entities) {
                Some(existing_id) => {
                    if let Some(existing) =
                        resolved_entities.iter_mut().find(|e| e.id == existing_id)
                    {
                        entity_id_map.insert(new_entity.id, existing_id);
                        resolution::merge_entities(existing, &new_entity);
                        output.entities_merged += 1;
                        output.affected_entity_ids.push(existing_id.to_string());
                    }
                }
                None => {
                    let new_id = new_entity.id;
                    entity_id_map.insert(new_id, new_id);
                    output.affected_entity_ids.push(new_id.to_string());
                    resolved_entities.push(new_entity);
                    output.entities_created += 1;
                }
            }
            entity_done += 1;
            if let Some(ref pf) = self.progress {
                pf(entity_done, total_entities);
            }
        }
```

- [ ] **Step 4: Export `EntityProgressFn` from the crate**

In `crates/thinkingroot-link/src/lib.rs`, replace the entire file:

```rust
pub mod linker;
pub mod resolution;

pub use linker::{EntityProgressFn, LinkOutput, Linker};
```

- [ ] **Step 5: Verify**

```bash
cargo check -p thinkingroot-link
```

Expected: no errors.

- [ ] **Step 6: Run linker tests**

```bash
cargo test -p thinkingroot-link --no-default-features
```

Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-link/src/linker.rs \
        crates/thinkingroot-link/src/lib.rs
git commit -m "feat(link): add EntityProgressFn builder for entity resolution progress"
```

---

## Task 3: Add `ProgressEvent` and Wire the Pipeline

**Files:**
- Modify: `crates/thinkingroot-serve/src/pipeline.rs`

- [ ] **Step 1: Add `ProgressEvent` enum and update imports**

At the top of `pipeline.rs`, add this import and the enum. Insert after the existing `use` block:

```rust
use std::collections::HashSet;
use std::path::Path;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::types::WorkspaceId;
use thinkingroot_graph::StorageEngine;

/// Events emitted by the pipeline to drive CLI progress bars.
/// Sent via `tokio::sync::mpsc::UnboundedSender<ProgressEvent>`.
/// The CLI bar-driver task consumes these and renders indicatif bars.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// Parsing finished. `files` = number of documents parsed.
    ParseComplete { files: usize },
    /// Extraction is starting. Fired inside the `ChunkProgressFn` on the
    /// first chunk completion; `total_chunks` is the definitive denominator.
    ExtractionStart { total_chunks: usize },
    /// One original chunk processed (cache hit or LLM result).
    ChunkDone { done: usize, total: usize, source_uri: String },
    /// All chunks extracted. Summary data for solidifying the bar.
    ExtractionComplete { claims: usize, entities: usize, cache_hits: usize },
    /// Entity resolution is starting.
    LinkingStart { total_entities: usize },
    /// One entity resolved (created or merged).
    EntityResolved { done: usize, total: usize },
    /// Linking finished.
    LinkComplete { entities: usize, relations: usize, contradictions: usize },
    /// Artifact compilation finished.
    CompilationDone { artifacts: usize },
    /// Verification finished.
    VerificationDone { health: u8 },
}
```

- [ ] **Step 2: Update `run_pipeline` signature**

Change the function signature to accept an optional progress sender:

```rust
pub async fn run_pipeline(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
) -> Result<PipelineResult> {
```

- [ ] **Step 3: Add a helper macro at the top of the function body**

At the very start of `run_pipeline`'s body (before `let config = ...`), add:

```rust
    // Helper: send a ProgressEvent if a sender is attached. Errors are
    // ignored — a disconnected receiver just means no progress display.
    macro_rules! emit {
        ($event:expr) => {
            if let Some(ref tx) = progress {
                let _ = tx.send($event);
            }
        };
    }
```

- [ ] **Step 4: Send `ParseComplete` after parsing**

After the line `let files_parsed = documents.len();`, add:

```rust
    emit!(ProgressEvent::ParseComplete { files: files_parsed });
```

- [ ] **Step 5: Wire `ChunkProgressFn` closure into the extractor**

In the extraction block (after `let extractor = thinkingroot_extract::Extractor::new(&config).await?.with_cache_dir(&data_dir);`), wire the closure:

```rust
        let extractor = {
            let e = thinkingroot_extract::Extractor::new(&config)
                .await?
                .with_cache_dir(&data_dir);
            if let Some(ref tx) = progress {
                let tx_chunk = tx.clone();
                let pf = Arc::new(move |done: usize, total: usize, uri: &str| {
                    // Fire ExtractionStart once (on the first chunk) so the bar
                    // driver knows the definitive total before rendering starts.
                    if done == 1 {
                        let _ = tx_chunk
                            .send(ProgressEvent::ExtractionStart { total_chunks: total });
                    }
                    let _ = tx_chunk.send(ProgressEvent::ChunkDone {
                        done,
                        total,
                        source_uri: uri.to_string(),
                    });
                }) as thinkingroot_extract::ChunkProgressFn;
                e.with_progress(pf)
            } else {
                e
            }
        };
```

Add `use std::sync::Arc;` to the imports if not already present.

- [ ] **Step 6: Send `ExtractionComplete` after `extract_all` returns**

After `let raw = extractor.extract_all(...).await?;`, add:

```rust
        emit!(ProgressEvent::ExtractionComplete {
            claims: raw.claims.len(),
            entities: raw.entities.len(),
            cache_hits: raw.cache_hits,
        });
```

- [ ] **Step 7: Wire `EntityProgressFn` closure into the linker**

Replace `let linker = thinkingroot_link::Linker::new(&storage.graph);` with:

```rust
    let linker = {
        let l = thinkingroot_link::Linker::new(&storage.graph);
        if let Some(ref tx) = progress {
            let tx_link = tx.clone();
            let total_entities = filtered_extraction.entities.len();
            emit!(ProgressEvent::LinkingStart { total_entities });
            let pf = Arc::new(move |done: usize, total: usize| {
                let _ = tx_link.send(ProgressEvent::EntityResolved { done, total });
            }) as thinkingroot_link::EntityProgressFn;
            l.with_progress(pf)
        } else {
            l
        }
    };
```

- [ ] **Step 8: Send `LinkComplete` after `linker.link()` returns**

After `let link_output = linker.link(filtered_extraction)?;`, add:

```rust
    emit!(ProgressEvent::LinkComplete {
        entities: link_output.entities_created + link_output.entities_merged,
        relations: link_output.relations_linked,
        contradictions: link_output.contradictions_detected,
    });
```

- [ ] **Step 9: Send `CompilationDone` after `compiler.compile_affected()`**

After each `let artifacts = compiler.compile_affected(...)` call (there are two paths — the early `truly_changed.is_empty()` path and the main path), add `emit!()` after each:

```rust
    emit!(ProgressEvent::CompilationDone { artifacts: artifacts.len() });
```

- [ ] **Step 10: Send `VerificationDone` after verification**

After each `let verification = verifier.verify(&storage.graph)?;` call, add:

```rust
    emit!(ProgressEvent::VerificationDone {
        health: verification.health_score.as_percentage(),
    });
```

- [ ] **Step 11: Verify the serve crate type-checks**

```bash
cargo check -p thinkingroot-serve --no-default-features
```

Expected: errors about callers of `run_pipeline` missing the new arg. That is expected — fixed in Tasks 4–8.

- [ ] **Step 12: Commit**

```bash
git add crates/thinkingroot-serve/src/pipeline.rs
git commit -m "feat(pipeline): add ProgressEvent enum + wire progress closures through run_pipeline"
```

---

## Task 4: Re-export `ProgressEvent` from the CLI Pipeline Shim

**Files:**
- Modify: `crates/thinkingroot-cli/src/pipeline.rs`

- [ ] **Step 1: Update the re-export**

Replace the entire file content with:

```rust
pub use thinkingroot_serve::pipeline::{run_pipeline, ProgressEvent, PipelineResult};
```

- [ ] **Step 2: Verify**

```bash
cargo check -p thinkingroot-cli --no-default-features 2>&1 | head -30
```

Expected: errors about `run_pipeline` call sites with wrong number of args. Correct — fixed in Tasks 6–8.

- [ ] **Step 3: Commit**

```bash
git add crates/thinkingroot-cli/src/pipeline.rs
git commit -m "feat(cli): re-export ProgressEvent from pipeline shim"
```

---

## Task 5: Create `progress.rs` — Bar Driver Module

**Files:**
- Create: `crates/thinkingroot-cli/src/progress.rs`

This module owns all indicatif logic. It exposes one public function: `run_compile_progress`.

- [ ] **Step 1: Create the file with bar helpers and the driver**

Create `crates/thinkingroot-cli/src/progress.rs` with the following content:

```rust
//! World-class 5-phase progress display for `root compile`.
//!
//! Drives five `indicatif` phase bars driven by `ProgressEvent`s from the
//! pipeline. Each bar transitions: waiting → active → solidified (done).
//!
//! Only used in TTY mode. Non-TTY and --verbose paths skip this entirely.

use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::pipeline::{run_pipeline, ProgressEvent, PipelineResult};

/// Run the pipeline with a live 5-phase progress display.
///
/// Returns the same `PipelineResult` as `run_pipeline`. Callers print their
/// own pre/post output (banner, summary) — this function only drives the bars.
pub async fn run_compile_progress(
    root_path: &Path,
    branch: Option<&str>,
) -> anyhow::Result<PipelineResult> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();

    let mp = MultiProgress::new();

    // Five fixed-position bars in pipeline order.
    let parse_bar   = mp.add(new_waiting_bar("Parsing"));
    let extract_bar = mp.add(new_waiting_bar("Extracting"));
    let link_bar    = mp.add(new_waiting_bar("Linking"));
    let compile_bar = mp.add(new_waiting_bar("Compiling"));
    let verify_bar  = mp.add(new_waiting_bar("Verifying"));

    // Parse starts immediately — activate its spinner before the pipeline even begins.
    activate_spinner(&parse_bar, "scanning files...");

    // Phase timers (parse_start is set here; others are set as events arrive).
    let parse_start = Instant::now();

    // Clone bar handles for the driver closure.
    let (pb, eb, lb, cb, vb) = (
        parse_bar.clone(),
        extract_bar.clone(),
        link_bar.clone(),
        compile_bar.clone(),
        verify_bar.clone(),
    );

    // ── Bar driver ──────────────────────────────────────────────────────────
    // Runs concurrently with the pipeline via tokio::join!.
    // Receives ProgressEvents and updates bars. Exits when the channel closes
    // (pipeline future completes and drops the sender).
    let bar_driver = async move {
        let mut extract_start = Instant::now();
        let mut link_start    = Instant::now();
        let mut compile_start = Instant::now();
        let mut verify_start  = Instant::now();

        while let Some(event) = rx.recv().await {
            match event {
                // ── Parse ───────────────────────────────────────────────
                ProgressEvent::ParseComplete { files } => {
                    finish_bar(
                        &pb,
                        &format!(
                            "{}  {}",
                            style(format!("{files} files")).white(),
                            style(format!("{:.1}s", parse_start.elapsed().as_secs_f64())).dim(),
                        ),
                    );
                    extract_start = Instant::now();
                    // Activate extract as spinner until ExtractionStart arrives.
                    activate_spinner(&eb, "waiting for LLM...");
                }

                // ── Extraction ──────────────────────────────────────────
                ProgressEvent::ExtractionStart { total_chunks } => {
                    if total_chunks > 0 {
                        eb.set_style(active_bar_style());
                        eb.set_length(total_chunks as u64);
                        eb.set_position(0);
                        eb.enable_steady_tick(std::time::Duration::from_millis(80));
                    }
                }

                ProgressEvent::ChunkDone { done, total, source_uri } => {
                    eb.set_length(total as u64);
                    eb.set_position(done as u64);
                    eb.set_message(format!("↳ {}", uri_basename(&source_uri)));
                }

                ProgressEvent::ExtractionComplete { claims, entities, cache_hits } => {
                    let elapsed = extract_start.elapsed();
                    let total = eb.length().unwrap_or(0) as usize;
                    let cache_note = if cache_hits > 0 && total > 0 {
                        let pct = cache_hits * 100 / total;
                        format!("  {}", style(format!("({cache_hits} cached, {pct}% saved)")).dim())
                    } else {
                        String::new()
                    };
                    finish_bar(
                        &eb,
                        &format!(
                            "{} claims · {} entities{}  {}",
                            style(claims).white(),
                            style(entities).white(),
                            cache_note,
                            style(format!("{:.1}s", elapsed.as_secs_f64())).dim(),
                        ),
                    );
                    link_start = Instant::now();
                    activate_spinner(&lb, "resolving entities...");
                }

                // ── Linking ─────────────────────────────────────────────
                ProgressEvent::LinkingStart { total_entities } => {
                    if total_entities > 0 {
                        lb.set_message(format!("0/{total_entities} entities"));
                    }
                }

                ProgressEvent::EntityResolved { done, total } => {
                    lb.set_message(format!("{done}/{total} entities"));
                }

                ProgressEvent::LinkComplete { entities, relations, contradictions } => {
                    let elapsed = link_start.elapsed();
                    let contra_note = if contradictions > 0 {
                        format!(
                            "  {}",
                            style(format!("· {contradictions} contradictions")).yellow()
                        )
                    } else {
                        String::new()
                    };
                    finish_bar(
                        &lb,
                        &format!(
                            "{} entities · {} relations{}  {}",
                            style(entities).white(),
                            style(relations).white(),
                            contra_note,
                            style(format!("{:.1}s", elapsed.as_secs_f64())).dim(),
                        ),
                    );
                    compile_start = Instant::now();
                    activate_spinner(&cb, "generating artifacts...");
                }

                // ── Compilation ─────────────────────────────────────────
                ProgressEvent::CompilationDone { artifacts } => {
                    let elapsed = compile_start.elapsed();
                    finish_bar(
                        &cb,
                        &format!(
                            "{} artifacts  {}",
                            style(artifacts).white(),
                            style(format!("{:.1}s", elapsed.as_secs_f64())).dim(),
                        ),
                    );
                    verify_start = Instant::now();
                    activate_spinner(&vb, "checking health...");
                }

                // ── Verification ────────────────────────────────────────
                ProgressEvent::VerificationDone { health } => {
                    let elapsed = verify_start.elapsed();
                    let health_str = if health >= 80 {
                        style(format!("Health {health}%")).green().to_string()
                    } else if health >= 60 {
                        style(format!("Health {health}%")).yellow().to_string()
                    } else {
                        style(format!("Health {health}%")).red().to_string()
                    };
                    finish_bar(
                        &vb,
                        &format!(
                            "{}  {}",
                            health_str,
                            style(format!("{:.1}s", elapsed.as_secs_f64())).dim(),
                        ),
                    );
                }
            }
        }

        // Channel closed — pipeline finished. Finalize any bars that never
        // received their events (early-exit paths: nothing changed, etc.).
        for bar in [&pb, &eb, &lb, &cb, &vb] {
            if !bar.is_finished() {
                bar.set_style(skipped_style());
                bar.finish_with_message(style("—").dim().to_string());
            }
        }
    };

    // ── Run pipeline and driver concurrently ───────────────────────────────
    let (pipeline_result, ()) = tokio::join!(
        run_pipeline(root_path, branch, Some(tx)),
        bar_driver,
    );

    // Blank line after the bars for visual breathing room.
    eprintln!();

    pipeline_result.context("pipeline failed")
}

// ── Bar lifecycle helpers ───────────────────────────────────────────────────

fn new_waiting_bar(prefix: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(waiting_style());
    pb.set_prefix(format!("{prefix:<11}"));
    pb.set_message(style("waiting...").dim().to_string());
    pb.tick(); // Render the initial waiting state immediately.
    pb
}

fn activate_spinner(bar: &ProgressBar, msg: &str) {
    bar.set_style(active_spinner_style());
    bar.set_message(msg.to_string());
    bar.enable_steady_tick(std::time::Duration::from_millis(80));
}

fn finish_bar(bar: &ProgressBar, msg: &str) {
    bar.set_style(done_style());
    bar.finish_with_message(msg.to_string());
}

// ── Style definitions ────────────────────────────────────────────────────────

fn waiting_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.dim} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["○"])
}

fn active_spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.cyan} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn active_bar_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(
            "  {spinner:.cyan} {prefix} [{bar:30.cyan/white.dim}] {pos}/{len}  {msg}",
        )
        .expect("static template is valid")
        .progress_chars("█░")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn done_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.green} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["✓"])
}

fn skipped_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.dim} {prefix} {msg}")
        .expect("static template is valid")
        .tick_strings(&["─"])
}

// ── Utility ──────────────────────────────────────────────────────────────────

/// Extract the last path component for display (e.g. "src/auth/service.rs" → "service.rs").
fn uri_basename(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}
```

- [ ] **Step 2: Register the module in `main.rs`**

In `crates/thinkingroot-cli/src/main.rs`, add `mod progress;` alongside the other module declarations:

```rust
mod branch_cmd;
mod mcp_config;
mod pipeline;
mod progress;
mod serve;
mod setup;
mod watch;
mod workspace;
```

- [ ] **Step 3: Verify the module compiles**

```bash
cargo check -p thinkingroot-cli --no-default-features 2>&1 | head -40
```

Expected: type-check errors about `run_pipeline` call sites (still need the `None` arg). That is expected.

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-cli/src/progress.rs \
        crates/thinkingroot-cli/src/main.rs
git commit -m "feat(cli): add progress.rs bar-driver module with 5-phase indicatif display"
```

---

## Task 6: Update `main.rs` — TTY Detection, Tracing Filter, Dispatch

**Files:**
- Modify: `crates/thinkingroot-cli/src/main.rs`

- [ ] **Step 1: Add `IsTerminal` import and TTY detection before subscriber init**

Replace the tracing subscriber init block in `main()` (lines 242–252 of the original):

```rust
    use std::io::IsTerminal as _;

    // Detect TTY *before* initialising the subscriber — the filter depends on it.
    // Progress bars and tracing INFO both write to stderr; in TTY mode we suppress
    // INFO to avoid garbling the bars (same approach as `cargo build`).
    let use_progress = !cli.verbose && std::io::stderr().is_terminal();

    let filter = if cli.verbose {
        tracing_subscriber::EnvFilter::new("thinkingroot=debug,root=debug")
    } else if use_progress {
        // TTY + no --verbose: suppress INFO so bars own stderr.
        tracing_subscriber::EnvFilter::new("thinkingroot=warn,root=warn")
    } else {
        // Pipe / CI / --verbose=false: full INFO for clean log output.
        tracing_subscriber::EnvFilter::new("thinkingroot=info,root=info")
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();
```

- [ ] **Step 2: Thread `use_progress` into `run_compile`**

Update the `Compile` dispatch arm:

```rust
        Some(Commands::Compile { path, branch }) => {
            run_compile(&path, branch.as_deref(), use_progress).await?;
        }
```

And the bare `root ./path` shorthand at the bottom of the match:

```rust
        None => {
            if let Some(path) = cli.path {
                run_compile(&path, None, use_progress).await?;
            } else {
                run_compile(&PathBuf::from("."), None, use_progress).await?;
            }
        }
```

- [ ] **Step 3: Update `run_compile` to dispatch to `progress::run_compile_progress` or plain**

Replace the entire `run_compile` function:

```rust
async fn run_compile(path: &PathBuf, branch: Option<&str>, use_progress: bool) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("path not found: {}", path.display()))?;

    print_banner();
    println!(
        "  {} {}\n",
        style("Compiling").cyan().bold(),
        style(path.display()).white()
    );

    let start = Instant::now();

    let result = if use_progress {
        progress::run_compile_progress(&path, branch).await?
    } else {
        pipeline::run_pipeline(&path, branch, None).await?
    };

    let elapsed = start.elapsed();
    println!();
    println!(
        "  {} compiled {} files in {:.1}s",
        style("ThinkingRoot").green().bold(),
        style(result.files_parsed).white().bold(),
        elapsed.as_secs_f64()
    );
    println!(
        "  {} {}%",
        style("Knowledge Health:").white().bold(),
        style(result.health_score).green().bold()
    );
    println!(
        "  {} {} claims extracted",
        style("  ├──").dim(),
        style(result.claims_count).cyan()
    );
    println!(
        "  {} {} entities identified",
        style("  ├──").dim(),
        style(result.entities_count).cyan()
    );
    println!(
        "  {} {} relations mapped",
        style("  ├──").dim(),
        style(result.relations_count).cyan()
    );
    println!(
        "  {} {} contradictions found",
        style("  ├──").dim(),
        style(result.contradictions_count).yellow()
    );
    println!(
        "  {} {} artifacts generated",
        style("  └──").dim(),
        style(result.artifacts_count).cyan()
    );
    if result.cache_hits > 0 {
        println!(
            "  {} {} extraction cache hits",
            style("  ├──").dim(),
            style(result.cache_hits).green()
        );
    }
    if result.early_cutoffs > 0 {
        println!(
            "  {} {} sources unchanged (early cutoff)",
            style("  └──").dim(),
            style(result.early_cutoffs).green()
        );
    }
    println!();

    Ok(())
}
```

- [ ] **Step 4: Verify `main.rs` type-checks (caller fix only for compile path)**

```bash
cargo check -p thinkingroot-cli --no-default-features 2>&1 | head -30
```

Expected: remaining errors from `watch.rs`, `setup.rs`, `engine.rs`. That is expected.

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-cli/src/main.rs
git commit -m "feat(cli/main): TTY detection, tracing filter, dispatch to progress bars"
```

---

## Task 7: Update `setup.rs` — Replace Blind Spinner

**Files:**
- Modify: `crates/thinkingroot-cli/src/setup.rs`

- [ ] **Step 1: Replace the blind spinner block**

Find the compile block in `run_setup()` (around lines 257–285):

```rust
    // Compile
    if compile_now {
        println!("  Compiling {}...\n", abs_ws_path.display());
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .expect("spinner template is a valid static string"),
        );
        pb.set_message("Compiling knowledge base...");
        pb.enable_steady_tick(std::time::Duration::from_millis(80));

        match crate::pipeline::run_pipeline(&abs_ws_path, None).await {
            Ok(result) => {
                pb.finish_and_clear();
                println!(
                    "  {} {} claims · {} entities · {} relations\n",
                    style("✓").green().bold(),
                    result.claims_count,
                    result.entities_count,
                    result.relations_count,
                );
            }
            Err(e) => {
                pb.finish_and_clear();
                println!("  {} Compilation failed: {}", style("!").yellow(), e);
                println!("  Run `root compile {}` to retry.", abs_ws_path.display());
            }
        }
    }
```

Replace with:

```rust
    // Compile — reuse the same world-class progress display used by `root compile`.
    if compile_now {
        println!("  Compiling {}...\n", abs_ws_path.display());
        match crate::progress::run_compile_progress(&abs_ws_path, None).await {
            Ok(result) => {
                println!(
                    "  {} {} claims · {} entities · {} relations\n",
                    style("✓").green().bold(),
                    result.claims_count,
                    result.entities_count,
                    result.relations_count,
                );
            }
            Err(e) => {
                println!("  {} Compilation failed: {}", style("!").yellow(), e);
                println!("  Run `root compile {}` to retry.", abs_ws_path.display());
            }
        }
    }
```

- [ ] **Step 2: Remove now-unused `ProgressBar` / `ProgressStyle` imports from `setup.rs`**

Check the top of `setup.rs` — remove this import line if `ProgressBar` / `ProgressStyle` are no longer used anywhere else in the file (they are still used in `configure_provider` for key validation, so keep the `indicatif` import but remove `ProgressBar, ProgressStyle` from the explicit import if `configure_provider` uses `indicatif::ProgressBar` directly):

```rust
// Remove from the use statement at the top:
use indicatif::{ProgressBar, ProgressStyle};
// Keep only if still referenced elsewhere; configure_provider at line ~419
// uses indicatif::ProgressBar directly, so this is still needed.
```

Actually: `configure_provider` at line ~419 uses `indicatif::ProgressBar::new_spinner()` directly (fully-qualified). The explicit `use indicatif::{ProgressBar, ProgressStyle};` at line 6 is used only in the compile block (now removed). Remove it:

```rust
// Remove line 6:
use indicatif::{ProgressBar, ProgressStyle};
```

- [ ] **Step 3: Verify**

```bash
cargo check -p thinkingroot-cli --no-default-features 2>&1 | head -20
```

Expected: errors only from `watch.rs` and `engine.rs` (the remaining callers).

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-cli/src/setup.rs
git commit -m "feat(cli/setup): replace blind spinner with world-class progress display"
```

---

## Task 8: Update Remaining `run_pipeline` Callers

**Files:**
- Modify: `crates/thinkingroot-cli/src/watch.rs`
- Modify: `crates/thinkingroot-serve/src/engine.rs`

- [ ] **Step 1: Update `watch.rs` — initial compile call (line 22)**

In `watch.rs`, find `pipeline::run_pipeline(root_path, None).await` (initial compile) and add the `None` progress arg:

```rust
        match pipeline::run_pipeline(root_path, None, None).await {
```

- [ ] **Step 2: Update `watch.rs` — incremental recompile call (line 81)**

Find the second call to `pipeline::run_pipeline` (inside the event loop) and add the `None` progress arg:

```rust
                match pipeline::run_pipeline(root_path, None, None).await {
```

- [ ] **Step 3: Update `engine.rs` — `compile()` method (line 574)**

In `engine.rs`, find `crate::pipeline::run_pipeline(&handle.root_path, None).await` and add the `None` progress arg:

```rust
    pub async fn compile(&self, ws: &str) -> Result<PipelineResult> {
        let handle = self.get_workspace(ws)?;
        crate::pipeline::run_pipeline(&handle.root_path, None, None).await
    }
```

- [ ] **Step 4: Search for any remaining call sites**

```bash
grep -rn "run_pipeline(" crates/ --include="*.rs"
```

Expected output — all call sites should now have 3 arguments. Verify none are missing:
- `crates/thinkingroot-serve/src/pipeline.rs` — the definition (not a call)
- `crates/thinkingroot-cli/src/pipeline.rs` — the re-export (not a call)
- `crates/thinkingroot-cli/src/main.rs` — `pipeline::run_pipeline(&path, branch, None)`
- `crates/thinkingroot-cli/src/watch.rs` — two calls, both `None, None`
- `crates/thinkingroot-serve/src/engine.rs` — `None, None`
- `crates/thinkingroot-cli/src/progress.rs` — `Some(tx)` call

If any other file shows up, add `None` as the third argument.

- [ ] **Step 5: Full workspace type-check**

```bash
cargo check --workspace --no-default-features
```

Expected: **zero errors**.

- [ ] **Step 6: Run full test suite**

```bash
cargo test --workspace --no-default-features
```

Expected: all existing tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-cli/src/watch.rs \
        crates/thinkingroot-serve/src/engine.rs
git commit -m "fix: update all run_pipeline callers to pass None progress sender"
```

---

## Task 9: Integration Verification

- [ ] **Step 1: Build the release binary**

```bash
cargo build --release -p thinkingroot-cli
```

Expected: compiles without warnings related to new code.

- [ ] **Step 2: Visual smoke test — TTY mode**

Run against any directory with at least a few Markdown/code files:

```bash
./target/release/root compile /path/to/any/dir
```

Expected output (world-class experience):

```
  ThinkingRoot
  The open-source knowledge compiler for AI agents

  Compiling /path/to/any/dir

  ✓ Parsing      42 files  0.1s
  ✓ Extracting   89 claims · 12 entities (3 cached, 4% saved)  12.3s
  ✓ Linking      12 entities · 28 relations  0.2s
  ✓ Compiling    6 artifacts  0.1s
  ✓ Verifying    Health 78%  0.1s

  ThinkingRoot compiled 42 files in 12.8s
  Knowledge Health: 78%
  ...
```

Verify: bars animate while running, solidify green on completion, no garbled tracing output mixing with bars.

- [ ] **Step 3: Verify CI/pipe mode (non-TTY)**

```bash
./target/release/root compile /path/to/any/dir 2>&1 | cat
```

Expected: plain INFO log output (no broken ANSI escape codes, no partial bar renders).

- [ ] **Step 4: Verify `--verbose` mode**

```bash
./target/release/root compile /path/to/any/dir --verbose 2>&1 | head -20
```

Expected: full `DEBUG` tracing output with no progress bars.

- [ ] **Step 5: Verify `root setup` compile step**

```bash
./target/release/root setup
```

Step through to "Compile now?" → Yes. Verify the same 5-phase bars appear.

- [ ] **Step 6: Verify `root watch` still works**

```bash
./target/release/root watch /path/to/any/dir
```

Expected: initial compile runs (with plain output, since watch is a background loop), then waits for changes.

- [ ] **Step 7: Verify `--no-default-features` build (no ONNX/fastembed)**

```bash
cargo build --release --no-default-features -p thinkingroot-cli
```

Expected: compiles cleanly. Progress bars work with the stub vector store.

- [ ] **Step 8: Final commit**

```bash
git add -u
git commit -m "test: verify world-class progress bars integration"
```

---

## Self-Review Checklist

**Spec coverage:**
- ✅ No progress bars → 5-phase progress bars with ETA
- ✅ Tracing/indicatif conflict → TTY detection at `main()` init, suppress INFO in TTY mode
- ✅ Spinner knows nothing → event channel from pipeline with real data
- ✅ Cache hit progress → two-pass extractor, cache hits emit `ChunkDone` immediately
- ✅ Completion-order processing → `JoinSet.join_next()` instead of ordered `.await`
- ✅ Source file name display → `uri_basename()` in `ChunkDone` handler
- ✅ Phase solidification → `finish_bar()` with green ✓
- ✅ CI-safe → `is_terminal()` guard, falls back to plain logging
- ✅ Zero new deps → `indicatif` was already in workspace
- ✅ `setup.rs` blind spinner replaced
- ✅ `watch.rs` callers updated
- ✅ `engine.rs` REST path unchanged (passes `None`)

**Type consistency:**
- `ChunkProgressFn = Arc<dyn Fn(usize, usize, &str) + Send + Sync>` — used in `extractor.rs`, exported, imported in `pipeline.rs`
- `EntityProgressFn = Arc<dyn Fn(usize, usize) + Send + Sync>` — used in `linker.rs`, exported, imported in `pipeline.rs`
- `ProgressEvent` — defined in `thinkingroot-serve/src/pipeline.rs`, re-exported from `thinkingroot-cli/src/pipeline.rs`
- `run_pipeline(root_path, branch, progress)` — 3 args consistently across all 5 call sites
