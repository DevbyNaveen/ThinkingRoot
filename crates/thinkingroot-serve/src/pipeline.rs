use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

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
}

pub async fn run_pipeline(
    root_path: &Path,
    branch: Option<&str>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
) -> Result<PipelineResult> {
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

    let documents = thinkingroot_parse::parse_directory(root_path, &config.parsers)?;
    let files_parsed = documents.len();
    emit!(ProgressEvent::ParseComplete { files: files_parsed });

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

    // ─── Early exit: nothing to process ────────────────────────────────
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
        });
    }

    // ─── Phase 2: Extract potentially-changed documents (with cache) ───
    let workspace_id = WorkspaceId::new();
    let cache_hits;
    let mut extraction;

    // ── Graph-Primed Context: inject known entities into extraction ──
    let known_entities = match storage.graph.get_known_entities() {
        Ok(entities) if !entities.is_empty() => {
            tracing::info!("graph-primed context: {} known entities loaded", entities.len());
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
            tracing::info!("graph-primed context: {} known relations loaded", relations.len());
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
            let e = thinkingroot_extract::Extractor::new(&config)
                .await?
                .with_cache_dir(&data_dir)
                .with_known_entities(ctx_with_relations);
            if let Some(ref tx) = progress {
                let tx_chunk = tx.clone();
                let pf = Arc::new(move |done: usize, total: usize, uri: &str| {
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
        let raw = extractor
            .extract_all(
                &potentially_changed.iter().map(|d| (*d).clone()).collect::<Vec<_>>(),
                workspace_id,
            )
            .await?;
        emit!(ProgressEvent::ExtractionComplete {
            claims: raw.claims.len(),
            entities: raw.entities.len(),
            cache_hits: raw.cache_hits,
        });
        cache_hits = raw.cache_hits;
        extraction = raw;
    }

    // Log tiered extraction stats.
    if extraction.structural_extractions > 0 {
        tracing::info!(
            "tiered extraction: {} structural (zero LLM), {} cache hits, {} LLM calls",
            extraction.structural_extractions,
            extraction.cache_hits,
            extraction.chunks_processed.saturating_sub(extraction.cache_hits + extraction.structural_extractions),
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
    // NliJudge::load() uses hf-hub's sync API (reqwest::blocking) which must
    // NOT be called from within an async context — it creates a nested Tokio
    // runtime that deadlocks the worker thread. We use spawn_blocking to move
    // it onto a dedicated blocking thread.

    // Partition: structural claims get 0.99, LLM claims go to tribunal.
    let (llm_claims, mut structural_claims): (Vec<_>, Vec<_>) = extraction.claims
        .into_iter()
        .partition(|c| c.extraction_tier == thinkingroot_core::types::ExtractionTier::Llm);

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
        let mut nli_judge = {
            let data_dir_clone = data_dir.clone();
            match tokio::task::spawn_blocking(move || {
                thinkingroot_ground::NliJudge::load(Some(&data_dir_clone))
            })
            .await
            {
                Ok(Ok(judge)) => Some(judge),
                Ok(Err(e)) => {
                    tracing::warn!("NLI model (Judge 4) unavailable, using Judges 1-3 only: {e}");
                    None
                }
                Err(e) => {
                    tracing::warn!("NLI model load task failed: {e}, using Judges 1-3 only");
                    None
                }
            }
        };

        extraction.claims = llm_claims;
        let pre_count = extraction.claims.len();
        let mut grounded = thinkingroot_ground::Grounder::new(
            thinkingroot_ground::GroundingConfig::default(),
        )
        .ground(
            extraction,
            #[cfg(feature = "vector")]
            Some(&mut storage.vector),
            #[cfg(feature = "vector")]
            nli_judge.as_mut(),
        );
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
    extraction = grounded_llm_claims;
    extraction.claims.extend(structural_claims);
    thinkingroot_ground::dedup::dedup_claims(&mut extraction.claims);

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
        let fp_bytes = serde_json::to_vec(&source_claims)
            .unwrap_or_default();
        let fp = crate::fingerprint::FingerprintStore::compute(&fp_bytes);

        if fingerprints.is_unchanged(&doc.uri, &fp) {
            fingerprint_cutoffs += 1;
            tracing::debug!("fingerprint early cutoff for {}", doc.uri);
        } else {
            fingerprints.update(&doc.uri, fp);
            truly_changed.push(*doc);
        }
    }

    // ─── Phase 4: Remove changed + deleted sources from graph ──────────
    let mut affected_triples: Vec<(String, String, String)> = Vec::new();
    let mut changed = 0usize;
    let mut deleted = 0usize;

    let mut stale_claim_vector_ids: Vec<String> = Vec::new();
    let mut stale_entity_candidate_ids: Vec<String> = Vec::new();

    for doc in &truly_changed {
        let existing_sources = storage.graph.find_sources_by_uri(&doc.uri)?;
        if !existing_sources.is_empty() {
            for (source_id, _, _) in &existing_sources {
                affected_triples
                    .extend(storage.graph.get_source_relation_triples(source_id)?);
                // Capture stale vector entries before removal.
                for cid in storage.graph.get_claim_ids_for_source(source_id)? {
                    stale_claim_vector_ids.push(format!("claim:{cid}"));
                }
                for eid in storage.graph.get_entity_ids_for_source(source_id)? {
                    stale_entity_candidate_ids.push(format!("entity:{eid}"));
                }
            }
            storage.graph.remove_source_by_uri(&doc.uri)?;
            changed += 1;
        }
    }

    for (source_id, uri) in &deleted_sources {
        affected_triples.extend(storage.graph.get_source_relation_triples(source_id)?);
        // Capture stale vector entries before removal.
        for cid in storage.graph.get_claim_ids_for_source(source_id)? {
            stale_claim_vector_ids.push(format!("claim:{cid}"));
        }
        for eid in storage.graph.get_entity_ids_for_source(source_id)? {
            stale_entity_candidate_ids.push(format!("entity:{eid}"));
        }
        storage.graph.remove_source_by_uri(uri)?;
        fingerprints.remove(uri);
        deleted += 1;
    }

    // ─── Phase 5: Incremental entity relation update for removals ──────
    if !affected_triples.is_empty() {
        affected_triples.sort_unstable();
        affected_triples.dedup();
        storage
            .graph
            .update_entity_relations_for_triples(&affected_triples)?;
    }

    let has_any_changes = changed > 0 || deleted > 0 || !truly_changed.is_empty();

    // If only deletions or all fingerprint hits — no new content to link.
    if truly_changed.is_empty() {
        update_vector_index_full(&mut storage)?;

        let compiler = thinkingroot_compile::Compiler::new(&config)?;
        let artifacts = compiler.compile_affected(&storage.graph, &data_dir, &[], has_any_changes)?;
        emit!(ProgressEvent::CompilationDone { artifacts: artifacts.len() });

        let verifier = thinkingroot_verify::Verifier::new(&config);
        let verification = verifier.verify(&storage.graph)?;
        emit!(ProgressEvent::VerificationDone {
            health: verification.health_score.as_percentage(),
        });

        fingerprints.save()?;
        config.save(root_path)?;

        return Ok(PipelineResult {
            files_parsed,
            claims_count: 0,
            entities_count: 0,
            relations_count: 0,
            contradictions_count: verification.contradictions,
            artifacts_count: artifacts.len(),
            health_score: verification.health_score.as_percentage(),
            cache_hits,
            early_cutoffs: skipped + fingerprint_cutoffs,
            structural_extractions: extraction.structural_extractions,
        });
    }

    // ─── Phase 6: Insert sources for truly-changed documents ───────────
    for doc in &truly_changed {
        let source = thinkingroot_core::Source::new(doc.uri.clone(), doc.source_type)
            .with_id(doc.source_id)
            .with_hash(doc.content_hash.clone());
        storage.graph.insert_source(&source)?;
    }

    // Filter extraction to only truly-changed sources.
    let truly_changed_ids: HashSet<thinkingroot_core::types::SourceId> =
        truly_changed.iter().map(|d| d.source_id).collect();

    let structural_extractions = extraction.structural_extractions;

    let filtered_extraction = thinkingroot_extract::ExtractionOutput {
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
    };

    let claims_count = filtered_extraction.claims.len();
    let entities_count = filtered_extraction.entities.len();
    let relations_count = filtered_extraction.relations.len();

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

    // ─── Phase 9: Vector update ─────────────────────────────────────────
    if deleted == 0 {
        // Surgical update: remove stale entries, upsert new ones.
        // Claims are always source-scoped — all stale claim IDs are safe to remove.
        // Entities may survive if other sources still reference them — only remove
        // those no longer present in the graph after removal.
        let current_entity_ids: std::collections::HashSet<String> = storage
            .graph
            .get_all_entities()?
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();

        let mut stale_ids: Vec<&str> = stale_claim_vector_ids.iter().map(|s| s.as_str()).collect();
        let truly_stale_entity_ids: Vec<String> = stale_entity_candidate_ids
            .iter()
            .filter(|id| {
                // Strip "entity:" prefix to get raw entity ID for graph lookup.
                let raw = id.strip_prefix("entity:").unwrap_or(id);
                !current_entity_ids.contains(raw)
            })
            .cloned()
            .collect();
        stale_ids.extend(truly_stale_entity_ids.iter().map(|s| s.as_str()));

        storage.vector.remove_by_ids(&stale_ids);

        // Build new vector items for affected entities and newly added claims.
        let all_entities = storage.graph.get_all_entities()?;
        let affected_set: std::collections::HashSet<&str> =
            link_output.affected_entity_ids.iter().map(|s| s.as_str()).collect();
        let new_entity_items: Vec<(String, String, String)> = all_entities
            .iter()
            .filter(|(id, _, _)| affected_set.contains(id.as_str()))
            .map(|(id, name, etype)| {
                (
                    format!("entity:{id}"),
                    format!("{name} ({etype})"),
                    format!("entity|{id}|{name}|{etype}"),
                )
            })
            .collect();

        let all_claims = storage.graph.get_all_claims_with_sources()?;
        let added_claim_set: std::collections::HashSet<&str> =
            link_output.added_claim_ids.iter().map(|s| s.as_str()).collect();
        let new_claim_items: Vec<(String, String, String)> = all_claims
            .iter()
            .filter(|(id, _, _, _, _)| added_claim_set.contains(id.as_str()))
            .map(|(id, statement, ctype, conf, uri)| {
                (
                    format!("claim:{id}"),
                    statement.clone(),
                    format!("claim|{id}|{ctype}|{conf}|{uri}"),
                )
            })
            .collect();

        storage.vector.upsert_batch(&new_entity_items)?;
        storage.vector.upsert_batch(&new_claim_items)?;
        storage.vector.save()?;

        tracing::info!(
            "vector index updated surgically: removed {}, added {} entities + {} claims",
            stale_ids.len(),
            new_entity_items.len(),
            new_claim_items.len(),
        );
    } else {
        // Deletions occurred — full rebuild to correctly handle orphaned entries.
        update_vector_index_full(&mut storage)?;
    }

    // ─── Phase 10: Selective compilation ────────────────────────────────
    let compiler = thinkingroot_compile::Compiler::new(&config)?;
    let artifacts = compiler.compile_affected(
        &storage.graph,
        &data_dir,
        &link_output.affected_entity_ids,
        true,
    )?;
    emit!(ProgressEvent::CompilationDone { artifacts: artifacts.len() });

    // ─── Phase 11: Verify + persist ─────────────────────────────────────
    let verifier = thinkingroot_verify::Verifier::new(&config);
    let verification = verifier.verify(&storage.graph)?;
    emit!(ProgressEvent::VerificationDone {
        health: verification.health_score.as_percentage(),
    });

    fingerprints.save()?;
    config.save(root_path)?;

    Ok(PipelineResult {
        files_parsed,
        claims_count,
        entities_count,
        relations_count,
        contradictions_count: verification.contradictions,
        artifacts_count: artifacts.len(),
        health_score: verification.health_score.as_percentage(),
        cache_hits,
        early_cutoffs: skipped + fingerprint_cutoffs,
        structural_extractions,
    })
}

fn update_vector_index_full(storage: &mut StorageEngine) -> Result<(usize, usize)> {
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

    let entity_count = storage.vector.upsert_batch(&entity_items)?;

    let claim_items: Vec<(String, String, String)> = claims
        .iter()
        .map(|(id, statement, ctype, conf, uri)| {
            (
                format!("claim:{id}"),
                statement.clone(),
                format!("claim|{id}|{ctype}|{conf}|{uri}"),
            )
        })
        .collect();

    let claim_count = storage.vector.upsert_batch(&claim_items)?;
    storage.vector.save()?;

    Ok((entity_count, claim_count))
}
