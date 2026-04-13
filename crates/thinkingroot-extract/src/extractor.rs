use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::*;

use crate::llm::LlmClient;
use crate::prompts;
use crate::scheduler::ThroughputScheduler;
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
    /// Known entities from the existing graph, injected into LLM prompts.
    known_entities: crate::graph_context::GraphPrimedContext,
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

        Ok(Self {
            llm: Arc::new(llm),
            concurrency: config.llm.max_concurrent_requests,
            min_confidence: config.extraction.min_confidence,
            max_chunk_tokens: config.extraction.max_chunk_tokens,
            cache: None,
            progress: None,
            known_entities: crate::graph_context::GraphPrimedContext::new(Vec::new()),
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
        }

        let mut cache_hits_data: Vec<(SourceId, String, ExtractionResult)> = Vec::new();
        let mut llm_work: Vec<ChunkWork> = Vec::new();
        let mut structural_results: Vec<(SourceId, String, ExtractionResult)> = Vec::new();

        for doc in documents {
            for chunk in &doc.chunks {
                // ── Tier Router: structural or LLM? ──
                if crate::router::classify(chunk) == crate::router::Tier::Structural {
                    let result = crate::structural::extract_structural(chunk, &doc.uri);
                    if !result.claims.is_empty()
                        || !result.entities.is_empty()
                        || !result.relations.is_empty()
                    {
                        structural_results.push((doc.source_id, doc.uri.clone(), result));
                        // No `continue` — chunk also queued for LLM below so both run additively.
                        // Structural provides graph topology at 0.99 confidence;
                        // LLM provides semantic meaning — they are complementary, not redundant.
                    }
                }

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
                    original_content: chunk.content.clone(),
                    sub_chunks,
                    context: prompts::build_context(
                        &doc.uri,
                        chunk.language.as_deref(),
                        chunk.heading.as_deref(),
                    ),
                    ast_anchor: prompts::build_ast_anchor_section(&chunk.metadata),
                });
            }
        }

        // Total = number of original chunks across all documents.
        // Each chunk fires one progress event from the LLM path (cache hit or LLM task).
        // Structural results are additive — they run in addition to the LLM path, not instead.
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

        // ── Spawn LLM tasks — one task per original chunk ───────────────
        // Sub-chunks are processed sequentially *within* each task so that
        // progress fires once per original chunk, not once per sub-chunk.
        // Concurrency is still bounded by the semaphore (one permit per
        // sub-chunk LLM call, released after each call completes).
        let mut join_set = tokio::task::JoinSet::new();

        // Compute the graph-primed context section once; cloned into each task.
        let known_entities_section = self.known_entities.prompt_section();

        for work in llm_work {
            let llm = Arc::clone(&self.llm);
            let sem = Arc::clone(&semaphore);
            let graph_ctx = known_entities_section.clone();

            join_set.spawn(async move {
                let source_id = work.source_id;
                let source_uri = work.source_uri;
                let original_content = work.original_content;
                let mut sub_results: Vec<(String, ExtractionResult)> = Vec::new();

                // Prepend AST anchor (if any) to the graph-primed context so the LLM
                // is anchored to the exact entity names AST already extracted — one
                // coherent pipeline instead of two blind parallel lanes.
                let combined_ctx = if work.ast_anchor.is_empty() {
                    graph_ctx
                } else {
                    format!("{}\n\n{}", work.ast_anchor, graph_ctx)
                };

                for sub_content in work.sub_chunks {
                    let _permit = sem.acquire().await.ok()?;
                    match extract_with_split(
                        Arc::clone(&llm),
                        sub_content.clone(),
                        work.context.clone(),
                        combined_ctx.clone(),
                        0,
                    )
                    .await
                    {
                        Ok(r) => sub_results.push((sub_content, r)),
                        Err(e) => {
                            tracing::warn!("extraction failed for chunk in {source_uri}: {e}");
                        }
                    }
                }

                if sub_results.is_empty() {
                    return None;
                }
                Some((source_id, source_uri, original_content, sub_results))
            });
        }

        // ── Collect in completion order (JoinSet.join_next) ─────────────
        // JoinSet yields results as each task finishes, giving smooth
        // progress updates rather than awaiting in spawn order.
        while let Some(join_result) = join_set.join_next().await {
            if let Ok(Some((source_id, source_uri, original_content, sub_results))) = join_result {
                if let Some(ref cache) = self.cache {
                    // Write each sub-chunk result under its own key.
                    for (sub_content, extraction_result) in &sub_results {
                        if let Err(e) = cache.put(sub_content, extraction_result) {
                            tracing::warn!("failed to write extraction cache entry: {e}");
                        }
                    }
                    // Also write the merged result under the original chunk key so that
                    // split chunks hit the cache on subsequent runs (the lookup key is
                    // always the original full chunk content, not the sub-chunk content).
                    if sub_results.len() > 1
                        || sub_results
                            .first()
                            .map(|(c, _)| c != &original_content)
                            .unwrap_or(false)
                    {
                        let merged = ExtractionResult {
                            claims: sub_results
                                .iter()
                                .flat_map(|(_, r)| r.claims.clone())
                                .collect(),
                            entities: sub_results
                                .iter()
                                .flat_map(|(_, r)| r.entities.clone())
                                .collect(),
                            relations: sub_results
                                .iter()
                                .flat_map(|(_, r)| r.relations.clone())
                                .collect(),
                        };
                        if let Err(e) = cache.put(&original_content, &merged) {
                            tracing::warn!("failed to write merged cache entry: {e}");
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
        for ext_claim in &result.claims {
            if ext_claim.confidence < min_confidence {
                continue;
            }
            let claim_type = parse_claim_type(&ext_claim.claim_type);
            let claim = Claim::new(&ext_claim.statement, claim_type, source_id, workspace_id)
                .with_confidence(ext_claim.confidence)
                .with_extraction_tier(ext_claim.extraction_tier);
            if !ext_claim.entities.is_empty() {
                output
                    .claim_entity_names
                    .insert(claim.id, ext_claim.entities.clone());
            }
            if let Some(ref quote) = ext_claim.source_quote {
                if !quote.is_empty() {
                    output.claim_source_quotes.insert(claim.id, quote.clone());
                }
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
/// `known_entities_section` is passed unchanged through all recursive splits so
/// every sub-chunk benefits from the same graph context.
fn extract_with_split(
    llm: SharedLlm,
    content: String,
    context: String,
    known_entities_section: String,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExtractionResult>> + Send>> {
    Box::pin(async move {
        match llm
            .extract_with_graph_context(&content, &context, &known_entities_section)
            .await
        {
            Ok(result) => Ok(result),

            Err(thinkingroot_core::Error::TruncatedOutput {
                ref provider,
                ref model,
            }) if depth < 3 => {
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
                let gctx1 = known_entities_section.clone();
                let gctx2 = known_entities_section.clone();

                let (r1, r2) = tokio::try_join!(
                    extract_with_split(llm1, first_half, ctx1, gctx1, depth + 1),
                    extract_with_split(llm2, second_half, ctx2, gctx2, depth + 1),
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
        self.structural_extractions += other.structural_extractions;
        self.source_texts.extend(other.source_texts);
        self.claim_source_quotes.extend(other.claim_source_quotes);
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
                heading: None,
                language: None,
                metadata: ChunkMetadata::default(),
            },
            Chunk {
                content: "use std::path::Path;".to_string(),
                chunk_type: ChunkType::Import,
                start_line: 1,
                end_line: 1,
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
