use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::*;

use crate::llm::LlmClient;
use crate::prompts;
use crate::scheduler::ThroughputScheduler;
use crate::schema::ExtractionResult;

/// Fallback batch size used only in tests that construct Extractor directly.
/// Production code always uses the dynamically computed value from `model_batch_size`.
pub const EXTRACTION_BATCH_SIZE: usize = 6;

type SharedLlm = Arc<LlmClient>;

/// Progress events emitted by the extractor so the CLI can render a live,
/// batch-aware extraction bar without compromising batch size.
#[derive(Debug, Clone)]
pub enum ExtractionProgressEvent {
    /// Extraction is ready to begin.
    Start {
        total_chunks: usize,
        batch_size: usize,
        total_batches: usize,
    },
    /// A new LLM batch has started running.
    BatchStart {
        batch_index: usize,
        total_batches: usize,
        range_start: usize,
        range_end: usize,
        batch_chunks: usize,
    },
    /// One original chunk finished (cache hit or completed LLM batch result).
    ChunkDone {
        done: usize,
        total: usize,
        source_uri: String,
    },
}

/// Callback fired for extractor progress updates.
pub type ChunkProgressFn = Arc<dyn Fn(ExtractionProgressEvent) + Send + Sync>;

/// Metadata that flows alongside each spawned batch's success result.
/// Captured at spawn time so the collect loop can record the batch in
/// the in-flight checkpoint log without re-deriving the batch's
/// position.  `batch_idx` mirrors the 0-indexed slot used by
/// `llm_work.chunks(batch_size).enumerate()`; the `range_*` fields
/// are 1-indexed inclusive chunk numbers (same vocabulary as
/// `ProgressEvent::ExtractionBatchStart`).
#[derive(Debug, Clone, Copy)]
struct BatchMeta {
    batch_idx: usize,
    range_start: usize,
    range_end: usize,
    batch_chunks: usize,
}

/// The main extraction engine. Takes DocumentIRs and produces
/// Claims, Entities, and Relations via LLM extraction.
pub struct Extractor {
    llm: SharedLlm,
    concurrency: usize,
    min_confidence: f64,
    /// Approximate max tokens per chunk sent to the LLM (chars / 4 approximation).
    max_chunk_tokens: usize,
    /// Number of cache-miss chunks packed into a single LLM batch call.
    /// Computed from model context window + output cap at construction time;
    /// overridable via `extraction_batch_size` in config.
    ///
    /// Wedge 1: this is the *upper bound* on chunks per batch — the actual
    /// batch size is variable and is decided by the token-aware packer
    /// (`pack_batches`) which seals a batch when either this chunk-count cap
    /// or `input_token_budget` is reached, whichever hits first.
    batch_size: usize,
    /// Wedge 1: per-call input-token budget for the variable-size batch
    /// packer.  Resolved from `ExtractionConfig::extraction_input_token_budget`
    /// or, when unset, from `model_input_token_budget(provider, model)`.
    input_token_budget: usize,
    cache: Option<crate::cache::ExtractionCache>,
    progress: Option<ChunkProgressFn>,
    /// Known entities from the existing graph, injected into LLM prompts.
    known_entities: crate::graph_context::GraphPrimedContext,
    /// When `true` (default), structural-classified chunks ALSO go to
    /// the LLM for semantic extraction.  Set to `false` for code-heavy
    /// repos where the LLM rarely adds value over the structural
    /// metadata.  Sourced from `config.extraction.structural_plus_llm`.
    structural_plus_llm: bool,
    /// Cancellation token consulted between batches in `extract_all`.
    /// `None` (the default) means cancellation is opt-out — existing
    /// callers retain pre-fix behaviour.  The pipeline orchestrator
    /// installs one via `with_cancel` so the desktop's Stop button can
    /// trip every in-flight LLM call.
    cancel: Option<CancellationToken>,
    /// Per-batch checkpoint log used to skip already-completed batches
    /// on resume.  None = no checkpointing (existing test callers).
    /// The pipeline installs one via `with_checkpoint` so a killed
    /// compile resumes from the last completed batch.
    checkpoint: Option<Arc<crate::checkpoint::InFlightCheckpoint>>,
    /// Snapshot of batches already completed in a previous run, loaded
    /// once at construction time.  Used by `extract_all` to short-
    /// circuit the spawn step for batches whose claims are already
    /// in the per-chunk content cache.
    completed_batches: crate::checkpoint::CompletedBatches,
}

/// The combined output of extraction across all documents.
#[derive(Debug, Default)]
pub struct ExtractionOutput {
    pub claims: Vec<Claim>,
    pub entities: Vec<Entity>,
    pub relations: Vec<SourcedRelation>,
    /// Maps ClaimId → entity names that the claim references.
    /// Used by the Linker to create claim→entity edges.
    pub claim_entity_names: HashMap<ClaimId, Vec<String>>,
    pub sources_processed: usize,
    pub chunks_processed: usize,
    /// Chunks served from the content-addressable extraction cache (no LLM call made).
    pub cache_hits: usize,
    /// Chunks extracted via structural (Tier 0) extraction — no LLM call made.
    pub structural_extractions: usize,
    /// Maps SourceId → the raw source text that was sent to the LLM.
    /// Used by the grounding system to verify claims against source.
    pub source_texts: HashMap<SourceId, String>,
    /// Maps ClaimId → the LLM's cited source_quote for that claim.
    /// Used by Judge 2 (span attribution) in the grounding system.
    pub claim_source_quotes: HashMap<ClaimId, String>,
    /// Number of LLM batches that exhausted retries and produced no
    /// claims.  Pre-fix these failures were silently dropped: the
    /// orchestrator reported "extraction complete" with claims missing.
    /// Surfaced to callers so the CLI / desktop can render a partial-
    /// failure warning, and so the next compile knows which chunks to
    /// re-target.
    pub failed_batches: usize,
    /// `(range_start, range_end)` chunk-index ranges (1-indexed,
    /// inclusive) of every batch that failed permanently.  Mirrors the
    /// shape of `ProgressEvent::ExtractionBatchStart::range_*` so a
    /// single user-visible vocabulary describes both states.  Empty
    /// when `failed_batches == 0`.
    pub failed_batch_ranges: Vec<(usize, usize)>,
    // ─── Compile Completeness Contract §5 — decorations carried to Phase 6.7
    /// Per-claim quantity rows extracted from the claim's statement.
    /// Phase 6.7 emits one `quantities` table row per entry. Empty when
    /// no numerics were detected. Populated by
    /// `crate::quantity::extract` during `convert_result_static`.
    pub claim_quantities:
        HashMap<ClaimId, Vec<crate::schema::ExtractedQuantity>>,
    /// Per-claim expiration signal + ISO-8601 absolute expiration date.
    /// Phase 6.7 writes the date into `claim_temporal.valid_until` and
    /// preserves the typed signal in a future `claim_expiration_signals`
    /// row. Populated by `crate::expiration::extract` during
    /// `convert_result_static`. `None` when no expiration phrasing was
    /// found.
    pub claim_expirations:
        HashMap<ClaimId, crate::expiration::ExtractedExpiration>,
}

#[derive(Debug, Clone)]
pub struct SourcedRelation {
    pub source: SourceId,
    pub relation: Relation,
}

impl Extractor {
    pub async fn new(config: &Config) -> Result<Self> {
        let scheduler = ThroughputScheduler::new(config.llm.max_concurrent_requests);
        let llm = LlmClient::new(&config.llm)
            .await?
            .with_max_retries(config.extraction.max_retries)
            .with_scheduler(Arc::clone(&scheduler));

        // Compute batch size: config override wins, otherwise auto-detect from model.
        let batch_size = config.extraction.extraction_batch_size.unwrap_or_else(|| {
            crate::llm::model_batch_size(
                &config.llm.default_provider,
                &config.llm.extraction_model,
                config.extraction.max_chunk_tokens,
            )
        });

        // Wedge 1: resolve the input-token budget driving the variable-size packer.
        let input_token_budget = config
            .extraction
            .extraction_input_token_budget
            .unwrap_or_else(|| {
                crate::llm::model_input_token_budget(
                    &config.llm.default_provider,
                    &config.llm.extraction_model,
                )
            });

        tracing::info!(
            "extraction batch caps: chunks_max={} input_tokens_max={} (provider={}, model={})",
            batch_size,
            input_token_budget,
            config.llm.default_provider,
            config.llm.extraction_model,
        );

        Ok(Self {
            llm: Arc::new(llm),
            concurrency: config.llm.max_concurrent_requests,
            min_confidence: config.extraction.min_confidence,
            max_chunk_tokens: config.extraction.max_chunk_tokens,
            batch_size,
            input_token_budget,
            cache: None,
            progress: None,
            known_entities: crate::graph_context::GraphPrimedContext::new(Vec::new()),
            structural_plus_llm: config.extraction.structural_plus_llm,
            cancel: None,
            checkpoint: None,
            completed_batches: crate::checkpoint::CompletedBatches::default(),
        })
    }

    /// Install an in-flight checkpoint log under `<data_dir>` so a
    /// killed compile resumes from the last completed batch instead
    /// of reissuing every cache-miss.  Loads any existing log up-front
    /// — if the file is malformed we err out rather than silently
    /// accepting a corrupt resume.
    ///
    /// The orchestrator clears the log via
    /// `InFlightCheckpoint::clear(data_dir)` after Phase 7 succeeds —
    /// at that point CozoDB is the source of truth.
    pub fn with_checkpoint(mut self, data_dir: &std::path::Path) -> Result<Self> {
        let completed = crate::checkpoint::InFlightCheckpoint::load_completed_batches(data_dir)?;
        if !completed.is_empty() {
            tracing::info!(
                completed_batches = completed.batches.len(),
                already_done_chunks = completed.chunks_already_done,
                "resuming from in-flight checkpoint"
            );
        }
        let ckpt = crate::checkpoint::InFlightCheckpoint::open(data_dir)?;
        self.checkpoint = Some(Arc::new(ckpt));
        self.completed_batches = completed;
        Ok(self)
    }

    /// Install a cancellation token consulted between extraction batches.
    /// When the token is tripped, in-flight tasks are aborted and
    /// `extract_all` returns `Err(Error::Cancelled)`.  Already-completed
    /// batches are NOT lost — their results are retained in the partial
    /// `ExtractionOutput` accessible to the caller via the checkpoint
    /// (introduced by C6 in the same fix series).
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Enable the content-addressable extraction cache stored at
    /// `{data_dir}/cache/extraction/`.
    pub fn with_cache_dir(mut self, data_dir: &std::path::Path) -> Self {
        match crate::cache::ExtractionCache::new(data_dir) {
            Ok(cache) => {
                tracing::info!("extraction cache enabled ({} entries)", cache.len());
                self.cache = Some(cache);
            }
            Err(e) => {
                tracing::warn!("extraction cache disabled (failed to init): {e}");
            }
        }
        self
    }

    /// Attach a progress callback. Called once per original chunk processed
    /// (cache hit or LLM result). Arguments: (done, total, source_uri).
    pub fn with_progress(mut self, f: ChunkProgressFn) -> Self {
        self.progress = Some(f);
        self
    }

    /// Inject known entities from the existing knowledge graph into LLM prompts.
    pub fn with_known_entities(mut self, ctx: crate::graph_context::GraphPrimedContext) -> Self {
        tracing::info!(
            "graph-primed context: {} known entities",
            ctx.entities.len()
        );
        self.known_entities = ctx;
        self
    }

    /// Extract knowledge from a batch of documents — all chunks run concurrently.
    pub async fn extract_all(
        &self,
        documents: &[DocumentIR],
        workspace_id: WorkspaceId,
    ) -> Result<ExtractionOutput> {
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let min_confidence = self.min_confidence;
        let max_chunk_tokens = self.max_chunk_tokens;
        let documents_len = documents.len();

        let mut output = ExtractionOutput {
            sources_processed: documents_len,
            ..Default::default()
        };

        // Build source text map from all documents (for grounding).
        for doc in documents {
            let text: String = doc
                .chunks
                .iter()
                .map(|c| c.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            output.source_texts.insert(doc.source_id, text);
        }

        // ── Pass 1: separate cache hits from LLM work ──────────────────
        // This gives us an accurate total_chunks denominator before any
        // progress events fire, without double-counting sub-chunks.
        #[derive(Clone)]
        struct ChunkWork {
            source_id: SourceId,
            source_uri: String,
            /// The original full chunk content — used as the cache key after
            /// all sub-chunks are processed, so split chunks are cached under
            /// their original key and hit on subsequent runs.
            original_content: String,
            sub_chunks: Vec<String>,
            context: String,
            /// AST-extracted anchor section injected into the LLM prompt.
            /// Empty string when the chunk has no AST metadata (prose, headings, etc.).
            ast_anchor: String,
            /// Byte offsets of the originating chunk within its source file.
            /// Backfilled onto every ExtractedClaim coming back from the LLM
            /// so v3 packs always cite a verifiable byte range. (0, 0) is
            /// the "parser hasn't been upgraded" sentinel — claims keep
            /// (0, 0) and downstream consumers fall back to file scope.
            chunk_byte_start: u64,
            chunk_byte_end: u64,
        }

        let mut cache_hits_data: Vec<(SourceId, String, u64, u64, ExtractionResult)> = Vec::new();
        let mut llm_work: Vec<ChunkWork> = Vec::new();
        let mut structural_results: Vec<(SourceId, String, ExtractionResult)> = Vec::new();

        for doc in documents {
            for chunk in &doc.chunks {
                // ── Tier Router: structural or LLM? ──
                let is_structural =
                    crate::router::classify(chunk) == crate::router::Tier::Structural;
                if is_structural {
                    let result = crate::structural::extract_structural(chunk, &doc.uri);
                    let produced = !result.claims.is_empty()
                        || !result.entities.is_empty()
                        || !result.relations.is_empty();
                    if produced {
                        structural_results.push((doc.source_id, doc.uri.clone(), result));
                    }
                    // M4: when `structural_plus_llm` is disabled, code-heavy
                    // chunks skip the LLM entirely once the structural pass
                    // produced something useful.  Default-true preserves
                    // pre-fix behaviour (additive structural + LLM).
                    if produced && !self.structural_plus_llm {
                        continue;
                    }
                }

                if let Some(ref cache) = self.cache
                    && let Some(cached) = cache.get(&chunk.content)
                {
                    tracing::debug!("extraction cache hit for chunk in {}", doc.uri);
                    cache_hits_data.push((
                        doc.source_id,
                        doc.uri.clone(),
                        chunk.byte_start,
                        chunk.byte_end,
                        cached,
                    ));
                    continue;
                }

                let sub_chunks = split_chunk_to_token_budget(chunk, max_chunk_tokens);
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
                    original_content: chunk.content.clone(),
                    sub_chunks,
                    context: prompts::build_context(
                        &doc.uri,
                        chunk.language.as_deref(),
                        chunk.heading.as_deref(),
                    ),
                    ast_anchor: prompts::build_ast_anchor_section(&chunk.metadata),
                    chunk_byte_start: chunk.byte_start,
                    chunk_byte_end: chunk.byte_end,
                });
            }
        }

        // Total = number of original chunks across all documents.
        // Each chunk fires one progress event from the LLM path (cache hit or LLM task).
        // Structural results are additive — they run in addition to the LLM path, not instead.
        let cache_hits_count = cache_hits_data.len();
        let total_chunks = cache_hits_count + llm_work.len();

        // Wedge 1: pack llm_work into variable-size batches honouring both
        // the chunk-count cap (`self.batch_size`) and the input-token budget
        // (`self.input_token_budget`).  Each packed range is `(start, end)`
        // half-open into `llm_work`.  Cost = chars/4 of the body the LLM
        // actually sees: sub_chunks joined + ast_anchor + context.
        let packed_ranges: Vec<(usize, usize)> = pack_batches(
            &llm_work,
            self.input_token_budget,
            self.batch_size,
            |w: &ChunkWork| {
                let body_chars: usize = w.sub_chunks.iter().map(|s| s.len()).sum();
                let aux_chars = w.ast_anchor.len() + w.context.len();
                estimate_tokens_chars(body_chars + aux_chars)
            },
        );
        let total_batches = packed_ranges.len();

        // The `batch_size` field on `ExtractionProgressEvent::Start` is
        // backward-compatible with desktop UI consumers — it now carries the
        // *average* batch size (rounded), not a static stride.
        let avg_batch_size = if total_batches == 0 {
            0
        } else {
            llm_work.len().div_ceil(total_batches)
        };
        let mut done: usize = 0;

        // Emit a start event immediately so the progress bar can switch from
        // "waiting for LLM..." to a real counted bar BEFORE any batch call starts.
        // `batch_size` here is the AVERAGE chunk count per batch under the
        // Wedge-1 token-aware packer.  Callers display it as a coarse hint.
        if let Some(ref pf) = self.progress {
            pf(ExtractionProgressEvent::Start {
                total_chunks,
                batch_size: avg_batch_size,
                total_batches,
            });
        }

        // ── Process cache hits (instant, no LLM) ───────────────────────
        output.cache_hits = cache_hits_count;
        for (source_id, source_uri, chunk_byte_start, chunk_byte_end, mut cached_result) in
            cache_hits_data
        {
            // Cached entries from before W1 byte-range work carry empty
            // source_path / (0, 0) byte ranges. Backfill from the chunk so
            // every claim flowing into convert_result_static carries the v3
            // citation triple even on warm-cache runs.
            backfill_chunk_origin(
                &mut cached_result,
                &source_uri,
                chunk_byte_start,
                chunk_byte_end,
            );
            let converted =
                Self::convert_result_static(cached_result, source_id, workspace_id, min_confidence);
            output.merge(converted);
            output.chunks_processed += 1;
            done += 1;
            if let Some(ref pf) = self.progress {
                pf(ExtractionProgressEvent::ChunkDone {
                    done,
                    total: total_chunks,
                    source_uri,
                });
            }
        }

        // ── Process structural results (instant, no LLM) ─────────────
        // Structural extraction is additive: the same chunks also run through the LLM
        // path below. Progress events and chunks_processed are tracked there (once per
        // original chunk). Here we only merge structural results and update the stat.
        let structural_count = structural_results.len();
        for (source_id, _source_uri, struct_result) in structural_results {
            // Use min_confidence=0.0 for structural — they're always 0.99, never filtered
            let converted =
                Self::convert_result_static(struct_result, source_id, workspace_id, 0.0);
            output.merge(converted);
            output.structural_extractions += 1;
        }
        if structural_count > 0 {
            tracing::info!(
                "structural extraction: {} chunks processed (additive with LLM, zero extra LLM calls)",
                structural_count
            );
        }

        // ── Batch LLM calls — EXTRACTION_BATCH_SIZE cache-misses per call ──────────
        // Cache hits were already processed above. Here we group remaining
        // llm_work into batches of EXTRACTION_BATCH_SIZE and fire one LLM call
        // per batch. Results split back per-chunk and cached individually.
        //
        // One semaphore permit = one batch call (not one chunk call).
        let known_entities_section = self.known_entities.prompt_section();
        let mut join_set = tokio::task::JoinSet::new();

        for (batch_idx, &(slice_start, slice_end)) in packed_ranges.iter().enumerate() {
            let batch_work: Vec<_> = llm_work[slice_start..slice_end].to_vec();
            let llm = Arc::clone(&self.llm);
            let sem = Arc::clone(&semaphore);
            let graph_ctx = known_entities_section.clone();
            let progress = self.progress.clone();
            let batch_index = batch_idx + 1;
            // Wedge 1: ranges follow the actual packer partition (1-indexed
            // inclusive on the user-facing `chunks` axis).  Cache hits sit at
            // positions [1..=cache_hits_count]; LLM work starts after.
            let range_start = cache_hits_count + slice_start + 1;
            let range_end = cache_hits_count + slice_end;
            let batch_chunks = batch_work.len();

            join_set.spawn(async move {
                // Spawn return type carries `Err((range_start, range_end))`
                // on permanent failure so the collect loop can attribute the
                // affected chunk range to the user.  Pre-fix this was an
                // `Option<...>` whose `None` was silently dropped — the user
                // saw "extraction complete" with claims missing.  The Ok
                // arm carries `BatchMeta` so the collect-side checkpoint
                // record can attribute each completed batch back to its
                // 0-indexed slot + 1-indexed chunk range.
                type BatchOk = (
                    BatchMeta,
                    Vec<ChunkWork>,
                    Vec<crate::batch::BatchChunkResult>,
                );
                type BatchFail = (usize, usize);
                let meta = BatchMeta {
                    batch_idx,
                    range_start,
                    range_end,
                    batch_chunks,
                };
                let permit = match sem.acquire().await {
                    Ok(p) => p,
                    Err(_) => {
                        // Semaphore closed (normally only on shutdown).
                        // Treat as a permanent failure so the caller knows.
                        return Err::<BatchOk, BatchFail>((range_start, range_end));
                    }
                };
                let _permit = permit;

                if let Some(ref pf) = progress {
                    pf(ExtractionProgressEvent::BatchStart {
                        batch_index,
                        total_batches,
                        range_start,
                        range_end,
                        batch_chunks,
                    });
                }

                // Build batch chunks — combine ast_anchor with graph context per chunk.
                let batch_chunks: Vec<crate::batch::BatchChunk> = batch_work
                    .iter()
                    .enumerate()
                    .map(|(i, work)| {
                        let combined_ctx = if work.ast_anchor.is_empty() {
                            graph_ctx.clone()
                        } else {
                            format!("{}\n\n{}", work.ast_anchor, graph_ctx)
                        };
                        crate::batch::BatchChunk {
                            id: i,
                            content: work.sub_chunks.join("\n"),
                            context: work.context.clone(),
                            ast_anchor: combined_ctx,
                        }
                    })
                    .collect();

                let expected_ids: Vec<usize> = (0..batch_chunks.len()).collect();
                let batch_prompt = crate::batch::build_batch_prompt(&batch_chunks, &graph_ctx);

                match llm.extract_batch_raw(&batch_prompt).await {
                    Ok(raw_response) => {
                        let batch_results =
                            crate::batch::parse_batch_response(&raw_response, &expected_ids);
                        Ok((meta, batch_work, batch_results))
                    }
                    Err(e) => {
                        tracing::warn!(
                            range_start,
                            range_end,
                            "batch extraction failed permanently: {e}"
                        );
                        Err((range_start, range_end))
                    }
                }
            });
        }

        // ── Collect batch results ──────────────────────────────────────────
        // Cancellation runs concurrently with the join: if the token
        // fires while every spawned task is still awaiting its LLM
        // round-trip, the `tokio::select!` arm picks it up
        // immediately instead of waiting for the slowest inflight
        // batch to return. Pre-fix the cancel check sat AFTER
        // `join_next().await` and the desktop's Stop button could
        // wait minutes against a slow-completing batch before the
        // pipeline acknowledged the cancel.
        loop {
            let join_result = match self.cancel.as_ref() {
                Some(tok) => {
                    tokio::select! {
                        biased;
                        _ = tok.cancelled() => {
                            join_set.shutdown().await;
                            return Err(thinkingroot_core::Error::Cancelled);
                        }
                        next = join_set.join_next() => match next {
                            Some(r) => r,
                            None => break,
                        }
                    }
                }
                None => match join_set.join_next().await {
                    Some(r) => r,
                    None => break,
                },
            };
            let batch_outcome = match join_result {
                Ok(inner) => inner,
                Err(join_err) => {
                    // Tokio task panic — count as a permanent failure
                    // but with no range information available.
                    tracing::error!("batch task panicked: {join_err}");
                    output.failed_batches += 1;
                    continue;
                }
            };
            let (batch_meta, batch_work, batch_results) = match batch_outcome {
                Ok(triple) => triple,
                Err((rs, re)) => {
                    output.failed_batches += 1;
                    output.failed_batch_ranges.push((rs, re));
                    continue;
                }
            };
            {
                for chunk_result in batch_results {
                    if chunk_result.id >= batch_work.len() {
                        continue;
                    }
                    let work = &batch_work[chunk_result.id];
                    let mut extraction_result = chunk_result.result;

                    // The LLM does not yet emit byte ranges per claim (Week
                    // 1.5 will teach the prompt + parser to do so). Until
                    // then, every claim from this chunk inherits the
                    // chunk's byte range — coarse but always verifiable
                    // against source bytes.
                    backfill_chunk_origin(
                        &mut extraction_result,
                        &work.source_uri,
                        work.chunk_byte_start,
                        work.chunk_byte_end,
                    );

                    // Write per-chunk cache entries.
                    if let Some(ref cache) = self.cache {
                        for sub_content in &work.sub_chunks {
                            if let Err(e) = cache.put(sub_content, &extraction_result) {
                                tracing::warn!("failed to write extraction cache entry: {e}");
                            }
                        }
                        // Also write under the original full-chunk key for split chunks.
                        let needs_original_key = work.sub_chunks.len() > 1
                            || work
                                .sub_chunks
                                .first()
                                .map(|c| c != &work.original_content)
                                .unwrap_or(false);
                        if needs_original_key
                            && let Err(e) = cache.put(&work.original_content, &extraction_result)
                        {
                            tracing::warn!("failed to write original cache entry: {e}");
                        }
                    }

                    let converted = Self::convert_result_static(
                        extraction_result,
                        work.source_id,
                        workspace_id,
                        min_confidence,
                    );
                    output.merge(converted);
                    output.chunks_processed += 1;
                    done += 1;
                    if let Some(ref pf) = self.progress {
                        pf(ExtractionProgressEvent::ChunkDone {
                            done,
                            total: total_chunks,
                            source_uri: work.source_uri.clone(),
                        });
                    }
                }
                // Cache + claim merge for this batch are durable now.
                // Record the batch in the in-flight log so a kill-and-
                // resume can fast-forward past it.  The cache write
                // ordering matters: the per-chunk content cache was
                // populated above, so a re-run will see cache hits for
                // these chunks even if the checkpoint write below fails.
                if let Some(ref ckpt) = self.checkpoint
                    && let Err(e) = ckpt.record_batch(
                        batch_meta.batch_idx,
                        batch_meta.range_start,
                        batch_meta.range_end,
                        batch_meta.batch_chunks,
                    )
                {
                    // Non-fatal: the per-chunk cache is the source of
                    // truth.  A missing checkpoint record just means
                    // the next run won't log the resume hint.
                    tracing::warn!(
                        batch_idx = batch_meta.batch_idx,
                        "failed to record checkpoint entry: {e}"
                    );
                }
            }
        }

        // Guard: if some tasks returned None (all sub-chunks failed), fire a
        // synthetic catch-up event so the bar always reaches 100%.
        if done < total_chunks
            && let Some(ref pf) = self.progress
        {
            pf(ExtractionProgressEvent::ChunkDone {
                done: total_chunks,
                total: total_chunks,
                source_uri: String::new(),
            });
        }

        // Deduplicate claims by normalized statement — prevents graph bloat from
        // overlapping chunks extracting the same fact.
        dedup_claims(&mut output);

        tracing::info!(
            "extraction complete: {} claims, {} entities, {} relations \
             from {} sources ({} chunks, {} cache hits, {} structural)",
            output.claims.len(),
            output.entities.len(),
            output.relations.len(),
            output.sources_processed,
            output.chunks_processed,
            output.cache_hits,
            output.structural_extractions,
        );

        Ok(output)
    }

    /// Convert LLM extraction results into core types (static so spawned tasks can call it).
    fn convert_result_static(
        result: ExtractionResult,
        source_id: SourceId,
        workspace_id: WorkspaceId,
        min_confidence: f64,
    ) -> ExtractionOutput {
        let mut output = ExtractionOutput::default();

        // Convert entities.
        let mut entity_map = std::collections::HashMap::new();
        for ext_entity in &result.entities {
            let entity_type = parse_entity_type(&ext_entity.entity_type);
            let mut entity = Entity::new(&ext_entity.name, entity_type);
            for alias in &ext_entity.aliases {
                entity.add_alias(alias);
            }
            entity.description = ext_entity.description.clone();
            entity_map.insert(ext_entity.name.to_lowercase(), entity.id);
            output.entities.push(entity);
        }

        // Convert claims and track their entity references.
        let now = chrono::Utc::now();
        for ext_claim in &result.claims {
            if ext_claim.confidence < min_confidence {
                continue;
            }
            let claim_type = parse_claim_type(&ext_claim.claim_type);
            let mut claim = Claim::new(&ext_claim.statement, claim_type, source_id, workspace_id)
                .with_confidence(ext_claim.confidence)
                .with_extraction_tier(ext_claim.extraction_tier);
            // Compile Completeness Contract §4.1 — propagate the symbol
            // identifier so Phase 7e can resolve `function_calls.callee_name`
            // → `claim_id` via the `claims.symbol` index.
            if let Some(sym) = &ext_claim.symbol
                && !sym.is_empty()
            {
                claim = claim.with_symbol(sym.clone());
            }

            // ─── Compile Completeness Contract §5 — decorate the claim ─
            // Sensitivity: regex layer reads the statement; merge with
            // any LLM-suggested tier the extractor stamped onto
            // `ext_claim.sensitivity`. Higher tier wins.
            let regex_tier = crate::sensitivity::classify_text(&ext_claim.statement);
            let merged_tier = crate::sensitivity::merge(ext_claim.sensitivity, regex_tier);
            if let Some(tier) = merged_tier {
                claim = claim.with_sensitivity(tier);
            }
            // Quantities: extract numeric tuples from the statement.
            // Multiple per claim are routine. Phase 6.7 reads
            // `output.claim_quantities[claim.id]` to emit `quantities` rows.
            let mut quantities = ext_claim.quantities.clone();
            quantities.extend(crate::quantity::extract(&ext_claim.statement));
            if !quantities.is_empty() {
                output.claim_quantities.insert(claim.id, quantities);
            }
            // Expiration: prefer LLM-stamped signal; otherwise classify
            // from the statement. `None` means no expiration phrasing —
            // Phase 6.7 leaves `claim_temporal.valid_until` at the
            // never-expires sentinel.
            let expiration = ext_claim
                .expiration_signal
                .clone()
                .map(|signal| crate::expiration::ExtractedExpiration {
                    signal,
                    valid_until: ext_claim.valid_until.clone(),
                })
                .or_else(|| crate::expiration::extract(&ext_claim.statement, now));
            if let Some(exp) = expiration {
                output.claim_expirations.insert(claim.id, exp);
            }
            // Propagate v3 byte-range citation onto the claim's source_span
            // when present. (0, 0) is the "unknown" sentinel from chunks
            // whose parser hasn't been upgraded yet — leave source_span
            // unset so downstream consumers fall back to whole-file scope.
            if ext_claim.byte_end > ext_claim.byte_start {
                claim =
                    claim.with_span(SourceSpan::bytes(ext_claim.byte_start, ext_claim.byte_end));
            }
            // Wire event_date: convert ISO string → DateTime<Utc>.
            if let Some(ref date_str) = ext_claim.event_date
                && let Ok(nd) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                && let Some(dt) = nd.and_hms_opt(12, 0, 0).map(|ndt| ndt.and_utc())
            {
                claim = claim.with_event_date(dt);
            }
            if !ext_claim.entities.is_empty() {
                output
                    .claim_entity_names
                    .insert(claim.id, ext_claim.entities.clone());
            }
            if let Some(ref quote) = ext_claim.source_quote
                && !quote.is_empty()
            {
                output.claim_source_quotes.insert(claim.id, quote.clone());
            }
            // Wire optional predicate from LLM output. Invalid entries
            // (unknown language, regex that fails to compile) are dropped
            // silently so the claim lands in `Attested` tier rather than
            // failing extraction.
            if let Some(ref ext_pred) = ext_claim.predicate
                && let Some(pred) = convert_predicate(ext_pred)
            {
                claim = claim.with_predicate(pred);
            }
            output.claims.push(claim);
        }

        // Convert relations — filter unknown types and low-confidence ones.
        for ext_rel in &result.relations {
            let from_id = entity_map.get(&ext_rel.from_entity.to_lowercase());
            let to_id = entity_map.get(&ext_rel.to_entity.to_lowercase());

            if let (Some(&from), Some(&to)) = (from_id, to_id) {
                // Reject unknown relation types (returns None) and explicit SKIP.
                let Some(rel_type) = parse_relation_type(&ext_rel.relation_type) else {
                    tracing::debug!(
                        "discarded relation '{}' → '{}' with unknown type '{}'",
                        ext_rel.from_entity,
                        ext_rel.to_entity,
                        ext_rel.relation_type
                    );
                    continue;
                };

                // Reject low-confidence relations (LLM was too uncertain).
                let confidence = ext_rel.confidence.clamp(0.0, 1.0);
                if confidence < 0.3 {
                    tracing::debug!(
                        "discarded low-confidence relation '{}' → '{}' ({:.2})",
                        ext_rel.from_entity,
                        ext_rel.to_entity,
                        confidence
                    );
                    continue;
                }

                let rel = Relation::new(from, to, rel_type)
                    .with_strength(confidence)
                    .with_description(ext_rel.description.clone().unwrap_or_default());
                output.relations.push(SourcedRelation {
                    source: source_id,
                    relation: rel,
                });
            }
        }

        output
    }
}

/// Backfill the v3 byte-range citation triple onto every claim in
/// `result` that doesn't already carry one. Called from both the
/// cache-hit path and the LLM-batch path of `Extractor::extract` so the
/// downstream `convert_result_static` always sees a populated
/// `(source_path, byte_start, byte_end)` triple — even when the cached
/// entry pre-dates the v3 schema or the LLM hasn't been taught to emit
/// per-claim byte spans yet.
///
/// `chunk_byte_start == chunk_byte_end == 0` is the "parser hasn't been
/// upgraded" sentinel; in that case byte ranges are left at (0, 0) and
/// the v3 pack writer + provenance probe fall back to file-level scope.
fn backfill_chunk_origin(
    result: &mut ExtractionResult,
    source_uri: &str,
    chunk_byte_start: u64,
    chunk_byte_end: u64,
) {
    for claim in &mut result.claims {
        if claim.source_path.is_empty() {
            claim.source_path = source_uri.to_string();
        }
        if claim.byte_start == 0 && claim.byte_end == 0 && chunk_byte_end > chunk_byte_start {
            claim.byte_start = chunk_byte_start;
            claim.byte_end = chunk_byte_end;
        }
    }
}

/// Wedge 1: estimate the input-token cost of a string for the batch packer.
/// Reuses the engine-wide `chars / 4` heuristic (matches
/// `split_to_token_budget` and the throughput scheduler) so all call sites
/// agree.  Always returns at least 1 — a chunk that contributes zero tokens
/// would otherwise let the packer accumulate infinite items.
pub(crate) fn estimate_tokens_chars(chars: usize) -> usize {
    (chars / 4).max(1)
}

/// Wedge 1: token-aware batch packer for the variable-size LLM call grouping.
///
/// Walks `items` left-to-right and seals a batch when adding the next item
/// would push the running token total past `token_budget` *or* the item
/// count past `max_chunks`.  Returns half-open index ranges `[start, end)`
/// into `items`.
///
/// Guarantees:
/// - Every input is covered by exactly one range (Σ (end - start) ==
///   items.len()).
/// - No empty range is produced.
/// - An item larger than `token_budget` becomes its own batch (size 1) — we
///   never silently drop oversized items, and `split_to_token_budget` has
///   already split content above the per-chunk cap before we get here.
/// - Provider chunk-count caps from `model_batch_size` (Bedrock = 8,
///   Perplexity = 1, …) flow in via `max_chunks`.
pub(crate) fn pack_batches<T>(
    items: &[T],
    token_budget: usize,
    max_chunks: usize,
    cost: impl Fn(&T) -> usize,
) -> Vec<(usize, usize)> {
    if items.is_empty() {
        return Vec::new();
    }
    let max_chunks = max_chunks.max(1);
    let token_budget = token_budget.max(1);

    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut acc_tokens: usize = 0;
    for (i, w) in items.iter().enumerate() {
        let item_cost = cost(w);
        let count_in_batch = i - start;
        let would_exceed_tokens = acc_tokens + item_cost > token_budget && count_in_batch > 0;
        let would_exceed_count = count_in_batch >= max_chunks;
        if would_exceed_tokens || would_exceed_count {
            out.push((start, i));
            start = i;
            acc_tokens = 0;
        }
        acc_tokens += item_cost;
    }
    if start < items.len() {
        out.push((start, items.len()));
    }
    out
}

/// Wedge 3: chunk-aware splitter dispatching on `chunk.chunk_type` and
/// `chunk.language` so oversized code chunks split at top-level statement
/// boundaries (not mid-body line cuts) and oversized prose splits at
/// paragraph / sentence boundaries.  Falls back to the line-based splitter
/// for any case where the AST or sentence path doesn't apply.
fn split_chunk_to_token_budget(
    chunk: &thinkingroot_core::ir::Chunk,
    max_tokens: usize,
) -> Vec<String> {
    use thinkingroot_core::ir::ChunkType;

    let max_chars = max_tokens.saturating_mul(4).max(1);
    if chunk.content.len() <= max_chars {
        return vec![chunk.content.clone()];
    }

    // Code-like chunks with a known language → tree-sitter statement split.
    let is_code_like = matches!(
        chunk.chunk_type,
        ChunkType::FunctionDef | ChunkType::TypeDef | ChunkType::Code | ChunkType::Import
    );
    if is_code_like
        && let Some(lang) = chunk.language.as_deref()
        && let Some(parts) =
            crate::ast_split::split_at_statement_boundaries(&chunk.content, lang, max_tokens)
    {
        return parts;
    }

    // Prose / heading / generic markdown → paragraph + sentence split.
    let is_prose_like = matches!(
        chunk.chunk_type,
        ChunkType::Prose
            | ChunkType::Heading
            | ChunkType::List
            | ChunkType::Comment
            | ChunkType::ModuleDoc
    );
    if is_prose_like {
        let parts = crate::prose_split::split_prose(&chunk.content, max_tokens);
        if parts.len() > 1 {
            return parts;
        }
    }

    // Final fallback: legacy line-based splitter.
    split_to_token_budget_lines(&chunk.content, max_tokens)
}

/// Legacy line-based splitter — preserved as the universal fallback for
/// chunks where AST / sentence splitting can't help (unknown language,
/// pathological single-line input).
fn split_to_token_budget_lines(content: &str, max_tokens: usize) -> Vec<String> {
    // chars/4 is a conservative token approximation that works across all tokenizers.
    let max_chars = max_tokens.saturating_mul(4).max(1);

    if content.len() <= max_chars {
        return vec![content.to_string()];
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in lines {
        // If adding this line would exceed budget, flush current and start new chunk.
        if !current.is_empty() && current.len() + line.len() + 1 > max_chars {
            chunks.push(current.trim().to_string());
            current = String::new();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }

    if chunks.is_empty() {
        vec![content.to_string()]
    } else {
        chunks
    }
}

/// Deduplicate claims by normalized statement text.
///
/// Normalization: lowercase + strip trailing sentence punctuation + collapse whitespace.
/// When duplicates found: the claim with the highest confidence survives.
///
/// Called once, after all batch LLM calls complete, before returning ExtractionOutput.
/// Prevents graph bloat when overlapping chunks extract the same fact.
fn dedup_claims(output: &mut ExtractionOutput) {
    fn normalize(s: &str) -> String {
        s.to_lowercase()
            .trim_end_matches(['.', '!', '?'])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    // First pass: for each normalized key, find the index of the highest-confidence claim.
    let mut best: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, claim) in output.claims.iter().enumerate() {
        let key = normalize(&claim.statement);
        best.entry(key)
            .and_modify(|prev_idx| {
                if claim.confidence.value() > output.claims[*prev_idx].confidence.value() {
                    *prev_idx = i;
                }
            })
            .or_insert(i);
    }

    // Collect the winning indices into a set.
    let keep: std::collections::HashSet<usize> = best.into_values().collect();

    let before = output.claims.len();
    let mut idx = 0usize;
    output.claims.retain(|_| {
        let keep_this = keep.contains(&idx);
        idx += 1;
        keep_this
    });

    let removed = before - output.claims.len();
    if removed > 0 {
        tracing::debug!(
            "dedup_claims: removed {removed} duplicate claims, kept {}",
            output.claims.len()
        );
    }
}

impl ExtractionOutput {
    fn merge(&mut self, other: ExtractionOutput) {
        self.claims.extend(other.claims);
        self.entities.extend(other.entities);
        self.relations.extend(other.relations);
        self.claim_entity_names.extend(other.claim_entity_names);
        self.sources_processed += other.sources_processed;
        self.chunks_processed += other.chunks_processed;
        self.cache_hits += other.cache_hits;
        self.structural_extractions += other.structural_extractions;
        self.source_texts.extend(other.source_texts);
        self.claim_source_quotes.extend(other.claim_source_quotes);
        self.failed_batches += other.failed_batches;
        self.failed_batch_ranges.extend(other.failed_batch_ranges);
        self.claim_quantities.extend(other.claim_quantities);
        self.claim_expirations.extend(other.claim_expirations);
    }
}

/// Convert the LLM's raw predicate payload into a validated core `Predicate`.
///
/// Returns `None` when:
/// - the language string isn't one we support (`regex`, `rust_ast`, `jsonpath`)
/// - the query is empty
/// - the query is a regex that fails to compile (dropped silently per plan §5.2)
fn convert_predicate(
    raw: &crate::schema::ExtractedPredicate,
) -> Option<thinkingroot_core::types::Predicate> {
    use thinkingroot_core::types::{Predicate, PredicateLanguage, PredicateScope};

    if raw.query.trim().is_empty() {
        return None;
    }
    let language = PredicateLanguage::from_str(&raw.language.to_lowercase())?;
    // Validate regex patterns eagerly so malformed queries never reach Rooting.
    // AST / JSONPath validation happens in their respective engines (Weeks 4–5).
    if language == PredicateLanguage::Regex && regex::Regex::new(&raw.query).is_err() {
        return None;
    }
    Some(Predicate {
        language,
        query: raw.query.clone(),
        scope: PredicateScope::from_globs(raw.scope_globs.clone()),
    })
}

fn parse_claim_type(s: &str) -> ClaimType {
    match s.to_lowercase().as_str() {
        "fact" => ClaimType::Fact,
        "decision" => ClaimType::Decision,
        "opinion" => ClaimType::Opinion,
        "plan" => ClaimType::Plan,
        "requirement" => ClaimType::Requirement,
        "metric" => ClaimType::Metric,
        "definition" => ClaimType::Definition,
        "dependency" => ClaimType::Dependency,
        "api_signature" => ClaimType::ApiSignature,
        "architecture" => ClaimType::Architecture,
        "preference" => ClaimType::Preference,
        _ => ClaimType::Fact,
    }
}

fn parse_entity_type(s: &str) -> EntityType {
    match s.to_lowercase().as_str() {
        "person" => EntityType::Person,
        "system" => EntityType::System,
        "service" => EntityType::Service,
        "concept" => EntityType::Concept,
        "team" => EntityType::Team,
        "api" => EntityType::Api,
        "database" => EntityType::Database,
        "library" => EntityType::Library,
        "file" => EntityType::File,
        "module" => EntityType::Module,
        "function" => EntityType::Function,
        "config" => EntityType::Config,
        "organization" => EntityType::Organization,
        _ => EntityType::Concept,
    }
}

fn parse_relation_type(s: &str) -> Option<RelationType> {
    match s.to_lowercase().trim() {
        "depends_on" => Some(RelationType::DependsOn),
        "owned_by" => Some(RelationType::OwnedBy),
        "replaces" => Some(RelationType::Replaces),
        "contradicts" => Some(RelationType::Contradicts),
        "implements" => Some(RelationType::Implements),
        "uses" => Some(RelationType::Uses),
        "contains" => Some(RelationType::Contains),
        "created_by" => Some(RelationType::CreatedBy),
        "part_of" => Some(RelationType::PartOf),
        "related_to" => Some(RelationType::RelatedTo),
        "calls" => Some(RelationType::Calls),
        "configured_by" => Some(RelationType::ConfiguredBy),
        "tested_by" => Some(RelationType::TestedBy),
        "skip_relation" | "" => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicate_claims_by_normalized_statement() {
        use thinkingroot_core::types::{Claim, ClaimType, SourceId, WorkspaceId};

        let src = SourceId::new();
        let ws = WorkspaceId::new();

        let claim_a = Claim::new("Rust is fast", ClaimType::Fact, src, ws).with_confidence(0.8);
        let claim_b = Claim::new("Rust is fast", ClaimType::Fact, src, ws).with_confidence(0.9);
        let claim_c = Claim::new("Go is simple", ClaimType::Fact, src, ws).with_confidence(0.7);

        let mut output = ExtractionOutput {
            claims: vec![claim_a, claim_b, claim_c],
            ..Default::default()
        };

        dedup_claims(&mut output);

        assert_eq!(output.claims.len(), 2, "duplicate claim must be removed");
        let rust_claim = output
            .claims
            .iter()
            .find(|c| c.statement == "Rust is fast")
            .unwrap();
        assert!(
            (rust_claim.confidence.value() - 0.9).abs() < 0.001,
            "surviving claim must have max confidence 0.9, got {}",
            rust_claim.confidence.value()
        );
    }

    #[test]
    fn dedup_claims_normalizes_case_and_trailing_punctuation() {
        use thinkingroot_core::types::{Claim, ClaimType, SourceId, WorkspaceId};

        let src = SourceId::new();
        let ws = WorkspaceId::new();

        let claims = vec![
            Claim::new("Rust is FAST.", ClaimType::Fact, src, ws).with_confidence(0.8),
            Claim::new("rust is fast", ClaimType::Fact, src, ws).with_confidence(0.9),
        ];

        let mut output = ExtractionOutput {
            claims,
            ..Default::default()
        };
        dedup_claims(&mut output);

        assert_eq!(
            output.claims.len(),
            1,
            "case/punctuation variants must be deduped"
        );
    }

    #[test]
    fn batch_size_constant_is_six() {
        assert_eq!(
            EXTRACTION_BATCH_SIZE, 6,
            "batch size must be 6 — see perf analysis"
        );
    }

    #[test]
    fn split_to_token_budget_no_split_needed() {
        let content = "hello world\nfoo bar";
        let chunks = split_to_token_budget_lines(content, 10000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], content);
    }

    #[test]
    fn split_to_token_budget_splits_at_line_boundary() {
        // 4 chars per token, budget of 5 tokens = 20 chars max.
        let line_a = "AAAAAAAAAA"; // 10 chars
        let line_b = "BBBBBBBBBB"; // 10 chars
        let line_c = "CCCCCCCCCC"; // 10 chars
        let content = format!("{line_a}\n{line_b}\n{line_c}");
        let chunks = split_to_token_budget_lines(&content, 5); // 20 chars budget
        // line_a + line_b = 21 chars (with \n), so they can't both fit.
        assert!(chunks.len() >= 2);
        // Every line must appear in some chunk.
        let rejoined = chunks.join("\n");
        assert!(rejoined.contains(line_a));
        assert!(rejoined.contains(line_b));
        assert!(rejoined.contains(line_c));
    }

    #[test]
    fn split_to_token_budget_single_large_line_kept_intact() {
        // A single line larger than budget is kept as-is (can't split mid-line).
        let big_line = "X".repeat(1000);
        let chunks = split_to_token_budget_lines(&big_line, 10); // 40 chars budget
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], big_line);
    }

    // ── Wedge 3: chunk-aware AST/prose dispatch ─────────────────────────

    #[test]
    fn split_chunk_to_token_budget_dispatches_code_to_ast_split() {
        use thinkingroot_core::ir::{Chunk, ChunkType};
        // Two adjacent Rust functions, each small individually but together
        // over the 50-token budget — the AST-aware splitter should produce 2.
        let src = "pub fn a() -> i32 { 1 }\n\npub fn b() -> i32 { 2 }\n".repeat(8);
        let mut c = Chunk::new(src.clone(), ChunkType::Code, 1, 1);
        c = c.with_language("rust");
        let parts = split_chunk_to_token_budget(&c, 30);
        assert!(parts.len() > 1, "expected AST split; got {} parts", parts.len());
        // Every part has balanced braces (no mid-function cuts).
        for p in &parts {
            assert_eq!(p.matches('{').count(), p.matches('}').count());
        }
    }

    #[test]
    fn split_chunk_to_token_budget_dispatches_prose_to_prose_split() {
        use thinkingroot_core::ir::{Chunk, ChunkType};
        let mut sentences = String::new();
        for i in 0..50 {
            sentences.push_str(&format!("Sentence number {i}. "));
        }
        let c = Chunk::new(sentences.clone(), ChunkType::Prose, 1, 1);
        let parts = split_chunk_to_token_budget(&c, 50);
        assert!(parts.len() > 1);
    }

    #[test]
    fn split_chunk_unknown_language_falls_back_to_lines() {
        use thinkingroot_core::ir::{Chunk, ChunkType};
        let big = "fn a() {}\n".repeat(2_000); // ~20K chars
        let mut c = Chunk::new(big, ChunkType::Code, 1, 1);
        c = c.with_language("cobol"); // unsupported by ts_language
        let parts = split_chunk_to_token_budget(&c, 100);
        assert!(parts.len() > 1, "line-based fallback must split");
    }

    #[test]
    fn unknown_relation_type_is_rejected_not_mapped_to_related_to() {
        let result = parse_relation_type("blah_relation");
        assert!(
            result.is_none(),
            "unknown types must be rejected, not silently mapped"
        );
    }

    #[test]
    fn skip_relation_is_rejected() {
        assert!(parse_relation_type("skip_relation").is_none());
        assert!(parse_relation_type("SKIP_RELATION").is_none());
        assert!(parse_relation_type("").is_none());
    }

    #[test]
    fn known_types_still_parse() {
        assert_eq!(
            parse_relation_type("depends_on"),
            Some(RelationType::DependsOn)
        );
        assert_eq!(parse_relation_type("calls"), Some(RelationType::Calls));
        assert_eq!(
            parse_relation_type("implements"),
            Some(RelationType::Implements)
        );
        assert_eq!(
            parse_relation_type("related_to"),
            Some(RelationType::RelatedTo)
        );
    }

    #[test]
    fn extraction_output_default_has_no_failed_batches() {
        // Regression for C4: pre-fix the partial-failure counter didn't
        // exist; failed batches were silently dropped.  A fresh
        // ExtractionOutput must start clean so the merge accumulator
        // produces 0 in the no-failures case.
        let out = ExtractionOutput::default();
        assert_eq!(out.failed_batches, 0);
        assert!(out.failed_batch_ranges.is_empty());
    }

    #[test]
    fn extraction_output_merge_accumulates_failed_batches() {
        // The pipeline calls `output.merge(converted)` for every chunk
        // result; failed_batches must roll forward end-to-end so the
        // CLI summary + ProgressEvent::ExtractionPartial see the right
        // total.  Mirrors how the merge of cache_hits / chunks_processed
        // already works.
        let mut a = ExtractionOutput {
            failed_batches: 1,
            failed_batch_ranges: vec![(1, 6)],
            ..Default::default()
        };
        let b = ExtractionOutput {
            failed_batches: 2,
            failed_batch_ranges: vec![(13, 18), (25, 30)],
            ..Default::default()
        };
        a.merge(b);
        assert_eq!(a.failed_batches, 3);
        assert_eq!(a.failed_batch_ranges, vec![(1, 6), (13, 18), (25, 30)]);
    }

    // ── Wedge 1: token-aware mega-batches ────────────────────────────────

    #[test]
    fn pack_batches_empty_input_yields_no_ranges() {
        let v: Vec<usize> = Vec::new();
        let packed = pack_batches(&v, 1000, 64, |_| 100);
        assert!(packed.is_empty());
    }

    #[test]
    fn pack_batches_respects_token_budget() {
        let costs = vec![100usize, 200, 300, 400, 100, 100];
        let packed = pack_batches(&costs, 600, 64, |&c| c);
        // Greedy fill: [100,200,300]=600 → seal, [400,100]=500 → seal at next
        // would-overflow, [100] stays.  Last batch is whatever fit at end.
        // Actual run: 100+200=300, +300=600 (== budget, not over), +400 would
        // be 1000 → seal at idx 3.  Then 400+100=500, +100=600, end.
        let totals: Vec<usize> = packed
            .iter()
            .map(|&(s, e)| costs[s..e].iter().sum())
            .collect();
        assert!(totals.iter().all(|&t| t <= 600));
        // Round-trip: every item is covered exactly once.
        let total_count: usize = packed.iter().map(|&(s, e)| e - s).sum();
        assert_eq!(total_count, costs.len());
    }

    #[test]
    fn pack_batches_respects_max_chunks_cap() {
        // Tiny chunks, generous token budget — chunk-count cap should
        // dominate.
        let costs = vec![1usize; 200];
        let packed = pack_batches(&costs, 1_000_000, 32, |&c| c);
        assert!(
            packed.iter().all(|&(s, e)| e - s <= 32),
            "no batch may exceed max_chunks: {packed:?}"
        );
        let total_count: usize = packed.iter().map(|&(s, e)| e - s).sum();
        assert_eq!(total_count, costs.len());
    }

    #[test]
    fn pack_batches_passes_through_oversized_item() {
        // A single item bigger than the budget must still appear (as its own
        // batch of size 1) — never silently dropped.
        let costs = vec![50usize, 99_999, 50];
        let packed = pack_batches(&costs, 100, 64, |&c| c);
        // Expect: [50] | [99_999] | [50]  (oversized item alone)
        // Or: [50, 99_999 won't fit] -> [50], [99_999], [50].
        assert_eq!(packed.len(), 3);
        let total_count: usize = packed.iter().map(|&(s, e)| e - s).sum();
        assert_eq!(total_count, 3);
        // Middle range carries exactly the oversized item.
        let middle = packed[1];
        assert_eq!(middle.1 - middle.0, 1);
    }

    #[test]
    fn pack_batches_produces_no_empty_ranges() {
        let costs = vec![10usize; 5];
        let packed = pack_batches(&costs, 25, 3, |&c| c);
        assert!(packed.iter().all(|&(s, e)| e > s));
    }

    #[test]
    fn pack_batches_handles_zero_token_budget_safely() {
        // Budget clamped internally to 1; oversized-item rule kicks in.
        let costs = vec![5usize, 5, 5];
        let packed = pack_batches(&costs, 0, 64, |&c| c);
        assert_eq!(packed.len(), 3);
        assert!(packed.iter().all(|&(s, e)| e - s == 1));
    }

    #[test]
    fn estimate_tokens_chars_floors_at_one() {
        assert_eq!(estimate_tokens_chars(0), 1);
        assert_eq!(estimate_tokens_chars(3), 1);
        assert_eq!(estimate_tokens_chars(4), 1);
        assert_eq!(estimate_tokens_chars(5), 1);
        assert_eq!(estimate_tokens_chars(8), 2);
        assert_eq!(estimate_tokens_chars(400), 100);
    }

    #[test]
    fn pack_batches_provider_caps_via_max_chunks() {
        // Bedrock's 8-cap and Perplexity's 1-cap arrive via max_chunks.
        let costs = vec![50usize; 100];
        let bedrock = pack_batches(&costs, 1_000_000, 8, |&c| c);
        assert!(bedrock.iter().all(|&(s, e)| e - s <= 8));

        let sonar = pack_batches(&costs, 1_000_000, 1, |&c| c);
        assert_eq!(sonar.len(), 100);
        assert!(sonar.iter().all(|&(s, e)| e - s == 1));
    }
}

#[cfg(test)]
mod tiered_tests {
    #[test]
    fn structural_chunks_produce_results_without_llm() {
        use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType};
        use thinkingroot_core::types::ExtractionTier;

        let chunk = Chunk {
            content: "pub fn compile(path: &Path) -> Result<()> { }".to_string(),
            chunk_type: ChunkType::FunctionDef,
            start_line: 1,
            end_line: 1,
            byte_start: 0,
            byte_end: 0,
            heading: None,
            language: Some("rust".to_string()),
            metadata: ChunkMetadata {
                function_name: Some("compile".to_string()),
                parameters: Some(vec!["path: &Path".to_string()]),
                return_type: Some("Result<()>".to_string()),
                visibility: Some("pub".to_string()),
                ..Default::default()
            },
        };

        let result = crate::structural::extract_structural(&chunk, "test/example.rs");
        assert!(
            !result.entities.is_empty(),
            "structural should produce entities"
        );
        assert!(
            !result.claims.is_empty(),
            "structural should produce claims"
        );
        let first_claim = result
            .claims
            .first()
            .expect("structural extractor must produce at least one claim");
        assert_eq!(
            first_claim.extraction_tier,
            ExtractionTier::Structural,
            "structural extractor must tag claims with ExtractionTier::Structural"
        );
    }

    #[test]
    fn router_correctly_splits_mixed_document() {
        use thinkingroot_core::ir::{Chunk, ChunkMetadata, ChunkType};

        let chunks = vec![
            Chunk {
                content: "pub fn foo() {}".to_string(),
                chunk_type: ChunkType::FunctionDef,
                start_line: 1,
                end_line: 1,
                byte_start: 0,
                byte_end: 0,
                heading: None,
                language: Some("rust".to_string()),
                metadata: ChunkMetadata {
                    function_name: Some("foo".to_string()),
                    ..Default::default()
                },
            },
            Chunk {
                content: "This module handles authentication.".to_string(),
                chunk_type: ChunkType::Prose,
                start_line: 5,
                end_line: 5,
                byte_start: 0,
                byte_end: 0,
                heading: None,
                language: None,
                metadata: ChunkMetadata::default(),
            },
            Chunk {
                content: "use std::path::Path;".to_string(),
                chunk_type: ChunkType::Import,
                start_line: 1,
                end_line: 1,
                byte_start: 0,
                byte_end: 0,
                heading: None,
                language: Some("rust".to_string()),
                metadata: ChunkMetadata {
                    import_path: Some("std::path::Path".to_string()),
                    ..Default::default()
                },
            },
        ];

        let (structural, llm) = crate::router::route_chunks(&chunks);
        assert_eq!(structural.len(), 2, "FunctionDef + Import = 2 structural");
        assert_eq!(llm.len(), 1, "Prose = 1 LLM");
        assert!(
            structural.contains(&0),
            "FunctionDef (index 0) should be structural"
        );
        assert!(
            structural.contains(&2),
            "Import (index 2) should be structural"
        );
        assert!(llm.contains(&1), "Prose (index 1) should be LLM");
    }
}
