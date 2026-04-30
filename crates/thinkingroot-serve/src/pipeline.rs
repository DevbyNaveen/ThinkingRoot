use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::types::WorkspaceId;
use thinkingroot_graph::StorageEngine;
use tokio_util::sync::CancellationToken;

/// Events emitted by the pipeline to drive CLI progress bars.
/// Sent via `tokio::sync::mpsc::UnboundedSender<ProgressEvent>`.
/// The CLI bar-driver task consumes these and renders indicatif bars.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// Parsing is about to begin. Emitted immediately before `parse_directory`
    /// so the bar driver can start its clock at the same instant the pipeline
    /// does — config load and data-dir setup are NOT counted as parse time.
    ParseStart,
    /// Parsing finished. `files` = number of documents parsed.
    ParseComplete { files: usize },
    /// Diff phase is starting — comparing parsed docs against the stored graph
    /// to identify changed/unchanged/deleted sources, and loading graph-primed
    /// context for extraction. Bar driver shows a "Diffing" spinner here so
    /// users see real progress instead of a misleading "waiting for LLM" while
    /// CozoDB queries run.
    DiffStart,
    /// Diff phase finished. Counts let the driver render an honest summary
    /// (e.g. "12 changed · 188 unchanged · 0 deleted") and decide whether to
    /// expect any later phases at all.
    DiffComplete {
        changed: usize,
        unchanged: usize,
        deleted: usize,
    },
    /// Extraction is starting. Includes batch sizing so the UI can explain
    /// what work is about to happen before the first batch returns.
    ExtractionStart {
        total_chunks: usize,
        batch_size: usize,
        total_batches: usize,
    },
    /// A batch of extraction work has started running.
    ExtractionBatchStart {
        batch_index: usize,
        total_batches: usize,
        range_start: usize,
        range_end: usize,
        batch_chunks: usize,
    },
    /// One original chunk processed (cache hit or LLM result).
    ChunkDone {
        done: usize,
        total: usize,
        source_uri: String,
    },
    /// All chunks extracted. Summary data for solidifying the bar.
    ExtractionComplete {
        claims: usize,
        entities: usize,
        cache_hits: usize,
    },
    /// Some LLM batches failed permanently (retries exhausted) and the
    /// claims they would have produced are missing.  Emitted after
    /// `ExtractionComplete` only when `failed_batches > 0`.  Pre-fix
    /// these failures were silently dropped — the user only saw "ok"
    /// even though their compile was incomplete.
    ExtractionPartial {
        failed_batches: usize,
        failed_chunk_ranges: Vec<(usize, usize)>,
    },
    /// Grounding tribunal is starting (runs between extraction and linking).
    GroundingStart {
        llm_claims: usize,
        structural_claims: usize,
    },
    /// NLI model finished loading — tribunal is now actively processing claims.
    /// Fired once, between GroundingStart and the first GroundingProgress event.
    GroundingModelReady,
    /// One batch of claims grounded. Drives the real progress bar.
    GroundingProgress { done: usize, total: usize },
    /// Grounding tribunal finished. `accepted` = claims that survived.
    GroundingDone { accepted: usize, rejected: usize },
    /// Fingerprint check finished. `cutoffs` = sources skipped by fingerprint match.
    FingerprintDone {
        truly_changed: usize,
        cutoffs: usize,
    },
    /// Entity resolution is starting.
    LinkingStart { total_entities: usize },
    /// One entity resolved (created or merged).
    EntityResolved { done: usize, total: usize },
    /// Linking finished.
    LinkComplete {
        entities: usize,
        relations: usize,
        contradictions: usize,
    },
    /// Vector index update finished.
    VectorUpdateDone {
        entities_indexed: usize,
        claims_indexed: usize,
    },
    /// Incremental vector upsert progress.
    VectorProgress { done: usize, total: usize },
    /// Artifact compilation finished.
    CompilationDone { artifacts: usize },
    /// One artifact compiled. Drives the real progress bar.
    CompilationProgress { done: usize, total: usize },
    /// Verification finished.
    VerificationDone { health: u8 },
    /// Rooting is starting — total candidate count.
    RootingStart { candidates: usize },
    /// One claim tried by the Rooter.
    RootingProgress { done: usize, total: usize },
    /// Rooting finished. Tier counts summarize the outcome.
    RootingDone {
        rooted: usize,
        attested: usize,
        quarantined: usize,
        rejected: usize,
    },
    /// The pipeline returned `Err(_)`. Emitted by the public `run_pipeline`
    /// wrapper before the channel closes, so the bar driver can finalise any
    /// in-flight bars with a failure style instead of the ambiguous "skipped"
    /// dim dash.
    PipelineFailed { error: String },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PipelineResult {
    pub files_parsed: usize,
    pub claims_count: usize,
    pub entities_count: usize,
    pub relations_count: usize,
    pub contradictions_count: usize,
    pub artifacts_count: usize,
    pub health_score: u8,
    pub cache_hits: usize,
    pub early_cutoffs: usize,
    pub structural_extractions: usize,
    /// `true` when the pipeline wrote at least one change to CozoDB.
    /// `false` means all files were fingerprint-identical — the cache is still
    /// current and the caller should skip the reload entirely.
    pub cache_dirty: bool,
    /// LLM batches that exhausted retries during extraction.  Non-zero
    /// means the compile is partial — claims are missing for chunks in
    /// `failed_chunk_ranges`.  Surfaced so the CLI can print a yellow
    /// warning and the desktop can render a non-fatal toast.
    #[serde(default)]
    pub failed_batches: usize,
    /// `(range_start, range_end)` chunk ranges (inclusive, 1-indexed) of
    /// every batch that failed permanently.  Identical wire shape to
    /// `ProgressEvent::ExtractionBatchStart::range_*` so callers don't
    /// need a second vocabulary.
    #[serde(default)]
    pub failed_chunk_ranges: Vec<(usize, usize)>,
}

/// Run the v3 pipeline: Parse → Extract+Ground+Rooting+Link+SVO →
/// CozoDB persist. The 3 user-visible phases (Parse / Extract /
/// Pack+Sign) of the v3 final plan §5 are realised here as Parse +
/// Extract; Pack+Sign lives in `tr-format` / `tr-sigstore` and runs
/// only when the user invokes `root pack`.
///
/// Vector indexing, markdown artifacts, and post-compile health
/// verification are NOT part of `root compile` — they live in
/// dedicated commands (`root query` / `root render` / `root health`)
/// per v3 spec §11. Skipping them at compile time is what lets the
/// 3-phase pipeline finish in ~30s instead of ~3min.
pub async fn run_pipeline(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
) -> Result<PipelineResult> {
    // Existing callers (CLI, MCP stdio, integration tests) get a token
    // that is never tripped — same behaviour as before this fix.  The
    // desktop calls `run_pipeline_with_cancel` directly with its own
    // token so it can implement the Stop button.
    run_pipeline_with_cancel(root_path, branch, progress, CancellationToken::new()).await
}

/// Cancel-aware variant of [`run_pipeline`].  When the supplied
/// `CancellationToken` is tripped mid-run, the pipeline stops at the
/// next checkpoint, surfaces `Error::Cancelled`, and emits a
/// `ProgressEvent::PipelineFailed { error: "pipeline cancelled by caller" }`
/// so subscribed bar drivers can finalise cleanly.  Partial state
/// already persisted by Phase 4 (changed-source removal) is preserved
/// — the next compile picks it up via the fingerprint check.
pub async fn run_pipeline_with_cancel(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
    cancel: CancellationToken,
) -> Result<PipelineResult> {
    let result = run_pipeline_inner(root_path, branch, progress.clone(), cancel).await;
    if let Err(ref e) = result
        && let Some(ref tx) = progress
    {
        let _ = tx.send(ProgressEvent::PipelineFailed {
            error: e.to_string(),
        });
    }
    result
}

async fn run_pipeline_inner(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
    cancel: CancellationToken,
) -> Result<PipelineResult> {
    // Helper macro — every long-running phase boundary checks this so
    // Stop / Ctrl-C never has to wait for the next batch to finish.
    macro_rules! bail_if_cancelled {
        () => {
            if cancel.is_cancelled() {
                return Err(thinkingroot_core::Error::Cancelled);
            }
        };
    }
    macro_rules! emit {
        ($event:expr) => {
            if let Some(ref tx) = progress {
                let _ = tx.send($event);
            }
        };
    }

    let config = Config::load_merged(root_path)?;
    let data_dir = thinkingroot_branch::snapshot::resolve_data_dir(root_path, branch);
    std::fs::create_dir_all(&data_dir)?;

    // ParseStart fires *here*, after config/data-dir setup but immediately
    // before the actual parse, so the displayed "Parsing" elapsed reflects
    // only the cost of `parse_directory` itself.
    emit!(ProgressEvent::ParseStart);
    bail_if_cancelled!();
    let documents = thinkingroot_parse::parse_directory(root_path, &config.parsers)?;
    let files_parsed = documents.len();
    emit!(ProgressEvent::ParseComplete {
        files: files_parsed
    });
    bail_if_cancelled!();

    // ─── Diff phase: compare against the stored graph ──────────────────
    // Storage open + fingerprint load + content-hash scan + deletion detect
    // + graph-primed context load all live under one user-visible bar.
    emit!(ProgressEvent::DiffStart);
    let mut storage = StorageEngine::init(&data_dir).await?;
    let mut fingerprints = crate::fingerprint::FingerprintStore::load(&data_dir);

    // ─── Phase 1: Identify potentially-changed documents ───────────────
    // (content hash differs from stored — NOT yet removed from graph)
    let mut potentially_changed: Vec<_> = Vec::new();
    let mut skipped = 0usize;

    for doc in &documents {
        let existing_sources = storage.graph.find_sources_by_uri(&doc.uri)?;
        if existing_sources.len() == 1
            && !doc.content_hash.0.is_empty()
            && existing_sources[0].1 == doc.content_hash.0
        {
            skipped += 1;
        } else {
            potentially_changed.push(doc);
        }
    }

    // Detect deleted files (in graph but not in filesystem).
    let current_uris: HashSet<&str> = documents.iter().map(|d| d.uri.as_str()).collect();
    let mut deleted_sources: Vec<(String, String)> = Vec::new(); // (source_id, uri)
    for (source_id, uri, source_type) in storage.graph.get_all_sources()? {
        let is_file_backed = matches!(source_type.as_str(), "File" | "Document");
        if is_file_backed && !current_uris.contains(uri.as_str()) {
            deleted_sources.push((source_id, uri));
        }
    }

    // Diff phase ends here — emit summary so the bar driver can finalise the
    // Diffing bar with concrete counts and decide whether to expect later phases.
    emit!(ProgressEvent::DiffComplete {
        changed: potentially_changed.len(),
        unchanged: skipped,
        deleted: deleted_sources.len(),
    });

    // ─── Early exit: nothing to process ────────────────────────────────
    // Vectors are not built here — `root query` lazy-builds them on
    // first call per v3 final plan §13.1.
    if potentially_changed.is_empty() && deleted_sources.is_empty() {
        return Ok(PipelineResult {
            files_parsed,
            claims_count: 0,
            entities_count: 0,
            relations_count: 0,
            contradictions_count: 0,
            artifacts_count: 0,
            health_score: 0,
            cache_hits: 0,
            early_cutoffs: skipped,
            structural_extractions: 0,
            // All files were content-hash identical — CozoDB was not touched.
            cache_dirty: false,
            failed_batches: 0,
            failed_chunk_ranges: Vec::new(),
        });
    }

    // ─── Phase 2: Extract potentially-changed documents (with cache) ───
    let workspace_id = WorkspaceId::new();
    let cache_hits;
    let mut extraction;

    // ── Graph-Primed Context: inject known entities into extraction ──
    let known_entities = match storage.graph.get_known_entities() {
        Ok(entities) if !entities.is_empty() => {
            tracing::info!(
                "graph-primed context: {} known entities loaded",
                entities.len()
            );
            thinkingroot_extract::GraphPrimedContext::from_tuples(entities)
        }
        Ok(_) => thinkingroot_extract::GraphPrimedContext::new(Vec::new()),
        Err(e) => {
            tracing::warn!("failed to load known entities for graph-priming: {e}");
            thinkingroot_extract::GraphPrimedContext::new(Vec::new())
        }
    };

    // ── Graph-Primed Context: also inject known relations ──
    let ctx_with_relations = match storage.graph.get_known_relations() {
        Ok(relations) if !relations.is_empty() => {
            tracing::info!(
                "graph-primed context: {} known relations loaded",
                relations.len()
            );
            let known_rels: Vec<thinkingroot_extract::KnownRelation> = relations
                .into_iter()
                .map(|(from, to, rel_type)| thinkingroot_extract::KnownRelation {
                    from,
                    to,
                    relation_type: rel_type,
                })
                .collect();
            known_entities.with_relations(known_rels)
        }
        Ok(_) => known_entities,
        Err(e) => {
            tracing::warn!("failed to load known relations for graph-priming: {e}");
            known_entities
        }
    };

    if potentially_changed.is_empty() {
        // Only deletions — no extraction needed.
        cache_hits = 0;
        extraction = thinkingroot_extract::ExtractionOutput::default();
    } else {
        let extractor = {
            // Open the in-flight checkpoint log under <data_dir>.  If
            // a previous run was interrupted mid-extract, the loader
            // surfaces those completed batches so the resume path can
            // log "resuming from N batches" without redoing them
            // (correctness is provided by the per-chunk content cache;
            // the checkpoint adds attribution + observability).
            let e = thinkingroot_extract::Extractor::new(&config)
                .await?
                .with_cache_dir(&data_dir)
                .with_known_entities(ctx_with_relations)
                .with_cancel(cancel.clone())
                .with_checkpoint(&data_dir)?;
            if let Some(ref tx) = progress {
                let tx_chunk = tx.clone();
                let pf = Arc::new(
                    move |event: thinkingroot_extract::ExtractionProgressEvent| {
                        let progress_event = match event {
                            thinkingroot_extract::ExtractionProgressEvent::Start {
                                total_chunks,
                                batch_size,
                                total_batches,
                            } => ProgressEvent::ExtractionStart {
                                total_chunks,
                                batch_size,
                                total_batches,
                            },
                            thinkingroot_extract::ExtractionProgressEvent::BatchStart {
                                batch_index,
                                total_batches,
                                range_start,
                                range_end,
                                batch_chunks,
                            } => ProgressEvent::ExtractionBatchStart {
                                batch_index,
                                total_batches,
                                range_start,
                                range_end,
                                batch_chunks,
                            },
                            thinkingroot_extract::ExtractionProgressEvent::ChunkDone {
                                done,
                                total,
                                source_uri,
                            } => ProgressEvent::ChunkDone {
                                done,
                                total,
                                source_uri,
                            },
                        };
                        let _ = tx_chunk.send(progress_event);
                    },
                ) as thinkingroot_extract::ChunkProgressFn;
                e.with_progress(pf)
            } else {
                e
            }
        };
        let raw = extractor
            .extract_all(
                &potentially_changed
                    .iter()
                    .map(|d| (*d).clone())
                    .collect::<Vec<_>>(),
                workspace_id,
            )
            .await?;
        emit!(ProgressEvent::ExtractionComplete {
            claims: raw.claims.len(),
            entities: raw.entities.len(),
            cache_hits: raw.cache_hits,
        });
        if raw.failed_batches > 0 {
            tracing::warn!(
                failed_batches = raw.failed_batches,
                "extraction completed with permanent batch failures — emitting partial event"
            );
            emit!(ProgressEvent::ExtractionPartial {
                failed_batches: raw.failed_batches,
                failed_chunk_ranges: raw.failed_batch_ranges.clone(),
            });
        }
        cache_hits = raw.cache_hits;
        extraction = raw;
    }

    // Log tiered extraction stats.
    if extraction.structural_extractions > 0 {
        tracing::info!(
            "tiered extraction: {} structural (zero LLM), {} cache hits, {} LLM calls",
            extraction.structural_extractions,
            extraction.cache_hits,
            extraction
                .chunks_processed
                .saturating_sub(extraction.cache_hits + extraction.structural_extractions),
        );
    }

    // ─── Phase 2b: Cascade Grounding ─────────────────────────────────────────────────
    // Structural claims (from AST) are auto-grounded at 0.99 — skip tribunal.
    // LLM claims run the full 4-judge grounding tribunal (unchanged behavior).
    //
    // IMPORTANT: We partition claims before passing to the grounder so that
    // the tribunal cannot overwrite auto-grounded structural scores. The
    // structural claims are merged back after the tribunal completes.
    //
    // NLI model is embedded in the binary (no downloads). Pool creation is
    // cheap (just RAM detection), but we still use spawn_blocking because
    // ONNX session creation from memory is CPU-heavy.

    bail_if_cancelled!();

    // Partition: structural claims get 0.99, LLM claims go to tribunal.
    let (llm_claims, mut structural_claims): (Vec<_>, Vec<_>) = extraction
        .claims
        .into_iter()
        .partition(|c| c.extraction_tier == thinkingroot_core::types::ExtractionTier::Llm);

    emit!(ProgressEvent::GroundingStart {
        llm_claims: llm_claims.len(),
        structural_claims: structural_claims.len(),
    });

    // Auto-ground structural claims.
    let structural_count = structural_claims.len();
    for claim in &mut structural_claims {
        claim.grounding_score = Some(0.99);
        claim.grounding_method = Some(thinkingroot_core::types::GroundingMethod::Structural);
    }
    if structural_count > 0 {
        tracing::info!(
            "cascade grounding: {} structural claims auto-grounded at 0.99 (skipped tribunal)",
            structural_count
        );
    }

    // Run tribunal on LLM claims only.
    let grounded_llm_claims = if !llm_claims.is_empty() {
        #[cfg(feature = "vector")]
        let nli_pool = {
            let data_dir_clone = data_dir.clone();
            let result = match tokio::task::spawn_blocking(move || {
                thinkingroot_ground::NliJudgePool::load(Some(&data_dir_clone))
            })
            .await
            {
                Ok(Ok(pool)) => {
                    tracing::info!("NLI pool ready: {} parallel workers", pool.num_workers);
                    Some(pool)
                }
                Ok(Err(e)) => {
                    tracing::warn!("NLI pool unavailable, using Judges 1-3 only: {e}");
                    None
                }
                Err(e) => {
                    tracing::warn!("NLI pool load task failed: {e}, using Judges 1-3 only");
                    None
                }
            };
            // Signal the progress bar — model is loaded, tribunal is starting.
            emit!(ProgressEvent::GroundingModelReady);
            result
        };

        extraction.claims = llm_claims;
        let pre_count = extraction.claims.len();
        let grounder = {
            let g =
                thinkingroot_ground::Grounder::new(thinkingroot_ground::GroundingConfig::default());
            if let Some(ref tx) = progress {
                let tx_ground = tx.clone();
                let pf = Arc::new(move |done: usize, total: usize| {
                    let _ = tx_ground.send(ProgressEvent::GroundingProgress { done, total });
                }) as thinkingroot_ground::GroundingProgressFn;
                g.with_progress(pf)
            } else {
                g
            }
        };
        // block_in_place: grounder.ground() is a long synchronous CPU/ONNX operation.
        // Telling tokio this thread will block lets it keep the spawned bar_driver task
        // and other async work scheduled on the remaining threads.
        let mut grounded = tokio::task::block_in_place(|| {
            grounder.ground(
                extraction,
                #[cfg(feature = "vector")]
                Some(&mut storage.vector),
                #[cfg(feature = "vector")]
                nli_pool.as_ref(),
            )
        });
        thinkingroot_ground::dedup::dedup_claims(&mut grounded.claims);
        let post_count = grounded.claims.len();
        if pre_count != post_count {
            tracing::info!(
                "grounding: {} → {} LLM claims ({} rejected/deduped)",
                pre_count,
                post_count,
                pre_count - post_count,
            );
        }
        grounded
    } else {
        // All claims are structural — rebuild extraction with empty claims vec.
        extraction.claims = Vec::new();
        extraction
    };

    // Merge: structural claims (0.99 grounding) + surviving LLM claims.
    let pre_grounding_total = grounded_llm_claims.claims.len() + structural_claims.len();
    extraction = grounded_llm_claims;
    extraction.claims.extend(structural_claims);
    thinkingroot_ground::dedup::dedup_claims(&mut extraction.claims);
    let post_grounding_total = extraction.claims.len();

    emit!(ProgressEvent::GroundingDone {
        accepted: post_grounding_total,
        rejected: pre_grounding_total.saturating_sub(post_grounding_total),
    });

    // Phase 2c (SVO event extraction) is intentionally deferred to Phase 2c-post-link
    // below.  It must run AFTER Phase 7 (Linker) so that entity names can be resolved
    // to their real CozoDB ULIDs.  Running it here (before entities exist) would
    // produce events with wrong / empty entity references, breaking the event calendar.

    // ─── Phase 3: Fingerprint check ────────────────────────────────────
    // For each potentially-changed doc, compute a fingerprint of its extracted
    // claims. If identical to stored fingerprint, skip this source entirely.
    let mut truly_changed: Vec<_> = Vec::new();
    let mut fingerprint_cutoffs = 0usize;

    for doc in &potentially_changed {
        // Collect claims for this source and serialize as fingerprint input.
        let source_claims: Vec<_> = extraction
            .claims
            .iter()
            .filter(|c| c.source == doc.source_id)
            .collect();
        let fp_bytes = serde_json::to_vec(&source_claims).unwrap_or_default();
        let fp = crate::fingerprint::FingerprintStore::compute(&fp_bytes);

        if fingerprints.is_unchanged(&doc.uri, &fp) {
            fingerprint_cutoffs += 1;
            tracing::debug!("fingerprint early cutoff for {}", doc.uri);
        } else {
            fingerprints.update(&doc.uri, fp);
            truly_changed.push(*doc);
        }
    }

    emit!(ProgressEvent::FingerprintDone {
        truly_changed: truly_changed.len(),
        cutoffs: fingerprint_cutoffs,
    });

    bail_if_cancelled!();

    // ─── Phase 4: Remove changed + deleted sources from graph ──────────
    let mut affected_triples: Vec<(String, String, String)> = Vec::new();

    for doc in &truly_changed {
        let existing_sources = storage.graph.find_sources_by_uri(&doc.uri)?;
        if !existing_sources.is_empty() {
            for (source_id, _, _) in &existing_sources {
                affected_triples.extend(storage.graph.get_source_relation_triples(source_id)?);
                let entity_ids_from_source = storage.graph.get_entity_ids_for_source(source_id)?;
                if !entity_ids_from_source.is_empty() {
                    let cross_file_triples = storage
                        .graph
                        .get_all_triples_involving_entities(&entity_ids_from_source)?;
                    let cross_file_count = cross_file_triples.len();
                    affected_triples.extend(cross_file_triples);
                    tracing::debug!(
                        "cross-file staleness: {} entity ids, {} cross-file triples added for source {}",
                        entity_ids_from_source.len(),
                        cross_file_count,
                        source_id
                    );
                }
            }
            storage.graph.remove_source_by_uri(&doc.uri)?;
        }
    }

    for (source_id, uri) in &deleted_sources {
        affected_triples.extend(storage.graph.get_source_relation_triples(source_id)?);
        let entity_ids_from_source = storage.graph.get_entity_ids_for_source(source_id)?;
        if !entity_ids_from_source.is_empty() {
            let cross_file_triples = storage
                .graph
                .get_all_triples_involving_entities(&entity_ids_from_source)?;
            let cross_file_count = cross_file_triples.len();
            affected_triples.extend(cross_file_triples);
            tracing::debug!(
                "cross-file staleness: {} entity ids, {} cross-file triples added for source {}",
                entity_ids_from_source.len(),
                cross_file_count,
                source_id
            );
        }
        storage.graph.remove_source_by_uri(uri)?;
        fingerprints.remove(uri);
    }

    // ─── Phase 5: Incremental entity relation update for removals ──────
    if !affected_triples.is_empty() {
        affected_triples.sort_unstable();
        affected_triples.dedup();
        storage
            .graph
            .update_entity_relations_for_triples(&affected_triples)?;
    }

    // If only deletions or all fingerprint hits — no new content to link.
    if truly_changed.is_empty() {
        emit!(ProgressEvent::LinkComplete {
            entities: 0,
            relations: 0,
            contradictions: 0
        });

        fingerprints.save()?;
        config.save(root_path)?;

        // Health/artifacts/contradictions are surfaced by `root health`
        // and `root render` — `root compile` only persists the graph.
        return Ok(PipelineResult {
            files_parsed,
            claims_count: 0,
            entities_count: 0,
            relations_count: 0,
            contradictions_count: 0,
            artifacts_count: 0,
            health_score: 0,
            cache_hits,
            early_cutoffs: skipped + fingerprint_cutoffs,
            structural_extractions: extraction.structural_extractions,
            // Deletions or fingerprint cutoffs mutated CozoDB — cache is stale.
            cache_dirty: true,
            failed_batches: extraction.failed_batches,
            failed_chunk_ranges: extraction.failed_batch_ranges.clone(),
        });
    }

    bail_if_cancelled!();

    // ─── Phase 6: Insert sources for truly-changed documents ───────────
    // Also persist source bytes to the durable Rooting byte-store so Phase 6.5
    // (and future re-rooting sweeps) can re-execute probes against them. The
    // byte-store is content-addressed, so multiple writes with the same hash
    // are no-ops — fresh recompiles of an unchanged file cost zero extra I/O.
    let byte_store = thinkingroot_rooting::FileSystemSourceStore::new(&data_dir)
        .map_err(|e| thinkingroot_core::Error::Config(format!("rooting byte store: {e}")))?;
    for doc in &truly_changed {
        let source = thinkingroot_core::Source::new(doc.uri.clone(), doc.source_type)
            .with_id(doc.source_id)
            .with_hash(doc.content_hash.clone());
        storage.graph.insert_source(&source)?;

        // Reconstruct the text used during extraction (chunks joined by \n).
        // This matches what the Grounder saw, so provenance probe results line
        // up with Phase 2b's tribunal when re-rooted later.
        let text: String = doc
            .chunks
            .iter()
            .map(|c| c.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        use thinkingroot_rooting::SourceByteStore;
        byte_store
            .put(doc.source_id, &doc.content_hash, text.as_bytes())
            .map_err(|e| thinkingroot_core::Error::Config(format!("rooting put: {e}")))?;
    }

    // Filter extraction to only truly-changed sources.
    let truly_changed_ids: HashSet<thinkingroot_core::types::SourceId> =
        truly_changed.iter().map(|d| d.source_id).collect();

    let structural_extractions = extraction.structural_extractions;

    let mut filtered_extraction = thinkingroot_extract::ExtractionOutput {
        claims: extraction
            .claims
            .into_iter()
            .filter(|c| truly_changed_ids.contains(&c.source))
            .collect(),
        entities: extraction.entities,
        relations: extraction
            .relations
            .into_iter()
            .filter(|r| truly_changed_ids.contains(&r.source))
            .collect(),
        claim_entity_names: extraction.claim_entity_names,
        sources_processed: truly_changed.len(),
        chunks_processed: extraction.chunks_processed,
        cache_hits: extraction.cache_hits,
        structural_extractions: extraction.structural_extractions,
        source_texts: extraction.source_texts,
        claim_source_quotes: extraction.claim_source_quotes,
        // Carry partial-failure attribution forward so consumers downstream
        // (currently the pipeline summary; soon the desktop UI per C4)
        // can render an honest "N batches failed" warning.
        failed_batches: extraction.failed_batches,
        failed_batch_ranges: extraction.failed_batch_ranges,
    };

    // ─── Phase 6.5: Rooting ────────────────────────────────────────────
    // Deterministic admission gate. Each candidate claim faces five probes
    // (provenance, contradiction, predicate, topology, temporal). Rejected
    // claims are removed from the extraction before Link sees them.
    //
    // Disabled when either the workspace config opts out
    // (`config.rooting.disabled`) or the per-invocation env flag is set
    // (`TR_ROOTING_DISABLED=1`, populated by `root compile --no-rooting`).
    let rooting_disabled_env = std::env::var("TR_ROOTING_DISABLED")
        .map(|v| v == "1")
        .unwrap_or(false);
    if !config.rooting.disabled && !rooting_disabled_env && !filtered_extraction.claims.is_empty() {
        let rooting_cfg = thinkingroot_rooting::RootingConfig {
            disabled: config.rooting.disabled,
            provenance_threshold: config.rooting.provenance_threshold,
            contradiction_floor: config.rooting.contradiction_floor,
            contribute_gate: config.rooting.contribute_gate.clone(),
            predicate_strength_threshold: config.rooting.predicate_strength_threshold,
        };
        let candidates_total = filtered_extraction.claims.len();
        emit!(ProgressEvent::RootingStart {
            candidates: candidates_total
        });

        let candidates: Vec<thinkingroot_rooting::CandidateClaim<'_>> = filtered_extraction
            .claims
            .iter()
            .map(|c| thinkingroot_rooting::CandidateClaim {
                claim: c,
                predicate: c.predicate.as_ref(),
                derivation: c.derivation.as_ref(),
            })
            .collect();

        let rooter = {
            let r = thinkingroot_rooting::Rooter::new(&storage.graph, &byte_store, rooting_cfg);
            if let Some(ref tx) = progress {
                let tx_root = tx.clone();
                let pf: thinkingroot_rooting::RootingProgressFn =
                    Arc::new(move |done: usize, total: usize| {
                        let _ = tx_root.send(ProgressEvent::RootingProgress { done, total });
                    });
                r.with_progress(pf)
            } else {
                r
            }
        };

        let rooting_output = rooter
            .root_batch(&candidates)
            .map_err(|e| thinkingroot_core::Error::Config(format!("rooting: {e}")))?;

        // Drop Rejected claims from the extraction so Link never sees them.
        let rejected_ids: std::collections::HashSet<thinkingroot_core::ClaimId> = rooting_output
            .verdicts
            .iter()
            .filter(|v| v.admission_tier == thinkingroot_core::types::AdmissionTier::Rejected)
            .map(|v| v.claim_id)
            .collect();
        if !rejected_ids.is_empty() {
            filtered_extraction
                .claims
                .retain(|c| !rejected_ids.contains(&c.id));
        }

        // Stamp survivors with their tier + last_rooted_at.
        let tier_map: std::collections::HashMap<thinkingroot_core::ClaimId, _> = rooting_output
            .verdicts
            .iter()
            .map(|v| (v.claim_id, (v.admission_tier, v.trial_at)))
            .collect();
        for c in &mut filtered_extraction.claims {
            if let Some((tier, trial_at)) = tier_map.get(&c.id) {
                c.admission_tier = *tier;
                c.last_rooted_at = Some(*trial_at);
            }
        }

        // Persist verdicts + certificates into CozoDB.
        thinkingroot_rooting::storage::insert_verdicts_batch(
            &storage.graph,
            &rooting_output.verdicts,
        )
        .map_err(|e| thinkingroot_core::Error::Config(format!("rooting verdicts: {e}")))?;
        thinkingroot_rooting::storage::insert_certificates_batch(
            &storage.graph,
            &rooting_output.certificates,
        )
        .map_err(|e| thinkingroot_core::Error::Config(format!("rooting certificates: {e}")))?;

        let rooted = rooting_output
            .verdicts
            .iter()
            .filter(|v| v.admission_tier == thinkingroot_core::types::AdmissionTier::Rooted)
            .count();
        let attested = rooting_output
            .verdicts
            .iter()
            .filter(|v| v.admission_tier == thinkingroot_core::types::AdmissionTier::Attested)
            .count();
        emit!(ProgressEvent::RootingDone {
            rooted,
            attested,
            quarantined: rooting_output.quarantined_count,
            rejected: rooting_output.rejected_count,
        });
        tracing::info!(
            "rooting: {} rooted, {} attested, {} quarantined, {} rejected",
            rooted,
            attested,
            rooting_output.quarantined_count,
            rooting_output.rejected_count
        );
    }

    let claims_count = filtered_extraction.claims.len();
    let entities_count = filtered_extraction.entities.len();
    let relations_count = filtered_extraction.relations.len();
    // Snapshot failed-batch attribution before Linker moves the
    // extraction.  The PipelineResult needs them at the very end so
    // the CLI/desktop can render an honest partial-failure summary.
    let failed_batches = filtered_extraction.failed_batches;
    let failed_chunk_ranges = filtered_extraction.failed_batch_ranges.clone();

    // Retain a lightweight clone of the filtered claims for Phase 2c-post-link
    // (SVO event extraction).  We clone before the linker takes ownership so that
    // the post-link phase has access to statements + event_date timestamps.
    let claims_for_svo: Vec<thinkingroot_core::Claim> = filtered_extraction.claims.clone();

    bail_if_cancelled!();

    // ─── Phase 7: Link ─────────────────────────────────────────────────
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
    let link_output = linker.link(filtered_extraction)?;
    emit!(ProgressEvent::LinkComplete {
        entities: link_output.entities_created + link_output.entities_merged,
        relations: link_output.relations_linked,
        contradictions: link_output.contradictions_detected,
    });

    // ─── Phase 2c-post-link: SVO Event Calendar ──────────────────────────
    // Now that Phase 7 has written all entities to CozoDB, we can build the
    // complete entity_name → ULID map and extract SVO events with correct IDs.
    //
    // This is the world-class temporal memory architecture:
    //   compile time  → events table populated with real entity ULIDs
    //   query time    → 50µs Datalog range scan (vs Chronos 100-200ms)
    //
    // Non-fatal: event calendar failure must never abort the pipeline.
    {
        let entity_name_to_id: std::collections::HashMap<String, String> = storage
            .graph
            .get_all_entities()
            .unwrap_or_default()
            .into_iter()
            .map(|(id, name, _)| (name.to_lowercase(), id))
            .collect();

        if entity_name_to_id.is_empty() {
            tracing::warn!("event calendar: entity table empty after linking — skipping");
        } else {
            let extractor = thinkingroot_extract::EventExtractor::new();
            let extracted_events =
                extractor.extract_from_claims(&claims_for_svo, &entity_name_to_id);

            if !extracted_events.is_empty() {
                match storage.graph.insert_events(&extracted_events) {
                    Ok(n) => tracing::info!(
                        count = n,
                        entities = entity_name_to_id.len(),
                        "event calendar: SVO events compiled with correct entity IDs"
                    ),
                    Err(e) => tracing::warn!("event calendar: insertion failed (non-fatal): {e}"),
                }
            } else {
                tracing::info!(
                    "event calendar: no SVO events found in {} claims",
                    claims_for_svo.len()
                );
            }
        }
    }

    // ─── Phase 8: Incremental entity relation update for new sources ───
    let mut new_triples: Vec<(String, String, String)> = Vec::new();
    for doc in &truly_changed {
        new_triples.extend(
            storage
                .graph
                .get_source_relation_triples(&doc.source_id.to_string())?,
        );
    }
    if new_triples.is_empty() && link_output.relations_linked > 0 {
        tracing::warn!(
            "relations were linked ({}) but no source relation triples found; \
             entity_relations may be stale",
            link_output.relations_linked
        );
    }
    new_triples.sort_unstable();
    new_triples.dedup();
    storage
        .graph
        .update_entity_relations_for_triples(&new_triples)?;

    // Vector index, markdown artifacts, and post-compile health
    // verification are NOT part of `root compile` in v3 — they live
    // in `root query` (which lazily builds the index on first call),
    // `root render`, and `root health` respectively. Per v3 final
    // plan §5.4 / §11.

    fingerprints.save()?;
    config.save(root_path)?;

    // Phase 7 succeeded — CozoDB is now the source of truth.  Clear the
    // in-flight checkpoint log so the next compile starts fresh.
    // Failure is non-fatal (a stale .in-flight.jsonl just means the
    // next run logs a misleading "resuming" message, then produces
    // identical output via cache hits).
    if let Err(e) = thinkingroot_extract::InFlightCheckpoint::clear(&data_dir) {
        tracing::warn!("failed to clear in-flight checkpoint after Phase 7: {e}");
    }

    Ok(PipelineResult {
        files_parsed,
        claims_count,
        entities_count,
        relations_count,
        contradictions_count: 0,
        artifacts_count: 0,
        health_score: 0,
        cache_hits,
        early_cutoffs: skipped + fingerprint_cutoffs,
        structural_extractions,
        // v3 pipeline ran — CozoDB has new data.
        cache_dirty: true,
        failed_batches,
        failed_chunk_ranges,
    })
}

/// Rebuild the vector index from the persisted CozoDB graph. Used by
/// `root query` / `root ask` on first call after a v3 `root compile`,
/// since v3 `root compile` deliberately does not embed (consumers
/// choose their own embedding model per v3 final plan §13.1).
///
/// Resets the existing index, embeds every entity + claim currently
/// in the graph, and saves to disk. Returns
/// `(entities_indexed, claims_indexed)`.
pub fn rebuild_vector_index(storage: &mut StorageEngine) -> Result<(usize, usize)> {
    storage.vector.reset();

    let entities = storage.graph.get_all_entities()?;
    let claims = storage.graph.get_all_claims_with_sources()?;

    let entity_items: Vec<(String, String, String)> = entities
        .iter()
        .map(|(id, name, etype)| {
            (
                format!("entity:{id}"),
                format!("{name} ({etype})"),
                format!("entity|{id}|{name}|{etype}"),
            )
        })
        .collect();

    let claim_items: Vec<(String, String, String)> = claims
        .iter()
        .map(|(id, statement, ctype, conf, uri, _)| {
            (
                format!("claim:{id}"),
                statement.clone(),
                format!("claim|{id}|{ctype}|{conf}|{uri}"),
            )
        })
        .collect();

    let entity_count = upsert_in_chunks(&mut storage.vector, &entity_items, 512)?;
    let claim_count = upsert_in_chunks(&mut storage.vector, &claim_items, 512)?;
    storage.vector.save()?;

    Ok((entity_count, claim_count))
}

fn upsert_in_chunks(
    vector: &mut thinkingroot_graph::vector::VectorStore,
    items: &[(String, String, String)],
    chunk_size: usize,
) -> Result<usize> {
    let mut done = 0usize;
    for chunk in items.chunks(chunk_size) {
        vector.upsert_batch(chunk)?;
        done += chunk.len();
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-cancelled tokens short-circuit the pipeline before any
    /// parsing or LLM work — the `bail_if_cancelled!()` checkpoint that
    /// fires after `ProgressEvent::ParseStart` and before
    /// `thinkingroot_parse::parse_directory` returns
    /// `Err(Error::Cancelled)`.  This is the foundational guarantee the
    /// desktop "Stop compile" button relies on (P3.4).
    #[tokio::test]
    async fn pre_cancelled_token_aborts_before_parse() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Touch a file so parse_directory wouldn't trivially return empty.
        std::fs::write(tmp.path().join("hello.md"), "# hello\n\nbody.").unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = run_pipeline_with_cancel(tmp.path(), None, None, cancel)
            .await
            .expect_err("pre-cancelled token must produce Err");
        assert!(
            matches!(err, thinkingroot_core::Error::Cancelled),
            "expected Error::Cancelled, got {err:?}"
        );
    }

    /// A fresh, never-tripped token must behave exactly like the old
    /// `run_pipeline` API — empty workspaces still report parse=0 with
    /// no error.  Guards against accidental tightening of the cancel
    /// check (e.g. an `if !is_cancelled` typo).
    #[tokio::test]
    async fn untripped_token_runs_to_completion_on_empty_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = run_pipeline_with_cancel(tmp.path(), None, None, CancellationToken::new())
            .await
            .expect("untripped token must not abort an empty compile");
        assert_eq!(result.files_parsed, 0);
        assert_eq!(result.claims_count, 0);
        assert!(!result.cache_dirty, "empty compile must not dirty cache");
    }
}
