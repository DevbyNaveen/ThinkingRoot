//! Phase 6.7 — Structural Persist (Compile Completeness Contract §6).
//!
//! Sits between Phase 6.5 (Rooting) and Phase 7 (Link) in the compile
//! pipeline. Walks every chunk in every truly-changed document and emits
//! typed rows into the 16 new structural CozoDB tables introduced by the
//! contract — one row per metadata field that has a typed home, plus a
//! `chunks_residual` fall-through for chunks no other emitter covers
//! (the catch-all that makes I-3 byte coverage tractable).
//!
//! The phase is **purely deterministic**: same source bytes → same
//! emitted rows → same `content_blake3` values. No LLM. No network.
//! That makes it trivially cacheable and `:put`-upsert-safe on
//! re-compile (Phase 6.7 short-circuits per chunk via the
//! `Blake3Cache`'s deterministic IDs).
//!
//! Fields populated by the emitters here come from data already
//! plumbed through Parse (`thinkingroot-parse`):
//!
//! - `ChunkMetadata.{function_name, parameters, return_type, visibility,
//!   parent, trait_name, field_types, calls_functions, heading_level,
//!   links, config_key, config_value, config_value_type, row_index,
//!   row_columns, doc_tags}` — the 16 fields the contract §4 enumerates.
//! - `ExtractedClaim.{quantities, expiration_signal, valid_until}` —
//!   the §5 decorator output, carried via `ExtractionOutput.claim_*`
//!   maps.
//!
//! Per-row BLAKE3 hashing is amortised by `Blake3Cache<'_>`
//! (`crates/thinkingroot-graph/src/row_blake3.rs`): each source's
//! bytes are hashed once per `(byte_start, byte_end)` tuple, not once
//! per emitter.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use thinkingroot_core::Result;
use thinkingroot_core::ir::{Chunk, ChunkType, DocumentIR};
use thinkingroot_core::types::{ContentHash, SourceId};
use thinkingroot_extract::ExtractionOutput;
use thinkingroot_graph::rows::{
    CodeLink, CodeMarker, CodeMetric, CodeSignature, ConfigTreeNode, DataRowRow, DocTagRow,
    FunctionCall, GitBlameRow, GitCommit, HeadingRow, QuantityRow, ResidualChunk,
    SourceAnnotation, TestAnnotation,
};
use thinkingroot_graph::Blake3Cache;
use thinkingroot_graph::graph::{GraphStore, PerSourceRows};
use thinkingroot_graph::{FileSystemSourceStore, SourceByteStore};

mod code_metrics;
mod git_blame;
mod git_commits;
mod markers;
mod source_annotations;
mod test_annotations;

/// Per-call statistics surfaced to pipeline.rs for logging + the SSE
/// progress event. Excludes the per-table counts when zero so log output
/// stays scannable.
#[derive(Debug, Default, Clone)]
pub struct Phase67Stats {
    pub sources_processed: usize,
    pub structural_rows_emitted: usize,
    pub residual_rows_emitted: usize,
    pub blake3_distinct_spans: usize,
    pub elapsed: Duration,
    pub per_table_counts: BTreeMap<&'static str, usize>,
}

impl Phase67Stats {
    fn record(&mut self, table: &'static str, n: usize) {
        if n == 0 {
            return;
        }
        self.structural_rows_emitted += n;
        *self.per_table_counts.entry(table).or_insert(0) += n;
    }
}

/// Per-document accumulator that owns one `Vec<Row>` per of the 16 new
/// structural tables. Drained at the end of `phase_6_7_structural_persist`
/// via the typed batch-insert helpers in
/// `thinkingroot-graph/src/structural_inserts.rs` (CHUNK = 500 per CozoDB
/// script, per-batch transactional).
///
/// The four tables not represented here — `git_commits`, `test_annotations`,
/// `git_blame`, `code_metrics` — are populated by their dedicated
/// language-aware passes and routed through their own batch helpers
/// directly.
#[derive(Default)]
struct PerTableBuckets {
    function_calls: Vec<FunctionCall>,
    doc_tags: Vec<DocTagRow>,
    code_links: Vec<CodeLink>,
    code_signatures: Vec<CodeSignature>,
    config_tree: Vec<ConfigTreeNode>,
    data_rows: Vec<DataRowRow>,
    headings: Vec<HeadingRow>,
    chunks_residual: Vec<ResidualChunk>,
    quantities: Vec<QuantityRow>,
    source_annotations: Vec<SourceAnnotation>,
    code_markers: Vec<CodeMarker>,
    test_annotations: Vec<TestAnnotation>,
    code_metrics: Vec<CodeMetric>,
    git_blame: Vec<GitBlameRow>,
    git_commits: Vec<GitCommit>,
}

/// Phase 6.7 driver — see module docs.
///
/// `documents` is borrowed from the pipeline's `truly_changed` slice
/// (`pipeline.rs:645`). `extraction` is `&mut` so the driver can stamp
/// the freshly-computed `content_blake3` onto every Claim flowing
/// into the linker (matches the pattern at extractor.rs:449-454 where
/// byte ranges are backfilled onto cached claims).
pub fn phase_6_7_structural_persist(
    documents: &[&DocumentIR],
    extraction: &mut ExtractionOutput,
    graph: &GraphStore,
    byte_store: &FileSystemSourceStore,
) -> Result<Phase67Stats> {
    let started = Instant::now();
    let mut stats = Phase67Stats::default();

    for doc in documents {
        let bytes = match byte_store
            .get(&doc.content_hash)
            .map_err(|e| thinkingroot_core::Error::Compilation {
                artifact_type: "phase_6_7".to_string(),
                message: format!("byte_store: {e}"),
            })?
        {
            Some(b) => b.bytes,
            None => {
                tracing::warn!(
                    source_id = %doc.source_id,
                    content_hash = %doc.content_hash.0,
                    "phase 6.7: source bytes missing in byte_store; skipping (chunks_residual would have nothing to hash)"
                );
                continue;
            }
        };
        let mut cache = Blake3Cache::new(&bytes);
        let mut buckets = PerTableBuckets::default();
        let source_id_str = doc.source_id.to_string();

        // ── File-level emitters (run once per doc) ─────────────────────
        // Source annotations — license / copyright / shebang / encoding /
        // mode / trailing_newline_norm. Always emitted (zero rows allowed
        // when the file has no recognised pragma).
        source_annotations::emit(
            doc,
            &bytes,
            &source_id_str,
            &mut cache,
            &mut buckets.source_annotations,
        );

        // git_commits — fires once per git-source DocumentIR, reading
        // the SourceMetadata fields populated by parse_git_log.
        git_commits::emit(doc, &bytes, &source_id_str, &mut cache, &mut buckets.git_commits);

        // git_blame — fires for File-typed sources whose URI maps to a
        // path on disk in a git repo. Default-enabled per contract §15
        // Q2; opt-out via the `compile.git_blame` config flag is read
        // upstream by the caller and gated by setting the env var.
        let git_blame_enabled = std::env::var("TR_GIT_BLAME")
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
            .unwrap_or(true);
        if git_blame_enabled {
            run_git_blame_for_file_source(
                doc,
                &bytes,
                &source_id_str,
                &mut cache,
                &mut buckets.git_blame,
            );
        }

        // Build a quick claim-byte-span index so the chunks_residual
        // emitter can skip chunks that already produced ≥1 admitted claim.
        let claim_spans: std::collections::HashSet<(u64, u64)> = extraction
            .claims
            .iter()
            .filter(|c| c.source == doc.source_id)
            .filter_map(|c| {
                c.source_span
                    .as_ref()
                    .and_then(|s| match (s.byte_start, s.byte_end) {
                        (Some(bs), Some(be)) if be > bs => Some((bs, be)),
                        _ => None,
                    })
            })
            .collect();

        // Maintain a (heading_level, heading_id) stack so the headings
        // emitter can resolve `parent_heading_id` to the actual emitted
        // row id rather than the heading text.
        let mut heading_stack: Vec<(u8, String)> = Vec::new();

        // Per-chunk emitter dispatch.
        for chunk in &doc.chunks {
            let pre_count = total_row_count(&buckets);
            dispatch_chunk(
                chunk,
                doc,
                &bytes,
                &source_id_str,
                &mut cache,
                &mut heading_stack,
                &mut buckets,
                extraction,
            );
            let added_rows = total_row_count(&buckets) - pre_count;

            // chunks_residual fall-through: only when 0 typed rows AND 0
            // admitted claims for this chunk's byte span.
            let span = (chunk.byte_start, chunk.byte_end);
            let claim_covered = claim_spans.contains(&span);
            if added_rows == 0 && !claim_covered && chunk.byte_end > chunk.byte_start {
                let id = stable_row_id(
                    "chunks_residual",
                    &source_id_str,
                    chunk.byte_start,
                    chunk.byte_end,
                    "",
                );
                buckets.chunks_residual.push(ResidualChunk {
                    id,
                    source_id: source_id_str.clone(),
                    chunk_type: chunk_type_str(chunk.chunk_type).to_string(),
                    content: chunk.content.clone(),
                    metadata_json: serde_json::to_string(&chunk.metadata).unwrap_or_default(),
                    byte_start: chunk.byte_start,
                    byte_end: chunk.byte_end,
                    content_blake3: cache.get(chunk.byte_start, chunk.byte_end).to_string(),
                });
                stats.residual_rows_emitted += 1;
            }
        }

        // Stamp content_blake3 onto every Claim from this source so the
        // linker writes the final value into `claims.content_blake3` in
        // a single insert (no second pass).
        for claim in extraction.claims.iter_mut().filter(|c| c.source == doc.source_id) {
            if let Some(span) = &claim.source_span
                && let (Some(bs), Some(be)) = (span.byte_start, span.byte_end)
                && be > bs
            {
                claim.row_blake3 = Some(cache.get(bs, be).to_string());
            }
        }

        // ─── Gap-filling chunks_residual emission (Compile Completeness
        // Contract I-3) ────────────────────────────────────────────────
        // Per-chunk chunks_residual catches CHUNKS the structural
        // emitters didn't claim. But chunks themselves don't cover the
        // full byte range of a source — there are gaps between chunks
        // (whitespace, line endings, file headers, blank lines). Phase 9
        // would fail with orphan ranges in those gaps.
        //
        // Sweep the doc here: collect every `(byte_start, byte_end)`
        // already emitted by this doc's pass (claims + every bucket
        // row) and emit one chunks_residual row per uncovered gap so
        // I-3 holds end-to-end. Per contract §15 Q1 we store the gap's
        // raw bytes inline.
        let mut covered: Vec<(u64, u64)> = Vec::new();
        for span in &claim_spans {
            covered.push(*span);
        }
        for r in &buckets.function_calls {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.doc_tags {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.code_links {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.code_signatures {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.config_tree {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.data_rows {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.headings {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.chunks_residual {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.source_annotations {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.code_markers {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.test_annotations {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.code_metrics {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.git_blame {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        for r in &buckets.git_commits {
            if r.source_id == source_id_str {
                covered.push((r.byte_start, r.byte_end));
            }
        }
        let total_bytes = bytes.len() as u64;
        for (gap_start, gap_end) in compute_gaps(&covered, total_bytes) {
            // Synthesise a unique id keyed on the gap's range. Re-running
            // Phase 6.7 over identical bytes produces identical ids →
            // `:put` is upsert-safe.
            let id = stable_row_id(
                "chunks_residual",
                &source_id_str,
                gap_start,
                gap_end,
                "gap",
            );
            // Inline content if reasonably small; truncate above 4KB to
            // keep the table from ballooning on pathological gaps (e.g.
            // a 2MB binary blob between two parsed sections).
            let s_idx = (gap_start as usize).min(bytes.len());
            let e_idx = (gap_end as usize).min(bytes.len());
            let raw = &bytes[s_idx..e_idx];
            let content = if raw.len() <= 4096 {
                String::from_utf8_lossy(raw).to_string()
            } else {
                format!(
                    "[gap of {} bytes; first 256: {}]",
                    raw.len(),
                    String::from_utf8_lossy(&raw[..256])
                )
            };
            buckets.chunks_residual.push(ResidualChunk {
                id,
                source_id: source_id_str.clone(),
                chunk_type: "byte_gap".to_string(),
                content,
                metadata_json: "{}".to_string(),
                byte_start: gap_start,
                byte_end: gap_end,
                content_blake3: cache.get(gap_start, gap_end).to_string(),
            });
            stats.residual_rows_emitted += 1;
        }

        // Emit `quantities` rows from the §5 decorator output. The map
        // is keyed by claim_id — every claim entry yields N rows.
        emit_quantities(
            extraction,
            doc,
            &source_id_str,
            &bytes,
            &mut cache,
            &mut buckets.quantities,
        );

        // Drain buckets via the T7 transactional rebuild — cascade :rm
        // for the 16 structural tables scoped to this source, then :put
        // every non-empty bucket, all inside one Cozo `multi_transaction`.
        // Concurrent AEP/Hybrid readers either see the prior compile's
        // rows or the new compile's rows, never a torn intermediate.
        // (Closes invariant I-W4 of the water-flow incremental compile
        // spec.)
        flush_buckets(graph, &source_id_str, &mut buckets, &mut stats)?;

        stats.sources_processed += 1;
        stats.blake3_distinct_spans += cache.len();
    }

    stats.elapsed = started.elapsed();
    Ok(stats)
}

/// Compute uncovered byte ranges given a set of `(start, end)` covered
/// intervals and a total byte size. Returns the gap intervals
/// `[0, total)` minus the union of covered intervals. Used by the
/// Phase 6.7 chunks_residual gap-filler so I-3 holds even when chunks
/// don't byte-exhaustively cover a source.
fn compute_gaps(covered: &[(u64, u64)], total: u64) -> Vec<(u64, u64)> {
    if total == 0 {
        return Vec::new();
    }
    let mut intervals: Vec<(u64, u64)> = covered
        .iter()
        .filter(|(s, e)| e > s)
        .map(|(s, e)| (*s, (*e).min(total)))
        .collect();
    intervals.sort_unstable();
    let mut gaps = Vec::new();
    let mut covered_to: u64 = 0;
    for (s, e) in intervals {
        if s > covered_to {
            gaps.push((covered_to, s));
        }
        if e > covered_to {
            covered_to = e;
        }
    }
    if covered_to < total {
        gaps.push((covered_to, total));
    }
    gaps
}

/// Run git-blame on a File-typed source whose URI resolves to a real
/// path on disk. No-op for non-file sources, untracked files, or
/// non-git workspaces (the emitter handles all those failure modes
/// silently).
fn run_git_blame_for_file_source(
    doc: &DocumentIR,
    bytes: &[u8],
    source_id: &str,
    cache: &mut Blake3Cache,
    out: &mut Vec<thinkingroot_graph::rows::GitBlameRow>,
) {
    if !matches!(doc.source_type, thinkingroot_core::types::SourceType::File) {
        return;
    }
    let path = match uri_to_path(&doc.uri) {
        Some(p) => p,
        None => return,
    };
    if !path.exists() {
        return;
    }
    // Walk up to find the repo root — git blame needs the right working
    // dir. If no parent has a `.git`, the file isn't tracked and the
    // emitter returns zero hunks.
    let repo_root = find_repo_root(&path).unwrap_or_else(|| {
        path.parent().map(|p| p.to_path_buf()).unwrap_or(path.clone())
    });
    git_blame::emit(&repo_root, &path, bytes, source_id, cache, out);
}

fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    if let Some(rest) = uri.strip_prefix("file://") {
        Some(std::path::PathBuf::from(rest))
    } else if uri.starts_with('/') {
        Some(std::path::PathBuf::from(uri))
    } else {
        None
    }
}

fn find_repo_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start.parent()?;
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn total_row_count(b: &PerTableBuckets) -> usize {
    b.function_calls.len()
        + b.doc_tags.len()
        + b.code_links.len()
        + b.code_signatures.len()
        + b.config_tree.len()
        + b.data_rows.len()
        + b.headings.len()
        + b.chunks_residual.len()
        + b.quantities.len()
        + b.code_markers.len()
        + b.source_annotations.len()
        + b.test_annotations.len()
        + b.code_metrics.len()
        + b.git_blame.len()
        + b.git_commits.len()
}

/// Drain `buckets` for the source `source_id` via the T7 transactional
/// rebuild path: cascade-then-emit on the 16 structural tables, all
/// inside one `multi_transaction`.  Per-table row counts are still
/// recorded into `stats` (zero-row tables stay out of `per_table_counts`).
///
/// Why we cascade even when every bucket is empty: a source whose new
/// compile produced no structural rows means the prior compile's rows
/// are stale by definition.  The cascade clears them.  See the
/// rationale at the top of `crates/thinkingroot-graph/src/per_source_rows.rs`.
fn flush_buckets(
    graph: &GraphStore,
    source_id: &str,
    buckets: &mut PerTableBuckets,
    stats: &mut Phase67Stats,
) -> Result<()> {
    // Capture per-table counts BEFORE the std::mem::take moves so the
    // counts survive the move into PerSourceRows.  Stats are recorded
    // AFTER transactional_rebuild_source returns Ok so we never claim
    // rows that were rolled back on error (the ? operator short-circuits
    // before the record() calls when the rebuild fails).
    let function_calls_n = buckets.function_calls.len();
    let doc_tags_n = buckets.doc_tags.len();
    let code_links_n = buckets.code_links.len();
    let code_signatures_n = buckets.code_signatures.len();
    let config_tree_n = buckets.config_tree.len();
    let data_rows_n = buckets.data_rows.len();
    let headings_n = buckets.headings.len();
    let chunks_residual_n = buckets.chunks_residual.len();
    let quantities_n = buckets.quantities.len();
    let source_annotations_n = buckets.source_annotations.len();
    let code_markers_n = buckets.code_markers.len();
    let test_annotations_n = buckets.test_annotations.len();
    let code_metrics_n = buckets.code_metrics.len();
    let git_blame_n = buckets.git_blame.len();
    let git_commits_n = buckets.git_commits.len();

    let rows = PerSourceRows {
        function_calls: std::mem::take(&mut buckets.function_calls),
        doc_tags: std::mem::take(&mut buckets.doc_tags),
        code_links: std::mem::take(&mut buckets.code_links),
        code_signatures: std::mem::take(&mut buckets.code_signatures),
        config_tree: std::mem::take(&mut buckets.config_tree),
        data_rows: std::mem::take(&mut buckets.data_rows),
        headings: std::mem::take(&mut buckets.headings),
        chunks_residual: std::mem::take(&mut buckets.chunks_residual),
        quantities: std::mem::take(&mut buckets.quantities),
        source_annotations: std::mem::take(&mut buckets.source_annotations),
        code_markers: std::mem::take(&mut buckets.code_markers),
        test_annotations: std::mem::take(&mut buckets.test_annotations),
        code_metrics: std::mem::take(&mut buckets.code_metrics),
        git_blame: std::mem::take(&mut buckets.git_blame),
        git_commits: std::mem::take(&mut buckets.git_commits),
        // `source_references` is not emitted by Phase 6.7 — Phase 7e
        // (`thinkingroot_link::structural_resolve`) builds it.  The rebuild
        // still cascades source_references for this source so any stale
        // reference rows from a prior compile are cleared; the linker
        // re-inserts after we return.
        source_references: Vec::new(),
    };

    graph.transactional_rebuild_source(source_id, &rows)?;

    // Record stats only after the commit succeeded — counts reflect what
    // is durably written, not what was attempted.
    stats.record("function_calls", function_calls_n);
    stats.record("doc_tags", doc_tags_n);
    stats.record("code_links", code_links_n);
    stats.record("code_signatures", code_signatures_n);
    stats.record("config_tree", config_tree_n);
    stats.record("data_rows", data_rows_n);
    stats.record("headings", headings_n);
    stats.record("chunks_residual", chunks_residual_n);
    stats.record("quantities", quantities_n);
    stats.record("source_annotations", source_annotations_n);
    stats.record("code_markers", code_markers_n);
    stats.record("test_annotations", test_annotations_n);
    stats.record("code_metrics", code_metrics_n);
    stats.record("git_blame", git_blame_n);
    stats.record("git_commits", git_commits_n);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dispatch_chunk(
    chunk: &Chunk,
    doc: &DocumentIR,
    bytes: &[u8],
    source_id: &str,
    cache: &mut Blake3Cache,
    heading_stack: &mut Vec<(u8, String)>,
    buckets: &mut PerTableBuckets,
    extraction: &ExtractionOutput,
) -> () {
    match chunk.chunk_type {
        ChunkType::FunctionDef | ChunkType::TypeDef => {
            emit_code_signature_and_calls(chunk, source_id, cache, buckets, extraction);
            test_annotations::emit(
                chunk,
                source_id,
                cache,
                &mut buckets.test_annotations,
                extraction,
            );
            code_metrics::emit(
                chunk,
                source_id,
                cache,
                &mut buckets.code_metrics,
                extraction,
            );
        }
        ChunkType::Heading => {
            emit_heading(chunk, source_id, cache, heading_stack, buckets);
        }
        ChunkType::Comment | ChunkType::ModuleDoc => {
            emit_doc_tags(chunk, source_id, cache, buckets, extraction);
            emit_code_links(chunk, source_id, cache, buckets);
            markers::emit(chunk, source_id, cache, &mut buckets.code_markers, extraction);
        }
        ChunkType::Code => {
            markers::emit(chunk, source_id, cache, &mut buckets.code_markers, extraction);
        }
        ChunkType::Prose => {
            emit_code_links(chunk, source_id, cache, buckets);
        }
        ChunkType::ConfigEntry => {
            emit_config_entry(chunk, source_id, cache, buckets);
        }
        ChunkType::DataRow => {
            emit_data_row(chunk, source_id, cache, buckets);
        }
        // List, Table, Import, ManifestDependency — no per-table emitter
        // here; they fall through to chunks_residual when no claim covers
        // their byte range. Import + ManifestDependency claims are emitted
        // by the structural extractor at Phase 2 and carry their
        // byte ranges, so the residual fallback rarely fires for them.
        ChunkType::List | ChunkType::Table | ChunkType::Import | ChunkType::ManifestDependency => {}
    }
    let _ = (doc, bytes); // reserved for future emitters that need the full byte slice
}

fn emit_code_signature_and_calls(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    buckets: &mut PerTableBuckets,
    extraction: &ExtractionOutput,
) {
    // Find the FunctionDef/TypeDef claim id that owns this chunk's bytes
    // — required because code_signatures is keyed on `claim_id`. If the
    // structural extractor didn't emit a claim for this chunk (rare —
    // requires ChunkMetadata.function_name/type_name to be empty), skip.
    let claim_id = extraction
        .claims
        .iter()
        .find(|c| {
            c.source_span
                .as_ref()
                .and_then(|s| match (s.byte_start, s.byte_end) {
                    (Some(bs), Some(be)) => {
                        Some(bs == chunk.byte_start && be == chunk.byte_end)
                    }
                    _ => None,
                })
                .unwrap_or(false)
                && (matches!(c.claim_type, thinkingroot_core::types::ClaimType::ApiSignature)
                    || matches!(c.claim_type, thinkingroot_core::types::ClaimType::Definition))
        })
        .map(|c| c.id.to_string());

    let Some(claim_id) = claim_id else {
        return;
    };

    let parameters_json = serde_json::to_string(
        chunk.metadata.parameters.as_deref().unwrap_or(&[]),
    )
    .unwrap_or_else(|_| "[]".to_string());
    let field_types_json = serde_json::to_string(&chunk.metadata.field_types)
        .unwrap_or_else(|_| "[]".to_string());
    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();

    buckets.code_signatures.push(CodeSignature {
        claim_id: claim_id.clone(),
        parameters_json,
        return_type: chunk.metadata.return_type.clone().unwrap_or_default(),
        visibility: chunk.metadata.visibility.clone().unwrap_or_default(),
        trait_name: chunk.metadata.trait_name.clone().unwrap_or_default(),
        parent_scope: chunk.metadata.parent.clone().unwrap_or_default(),
        field_types_json,
        source_id: source_id.to_string(),
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        content_blake3: blake3_str.clone(),
    });

    // function_calls — one row per callee. Only fires for FunctionDef
    // (TypeDef chunks have no calls_functions[]).
    if matches!(chunk.chunk_type, ChunkType::FunctionDef) {
        for (idx, callee_name) in chunk.metadata.calls_functions.iter().enumerate() {
            let id = stable_row_id(
                "function_calls",
                source_id,
                chunk.byte_start,
                chunk.byte_end,
                &format!("{idx}|{callee_name}"),
            );
            buckets.function_calls.push(FunctionCall {
                id,
                caller_claim_id: claim_id.clone(),
                callee_name: callee_name.clone(),
                callee_claim_id: String::new(), // resolved at Phase 7e
                source_id: source_id.to_string(),
                byte_start: chunk.byte_start,
                byte_end: chunk.byte_end,
                content_blake3: blake3_str.clone(),
            });
        }
    }
}

fn emit_heading(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    heading_stack: &mut Vec<(u8, String)>,
    buckets: &mut PerTableBuckets,
) {
    let level = chunk.metadata.heading_level.unwrap_or(1).max(1);
    // Pop the stack until top has level < this heading's level.
    while heading_stack
        .last()
        .map(|(l, _)| *l >= level)
        .unwrap_or(false)
    {
        heading_stack.pop();
    }
    let parent_heading_id = heading_stack
        .last()
        .map(|(_, id)| id.clone())
        .unwrap_or_default();

    let id = stable_row_id(
        "headings",
        source_id,
        chunk.byte_start,
        chunk.byte_end,
        &chunk.content,
    );
    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();
    buckets.headings.push(HeadingRow {
        id: id.clone(),
        source_id: source_id.to_string(),
        level,
        text: chunk.content.clone(),
        parent_heading_id,
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        content_blake3: blake3_str,
    });
    heading_stack.push((level, id));
}

fn emit_doc_tags(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    buckets: &mut PerTableBuckets,
    extraction: &ExtractionOutput,
) {
    if chunk.metadata.doc_tags.is_empty() {
        return;
    }
    // doc_tags rows reference the owning claim. Find it by byte span on
    // the same source. Doc-comment claims emit `claim_type = "doc_comment"`.
    let claim_id = extraction
        .claims
        .iter()
        .find(|c| {
            c.source_span
                .as_ref()
                .and_then(|s| match (s.byte_start, s.byte_end) {
                    (Some(bs), Some(be)) => {
                        Some(bs == chunk.byte_start && be == chunk.byte_end)
                    }
                    _ => None,
                })
                .unwrap_or(false)
        })
        .map(|c| c.id.to_string())
        .unwrap_or_default();

    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();
    for (idx, tag) in chunk.metadata.doc_tags.iter().enumerate() {
        let id = stable_row_id(
            "doc_tags",
            source_id,
            chunk.byte_start,
            chunk.byte_end,
            &format!("{idx}|{}|{}", tag.kind, tag.name.clone().unwrap_or_default()),
        );
        buckets.doc_tags.push(DocTagRow {
            id,
            claim_id: claim_id.clone(),
            kind: tag.kind.clone(),
            target: tag.name.clone().unwrap_or_default(),
            description: tag.description.clone(),
            source_id: source_id.to_string(),
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
            content_blake3: blake3_str.clone(),
        });
    }
}

fn emit_code_links(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    buckets: &mut PerTableBuckets,
) {
    if chunk.metadata.links.is_empty() {
        return;
    }
    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();
    for (idx, url) in chunk.metadata.links.iter().enumerate() {
        let id = stable_row_id(
            "code_links",
            source_id,
            chunk.byte_start,
            chunk.byte_end,
            &format!("{idx}|{url}"),
        );
        buckets.code_links.push(CodeLink {
            id,
            source_id: source_id.to_string(),
            chunk_id: String::new(),
            url: url.clone(),
            link_text: String::new(), // markdown.rs doesn't currently capture link text
            is_internal: false,         // resolved at Phase 7e
            target_source_id: String::new(),
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
            content_blake3: blake3_str.clone(),
        });
    }
}

fn emit_config_entry(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    buckets: &mut PerTableBuckets,
) {
    let Some(dotted_path) = chunk.metadata.config_key.as_ref() else {
        return;
    };
    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();
    buckets.config_tree.push(ConfigTreeNode {
        source_id: source_id.to_string(),
        dotted_path: dotted_path.clone(),
        value: chunk.metadata.config_value.clone().unwrap_or_default(),
        value_type: chunk
            .metadata
            .config_value_type
            .clone()
            .unwrap_or_else(|| "string".to_string()),
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        content_blake3: blake3_str,
    });
}

fn emit_data_row(
    chunk: &Chunk,
    source_id: &str,
    cache: &mut Blake3Cache,
    buckets: &mut PerTableBuckets,
) {
    let row_index = chunk.metadata.row_index.unwrap_or(0);
    // columns_json: serialise Vec<(header, cell)> to JSON object {header: cell}.
    let mut obj = serde_json::Map::new();
    for (k, v) in &chunk.metadata.row_columns {
        obj.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    let columns_json =
        serde_json::Value::Object(obj).to_string();
    let id = stable_row_id(
        "data_rows",
        source_id,
        chunk.byte_start,
        chunk.byte_end,
        &row_index.to_string(),
    );
    let blake3_str = cache.get(chunk.byte_start, chunk.byte_end).to_string();
    buckets.data_rows.push(DataRowRow {
        id,
        source_id: source_id.to_string(),
        row_index,
        columns_json,
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        content_blake3: blake3_str,
    });
}

fn emit_quantities(
    extraction: &ExtractionOutput,
    doc: &DocumentIR,
    source_id: &str,
    bytes: &[u8],
    cache: &mut Blake3Cache,
    out: &mut Vec<QuantityRow>,
) {
    let captured_at = chrono::Utc::now().timestamp() as f64;
    for claim in extraction.claims.iter().filter(|c| c.source == doc.source_id) {
        let Some(qs) = extraction.claim_quantities.get(&claim.id) else {
            continue;
        };
        let claim_id_str = claim.id.to_string();
        // Anchor quantity bytes to the claim's byte span when the
        // claim has one — quantity.rs currently produces offsets
        // relative to the input string (the claim's statement), so
        // we bias toward the claim span when available. For v1 we
        // store the claim's full byte span; refine to per-quantity
        // sub-spans in a follow-up once quantity.rs threads claim-
        // local offsets back to absolute file-local bytes.
        let (bs, be) = match &claim.source_span {
            Some(s) => match (s.byte_start, s.byte_end) {
                (Some(a), Some(b)) if b > a => (a, b),
                _ => (0, 0),
            },
            None => (0, 0),
        };
        let blake3_str = if be > bs {
            cache.get(bs, be).to_string()
        } else {
            // Fall back to file-level hash so the row remains verifiable.
            thinkingroot_graph::row_blake3(bytes, 0, bytes.len() as u64)
        };
        for (idx, q) in qs.iter().enumerate() {
            let id = stable_row_id(
                "quantities",
                source_id,
                bs,
                be,
                &format!("{}|{idx}|{}", claim_id_str, q.metric_name),
            );
            out.push(QuantityRow {
                id,
                claim_id: claim_id_str.clone(),
                metric_name: q.metric_name.clone(),
                value: q.value,
                unit: q.unit.clone(),
                qualifier: q.qualifier.clone(),
                is_live: q.is_live,
                captured_at,
                source_id: source_id.to_string(),
                byte_start: bs,
                byte_end: be,
                content_blake3: blake3_str.clone(),
            });
        }
    }
}

/// Generate a deterministic row id from the row's positional context.
/// Re-running Phase 6.7 over identical source bytes produces identical
/// ids → `:put` is upsert-safe.
pub(crate) fn stable_row_id(
    table: &str,
    source_id: &str,
    byte_start: u64,
    byte_end: u64,
    suffix: &str,
) -> String {
    let mut h = blake3::Hasher::new();
    h.update(table.as_bytes());
    h.update(b"|");
    h.update(source_id.as_bytes());
    h.update(b"|");
    h.update(&byte_start.to_le_bytes());
    h.update(b"|");
    h.update(&byte_end.to_le_bytes());
    h.update(b"|");
    h.update(suffix.as_bytes());
    let mut out = String::from(table);
    out.push(':');
    out.push_str(h.finalize().to_hex().as_str());
    out
}

fn chunk_type_str(c: ChunkType) -> &'static str {
    match c {
        ChunkType::Prose => "prose",
        ChunkType::Code => "code",
        ChunkType::Heading => "heading",
        ChunkType::List => "list",
        ChunkType::Table => "table",
        ChunkType::FunctionDef => "function_def",
        ChunkType::TypeDef => "type_def",
        ChunkType::Import => "import",
        ChunkType::Comment => "comment",
        ChunkType::ModuleDoc => "module_doc",
        ChunkType::ManifestDependency => "manifest_dependency",
        ChunkType::ConfigEntry => "config_entry",
        ChunkType::DataRow => "data_row",
    }
}

// Borrow `_` for unused imports to keep the public surface minimal.
#[allow(unused_imports)]
use {ContentHash as _, SourceId as _};
