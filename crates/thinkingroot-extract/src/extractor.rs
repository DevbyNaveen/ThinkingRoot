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

/// The main extraction engine. Takes DocumentIRs and produces
/// Claims, Entities, and Relations via LLM extraction.
pub struct Extractor {
    llm: SharedLlm,
    concurrency: usize,
    min_confidence: f64,
    cache: Option<crate::cache::ExtractionCache>,
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
}

#[derive(Debug)]
pub struct SourcedRelation {
    pub source: SourceId,
    pub relation: Relation,
}

impl Extractor {
    pub async fn new(config: &Config) -> Result<Self> {
        let llm = LlmClient::new(&config.llm)
            .await?
            .with_max_retries(config.extraction.max_retries);

        Ok(Self {
            llm: Arc::new(llm),
            concurrency: config.llm.max_concurrent_requests,
            min_confidence: config.extraction.min_confidence,
            cache: None,
        })
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

    /// Extract knowledge from a batch of documents — all chunks run concurrently.
    pub async fn extract_all(
        &self,
        documents: &[DocumentIR],
        workspace_id: WorkspaceId,
    ) -> Result<ExtractionOutput> {
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let min_confidence = self.min_confidence;

        let mut output = ExtractionOutput::default();
        let documents_len = documents.len();

        // Separate cache hits (processed immediately) from misses (spawned as LLM tasks).
        let mut handles = Vec::new();

        for doc in documents {
            for chunk in &doc.chunks {
                let content = chunk.content.clone();
                let source_id = doc.source_id;

                // Check cache first.
                if let Some(ref cache) = self.cache {
                    if let Some(cached_result) = cache.get(&content) {
                        tracing::debug!("extraction cache hit for chunk in {}", doc.uri);
                        let converted = Self::convert_result_static(
                            cached_result,
                            source_id,
                            workspace_id,
                            min_confidence,
                        );
                        output.merge(converted);
                        output.chunks_processed += 1;
                        output.cache_hits += 1;
                        continue;
                    }
                }

                // Cache miss — spawn LLM task.
                let llm = Arc::clone(&self.llm);
                let sem = Arc::clone(&semaphore);
                let uri = doc.uri.clone();
                let context = prompts::build_context(
                    &doc.uri,
                    chunk.language.as_deref(),
                    chunk.heading.as_deref(),
                );

                let handle = tokio::spawn(async move {
                    let _permit = sem.acquire().await.ok()?;
                    match llm.extract(&content, &context).await {
                        Ok(result) => Some((source_id, uri, content, result)),
                        Err(e) => {
                            tracing::warn!("extraction failed for chunk in {uri}: {e}");
                            None
                        }
                    }
                });

                handles.push(handle);
            }
        }

        let sources_processed = documents_len;

        for handle in handles {
            if let Ok(Some((source_id, _uri, content, result))) = handle.await {
                // Write to cache for future runs.
                if let Some(ref cache) = self.cache {
                    if let Err(e) = cache.put(&content, &result) {
                        tracing::warn!("failed to write extraction cache entry: {e}");
                    }
                }

                let converted =
                    Self::convert_result_static(result, source_id, workspace_id, min_confidence);
                output.merge(converted);
                output.chunks_processed += 1;
            }
        }

        output.sources_processed = sources_processed;

        tracing::info!(
            "extraction complete: {} claims, {} entities, {} relations from {} sources ({} chunks)",
            output.claims.len(),
            output.entities.len(),
            output.relations.len(),
            output.sources_processed,
            output.chunks_processed,
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
        for ext_claim in &result.claims {
            if ext_claim.confidence < min_confidence {
                continue;
            }
            let claim_type = parse_claim_type(&ext_claim.claim_type);
            let claim = Claim::new(&ext_claim.statement, claim_type, source_id, workspace_id)
                .with_confidence(ext_claim.confidence);
            if !ext_claim.entities.is_empty() {
                output
                    .claim_entity_names
                    .insert(claim.id, ext_claim.entities.clone());
            }
            output.claims.push(claim);
        }

        // Convert relations.
        for ext_rel in &result.relations {
            let from_id = entity_map.get(&ext_rel.from_entity.to_lowercase());
            let to_id = entity_map.get(&ext_rel.to_entity.to_lowercase());

            if let (Some(&from), Some(&to)) = (from_id, to_id) {
                let rel_type = parse_relation_type(&ext_rel.relation_type);
                let rel = Relation::new(from, to, rel_type)
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

impl ExtractionOutput {
    fn merge(&mut self, other: ExtractionOutput) {
        self.claims.extend(other.claims);
        self.entities.extend(other.entities);
        self.relations.extend(other.relations);
        self.claim_entity_names.extend(other.claim_entity_names);
        self.sources_processed += other.sources_processed;
        self.chunks_processed += other.chunks_processed;
        self.cache_hits += other.cache_hits;
    }
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

fn parse_relation_type(s: &str) -> RelationType {
    match s.to_lowercase().as_str() {
        "depends_on" => RelationType::DependsOn,
        "owned_by" => RelationType::OwnedBy,
        "replaces" => RelationType::Replaces,
        "contradicts" => RelationType::Contradicts,
        "implements" => RelationType::Implements,
        "uses" => RelationType::Uses,
        "contains" => RelationType::Contains,
        "created_by" => RelationType::CreatedBy,
        "part_of" => RelationType::PartOf,
        "related_to" => RelationType::RelatedTo,
        "calls" => RelationType::Calls,
        "configured_by" => RelationType::ConfiguredBy,
        "tested_by" => RelationType::TestedBy,
        _ => RelationType::RelatedTo,
    }
}
