// Many LLM-batch-era fields + methods on `Extractor` are now
// unreachable post-Witness-Mesh cutover. They are kept for struct
// stability during the dual-write transition; a follow-up session
// removes them entirely when `extractor.rs` is fully rewritten to
// drop the LlmClient handle. `#![allow(dead_code)]` suppresses the
// transitional warnings without hiding genuine issues — every new
// edit in this file should still treat unused-code warnings as bugs.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use thinkingroot_core::Result;
use thinkingroot_core::config::Config;
use thinkingroot_core::ir::DocumentIR;
use thinkingroot_core::types::*;

// LLM client + scheduler moved to `thinkingroot-llm` (Phase 2 cleanup,
// 2026-05-14). The Witness Mesh substrate consults no LLM at compile
// time; structural extraction parses tree-sitter / regex over chunk
// metadata.
use crate::schema::ExtractionResult;


/// **Deprecated** — pre-cutover this was the fallback batch size for
/// LLM dispatch. Witness Mesh has no batches. Kept as `pub const` for
/// source-compat with the few legacy tests that import the symbol.
pub const EXTRACTION_BATCH_SIZE: usize = 6;

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
/// Claims, Entities, and Relations via structural extraction (Witness
/// Mesh era — no LLM, no batches, no scheduler). Constructed by
/// `Extractor::new`; orchestrates one structural-extraction pass per
/// chunk via `extract_all`.
pub struct Extractor {
    min_confidence: f64,
    progress: Option<ChunkProgressFn>,
    /// Cancellation token consulted between phases in `extract_all`.
    /// `None` = opt-out (test callers). The pipeline orchestrator
    /// installs one via `with_cancel`.
    cancel: Option<CancellationToken>,
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
    /// Witness Mesh — Witnesses produced by the rule-catalog
    /// extractors (`comment_claims`, `parse_doc_rules`,
    /// `test_assertions`, `lsp_rules`). Populated by
    /// `Extractor::collect_witnesses_from_documents`, called from
    /// `extract_all` after the existing claim extraction. Empty when
    /// the caller has not opted into the Witness Mesh pass (the
    /// pipeline integration runs it unconditionally; tests that
    /// only exercise the claim path may leave this empty).
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
/// pass needs none of the LLM scheduler / batch-checkpoint state
/// that `Extractor` carries — its inputs are pure (DocumentIRs in,
/// Witnesses out). Keeping it free lets `backfill_witness_mesh` and
/// pipeline integration tests call it directly without spinning up
/// the full LLM stack.
///
/// Mesh assembly (dedup, SAFETY-rule cross-check, deterministic
/// sort) runs at the caller's discretion via
/// `witness_mesh::assemble` — this function returns the raw stream
/// so callers can attach per-document context if needed.
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
    /// `config` parameter is honoured only for `min_confidence` —
    /// every other historical field is dead.
    pub async fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            min_confidence: config.extraction.min_confidence,
            progress: None,
            cancel: None,
        })
    }

    /// Install a cancellation token consulted between extraction
    /// phases. When the token is tripped, `extract_all` returns
    /// `Err(Error::Cancelled)` at the next phase boundary.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Attach a progress callback. Called once per chunk processed.
    /// Arguments: `ExtractionProgressEvent`.
    pub fn with_progress(mut self, f: ChunkProgressFn) -> Self {
        self.progress = Some(f);
        self
    }

    // ── Deprecated builders kept as no-ops for source compat ───────
    // These were load-bearing in the LLM era. Post-cutover they're
    // no-ops so existing callers keep compiling; the next release
    // removes them after callers migrate.

    /// **Deprecated, no-op.** Pre-cutover this installed the LLM
    /// in-flight checkpoint log; post-cutover there are no batches
    /// to checkpoint. Retained as a no-op for compile compatibility.
    pub fn with_checkpoint(self, _data_dir: &std::path::Path) -> Result<Self> {
        Ok(self)
    }

    /// **Deprecated, no-op.** Pre-cutover this enabled the LLM
    /// extraction cache; post-cutover structural extraction is
    /// deterministic and runs in microseconds — no cache layer
    /// helps. Retained as a no-op.
    pub fn with_cache_dir(self, _data_dir: &std::path::Path) -> Self {
        self
    }

    /// Extract knowledge from a batch of documents — all chunks run concurrently.
    ///
    /// `sources_to_extract`: when `Some`, only documents whose `source_id` is
    /// present in the set are processed; documents not in the set are skipped
    /// entirely — before any cache lookup or LLM dispatch.  `None` means
    /// extract all documents, which preserves the pre-T12 behaviour.  An empty
    /// `Some(HashSet::new())` is a valid degenerate case that produces an empty
    /// `ExtractionOutput` without error.
    pub async fn extract_all(
        &self,
        documents: &[DocumentIR],
        workspace_id: WorkspaceId,
        sources_to_extract: Option<std::collections::HashSet<thinkingroot_core::types::SourceId>>,
    ) -> Result<ExtractionOutput> {
        // Source-granular re-extraction (T12): filter at the DocumentIR level
        // BEFORE any cache lookup or LLM dispatch so unchanged documents never
        // even enter the work queues.  `None` = extract all (pre-T12 behaviour).
        // Cloning the filtered subset is proportional to the truly-changed set
        // (typically 1 document in the "1 file edited" hot path), not the full
        // corpus — the cost is negligible compared to extraction itself.
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

    /// Inner implementation that operates on the (already-filtered) document slice.
    /// Called by `extract_all` after the source-id filter is applied.
    /// Inner extraction — Witness Mesh era.
    ///
    /// Pre-cutover this method dispatched chunks through an LLM batch
    /// pipeline. Post-cutover (2026-05-11) it runs structural-only:
    /// every chunk goes through `structural::extract_structural`, no
    /// LLM is consulted, no cache is hit, no batches are packed.
    ///
    /// The legacy path's complexity (semaphores, batch packing,
    /// schedulers, in-flight checkpoints, retries) is gone because
    /// structural extraction is purely CPU — runs in microseconds
    /// per chunk and produces deterministic output. Whatever the
    /// LLM produced is now obviated by the Witness Mesh substrate
    /// populated in parallel via `collect_witnesses_from_documents`.
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

// `split_chunk_to_token_budget` deleted in Witness Mesh cutover —
// chunk splitting for LLM context windows is moot when there is no
// LLM. Structural extraction operates on the full chunk regardless
// of size. The `split_to_token_budget_lines` helper below is kept
// because its tests exercise edge-case fallback logic that's still
// useful for future text-segmentation needs.

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

    // ── Wedge 3 split tests deleted in Witness Mesh cutover ─────────
    // `split_chunk_to_token_budget` itself was deleted because
    // chunk splitting for LLM context windows is moot post-cutover.
    // The line-based fallback (`split_to_token_budget_lines`) and
    // its three tests above are retained for unrelated future use.

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

    // `router_correctly_splits_mixed_document` test deleted in
    // Witness Mesh cutover — the tier router (`crate::router`) was
    // deleted because there's no longer an LLM tier to route to.
    // All chunks now flow through structural extraction unconditionally.
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
