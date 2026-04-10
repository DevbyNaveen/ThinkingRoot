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

    // ─── Phase 1: Diff ─────────────────────────────────────────────────
    let mut new_documents = Vec::new();
    let mut skipped = 0usize;
    let mut changed = 0usize;
    let mut deleted = 0usize;
    let mut affected_triples: Vec<(String, String, String)> = Vec::new();

    for doc in &documents {
        let existing_sources = storage.graph.find_sources_by_uri(&doc.uri)?;

        if existing_sources.len() == 1
            && !doc.content_hash.0.is_empty()
            && existing_sources[0].1 == doc.content_hash.0
        {
            skipped += 1;
            continue;
        }

        if !existing_sources.is_empty() {
            // Capture affected relation triples BEFORE removal.
            for (source_id, _, _) in &existing_sources {
                affected_triples
                    .extend(storage.graph.get_source_relation_triples(source_id)?);
            }
            storage.graph.remove_source_by_uri(&doc.uri)?;
            fingerprints.remove(&doc.uri);
            changed += 1;
        }

        new_documents.push(doc.clone());
    }

    // Detect deleted files.
    let current_uris: HashSet<&str> = documents.iter().map(|doc| doc.uri.as_str()).collect();
    for (source_id, uri, source_type) in storage.graph.get_all_sources()? {
        let is_file_backed = matches!(source_type.as_str(), "File" | "Document");
        if is_file_backed && !current_uris.contains(uri.as_str()) {
            // Capture affected triples before deletion.
            affected_triples.extend(storage.graph.get_source_relation_triples(&source_id)?);
            storage.graph.remove_source_by_uri(&uri)?;
            fingerprints.remove(&uri);
            deleted += 1;
        }
    }

    // ─── Phase 2: Incremental entity relation update for deletions ─────
    if !affected_triples.is_empty() {
        affected_triples.sort_unstable();
        affected_triples.dedup();
        storage
            .graph
            .update_entity_relations_for_triples(&affected_triples)?;
    }

    // ─── Early exit: truly nothing to process ──────────────────────────
    if new_documents.is_empty() && changed == 0 && deleted == 0 {
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

    // If only deletions (no new docs), recompile affected artifacts and exit.
    if new_documents.is_empty() {
        update_vector_index_full(&mut storage)?;

        let compiler = thinkingroot_compile::Compiler::new(&config)?;
        let artifacts = compiler.compile_affected(&storage.graph, &data_dir, &[], true)?;

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
            cache_hits: 0,
            early_cutoffs: 0,
        });
    }

    // ─── Phase 3: Extract (with cache) ─────────────────────────────────
    let workspace_id = WorkspaceId::new();
    let extractor = thinkingroot_extract::Extractor::new(&config)
        .await?
        .with_cache_dir(&data_dir);
    let extraction = extractor.extract_all(&new_documents, workspace_id).await?;

    let claims_count = extraction.claims.len();
    let entities_count = extraction.entities.len();
    let relations_count = extraction.relations.len();
    let cache_hits = extraction.cache_hits;

    // ─── Phase 4: Insert sources ───────────────────────────────────────
    for doc in &new_documents {
        let source = thinkingroot_core::Source::new(doc.uri.clone(), doc.source_type)
            .with_id(doc.source_id)
            .with_hash(doc.content_hash.clone());
        storage.graph.insert_source(&source)?;
    }

    // ─── Phase 5: Link ─────────────────────────────────────────────────
    let linker = thinkingroot_link::Linker::new(&storage.graph);
    let link_output = linker.link(extraction)?;

    // ─── Phase 6: Incremental entity relation update for new sources ───
    let mut new_triples: Vec<(String, String, String)> = Vec::new();
    for doc in &new_documents {
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

    // ─── Phase 7: Incremental vector update ────────────────────────────
    update_vector_index_full(&mut storage)?;

    // ─── Phase 8: Selective compilation ────────────────────────────────
    let compiler = thinkingroot_compile::Compiler::new(&config)?;
    let artifacts = compiler.compile_affected(
        &storage.graph,
        &data_dir,
        &link_output.affected_entity_ids,
        true,
    )?;

    // ─── Phase 9: Verify + persist ─────────────────────────────────────
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
        early_cutoffs: skipped,
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
