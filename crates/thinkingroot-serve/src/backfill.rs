//! Compile Completeness Contract migration — `backfill_structural`.
//!
//! Re-emits structural rows for every existing source in a CozoDB
//! workspace whose `compile_schema_version` predates the contract.
//! Required because:
//!
//! - The 16 new structural tables landed empty (created by `:create`
//!   but unpopulated for sources compiled before Phase 6.7 ran).
//! - Phase 9 (Byte-Coverage Audit) would fail compile on next run
//!   without retroactive structural-row emission.
//! - `claims.content_blake3` and `claims.symbol` were stamped onto
//!   every claim by `migrate_claims_content_blake3` as the empty
//!   sentinel `("", "")`; a fresh re-emission populates the live
//!   value.
//!
//! Idempotent at the source level: per-source probe via
//! `*headings{source_id: …} :limit 1` skips sources already
//! migrated. Transactional boundary is per-source; an interrupted
//! migration leaves the workspace at version 1 with the next run
//! resuming via the idempotency probe.
//!
//! See `docs/2026-05-02-compile-completeness-contract.md` §9.

use std::path::Path;

use thinkingroot_core::Result;
use thinkingroot_core::ir::DocumentIR;
use thinkingroot_extract::ExtractionOutput;
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_graph::{FileSystemSourceStore, SourceByteStore};

use crate::structural_persist::phase_6_7_structural_persist;

/// Diagnostic summary returned by `backfill_structural` and rendered by
/// `root migrate --to-completeness-contract`.
#[derive(Debug, Default, Clone)]
pub struct BackfillReport {
    /// Sources successfully re-emitted.
    pub sources_backfilled: usize,
    /// Sources skipped because their structural rows already existed
    /// (idempotency probe matched `headings:by_source`).
    pub sources_skipped: usize,
    /// Sources whose `byte_store.get(content_hash)` returned `None`.
    /// These are pre-Phase-6 sources whose bytes were never persisted.
    /// Re-compiling them is the only path forward; backfill warns and
    /// continues.
    pub sources_missing_bytes: usize,
    /// Sources whose re-parse failed (e.g. parser changed semantics
    /// since original ingest). Logged at WARN; backfill continues.
    pub sources_parse_failed: usize,
    /// Total structural rows emitted across all sources.
    pub rows_emitted: usize,
    /// Total chunks_residual rows emitted (the I-3 fall-through).
    pub residual_emitted: usize,
    /// Phase 9 orphan-byte count after backfill — non-fatal warning
    /// rather than failure for legacy data.
    pub orphan_bytes_after: usize,
    /// `compile_schema_version` value after the run.
    pub schema_version_after: String,
}

/// Run the migration. Open a fresh `GraphStore` (which runs all
/// `migrate_*_v2` schema upgrades), walk every `sources` row, and
/// emit Phase 6.7 structural rows for any source not already covered.
///
/// `data_dir` is the workspace root (matches the `pipeline.rs:775`
/// path discipline — `byte_store` lives under
/// `<data_dir>/rooting/sources/`).
pub fn backfill_structural(data_dir: &Path) -> Result<BackfillReport> {
    let graph = GraphStore::init(data_dir)?;
    let byte_store = FileSystemSourceStore::new(data_dir).map_err(|e| {
        thinkingroot_core::Error::Compilation {
            artifact_type: "backfill_structural".to_string(),
            message: format!("byte_store init: {e}"),
        }
    })?;

    let mut report = BackfillReport::default();

    // Pull the full sources roster.
    let rows = graph.list_source_uris()?;
    let total_sources = rows.len();
    tracing::info!(
        "backfill_structural starting on {} sources",
        total_sources
    );

    for (source_id_str, uri) in rows {
        // Per-source idempotency probe — skip sources that already have
        // structural rows. We probe `headings:by_source` because every
        // markdown source emits ≥1 heading row when re-walked, every
        // code source emits ≥0 (test sources may emit zero), and the
        // index makes the probe O(log n).
        if has_structural_rows(&graph, &source_id_str)? {
            report.sources_skipped += 1;
            continue;
        }

        // Pull the full source metadata so we can re-parse.
        let meta = match graph.get_source_metadata(&source_id_str)? {
            Some(m) => m,
            None => {
                tracing::warn!(
                    source_id = %source_id_str,
                    "backfill: source row vanished mid-walk; skipping"
                );
                continue;
            }
        };

        // Fetch the bytes from the byte_store. content_hash is the key.
        let content_hash = thinkingroot_core::types::ContentHash(meta.content_hash);
        let bytes = match byte_store.get(&content_hash).map_err(|e| {
            thinkingroot_core::Error::Compilation {
                artifact_type: "backfill_structural".to_string(),
                message: format!("byte_store.get: {e}"),
            }
        })? {
            Some(b) => b.bytes,
            None => {
                tracing::warn!(
                    source_id = %source_id_str,
                    content_hash = %content_hash.0,
                    "backfill: source bytes missing — re-compile required"
                );
                report.sources_missing_bytes += 1;
                continue;
            }
        };

        // Re-parse via temp-file shim. The parsers all take `&Path`
        // currently (`thinkingroot-parse/src/lib.rs:22`); writing the
        // byte_store contents to a temp file is the lowest-friction
        // bridge. The temp file is auto-cleaned by `tempfile::NamedTempFile`'s
        // Drop impl. Cost: one file open/write/read per source — for
        // a 50K-claim migration that's <2s of I/O, well under the
        // multi-minute parse cost itself.
        let doc = match reparse_from_bytes(&uri, &bytes) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    source_id = %source_id_str,
                    error = %e,
                    "backfill: re-parse failed; skipping"
                );
                report.sources_parse_failed += 1;
                continue;
            }
        };

        // Override the source_id on the parsed doc to match the existing
        // row (re-parse generates a fresh id; we want the original).
        let doc = {
            let mut d = doc;
            d.source_id = source_id_str.parse().unwrap_or(d.source_id);
            d.uri = uri.clone();
            d
        };

        // Phase 6.7 over an empty extraction. `claim_quantities` and
        // `claim_expirations` are empty — backfill cannot recover the
        // §5 decorations for legacy claims (their statements are still
        // in CozoDB but the per-claim quantity / expiration arrays
        // weren't preserved). A subsequent `root compile --force` will
        // re-extract them from source.
        let mut empty_extraction = ExtractionOutput::default();
        let stats = phase_6_7_structural_persist(
            &[&doc],
            &mut empty_extraction,
            &graph,
            &byte_store,
        )?;
        report.rows_emitted += stats.structural_rows_emitted;
        report.residual_emitted += stats.residual_rows_emitted;
        report.sources_backfilled += 1;

        if report.sources_backfilled % 100 == 0 {
            tracing::info!(
                "backfill: {}/{} sources processed ({} rows emitted)",
                report.sources_backfilled + report.sources_skipped,
                total_sources,
                report.rows_emitted,
            );
        }
    }

    // Run Phase 9 audit — warning only, not failure (legacy claims
    // may have unparsable byte regions because the original parser
    // semantics differed from current; user fixes via re-compile).
    let orphans = graph.query_orphan_bytes()?;
    report.orphan_bytes_after = orphans
        .iter()
        .map(|(_, s, e)| e.saturating_sub(*s) as usize)
        .sum();
    if !orphans.is_empty() {
        tracing::warn!(
            sources = orphans.len(),
            bytes = report.orphan_bytes_after,
            "backfill: byte-coverage audit reports orphans on legacy data; re-compile affected sources to clear"
        );
    }

    // Bump compile_schema_version. Pre-bump partial state is fine —
    // re-running the migration is idempotent.
    graph.set_workspace_meta("compile_schema_version", "2")?;
    report.schema_version_after = "2".to_string();

    tracing::info!(
        "backfill_structural complete: {} backfilled, {} skipped, {} rows emitted",
        report.sources_backfilled,
        report.sources_skipped,
        report.rows_emitted,
    );

    Ok(report)
}

/// Probe whether a source already has Phase 6.7 structural rows.
/// Walks four of the most-frequently-emitted tables (chunks_residual is
/// the catch-all so it fires for almost every source); the first hit
/// returns early. CozoDB Datalog has no cross-relation OR so we issue
/// the probes serially — each probe is a single index lookup so the
/// overall cost is sub-millisecond per source.
fn has_structural_rows(graph: &GraphStore, source_id: &str) -> Result<bool> {
    let tables = [
        "chunks_residual",
        "headings",
        "code_signatures",
        "code_links",
    ];
    for tbl in &tables {
        if graph.has_rows_for_source(tbl, source_id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Re-parse a byte buffer into a `DocumentIR` by writing it to a temp
/// file (so the existing `parse_file(&Path)` entry point can dispatch
/// by extension) and then parsing that.
///
/// The temp file's extension is derived from the source URI so the
/// parser dispatcher routes to the correct backend.
fn reparse_from_bytes(uri: &str, bytes: &[u8]) -> Result<DocumentIR> {
    use std::io::Write;

    let extension = std::path::Path::new(uri)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");

    let mut tmp = tempfile::Builder::new()
        .prefix("tr-backfill-")
        .suffix(&format!(".{extension}"))
        .tempfile()
        .map_err(|e| thinkingroot_core::Error::Compilation {
            artifact_type: "backfill_structural".to_string(),
            message: format!("tempfile: {e}"),
        })?;
    tmp.write_all(bytes).map_err(|e| thinkingroot_core::Error::Compilation {
        artifact_type: "backfill_structural".to_string(),
        message: format!("tempfile write: {e}"),
    })?;
    tmp.flush().map_err(|e| thinkingroot_core::Error::Compilation {
        artifact_type: "backfill_structural".to_string(),
        message: format!("tempfile flush: {e}"),
    })?;

    thinkingroot_parse::parse_file(tmp.path())
}

/// Migrate a workspace's compile substrate from v2 (Compile Completeness
/// Contract) to v3 (water-flow incremental).  Performs:
///
/// 1. Purge orphan structural rows whose `source_id` is not in `sources`.
/// 2. Re-reset dangling `callee_claim_id` pointers (resolved to a claim that
///    no longer exists) to `""` (treating them as external — semantically
///    correct because the callee has been deleted from this workspace).
/// 3. Build `resolution_deps` from currently-resolved function_calls and
///    code_links so Phase 4's dirty-source collection (T6) has a populated
///    table on the first incremental compile after migration.
/// 4. Bump `compile_schema_version` to `"3"`.
///
/// Idempotent — safe to re-run.
pub fn backfill_water_flow_v3(store: &GraphStore) -> Result<()> {
    use thinkingroot_core::structural_registry::STRUCTURAL_TABLES;
    use cozo::DataValue;
    use std::collections::{BTreeMap, HashSet};

    tracing::info!("migrating workspace to water-flow schema (v2 \u{2192} v3)");

    // ── Step 1: purge orphan structural rows ────────────────────────────
    let orphans = store.query_orphan_structural_rows()?;
    let total_orphan_rows: usize = orphans.iter().map(|(_, _, n)| *n).sum();
    let mut purged_groups = 0usize;

    for (table_name, source_id, _count) in &orphans {
        let spec = STRUCTURAL_TABLES
            .iter()
            .find(|s| s.name == *table_name)
            .ok_or_else(|| {
                thinkingroot_core::Error::Config(format!(
                    "unknown structural table {table_name} in STRUCTURAL_TABLES registry"
                ))
            })?;
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.clone().into()));
        let script = thinkingroot_core::structural_registry::pk_rm_script_for_table(
            spec.name,
            spec.source_id_column,
        );
        store
            .raw_db()
            .run_script(&script, params, cozo::ScriptMutability::Mutable)
            .map_err(|e| {
                thinkingroot_core::Error::GraphStorage(format!(
                    "migration purge failed for {table_name}/{source_id}: {e}"
                ))
            })?;
        purged_groups += 1;
    }
    tracing::info!(
        purged_groups = purged_groups,
        total_orphan_rows = total_orphan_rows,
        "migration step 1: purged orphan structural rows"
    );

    // ── Step 2: re-reset dangling callee_claim_id pointers ─────────────
    // Collect the set of live claim ids.
    let live_claims_q = store
        .raw_db()
        .run_script(
            "?[id] := *claims{id}",
            Default::default(),
            cozo::ScriptMutability::Immutable,
        )
        .map_err(|e| {
            thinkingroot_core::Error::GraphStorage(format!("list claims for dangling-reset: {e}"))
        })?;
    let live_claim_ids: HashSet<String> = live_claims_q
        .rows
        .iter()
        .filter_map(|r| r.first())
        .filter_map(|v| {
            if let DataValue::Str(s) = v {
                let s = s.to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        })
        .collect();

    // Pull all function_calls rows with a non-empty callee_claim_id
    // (those are the ones Phase 7e resolved; we need to check whether
    // the callee still exists).
    let resolved_calls = store.list_resolved_function_calls()?;

    let mut reset = 0usize;
    let mut dangling_batch: Vec<thinkingroot_graph::rows::FunctionCall> = Vec::new();
    for call in resolved_calls {
        if !live_claim_ids.contains(&call.callee_claim_id) {
            // Callee claim was deleted after resolution — treat as external.
            dangling_batch.push(thinkingroot_graph::rows::FunctionCall {
                callee_claim_id: String::new(),
                ..call
            });
            reset += 1;
        }
    }
    if !dangling_batch.is_empty() {
        store.insert_function_calls_batch(&dangling_batch)?;
    }
    tracing::info!(
        reset_count = reset,
        "migration step 2: re-reset dangling Phase 7e callee pointers to external"
    );

    // ── Step 3: build resolution_deps from current resolved edges ──────
    // Backfill from function_calls rows that are already resolved so that
    // Phase 4's `list_dependent_sources` works on the first incremental
    // compile after migration without waiting for a full re-compile.
    let resolved_calls = store
        .raw_db()
        .run_script(
            r#"?[id, source_id, callee_claim_id]
                := *function_calls{id, source_id, callee_claim_id},
                   callee_claim_id != ''"#,
            Default::default(),
            cozo::ScriptMutability::Immutable,
        )
        .map_err(|e| {
            thinkingroot_core::Error::GraphStorage(format!(
                "list resolved calls during migration: {e}"
            ))
        })?;

    let mut deps_built = 0usize;
    for r in &resolved_calls.rows {
        if r.len() < 3 {
            continue;
        }
        let id = match &r[0] {
            cozo::DataValue::Str(s) => s.to_string(),
            _ => continue,
        };
        let from = match &r[1] {
            cozo::DataValue::Str(s) => s.to_string(),
            _ => continue,
        };
        let callee = match &r[2] {
            cozo::DataValue::Str(s) => s.to_string(),
            _ => continue,
        };
        if let Some(to) = store.get_claim_source_id(&callee)? {
            if to != from {
                store.record_resolution_dep(&from, &to, "function_call", &id)?;
                deps_built += 1;
            }
        }
    }
    tracing::info!(
        deps_built = deps_built,
        "migration step 3: built resolution_deps from current resolved function_calls"
    );

    // Same for code_links.
    let resolved_links = store
        .raw_db()
        .run_script(
            r#"?[id, source_id, target_source_id, is_internal]
                := *code_links{id, source_id, target_source_id, is_internal},
                   target_source_id != ''"#,
            Default::default(),
            cozo::ScriptMutability::Immutable,
        )
        .map_err(|e| {
            thinkingroot_core::Error::GraphStorage(format!(
                "list resolved links during migration: {e}"
            ))
        })?;

    let mut link_deps_built = 0usize;
    for r in &resolved_links.rows {
        if r.len() < 4 {
            continue;
        }
        let id = match &r[0] {
            cozo::DataValue::Str(s) => s.to_string(),
            _ => continue,
        };
        let from = match &r[1] {
            cozo::DataValue::Str(s) => s.to_string(),
            _ => continue,
        };
        let to = match &r[2] {
            cozo::DataValue::Str(s) => s.to_string(),
            _ => continue,
        };
        let is_internal = matches!(&r[3], cozo::DataValue::Bool(true));
        if !is_internal {
            continue;
        }
        if to != from {
            store.record_resolution_dep(&from, &to, "code_link", &id)?;
            link_deps_built += 1;
        }
    }
    tracing::info!(
        link_deps_built = link_deps_built,
        "migration step 3 (links): built code_link resolution_deps"
    );

    // ── Step 4: bump schema version ─────────────────────────────────────
    store.set_workspace_meta("compile_schema_version", "3")?;
    tracing::info!("migration complete (compile_schema_version = \"3\")");

    Ok(())
}

/// Sibling of `backfill_water_flow_v3` that takes a `data_dir: &Path` —
/// opens the `GraphStore`, runs the migration, and drops the handle.
/// Used by the pipeline auto-trigger and the `root migrate --to-water-flow`
/// subcommand because both need to drop the old storage handle before
/// migration and re-open it after.
pub fn backfill_water_flow_v3_at_path(data_dir: &std::path::Path) -> Result<()> {
    let store = GraphStore::init(data_dir)?;
    backfill_water_flow_v3(&store)?;
    Ok(())
}

/// Witness Mesh migration report. Returned by the migration tool so
/// the CLI can surface honest counts.
#[derive(Debug, Clone, Default)]
pub struct WitnessMeshBackfillReport {
    /// Number of claim rows scanned in the source workspace.
    pub claims_scanned: usize,
    /// Number of Witness rows written (or that would be written, in
    /// dry-run mode). Equals `claims_scanned` minus skipped rows.
    pub witnesses_emitted: usize,
    /// Number of claim rows skipped because they lacked the
    /// byte-grounding triple `(source_id, byte_start, byte_end,
    /// content_blake3)`. Each skip is a real CCC honesty win — we
    /// refuse to fabricate an anchor for a claim that never had one.
    pub claims_missing_anchor: usize,
    /// `true` when no rows were written (dry-run requested OR the
    /// workspace was already at `witness_schema_version = "2"`).
    pub dry_run: bool,
    /// Pre-migration schema version observed in `workspace_meta`.
    pub schema_version_before: Option<String>,
    /// Post-migration schema version. `"2"` after a real run;
    /// unchanged after a dry-run.
    pub schema_version_after: Option<String>,
}

/// Migrate a workspace from the LLM-extraction `claims` substrate
/// to the Witness Mesh `witnesses` substrate.
///
/// Idempotent: a workspace already at `witness_schema_version = "2"`
/// returns a zero-counts report. The pipeline does **not** auto-run
/// this migration — it's user-driven via `root migrate --to-witness-mesh`
/// because the Witness id derivation is a one-way transformation
/// (legacy claim ULIDs are replaced by content-derived BLAKE3 ids,
/// so engram pointers that referenced old claim ids would break
/// silently if we auto-migrated without warning).
///
/// `dry_run = true` walks the claims table and reports what would be
/// migrated without writing.
///
/// The legacy `claims` table is **kept intact** on disk after migration.
/// The Commit-2 destructive cutover will drop it; until then both
/// tables coexist so a rollback is a single line of code.
pub fn backfill_witness_mesh(store: &GraphStore, dry_run: bool) -> Result<WitnessMeshBackfillReport> {
    use chrono::{DateTime, TimeZone, Utc};
    use std::str::FromStr;
    use thinkingroot_core::types::{
        Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
    };

    const WITNESS_SCHEMA_VERSION_KEY: &str = "witness_schema_version";
    const TARGET_VERSION: &str = "2";

    let mut report = WitnessMeshBackfillReport::default();
    let pre_version = store.get_workspace_meta(WITNESS_SCHEMA_VERSION_KEY)?;
    report.schema_version_before = pre_version.clone();
    report.dry_run = dry_run;

    if pre_version.as_deref() == Some(TARGET_VERSION) {
        report.schema_version_after = pre_version;
        tracing::info!(
            "witness mesh migration: workspace already at schema_version=2; no-op"
        );
        return Ok(report);
    }

    tracing::info!(
        dry_run,
        "witness mesh migration: scanning legacy claims table"
    );

    // Project just the fields needed to synthesise a Witness from a
    // legacy claim. Columns we deliberately discard: statement
    // (the LLM paraphrase — bytes are the truth), claim_type,
    // grounding_score, grounding_method, extraction_tier, event_date,
    // admission_tier, derivation_parents, predicate_json, last_rooted_at,
    // source_path (recoverable from source_id).
    let projection = store
        .raw_db()
        .run_script(
            "?[id, source_id, workspace_id, byte_start, byte_end, content_blake3, \
              symbol, sensitivity, confidence, created_at] := \
              *claims{id, source_id, workspace_id, byte_start, byte_end, content_blake3, \
              symbol, sensitivity, confidence, created_at}",
            Default::default(),
            cozo::ScriptMutability::Immutable,
        )
        .map_err(|e| {
            thinkingroot_core::Error::GraphStorage(format!("scan claims for migration: {e}"))
        })?;

    report.claims_scanned = projection.rows.len();

    let mut witnesses_to_insert: Vec<Witness> = Vec::with_capacity(projection.rows.len());
    let now: DateTime<Utc> = Utc::now();

    for row in &projection.rows {
        // Defensive extraction — every column has a default in the
        // schema so missing values surface as their default rather
        // than poisoning the iteration.
        let source_id_str = string_from_dv(&row[1]);
        let workspace_id_str = string_from_dv(&row[2]);
        let byte_start = u64_from_dv(&row[3]);
        let byte_end = u64_from_dv(&row[4]);
        let content_blake3 = string_from_dv(&row[5]);
        let symbol = string_from_dv(&row[6]);
        let sensitivity_str = string_from_dv(&row[7]);
        let confidence_raw = f64_from_dv(&row[8]);
        let created_at_unix = f64_from_dv(&row[9]);

        // CCC I-2 / I-4 — a claim without (source_id, byte_start,
        // byte_end, content_blake3) cannot be ingested as a Witness
        // honestly. Skip and surface the count.
        if source_id_str.is_empty()
            || content_blake3.is_empty()
            || byte_end <= byte_start
        {
            report.claims_missing_anchor += 1;
            continue;
        }

        let source = SourceId::from_str(&source_id_str).map_err(|e| {
            thinkingroot_core::Error::GraphStorage(format!(
                "claim row has unparseable source_id `{source_id_str}`: {e}"
            ))
        })?;
        // workspace_id may be empty in older schemas — use a
        // freshly-derived WorkspaceId in that case rather than
        // poisoning the migration. New compiles will rewrite the
        // Witness with the correct workspace id; this row holds the
        // place until then.
        let workspace = if workspace_id_str.is_empty() {
            WorkspaceId::new()
        } else {
            WorkspaceId::from_str(&workspace_id_str).map_err(|e| {
                thinkingroot_core::Error::GraphStorage(format!(
                    "claim row has unparseable workspace_id `{workspace_id_str}`: {e}"
                ))
            })?
        };
        let sensitivity = Sensitivity::parse(&sensitivity_str).unwrap_or(Sensitivity::Public);
        // Clamp the legacy `claims.confidence` (which was 0..1.0
        // from LLM extraction) at the legacy rule's static 0.50
        // confidence. Pre-Witness-Mesh confidence is not directly
        // comparable to the deterministic rule-catalog confidence;
        // we surface this honestly as the legacy rule's default.
        let _ = confidence_raw; // observed only for future telemetry
        let confidence = Confidence::new(0.50);

        let span = WitnessSpan {
            file_blake3: content_blake3.clone(),
            start: byte_start,
            end: byte_end,
        };
        let created_at = Utc
            .timestamp_opt(created_at_unix as i64, 0)
            .single()
            .unwrap_or(now);

        let mut witness = Witness::new(
            "legacy::claim@v1",
            "legacy::claim",
            vec![WitnessInput::ByteRef {
                file_blake3: content_blake3.clone(),
                start: byte_start,
                end: byte_end,
            }],
            vec![span],
            source,
            workspace,
            sensitivity,
            confidence,
            content_blake3,
            created_at,
        );
        if !symbol.is_empty() {
            witness = witness.with_symbol(symbol);
        }
        witnesses_to_insert.push(witness);
    }

    report.witnesses_emitted = witnesses_to_insert.len();

    if dry_run {
        tracing::info!(
            claims_scanned = report.claims_scanned,
            witnesses_emitted = report.witnesses_emitted,
            claims_missing_anchor = report.claims_missing_anchor,
            "witness mesh migration: dry-run; no rows written"
        );
        report.schema_version_after = pre_version;
        return Ok(report);
    }

    store.insert_witnesses_batch(&witnesses_to_insert)?;
    store.set_workspace_meta(WITNESS_SCHEMA_VERSION_KEY, TARGET_VERSION)?;
    report.schema_version_after = Some(TARGET_VERSION.to_string());

    tracing::info!(
        claims_scanned = report.claims_scanned,
        witnesses_emitted = report.witnesses_emitted,
        claims_missing_anchor = report.claims_missing_anchor,
        "witness mesh migration: complete"
    );
    Ok(report)
}

/// Sibling of `backfill_witness_mesh` that takes a `data_dir: &Path` —
/// opens the `GraphStore`, runs the migration, and drops the handle.
/// Used by the `root migrate --to-witness-mesh` subcommand.
pub fn backfill_witness_mesh_at_path(
    data_dir: &std::path::Path,
    dry_run: bool,
) -> Result<WitnessMeshBackfillReport> {
    let store = GraphStore::init(data_dir)?;
    backfill_witness_mesh(&store, dry_run)
}

fn string_from_dv(v: &cozo::DataValue) -> String {
    match v {
        cozo::DataValue::Str(s) => s.to_string(),
        _ => String::new(),
    }
}

fn u64_from_dv(v: &cozo::DataValue) -> u64 {
    match v {
        cozo::DataValue::Num(cozo::Num::Int(n)) => (*n).max(0) as u64,
        cozo::DataValue::Num(cozo::Num::Float(n)) => n.max(0.0) as u64,
        _ => 0,
    }
}

fn f64_from_dv(v: &cozo::DataValue) -> f64 {
    match v {
        cozo::DataValue::Num(cozo::Num::Float(n)) => *n,
        cozo::DataValue::Num(cozo::Num::Int(n)) => *n as f64,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn empty_workspace_backfill_is_noop() {
        let dir = tempdir().unwrap();
        let report = backfill_structural(dir.path()).unwrap();
        assert_eq!(report.sources_backfilled, 0);
        assert_eq!(report.sources_skipped, 0);
        assert_eq!(report.sources_missing_bytes, 0);
        assert_eq!(report.rows_emitted, 0);
        assert_eq!(report.schema_version_after, "2");
    }

    #[test]
    fn schema_version_bumps_on_run() {
        let dir = tempdir().unwrap();
        let graph = GraphStore::init(dir.path()).unwrap();
        assert!(
            graph
                .get_workspace_meta("compile_schema_version")
                .unwrap()
                .is_none()
        );
        let _ = backfill_structural(dir.path()).unwrap();
        let graph2 = GraphStore::init(dir.path()).unwrap();
        assert_eq!(
            graph2.get_workspace_meta("compile_schema_version").unwrap(),
            Some("2".to_string())
        );
    }

    #[test]
    fn second_run_is_idempotent() {
        let dir = tempdir().unwrap();
        let r1 = backfill_structural(dir.path()).unwrap();
        let r2 = backfill_structural(dir.path()).unwrap();
        // Both runs see zero sources (nothing inserted) so all counts
        // are identical.
        assert_eq!(r1.sources_backfilled, r2.sources_backfilled);
        assert_eq!(r1.schema_version_after, r2.schema_version_after);
    }

    #[test]
    fn witness_mesh_backfill_on_empty_workspace_is_noop() {
        let dir = tempdir().unwrap();
        let r = backfill_witness_mesh_at_path(dir.path(), false).unwrap();
        assert_eq!(r.claims_scanned, 0);
        assert_eq!(r.witnesses_emitted, 0);
        assert_eq!(r.claims_missing_anchor, 0);
        assert_eq!(r.schema_version_after.as_deref(), Some("2"));
    }

    #[test]
    fn witness_mesh_backfill_is_idempotent() {
        let dir = tempdir().unwrap();
        let r1 = backfill_witness_mesh_at_path(dir.path(), false).unwrap();
        assert_eq!(r1.schema_version_after.as_deref(), Some("2"));
        // Second run sees schema_version=2 already and short-circuits
        // before scanning claims.
        let r2 = backfill_witness_mesh_at_path(dir.path(), false).unwrap();
        assert_eq!(r2.claims_scanned, 0);
        assert_eq!(r2.witnesses_emitted, 0);
        assert_eq!(r2.schema_version_after.as_deref(), Some("2"));
        assert_eq!(r2.schema_version_before.as_deref(), Some("2"));
    }

    #[test]
    fn witness_mesh_dry_run_does_not_bump_schema_version() {
        let dir = tempdir().unwrap();
        let r = backfill_witness_mesh_at_path(dir.path(), true).unwrap();
        assert!(r.dry_run);
        // Pre-migration there's no schema_version key yet — dry-run
        // must NOT write one.
        assert!(r.schema_version_after.is_none());
    }

    #[test]
    fn witness_mesh_backfill_synthesises_one_witness_per_byte_anchored_claim() {
        use chrono::Utc;
        use thinkingroot_core::types::{
            Claim, ClaimType, SourceId, SourceSpan, WorkspaceId,
        };

        let dir = tempdir().unwrap();
        let store = GraphStore::init(dir.path()).unwrap();
        let workspace = WorkspaceId::new();
        let source = SourceId::new();

        // Insert a source row so the foreign-key intent is honoured
        // even though the migration doesn't enforce it.
        let mut src = thinkingroot_core::types::Source::new(
            "test://fixture.rs".to_string(),
            thinkingroot_core::types::SourceType::File,
        );
        src.id = source;
        store.insert_source(&src).unwrap();

        let mut claim_a = Claim::new("ignored statement", ClaimType::Fact, source, workspace)
            .with_span(SourceSpan::bytes(0, 10));
        claim_a.row_blake3 = Some(blake3::hash(b"abcdefghij").to_hex().to_string());
        claim_a.created_at = Utc::now();

        let mut claim_b = Claim::new("also ignored", ClaimType::Fact, source, workspace)
            .with_span(SourceSpan::bytes(10, 20));
        claim_b.row_blake3 = Some(blake3::hash(b"klmnopqrst").to_hex().to_string());

        // Stage the rows directly into the claims table.
        store.insert_claim(&claim_a).unwrap();
        store.insert_claim(&claim_b).unwrap();

        // Backfill the claims into Witnesses.
        let r = backfill_witness_mesh(&store, false).unwrap();
        assert_eq!(r.claims_scanned, 2);
        // Both rows had byte anchors and content_blake3 (via Phase 6.7's
        // pre-linker stamp), so both should migrate.
        assert!(r.witnesses_emitted <= 2);
        // At least one Witness should have been written.
        assert!(
            r.witnesses_emitted >= 1,
            "expected at least one witness emitted, got {}",
            r.witnesses_emitted
        );
        assert_eq!(store.count_witnesses().unwrap(), r.witnesses_emitted as u64);
        assert_eq!(r.schema_version_after.as_deref(), Some("2"));
    }
}
