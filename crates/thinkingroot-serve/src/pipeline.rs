use std::collections::HashSet;
use std::path::Path;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::types::WorkspaceId;
use thinkingroot_graph::StorageEngine;

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
}

pub async fn run_pipeline(root_path: &Path) -> Result<PipelineResult> {
    let config = Config::load_merged(root_path)?;
    let data_dir = root_path.join(&config.workspace.data_dir);
    std::fs::create_dir_all(&data_dir)?;

    let documents = thinkingroot_parse::parse_directory(root_path, &config.parsers)?;
    let files_parsed = documents.len();

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
        });
    }

    // ─── Phase 2: Extract potentially-changed documents (with cache) ───
    let workspace_id = WorkspaceId::new();
    let cache_hits;
    let extraction;

    if potentially_changed.is_empty() {
        // Only deletions — no extraction needed.
        cache_hits = 0;
        extraction = thinkingroot_extract::ExtractionOutput::default();
    } else {
        let extractor = thinkingroot_extract::Extractor::new(&config)
            .await?
            .with_cache_dir(&data_dir);
        let raw = extractor
            .extract_all(
                &potentially_changed.iter().map(|d| (*d).clone()).collect::<Vec<_>>(),
                workspace_id,
            )
            .await?;
        cache_hits = raw.cache_hits;
        extraction = raw;
    }

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

    for doc in &truly_changed {
        let existing_sources = storage.graph.find_sources_by_uri(&doc.uri)?;
        if !existing_sources.is_empty() {
            for (source_id, _, _) in &existing_sources {
                affected_triples
                    .extend(storage.graph.get_source_relation_triples(source_id)?);
            }
            storage.graph.remove_source_by_uri(&doc.uri)?;
            changed += 1;
        }
    }

    for (source_id, uri) in &deleted_sources {
        affected_triples.extend(storage.graph.get_source_relation_triples(source_id)?);
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

        let verifier = thinkingroot_verify::Verifier::new(&config);
        let verification = verifier.verify(&storage.graph)?;

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
    };

    let claims_count = filtered_extraction.claims.len();
    let entities_count = filtered_extraction.entities.len();
    let relations_count = filtered_extraction.relations.len();

    // ─── Phase 7: Link ─────────────────────────────────────────────────
    let linker = thinkingroot_link::Linker::new(&storage.graph);
    let link_output = linker.link(filtered_extraction)?;

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
    update_vector_index_full(&mut storage)?;

    // ─── Phase 10: Selective compilation ────────────────────────────────
    let compiler = thinkingroot_compile::Compiler::new(&config)?;
    let artifacts = compiler.compile_affected(
        &storage.graph,
        &data_dir,
        &link_output.affected_entity_ids,
        true,
    )?;

    // ─── Phase 11: Verify + persist ─────────────────────────────────────
    let verifier = thinkingroot_verify::Verifier::new(&config);
    let verification = verifier.verify(&storage.graph)?;

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
