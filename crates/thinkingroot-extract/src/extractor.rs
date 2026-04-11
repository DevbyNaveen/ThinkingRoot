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
            max_chunk_tokens: config.extraction.max_chunk_tokens,
            cache: None,
            progress: None,
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

    /// Attach a progress callback. Called once per original chunk processed
    /// (cache hit or LLM result). Arguments: (done, total, source_uri).
    pub fn with_progress(mut self, f: ChunkProgressFn) -> Self {
        self.progress = Some(f);
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

        let mut output = ExtractionOutput::default();
        output.sources_processed = documents_len;

        // ── Pass 1: separate cache hits from LLM work ──────────────────
        // This gives us an accurate total_chunks denominator before any
        // progress events fire, without double-counting sub-chunks.
        struct ChunkWork {
            source_id: SourceId,
            source_uri: String,
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

        // Guard: if some tasks returned None (all sub-chunks failed), fire a
        // synthetic catch-up event so the bar always reaches 100%.
        if done < total_chunks {
            if let Some(ref pf) = self.progress {
                pf(total_chunks, total_chunks, "");
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

/// Split content into sub-chunks that stay within the token budget.
/// Splits at line boundaries to preserve semantic integrity.
fn split_to_token_budget(content: &str, max_tokens: usize) -> Vec<String> {
    // chars/4 is a conservative token approximation that works across all tokenizers.
    let max_chars = max_tokens * 4;

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

/// Recursively extract from content, splitting at line boundaries if truncated.
/// Depth limit of 3 prevents infinite recursion on pathological inputs.
fn extract_with_split(
    llm: SharedLlm,
    content: String,
    context: String,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExtractionResult>> + Send>> {
    Box::pin(async move {
        match llm.extract(&content, &context).await {
            Ok(result) => Ok(result),

            Err(thinkingroot_core::Error::TruncatedOutput { ref provider, ref model })
                if depth < 3 =>
            {
                let lines: Vec<&str> = content.lines().collect();
                if lines.len() < 2 {
                    tracing::warn!(
                        "chunk from {provider}/{model} cannot be split further — skipping"
                    );
                    return Ok(ExtractionResult {
                        claims: vec![],
                        entities: vec![],
                        relations: vec![],
                    });
                }

                let mid = lines.len() / 2;
                let first_half = lines[..mid].join("\n");
                let second_half = lines[mid..].join("\n");

                tracing::info!(
                    "output truncated by {provider}/{model}, splitting chunk at line {mid} (depth={depth})"
                );

                let llm1 = Arc::clone(&llm);
                let llm2 = Arc::clone(&llm);
                let ctx1 = context.clone();
                let ctx2 = context.clone();

                let (r1, r2) = tokio::try_join!(
                    extract_with_split(llm1, first_half, ctx1, depth + 1),
                    extract_with_split(llm2, second_half, ctx2, depth + 1),
                )?;

                Ok(ExtractionResult {
                    claims: r1.claims.into_iter().chain(r2.claims).collect(),
                    entities: r1.entities.into_iter().chain(r2.entities).collect(),
                    relations: r1.relations.into_iter().chain(r2.relations).collect(),
                })
            }

            Err(thinkingroot_core::Error::TruncatedOutput { provider, model }) => {
                tracing::error!(
                    "chunk still truncated after max splits for {provider}/{model} — skipping"
                );
                Ok(ExtractionResult {
                    claims: vec![],
                    entities: vec![],
                    relations: vec![],
                })
            }

            Err(e) => Err(e),
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_to_token_budget_no_split_needed() {
        let content = "hello world\nfoo bar";
        let chunks = split_to_token_budget(content, 10000);
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
        let chunks = split_to_token_budget(&content, 5); // 20 chars budget
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
        let chunks = split_to_token_budget(&big_line, 10); // 40 chars budget
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], big_line);
    }
}
