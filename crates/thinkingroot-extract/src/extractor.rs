use std::collections::HashMap;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::*;

// LLM client + scheduler moved to `thinkingroot-llm` (Phase 2 cleanup,
// 2026-05-14). The Witness Mesh substrate consults no LLM at compile
// time; structural extraction parses tree-sitter / regex over chunk
// metadata.
use crate::schema::ExtractionResult;

/// The main extraction engine. Takes DocumentIRs and produces
/// Claims, Entities, and Relations via structural extraction (Witness
/// Mesh era — no LLM, no batches, no scheduler). Constructed by
/// `Extractor::new`; orchestrates one structural-extraction pass per
/// chunk via `extract_all`.
pub struct Extractor {
    min_confidence: f64,
}

/// The combined output of extraction across all documents.
///
/// The `cache_hits`, `failed_batches`, `failed_batch_ranges`, and
/// `claim_source_quotes` fields are LLM-batch-era counters that the
/// Witness Mesh path never populates — they remain on the struct
/// because `PipelineResult` (a wire type consumed by the CLI, REST,
/// and the desktop) reads them. Always 0 / empty in the post-cutover
/// world.
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
    /// Always 0 post-cutover; retained for wire-compat with `PipelineResult`.
    pub cache_hits: usize,
    /// Chunks extracted via structural (Tier 0) extraction — no LLM call made.
    pub structural_extractions: usize,
    /// Maps SourceId → the raw source text seen by extraction. Used
    /// by legacy AEP `source_authority` joins that reference the
    /// full source text by source id.
    pub source_texts: HashMap<SourceId, String>,
    /// Maps ClaimId → an LLM-era citation quote. Always empty
    /// post-cutover; retained for wire-compat with `PipelineResult`.
    pub claim_source_quotes: HashMap<ClaimId, String>,
    /// LLM-batch-era partial-failure counter. Always 0 post-cutover;
    /// retained for wire-compat with `PipelineResult` + the
    /// `ProgressEvent::ExtractionPartial` SSE event shape.
    pub failed_batches: usize,
    /// LLM-batch-era partial-failure detail. Always empty post-cutover;
    /// retained for wire-compat with `PipelineResult`.
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
    /// Witness Mesh — Witnesses produced by the rule-catalog
    /// extractors (`comment_claims`, `parse_doc_rules`,
    /// `test_assertions`, `lsp_rules`). Populated by
    /// `Extractor::collect_witnesses_from_documents`, called from
    /// `extract_all` after the existing claim extraction.
    pub witnesses: Vec<thinkingroot_core::types::Witness>,
}

#[derive(Debug, Clone)]
pub struct SourcedRelation {
    pub source: SourceId,
    pub relation: Relation,
}

/// Run the Witness Mesh rule-catalog extractors over every chunk of
/// every document and return the collected, deduplicated Witnesses.
///
/// Why a free function (not a method on `Extractor`): the witness
/// pass needs none of the configuration state that `Extractor`
/// carries — its inputs are pure (DocumentIRs in, Witnesses out).
/// Keeping it free lets `backfill_witness_mesh` and pipeline
/// integration tests call it directly without constructing an
/// `Extractor`.
///
/// Mesh assembly (dedup, SAFETY-rule cross-check, deterministic
/// sort) runs at the caller's discretion via
/// `witness_mesh::assemble` — this function returns the raw stream
/// so callers can attach per-document context if needed.
/// Pull the lower-case extension off a DocumentIR's `uri`.
/// Returns an empty string when the URI carries no `.`.
fn doc_extension(doc: &DocumentIR) -> String {
    doc.uri
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Detect a chunkless image DocumentIR by URI extension. The parser
/// emits image documents via `thinkingroot_parse::image_meta::parse`
/// which routes off the same extension set as
/// [`crate::image_rules::is_image_extension`]. Keep the two
/// extension catalogues in lockstep by delegating here.
fn is_image_document(doc: &DocumentIR) -> bool {
    crate::image_rules::is_image_extension(&doc_extension(doc))
}

/// Detect a chunkless audio DocumentIR by URI extension. Mirrors
/// [`is_image_document`]; backed by
/// [`crate::audio_rules::is_audio_extension`].
fn is_audio_document(doc: &DocumentIR) -> bool {
    crate::audio_rules::is_audio_extension(&doc_extension(doc))
}

pub fn collect_witnesses_from_documents(
    documents: &[DocumentIR],
    workspace_id: WorkspaceId,
) -> Vec<thinkingroot_core::types::Witness> {
    use chrono::Utc;

    let now = Utc::now();
    let mut out: Vec<thinkingroot_core::types::Witness> = Vec::new();

    for doc in documents {
        // The Source's content_hash is the canonical file BLAKE3 —
        // matches `WitnessSpan.file_blake3` semantics. When a parser
        // has not yet stamped the hash, we honestly skip; emitting
        // Witnesses against an empty file_blake3 would let an
        // unanchored row slip past the I-W8 verifier.
        let file_blake3 = doc.content_hash.0.clone();
        if file_blake3.is_empty() {
            continue;
        }

        // Image-family dispatch (chunkless DocumentIR). The image
        // rule modules operate on the whole file bytes, not on
        // text chunks — re-read the bytes via `doc.uri` and emit
        // image::* witnesses. Decode failures and oversized files
        // surface as `image::skipped@v1` (honest absence), never
        // as a missing row.
        if is_image_document(doc) {
            if let Ok(bytes) = std::fs::read(&doc.uri) {
                out.extend(crate::image_rules::extract_image_witnesses(
                    &bytes,
                    &file_blake3,
                    doc.source_id,
                    workspace_id,
                    now,
                ));
            } else {
                tracing::warn!(
                    uri = %doc.uri,
                    "image document unreadable at extract time — skipping image::* rules"
                );
            }
            // Image documents have no text chunks; the rest of the
            // per-chunk loop below would be a no-op anyway, but
            // skipping it explicitly keeps the witness flow clear.
            continue;
        }

        // Audio-family dispatch. Same shape as image: read bytes,
        // call into `audio_rules`. Failures surface as
        // `audio::skipped@v1`.
        if is_audio_document(doc) {
            if let Ok(bytes) = std::fs::read(&doc.uri) {
                out.extend(crate::audio_rules::extract_audio_witnesses(
                    &bytes,
                    &file_blake3,
                    doc.source_id,
                    workspace_id,
                    now,
                ));
            } else {
                tracing::warn!(
                    uri = %doc.uri,
                    "audio document unreadable at extract time — skipping audio::* rules"
                );
            }
            continue;
        }

        // Reconstruct the full file bytes from chunk content. This
        // is approximate (chunks may be trimmed by parsers), but
        // sufficient for the extractors that match on chunk-local
        // regex patterns. content_blake3 is computed per-witness
        // from the precise span bytes the extractor selects.
        //
        // For the production pipeline path, the walker reads the
        // file bytes directly and threads them through — that's a
        // pipeline-integration concern, handled in the
        // pipeline.rs witness pass. Here we accept the chunk-text
        // reconstruction as the contract for callers that have
        // only DocumentIR in hand (backfill, tests).
        let approx_source_bytes: Vec<u8> = doc
            .chunks
            .iter()
            .flat_map(|c| c.content.bytes())
            .collect();
        let source_bytes = approx_source_bytes.as_slice();

        for chunk in &doc.chunks {
            // Each extractor decides its own applicability based
            // on chunk_type / language / byte-range — calling all
            // four is safe and the cost is one regex+is_match per
            // chunk for the ones that early-return.
            out.extend(crate::comment_claims::extract_witnesses_from_chunk(
                chunk,
                source_bytes,
                &file_blake3,
                doc.source_id,
                workspace_id,
                now,
            ));
            let doc_out = crate::parse_doc_rules::extract_witnesses_from_chunk(
                chunk,
                source_bytes,
                &file_blake3,
                doc.source_id,
                workspace_id,
                now,
            );
            out.extend(doc_out.witnesses);
            out.extend(crate::test_assertions::extract_witnesses_from_chunk(
                chunk,
                source_bytes,
                &file_blake3,
                doc.source_id,
                workspace_id,
                now,
            ));
        }
    }
    out
}

impl Extractor {
    /// Construct a new extractor. Witness Mesh era: no LLM client is
    /// initialised; no scheduler, cache, or checkpoint. The
    /// `config` parameter is honoured only for `min_confidence`.
    pub async fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            min_confidence: config.extraction.min_confidence,
        })
    }

    /// Extract knowledge from a batch of documents. Structural
    /// extraction is pure CPU and runs per-chunk; no batching or
    /// concurrency layer is needed.
    ///
    /// `sources_to_extract`: when `Some`, only documents whose `source_id` is
    /// present in the set are processed; documents not in the set are skipped
    /// entirely — before any work is dispatched.  `None` means extract all
    /// documents.  An empty `Some(HashSet::new())` is a valid degenerate
    /// case that produces an empty `ExtractionOutput` without error.
    pub async fn extract_all(
        &self,
        documents: &[DocumentIR],
        workspace_id: WorkspaceId,
        sources_to_extract: Option<std::collections::HashSet<thinkingroot_core::types::SourceId>>,
    ) -> Result<ExtractionOutput> {
        // Source-granular re-extraction (T12): filter at the DocumentIR level
        // so unchanged documents never enter the work queue. Cloning the
        // filtered subset is proportional to the truly-changed set
        // (typically 1 document in the "1 file edited" hot path).
        let filtered: Vec<DocumentIR>;
        let work: &[DocumentIR] = if let Some(ref filter) = sources_to_extract {
            filtered = documents
                .iter()
                .filter(|d| filter.contains(&d.source_id))
                .cloned()
                .collect();
            &filtered
        } else {
            documents
        };
        let mut output = self.extract_all_inner(work, workspace_id).await?;
        // Witness Mesh pass — populate ExtractionOutput.witnesses
        // alongside the legacy claim flow. Pure addition; existing
        // consumers that read `.claims` continue to work. The pass
        // is per-document and cheap (regex + tree-sitter walk —
        // ~0.5 ms per source).
        let witnesses = collect_witnesses_from_documents(work, workspace_id);
        output.witnesses = witnesses;
        Ok(output)
    }

    /// Inner extraction — Witness Mesh era.
    ///
    /// Runs structural-only: every chunk goes through
    /// `structural::extract_structural`, no LLM is consulted, no
    /// cache is hit, no batches are packed. The complexity of the
    /// pre-cutover LLM path (semaphores, batch packing, schedulers,
    /// in-flight checkpoints, retries) is gone because structural
    /// extraction is purely CPU — runs in microseconds per chunk and
    /// produces deterministic output.
    async fn extract_all_inner(
        &self,
        documents: &[DocumentIR],
        workspace_id: WorkspaceId,
    ) -> Result<ExtractionOutput> {
        let min_confidence = self.min_confidence;
        let documents_len = documents.len();

        let mut output = ExtractionOutput {
            sources_processed: documents_len,
            ..Default::default()
        };

        // Source text map (formerly used by the grounding tribunal,
        // now retained for legacy AEP `source_authority` joins that
        // still reference the full source text by source id).
        for doc in documents {
            let text: String = doc
                .chunks
                .iter()
                .map(|c| c.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            output.source_texts.insert(doc.source_id, text);
        }

        // Per-chunk structural extraction. Each `ExtractionResult` is
        // converted into `ExtractionOutput` shape via
        // `convert_result_static` (preserves byte spans, applies the
        // sensitivity / quantity / expiration decorators).
        for doc in documents {
            for chunk in &doc.chunks {
                output.chunks_processed += 1;
                let result = crate::structural::extract_structural(chunk, &doc.uri);
                if result.claims.is_empty()
                    && result.entities.is_empty()
                    && result.relations.is_empty()
                {
                    continue;
                }
                output.structural_extractions += 1;
                let mut converted = Self::convert_result_static(
                    result,
                    doc.source_id,
                    workspace_id,
                    min_confidence,
                );
                // Stamp byte ranges from the chunk onto every claim
                // that lacks an authoritative span — matches the
                // pre-cutover behaviour where structural-only claims
                // inherited the chunk's range.
                for claim in &mut converted.claims {
                    if claim.source_span.is_none() && chunk.byte_end > chunk.byte_start {
                        claim.source_span = Some(
                            thinkingroot_core::types::SourceSpan::bytes(
                                chunk.byte_start,
                                chunk.byte_end,
                            ),
                        );
                    }
                }
                output.claims.extend(converted.claims);
                output.entities.extend(converted.entities);
                output.relations.extend(converted.relations);
                output
                    .claim_entity_names
                    .extend(converted.claim_entity_names);
                output.claim_quantities.extend(converted.claim_quantities);
                output.claim_expirations.extend(converted.claim_expirations);
            }
        }

        tracing::info!(
            "structural extraction: {} claims, {} entities, {} relations across {} sources / {} chunks ({} structurally extracted)",
            output.claims.len(),
            output.entities.len(),
            output.relations.len(),
            output.sources_processed,
            output.chunks_processed,
            output.structural_extractions,
        );

        Ok(output)
    }

    /// Convert structural extraction results into core types.
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
            // Wire optional predicate from structural output. Invalid
            // entries (unknown language, regex that fails to compile)
            // are dropped silently so the claim lands in `Attested`
            // tier rather than failing extraction.
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

                // Reject low-confidence relations.
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

/// Apply the source-id filter from `extract_all` to a document slice.
///
/// Returns the subset of `documents` whose `source_id` is present in
/// `filter`.  When `filter` is `None`, the full slice is returned
/// unchanged (pre-T12 behaviour).  Exposed for testing only — the
/// production path inlines equivalent logic in `extract_all` to avoid
/// an extra allocation in the `None` (extract-all) fast path.
#[cfg(test)]
pub(crate) fn apply_source_filter<'a>(
    documents: &'a [DocumentIR],
    filter: Option<&std::collections::HashSet<thinkingroot_core::types::SourceId>>,
) -> Vec<&'a DocumentIR> {
    match filter {
        None => documents.iter().collect(),
        Some(set) => documents.iter().filter(|d| set.contains(&d.source_id)).collect(),
    }
}

/// Convert a structural-extracted predicate payload into a validated
/// core `Predicate`.
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
        // ExtractionOutput must start clean.
        let out = ExtractionOutput::default();
        assert_eq!(out.failed_batches, 0);
        assert!(out.failed_batch_ranges.is_empty());
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
}

// ── T12: source-granular re-extract filter tests ─────────────────────────────
//
// These tests verify the `apply_source_filter` helper used by `extract_all`
// to restrict document processing to the Phase-1 potentially-changed set.
// They run synchronously (no LLM, no Extractor construction) so they are
// reliable in offline CI.

#[cfg(test)]
mod witness_collection_tests {
    use super::collect_witnesses_from_documents;
    use thinkingroot_core::ir::{Chunk, ChunkType, DocumentIR};
    use thinkingroot_core::types::{ContentHash, SourceId, SourceType, WorkspaceId};

    fn make_comment_doc(content: &str) -> DocumentIR {
        let source_id = SourceId::new();
        let mut doc = DocumentIR::new(source_id, "fixture.rs".into(), SourceType::File);
        doc.content_hash = ContentHash::from_bytes(content.as_bytes());
        let mut chunk = Chunk::new(content, ChunkType::Comment, 1, 1);
        chunk.byte_start = 0;
        chunk.byte_end = content.len() as u64;
        chunk.language = Some("rust".into());
        doc.add_chunk(chunk);
        doc
    }

    #[test]
    fn collects_claim_witness_from_comment_chunk() {
        let doc = make_comment_doc("/// @claim does the thing");
        let witnesses = collect_witnesses_from_documents(&[doc], WorkspaceId::new());
        assert!(
            witnesses.iter().any(|w| w.witness_type == "claim::@claim"),
            "expected a claim::@claim witness, got types {:?}",
            witnesses.iter().map(|w| &w.witness_type).collect::<Vec<_>>()
        );
    }

    #[test]
    fn skips_documents_without_content_hash() {
        let source_id = SourceId::new();
        let mut doc = DocumentIR::new(source_id, "fixture.rs".into(), SourceType::File);
        // content_hash stays empty — honest skip per file_blake3
        // empty-string guard in collect_witnesses_from_documents.
        let mut chunk = Chunk::new("/// @claim hi", ChunkType::Comment, 1, 1);
        chunk.byte_start = 0;
        chunk.byte_end = 13;
        doc.add_chunk(chunk);
        let witnesses = collect_witnesses_from_documents(&[doc], WorkspaceId::new());
        assert!(
            witnesses.is_empty(),
            "expected no witnesses when content_hash is unset, got {} witnesses",
            witnesses.len()
        );
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        let witnesses = collect_witnesses_from_documents(&[], WorkspaceId::new());
        assert!(witnesses.is_empty());
    }
}

#[cfg(test)]
mod source_filter_tests {
    use super::apply_source_filter;
    use std::collections::HashSet;
    use thinkingroot_core::ir::{Chunk, ChunkType, DocumentIR};
    use thinkingroot_core::types::{SourceId, SourceType};

    fn make_doc(source_id: SourceId) -> DocumentIR {
        let mut doc = DocumentIR::new(
            source_id,
            format!("file_{source_id}.md"),
            SourceType::File,
        );
        doc.add_chunk(Chunk::new(
            format!("# Heading for {source_id}"),
            ChunkType::Heading,
            1,
            1,
        ));
        doc
    }

    // 1. None filter: all documents pass through unchanged.
    #[test]
    fn extract_all_with_none_filter_processes_all_documents() {
        let ids: Vec<SourceId> = (0..5).map(|_| SourceId::new()).collect();
        let docs: Vec<DocumentIR> = ids.iter().map(|&id| make_doc(id)).collect();

        let result = apply_source_filter(&docs, None);

        assert_eq!(
            result.len(),
            5,
            "None filter must pass all 5 documents through; got {}",
            result.len()
        );
        for (original, filtered) in docs.iter().zip(result.iter()) {
            assert_eq!(
                original.source_id, filtered.source_id,
                "None filter must preserve document order and identity"
            );
        }
    }

    // 2. Some(matched subset): only documents in the set are returned.
    #[test]
    fn extract_all_with_filter_skips_unmatched_sources() {
        let source_a = SourceId::new();
        let source_b = SourceId::new();
        let source_c = SourceId::new();
        let source_d = SourceId::new();
        let source_e = SourceId::new();

        let docs = vec![
            make_doc(source_a),
            make_doc(source_b),
            make_doc(source_c),
            make_doc(source_d),
            make_doc(source_e),
        ];

        // Filter to only source_a and source_c; source_b / source_d / source_e
        // must be skipped — before any cache lookup or LLM dispatch.
        let filter: HashSet<SourceId> = [source_a, source_c].into_iter().collect();
        let result = apply_source_filter(&docs, Some(&filter));

        assert_eq!(
            result.len(),
            2,
            "filter {{source_a, source_c}} must pass 2 documents; got {}",
            result.len()
        );
        let returned_ids: HashSet<SourceId> = result.iter().map(|d| d.source_id).collect();
        assert!(
            returned_ids.contains(&source_a),
            "source_a must be included in the filtered result"
        );
        assert!(
            returned_ids.contains(&source_c),
            "source_c must be included in the filtered result"
        );
        assert!(
            !returned_ids.contains(&source_b),
            "source_b must NOT be included (not in filter set)"
        );
        assert!(
            !returned_ids.contains(&source_d),
            "source_d must NOT be included (not in filter set)"
        );
        assert!(
            !returned_ids.contains(&source_e),
            "source_e must NOT be included (not in filter set)"
        );
    }

    // 3. Some(empty set): zero documents pass — valid degenerate case, not an error.
    #[test]
    fn extract_all_with_empty_filter_processes_no_documents() {
        let docs: Vec<DocumentIR> = (0..5).map(|_| make_doc(SourceId::new())).collect();

        let empty_filter: HashSet<SourceId> = HashSet::new();
        let result = apply_source_filter(&docs, Some(&empty_filter));

        assert_eq!(
            result.len(),
            0,
            "empty filter set must pass zero documents; got {}",
            result.len()
        );
    }
}
