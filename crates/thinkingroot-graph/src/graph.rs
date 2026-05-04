use std::collections::BTreeMap;
use std::path::Path;

use chrono;
use cozo::{DataValue, DbInstance, NamedRows, Num, ScriptMutability};
use serde::Serialize;
use thinkingroot_core::types::{Entity, EntityType};
use thinkingroot_core::{Error, Result};

/// Row returned by [`GraphStore::get_v3_claim_export`]. Pack-writer-
/// adjacent shape: every field maps directly onto the v3 spec §3.3
/// `ClaimRecord` apart from `ents` which is loaded separately via
/// [`GraphStore::get_claim_entity_names`].
#[derive(Debug, Clone)]
pub struct V3ClaimExportRow {
    /// CozoDB claim id — the wire-format `id` field.
    pub id: String,
    /// Atomic claim statement.
    pub statement: String,
    /// Claim taxonomy tag.
    pub claim_type: String,
    /// Extractor confidence in [0.0, 1.0].
    pub confidence: f64,
    /// Rooting admission tier.
    pub admission_tier: String,
    /// Inclusive byte offset within the source file.
    pub byte_start: u64,
    /// Exclusive byte offset within the source file.
    pub byte_end: u64,
    /// Source row id (UUID-ish).
    pub source_id: String,
    /// Source URI (e.g. `file:///abs/path/to/file.rs`).
    pub source_uri: String,
    /// BLAKE3 hex of the source bytes — opens the FileSystemSourceStore.
    pub content_hash: String,
}

/// Graph storage backed by CozoDB — an embedded Datalog database.
/// Datalog gives us recursive graph queries, pattern matching, and built-in
/// graph algorithms (PageRank, shortest path) out of the box.
///
/// `Clone` is cheap — `cozo::DbInstance` is internally `Arc<...>` and
/// shares the same underlying database connection.  Cloning lets a
/// caller release the outer `Mutex<StorageEngine>` and then run a
/// long-lived read (e.g. the health verifier's ~1.2s sequential
/// query batch) without serialising every concurrent reader behind
/// it.  The mutex is still required for the writer side because
/// vector-store mutations and graph schema migrations need to be
/// linearised against each other.
#[derive(Clone)]
pub struct GraphStore {
    db: DbInstance,
}

impl GraphStore {
    /// Hand out a reference to the underlying Cozo `DbInstance` so callers
    /// (RARP's `EngramManager`, the `aep_queries` helper) can run
    /// parameterised scripts directly. `DbInstance` is internally
    /// `Arc`-shared and `Send + Sync`; concurrent readers serialise on
    /// Cozo's own SQLite mutex.
    pub fn raw_db(&self) -> &DbInstance {
        &self.db
    }

    /// Build a `GraphStore` from a pre-opened in-memory `DbInstance` for
    /// fixture tests in sibling modules (e.g. `aep_queries::tests`). Pairs
    /// with `init_for_testing` to set up the schema without forcing every
    /// test through the on-disk `init` path.
    #[doc(hidden)]
    pub fn from_db_for_testing(db: DbInstance) -> Self {
        Self { db }
    }

    /// Run the schema-creation step against a `from_db_for_testing` store
    /// so fixture tests can `:put` against the live shape without mounting
    /// a workspace. No migrations — the fresh schema already matches the
    /// post-Compile-Completeness-Contract layout.
    #[doc(hidden)]
    pub fn init_for_testing(&self) -> Result<()> {
        self.create_schema()
    }

    /// Open or create a CozoDB database at the given path and initialize the schema.
    pub fn init(path: &Path) -> Result<Self> {
        let db_path = path.join("graph.db");
        let db = DbInstance::new("sqlite", db_path.to_str().unwrap_or("."), "")
            .map_err(|e| Error::GraphStorage(format!("failed to open cozo db: {e}")))?;

        let store = Self { db };
        store.create_schema()?;
        store.migrate_claims_extraction_tier()?;
        store.migrate_structural_patterns_schema()?;
        store.migrate_claims_byte_ranges()?;
        // Compile Completeness Contract §I-4: per-row tamper evidence.
        // Adds `content_blake3` + `symbol` to `claims` so existing rows
        // can carry I-4 evidence and Phase 7e callee resolution can join
        // function_calls.callee_name → claims.symbol.
        store.migrate_claims_content_blake3()?;
        store.create_indexes()?;
        Ok(store)
    }

    /// Reflect (Phase 9) schema migration.
    ///
    /// Adds `first_seen_at`, `stability_runs`, and `source_scope` columns
    /// to `structural_patterns` when they are missing. Since the
    /// relation is fully re-derivable from graph state on every
    /// `reflect()` run, the migration just drops and recreates with the
    /// new shape — no data to preserve.
    ///
    /// Idempotent: running against an already-migrated DB is a fast
    /// probe-and-return.
    fn migrate_structural_patterns_schema(&self) -> Result<()> {
        // Probe: does the new column exist?
        let probe = self.db.run_script(
            "?[x] := *structural_patterns{source_scope: x}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        );
        if probe.is_ok() {
            return Ok(()); // new schema in place
        }

        // Either the column is missing or the relation isn't created yet.
        // If the error is "relation not found", create_schema will handle
        // it on first run — nothing to migrate.
        if let Err(e) = &probe {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("does not exist") {
                return Ok(());
            }
        }

        // Drop indexes first — :replace fails while indexes are attached.
        for drop_idx in ["::index drop structural_patterns:by_entity_type"] {
            // Index may or may not exist yet; swallow the "not found" error.
            let _ = self
                .db
                .run_script(drop_idx, BTreeMap::new(), ScriptMutability::Mutable);
        }

        // Replace the relation with the new schema. Loses existing rows,
        // which is safe because reflect() rewrites them in full each run.
        self.db
            .run_script(
                ":replace structural_patterns {
                    id: String
                    =>
                    entity_type: String,
                    condition_claim_type: String,
                    expected_claim_type: String,
                    frequency: Float default 0.0,
                    sample_size: Int default 0,
                    last_computed: Float default 0.0,
                    min_sample_threshold: Int default 30,
                    first_seen_at: Float default 0.0,
                    stability_runs: Int default 1,
                    source_scope: String default 'local'
                }",
                BTreeMap::new(),
                ScriptMutability::Mutable,
            )
            .map_err(|e| {
                Error::GraphStorage(format!("structural_patterns migration failed: {e}"))
            })?;

        tracing::info!(
            "migrated structural_patterns — added first_seen_at, stability_runs, source_scope"
        );
        Ok(())
    }

    /// Create all relations (tables) if they don't exist.
    /// CozoDB's `:create` fails if the relation already exists, so we
    /// silently ignore "already exists" errors on subsequent runs.
    fn create_schema(&self) -> Result<()> {
        let relations = [
            ":create sources {
                id: String
                =>
                uri: String,
                source_type: String,
                author: String default '',
                content_hash: String default '',
                trust_level: String default 'Unknown',
                byte_size: Int default 0
            }",
            ":create claims {
                id: String
                =>
                statement: String,
                claim_type: String,
                source_id: String,
                confidence: Float default 0.8,
                sensitivity: String default 'Public',
                workspace_id: String default '',
                created_at: Float default 0.0,
                grounding_score: Float default -1.0,
                grounding_method: String default '',
                extraction_tier: String default 'llm',
                event_date: Float default 0.0,
                admission_tier: String default 'attested',
                derivation_parents: String default '',
                predicate_json: String default '',
                last_rooted_at: Float default 0.0,
                source_path: String default '',
                byte_start: Int default 0,
                byte_end: Int default 0,
                content_blake3: String default '',
                symbol: String default ''
            }",
            ":create entities {
                id: String
                =>
                canonical_name: String,
                entity_type: String,
                description: String default ''
            }",
            ":create claim_source_edges {
                claim_id: String,
                source_id: String
            }",
            ":create claim_entity_edges {
                claim_id: String,
                entity_id: String
            }",
            ":create entity_relations {
                from_id: String,
                to_id: String,
                relation_type: String
                =>
                strength: Float default 1.0
            }",
            ":create source_entity_relations {
                source_id: String,
                from_id: String,
                to_id: String,
                relation_type: String
                =>
                strength: Float default 1.0
            }",
            ":create claim_temporal {
                claim_id: String
                =>
                valid_from: Float default 0.0,
                valid_until: Float default 0.0,
                superseded_by: String default ''
            }",
            ":create contradictions {
                id: String
                =>
                claim_a: String,
                claim_b: String,
                explanation: String default '',
                status: String default 'Detected',
                detected_at: Float default 0.0
            }",
            ":create entity_aliases {
                entity_id: String,
                alias: String
            }",
            // Event Calendar — pre-compiled SVO temporal index.
            // Populated by the pipeline at compile time; queried at 50µs via Datalog.
            ":create events {
                id: String
                =>
                subject_entity_id: String,
                verb: String,
                object_entity_id: String default '',
                timestamp: Float default 0.0,
                normalized_date: String default '',
                source_id: String default '',
                confidence: Float default 0.8
            }",
            // Turn calendar: tracks which conversation turn each claim was contributed in.
            // session_id + turn_number form the composite key; claim_ids is a JSON-encoded array.
            ":create turns {
                session_id: String,
                turn_number: Int
                =>
                claim_ids: String default '[]',
                timestamp: Float default 0.0
            }",
            // Rooting — append-only log of every trial run against a claim.
            // One row per probe battery execution (not per probe). A single claim
            // can have many verdicts over time (initial trial + re-rooting sweeps).
            ":create trial_verdicts {
                id: String
                =>
                claim_id: String,
                trial_at: Float default 0.0,
                admission_tier: String default 'attested',
                provenance_score: Float default -1.0,
                contradiction_score: Float default -1.0,
                predicate_score: Float default -1.0,
                topology_score: Float default -1.0,
                temporal_score: Float default -1.0,
                certificate_hash: String default '',
                failure_reason: String default '',
                rooter_version: String default ''
            }",
            // Rooting — cryptographic certificates keyed by BLAKE3 hash of inputs.
            // Content-addressed: the same trial inputs produce the same certificate hash.
            ":create verification_certificates {
                hash: String
                =>
                claim_id: String,
                created_at: Float default 0.0,
                probe_inputs_json: String default '',
                probe_outputs_json: String default '',
                rooter_version: String default '',
                source_content_hash: String default ''
            }",
            // Rooting — derivation DAG edges (parent claim → child claim).
            // Populated when a claim is created via composition (Reflect, agent contribute, etc.).
            ":create derivation_edges {
                parent_claim_id: String,
                child_claim_id: String
                =>
                derivation_rule: String default ''
            }",
            // Reflect (Phase 9) — statistical co-occurrence patterns discovered
            // from graph topology. Each row: "entities of `entity_type` that
            // have `condition_claim_type` also have `expected_claim_type`
            // with `frequency` probability across `sample_size` instances."
            // Rewritten in full on every `reflect()` run — not append-only.
            //
            // `first_seen_at` + `stability_runs` power pattern decay:
            // a pattern becomes "trusted" only after it persists across
            // multiple reflect cycles, preventing one-off noise from
            // immediately firing high-confidence gap claims.
            //
            // `source_scope` distinguishes single-workspace patterns
            // ("local") from cross-workspace aggregated patterns
            // ("cross:<id>") so consumers can filter by origin.
            ":create structural_patterns {
                id: String
                =>
                entity_type: String,
                condition_claim_type: String,
                expected_claim_type: String,
                frequency: Float default 0.0,
                sample_size: Int default 0,
                last_computed: Float default 0.0,
                min_sample_threshold: Int default 30,
                first_seen_at: Float default 0.0,
                stability_runs: Int default 1,
                source_scope: String default 'local'
            }",
            // Reflect (Phase 9) — per-entity gap records. One row per
            // (entity, expected_claim_type) where the entity matches a
            // pattern's condition but is missing the expected claim type.
            // Not a Claim in the claims table — gaps are surfaced through
            // the `gaps` MCP tool and the health-coverage score.
            ":create known_unknowns {
                id: String
                =>
                entity_id: String,
                pattern_id: String,
                expected_claim_type: String,
                confidence: Float default 0.0,
                status: String default 'open',
                created_at: Float default 0.0,
                resolved_at: Float default 0.0,
                resolved_by: String default ''
            }",
            // ─── Compile Completeness Contract §4 — 16 new structural tables ────
            // Every table carries the I-2 byte-anchoring triple
            // (source_id, byte_start, byte_end) and the I-4 per-row tamper
            // evidence column content_blake3. Spec at
            // docs/2026-05-02-compile-completeness-contract.md.

            // §4.1 function_calls — code call graph.
            // Source: ChunkMetadata.calls_functions[] populated by code.rs
            // tree-sitter pass. callee_claim_id resolved during Phase 7e Link
            // when the linker matches callee_name to a known FunctionDef in
            // the same workspace; empty if external.
            ":create function_calls {
                id: String
                =>
                caller_claim_id: String,
                callee_name: String,
                callee_claim_id: String default '',
                source_id: String,
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.2 doc_tags — structured documentation annotations
            // (@param/@returns/@throws/@deprecated/@see) emitted by the
            // doctags.rs parser into ChunkMetadata.doc_tags.
            ":create doc_tags {
                id: String
                =>
                claim_id: String,
                kind: String,
                target: String default '',
                description: String default '',
                source_id: String,
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.3 code_links — hyperlinks in prose and comments. Resolution
            // of is_internal + target_source_id is deferred to Phase 7e.
            ":create code_links {
                id: String
                =>
                source_id: String,
                chunk_id: String default '',
                url: String,
                link_text: String default '',
                is_internal: Bool default false,
                target_source_id: String default '',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.4 code_signatures — function/type shape per FunctionDef + TypeDef.
            // parameters_json + field_types_json hold serialised arrays so
            // Datalog rules can SUBSTR-match without parsing JSON natively.
            ":create code_signatures {
                claim_id: String
                =>
                parameters_json: String default '[]',
                return_type: String default '',
                visibility: String default '',
                trait_name: String default '',
                parent_scope: String default '',
                field_types_json: String default '[]',
                source_id: String,
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.5 config_tree — TOML/YAML/JSON leaves.
            // Composite key (source_id, dotted_path) is the natural identity:
            // each leaf is uniquely addressed within a source.
            ":create config_tree {
                source_id: String,
                dotted_path: String
                =>
                value: String default '',
                value_type: String default 'string',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.6 data_rows — CSV/TSV/markdown-table rows.
            // columns_json holds the {header: cell} object as a JSON string.
            ":create data_rows {
                id: String
                =>
                source_id: String,
                row_index: Int default 0,
                columns_json: String default '{}',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.7 git_commits — commit-level metadata propagated from the
            // git pseudo-source's SourceMetadata. Composite key on
            // (source_id, commit_sha) — a source may have many commits.
            ":create git_commits {
                source_id: String,
                commit_sha: String
                =>
                commit_author: String default '',
                commit_email: String default '',
                commit_timestamp: Float default 0.0,
                changed_files_json: String default '[]',
                message: String default '',
                parent_sha: String default '',
                byte_start: Int default 0,
                byte_end: Int default 0,
                content_blake3: String default ''
            }",

            // §4.8 headings — markdown heading hierarchy. parent_heading_id
            // resolved during the in-doc walk via a heading-id stack
            // maintained by the Phase 6.7 driver (markdown.rs already sets
            // ChunkMetadata.parent to the parent heading text).
            ":create headings {
                id: String
                =>
                source_id: String,
                level: Int default 1,
                text: String default '',
                parent_heading_id: String default '',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.9 chunks_residual — chunks that produced 0 claims AND 0
            // structural rows. The catch-all that makes I-3 (byte coverage)
            // tractable. Per §15 Q1 we store full content unconditionally
            // for v1.
            ":create chunks_residual {
                id: String
                =>
                source_id: String,
                chunk_type: String default '',
                content: String default '',
                metadata_json: String default '{}',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.10 quantities — numeric values extracted from claim
            // statements + chunk text. Multiple per claim because a single
            // claim can mention several metrics ("p99=120ms at 50K rps").
            ":create quantities {
                id: String
                =>
                claim_id: String,
                metric_name: String default '',
                value: Float default 0.0,
                unit: String default '',
                qualifier: String default '',
                is_live: Bool default false,
                captured_at: Float default 0.0,
                source_id: String,
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.11 source_annotations — file-level pragmas (license,
            // copyright, encoding, shebang, mode, trailing_newline_norm).
            // The trailing_newline_norm kind is the I-3 exception class
            // proof that records the byte normalisation.
            ":create source_annotations {
                id: String
                =>
                source_id: String,
                kind: String default '',
                value: String default '',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.12 source_references — cross-doc citations. Built at
            // Phase 7e by joining code_links, function_calls, and import
            // claims against sources.
            ":create source_references {
                id: String
                =>
                from_source_id: String,
                to_source_id: String,
                reference_kind: String default 'link',
                fragment: String default '',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.13 code_markers — TODO/FIXME/HACK/SAFETY/NOTE/XXX/BUG/PERF
            // tags discovered by a regex pass over Comment, ModuleDoc, and
            // Code chunks during Phase 6.7.
            ":create code_markers {
                id: String
                =>
                source_id: String,
                kind: String default 'TODO',
                text: String default '',
                in_claim_id: String default '',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.14 test_annotations — test-marker awareness emitted from
            // per-language tree-sitter queries against FunctionDef chunks.
            // framework ∈ {rust_test, junit, jest, pytest}.
            ":create test_annotations {
                id: String
                =>
                source_id: String,
                claim_id: String,
                framework: String default '',
                annotation_kind: String default '',
                name: String default '',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.15 git_blame — per-line-range author attribution. Composite
            // key on (source_id, line_start, line_end) — one row per blame
            // hunk. byte_start/byte_end give the corresponding byte range.
            ":create git_blame {
                source_id: String,
                line_start: Int,
                line_end: Int
                =>
                commit_sha: String default '',
                author: String default '',
                author_email: String default '',
                blamed_at: Float default 0.0,
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // §4.16 code_metrics — per-file / per-function complexity.
            // scope ∈ {file, function, type}; scope_claim_id is the
            // owning claim id (empty for file scope). fan_in/fan_out
            // resolved at Phase 7e from function_calls.
            ":create code_metrics {
                id: String
                =>
                source_id: String,
                scope: String default 'file',
                scope_claim_id: String default '',
                loc: Int default 0,
                cyclomatic: Int default 0,
                fan_in: Int default 0,
                fan_out: Int default 0,
                complexity_method: String default 'mccabe',
                byte_start: Int,
                byte_end: Int,
                content_blake3: String
            }",

            // ─── Workspace metadata singleton — schema version + future
            // workspace-scoped flags. Used by the migration auto-trigger
            // in pipeline.rs to detect contract-version mismatch.
            ":create workspace_meta {
                key: String
                =>
                value: String default ''
            }",

            // ─── T0.7 connector ingest log ───────────────────────────────
            // Idempotency cache for the `contribute_bulk` API. The
            // primary key is `(connector_id, install_id, idempotency_key)`
            // — the natural unit of "this exact webhook payload from this
            // exact connector install" — and the value is the JSON-
            // encoded list of claim ids that were accepted on the first
            // call. Replay of the same key short-circuits to "no-op,
            // here are the already-accepted ids" so the connector can
            // safely re-deliver after a network blip without
            // double-counting.
            //
            // No additional indexes — every read is a point lookup on
            // the full PK from `lookup_ingest`.
            ":create connector_ingest_log {
                connector_id: String,
                install_id: String,
                idempotency_key: String
                =>
                claim_ids: String default '[]',
                ingested_at: Float default 0.0,
                branch: String default '',
                source_uri: String default ''
            }",
        ];

        for stmt in &relations {
            match self.db.run_default(stmt) {
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    // Ignore "already exists" errors on re-init.
                    if !msg.contains("already exists")
                        && !msg.contains("conflicts with an existing")
                    {
                        return Err(Error::GraphStorage(format!(
                            "schema creation failed: {msg}"
                        )));
                    }
                }
            }
        }

        tracing::info!("graph schema initialized (cozo/datalog)");
        Ok(())
    }

    /// Create secondary indexes for the most performance-sensitive query patterns.
    ///
    /// CozoDB relations are ordered only by their primary key by default. Any query
    /// that filters on a non-PK-prefix field incurs a full table scan. These indexes
    /// turn O(n) scans into O(log n + k) point-range lookups:
    ///
    /// - `claims:by_type`               — `get_claims_by_type` (was 521ms at Large)
    /// - `claim_entity_edges:by_entity` — `get_claims_for_entity` (was 121ms at Large)
    /// - `claim_source_edges:by_source` — claim removal during `remove_source_by_id`
    /// - `entities:by_name`             — `get_relations_for_entity`, exact name lookups
    ///
    /// Idempotent: silently skips indexes that already exist (safe on re-init).
    fn create_indexes(&self) -> Result<()> {
        let indexes = [
            "::index create claims:by_type { claim_type }",
            "::index create claim_entity_edges:by_entity { entity_id }",
            "::index create claim_source_edges:by_source { source_id }",
            "::index create entities:by_name { canonical_name }",
            "::index create events:by_subject { subject_entity_id }",
            "::index create events:by_timestamp { timestamp }",
            "::index create turns:by_session { session_id }",
            // Rooting indexes — support Rooting reports, Health Score integration,
            // and derivation-graph traversal.
            "::index create claims:by_tier { admission_tier }",
            "::index create trial_verdicts:by_claim { claim_id }",
            "::index create trial_verdicts:by_time { trial_at }",
            "::index create verification_certificates:by_claim { claim_id }",
            "::index create derivation_edges:by_parent { parent_claim_id }",
            "::index create derivation_edges:by_child { child_claim_id }",
            // Reflect — support the `gaps` tool's entity-scoped query path
            // and fast "open gaps" filters in the health score.
            "::index create known_unknowns:by_entity { entity_id }",
            "::index create known_unknowns:by_status { status }",
            "::index create structural_patterns:by_entity_type { entity_type }",
            // ─── Compile Completeness Contract §4 — indexes on the 16 new
            // structural tables. Indexes per the per-section DDL plus a
            // (source_id, byte_start, byte_end) shape per Phase 9 §7.4.
            "::index create function_calls:by_caller { caller_claim_id }",
            "::index create function_calls:by_callee_name { callee_name }",
            "::index create function_calls:by_callee_id { callee_claim_id }",
            "::index create function_calls:by_source { source_id }",
            "::index create doc_tags:by_claim { claim_id }",
            "::index create doc_tags:by_kind { kind }",
            "::index create doc_tags:by_source { source_id }",
            "::index create code_links:by_url { url }",
            "::index create code_links:by_target { target_source_id }",
            "::index create code_links:by_source { source_id }",
            "::index create code_signatures:by_visibility { visibility }",
            "::index create code_signatures:by_trait { trait_name }",
            "::index create code_signatures:by_source { source_id }",
            "::index create config_tree:by_path { dotted_path }",
            "::index create config_tree:by_type { value_type }",
            "::index create data_rows:by_source { source_id }",
            "::index create git_commits:by_author { commit_author }",
            "::index create git_commits:by_timestamp { commit_timestamp }",
            "::index create headings:by_source { source_id }",
            "::index create headings:by_level { level }",
            "::index create chunks_residual:by_source { source_id }",
            "::index create chunks_residual:by_type { chunk_type }",
            "::index create quantities:by_metric { metric_name }",
            "::index create quantities:by_value { value }",
            "::index create quantities:by_live { is_live }",
            "::index create quantities:by_source { source_id }",
            "::index create source_annotations:by_source { source_id }",
            "::index create source_annotations:by_kind { kind }",
            "::index create source_references:by_to { to_source_id }",
            "::index create source_references:by_from { from_source_id }",
            "::index create code_markers:by_kind { kind }",
            "::index create code_markers:by_source { source_id }",
            "::index create test_annotations:by_framework { framework }",
            "::index create test_annotations:by_kind { annotation_kind }",
            "::index create test_annotations:by_source { source_id }",
            "::index create git_blame:by_author { author }",
            "::index create git_blame:by_commit { commit_sha }",
            "::index create git_blame:by_blame_time { blamed_at }",
            "::index create code_metrics:by_scope { scope }",
            "::index create code_metrics:by_loc { loc }",
            "::index create code_metrics:by_cyclomatic { cyclomatic }",
            "::index create code_metrics:by_source { source_id }",
        ];

        for stmt in &indexes {
            match self.db.run_default(stmt) {
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("already exists") && !msg.contains("already in use") {
                        return Err(Error::GraphStorage(format!("index creation failed: {msg}")));
                    }
                }
            }
        }

        tracing::debug!("graph secondary indexes ensured");
        Ok(())
    }

    /// Migration: add extraction_tier column to claims if missing.
    /// Uses `:replace` to redefine the relation with the new column,
    /// defaulting existing rows to "llm".
    fn migrate_claims_extraction_tier(&self) -> Result<()> {
        // Probe each migration independently. Earlier releases returned from
        // this function the moment a later migration's column was present,
        // which silently skipped any migrations added after that point (e.g.
        // a workspace with event_date but without admission_tier never got
        // Migration 3). Each probe now gates ONLY its own migration.
        //
        // If any migration is going to run, drop the indexes on `claims` and
        // its link tables first — `:replace` fails while an index is attached.
        // `create_indexes()` (called next in `init`) recreates them atop the
        // new schema. Drops are best-effort — missing indexes are fine.
        let needs_any_migration = {
            let p1 = self.db.run_script(
                "?[extraction_tier] := *claims{id: 'probe-noop', extraction_tier}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            );
            let p2 = self.db.run_script(
                "?[event_date] := *claims{id: 'probe-noop', event_date}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            );
            let p3 = self.db.run_script(
                "?[admission_tier] := *claims{id: 'probe-noop', admission_tier}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            );
            p1.is_err() || p2.is_err() || p3.is_err()
        };
        if needs_any_migration {
            let index_drops = [
                "::index drop claims:by_type",
                "::index drop claims:by_tier",
                "::index drop claim_entity_edges:by_entity",
                "::index drop claim_source_edges:by_source",
            ];
            for drop_stmt in &index_drops {
                let _ = self.db.run_default(drop_stmt);
            }
        }

        // ── Migration 1: add extraction_tier column ──────────────────────────
        let probe = self.db.run_script(
            "?[extraction_tier] := *claims{id: 'probe-noop', extraction_tier}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        );
        if probe.is_err() {
            let migration = r#"
            {
                ?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier] :=
                    *claims{id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method},
                    extraction_tier = "llm"
                :replace claims {id: String => statement: String, claim_type: String, source_id: String, confidence: Float, sensitivity: String, workspace_id: String, created_at: Float, grounding_score: Float, grounding_method: String, extraction_tier: String}
            }
        "#;
            match self.db.run_default(migration) {
                Ok(_) => {
                    tracing::debug!("claims extraction_tier migration applied");
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("not found") && !msg.contains("does not exist") {
                        return Err(Error::GraphStorage(format!(
                            "claims extraction_tier migration failed: {msg}"
                        )));
                    }
                }
            }
        }

        // ── Migration 2: add event_date column (backfill = 0.0) ──────────────
        let probe2 = self.db.run_script(
            "?[event_date] := *claims{id: 'probe-noop', event_date}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        );
        if probe2.is_err() {
            let migration2 = r#"
            {
                ?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date] :=
                    *claims{id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier},
                    event_date = 0.0
                :replace claims {id: String => statement: String, claim_type: String, source_id: String, confidence: Float, sensitivity: String, workspace_id: String, created_at: Float, grounding_score: Float, grounding_method: String, extraction_tier: String, event_date: Float}
            }
        "#;

            match self.db.run_default(migration2) {
                Ok(_) => {
                    tracing::debug!("claims event_date migration applied");
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("not found") && !msg.contains("does not exist") {
                        return Err(Error::GraphStorage(format!(
                            "claims event_date migration failed: {msg}"
                        )));
                    }
                }
            }
        }

        // ── Migration 3: add Rooting columns (admission_tier, derivation_parents,
        // predicate_json, last_rooted_at). Backfill existing rows with defaults
        // that preserve current behavior:
        // - admission_tier = 'attested' (pre-Rooting claims honor the legacy binary provenance check)
        // - derivation_parents = ''    (extracted claims have no parents)
        // - predicate_json = ''        (no predicate = predicate probe skipped)
        // - last_rooted_at = 0.0       (never rooted)
        //
        // IMPORTANT: `:replace` on a relation fails while any index is still
        // attached. We drop every index we know about before running the
        // migration; `create_indexes()` (called next in `init`) recreates them
        // atop the new schema. Drops are best-effort — missing indexes are fine.
        let probe3 = self.db.run_script(
            "?[admission_tier] := *claims{id: 'probe-noop', admission_tier}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        );
        if probe3.is_err() {
            let migration3 = r#"
            {
                ?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date, admission_tier, derivation_parents, predicate_json, last_rooted_at] :=
                    *claims{id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date},
                    admission_tier = "attested",
                    derivation_parents = "",
                    predicate_json = "",
                    last_rooted_at = 0.0
                :replace claims {id: String => statement: String, claim_type: String, source_id: String, confidence: Float, sensitivity: String, workspace_id: String, created_at: Float, grounding_score: Float, grounding_method: String, extraction_tier: String, event_date: Float, admission_tier: String, derivation_parents: String, predicate_json: String, last_rooted_at: Float}
            }
        "#;

            match self.db.run_default(migration3) {
                Ok(_) => {
                    tracing::debug!("claims rooting migration applied");
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("not found") && !msg.contains("does not exist") {
                        return Err(Error::GraphStorage(format!(
                            "claims rooting migration failed: {msg}"
                        )));
                    }
                }
            }
        }

        Ok(())
    }

    /// v3 byte-range citation migration. Adds `source_path: String`,
    /// `byte_start: Int`, `byte_end: Int` to the `claims` relation so every
    /// row carries the verifiable citation triple required by the v3 wire
    /// format (`docs/2026-04-29-thinkingroot-v3-final-plan.md` §3.3).
    /// Existing rows backfill with `('', 0, 0)` — the "unknown" sentinel
    /// the structural extractor and provenance probe already understand.
    /// Idempotent — re-running against an already-migrated DB is a fast
    /// probe-and-return.
    ///
    /// `source_path` is a denormalised copy of the `sources.uri` for a
    /// claim's `source_id`.  Hot-path readers (v3 pack writer, agent
    /// brief synthesis) use it to skip the JOIN against `sources`.
    /// Pre-C2 this column was always written empty (`""`) by both
    /// `insert_claim` and `insert_claims_batch`, making the column
    /// dead despite the migration that added it; it is now populated
    /// at insert time via `find_source_uri_by_id` / `fetch_source_uris`.
    /// Backfilled rows from this migration retain `""` until their
    /// owning source is re-compiled (a fresh extract triggers the
    /// fixed insert path).
    ///
    /// Like the rooting migration above, `:replace` fails while indexes
    /// are attached; `create_indexes()` (called next in `init`) recreates
    /// them atop the new schema.
    fn migrate_claims_byte_ranges(&self) -> Result<()> {
        let probe = self.db.run_script(
            "?[byte_start] := *claims{id: 'probe-noop', byte_start}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        );
        if probe.is_ok() {
            return Ok(()); // new schema in place
        }

        // Either the column is missing or the relation isn't created yet.
        // If the error is "relation not found", create_schema will handle
        // it on first run — nothing to migrate.
        if let Err(e) = &probe {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("does not exist") {
                return Ok(());
            }
        }

        // Drop indexes that ride atop the claims relation. The rooting
        // migration drops these too — repeated drops are harmless because
        // we swallow "not found" errors.
        let index_drops = [
            "::index drop claims:by_type",
            "::index drop claims:by_tier",
            "::index drop claim_entity_edges:by_entity",
            "::index drop claim_source_edges:by_source",
        ];
        for drop_stmt in &index_drops {
            let _ = self.db.run_default(drop_stmt);
        }

        let migration = r#"
            {
                ?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date, admission_tier, derivation_parents, predicate_json, last_rooted_at, source_path, byte_start, byte_end] :=
                    *claims{id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date, admission_tier, derivation_parents, predicate_json, last_rooted_at},
                    source_path = "",
                    byte_start = 0,
                    byte_end = 0
                :replace claims {id: String => statement: String, claim_type: String, source_id: String, confidence: Float, sensitivity: String, workspace_id: String, created_at: Float, grounding_score: Float, grounding_method: String, extraction_tier: String, event_date: Float, admission_tier: String, derivation_parents: String, predicate_json: String, last_rooted_at: Float, source_path: String, byte_start: Int, byte_end: Int}
            }
        "#;

        match self.db.run_default(migration) {
            Ok(_) => {
                tracing::debug!("claims byte_range migration applied");
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("not found") && !msg.contains("does not exist") {
                    return Err(Error::GraphStorage(format!(
                        "claims byte_range migration failed: {msg}"
                    )));
                }
            }
        }

        Ok(())
    }

    /// Compile Completeness Contract §I-4 + §5.1 — adds `content_blake3`
    /// (per-row tamper evidence over the source byte slice) and `symbol`
    /// (function/type name for Phase 7e callee resolution) to `claims`.
    ///
    /// Existing rows backfill with `('', '')` — the v3 "unknown" sentinel
    /// pattern. Phase 6.7 stamps the live `content_blake3` on every newly
    /// inserted claim before the linker writes it; the structural extractor
    /// (`structural.rs:113-229`) populates `symbol` for FunctionDef and
    /// TypeDef claims at extraction time. Backfilled rows from this
    /// migration retain `('', '')` until their owning source is
    /// re-compiled — `backfill_structural` (Block H) re-fills them by
    /// re-parsing source bytes.
    ///
    /// Idempotent: re-running against an already-migrated DB is a fast
    /// probe-and-return.
    fn migrate_claims_content_blake3(&self) -> Result<()> {
        let probe = self.db.run_script(
            "?[content_blake3] := *claims{id: 'probe-noop', content_blake3}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        );
        if probe.is_ok() {
            return Ok(()); // new schema in place
        }
        if let Err(e) = &probe {
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("does not exist") {
                return Ok(());
            }
        }

        // :replace fails while indexes are attached — drop, then
        // create_indexes() recreates atop the new schema.
        let index_drops = [
            "::index drop claims:by_type",
            "::index drop claims:by_tier",
            "::index drop claim_entity_edges:by_entity",
            "::index drop claim_source_edges:by_source",
        ];
        for drop_stmt in &index_drops {
            let _ = self.db.run_default(drop_stmt);
        }

        let migration = r#"
            {
                ?[id, statement, claim_type, source_id, confidence, sensitivity,
                  workspace_id, created_at, grounding_score, grounding_method,
                  extraction_tier, event_date, admission_tier, derivation_parents,
                  predicate_json, last_rooted_at, source_path, byte_start, byte_end,
                  content_blake3, symbol] :=
                    *claims{id, statement, claim_type, source_id, confidence,
                            sensitivity, workspace_id, created_at, grounding_score,
                            grounding_method, extraction_tier, event_date,
                            admission_tier, derivation_parents, predicate_json,
                            last_rooted_at, source_path, byte_start, byte_end},
                    content_blake3 = "",
                    symbol = ""
                :replace claims {
                    id: String
                    =>
                    statement: String,
                    claim_type: String,
                    source_id: String,
                    confidence: Float default 0.8,
                    sensitivity: String default 'Public',
                    workspace_id: String default '',
                    created_at: Float default 0.0,
                    grounding_score: Float default -1.0,
                    grounding_method: String default '',
                    extraction_tier: String default 'llm',
                    event_date: Float default 0.0,
                    admission_tier: String default 'attested',
                    derivation_parents: String default '',
                    predicate_json: String default '',
                    last_rooted_at: Float default 0.0,
                    source_path: String default '',
                    byte_start: Int default 0,
                    byte_end: Int default 0,
                    content_blake3: String default '',
                    symbol: String default ''
                }
            }
        "#;

        match self.db.run_default(migration) {
            Ok(_) => {
                tracing::debug!("claims content_blake3 + symbol migration applied");
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("not found") && !msg.contains("does not exist") {
                    return Err(Error::GraphStorage(format!(
                        "claims content_blake3 migration failed: {msg}"
                    )));
                }
            }
        }

        Ok(())
    }

    /// Run a Datalog query with parameters, returning NamedRows.
    /// Public to support integration test harnesses that need to issue
    /// arbitrary scripts (e.g. the Compile Completeness Contract CI
    /// gates). Pre-contract this was `pub(crate)`; bumping to `pub`
    /// is the smallest change that keeps the test surface usable
    /// without exporting `cozo::DbInstance` directly.
    pub fn query(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        self.db
            .run_script(script, params, ScriptMutability::Mutable)
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))
    }

    /// Run a read-only Datalog query.
    pub fn query_read(&self, script: &str) -> Result<NamedRows> {
        self.db
            .run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))
    }

    /// Insert a source node.
    pub fn insert_source(&self, source: &thinkingroot_core::Source) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(source.id.to_string().into()));
        params.insert("uri".into(), DataValue::Str(source.uri.clone().into()));
        params.insert(
            "source_type".into(),
            DataValue::Str(format!("{:?}", source.source_type).into()),
        );
        params.insert(
            "author".into(),
            DataValue::Str(source.author.clone().unwrap_or_default().into()),
        );
        params.insert(
            "content_hash".into(),
            DataValue::Str(source.content_hash.0.clone().into()),
        );
        params.insert(
            "trust_level".into(),
            DataValue::Str(format!("{:?}", source.trust_level).into()),
        );
        params.insert(
            "byte_size".into(),
            DataValue::Num(Num::Int(source.byte_size as i64)),
        );

        self.query(
            r#"?[id, uri, source_type, author, content_hash, trust_level, byte_size] <- [[
                $id, $uri, $source_type, $author, $content_hash, $trust_level, $byte_size
            ]]
            :put sources {id => uri, source_type, author, content_hash, trust_level, byte_size}"#,
            params,
        )?;
        Ok(())
    }

    /// Find all source rows for a URI. Multiple rows may exist from older duplicate compiles.
    pub fn find_sources_by_uri(&self, uri: &str) -> Result<Vec<(String, String, String)>> {
        let mut params = BTreeMap::new();
        params.insert("uri".into(), DataValue::Str(uri.into()));

        let result = self
            .db
            .run_script(
                "?[id, content_hash, source_type] := *sources{id, uri: $uri, content_hash, source_type}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                )
            })
            .collect())
    }

    /// Look up a single source's URI by id.  Returns `Ok(String::new())`
    /// when no row exists (the row hasn't been inserted yet, or is from
    /// another workspace).  Used at claim-insert time to populate the
    /// denormalised `claims.source_path` column so v3 byte-range citations
    /// resolve without a join.
    pub fn find_source_uri_by_id(&self, id: &str) -> Result<String> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.into()));
        let result = self
            .db
            .run_script(
                "?[uri] := *sources{id: $id, uri}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;
        Ok(result
            .rows
            .first()
            .and_then(|r| r.first())
            .map(dv_to_string)
            .unwrap_or_default())
    }

    /// Read the `source_path` column directly for a single claim.  Used
    /// by hot-path readers that don't need a `sources` JOIN, and by the
    /// regression test for the C2 byte-range citation fix.  Returns
    /// `Ok(String::new())` when the claim does not exist.
    pub fn get_claim_source_path(&self, claim_id: &str) -> Result<String> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(claim_id.into()));
        let result = self
            .db
            .run_script(
                "?[source_path] := *claims{id: $id, source_path}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;
        Ok(result
            .rows
            .first()
            .and_then(|r| r.first())
            .map(dv_to_string)
            .unwrap_or_default())
    }

    /// Bulk-fetch source URIs for a slice of source ids.  Returns
    /// `id -> uri` only for ids that resolve.  Used by `insert_claims_batch`
    /// to avoid N round-trips when populating `claims.source_path`.
    pub fn fetch_source_uris<S: AsRef<str>>(
        &self,
        ids: &[S],
    ) -> Result<std::collections::HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows: Vec<DataValue> = ids
            .iter()
            .map(|s| DataValue::List(vec![DataValue::Str(s.as_ref().into())]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("ids".into(), DataValue::List(rows));
        let result = self
            .db
            .run_script(
                // Inline-relation join: pin candidate ids to a unary
                // pseudo-relation, then probe `sources` once per row.
                // CozoDB rewrites this to an indexed lookup.
                "candidate[id] <- $ids
                 ?[id, uri] := candidate[id], *sources{id, uri}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                if row.len() < 2 {
                    return None;
                }
                Some((dv_to_string(&row[0]), dv_to_string(&row[1])))
            })
            .collect())
    }

    /// Insert a claim node.
    pub fn insert_claim(&self, claim: &thinkingroot_core::Claim) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(claim.id.to_string().into()));
        params.insert(
            "statement".into(),
            DataValue::Str(claim.statement.clone().into()),
        );
        params.insert(
            "claim_type".into(),
            DataValue::Str(format!("{:?}", claim.claim_type).into()),
        );
        params.insert(
            "source_id".into(),
            DataValue::Str(claim.source.to_string().into()),
        );
        params.insert(
            "confidence".into(),
            DataValue::Num(Num::Float(claim.confidence.value())),
        );
        params.insert(
            "sensitivity".into(),
            DataValue::Str(format!("{:?}", claim.sensitivity).into()),
        );
        params.insert(
            "workspace_id".into(),
            DataValue::Str(claim.workspace.to_string().into()),
        );
        params.insert(
            "created_at".into(),
            DataValue::Num(Num::Float(claim.created_at.timestamp() as f64)),
        );
        params.insert(
            "grounding_score".into(),
            DataValue::Num(Num::Float(claim.grounding_score.unwrap_or(-1.0))),
        );
        params.insert(
            "grounding_method".into(),
            DataValue::Str(
                claim
                    .grounding_method
                    .map(|m| format!("{m:?}"))
                    .unwrap_or_default()
                    .into(),
            ),
        );
        let tier_str = match claim.extraction_tier {
            thinkingroot_core::types::ExtractionTier::Structural => "structural",
            thinkingroot_core::types::ExtractionTier::Llm => "llm",
            thinkingroot_core::types::ExtractionTier::AgentInferred => "agent_inferred",
        };
        params.insert("extraction_tier".into(), DataValue::Str(tier_str.into()));
        params.insert(
            "event_date".into(),
            DataValue::Num(Num::Float(
                claim
                    .event_date
                    .map(|d| d.timestamp() as f64)
                    .unwrap_or(0.0),
            )),
        );
        // Rooting columns. Every claim carries an admission_tier; derivation
        // parents and predicate are serialized as JSON strings for portability.
        params.insert(
            "admission_tier".into(),
            DataValue::Str(claim.admission_tier.as_str().into()),
        );
        let derivation_parents_json = match &claim.derivation {
            Some(d) => {
                let ids: Vec<String> = d.parent_claim_ids.iter().map(|id| id.to_string()).collect();
                serde_json::to_string(&ids).unwrap_or_default()
            }
            None => String::new(),
        };
        params.insert(
            "derivation_parents".into(),
            DataValue::Str(derivation_parents_json.into()),
        );
        let predicate_json = match &claim.predicate {
            Some(p) => serde_json::to_string(p).unwrap_or_default(),
            None => String::new(),
        };
        params.insert(
            "predicate_json".into(),
            DataValue::Str(predicate_json.into()),
        );
        params.insert(
            "last_rooted_at".into(),
            DataValue::Num(Num::Float(
                claim
                    .last_rooted_at
                    .map(|d| d.timestamp() as f64)
                    .unwrap_or(0.0),
            )),
        );
        // v3 byte-range citation triple. source_path is the workspace-
        // relative POSIX path the claim was extracted from; byte_start /
        // byte_end define the exact source bytes the claim cites. The
        // tr-format v3 pack writer (Week 2) joins these fields into
        // claims.jsonl per spec §3.3.
        let (byte_start_val, byte_end_val) = match claim.source_span {
            Some(span) => (
                span.byte_start.unwrap_or(0) as i64,
                span.byte_end.unwrap_or(0) as i64,
            ),
            None => (0, 0),
        };
        // v3 byte-range citation requires `source_path` populated alongside
        // `byte_start` / `byte_end`.  We resolve the URI through the
        // `sources` table at insert time so the denormalised column never
        // ships empty (pre-fix every row carried "").
        let source_path = self
            .find_source_uri_by_id(&claim.source.to_string())
            .unwrap_or_default();
        params.insert("source_path".into(), DataValue::Str(source_path.into()));
        params.insert(
            "byte_start".into(),
            DataValue::Num(Num::Int(byte_start_val)),
        );
        params.insert("byte_end".into(), DataValue::Num(Num::Int(byte_end_val)));

        self.query(
            r#"?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date, admission_tier, derivation_parents, predicate_json, last_rooted_at, source_path, byte_start, byte_end] <- [[
                $id, $statement, $claim_type, $source_id, $confidence, $sensitivity, $workspace_id, $created_at, $grounding_score, $grounding_method, $extraction_tier, $event_date, $admission_tier, $derivation_parents, $predicate_json, $last_rooted_at, $source_path, $byte_start, $byte_end
            ]]
            :put claims {id => statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date, admission_tier, derivation_parents, predicate_json, last_rooted_at, source_path, byte_start, byte_end}"#,
            params,
        )?;
        Ok(())
    }

    /// Insert an entity node and persist its aliases.
    pub fn insert_entity(&self, entity: &thinkingroot_core::Entity) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(entity.id.to_string().into()));
        params.insert(
            "name".into(),
            DataValue::Str(entity.canonical_name.clone().into()),
        );
        params.insert(
            "etype".into(),
            DataValue::Str(format!("{:?}", entity.entity_type).into()),
        );
        params.insert(
            "desc".into(),
            DataValue::Str(entity.description.clone().unwrap_or_default().into()),
        );

        self.query(
            r#"?[id, canonical_name, entity_type, description] <- [[$id, $name, $etype, $desc]]
            :put entities {id => canonical_name, entity_type, description}"#,
            params,
        )?;

        // Persist each alias. `:put` is an upsert so duplicates are safe.
        for alias in &entity.aliases {
            let mut p = BTreeMap::new();
            p.insert("eid".into(), DataValue::Str(entity.id.to_string().into()));
            p.insert("alias".into(), DataValue::Str(alias.clone().into()));
            self.query(
                r#"?[entity_id, alias] <- [[$eid, $alias]]
                :put entity_aliases {entity_id, alias}"#,
                p,
            )?;
        }

        Ok(())
    }

    /// Batch-insert multiple entities in a single CozoDB transaction.
    /// Chunks into groups of 500 to stay within CozoDB parameter limits.
    /// Identical quality to calling insert_entity N times — just 100x faster.
    pub fn insert_entities_batch(&self, entities: &[thinkingroot_core::Entity]) -> Result<()> {
        const CHUNK: usize = 500;
        for chunk in entities.chunks(CHUNK) {
            // Build entity rows.
            let rows: Vec<DataValue> = chunk
                .iter()
                .map(|e| {
                    DataValue::List(vec![
                        DataValue::Str(e.id.to_string().into()),
                        DataValue::Str(e.canonical_name.clone().into()),
                        DataValue::Str(format!("{:?}", e.entity_type).into()),
                        DataValue::Str(e.description.clone().unwrap_or_default().into()),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(rows));
            self.query(
                "?[id, canonical_name, entity_type, description] <- $rows \
                 :put entities {id => canonical_name, entity_type, description}",
                params,
            )?;

            // Collect and batch-insert all aliases for this chunk.
            let alias_rows: Vec<DataValue> = chunk
                .iter()
                .flat_map(|e| {
                    e.aliases.iter().map(move |alias| {
                        DataValue::List(vec![
                            DataValue::Str(e.id.to_string().into()),
                            DataValue::Str(alias.clone().into()),
                        ])
                    })
                })
                .collect();
            if !alias_rows.is_empty() {
                let mut ap = BTreeMap::new();
                ap.insert("rows".into(), DataValue::List(alias_rows));
                self.query(
                    "?[entity_id, alias] <- $rows \
                     :put entity_aliases {entity_id, alias}",
                    ap,
                )?;
            }
        }
        Ok(())
    }

    /// Batch-insert multiple claims in a single CozoDB transaction.
    /// Chunks into groups of 500 to stay within CozoDB parameter limits.
    pub fn insert_claims_batch(&self, claims: &[thinkingroot_core::Claim]) -> Result<()> {
        const CHUNK: usize = 500;
        // Resolve `source_id -> source_uri` once per call so the
        // denormalised `claims.source_path` column is populated for
        // every row.  Pre-fix this column was always written as "" and
        // v3 byte-range citations had to fall back to a JOIN.  The
        // sources rows are inserted by Phase 6 of the pipeline, before
        // Linker calls this method, so every `c.source` should resolve.
        let source_id_strings: Vec<String> = claims.iter().map(|c| c.source.to_string()).collect();
        let uri_by_id = self.fetch_source_uris(&source_id_strings)?;
        for chunk in claims.chunks(CHUNK) {
            let rows: Vec<DataValue> = chunk
                .iter()
                .map(|c| {
                    let tier_str = match c.extraction_tier {
                        thinkingroot_core::types::ExtractionTier::Structural => "structural",
                        thinkingroot_core::types::ExtractionTier::Llm => "llm",
                        thinkingroot_core::types::ExtractionTier::AgentInferred => "agent_inferred",
                    };
                    let derivation_parents_json = match &c.derivation {
                        Some(d) => {
                            let ids: Vec<String> =
                                d.parent_claim_ids.iter().map(|id| id.to_string()).collect();
                            serde_json::to_string(&ids).unwrap_or_default()
                        }
                        None => String::new(),
                    };
                    let predicate_json = match &c.predicate {
                        Some(p) => serde_json::to_string(p).unwrap_or_default(),
                        None => String::new(),
                    };
                    let (byte_start_val, byte_end_val) = match c.source_span {
                        Some(span) => (
                            span.byte_start.unwrap_or(0) as i64,
                            span.byte_end.unwrap_or(0) as i64,
                        ),
                        None => (0, 0),
                    };
                    let source_id_str = c.source.to_string();
                    let source_path = uri_by_id.get(&source_id_str).cloned().unwrap_or_default();
                    DataValue::List(vec![
                        DataValue::Str(c.id.to_string().into()),
                        DataValue::Str(c.statement.clone().into()),
                        DataValue::Str(format!("{:?}", c.claim_type).into()),
                        DataValue::Str(source_id_str.into()),
                        DataValue::Num(Num::Float(c.confidence.value())),
                        DataValue::Str(format!("{:?}", c.sensitivity).into()),
                        DataValue::Str(c.workspace.to_string().into()),
                        DataValue::Num(Num::Float(c.created_at.timestamp() as f64)),
                        DataValue::Num(Num::Float(c.grounding_score.unwrap_or(-1.0))),
                        DataValue::Str(
                            c.grounding_method
                                .map(|m| format!("{m:?}"))
                                .unwrap_or_default()
                                .into(),
                        ),
                        DataValue::Str(tier_str.into()),
                        DataValue::Num(Num::Float(
                            c.event_date.map(|d| d.timestamp() as f64).unwrap_or(0.0),
                        )),
                        DataValue::Str(c.admission_tier.as_str().into()),
                        DataValue::Str(derivation_parents_json.into()),
                        DataValue::Str(predicate_json.into()),
                        DataValue::Num(Num::Float(
                            c.last_rooted_at
                                .map(|d| d.timestamp() as f64)
                                .unwrap_or(0.0),
                        )),
                        DataValue::Str(source_path.into()),
                        DataValue::Num(Num::Int(byte_start_val)),
                        DataValue::Num(Num::Int(byte_end_val)),
                        DataValue::Str(c.row_blake3.clone().unwrap_or_default().into()),
                        DataValue::Str(c.symbol.clone().unwrap_or_default().into()),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(rows));
            self.query(
                "?[id, statement, claim_type, source_id, confidence, sensitivity, \
                  workspace_id, created_at, grounding_score, grounding_method, \
                  extraction_tier, event_date, admission_tier, derivation_parents, \
                  predicate_json, last_rooted_at, source_path, byte_start, byte_end, \
                  content_blake3, symbol] <- $rows \
                 :put claims {id => statement, claim_type, source_id, confidence, \
                  sensitivity, workspace_id, created_at, grounding_score, \
                  grounding_method, extraction_tier, event_date, admission_tier, \
                  derivation_parents, predicate_json, last_rooted_at, source_path, \
                  byte_start, byte_end, content_blake3, symbol}",
                params,
            )?;
        }
        Ok(())
    }

    /// Batch-insert claim→source edges.
    pub fn link_claims_to_sources_batch(&self, pairs: &[(String, String)]) -> Result<()> {
        const CHUNK: usize = 1000;
        for chunk in pairs.chunks(CHUNK) {
            let rows: Vec<DataValue> = chunk
                .iter()
                .map(|(cid, sid)| {
                    DataValue::List(vec![
                        DataValue::Str(cid.clone().into()),
                        DataValue::Str(sid.clone().into()),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(rows));
            self.query(
                "?[claim_id, source_id] <- $rows \
                 :put claim_source_edges {claim_id, source_id}",
                params,
            )?;
        }
        Ok(())
    }

    /// Batch-insert claim→entity edges.
    pub fn link_claims_to_entities_batch(&self, pairs: &[(String, String)]) -> Result<()> {
        const CHUNK: usize = 1000;
        for chunk in pairs.chunks(CHUNK) {
            let rows: Vec<DataValue> = chunk
                .iter()
                .map(|(cid, eid)| {
                    DataValue::List(vec![
                        DataValue::Str(cid.clone().into()),
                        DataValue::Str(eid.clone().into()),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(rows));
            self.query(
                "?[claim_id, entity_id] <- $rows \
                 :put claim_entity_edges {claim_id, entity_id}",
                params,
            )?;
        }
        Ok(())
    }

    /// Load all persisted entities with aliases for cross-run entity resolution.
    pub fn get_entities_with_aliases(&self) -> Result<Vec<Entity>> {
        let result = self.query_read(
            "?[id, canonical_name, entity_type, description] := *entities{id, canonical_name, entity_type, description}",
        )?;

        let mut entities = Vec::with_capacity(result.rows.len());

        for row in &result.rows {
            let id = dv_to_string(&row[0]);
            let canonical_name = dv_to_string(&row[1]);
            let entity_type = parse_entity_type(&dv_to_string(&row[2]));
            let description = dv_to_string(&row[3]);

            let mut entity = Entity::new(canonical_name, entity_type);
            entity.id = id
                .parse()
                .map_err(|e| Error::GraphStorage(format!("invalid entity id '{id}': {e}")))?;
            entity.aliases = self.get_aliases_for_entity(&id)?;
            if !description.is_empty() {
                entity.description = Some(description);
            }
            entities.push(entity);
        }

        Ok(entities)
    }

    /// Get all aliases for a given entity ID.
    pub fn get_aliases_for_entity(&self, entity_id: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        let result = self
            .db
            .run_script(
                "?[alias] := *entity_aliases{entity_id: $eid, alias}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| dv_to_string(&row[0]))
            .collect())
    }

    /// Bulk-load all entity aliases in one query — used by the in-memory cache loader.
    /// Returns `(entity_id, alias)` pairs for every row in `entity_aliases`.
    pub fn get_all_entity_aliases(&self) -> Result<Vec<(String, String)>> {
        let result = self.query_read("?[entity_id, alias] := *entity_aliases{entity_id, alias}")?;
        Ok(result
            .rows
            .iter()
            .map(|row| (dv_to_string(&row[0]), dv_to_string(&row[1])))
            .collect())
    }

    /// Bulk-load all claim→entity edges in one query — used by the in-memory cache loader.
    /// Returns `(claim_id, entity_id)` pairs for every row in `claim_entity_edges`.
    pub fn get_all_claim_entity_edges(&self) -> Result<Vec<(String, String)>> {
        let result =
            self.query_read("?[claim_id, entity_id] := *claim_entity_edges{claim_id, entity_id}")?;
        Ok(result
            .rows
            .iter()
            .map(|row| (dv_to_string(&row[0]), dv_to_string(&row[1])))
            .collect())
    }

    /// Create a relationship between a claim and its source.
    pub fn link_claim_to_source(&self, claim_id: &str, source_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        params.insert("sid".into(), DataValue::Str(source_id.into()));

        self.query(
            r#"?[claim_id, source_id] <- [[$cid, $sid]]
            :put claim_source_edges {claim_id, source_id}"#,
            params,
        )?;
        Ok(())
    }

    /// Create a relationship between a claim and an entity.
    pub fn link_claim_to_entity(&self, claim_id: &str, entity_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        self.query(
            r#"?[claim_id, entity_id] <- [[$cid, $eid]]
            :put claim_entity_edges {claim_id, entity_id}"#,
            params,
        )?;
        Ok(())
    }

    /// Create a relationship between two entities.
    pub fn link_entities(
        &self,
        from_id: &str,
        to_id: &str,
        relation_type: &str,
        strength: f64,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("fid".into(), DataValue::Str(from_id.into()));
        params.insert("tid".into(), DataValue::Str(to_id.into()));
        params.insert("rtype".into(), DataValue::Str(relation_type.into()));
        params.insert("str".into(), DataValue::Num(Num::Float(strength)));

        self.query(
            r#"?[from_id, to_id, relation_type, strength] <- [[$fid, $tid, $rtype, $str]]
            :put entity_relations {from_id, to_id, relation_type => strength}"#,
            params,
        )?;
        Ok(())
    }

    /// Persist a relation edge scoped to the source that produced it.
    pub fn link_entities_for_source(
        &self,
        source_id: &str,
        from_id: &str,
        to_id: &str,
        relation_type: &str,
        strength: f64,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));
        params.insert("fid".into(), DataValue::Str(from_id.into()));
        params.insert("tid".into(), DataValue::Str(to_id.into()));
        params.insert("rtype".into(), DataValue::Str(relation_type.into()));
        params.insert("str".into(), DataValue::Num(Num::Float(strength)));

        self.query(
            r#"?[source_id, from_id, to_id, relation_type, strength] <- [[$sid, $fid, $tid, $rtype, $str]]
            :put source_entity_relations {source_id, from_id, to_id, relation_type => strength}"#,
            params,
        )?;
        Ok(())
    }

    /// Batch-insert source-scoped entity relations.
    pub fn link_entities_for_source_batch(
        &self,
        tuples: &[(String, String, String, String, f64)],
    ) -> Result<()> {
        const CHUNK: usize = 500;
        for chunk in tuples.chunks(CHUNK) {
            let rows: Vec<DataValue> = chunk
                .iter()
                .map(|(sid, fid, tid, rtype, strength)| {
                    DataValue::List(vec![
                        DataValue::Str(sid.clone().into()),
                        DataValue::Str(fid.clone().into()),
                        DataValue::Str(tid.clone().into()),
                        DataValue::Str(rtype.clone().into()),
                        DataValue::Num(Num::Float(*strength)),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(rows));
            self.query(
                "?[source_id, from_id, to_id, relation_type, strength] <- $rows \
                 :put source_entity_relations {source_id, from_id, to_id, relation_type => strength}",
                params,
            )?;
        }
        Ok(())
    }

    /// Rebuild the aggregated entity relation view from source-scoped relations.
    /// Uses noisy-OR aggregation: strength = 1 − ∏(1 − s_i).
    pub fn rebuild_entity_relations(&self) -> Result<()> {
        self.clear_entity_relations()?;

        // Fetch all (from, to, relation_type, strength) rows from source-scoped table.
        let result = self
            .db
            .run_script(
                "?[from_id, to_id, relation_type, strength] := *source_entity_relations{source_id, from_id, to_id, relation_type, strength}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        // Group by (from_id, to_id, relation_type) and compute noisy-OR.
        let mut grouped: std::collections::BTreeMap<(String, String, String), Vec<f64>> =
            std::collections::BTreeMap::new();
        for row in &result.rows {
            let from_id = dv_to_string(&row[0]);
            let to_id = dv_to_string(&row[1]);
            let relation_type = dv_to_string(&row[2]);
            let strength = match &row[3] {
                DataValue::Num(Num::Float(f)) => f.clamp(0.0, 1.0),
                DataValue::Num(Num::Int(i)) => (*i as f64).clamp(0.0, 1.0),
                _ => 0.0,
            };
            grouped
                .entry((from_id, to_id, relation_type))
                .or_default()
                .push(strength);
        }

        for ((from_id, to_id, relation_type), strengths) in &grouped {
            let complement_product = strengths
                .iter()
                .fold(1.0_f64, |acc, &s| acc * (1.0 - s.clamp(0.0, 1.0)));
            let noisy_or_strength = (1.0 - complement_product).clamp(0.0, 1.0);
            self.link_entities(from_id, to_id, relation_type, noisy_or_strength)?;
        }

        Ok(())
    }

    /// Get (from_id, to_id, relation_type) triples contributed by a specific source.
    /// Used to capture affected triples before source removal for incremental updates.
    pub fn get_source_relation_triples(
        &self,
        source_id: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));

        let result = self
            .db
            .run_script(
                "?[from_id, to_id, relation_type] := *source_entity_relations{source_id: $sid, from_id, to_id, relation_type}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                )
            })
            .collect())
    }

    /// Get all `(from_id, to_id, relation_type)` triples in `entity_relations`
    /// where at least one endpoint is in `entity_ids`.
    ///
    /// Used by the incremental pipeline to collect cross-file triples that need
    /// re-evaluation when a source's entities are removed or changed.
    /// Returns deduplicated triples.
    pub fn get_all_triples_involving_entities(
        &self,
        entity_ids: &[String],
    ) -> Result<Vec<(String, String, String)>> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut seen = std::collections::HashSet::new();

        for eid in entity_ids {
            let mut params = BTreeMap::new();
            params.insert("eid".into(), DataValue::Str(eid.clone().into()));

            // Triples where this entity is the source (from_id == eid).
            let from_result = self
                .db
                .run_script(
                    "?[f, t, rel_type] := \
                     *entity_relations{from_id: $eid, to_id: t, relation_type: rel_type}, \
                     f = $eid",
                    params.clone(),
                    ScriptMutability::Immutable,
                )
                .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

            // Triples where this entity is the target (to_id == eid).
            let to_result = self
                .db
                .run_script(
                    "?[f, t, rel_type] := \
                     *entity_relations{from_id: f, to_id: $eid, relation_type: rel_type}, \
                     t = $eid",
                    params,
                    ScriptMutability::Immutable,
                )
                .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

            for row in from_result.rows.iter().chain(to_result.rows.iter()) {
                seen.insert((
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                ));
            }
        }

        Ok(seen.into_iter().collect())
    }

    /// Incrementally update entity_relations for specific (from, to, rel_type) triples.
    /// Removes the stale aggregated edge, then re-aggregates from source_entity_relations.
    /// If no source still contributes a triple, the aggregated edge stays deleted.
    ///
    /// Note: the re-aggregation query scans source_entity_relations per triple because
    /// (from_id, to_id, relation_type) is not a key prefix (source_id leads the key).
    /// For graphs with many source-relation rows, callers should batch affected triples.
    ///
    /// If the same triple appears multiple times in `triples`, each occurrence is
    /// processed independently (idempotent result, redundant work). Callers that
    /// accumulate triples from multiple sources should deduplicate before calling.
    pub fn update_entity_relations_for_triples(
        &self,
        triples: &[(String, String, String)],
    ) -> Result<()> {
        for (from_id, to_id, relation_type) in triples {
            // Remove stale aggregated edge.
            let mut params = BTreeMap::new();
            params.insert("fid".into(), DataValue::Str(from_id.clone().into()));
            params.insert("tid".into(), DataValue::Str(to_id.clone().into()));
            params.insert("rtype".into(), DataValue::Str(relation_type.clone().into()));
            self.query(
                r#"?[from_id, to_id, relation_type] <- [[$fid, $tid, $rtype]]
                :rm entity_relations {from_id, to_id, relation_type}"#,
                params.clone(),
            )?;

            // Re-aggregate using noisy-OR: 1 − ∏(1 − s_i)
            // Include source_id in the projection so CozoDB does not deduplicate
            // rows that share the same strength value (e.g., three sources all at 0.5).
            let result = self
                .db
                .run_script(
                    "?[source_id, strength] := *source_entity_relations{source_id, from_id: $fid, to_id: $tid, relation_type: $rtype, strength}",
                    params,
                    ScriptMutability::Immutable,
                )
                .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

            if result.rows.is_empty() {
                // No sources remain — edge stays deleted.
                continue;
            }

            // Compute noisy-OR across all source strengths.
            let complement_product = result.rows.iter().fold(1.0_f64, |acc, row| {
                let s = match &row[1] {
                    DataValue::Num(Num::Float(f)) => f.clamp(0.0, 1.0),
                    DataValue::Num(Num::Int(i)) => (*i as f64).clamp(0.0, 1.0),
                    _ => 0.0,
                };
                acc * (1.0 - s)
            });
            let noisy_or_strength = (1.0 - complement_product).clamp(0.0, 1.0);

            self.link_entities(from_id, to_id, relation_type, noisy_or_strength)?;
        }
        Ok(())
    }

    /// Query all entities.
    pub fn get_all_entities(&self) -> Result<Vec<(String, String, String)>> {
        let result = self.query_read(
            "?[id, canonical_name, entity_type] := *entities{id, canonical_name, entity_type}",
        )?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                )
            })
            .collect())
    }

    /// Returns (canonical_name, entity_type) pairs for all entities.
    /// Used by graph-primed extraction to inject KNOWN_ENTITIES into LLM prompts.
    pub fn get_known_entities(&self) -> Result<Vec<(String, String)>> {
        let result = self
            .query_read("?[name, entity_type] := *entities{canonical_name: name, entity_type}")?;
        Ok(result
            .rows
            .into_iter()
            .filter_map(|row| {
                let name = row.first()?.get_str()?.to_string();
                let entity_type = row.get(1)?.get_str()?.to_string();
                Some((name, entity_type))
            })
            .collect())
    }

    /// Returns `(from_name, to_name, relation_type)` triples for all relations in the graph.
    /// Used by graph-primed extraction to inject KNOWN_RELATIONS into LLM prompts.
    pub fn get_known_relations(&self) -> Result<Vec<(String, String, String)>> {
        let result = self.query_read(
            r#"?[from_name, to_name, rel_type] :=
                *entity_relations{from_id, to_id, relation_type: rel_type},
                *entities{id: from_id, canonical_name: from_name},
                *entities{id: to_id, canonical_name: to_name}"#,
        )?;
        Ok(result
            .rows
            .into_iter()
            .filter_map(|row| {
                let from_name = row.first()?.get_str()?.to_string();
                let to_name = row.get(1)?.get_str()?.to_string();
                let rel_type = row.get(2)?.get_str()?.to_string();
                Some((from_name, to_name, rel_type))
            })
            .collect())
    }

    /// Remove all graph state derived from a source URI.
    pub fn remove_source_by_uri(&self, uri: &str) -> Result<usize> {
        let sources = self.find_sources_by_uri(uri)?;
        if sources.is_empty() {
            return Ok(0);
        }

        for (source_id, _, _) in &sources {
            self.remove_source_by_id(source_id)?;
        }

        Ok(sources.len())
    }

    /// Delete a claim and every downstream edge that names it: the
    /// `claim_source_edges`, `claim_entity_edges`, and `claim_temporal`
    /// rows, plus any `contradictions` referring to the claim.
    ///
    /// Used by the Rooting contribute-gate enforce mode to excise a
    /// Rejected-tier claim after the trial has recorded its verdict +
    /// certificate. The `trial_verdicts` row is deliberately retained for
    /// audit — enforce removes the admission, not the proof that it was
    /// ever considered.
    pub fn remove_claim_fully(&self, claim_id: &str) -> Result<()> {
        // Gather entity edges first (we need to query before deletion).
        let entity_edges = {
            let mut params = BTreeMap::new();
            params.insert("cid".into(), DataValue::Str(claim_id.into()));
            let r = self
                .db
                .run_script(
                    "?[entity_id] := *claim_entity_edges{claim_id: $cid, entity_id}",
                    params,
                    ScriptMutability::Immutable,
                )
                .map_err(|e| Error::GraphStorage(format!("entity edges: {e}")))?;
            r.rows
                .into_iter()
                .map(|row| dv_to_string(&row[0]))
                .collect::<Vec<_>>()
        };

        for eid in entity_edges {
            self.remove_claim_entity_edge(claim_id, &eid)?;
        }

        // Source edges are cleaned by a single pass.
        {
            let mut params = BTreeMap::new();
            params.insert("cid".into(), DataValue::Str(claim_id.into()));
            let r = self
                .db
                .run_script(
                    "?[source_id] := *claim_source_edges{claim_id: $cid, source_id}",
                    params,
                    ScriptMutability::Immutable,
                )
                .map_err(|e| Error::GraphStorage(format!("source edges: {e}")))?;
            for row in &r.rows {
                let sid = dv_to_string(&row[0]);
                let mut rm_params = BTreeMap::new();
                rm_params.insert("cid".into(), DataValue::Str(claim_id.into()));
                rm_params.insert("sid".into(), DataValue::Str(sid.into()));
                self.query(
                    r#"?[claim_id, source_id] <- [[$cid, $sid]]
                       :rm claim_source_edges {claim_id, source_id}"#,
                    rm_params,
                )?;
            }
        }

        self.remove_claim_temporal(claim_id)?;
        self.remove_contradictions_for_claim(claim_id)?;
        self.remove_claim(claim_id)?;
        Ok(())
    }

    /// Query all claims for a given entity (Datalog join).
    pub fn get_claims_for_entity(&self, entity_id: &str) -> Result<Vec<(String, String, String)>> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        let result = self
            .db
            .run_script(
                r#"?[id, statement, claim_type] :=
                    *claim_entity_edges{claim_id: id, entity_id: $eid},
                    *claims{id, statement, claim_type}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                )
            })
            .collect())
    }

    /// Insert a contradiction.
    pub fn insert_contradiction(
        &self,
        id: &str,
        claim_a: &str,
        claim_b: &str,
        explanation: &str,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.into()));
        params.insert("ca".into(), DataValue::Str(claim_a.into()));
        params.insert("cb".into(), DataValue::Str(claim_b.into()));
        params.insert("expl".into(), DataValue::Str(explanation.into()));
        params.insert(
            "ts".into(),
            DataValue::Num(Num::Float(chrono::Utc::now().timestamp() as f64)),
        );

        self.query(
            r#"?[id, claim_a, claim_b, explanation, status, detected_at] <- [[
                $id, $ca, $cb, $expl, 'Detected', $ts
            ]]
            :put contradictions {id => claim_a, claim_b, explanation, status, detected_at}"#,
            params,
        )?;
        Ok(())
    }

    // ─── Reflect (Phase 9): pattern discovery + gap tracking ────────────
    //
    // These helpers keep the cozo/Datalog surface inside this crate so
    // the `thinkingroot-reflect` phase crate stays cozo-free. Parameters
    // and return types are plain-tuple or primitive, following the same
    // convention as the Rooting helpers below.

    /// Discover co-occurrence of claim-type pairs across entities of the
    /// same type. One row per (entity_type, condition_claim_type,
    /// expected_claim_type) where both claim types appear on ≥1 entity of
    /// that type.
    ///
    /// Returned tuple: `(entity_type, condition_claim_type,
    /// expected_claim_type, condition_count, both_count)`.
    ///
    /// - `condition_count` = distinct entities of `entity_type` that carry
    ///   `condition_claim_type` at all.
    /// - `both_count` = distinct entities that carry both claim types.
    #[allow(clippy::type_complexity)]
    pub fn reflect_co_occurrences(&self) -> Result<Vec<(String, String, String, usize, usize)>> {
        let result = self
            .db
            .run_script(
                r#"entity_has[eid, etype, ctype] :=
                    *entities{id: eid, entity_type: etype},
                    *claim_entity_edges{entity_id: eid, claim_id: cid},
                    *claims{id: cid, claim_type: ctype}
                cond_count[etype, cta, count_unique(eid)] :=
                    entity_has[eid, etype, cta]
                both_count[etype, cta, ctb, count_unique(eid)] :=
                    entity_has[eid, etype, cta],
                    entity_has[eid, etype, ctb],
                    cta != ctb
                ?[etype, cta, ctb, cond_n, both_n] :=
                    cond_count[etype, cta, cond_n],
                    both_count[etype, cta, ctb, both_n]"#,
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("reflect_co_occurrences: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    count_from_single(&row[3]),
                    count_from_single(&row[4]),
                )
            })
            .collect())
    }

    /// Return entity ids of type `entity_type` that have a claim of
    /// `condition_claim_type` but none of `expected_claim_type`.
    pub fn reflect_entities_missing_expected(
        &self,
        entity_type: &str,
        condition_claim_type: &str,
        expected_claim_type: &str,
    ) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert(
            "etype".into(),
            DataValue::Str(entity_type.to_string().into()),
        );
        params.insert(
            "cta".into(),
            DataValue::Str(condition_claim_type.to_string().into()),
        );
        params.insert(
            "ctb".into(),
            DataValue::Str(expected_claim_type.to_string().into()),
        );

        let result = self
            .db
            .run_script(
                r#"has_condition[eid] :=
                    *entities{id: eid, entity_type: $etype},
                    *claim_entity_edges{entity_id: eid, claim_id: cid},
                    *claims{id: cid, claim_type: $cta}
                has_expected[eid] :=
                    *claim_entity_edges{entity_id: eid, claim_id: cid},
                    *claims{id: cid, claim_type: $ctb}
                ?[eid] :=
                    has_condition[eid],
                    not has_expected[eid]"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("reflect_entities_missing_expected: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| dv_to_string(&row[0]))
            .collect())
    }

    /// Find any claim id on `entity_id` whose `claim_type` matches.
    /// Returns the first match if present.
    pub fn find_claim_id_for_entity_by_type(
        &self,
        entity_id: &str,
        claim_type: &str,
    ) -> Result<Option<String>> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.to_string().into()));
        params.insert(
            "ctype".into(),
            DataValue::Str(claim_type.to_string().into()),
        );
        let result = self
            .db
            .run_script(
                r#"?[cid] :=
                    *claim_entity_edges{entity_id: $eid, claim_id: cid},
                    *claims{id: cid, claim_type: $ctype}
                :limit 1"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("find_claim_id_for_entity_by_type: {e}")))?;
        Ok(result.rows.first().map(|r| dv_to_string(&r[0])))
    }

    /// Replace patterns whose `source_scope` matches the given scope,
    /// leaving patterns at other scopes intact. Use `"local"` for the
    /// single-workspace reflect cycle and `"cross:<id>"` for
    /// cross-workspace aggregates — they coexist in the same table.
    ///
    /// Row tuple order: `(id, entity_type, condition_claim_type,
    /// expected_claim_type, frequency, sample_size, last_computed,
    /// min_sample_threshold, first_seen_at, stability_runs, source_scope)`.
    ///
    /// Every row's `source_scope` must equal the `scope` parameter;
    /// mismatches are a programming error and the function rejects
    /// them up front.
    #[allow(clippy::type_complexity)]
    pub fn reflect_rewrite_patterns_for_scope(
        &self,
        scope: &str,
        rows: &[(
            String,
            String,
            String,
            String,
            f64,
            usize,
            f64,
            usize,
            f64,
            u32,
            String,
        )],
    ) -> Result<()> {
        for r in rows {
            if r.10 != scope {
                return Err(Error::GraphStorage(format!(
                    "reflect_rewrite_patterns_for_scope: row scope '{}' does not match requested scope '{}'",
                    r.10, scope
                )));
            }
        }
        let mut params = BTreeMap::new();
        params.insert("scope".into(), DataValue::Str(scope.to_string().into()));
        // Row-level delete via subquery. `::remove` would be simpler but
        // cozo rejects it while any `::index` is attached — and we have
        // `structural_patterns:by_entity_type` to keep. We scope the
        // delete by `source_scope` so local and cross patterns don't
        // trample each other.
        self.db
            .run_script(
                r#"?[id] := *structural_patterns{id, source_scope: $scope}
                :rm structural_patterns {id}"#,
                params,
                ScriptMutability::Mutable,
            )
            .map_err(|e| Error::GraphStorage(format!("truncate structural_patterns: {e}")))?;

        if rows.is_empty() {
            return Ok(());
        }

        let data_rows: Vec<DataValue> = rows
            .iter()
            .map(|r| {
                DataValue::List(vec![
                    DataValue::Str(r.0.clone().into()),
                    DataValue::Str(r.1.clone().into()),
                    DataValue::Str(r.2.clone().into()),
                    DataValue::Str(r.3.clone().into()),
                    DataValue::Num(Num::Float(r.4)),
                    DataValue::Num(Num::Int(r.5 as i64)),
                    DataValue::Num(Num::Float(r.6)),
                    DataValue::Num(Num::Int(r.7 as i64)),
                    DataValue::Num(Num::Float(r.8)),
                    DataValue::Num(Num::Int(r.9 as i64)),
                    DataValue::Str(r.10.clone().into()),
                ])
            })
            .collect();
        let mut params = BTreeMap::new();
        params.insert("rows".into(), DataValue::List(data_rows));
        self.query(
            r#"?[id, entity_type, condition_claim_type, expected_claim_type,
                 frequency, sample_size, last_computed, min_sample_threshold,
                 first_seen_at, stability_runs, source_scope] <- $rows
            :put structural_patterns {
                id =>
                entity_type, condition_claim_type, expected_claim_type,
                frequency, sample_size, last_computed, min_sample_threshold,
                first_seen_at, stability_runs, source_scope
            }"#,
            params,
        )?;
        Ok(())
    }

    /// Load every row of `structural_patterns`.
    ///
    /// Returned tuple: `(id, entity_type, condition_claim_type,
    /// expected_claim_type, frequency, sample_size, last_computed,
    /// min_sample_threshold, first_seen_at, stability_runs, source_scope)`.
    #[allow(clippy::type_complexity)]
    pub fn reflect_load_structural_patterns(
        &self,
    ) -> Result<
        Vec<(
            String,
            String,
            String,
            String,
            f64,
            usize,
            f64,
            usize,
            f64,
            u32,
            String,
        )>,
    > {
        let result = self
            .db
            .run_script(
                r#"?[id, etype, cond, expected, freq, sample, last_computed, threshold,
                     first_seen_at, stability_runs, source_scope] :=
                    *structural_patterns{id, entity_type: etype,
                                         condition_claim_type: cond,
                                         expected_claim_type: expected,
                                         frequency: freq, sample_size: sample,
                                         last_computed, min_sample_threshold: threshold,
                                         first_seen_at, stability_runs, source_scope}"#,
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("reflect_load_structural_patterns: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    dv_to_string(&row[3]),
                    dv_to_float(&row[4]),
                    count_from_single(&row[5]),
                    dv_to_float(&row[6]),
                    count_from_single(&row[7]),
                    dv_to_float(&row[8]),
                    count_from_single(&row[9]) as u32,
                    dv_to_string(&row[10]),
                )
            })
            .collect())
    }

    /// Load every row of `known_unknowns` (regardless of status).
    ///
    /// Returned tuple: `(id, entity_id, pattern_id, expected_claim_type,
    /// confidence, status, created_at, resolved_at, resolved_by)`.
    #[allow(clippy::type_complexity)]
    pub fn reflect_load_known_unknowns(
        &self,
    ) -> Result<
        Vec<(
            String,
            String,
            String,
            String,
            f64,
            String,
            f64,
            f64,
            String,
        )>,
    > {
        let result = self
            .db
            .run_script(
                r#"?[id, eid, pid, expected, conf, status, created, resolved, resolved_by] :=
                    *known_unknowns{id, entity_id: eid, pattern_id: pid,
                                    expected_claim_type: expected, confidence: conf, status,
                                    created_at: created, resolved_at: resolved,
                                    resolved_by}"#,
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("reflect_load_known_unknowns: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    dv_to_string(&row[3]),
                    dv_to_float(&row[4]),
                    dv_to_string(&row[5]),
                    dv_to_float(&row[6]),
                    dv_to_float(&row[7]),
                    dv_to_string(&row[8]),
                )
            })
            .collect())
    }

    /// Upsert one row of `known_unknowns`.
    #[allow(clippy::too_many_arguments)]
    pub fn reflect_upsert_known_unknown(
        &self,
        id: &str,
        entity_id: &str,
        pattern_id: &str,
        expected_claim_type: &str,
        confidence: f64,
        status: &str,
        created_at: f64,
        resolved_at: f64,
        resolved_by: &str,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.to_string().into()));
        params.insert("eid".into(), DataValue::Str(entity_id.to_string().into()));
        params.insert("pid".into(), DataValue::Str(pattern_id.to_string().into()));
        params.insert(
            "expected".into(),
            DataValue::Str(expected_claim_type.to_string().into()),
        );
        params.insert("conf".into(), DataValue::Num(Num::Float(confidence)));
        params.insert("status".into(), DataValue::Str(status.to_string().into()));
        params.insert("created".into(), DataValue::Num(Num::Float(created_at)));
        params.insert("resolved".into(), DataValue::Num(Num::Float(resolved_at)));
        params.insert(
            "resolved_by".into(),
            DataValue::Str(resolved_by.to_string().into()),
        );
        self.query(
            r#"?[id, entity_id, pattern_id, expected_claim_type, confidence,
                 status, created_at, resolved_at, resolved_by] <-
                [[$id, $eid, $pid, $expected, $conf, $status, $created, $resolved, $resolved_by]]
            :put known_unknowns {
                id =>
                entity_id, pattern_id, expected_claim_type, confidence,
                status, created_at, resolved_at, resolved_by
            }"#,
            params,
        )?;
        Ok(())
    }

    /// Count open gap records (status = 'open').
    pub fn reflect_count_open_known_unknowns(&self) -> Result<usize> {
        let result = self
            .db
            .run_script(
                "?[count(gid)] := *known_unknowns{id: gid, status: 'open'}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("reflect_count_open_known_unknowns: {e}")))?;
        Ok(result
            .rows
            .first()
            .map(|r| count_from_single(&r[0]))
            .unwrap_or(0))
    }

    /// List open gaps joined with entity + pattern detail, filtered by
    /// `min_confidence` and optionally scoped to an entity canonical name.
    ///
    /// Returned tuple: `(gap_id, entity_id, entity_name, entity_type,
    /// expected_claim_type, confidence, pattern_id, sample_size,
    /// created_at)`.
    #[allow(clippy::type_complexity)]
    pub fn reflect_list_open_gap_rows(
        &self,
        entity_name: Option<&str>,
        min_confidence: f64,
    ) -> Result<
        Vec<(
            String,
            String,
            String,
            String,
            String,
            f64,
            String,
            usize,
            f64,
        )>,
    > {
        let mut params = BTreeMap::new();
        params.insert(
            "min_conf".into(),
            DataValue::Num(Num::Float(min_confidence)),
        );
        let script = if let Some(name) = entity_name {
            params.insert("name".into(), DataValue::Str(name.to_string().into()));
            r#"?[gid, eid, ename, etype, expected, confidence, pid, sample, created] :=
                *known_unknowns{id: gid, entity_id: eid, pattern_id: pid,
                                expected_claim_type: expected, confidence, status: 'open',
                                created_at: created},
                *entities{id: eid, canonical_name: ename, entity_type: etype},
                *structural_patterns{id: pid, sample_size: sample},
                confidence >= $min_conf,
                ename == $name"#
        } else {
            r#"?[gid, eid, ename, etype, expected, confidence, pid, sample, created] :=
                *known_unknowns{id: gid, entity_id: eid, pattern_id: pid,
                                expected_claim_type: expected, confidence, status: 'open',
                                created_at: created},
                *entities{id: eid, canonical_name: ename, entity_type: etype},
                *structural_patterns{id: pid, sample_size: sample},
                confidence >= $min_conf"#
        };

        let result = self
            .db
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| Error::GraphStorage(format!("reflect_list_open_gap_rows: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    dv_to_string(&row[3]),
                    dv_to_string(&row[4]),
                    dv_to_float(&row[5]),
                    dv_to_string(&row[6]),
                    count_from_single(&row[7]),
                    dv_to_float(&row[8]),
                )
            })
            .collect())
    }

    // ─── Rooting: trial_verdicts / verification_certificates / derivation_edges ──

    /// Batch-insert Rooting trial verdicts. Parameters are passed as primitive
    /// tuples so this crate does not need to depend on `thinkingroot-rooting`.
    ///
    /// Row tuple order:
    /// `(id, claim_id, trial_at, admission_tier, provenance_score,
    ///   contradiction_score, predicate_score, topology_score, temporal_score,
    ///   certificate_hash, failure_reason, rooter_version)`
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub fn insert_trial_verdicts_batch(
        &self,
        rows: &[(
            String,
            String,
            f64,
            String,
            f64,
            f64,
            f64,
            f64,
            f64,
            String,
            String,
            String,
        )],
    ) -> Result<()> {
        const CHUNK: usize = 500;
        for chunk in rows.chunks(CHUNK) {
            let data_rows: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        DataValue::Str(r.0.clone().into()),
                        DataValue::Str(r.1.clone().into()),
                        DataValue::Num(Num::Float(r.2)),
                        DataValue::Str(r.3.clone().into()),
                        DataValue::Num(Num::Float(r.4)),
                        DataValue::Num(Num::Float(r.5)),
                        DataValue::Num(Num::Float(r.6)),
                        DataValue::Num(Num::Float(r.7)),
                        DataValue::Num(Num::Float(r.8)),
                        DataValue::Str(r.9.clone().into()),
                        DataValue::Str(r.10.clone().into()),
                        DataValue::Str(r.11.clone().into()),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(data_rows));
            self.query(
                "?[id, claim_id, trial_at, admission_tier, provenance_score, \
                  contradiction_score, predicate_score, topology_score, \
                  temporal_score, certificate_hash, failure_reason, rooter_version] \
                  <- $rows \
                  :put trial_verdicts {id => claim_id, trial_at, admission_tier, \
                  provenance_score, contradiction_score, predicate_score, \
                  topology_score, temporal_score, certificate_hash, \
                  failure_reason, rooter_version}",
                params,
            )?;
        }
        Ok(())
    }

    /// Batch-insert Rooting verification certificates. Idempotent — identical
    /// `hash` values will upsert the same row.
    ///
    /// Row tuple order:
    /// `(hash, claim_id, created_at, probe_inputs_json, probe_outputs_json,
    ///   rooter_version, source_content_hash)`
    #[allow(clippy::type_complexity)]
    pub fn insert_certificates_batch(
        &self,
        rows: &[(String, String, f64, String, String, String, String)],
    ) -> Result<()> {
        const CHUNK: usize = 500;
        for chunk in rows.chunks(CHUNK) {
            let data_rows: Vec<DataValue> = chunk
                .iter()
                .map(|r| {
                    DataValue::List(vec![
                        DataValue::Str(r.0.clone().into()),
                        DataValue::Str(r.1.clone().into()),
                        DataValue::Num(Num::Float(r.2)),
                        DataValue::Str(r.3.clone().into()),
                        DataValue::Str(r.4.clone().into()),
                        DataValue::Str(r.5.clone().into()),
                        DataValue::Str(r.6.clone().into()),
                    ])
                })
                .collect();
            let mut params = BTreeMap::new();
            params.insert("rows".into(), DataValue::List(data_rows));
            self.query(
                "?[hash, claim_id, created_at, probe_inputs_json, probe_outputs_json, \
                  rooter_version, source_content_hash] <- $rows \
                  :put verification_certificates {hash => claim_id, created_at, \
                  probe_inputs_json, probe_outputs_json, rooter_version, \
                  source_content_hash}",
                params,
            )?;
        }
        Ok(())
    }

    /// Get all trial verdicts for a specific claim, ordered by trial time descending.
    #[allow(clippy::type_complexity)]
    pub fn get_trial_verdicts_for_claim(
        &self,
        claim_id: &str,
    ) -> Result<
        Vec<(
            String,
            f64,
            String,
            f64,
            f64,
            f64,
            f64,
            f64,
            String,
            String,
            String,
        )>,
    > {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        let result = self
            .db
            .run_script(
                "?[id, trial_at, admission_tier, provenance_score, contradiction_score, \
                  predicate_score, topology_score, temporal_score, certificate_hash, \
                  failure_reason, rooter_version] := \
                  *trial_verdicts{id, claim_id: $cid, trial_at, admission_tier, \
                  provenance_score, contradiction_score, predicate_score, \
                  topology_score, temporal_score, certificate_hash, failure_reason, \
                  rooter_version}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_trial_verdicts_for_claim: {e}")))?;
        let mut out = Vec::with_capacity(result.rows.len());
        for row in &result.rows {
            if row.len() < 11 {
                continue;
            }
            out.push((
                dv_to_string(&row[0]),
                dv_to_float(&row[1]),
                dv_to_string(&row[2]),
                dv_to_float(&row[3]),
                dv_to_float(&row[4]),
                dv_to_float(&row[5]),
                dv_to_float(&row[6]),
                dv_to_float(&row[7]),
                dv_to_string(&row[8]),
                dv_to_string(&row[9]),
                dv_to_string(&row[10]),
            ));
        }
        // Most-recent first — trial_at descending.
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }

    /// Look up a verification certificate by its BLAKE3 hash.
    #[allow(clippy::type_complexity)]
    pub fn get_certificate_by_hash(
        &self,
        hash: &str,
    ) -> Result<Option<(String, f64, String, String, String, String)>> {
        let mut params = BTreeMap::new();
        params.insert("h".into(), DataValue::Str(hash.into()));
        let result = self
            .db
            .run_script(
                "?[claim_id, created_at, probe_inputs_json, probe_outputs_json, \
                  rooter_version, source_content_hash] := \
                  *verification_certificates{hash: $h, claim_id, created_at, \
                  probe_inputs_json, probe_outputs_json, rooter_version, \
                  source_content_hash}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_certificate_by_hash: {e}")))?;
        if let Some(row) = result.rows.first() {
            Ok(Some((
                dv_to_string(&row[0]),
                dv_to_float(&row[1]),
                dv_to_string(&row[2]),
                dv_to_string(&row[3]),
                dv_to_string(&row[4]),
                dv_to_string(&row[5]),
            )))
        } else {
            Ok(None)
        }
    }

    /// Insert a derivation edge linking a parent claim to a derived child claim.
    pub fn insert_derivation_edge(
        &self,
        parent_claim_id: &str,
        child_claim_id: &str,
        derivation_rule: &str,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("p".into(), DataValue::Str(parent_claim_id.into()));
        params.insert("c".into(), DataValue::Str(child_claim_id.into()));
        params.insert("r".into(), DataValue::Str(derivation_rule.into()));
        self.query(
            "?[parent_claim_id, child_claim_id, derivation_rule] <- [[$p, $c, $r]] \
             :put derivation_edges {parent_claim_id, child_claim_id => derivation_rule}",
            params,
        )?;
        Ok(())
    }

    // ─── End Rooting helpers ─────────────────────────────────────────────

    /// Get all contradictions.
    #[allow(clippy::type_complexity)]
    pub fn get_contradictions(&self) -> Result<Vec<(String, String, String, String, String)>> {
        let result = self.query_read(
            "?[id, claim_a, claim_b, explanation, status] := *contradictions{id, claim_a, claim_b, explanation, status}",
        )?;
        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    dv_to_string(&row[3]),
                    dv_to_string(&row[4]),
                )
            })
            .collect())
    }

    /// Get claims for a specific entity with their source URIs (Datalog 3-way join).
    #[allow(clippy::type_complexity)]
    pub fn get_claims_with_sources_for_entity(
        &self,
        entity_id: &str,
    ) -> Result<Vec<(String, String, String, String, f64)>> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        let result = self
            .db
            .run_script(
                r#"?[id, statement, claim_type, uri, confidence] :=
                    *claim_entity_edges{claim_id: id, entity_id: $eid},
                    *claims{id, statement, claim_type, source_id, confidence},
                    *sources{id: source_id, uri}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    dv_to_string(&row[3]),
                    match &row[4] {
                        DataValue::Num(Num::Float(f)) => *f,
                        DataValue::Num(Num::Int(i)) => *i as f64,
                        _ => 0.8,
                    },
                )
            })
            .collect())
    }

    /// Get all entity relations (for architecture map).
    #[allow(clippy::type_complexity)]
    pub fn get_all_relations(&self) -> Result<Vec<(String, String, String, String, String, f64)>> {
        let result = self.query_read(
            r#"?[from_name, to_name, rel_type, from_type, to_type, strength] :=
                *entity_relations{from_id, to_id, relation_type: rel_type, strength},
                *entities{id: from_id, canonical_name: from_name, entity_type: from_type},
                *entities{id: to_id, canonical_name: to_name, entity_type: to_type}"#,
        )?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    dv_to_string(&row[3]),
                    dv_to_string(&row[4]),
                    match &row[5] {
                        DataValue::Num(Num::Float(f)) => *f,
                        DataValue::Num(Num::Int(i)) => *i as f64,
                        _ => 1.0,
                    },
                )
            })
            .collect())
    }

    /// Count stale claims (created_at older than cutoff_timestamp).
    pub fn count_stale_claims(&self, cutoff_timestamp: f64) -> Result<usize> {
        let mut params = BTreeMap::new();
        params.insert(
            "cutoff".into(),
            DataValue::Num(Num::Float(cutoff_timestamp)),
        );

        let result = self
            .db
            .run_script(
                "?[count(id)] := *claims{id, created_at}, created_at < $cutoff",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        if let Some(row) = result.rows.first() {
            match &row[0] {
                DataValue::Num(Num::Int(n)) => Ok(*n as usize),
                DataValue::Num(Num::Float(n)) => Ok(*n as usize),
                _ => Ok(0),
            }
        } else {
            Ok(0)
        }
    }

    /// Count claims with grounding_score below a threshold.
    /// Ignores ungrounded claims (score = -1.0).
    ///
    /// Pure read — uses `ScriptMutability::Immutable` so the health
    /// scorer (which calls this) doesn't acquire CozoDB's write lock
    /// and serialise concurrent Brain reads behind it.  Pre-fix the
    /// helper went through `self.query` which is `Mutable`, inflating
    /// `brain_load` latency every time `root health` ran.
    pub fn count_low_grounding_claims(&self, threshold: f64) -> Result<usize> {
        let mut params = BTreeMap::new();
        params.insert("threshold".into(), DataValue::Num(Num::Float(threshold)));
        let result = self
            .db
            .run_script(
                "?[count(id)] := *claims{id, grounding_score: gs}, gs >= 0.0, gs < $threshold",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("count_low_grounding_claims: {e}")))?;
        Ok(count_from_rows(&result.rows))
    }

    /// List claims with `admission_tier = 'rooted'`, optionally filtered by
    /// claim type, entity name, and/or minimum confidence. Returns tuples of
    /// `(id, statement, claim_type, confidence, source_uri, event_date)`.
    /// Used by the `query_rooted` MCP tool.
    #[allow(clippy::type_complexity)]
    pub fn get_rooted_claims_filtered(
        &self,
        type_filter: Option<&str>,
        entity_filter: Option<&str>,
        min_confidence: Option<f64>,
    ) -> Result<Vec<(String, String, String, f64, String, f64)>> {
        // Base query: Rooted claims joined with their source URIs.
        // Entity filter joins through claim_entity_edges + entities.canonical_name.
        let (script, params) = if let Some(ename) = entity_filter {
            let mut p = BTreeMap::new();
            p.insert("ename".into(), DataValue::Str(ename.into()));
            (
                "?[id, statement, claim_type, confidence, source_uri, event_date] := \
                  *claims{id, statement, claim_type, source_id, confidence, event_date, admission_tier}, \
                  admission_tier = 'rooted', \
                  *sources{id: source_id, uri: source_uri}, \
                  *claim_entity_edges{claim_id: id, entity_id}, \
                  *entities{id: entity_id, canonical_name: $ename}",
                p,
            )
        } else {
            (
                "?[id, statement, claim_type, confidence, source_uri, event_date] := \
                  *claims{id, statement, claim_type, source_id, confidence, event_date, admission_tier}, \
                  admission_tier = 'rooted', \
                  *sources{id: source_id, uri: source_uri}",
                BTreeMap::new(),
            )
        };

        let result = self
            .db
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| Error::GraphStorage(format!("get_rooted_claims_filtered: {e}")))?;

        let mut out: Vec<(String, String, String, f64, String, f64)> = Vec::new();
        for row in &result.rows {
            if row.len() < 6 {
                continue;
            }
            let claim_type = dv_to_string(&row[2]);
            if let Some(t) = type_filter
                && !claim_type.eq_ignore_ascii_case(t)
            {
                continue;
            }
            let confidence = dv_to_float(&row[3]);
            if let Some(min) = min_confidence
                && confidence < min
            {
                continue;
            }
            out.push((
                dv_to_string(&row[0]),
                dv_to_string(&row[1]),
                claim_type,
                confidence,
                dv_to_string(&row[4]),
                dv_to_float(&row[5]),
            ));
        }
        Ok(out)
    }

    /// Return every claim ID in the workspace. Used by `root rooting re-run --all`
    /// to drive re-execution over the full graph.
    pub fn get_all_claim_ids(&self) -> Result<Vec<String>> {
        let result = self.query_read("?[id] := *claims{id}")?;
        Ok(result
            .rows
            .iter()
            .map(|row| dv_to_string(&row[0]))
            .collect())
    }

    /// Return claim IDs filtered by admission tier
    /// (`"rooted"`, `"attested"`, `"quarantined"`, or `"rejected"`). Used by
    /// the Rooting ablation harness to gate retrieval-time exclusion.
    pub fn get_claim_ids_by_admission_tier(&self, tier: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("t".into(), DataValue::Str(tier.into()));
        let result = self
            .db
            .run_script(
                "?[id] := *claims{id, admission_tier}, admission_tier = $t",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_claim_ids_by_admission_tier: {e}")))?;
        Ok(result
            .rows
            .iter()
            .map(|row| dv_to_string(&row[0]))
            .collect())
    }

    /// Count claims grouped by their Rooting admission tier.
    /// Returns `(rooted, attested, quarantined, rejected)`. Used by the
    /// Health Score calculation and by `root rooting report`.
    pub fn count_claims_by_admission_tier(&self) -> Result<(usize, usize, usize, usize)> {
        let result = self.query_read("?[tier, count(id)] := *claims{id, admission_tier: tier}")?;
        let mut rooted = 0usize;
        let mut attested = 0usize;
        let mut quarantined = 0usize;
        let mut rejected = 0usize;
        for row in &result.rows {
            if row.len() < 2 {
                continue;
            }
            let tier = dv_to_string(&row[0]);
            let count = match &row[1] {
                DataValue::Num(Num::Int(n)) => *n as usize,
                DataValue::Num(Num::Float(f)) => *f as usize,
                _ => 0,
            };
            match tier.as_str() {
                "rooted" => rooted = count,
                "quarantined" => quarantined = count,
                "rejected" => rejected = count,
                _ => attested = count,
            }
        }
        Ok((rooted, attested, quarantined, rejected))
    }

    /// Check if a source with this content_hash already exists.
    pub fn source_hash_exists(&self, content_hash: &str) -> Result<bool> {
        let mut params = BTreeMap::new();
        params.insert("hash".into(), DataValue::Str(content_hash.into()));

        let result = self
            .db
            .run_script(
                "?[count(id)] := *sources{id, content_hash}, content_hash == $hash",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        if let Some(row) = result.rows.first() {
            match &row[0] {
                DataValue::Num(Num::Int(n)) => Ok(*n > 0),
                DataValue::Num(Num::Float(n)) => Ok(*n > 0.0),
                _ => Ok(false),
            }
        } else {
            Ok(false)
        }
    }

    /// Get all claims of a specific type (e.g., "Decision", "Requirement").
    #[allow(clippy::type_complexity)]
    pub fn get_claims_by_type(
        &self,
        claim_type: &str,
    ) -> Result<Vec<(String, String, String, f64, String)>> {
        let mut params = BTreeMap::new();
        params.insert("ctype".into(), DataValue::Str(claim_type.into()));

        let result = self
            .db
            .run_script(
                r#"?[id, statement, source_id, confidence, uri] :=
                    *claims{id, statement, claim_type: $ctype, source_id, confidence},
                    *claim_source_edges{claim_id: id, source_id: sid},
                    *sources{id: sid, uri}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    match &row[3] {
                        DataValue::Num(Num::Float(f)) => *f,
                        DataValue::Num(Num::Int(i)) => *i as f64,
                        _ => 0.8,
                    },
                    dv_to_string(&row[4]),
                )
            })
            .collect())
    }

    /// Get all claims with their source URIs (for bulk artifact generation).
    #[allow(clippy::type_complexity)]
    pub fn get_all_claims_with_sources(
        &self,
    ) -> Result<Vec<(String, String, String, f64, String, f64)>> {
        let result = self.query_read(
            r#"?[id, statement, claim_type, confidence, uri, event_date] :=
                *claims{id, statement, claim_type, confidence, event_date},
                *claim_source_edges{claim_id: id, source_id: sid},
                *sources{id: sid, uri}"#,
        )?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    match &row[3] {
                        DataValue::Num(Num::Float(f)) => *f,
                        DataValue::Num(Num::Int(i)) => *i as f64,
                        _ => 0.8,
                    },
                    dv_to_string(&row[4]),
                    match &row[5] {
                        DataValue::Num(Num::Float(f)) => *f,
                        DataValue::Num(Num::Int(i)) => *i as f64,
                        _ => 0.0,
                    },
                )
            })
            .collect())
    }

    /// Get relations for a specific entity (by name).
    pub fn get_relations_for_entity(
        &self,
        entity_name: &str,
    ) -> Result<Vec<(String, String, f64)>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(entity_name.into()));

        let result = self
            .db
            .run_script(
                r#"?[to_name, rel_type, strength] :=
                    *entities{id: from_id, canonical_name: $name},
                    *entity_relations{from_id, to_id, relation_type: rel_type, strength},
                    *entities{id: to_id, canonical_name: to_name}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    match &row[2] {
                        DataValue::Num(Num::Float(f)) => *f,
                        DataValue::Num(Num::Int(i)) => *i as f64,
                        _ => 1.0,
                    },
                )
            })
            .collect())
    }

    /// Get all source URIs.
    /// Return `(claim_id, source_id)` pairs for all claims that have a `source_id`
    /// field in the claims table.  Used by the diff algorithm to carry real SourceIds
    /// into `DiffClaim` objects rather than synthetic placeholder IDs.
    pub fn get_claim_source_id_map(&self) -> Result<std::collections::HashMap<String, String>> {
        let result = self.query_read("?[id, source_id] := *claims{id, source_id}")?;
        Ok(result
            .rows
            .iter()
            .map(|row| (dv_to_string(&row[0]), dv_to_string(&row[1])))
            .collect())
    }

    /// Return every claim joined with its source row — the input shape
    /// the v3 pack writer needs. See [`V3ClaimExportRow`] for field-by-
    /// field semantics. Empty `content_hash` means the source
    /// has no byte-level body (e.g. synthetic agent contributions);
    /// the caller decides whether to skip those claims when building a
    /// v3 pack.
    pub fn get_v3_claim_export(&self) -> Result<Vec<V3ClaimExportRow>> {
        let q = r#"?[id, statement, claim_type, confidence, admission_tier, byte_start, byte_end, source_id, source_uri, content_hash] :=
            *claims{id, statement, claim_type, confidence, admission_tier, byte_start, byte_end, source_id},
            *sources{id: source_id, uri: source_uri, content_hash}
        "#;
        let result = self.query_read(q)?;
        Ok(result
            .rows
            .iter()
            .map(|row| V3ClaimExportRow {
                id: dv_to_string(&row[0]),
                statement: dv_to_string(&row[1]),
                claim_type: dv_to_string(&row[2]),
                confidence: match &row[3] {
                    DataValue::Num(Num::Float(f)) => *f,
                    DataValue::Num(Num::Int(i)) => *i as f64,
                    _ => 0.8,
                },
                admission_tier: dv_to_string(&row[4]),
                byte_start: match &row[5] {
                    DataValue::Num(Num::Int(i)) => (*i).max(0) as u64,
                    _ => 0,
                },
                byte_end: match &row[6] {
                    DataValue::Num(Num::Int(i)) => (*i).max(0) as u64,
                    _ => 0,
                },
                source_id: dv_to_string(&row[7]),
                source_uri: dv_to_string(&row[8]),
                content_hash: dv_to_string(&row[9]),
            })
            .collect())
    }

    /// Return a `claim_id → [entity_name]` map. Used alongside
    /// [`Self::get_v3_claim_export`] by the v3 pack writer to populate
    /// the `ents` field on each emitted `ClaimRecord`.
    pub fn get_claim_entity_names(&self) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let q = r#"?[claim_id, entity_name] :=
            *claim_entity_edges{claim_id, entity_id},
            *entities{id: entity_id, canonical_name: entity_name}
        "#;
        let result = self.query_read(q)?;
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for row in &result.rows {
            map.entry(dv_to_string(&row[0]))
                .or_default()
                .push(dv_to_string(&row[1]));
        }
        Ok(map)
    }

    pub fn get_all_sources(&self) -> Result<Vec<(String, String, String)>> {
        let result =
            self.query_read("?[id, uri, source_type] := *sources{id, uri, source_type}")?;
        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                )
            })
            .collect())
    }

    /// Return all sources as `(uri, content_hash)` pairs.
    ///
    /// Used by `root status` to compare stored hashes against current on-disk
    /// file contents, identifying modified, untracked, and deleted sources
    /// without running a full compile.
    pub fn get_sources_with_hashes(&self) -> Result<Vec<(String, String)>> {
        let result = self.query_read("?[uri, content_hash] := *sources{uri, content_hash}")?;
        Ok(result
            .rows
            .iter()
            .map(|row| (dv_to_string(&row[0]), dv_to_string(&row[1])))
            .collect())
    }

    /// Look up a source by its ID and return a reconstructed `Source` struct.
    /// Returns `None` if no source with that ID exists.
    pub fn get_source_by_id(&self, id: &str) -> Result<Option<thinkingroot_core::Source>> {
        use thinkingroot_core::types::{ContentHash, SourceId, SourceType, TrustLevel};

        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.into()));

        let result = self.db.run_script(
            "?[uri, source_type, author, content_hash, trust_level, byte_size] := *sources{id: $id, uri, source_type, author, content_hash, trust_level, byte_size}",
            params,
            ScriptMutability::Immutable,
        ).map_err(|e| Error::GraphStorage(format!("get_source_by_id query failed: {e}")))?;

        let row = match result.rows.first() {
            Some(r) => r,
            None => return Ok(None),
        };

        let uri = dv_to_string(&row[0]);
        let source_type_str = dv_to_string(&row[1]);
        let author_str = dv_to_string(&row[2]);
        let content_hash = ContentHash(dv_to_string(&row[3]));
        let trust_level_str = dv_to_string(&row[4]);
        let byte_size = match &row[5] {
            DataValue::Num(Num::Int(n)) => *n as u64,
            DataValue::Num(Num::Float(n)) => *n as u64,
            _ => 0u64,
        };

        let source_type = match source_type_str.as_str() {
            "GitCommit" => SourceType::GitCommit,
            "GitDiff" => SourceType::GitDiff,
            "Document" => SourceType::Document,
            "ChatMessage" => SourceType::ChatMessage,
            "WebPage" => SourceType::WebPage,
            "Api" => SourceType::Api,
            "Manual" => SourceType::Manual,
            _ => SourceType::File,
        };

        let trust_level = match trust_level_str.as_str() {
            "Quarantined" => TrustLevel::Quarantined,
            "Untrusted" => TrustLevel::Untrusted,
            "Trusted" => TrustLevel::Trusted,
            "Verified" => TrustLevel::Verified,
            _ => TrustLevel::Unknown,
        };

        let source_id: SourceId = id.parse().unwrap_or_else(|_| SourceId::new());
        let mut source = thinkingroot_core::Source::new(uri, source_type)
            .with_id(source_id)
            .with_hash(content_hash)
            .with_size(byte_size)
            .with_trust(trust_level);
        if !author_str.is_empty() {
            source.author = Some(author_str);
        }
        Ok(Some(source))
    }

    /// Look up a single claim by ID and return a reconstructed `Claim` struct.
    /// Joins `claims` with `claim_temporal` for full temporal metadata.
    /// Returns `None` if no claim with that ID exists.
    pub fn get_claim_by_id(&self, id: &str) -> Result<Option<thinkingroot_core::Claim>> {
        use thinkingroot_core::types::{ClaimType, Confidence, PipelineVersion, Sensitivity};
        use thinkingroot_core::{Claim, ClaimId, SourceId, WorkspaceId};

        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.into()));

        let result = self.db.run_script(
            r#"?[statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date, admission_tier, derivation_parents, predicate_json, last_rooted_at] :=
                *claims{id: $id, statement, claim_type, source_id, confidence, sensitivity, workspace_id, created_at, grounding_score, grounding_method, extraction_tier, event_date, admission_tier, derivation_parents, predicate_json, last_rooted_at}"#,
            params,
            ScriptMutability::Immutable,
        ).map_err(|e| Error::GraphStorage(format!("get_claim_by_id query failed: {e}")))?;

        let row = match result.rows.first() {
            Some(r) => r,
            None => return Ok(None),
        };

        let statement = dv_to_string(&row[0]);
        let claim_type_s = dv_to_string(&row[1]);
        let source_id_s = dv_to_string(&row[2]);
        let confidence = match &row[3] {
            DataValue::Num(Num::Float(f)) => *f,
            DataValue::Num(Num::Int(n)) => *n as f64,
            _ => 0.8,
        };
        let sensitivity_s = dv_to_string(&row[4]);
        let workspace_id_s = dv_to_string(&row[5]);
        let created_ts = match &row[6] {
            DataValue::Num(Num::Float(f)) => *f,
            DataValue::Num(Num::Int(n)) => *n as f64,
            _ => 0.0,
        };

        let grounding_score_val = match &row[7] {
            DataValue::Num(Num::Float(f)) if *f >= 0.0 => Some(*f),
            DataValue::Num(Num::Int(n)) if *n >= 0 => Some(*n as f64),
            _ => None, // -1.0 is stored when unset
        };
        let grounding_method_s = dv_to_string(&row[8]);

        let claim_type = match claim_type_s.as_str() {
            "Decision" => ClaimType::Decision,
            "Opinion" => ClaimType::Opinion,
            "Plan" => ClaimType::Plan,
            "Requirement" => ClaimType::Requirement,
            "Metric" => ClaimType::Metric,
            "Definition" => ClaimType::Definition,
            "Dependency" => ClaimType::Dependency,
            "ApiSignature" => ClaimType::ApiSignature,
            "Architecture" => ClaimType::Architecture,
            _ => ClaimType::Fact,
        };

        let sensitivity = match sensitivity_s.as_str() {
            "Internal" => Sensitivity::Internal,
            "Confidential" => Sensitivity::Confidential,
            "Restricted" => Sensitivity::Restricted,
            _ => Sensitivity::Public,
        };

        let claim_id = id.parse::<ClaimId>().unwrap_or_else(|_| ClaimId::new());
        let source_id = source_id_s
            .parse::<SourceId>()
            .unwrap_or_else(|_| SourceId::new());
        let workspace = workspace_id_s
            .parse::<WorkspaceId>()
            .unwrap_or_else(|_| WorkspaceId::new());
        let created_at =
            chrono::DateTime::from_timestamp(created_ts as i64, 0).unwrap_or_else(chrono::Utc::now);

        use thinkingroot_core::types::GroundingMethod;
        let grounding_method = match grounding_method_s.as_str() {
            "Lexical" => Some(GroundingMethod::Lexical),
            "Span" => Some(GroundingMethod::Span),
            "Semantic" => Some(GroundingMethod::Semantic),
            "Combined" => Some(GroundingMethod::Combined),
            "Unverified" => Some(GroundingMethod::Unverified),
            "Structural" => Some(GroundingMethod::Structural),
            _ => None,
        };

        let event_date_ts = match &row[10] {
            DataValue::Num(Num::Float(f)) if *f > 0.0 => *f,
            DataValue::Num(Num::Int(n)) if *n > 0 => *n as f64,
            _ => 0.0,
        };
        let event_date = if event_date_ts > 0.0 {
            chrono::DateTime::from_timestamp(event_date_ts as i64, 0)
        } else {
            None
        };

        // Rooting columns (Migration 3). Row indices 11–14.
        let admission_tier =
            thinkingroot_core::types::AdmissionTier::from_str(dv_to_string(&row[11]).as_str());
        let derivation_parents_str = dv_to_string(&row[12]);
        let derivation = if derivation_parents_str.is_empty() {
            None
        } else {
            let parsed: std::result::Result<Vec<String>, _> =
                serde_json::from_str(&derivation_parents_str);
            match parsed {
                Ok(ids) => {
                    let parent_claim_ids: Vec<ClaimId> = ids
                        .iter()
                        .filter_map(|s| s.parse::<ClaimId>().ok())
                        .collect();
                    if parent_claim_ids.is_empty() {
                        None
                    } else {
                        Some(thinkingroot_core::types::DerivationProof {
                            parent_claim_ids,
                            derivation_rule: String::new(),
                        })
                    }
                }
                Err(_) => None,
            }
        };
        let predicate_json_str = dv_to_string(&row[13]);
        let predicate = if predicate_json_str.is_empty() {
            None
        } else {
            serde_json::from_str::<thinkingroot_core::types::Predicate>(&predicate_json_str).ok()
        };
        let last_rooted_ts = match &row[14] {
            DataValue::Num(Num::Float(f)) if *f > 0.0 => *f,
            DataValue::Num(Num::Int(n)) if *n > 0 => *n as f64,
            _ => 0.0,
        };
        let last_rooted_at = if last_rooted_ts > 0.0 {
            chrono::DateTime::from_timestamp(last_rooted_ts as i64, 0)
        } else {
            None
        };

        Ok(Some(Claim {
            id: claim_id,
            statement,
            claim_type,
            source: source_id,
            source_span: None,
            confidence: Confidence::new(confidence),
            valid_from: created_at,
            valid_until: None,
            sensitivity,
            workspace,
            extracted_by: PipelineVersion::current(),
            superseded_by: None,
            created_at,
            grounding_score: grounding_score_val,
            grounding_method,
            extraction_tier: match dv_to_string(&row[9]).as_str() {
                "structural" => thinkingroot_core::types::ExtractionTier::Structural,
                "agent_inferred" => thinkingroot_core::types::ExtractionTier::AgentInferred,
                _ => thinkingroot_core::types::ExtractionTier::Llm,
            },
            event_date,
            admission_tier,
            derivation,
            predicate,
            last_rooted_at,
            row_blake3: None,
            symbol: None,
        }))
    }

    /// Count orphaned claims (claims whose source_id has no matching source).
    pub fn count_orphaned_claims(&self) -> Result<usize> {
        let result = self.query_read(
            r#"?[count(cid)] :=
                *claims{id: cid, source_id},
                not *sources{id: source_id}"#,
        )?;
        if let Some(row) = result.rows.first() {
            match &row[0] {
                DataValue::Num(Num::Int(n)) => Ok(*n as usize),
                DataValue::Num(Num::Float(n)) => Ok(*n as usize),
                _ => Ok(0),
            }
        } else {
            Ok(0)
        }
    }

    /// Search claims by keyword (case-insensitive substring match).
    #[allow(clippy::type_complexity)]
    pub fn search_claims(
        &self,
        keyword: &str,
    ) -> Result<Vec<(String, String, String, f64, String)>> {
        let mut params = BTreeMap::new();
        params.insert("kw".into(), DataValue::Str(keyword.to_lowercase().into()));

        let result = self
            .db
            .run_script(
                r#"?[id, statement, claim_type, confidence, uri] :=
                    *claims{id, statement, claim_type, confidence},
                    lower_stmt = lowercase(statement),
                    regex_matches(lower_stmt, $kw),
                    *claim_source_edges{claim_id: id, source_id: sid},
                    *sources{id: sid, uri}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    match &row[3] {
                        DataValue::Num(Num::Float(f)) => *f,
                        DataValue::Num(Num::Int(i)) => *i as f64,
                        _ => 0.8,
                    },
                    dv_to_string(&row[4]),
                )
            })
            .collect())
    }

    /// Search entities by name (case-insensitive substring match).
    pub fn search_entities(&self, keyword: &str) -> Result<Vec<(String, String, String)>> {
        let mut params = BTreeMap::new();
        params.insert("kw".into(), DataValue::Str(keyword.to_lowercase().into()));

        let result = self
            .db
            .run_script(
                r#"?[id, canonical_name, entity_type] :=
                    *entities{id, canonical_name, entity_type},
                    lower_name = lowercase(canonical_name),
                    regex_matches(lower_name, $kw)"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                )
            })
            .collect())
    }

    /// Set temporal metadata for a claim (valid_from, valid_until, superseded_by).
    pub fn set_claim_temporal(
        &self,
        claim_id: &str,
        valid_from: f64,
        valid_until: f64,
        superseded_by: &str,
    ) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        params.insert("vf".into(), DataValue::Num(Num::Float(valid_from)));
        params.insert("vu".into(), DataValue::Num(Num::Float(valid_until)));
        params.insert("sb".into(), DataValue::Str(superseded_by.into()));

        self.query(
            r#"?[claim_id, valid_from, valid_until, superseded_by] <- [[$cid, $vf, $vu, $sb]]
            :put claim_temporal {claim_id => valid_from, valid_until, superseded_by}"#,
            params,
        )?;
        Ok(())
    }

    /// Supersede a claim: set its valid_until to now and record the superseding claim.
    pub fn supersede_claim(&self, old_claim_id: &str, new_claim_id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp() as f64;
        self.set_claim_temporal(old_claim_id, 0.0, now, new_claim_id)
    }

    /// Count superseded (expired) claims.
    pub fn count_superseded_claims(&self) -> Result<usize> {
        let result = self.query_read(
            r#"?[count(claim_id)] := *claim_temporal{claim_id, valid_until, superseded_by},
                valid_until > 0.0"#,
        )?;
        if let Some(row) = result.rows.first() {
            match &row[0] {
                DataValue::Num(Num::Int(n)) => Ok(*n as usize),
                DataValue::Num(Num::Float(n)) => Ok(*n as usize),
                _ => Ok(0),
            }
        } else {
            Ok(0)
        }
    }

    /// Get total counts of sources, claims, and entities.
    pub fn get_counts(&self) -> Result<(usize, usize, usize)> {
        let s = self.count_relation("sources")?;
        let c = self.count_relation("claims")?;
        let e = self.count_relation("entities")?;
        Ok((s, c, e))
    }

    fn count_relation(&self, name: &str) -> Result<usize> {
        let query = format!("?[count(id)] := *{name}{{id}}");
        let result = self.query_read(&query)?;
        if let Some(row) = result.rows.first() {
            match &row[0] {
                DataValue::Num(Num::Int(n)) => Ok(*n as usize),
                DataValue::Num(Num::Float(n)) => Ok(*n as usize),
                _ => Ok(0),
            }
        } else {
            Ok(0)
        }
    }

    /// Cascade-remove every structural row keyed on `source_id` across the
    /// 16 byte-anchored tables defined in `thinkingroot_core::STRUCTURAL_TABLES`.
    /// Called from `remove_source_by_id` after the legacy claim/entity
    /// cascade so a source delete leaves zero orphan structural rows.
    ///
    /// All 16 tables have a `:by_source` (or `:by_from`) index — each `:rm`
    /// is O(matches), not O(table-size).
    fn cascade_structural_tables_for_source(&self, source_id: &str) -> Result<()> {
        use thinkingroot_core::structural_registry::STRUCTURAL_TABLES;

        for spec in STRUCTURAL_TABLES {
            let mut params = BTreeMap::new();
            params.insert("sid".into(), DataValue::Str(source_id.into()));
            let script = pk_rm_script_for_table(spec.name, spec.source_id_column);
            self.query(&script, params)?;
        }
        Ok(())
    }

    fn remove_source_by_id(&self, source_id: &str) -> Result<()> {
        let claim_ids = self.get_claim_ids_for_source(source_id)?;
        self.remove_source_relations(source_id)?;

        let mut affected_entity_ids = std::collections::BTreeSet::new();

        for claim_id in &claim_ids {
            for entity_id in self.get_entity_ids_for_claim(claim_id)? {
                self.remove_claim_entity_edge(claim_id, &entity_id)?;
                affected_entity_ids.insert(entity_id);
            }

            self.remove_claim_source_edges_for_claim(claim_id)?;
            self.remove_claim_temporal(claim_id)?;
            self.remove_contradictions_for_claim(claim_id)?;
            // Cascade rooting-side state keyed by claim_id before the
            // claim row itself disappears.  Each helper uses the
            // predicate-`:rm` shape because the relations carry
            // attached secondary indexes.
            self.remove_trial_verdicts_for_claim(claim_id)?;
            self.remove_certificates_for_claim(claim_id)?;
            self.remove_derivation_edges_for_claim(claim_id)?;
            self.remove_claim(claim_id)?;
        }

        // Prune dangling references from the session-turn calendar
        // before the rest of the cascade finishes.  Pre-fix `turns`
        // stored claim ids as a JSON string with no FK back to
        // `claims`, so once a source was forgotten the turns table
        // accumulated dangling ids that would resolve to "claim
        // missing" for any future replay.  See
        // `prune_turns_referencing_claims` for the rewrite shape.
        if !claim_ids.is_empty() {
            self.prune_turns_referencing_claims(&claim_ids)?;
        }

        // Derived SVO calendar — must be cascaded before the
        // orphan-entity sweep, otherwise events keep pointing at
        // entity ULIDs the sweep is about to delete.
        self.remove_events_for_source(source_id)?;

        // Cascade the 16 byte-anchored structural tables introduced by the
        // Compile Completeness Contract.  Pre-water-flow these rows survived
        // source delete, leaving AEP/Hybrid clusters joining through dead
        // claim_ids.
        self.cascade_structural_tables_for_source(source_id)?;

        self.remove_source(source_id)?;

        for entity_id in affected_entity_ids {
            if !self.entity_has_claims(&entity_id)?
                && !self.entity_has_source_relations(&entity_id)?
            {
                self.remove_entity(&entity_id)?;
            }
        }

        Ok(())
    }

    pub fn get_claim_ids_for_source(&self, source_id: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));

        let result = self
            .db
            .run_script(
                "?[id] := *claims{id, source_id: $sid}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| dv_to_string(&row[0]))
            .collect())
    }

    /// Get entity IDs that have at least one claim from this source.
    /// Used to identify candidate stale vector entries before source removal.
    pub fn get_entity_ids_for_source(&self, source_id: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));

        let result = self
            .db
            .run_script(
                "?[entity_id] := *claim_source_edges{claim_id, source_id: $sid}, \
                 *claim_entity_edges{claim_id, entity_id}
                 ?[entity_id] := *source_entity_relations{source_id: $sid, from_id: entity_id}
                 ?[entity_id] := *source_entity_relations{source_id: $sid, to_id: entity_id}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| dv_to_string(&row[0]))
            .collect())
    }

    /// Point lookup: return (canonical_name, entity_type, description) for one entity.
    /// Used by branch union search to resolve hits that exist only in the branch graph.
    pub fn get_entity_by_id(&self, entity_id: &str) -> Result<Option<(String, String, String)>> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        let result = self
            .db
            .run_script(
                "?[canonical_name, entity_type, description] := \
                 *entities{id: $eid, canonical_name, entity_type, description}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_entity_by_id query failed: {e}")))?;

        Ok(result.rows.first().map(|row| {
            (
                dv_to_string(&row[0]),
                dv_to_string(&row[1]),
                dv_to_string(&row[2]),
            )
        }))
    }

    /// Point lookup: return (statement, claim_type, confidence, source_uri) for one claim.
    /// Used by branch union search to resolve hits that exist only in the branch graph.
    pub fn get_claim_with_source(
        &self,
        claim_id: &str,
    ) -> Result<Option<(String, String, f64, String)>> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));

        let result = self
            .db
            .run_script(
                r#"?[statement, claim_type, confidence, uri] :=
                    *claims{id: $cid, statement, claim_type, source_id, confidence},
                    *sources{id: source_id, uri}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_claim_with_source query failed: {e}")))?;

        Ok(result.rows.first().map(|row| {
            let conf = match &row[2] {
                DataValue::Num(Num::Float(f)) => *f,
                DataValue::Num(Num::Int(n)) => *n as f64,
                _ => 0.8,
            };
            (
                dv_to_string(&row[0]),
                dv_to_string(&row[1]),
                conf,
                dv_to_string(&row[3]),
            )
        }))
    }

    pub fn get_entity_ids_for_claim(&self, claim_id: &str) -> Result<Vec<String>> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));

        let result = self
            .db
            .run_script(
                "?[entity_id] := *claim_entity_edges{claim_id: $cid, entity_id}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| dv_to_string(&row[0]))
            .collect())
    }

    fn remove_claim_source_edges_for_claim(&self, claim_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));

        let result = self
            .db
            .run_script(
                "?[source_id] := *claim_source_edges{claim_id: $cid, source_id}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        for row in &result.rows {
            let source_id = dv_to_string(&row[0]);
            let mut rm_params = BTreeMap::new();
            rm_params.insert("cid".into(), DataValue::Str(claim_id.into()));
            rm_params.insert("sid".into(), DataValue::Str(source_id.into()));
            self.query(
                r#"?[claim_id, source_id] <- [[$cid, $sid]]
                :rm claim_source_edges {claim_id, source_id}"#,
                rm_params,
            )?;
        }

        Ok(())
    }

    fn remove_claim_entity_edge(&self, claim_id: &str, entity_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        self.query(
            r#"?[claim_id, entity_id] <- [[$cid, $eid]]
            :rm claim_entity_edges {claim_id, entity_id}"#,
            params,
        )?;
        Ok(())
    }

    fn remove_claim_temporal(&self, claim_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));

        self.query(
            r#"?[claim_id] <- [[$cid]]
            :rm claim_temporal {claim_id}"#,
            params,
        )?;
        Ok(())
    }

    fn remove_contradictions_for_claim(&self, claim_id: &str) -> Result<()> {
        for (id, claim_a, claim_b, _, _) in self.get_contradictions()? {
            if claim_a == claim_id || claim_b == claim_id {
                let mut params = BTreeMap::new();
                params.insert("id".into(), DataValue::Str(id.into()));
                self.query(
                    r#"?[id] <- [[$id]]
                    :rm contradictions {id}"#,
                    params,
                )?;
            }
        }

        Ok(())
    }

    fn remove_claim(&self, claim_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));

        self.query(
            r#"?[id] <- [[$cid]]
            :rm claims {id}"#,
            params,
        )?;
        Ok(())
    }

    fn remove_source(&self, source_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));

        self.query(
            r#"?[id] <- [[$sid]]
            :rm sources {id}"#,
            params,
        )?;
        Ok(())
    }

    /// Cascade-remove every `events` row whose `source_id` matches.
    ///
    /// Called from `remove_source_by_id` when a source is deleted.
    /// The `events` relation is a derived SVO calendar populated at
    /// compile time; without this cascade an orphan row survives
    /// every file delete or rename and `query_events_in_range` /
    /// `query_events_for_entity` emit ULID strings for entities the
    /// orphan-entity sweep has already GCd.  Mirrors the predicate
    /// `:rm` shape used by `truncate_structural_patterns_for_scope`
    /// — `::remove` is rejected by Cozo while the
    /// `events:by_subject` / `events:by_timestamp` indexes are
    /// attached, so we delete by subquery.
    fn remove_events_for_source(&self, source_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(source_id.into()));
        self.query(
            r#"?[id] := *events{id, source_id: $sid}
            :rm events {id}"#,
            params,
        )?;
        Ok(())
    }

    /// Cascade-remove every `trial_verdicts` row whose `claim_id`
    /// matches. Called from `remove_source_by_id` after the claims
    /// themselves are gone.
    ///
    /// `trial_verdicts` is the append-only audit log of probe runs;
    /// pre-fix every source-removal cycle leaked rows that pointed
    /// at no-longer-existent claim IDs, distorting the health-score
    /// admission-tier histogram and silently inflating verdict
    /// counts as users edit/rename files.  Predicate-`:rm` shape
    /// mirrors the events cascade — `trial_verdicts:by_claim` and
    /// `:by_time` indexes are attached so `::remove` is rejected.
    fn remove_trial_verdicts_for_claim(&self, claim_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        self.query(
            r#"?[id] := *trial_verdicts{id, claim_id: $cid}
            :rm trial_verdicts {id}"#,
            params,
        )?;
        Ok(())
    }

    /// Cascade-remove every `verification_certificates` row whose
    /// `claim_id` matches.  Called from `remove_source_by_id`.
    ///
    /// Without this cascade `get_certificate_by_hash` returns a
    /// certificate whose `claim_id` is dangling (the row in
    /// `claims` is gone), and the `certificates` relation grows
    /// indefinitely across compile cycles — the table is
    /// content-addressed by hash so the same trial inputs would
    /// otherwise stack up forever.  The newly-added
    /// `verification_certificates:by_claim` secondary index makes
    /// this O(matches) instead of O(n).
    fn remove_certificates_for_claim(&self, claim_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        self.query(
            r#"?[hash] := *verification_certificates{hash, claim_id: $cid}
            :rm verification_certificates {hash}"#,
            params,
        )?;
        Ok(())
    }

    /// Cascade-remove every `derivation_edges` row that names
    /// `claim_id` as parent OR child.  Called from
    /// `remove_source_by_id` so the rooting graph-traversal queries
    /// (which walk `derivation_edges:by_parent`/`:by_child`) don't
    /// follow dead edges into deleted claims and silently return
    /// empty proofs for surviving claims that derived from the
    /// removed source's claims.
    fn remove_derivation_edges_for_claim(&self, claim_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        self.query(
            r#"?[parent_claim_id, child_claim_id] := *derivation_edges{parent_claim_id, child_claim_id},
                (parent_claim_id = $cid or child_claim_id = $cid)
            :rm derivation_edges {parent_claim_id, child_claim_id}"#,
            params,
        )?;
        Ok(())
    }

    /// Cascade-remove every `known_unknowns` row whose `entity_id`
    /// matches. Called from `remove_entity` so the orphan sweep
    /// can't leave gap rows pointing at GC'd entities.  Pre-fix
    /// `reflect_list_open_gap_rows` (which joins `known_unknowns`
    /// with `entities`) would silently drop these orphan rows
    /// while `reflect_count_open_known_unknowns` (no join) kept
    /// counting them — count and detail diverged.
    fn remove_known_unknowns_for_entity(&self, entity_id: &str) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));
        self.query(
            r#"?[id] := *known_unknowns{id, entity_id: $eid}
            :rm known_unknowns {id}"#,
            params,
        )?;
        Ok(())
    }

    fn remove_source_relations(&self, source_id: &str) -> Result<()> {
        for (sid, from_id, to_id, relation_type, _) in self.get_all_source_relations_raw()? {
            if sid == source_id {
                let mut params = BTreeMap::new();
                params.insert("sid".into(), DataValue::Str(sid.into()));
                params.insert("fid".into(), DataValue::Str(from_id.into()));
                params.insert("tid".into(), DataValue::Str(to_id.into()));
                params.insert("rtype".into(), DataValue::Str(relation_type.into()));
                self.query(
                    r#"?[source_id, from_id, to_id, relation_type] <- [[$sid, $fid, $tid, $rtype]]
                    :rm source_entity_relations {source_id, from_id, to_id, relation_type}"#,
                    params,
                )?;
            }
        }

        Ok(())
    }

    fn entity_has_claims(&self, entity_id: &str) -> Result<bool> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        let result = self
            .db
            .run_script(
                "?[count(claim_id)] := *claim_entity_edges{claim_id, entity_id: $eid}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(count_from_rows(&result.rows) > 0)
    }

    fn entity_has_source_relations(&self, entity_id: &str) -> Result<bool> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        let from_rows = self
            .db
            .run_script(
                "?[count(source_id)] := *source_entity_relations{source_id, from_id: $eid, to_id, relation_type, strength}",
                params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        if count_from_rows(&from_rows.rows) > 0 {
            return Ok(true);
        }

        let to_rows = self
            .db
            .run_script(
                "?[count(source_id)] := *source_entity_relations{source_id, from_id, to_id: $eid, relation_type, strength}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(count_from_rows(&to_rows.rows) > 0)
    }

    fn remove_entity(&self, entity_id: &str) -> Result<()> {
        let aliases = self.get_aliases_for_entity(entity_id)?;
        for alias in aliases {
            let mut params = BTreeMap::new();
            params.insert("eid".into(), DataValue::Str(entity_id.into()));
            params.insert("alias".into(), DataValue::Str(alias.into()));
            self.query(
                r#"?[entity_id, alias] <- [[$eid, $alias]]
                :rm entity_aliases {entity_id, alias}"#,
                params,
            )?;
        }

        self.remove_relations_for_entity(entity_id)?;

        // Cascade `known_unknowns` rows keyed by this entity before the
        // entity row itself is gone — otherwise `reflect_list_open_gap_rows`
        // (joining `known_unknowns` with `entities`) silently drops them
        // while `reflect_count_open_known_unknowns` (no join) keeps
        // counting them.
        self.remove_known_unknowns_for_entity(entity_id)?;

        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));
        self.query(
            r#"?[id] <- [[$eid]]
            :rm entities {id}"#,
            params,
        )?;
        Ok(())
    }

    fn clear_entity_relations(&self) -> Result<()> {
        let result = self.query_read(
            "?[from_id, to_id, relation_type] := *entity_relations{from_id, to_id, relation_type, strength}",
        )?;

        for row in &result.rows {
            let from_id = dv_to_string(&row[0]);
            let to_id = dv_to_string(&row[1]);
            let relation_type = dv_to_string(&row[2]);
            let mut params = BTreeMap::new();
            params.insert("fid".into(), DataValue::Str(from_id.into()));
            params.insert("tid".into(), DataValue::Str(to_id.into()));
            params.insert("rtype".into(), DataValue::Str(relation_type.into()));
            self.query(
                r#"?[from_id, to_id, relation_type] <- [[$fid, $tid, $rtype]]
                :rm entity_relations {from_id, to_id, relation_type}"#,
                params,
            )?;
        }

        Ok(())
    }

    fn remove_relations_for_entity(&self, entity_id: &str) -> Result<()> {
        for (source_id, from_id, to_id, relation_type, _) in self.get_all_source_relations_raw()? {
            if from_id == entity_id || to_id == entity_id {
                let mut params = BTreeMap::new();
                params.insert("sid".into(), DataValue::Str(source_id.into()));
                params.insert("fid".into(), DataValue::Str(from_id.into()));
                params.insert("tid".into(), DataValue::Str(to_id.into()));
                params.insert("rtype".into(), DataValue::Str(relation_type.into()));
                self.query(
                    r#"?[source_id, from_id, to_id, relation_type] <- [[$sid, $fid, $tid, $rtype]]
                    :rm source_entity_relations {source_id, from_id, to_id, relation_type}"#,
                    params,
                )?;
            }
        }

        let result = self.query_read(
            "?[from_id, to_id, relation_type] := *entity_relations{from_id, to_id, relation_type, strength}",
        )?;

        for row in &result.rows {
            let from_id = dv_to_string(&row[0]);
            let to_id = dv_to_string(&row[1]);
            let relation_type = dv_to_string(&row[2]);
            if from_id == entity_id || to_id == entity_id {
                let mut params = BTreeMap::new();
                params.insert("fid".into(), DataValue::Str(from_id.into()));
                params.insert("tid".into(), DataValue::Str(to_id.into()));
                params.insert("rtype".into(), DataValue::Str(relation_type.into()));
                self.query(
                    r#"?[from_id, to_id, relation_type] <- [[$fid, $tid, $rtype]]
                    :rm entity_relations {from_id, to_id, relation_type}"#,
                    params,
                )?;
            }
        }

        Ok(())
    }

    /// Returns a map from claim_id → list of entity canonical names linked to that claim.
    /// Only claim IDs present in `claim_ids` are included in the result.
    pub fn get_entity_names_for_claims(
        &self,
        claim_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        if claim_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let result = self.query_read(
            "?[claim_id, name] := *claim_entity_edges{claim_id, entity_id: eid}, \
             *entities{id: eid, canonical_name: name}",
        )?;

        let id_set: std::collections::HashSet<&str> = claim_ids.iter().copied().collect();
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        for row in &result.rows {
            let cid = dv_to_string(&row[0]);
            let name = dv_to_string(&row[1]);
            if id_set.contains(cid.as_str()) {
                map.entry(cid).or_default().push(name);
            }
        }

        Ok(map)
    }

    /// Find an entity ID by its canonical name. Returns `None` if not found.
    pub fn find_entity_id_by_name(&self, name: &str) -> Result<Option<String>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));

        let result = self
            .db
            .run_script(
                "?[id] := *entities{id, canonical_name: $name}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query failed: {e}")))?;

        Ok(result.rows.first().map(|row| dv_to_string(&row[0])))
    }

    #[allow(clippy::type_complexity)]
    fn get_all_source_relations_raw(&self) -> Result<Vec<(String, String, String, String, f64)>> {
        let result = self.query_read(
            "?[source_id, from_id, to_id, relation_type, strength] := *source_entity_relations{source_id, from_id, to_id, relation_type, strength}",
        )?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_string(&row[2]),
                    dv_to_string(&row[3]),
                    match &row[4] {
                        DataValue::Num(Num::Float(f)) => *f,
                        DataValue::Num(Num::Int(i)) => *i as f64,
                        _ => 1.0,
                    },
                )
            })
            .collect())
    }
}

// ─── Intelligent Serve Layer: graph traversal types ──────────────────────────

/// A single claim with its source provenance — used in entity context queries.
#[derive(Debug, Clone, Serialize)]
pub struct ContextClaim {
    pub id: String,
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub source_uri: String,
    pub extraction_tier: String,
}

/// A contradiction involving one of an entity's claims.
#[derive(Debug, Clone, Serialize)]
pub struct ContextContradiction {
    pub explanation: String,
    pub status: String,
}

/// Full context for one entity: its metadata, relations (both directions),
/// claims with provenance, and any active contradictions.
#[derive(Debug, Clone, Serialize)]
pub struct EntityContext {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub description: String,
    pub aliases: Vec<String>,
    /// Relations FROM this entity to others: (target_name, rel_type, strength).
    pub outgoing_relations: Vec<(String, String, f64)>,
    /// Relations TO this entity from others: (source_name, rel_type, strength).
    pub incoming_relations: Vec<(String, String, f64)>,
    pub claims: Vec<ContextClaim>,
    pub contradictions: Vec<ContextContradiction>,
}

/// A direct neighbour of the focal entity in the graph.
#[derive(Debug, Clone, Serialize)]
pub struct NeighborhoodEntity {
    pub name: String,
    pub entity_type: String,
    pub relation_type: String,
    /// "outgoing" (focal → neighbour) or "incoming" (neighbour → focal).
    pub direction: String,
    pub claim_count: usize,
}

/// Top entity by claim count — used for workspace overview.
#[derive(Debug, Clone, Serialize)]
pub struct TopEntity {
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
}

// ─── GraphStore: intelligent serve methods ───────────────────────────────────

impl GraphStore {
    /// Return complete context for the entity with the given canonical name.
    /// Executes 6 Datalog queries covering entity metadata, outgoing relations,
    /// incoming relations, claims with sources, and contradictions (both sides).
    /// Returns `None` when no entity with that name exists.
    pub fn get_entity_context(&self, entity_name: &str) -> Result<Option<EntityContext>> {
        let mut name_params = BTreeMap::new();
        name_params.insert("name".into(), DataValue::Str(entity_name.into()));

        // 1. Resolve entity id, type, description.
        let entity_rows = self
            .db
            .run_script(
                "?[id, entity_type, description] := *entities{id, canonical_name: $name, entity_type, description}",
                name_params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("entity_context query failed: {e}")))?;

        let (eid, entity_type, description) = match entity_rows.rows.first() {
            None => return Ok(None),
            Some(row) => (
                dv_to_string(&row[0]),
                dv_to_string(&row[1]),
                dv_to_string(&row[2]),
            ),
        };

        let mut eid_params = BTreeMap::new();
        eid_params.insert("eid".into(), DataValue::Str(eid.clone().into()));

        // 2. Aliases.
        let aliases = self.get_aliases_for_entity(&eid)?;

        // 3. Outgoing relations: focal → neighbour.
        let out_rows = self
            .db
            .run_script(
                r#"?[to_name, rel_type, strength] :=
                    *entity_relations{from_id: $eid, to_id, relation_type: rel_type, strength},
                    *entities{id: to_id, canonical_name: to_name}"#,
                eid_params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("outgoing_relations query failed: {e}")))?;

        let outgoing_relations = out_rows
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_float(&row[2]),
                )
            })
            .collect();

        // 4. Incoming relations: neighbour → focal (reverse deps).
        let in_rows = self
            .db
            .run_script(
                r#"?[from_name, rel_type, strength] :=
                    *entity_relations{from_id, to_id: $eid, relation_type: rel_type, strength},
                    *entities{id: from_id, canonical_name: from_name}"#,
                eid_params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("incoming_relations query failed: {e}")))?;

        let incoming_relations = in_rows
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_float(&row[2]),
                )
            })
            .collect();

        // 5. Claims with source URIs (3-way join).
        let claims_rows = self
            .db
            .run_script(
                r#"?[id, statement, claim_type, confidence, uri, extraction_tier] :=
                    *claim_entity_edges{claim_id: id, entity_id: $eid},
                    *claims{id, statement, claim_type, confidence, extraction_tier},
                    *claim_source_edges{claim_id: id, source_id: sid},
                    *sources{id: sid, uri}"#,
                eid_params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("claims_context query failed: {e}")))?;

        let claims = claims_rows
            .rows
            .iter()
            .map(|row| ContextClaim {
                id: dv_to_string(&row[0]),
                statement: dv_to_string(&row[1]),
                claim_type: dv_to_string(&row[2]),
                confidence: dv_to_float(&row[3]),
                source_uri: dv_to_string(&row[4]),
                extraction_tier: dv_to_string(&row[5]),
            })
            .collect();

        // 6a. Contradictions where this entity's claim is claim_a.
        let contra_a = self
            .db
            .run_script(
                r#"?[explanation, status] :=
                    *claim_entity_edges{claim_id, entity_id: $eid},
                    *contradictions{claim_a: claim_id, explanation, status}"#,
                eid_params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("contradictions_a query failed: {e}")))?;

        // 6b. Contradictions where this entity's claim is claim_b.
        let contra_b = self
            .db
            .run_script(
                r#"?[explanation, status] :=
                    *claim_entity_edges{claim_id, entity_id: $eid},
                    *contradictions{claim_b: claim_id, explanation, status}"#,
                eid_params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("contradictions_b query failed: {e}")))?;

        let mut contradictions: Vec<ContextContradiction> = contra_a
            .rows
            .iter()
            .chain(contra_b.rows.iter())
            .map(|row| ContextContradiction {
                explanation: dv_to_string(&row[0]),
                status: dv_to_string(&row[1]),
            })
            .collect();

        // Deduplicate by explanation text (both sides may yield same contradiction).
        contradictions.sort_by_key(|a| a.explanation.clone());
        contradictions.dedup_by(|a, b| a.explanation == b.explanation);

        Ok(Some(EntityContext {
            id: eid,
            name: entity_name.to_string(),
            entity_type,
            description,
            aliases,
            outgoing_relations,
            incoming_relations,
            claims,
            contradictions,
        }))
    }

    /// Return all entities that have a relation pointing TO `entity_name`.
    /// Result: (caller_name, relation_type, strength).
    pub fn find_reverse_deps(&self, entity_name: &str) -> Result<Vec<(String, String, f64)>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(entity_name.into()));

        let result = self
            .db
            .run_script(
                r#"?[from_name, rel_type, strength] :=
                    *entities{id: to_id, canonical_name: $name},
                    *entity_relations{from_id, to_id, relation_type: rel_type, strength},
                    *entities{id: from_id, canonical_name: from_name}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("reverse_deps query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                (
                    dv_to_string(&row[0]),
                    dv_to_string(&row[1]),
                    dv_to_float(&row[2]),
                )
            })
            .collect())
    }

    /// Return all direct neighbours (radius = 1) of `entity_name`, in both
    /// directions, with their entity type and claim count.
    pub fn get_neighborhood(&self, entity_name: &str) -> Result<Vec<NeighborhoodEntity>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(entity_name.into()));

        // Outgoing: focal → neighbour.
        let out_rows = self
            .db
            .run_script(
                r#"?[neighbor_name, neighbor_type, rel_type] :=
                    *entities{id: eid, canonical_name: $name},
                    *entity_relations{from_id: eid, to_id, relation_type: rel_type},
                    *entities{id: to_id, canonical_name: neighbor_name, entity_type: neighbor_type}"#,
                params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("neighborhood_out query failed: {e}")))?;

        // Incoming: neighbour → focal.
        let in_rows = self
            .db
            .run_script(
                r#"?[neighbor_name, neighbor_type, rel_type] :=
                    *entities{id: eid, canonical_name: $name},
                    *entity_relations{from_id, to_id: eid, relation_type: rel_type},
                    *entities{id: from_id, canonical_name: neighbor_name, entity_type: neighbor_type}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("neighborhood_in query failed: {e}")))?;

        let mut neighbors: Vec<NeighborhoodEntity> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (rows, direction) in [(&out_rows.rows, "outgoing"), (&in_rows.rows, "incoming")] {
            for row in rows {
                let name = dv_to_string(&row[0]);
                if seen.insert(name.clone()) {
                    let claim_count = self.get_claim_count_for_entity_name(&name).unwrap_or(0);
                    neighbors.push(NeighborhoodEntity {
                        name,
                        entity_type: dv_to_string(&row[1]),
                        relation_type: dv_to_string(&row[2]),
                        direction: direction.to_string(),
                        claim_count,
                    });
                }
            }
        }

        Ok(neighbors)
    }

    /// Return the top `limit` entities ranked by claim count.
    pub fn get_top_entities_by_claim_count(&self, limit: usize) -> Result<Vec<TopEntity>> {
        // Push the caller's `limit` into the Datalog query so a
        // request for `limit > 20` actually returns more than 20
        // rows.  Pre-fix the script hardcoded `:limit 20`, which
        // capped the server-side result regardless of `limit` and
        // silently truncated `top entities` dashboards — same
        // family of bug as the Brain view's pre-rewrite SQL LIMIT
        // (CLAUDE.md "no SQL LIMIT" invariant).
        let mut params = BTreeMap::new();
        params.insert("lim".into(), DataValue::Num(Num::Int(limit as i64)));
        let result = self
            .db
            .run_script(
                r#"entity_cnts[eid, count(cid)] :=
                    *claim_entity_edges{entity_id: eid, claim_id: cid}
                ?[name, entity_type, cnt] :=
                    entity_cnts[eid, cnt],
                    *entities{id: eid, canonical_name: name, entity_type}
                :order -cnt
                :limit $lim"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("top_entities query failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .take(limit)
            .map(|row| TopEntity {
                name: dv_to_string(&row[0]),
                entity_type: dv_to_string(&row[1]),
                claim_count: match &row[2] {
                    DataValue::Num(Num::Int(n)) => *n as usize,
                    DataValue::Num(Num::Float(f)) => *f as usize,
                    _ => 0,
                },
            })
            .collect())
    }

    /// Count claims linked to an entity looked up by canonical name.
    fn get_claim_count_for_entity_name(&self, entity_name: &str) -> Result<usize> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(entity_name.into()));

        let result = self
            .db
            .run_script(
                r#"?[count(cid)] :=
                    *entities{id: eid, canonical_name: $name},
                    *claim_entity_edges{claim_id: cid, entity_id: eid}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("claim_count query failed: {e}")))?;

        Ok(count_from_rows(&result.rows))
    }

    /// Find an entity by exact canonical name (case-insensitive) or by alias.
    /// Returns `(id, canonical_name)` if found.
    pub fn find_entity_by_name(&self, name: &str) -> Result<Option<(String, String)>> {
        let lower = name.to_lowercase();
        let mut params = BTreeMap::new();
        params.insert("lower".into(), DataValue::Str(lower.clone().into()));

        // Exact case-insensitive match on canonical name.
        let result = self
            .db
            .run_script(
                r#"?[id, canonical_name] :=
                    *entities{id, canonical_name},
                    lowercase(canonical_name) = $lower"#,
                params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("find_entity query failed: {e}")))?;

        if let Some(row) = result.rows.first() {
            return Ok(Some((dv_to_string(&row[0]), dv_to_string(&row[1]))));
        }

        // Alias match.
        let alias_result = self
            .db
            .run_script(
                r#"?[id, canonical_name] :=
                    *entity_aliases{entity_id: id, alias},
                    lowercase(alias) = $lower,
                    *entities{id, canonical_name}"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("find_entity_alias query failed: {e}")))?;

        Ok(alias_result
            .rows
            .first()
            .map(|row| (dv_to_string(&row[0]), dv_to_string(&row[1]))))
    }

    // ── Event Calendar ────────────────────────────────────────────────────────

    /// Insert a batch of SVO events into the `events` table.
    /// Called from the pipeline's Phase 2c (post-extraction).
    pub fn insert_events(
        &mut self,
        events: &[thinkingroot_core::types::ExtractedEvent],
    ) -> Result<usize> {
        let mut count = 0;
        for ev in events {
            // CozoDB `:put` does not support `col: var` rename mapping
            // inside the relation braces — the names must match either
            // the head clause's bound vars or the relation's column
            // names verbatim.  Match the existing convention used by
            // `insert_source` / `insert_claim`: head vars and params
            // are named identically to the schema columns.
            let mut params = BTreeMap::new();
            params.insert("id".into(), DataValue::Str(ev.id.clone().into()));
            params.insert(
                "subject_entity_id".into(),
                DataValue::Str(ev.subject_entity_id.clone().into()),
            );
            params.insert("verb".into(), DataValue::Str(ev.verb.clone().into()));
            params.insert(
                "object_entity_id".into(),
                DataValue::Str(ev.object_entity_id.clone().into()),
            );
            params.insert("timestamp".into(), DataValue::from(ev.timestamp));
            params.insert(
                "normalized_date".into(),
                DataValue::Str(ev.normalized_date.clone().into()),
            );
            params.insert(
                "source_id".into(),
                DataValue::Str(ev.source_id.clone().into()),
            );
            params.insert("confidence".into(), DataValue::from(ev.confidence));

            self.query(
                r#"?[id, subject_entity_id, verb, object_entity_id, timestamp, normalized_date, source_id, confidence] <- [[
                    $id, $subject_entity_id, $verb, $object_entity_id, $timestamp, $normalized_date, $source_id, $confidence
                ]]
                :put events {id => subject_entity_id, verb, object_entity_id, timestamp, normalized_date, source_id, confidence}"#,
                params,
            )?;
            count += 1;
        }
        Ok(count)
    }

    /// Query events whose timestamp falls within [start_ts, end_ts].
    pub fn query_events_in_range(&self, start_ts: f64, end_ts: f64) -> Result<Vec<EventRow>> {
        let mut params = BTreeMap::new();
        params.insert("start".into(), DataValue::from(start_ts));
        params.insert("end".into(), DataValue::from(end_ts));

        let result = self
            .db
            .run_script(
                "?[id, subj, verb, obj, nd] :=
                *events{id, subject_entity_id: subj, verb, object_entity_id: obj,
                        timestamp: ts, normalized_date: nd},
                ts >= $start, ts <= $end",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query_events_in_range failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| EventRow {
                id: dv_to_string(&row[0]),
                subject_entity_id: dv_to_string(&row[1]),
                verb: dv_to_string(&row[2]),
                object_entity_id: dv_to_string(&row[3]),
                normalized_date: dv_to_string(&row[4]),
                subject_name: String::new(),
                object_name: String::new(),
            })
            .collect())
    }

    /// Query all events where the given entity is the subject.
    pub fn query_events_for_entity(&self, entity_id: &str) -> Result<Vec<EventRow>> {
        let mut params = BTreeMap::new();
        params.insert("eid".into(), DataValue::Str(entity_id.into()));

        let result = self
            .db
            .run_script(
                "?[id, subj, verb, obj, nd] :=
                *events{id, subject_entity_id: $eid, verb, object_entity_id: obj,
                        normalized_date: nd},
                subj = $eid",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query_events_for_entity failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| EventRow {
                id: dv_to_string(&row[0]),
                subject_entity_id: dv_to_string(&row[1]),
                verb: dv_to_string(&row[2]),
                object_entity_id: dv_to_string(&row[3]),
                normalized_date: dv_to_string(&row[4]),
                subject_name: String::new(),
                object_name: String::new(),
            })
            .collect())
    }

    /// Return the maximum `event_date` timestamp stored in the claims table.
    ///
    /// Used as the **temporal anchor** for relative date queries ("last month",
    /// "X days ago").  For personal-memory workspaces the most recent claim
    /// event_date approximates "now" from the user's perspective — far more
    /// accurate than using the compile/query wall-clock time which would be
    /// months or years after the stored sessions.
    ///
    /// Returns `None` when the claims table is empty or no claim has event_date > 0.
    pub fn get_max_event_timestamp(&self) -> Result<Option<f64>> {
        let result = self
            .db
            .run_script(
                "?[max(event_date)] := *claims{event_date}, event_date > 0.0",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_max_event_timestamp failed: {e}")))?;

        if let Some(row) = result.rows.first() {
            let ts = match &row[0] {
                DataValue::Num(Num::Float(f)) => *f,
                DataValue::Num(Num::Int(i)) => *i as f64,
                _ => 0.0,
            };
            if ts > 0.0 {
                return Ok(Some(ts));
            }
        }
        Ok(None)
    }

    // ── Turn calendar ─────────────────────────────────────────────────────────

    /// Record that a set of claim IDs were contributed in turn `turn_number` of
    /// session `session_id`.  Upserts so reconnected sessions accumulate turns.
    pub fn record_turn(
        &self,
        session_id: &str,
        turn_number: u64,
        claim_ids: &[String],
    ) -> Result<()> {
        let ts = chrono::Utc::now().timestamp() as f64;
        let claim_ids_json = serde_json::to_string(claim_ids).unwrap_or_else(|_| "[]".to_string());

        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(session_id.into()));
        params.insert("turn".into(), DataValue::Num(Num::Int(turn_number as i64)));
        params.insert("cids".into(), DataValue::Str(claim_ids_json.into()));
        params.insert("ts".into(), DataValue::Num(Num::Float(ts)));

        self.db
            .run_script(
                "?[session_id, turn_number, claim_ids, timestamp] <- \
             [[$sid, $turn, $cids, $ts]] \
             :put turns { session_id, turn_number => claim_ids, timestamp }",
                params,
                ScriptMutability::Mutable,
            )
            .map_err(|e| Error::GraphStorage(format!("record_turn failed: {e}")))?;

        Ok(())
    }

    /// Filter dangling claim references out of the turns calendar.
    ///
    /// `turns.claim_ids` is a JSON-encoded `Vec<String>` because the
    /// list is variable-length per turn — CozoDB doesn't support
    /// inline arrays as primary-key components.  The downside is that
    /// CozoDB can't enforce a foreign-key cascade from
    /// `claims.id → turns.claim_ids[*]`, so when a claim is removed
    /// (e.g. via `remove_source_by_id`) the turn rows would otherwise
    /// keep pointing at the dead id.
    ///
    /// This helper rewrites every turn row whose `claim_ids` overlap
    /// with `removed_claim_ids`, decoding the JSON, filtering, and
    /// re-encoding.  Rows with no overlap are not touched.  When a
    /// turn ends up with zero remaining claim ids it is deleted in
    /// full — an empty turn carries no replay value.
    pub fn prune_turns_referencing_claims(&self, removed_claim_ids: &[String]) -> Result<()> {
        if removed_claim_ids.is_empty() {
            return Ok(());
        }
        // Pull every turn row first.  Turns are bounded by session
        // count × turn count which is tiny relative to the claim set,
        // so a full scan is fine here — and avoids a Datalog rewrite
        // we'd have to implement around the JSON-string encoding.
        let result = self
            .db
            .run_script(
                "?[session_id, turn_number, claim_ids, timestamp] := \
                    *turns{session_id, turn_number, claim_ids, timestamp}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("scan turns: {e}")))?;

        let removed: std::collections::HashSet<&str> =
            removed_claim_ids.iter().map(String::as_str).collect();

        for row in &result.rows {
            let session_id = dv_to_string(&row[0]);
            let turn_number = match &row[1] {
                DataValue::Num(Num::Int(n)) => *n as u64,
                DataValue::Num(Num::Float(f)) => *f as u64,
                _ => continue,
            };
            let claim_ids_json = dv_to_string(&row[2]);
            let timestamp = match &row[3] {
                DataValue::Num(Num::Float(f)) => *f,
                DataValue::Num(Num::Int(n)) => *n as f64,
                _ => continue,
            };

            let claim_ids: Vec<String> =
                serde_json::from_str(&claim_ids_json).unwrap_or_default();
            let kept: Vec<String> = claim_ids
                .into_iter()
                .filter(|id| !removed.contains(id.as_str()))
                .collect();

            // Skip rows that didn't reference any removed claim — the
            // length comparison is cheaper than re-encoding +
            // re-writing.  When we re-encode, we always upsert with
            // the canonical `serde_json` shape so the read path's
            // `serde_json::from_str` round-trips cleanly.
            let kept_json = serde_json::to_string(&kept).unwrap_or_else(|_| "[]".to_string());
            if kept_json == claim_ids_json {
                continue;
            }

            if kept.is_empty() {
                // Empty turns are useless — delete the row outright.
                let mut params = BTreeMap::new();
                params.insert("sid".into(), DataValue::Str(session_id.clone().into()));
                params.insert("turn".into(), DataValue::Num(Num::Int(turn_number as i64)));
                self.db
                    .run_script(
                        "?[session_id, turn_number] <- [[$sid, $turn]] \
                         :rm turns { session_id, turn_number }",
                        params,
                        ScriptMutability::Mutable,
                    )
                    .map_err(|e| {
                        Error::GraphStorage(format!("delete empty turn: {e}"))
                    })?;
            } else {
                let mut params = BTreeMap::new();
                params.insert("sid".into(), DataValue::Str(session_id.into()));
                params.insert("turn".into(), DataValue::Num(Num::Int(turn_number as i64)));
                params.insert("cids".into(), DataValue::Str(kept_json.into()));
                params.insert("ts".into(), DataValue::Num(Num::Float(timestamp)));
                self.db
                    .run_script(
                        "?[session_id, turn_number, claim_ids, timestamp] <- \
                         [[$sid, $turn, $cids, $ts]] \
                         :put turns { session_id, turn_number => claim_ids, timestamp }",
                        params,
                        ScriptMutability::Mutable,
                    )
                    .map_err(|e| {
                        Error::GraphStorage(format!("rewrite pruned turn: {e}"))
                    })?;
            }
        }

        Ok(())
    }

    /// Query all turns for a session, ordered by turn_number ascending.
    pub fn query_turns_for_session(&self, session_id: &str) -> Result<Vec<TurnRow>> {
        let mut params = BTreeMap::new();
        params.insert("sid".into(), DataValue::Str(session_id.into()));

        let result = self
            .db
            .run_script(
                "?[turn_number, claim_ids, timestamp] := \
             *turns{session_id: $sid, turn_number, claim_ids, timestamp} \
             :order turn_number",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("query_turns_for_session failed: {e}")))?;

        Ok(result
            .rows
            .iter()
            .map(|row| {
                let turn_number = match &row[0] {
                    DataValue::Num(Num::Int(n)) => *n as u64,
                    DataValue::Num(Num::Float(f)) => *f as u64,
                    _ => 0,
                };
                let claim_ids_json = dv_to_string(&row[1]);
                let claim_ids: Vec<String> =
                    serde_json::from_str(&claim_ids_json).unwrap_or_default();
                let timestamp = match &row[2] {
                    DataValue::Num(Num::Float(f)) => *f,
                    DataValue::Num(Num::Int(n)) => *n as f64,
                    _ => 0.0,
                };
                TurnRow {
                    turn_number,
                    claim_ids,
                    timestamp,
                }
            })
            .collect())
    }

    // ─── T0.7: connector ingest log helpers ─────────────────────────────

    /// Look up a previously-recorded connector ingest by its
    /// `(connector_id, install_id, idempotency_key)` triple.
    ///
    /// Returns `Some(claim_ids)` when the key has been seen before
    /// (the bulk-contribute API short-circuits to "no-op, here are the
    /// already-accepted ids"), or `None` for a first-time call.
    ///
    /// The lookup is a single point query against the relation's
    /// primary key — O(log n) on the BTree-backed CozoDB store.
    pub fn lookup_connector_ingest(
        &self,
        connector_id: &str,
        install_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<ConnectorIngestRecord>> {
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(connector_id.into()));
        params.insert("iid".into(), DataValue::Str(install_id.into()));
        params.insert("ik".into(), DataValue::Str(idempotency_key.into()));

        let result = self
            .db
            .run_script(
                r#"?[claim_ids, ingested_at, branch, source_uri] :=
                    *connector_ingest_log{
                        connector_id: $cid, install_id: $iid, idempotency_key: $ik,
                        claim_ids, ingested_at, branch, source_uri
                    }"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("lookup_connector_ingest failed: {e}")))?;

        let Some(row) = result.rows.first() else {
            return Ok(None);
        };

        let claim_ids_json = dv_to_string(&row[0]);
        let claim_ids: Vec<String> =
            serde_json::from_str(&claim_ids_json).unwrap_or_default();
        let ingested_at = match &row[1] {
            DataValue::Num(Num::Float(f)) => *f,
            DataValue::Num(Num::Int(i)) => *i as f64,
            _ => 0.0,
        };
        let branch_owned = dv_to_string(&row[2]);
        let source_uri = dv_to_string(&row[3]);

        Ok(Some(ConnectorIngestRecord {
            connector_id: connector_id.to_string(),
            install_id: install_id.to_string(),
            idempotency_key: idempotency_key.to_string(),
            claim_ids,
            ingested_at,
            branch: if branch_owned.is_empty() {
                None
            } else {
                Some(branch_owned)
            },
            source_uri,
        }))
    }

    /// Record a successful connector ingest. Atomic `:put` semantics
    /// — replay against the same PK overwrites the existing row,
    /// which is the right behaviour because the caller has already
    /// determined (via `lookup_connector_ingest`) that this is a
    /// first-time call. The overwrite path only fires on a race
    /// where two parallel webhook deliveries hit the daemon
    /// simultaneously; both end up writing the same ids, so
    /// last-writer-wins is safe.
    pub fn record_connector_ingest(
        &self,
        connector_id: &str,
        install_id: &str,
        idempotency_key: &str,
        claim_ids: &[String],
        branch: Option<&str>,
        source_uri: &str,
    ) -> Result<()> {
        let ts = chrono::Utc::now().timestamp() as f64;
        let claim_ids_json =
            serde_json::to_string(claim_ids).unwrap_or_else(|_| "[]".to_string());

        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(connector_id.into()));
        params.insert("iid".into(), DataValue::Str(install_id.into()));
        params.insert("ik".into(), DataValue::Str(idempotency_key.into()));
        params.insert("cids".into(), DataValue::Str(claim_ids_json.into()));
        params.insert("ts".into(), DataValue::Num(Num::Float(ts)));
        params.insert(
            "branch".into(),
            DataValue::Str(branch.unwrap_or("").into()),
        );
        params.insert("src".into(), DataValue::Str(source_uri.into()));

        self.db
            .run_script(
                "?[connector_id, install_id, idempotency_key, claim_ids, ingested_at, branch, source_uri] <- \
                 [[$cid, $iid, $ik, $cids, $ts, $branch, $src]] \
                 :put connector_ingest_log { \
                     connector_id, install_id, idempotency_key => \
                     claim_ids, ingested_at, branch, source_uri \
                 }",
                params,
                ScriptMutability::Mutable,
            )
            .map_err(|e| Error::GraphStorage(format!("record_connector_ingest failed: {e}")))?;

        Ok(())
    }

    // ─── T2.6: per-claim sensitivity lookup ─────────────────────────────

    /// Fetch the `claims.sensitivity` value (Pascal-case form, e.g.
    /// `"Public"`, `"Internal"`, `"Confidential"`, `"Restricted"`) for
    /// a batch of claim ids. Used by the per-branch redaction filter
    /// to decide which rows to drop or rewrite at the response
    /// boundary without joining sensitivity into every read query.
    ///
    /// Missing claims are simply absent from the returned map — the
    /// caller decides the default (typically `Sensitivity::Public`).
    pub fn get_sensitivities_for_claims(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        if ids.is_empty() {
            return Ok(out);
        }
        // CozoDB's parameter binding doesn't support list parameters
        // for `*claims{id: $ids}` membership tests; the cleanest path
        // is one point query per id, which is O(log n) per lookup
        // (fast enough for the per-call top-K rows the redaction
        // layer touches — never the full claims table).
        for id in ids {
            let mut params = BTreeMap::new();
            params.insert("id".into(), DataValue::Str(id.as_str().into()));
            let result = self
                .db
                .run_script(
                    "?[s] := *claims{id: $id, sensitivity: s}",
                    params,
                    ScriptMutability::Immutable,
                )
                .map_err(|e| {
                    Error::GraphStorage(format!("get_sensitivities_for_claims({id}) failed: {e}"))
                })?;
            if let Some(row) = result.rows.first() {
                out.insert(id.clone(), dv_to_string(&row[0]));
            }
        }
        Ok(out)
    }
}

/// A single connector ingest record returned by
/// [`GraphStore::lookup_connector_ingest`].
#[derive(Debug, Clone)]
pub struct ConnectorIngestRecord {
    pub connector_id: String,
    pub install_id: String,
    pub idempotency_key: String,
    pub claim_ids: Vec<String>,
    pub ingested_at: f64,
    /// Branch the ingest targeted, or `None` for main.
    pub branch: Option<String>,
    /// Synthetic source URI created for the ingest (matches the
    /// `mcp://agent/...` convention but with a connector identifier).
    pub source_uri: String,
}

/// An SVO event row returned from the `events` table.
#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub id: String,
    pub subject_entity_id: String,
    pub verb: String,
    pub object_entity_id: String,
    pub normalized_date: String,
    /// Human-readable subject name — resolved by the engine layer from the KG cache.
    /// Empty string if not yet resolved.
    pub subject_name: String,
    /// Human-readable object name — resolved by the engine layer from the KG cache.
    /// Empty string when there is no object entity or resolution failed.
    pub object_name: String,
}

/// A turn calendar row: one conversation turn and the claims contributed in it.
#[derive(Debug, Clone, Serialize)]
pub struct TurnRow {
    pub turn_number: u64,
    pub claim_ids: Vec<String>,
    pub timestamp: f64,
}

/// Per-structural-table `:rm` script string.  Each table's primary key
/// is encoded here because Cozo's `:rm` requires the PK projection,
/// Delegate to the canonical implementation in `thinkingroot_core::structural_registry`
/// so that `cascade_structural_tables_for_source` and the v3 migration GC sweep in
/// `thinkingroot_serve::backfill` share one authoritative copy of these scripts.
/// Any future PK-shape change is made once in `structural_registry.rs` and picked
/// up here automatically.
fn pk_rm_script_for_table(name: &str, sid_col: &str) -> String {
    thinkingroot_core::structural_registry::pk_rm_script_for_table(name, sid_col)
}

/// Extract a String from a DataValue.
fn dv_to_string(val: &DataValue) -> String {
    match val {
        DataValue::Str(s) => s.to_string(),
        DataValue::Num(Num::Int(i)) => i.to_string(),
        DataValue::Num(Num::Float(f)) => f.to_string(),
        DataValue::Null => String::new(),
        other => format!("{other:?}"),
    }
}

/// Extract an f64 from a DataValue — handles both Float and Int variants.
fn dv_to_float(val: &DataValue) -> f64 {
    match val {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

fn count_from_rows(rows: &[Vec<DataValue>]) -> usize {
    if let Some(row) = rows.first() {
        count_from_single(&row[0])
    } else {
        0
    }
}

/// Extract a non-negative integer count from a single DataValue. Handles
/// both Int and Float variants; negative values clamp to 0.
fn count_from_single(val: &DataValue) -> usize {
    match val {
        DataValue::Num(Num::Int(n)) => (*n).max(0) as usize,
        DataValue::Num(Num::Float(f)) => f.max(0.0) as usize,
        _ => 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_store() -> GraphStore {
        let db = DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore { db };
        store.create_schema().unwrap();
        store
    }

    #[test]
    fn init_and_counts() {
        let store = mem_store();
        let (s, c, e) = store.get_counts().unwrap();
        assert_eq!((s, c, e), (0, 0, 0));
    }

    #[test]
    fn insert_and_query_entity() {
        let store = mem_store();

        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str("e1".into()));
        params.insert("name".into(), DataValue::Str("Rust".into()));
        params.insert("etype".into(), DataValue::Str("Concept".into()));
        params.insert("desc".into(), DataValue::Str("A language".into()));

        store
            .query(
                r#"?[id, canonical_name, entity_type, description] <- [[$id, $name, $etype, $desc]]
                :put entities {id => canonical_name, entity_type, description}"#,
                params,
            )
            .unwrap();

        let entities = store.get_all_entities().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].1, "Rust");
    }

    #[test]
    fn link_and_query_claims_for_entity() {
        let store = mem_store();

        // Insert entity.
        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str("e1".into()));
        p.insert("name".into(), DataValue::Str("Rust".into()));
        p.insert("etype".into(), DataValue::Str("Concept".into()));
        p.insert("desc".into(), DataValue::Str("".into()));
        store
            .query(
                r#"?[id, canonical_name, entity_type, description] <- [[$id, $name, $etype, $desc]]
                :put entities {id => canonical_name, entity_type, description}"#,
                p,
            )
            .unwrap();

        // Insert claim.
        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str("c1".into()));
        p.insert("stmt".into(), DataValue::Str("Rust is fast".into()));
        p.insert("ct".into(), DataValue::Str("Fact".into()));
        p.insert("sid".into(), DataValue::Str("s1".into()));
        store
            .query(
                r#"?[id, statement, claim_type, source_id, confidence, sensitivity, workspace_id] <- [[
                    $id, $stmt, $ct, $sid, 0.8, 'Public', ''
                ]]
                :put claims {id => statement, claim_type, source_id, confidence, sensitivity, workspace_id}"#,
                p,
            )
            .unwrap();

        // Link claim → entity.
        store.link_claim_to_entity("c1", "e1").unwrap();

        // Query claims for entity.
        let claims = store.get_claims_for_entity("e1").unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].1, "Rust is fast");
    }

    #[test]
    fn remove_source_by_uri_cleans_derived_graph_state() {
        let store = mem_store();

        let source = thinkingroot_core::Source::new(
            "test://doc.md".into(),
            thinkingroot_core::types::SourceType::File,
        )
        .with_hash(thinkingroot_core::types::ContentHash("hash-1".into()));
        store.insert_source(&source).unwrap();

        let entity = thinkingroot_core::Entity::new(
            "PostgreSQL",
            thinkingroot_core::types::EntityType::Database,
        );
        store.insert_entity(&entity).unwrap();

        let claim = thinkingroot_core::Claim::new(
            "PostgreSQL stores transactions",
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            thinkingroot_core::types::WorkspaceId::new(),
        );
        store.insert_claim(&claim).unwrap();
        store
            .link_claim_to_source(&claim.id.to_string(), &source.id.to_string())
            .unwrap();
        store
            .link_claim_to_entity(&claim.id.to_string(), &entity.id.to_string())
            .unwrap();
        store
            .link_entities_for_source(
                &source.id.to_string(),
                &entity.id.to_string(),
                &entity.id.to_string(),
                "Uses",
                1.0,
            )
            .unwrap();
        store.rebuild_entity_relations().unwrap();
        store
            .insert_contradiction("cx1", &claim.id.to_string(), "other-claim", "conflict")
            .unwrap();
        store
            .supersede_claim(&claim.id.to_string(), "newer-claim")
            .unwrap();

        let removed = store.remove_source_by_uri("test://doc.md").unwrap();
        assert_eq!(removed, 1);
        store.rebuild_entity_relations().unwrap();

        let (sources, claims, entities) = store.get_counts().unwrap();
        assert_eq!((sources, claims, entities), (0, 0, 0));
        assert!(store.get_all_relations().unwrap().is_empty());
        assert!(store.get_contradictions().unwrap().is_empty());
        assert_eq!(store.count_superseded_claims().unwrap(), 0);
        assert!(
            store
                .find_sources_by_uri("test://doc.md")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn get_source_relation_triples_returns_triples_for_source() {
        let store = mem_store();

        store
            .link_entities_for_source("src-a", "e1", "e2", "Uses", 0.8)
            .unwrap();
        store
            .link_entities_for_source("src-a", "e1", "e3", "DependsOn", 0.7)
            .unwrap();
        store
            .link_entities_for_source("src-b", "e1", "e2", "Uses", 0.9)
            .unwrap();

        let triples = store.get_source_relation_triples("src-a").unwrap();
        assert_eq!(triples.len(), 2, "src-a contributes 2 triples");

        let triples_b = store.get_source_relation_triples("src-b").unwrap();
        assert_eq!(triples_b.len(), 1, "src-b contributes 1 triple");

        let empty = store.get_source_relation_triples("nonexistent").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn incremental_update_preserves_supported_triple_removes_unsupported() {
        let store = mem_store();

        // Create real entities so get_all_relations() JOIN works.
        let e1 =
            thinkingroot_core::Entity::new("Alpha", thinkingroot_core::types::EntityType::System);
        let e2 =
            thinkingroot_core::Entity::new("Beta", thinkingroot_core::types::EntityType::Service);
        let e3 =
            thinkingroot_core::Entity::new("Gamma", thinkingroot_core::types::EntityType::Database);
        store.insert_entity(&e1).unwrap();
        store.insert_entity(&e2).unwrap();
        store.insert_entity(&e3).unwrap();

        let eid1 = e1.id.to_string();
        let eid2 = e2.id.to_string();
        let eid3 = e3.id.to_string();

        let src_a = thinkingroot_core::Source::new(
            "test://a.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        let src_b = thinkingroot_core::Source::new(
            "test://b.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&src_a).unwrap();
        store.insert_source(&src_b).unwrap();

        let sid_a = src_a.id.to_string();
        let sid_b = src_b.id.to_string();

        // Source A: e1→Uses→e2 (0.8) and e1→DependsOn→e3 (0.7).
        // Source B: e1→Uses→e2 (0.9) — also contributes to first triple.
        store
            .link_entities_for_source(&sid_a, &eid1, &eid2, "Uses", 0.8)
            .unwrap();
        store
            .link_entities_for_source(&sid_a, &eid1, &eid3, "DependsOn", 0.7)
            .unwrap();
        store
            .link_entities_for_source(&sid_b, &eid1, &eid2, "Uses", 0.9)
            .unwrap();

        // Full rebuild to set initial entity_relations state.
        store.rebuild_entity_relations().unwrap();
        let before = store.get_all_relations().unwrap();
        assert_eq!(before.len(), 2, "two distinct relation triples");

        // Capture affected triples BEFORE removing source A.
        let affected = store.get_source_relation_triples(&sid_a).unwrap();
        assert_eq!(affected.len(), 2);

        // Remove source A (cascading cleanup removes its source_entity_relations).
        store.remove_source_by_uri("test://a.md").unwrap();

        // Incremental update — only re-aggregate affected triples.
        store
            .update_entity_relations_for_triples(&affected)
            .unwrap();

        let after = store.get_all_relations().unwrap();
        // e1→Uses→e2 should remain (src_b still has it at 0.9).
        // e1→DependsOn→e3 should be gone (src_a was the only contributor).
        assert_eq!(
            after.len(),
            1,
            "only the triple still supported by src-b should remain"
        );
    }

    #[test]
    fn incremental_update_recomputes_strength_noisy_or() {
        let store = mem_store();

        let e1 =
            thinkingroot_core::Entity::new("Svc1", thinkingroot_core::types::EntityType::Service);
        let e2 =
            thinkingroot_core::Entity::new("Svc2", thinkingroot_core::types::EntityType::Service);
        store.insert_entity(&e1).unwrap();
        store.insert_entity(&e2).unwrap();

        let eid1 = e1.id.to_string();
        let eid2 = e2.id.to_string();

        let src_a = thinkingroot_core::Source::new(
            "test://a.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        let src_b = thinkingroot_core::Source::new(
            "test://b.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&src_a).unwrap();
        store.insert_source(&src_b).unwrap();

        let sid_a = src_a.id.to_string();
        let sid_b = src_b.id.to_string();

        // Source A: strength 1.0 (highest). Source B: strength 0.5.
        // noisy-OR(1.0, 0.5) = 1 - (1-1.0)*(1-0.5) = 1 - 0 = 1.0
        store
            .link_entities_for_source(&sid_a, &eid1, &eid2, "Uses", 1.0)
            .unwrap();
        store
            .link_entities_for_source(&sid_b, &eid1, &eid2, "Uses", 0.5)
            .unwrap();

        store.rebuild_entity_relations().unwrap();
        let before = store.get_all_relations().unwrap();
        let (_, _, _, _, _, initial_strength) = before[0];
        assert_eq!(
            initial_strength, 1.0,
            "noisy-OR with a 1.0 source should give 1.0 initially"
        );

        // Capture triples, remove source A, re-add at lower strength.
        let affected = store.get_source_relation_triples(&sid_a).unwrap();
        store.remove_source_by_uri("test://a.md").unwrap();

        // Re-insert source A with lower strength (simulates file content change).
        store.insert_source(&src_a).unwrap();
        store
            .link_entities_for_source(&sid_a, &eid1, &eid2, "Uses", 0.3)
            .unwrap();

        // Incremental update should recompute noisy-OR(0.3, 0.5) = 1 - (1-0.3)*(1-0.5) = 1 - 0.35 = 0.65.
        store
            .update_entity_relations_for_triples(&affected)
            .unwrap();

        let after = store.get_all_relations().unwrap();
        assert_eq!(after.len(), 1);
        let (_, _, _, _, _, final_strength) = after[0];
        assert!(
            (final_strength - 0.65).abs() < 0.01,
            "noisy-OR(0.3, 0.5) should give ~0.65, got {final_strength}"
        );
    }

    #[test]
    fn get_entity_ids_for_source_returns_linked_entities() {
        let store = mem_store();

        let entity = thinkingroot_core::types::Entity::new(
            "Alpha",
            thinkingroot_core::types::EntityType::System,
        );
        store.insert_entity(&entity).unwrap();

        let source = thinkingroot_core::Source::new(
            "test://a.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&source).unwrap();

        let workspace = thinkingroot_core::types::WorkspaceId::new();
        let claim = thinkingroot_core::types::Claim::new(
            "Alpha is fast",
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            workspace,
        );
        store.insert_claim(&claim).unwrap();
        store
            .link_claim_to_source(&claim.id.to_string(), &source.id.to_string())
            .unwrap();
        store
            .link_claim_to_entity(&claim.id.to_string(), &entity.id.to_string())
            .unwrap();

        let entity_ids = store
            .get_entity_ids_for_source(&source.id.to_string())
            .unwrap();
        assert_eq!(entity_ids.len(), 1);
        assert_eq!(entity_ids[0], entity.id.to_string());

        // Different source: no claims linked → empty result.
        let source2 = thinkingroot_core::Source::new(
            "test://b.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&source2).unwrap();
        let entity_ids2 = store
            .get_entity_ids_for_source(&source2.id.to_string())
            .unwrap();
        assert!(entity_ids2.is_empty());
    }

    #[test]
    fn test_get_entity_names_for_claims() {
        let store = mem_store();

        let source = thinkingroot_core::Source::new(
            "test.md".to_string(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&source).unwrap();

        let workspace_id = thinkingroot_core::types::WorkspaceId::new();
        let entity = thinkingroot_core::types::Entity::new(
            "AuthService",
            thinkingroot_core::types::EntityType::Service,
        );
        store.insert_entity(&entity).unwrap();

        let claim = thinkingroot_core::types::Claim::new(
            "AuthService uses JWT",
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            workspace_id,
        );
        store.insert_claim(&claim).unwrap();
        store
            .link_claim_to_entity(&claim.id.to_string(), &entity.id.to_string())
            .unwrap();

        let claim_id_str = claim.id.to_string();
        let map = store
            .get_entity_names_for_claims(&[claim_id_str.as_str()])
            .unwrap();
        assert_eq!(
            map.get(&claim_id_str).unwrap(),
            &vec!["AuthService".to_string()]
        );

        // An unrelated claim_id should not appear in the map.
        let empty_map = store.get_entity_names_for_claims(&[]).unwrap();
        assert!(empty_map.is_empty());
    }

    #[test]
    fn test_find_entity_id_by_name() {
        let store = mem_store();

        let entity = thinkingroot_core::types::Entity::new(
            "MyService",
            thinkingroot_core::types::EntityType::Service,
        );
        store.insert_entity(&entity).unwrap();

        let found = store.find_entity_id_by_name("MyService").unwrap();
        assert_eq!(found, Some(entity.id.to_string()));

        let not_found = store.find_entity_id_by_name("NonExistent").unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn noisy_or_combines_multiple_sources_stronger_than_max() {
        let store = mem_store();

        let e1 = thinkingroot_core::Entity::new("A", thinkingroot_core::types::EntityType::Service);
        let e2 = thinkingroot_core::Entity::new("B", thinkingroot_core::types::EntityType::Service);
        store.insert_entity(&e1).unwrap();
        store.insert_entity(&e2).unwrap();

        let eid1 = e1.id.to_string();
        let eid2 = e2.id.to_string();

        let src_a = thinkingroot_core::Source::new(
            "test://a.rs".into(),
            thinkingroot_core::types::SourceType::File,
        );
        let src_b = thinkingroot_core::Source::new(
            "test://b.rs".into(),
            thinkingroot_core::types::SourceType::File,
        );
        let src_c = thinkingroot_core::Source::new(
            "test://c.rs".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&src_a).unwrap();
        store.insert_source(&src_b).unwrap();
        store.insert_source(&src_c).unwrap();

        // Three sources each with strength 0.5.
        // MAX would give 0.5.
        // Noisy-OR gives 1 - (1-0.5)^3 = 1 - 0.125 = 0.875.
        store
            .link_entities_for_source(&src_a.id.to_string(), &eid1, &eid2, "Uses", 0.5)
            .unwrap();
        store
            .link_entities_for_source(&src_b.id.to_string(), &eid1, &eid2, "Uses", 0.5)
            .unwrap();
        store
            .link_entities_for_source(&src_c.id.to_string(), &eid1, &eid2, "Uses", 0.5)
            .unwrap();

        // Trigger aggregation.
        let triples = vec![(eid1.clone(), eid2.clone(), "Uses".to_string())];
        store.update_entity_relations_for_triples(&triples).unwrap();

        let relations = store.get_all_relations().unwrap();
        assert_eq!(relations.len(), 1);
        let (_, _, _, _, _, strength) = &relations[0];
        // Must be greater than 0.5 (the max) — noisy-OR gives ~0.875
        assert!(
            *strength > 0.5,
            "noisy-OR with 3 sources of 0.5 should produce strength > 0.5, got {strength}"
        );
        assert!(
            (*strength - 0.875).abs() < 0.01,
            "expected ~0.875 from noisy-OR, got {strength}"
        );
    }

    #[test]
    fn get_all_triples_involving_entities_returns_cross_file_edges() {
        let store = mem_store();

        let e1 =
            thinkingroot_core::Entity::new("Alpha", thinkingroot_core::types::EntityType::Service);
        let e2 =
            thinkingroot_core::Entity::new("Beta", thinkingroot_core::types::EntityType::Service);
        let e3 =
            thinkingroot_core::Entity::new("Gamma", thinkingroot_core::types::EntityType::Database);
        store.insert_entity(&e1).unwrap();
        store.insert_entity(&e2).unwrap();
        store.insert_entity(&e3).unwrap();

        let eid1 = e1.id.to_string();
        let eid2 = e2.id.to_string();
        let eid3 = e3.id.to_string();

        let src_a = thinkingroot_core::Source::new(
            "a.rs".into(),
            thinkingroot_core::types::SourceType::File,
        );
        let src_b = thinkingroot_core::Source::new(
            "b.rs".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&src_a).unwrap();
        store.insert_source(&src_b).unwrap();

        store
            .link_entities_for_source(&src_a.id.to_string(), &eid1, &eid2, "Uses", 0.9)
            .unwrap();
        store
            .link_entities_for_source(&src_b.id.to_string(), &eid2, &eid3, "DependsOn", 0.8)
            .unwrap();
        store.rebuild_entity_relations().unwrap();

        // Query triples involving e1.
        let triples = store
            .get_all_triples_involving_entities(&[eid1.clone()])
            .unwrap();
        assert_eq!(triples.len(), 1);
        assert!(triples.iter().any(|(f, t, _)| f == &eid1 && t == &eid2));

        // Query triples involving e2 (appears in BOTH triples).
        let triples2 = store
            .get_all_triples_involving_entities(&[eid2.clone()])
            .unwrap();
        assert_eq!(
            triples2.len(),
            2,
            "e2 is in both triples (as target of first, source of second)"
        );

        // Empty input returns empty.
        let empty = store.get_all_triples_involving_entities(&[]).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn insert_and_get_claim_preserves_extraction_tier() {
        use thinkingroot_core::types::ExtractionTier;

        let store = mem_store();

        let source = thinkingroot_core::Source::new(
            "test://tier-roundtrip.rs".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&source).unwrap();

        let mut claim = thinkingroot_core::Claim::new(
            "compile is defined in example.rs",
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            thinkingroot_core::types::WorkspaceId::new(),
        );
        claim.extraction_tier = ExtractionTier::Structural;
        store.insert_claim(&claim).unwrap();

        let retrieved = store
            .get_claim_by_id(&claim.id.to_string())
            .unwrap()
            .expect("claim should be present after insert");
        assert_eq!(
            retrieved.extraction_tier,
            ExtractionTier::Structural,
            "extraction_tier must survive insert+get round-trip"
        );

        // Also verify ExtractionTier::Llm round-trips correctly
        let mut llm_claim = thinkingroot_core::Claim::new(
            "another fact",
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            thinkingroot_core::types::WorkspaceId::new(),
        );
        llm_claim.extraction_tier = ExtractionTier::Llm;
        store.insert_claim(&llm_claim).unwrap();
        let retrieved_llm = store
            .get_claim_by_id(&llm_claim.id.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            retrieved_llm.extraction_tier,
            ExtractionTier::Llm,
            "ExtractionTier::Llm must survive insert+get round-trip"
        );
    }

    // ─── Rooting migration tests ────────────────────────────────────────

    #[test]
    fn fresh_db_has_admission_tier_column() {
        // Fresh DBs go through create_schema, which includes the Rooting columns
        // natively. The migration probe should detect them and no-op.
        let store = mem_store();
        store.migrate_claims_extraction_tier().unwrap();

        // Insert a claim and read it back. The migration should have left
        // things consistent.
        let source = thinkingroot_core::Source::new(
            "test://doc.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&source).unwrap();

        let claim = thinkingroot_core::Claim::new(
            "a plain claim",
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            thinkingroot_core::types::WorkspaceId::new(),
        );
        store.insert_claim(&claim).unwrap();

        let retrieved = store
            .get_claim_by_id(&claim.id.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            retrieved.admission_tier,
            thinkingroot_core::types::AdmissionTier::Attested,
            "plain claim must default to Attested tier"
        );
        assert!(retrieved.derivation.is_none());
        assert!(retrieved.predicate.is_none());
        assert!(retrieved.last_rooted_at.is_none());
    }

    #[test]
    fn migration_is_idempotent() {
        // Running the migration path multiple times on a fresh DB must not fail,
        // must not change the schema, and must not lose data.
        let store = mem_store();
        for _ in 0..3 {
            store.migrate_claims_extraction_tier().unwrap();
        }

        let source = thinkingroot_core::Source::new(
            "test://repeat.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&source).unwrap();
        let claim = thinkingroot_core::Claim::new(
            "repeat test",
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            thinkingroot_core::types::WorkspaceId::new(),
        );
        store.insert_claim(&claim).unwrap();

        // Still readable after multiple migration calls.
        assert!(
            store
                .get_claim_by_id(&claim.id.to_string())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn insert_claim_preserves_rooting_fields_round_trip() {
        use thinkingroot_core::types::{
            AdmissionTier, DerivationProof, Predicate, PredicateLanguage, PredicateScope,
        };

        let store = mem_store();

        let source = thinkingroot_core::Source::new(
            "test://rooting.rs".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&source).unwrap();

        let parent_id = thinkingroot_core::ClaimId::new();
        let claim = thinkingroot_core::Claim::new(
            "derived claim",
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            thinkingroot_core::types::WorkspaceId::new(),
        )
        .with_admission_tier(AdmissionTier::Rooted)
        .with_derivation(DerivationProof {
            parent_claim_ids: vec![parent_id],
            derivation_rule: "test-rule".into(),
        })
        .with_predicate(Predicate {
            language: PredicateLanguage::Regex,
            query: r"fn\s+main".into(),
            scope: PredicateScope::from_globs(vec!["src/**/*.rs".into()]),
        })
        .with_last_rooted_at(chrono::Utc::now());

        store.insert_claim(&claim).unwrap();

        let round = store
            .get_claim_by_id(&claim.id.to_string())
            .unwrap()
            .unwrap();

        assert_eq!(round.admission_tier, AdmissionTier::Rooted);
        let derivation = round.derivation.expect("derivation round-tripped");
        assert_eq!(derivation.parent_claim_ids, vec![parent_id]);
        // derivation_rule is not persisted in the current schema (only parent IDs
        // are stored in derivation_parents); this is by design for v1.
        let predicate = round.predicate.expect("predicate round-tripped");
        assert_eq!(predicate.language, PredicateLanguage::Regex);
        assert_eq!(predicate.query, r"fn\s+main");
        assert_eq!(predicate.scope.globs.len(), 1);
        assert!(round.last_rooted_at.is_some());
    }

    #[test]
    fn count_claims_by_admission_tier_groups_correctly() {
        use thinkingroot_core::types::AdmissionTier;

        let store = mem_store();
        let source = thinkingroot_core::Source::new(
            "test://count.md".into(),
            thinkingroot_core::types::SourceType::File,
        );
        store.insert_source(&source).unwrap();

        let make = |tier: AdmissionTier, label: &str| {
            let c = thinkingroot_core::Claim::new(
                label,
                thinkingroot_core::types::ClaimType::Fact,
                source.id,
                thinkingroot_core::types::WorkspaceId::new(),
            )
            .with_admission_tier(tier);
            store.insert_claim(&c).unwrap();
        };

        make(AdmissionTier::Rooted, "r1");
        make(AdmissionTier::Rooted, "r2");
        make(AdmissionTier::Attested, "a1");
        make(AdmissionTier::Quarantined, "q1");
        make(AdmissionTier::Quarantined, "q2");
        make(AdmissionTier::Quarantined, "q3");
        make(AdmissionTier::Rejected, "x1");

        let (rooted, attested, quarantined, rejected) =
            store.count_claims_by_admission_tier().unwrap();
        assert_eq!(rooted, 2);
        assert_eq!(attested, 1);
        assert_eq!(quarantined, 3);
        assert_eq!(rejected, 1);
    }

    #[test]
    fn rooting_relations_exist_on_fresh_db() {
        // Fresh DB must have trial_verdicts, verification_certificates, and
        // derivation_edges available for insert/query (no errors).
        let store = mem_store();

        // Trial verdict insert.
        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str("v1".into()));
        p.insert("claim_id".into(), DataValue::Str("c1".into()));
        p.insert("trial_at".into(), DataValue::Num(Num::Float(0.0)));
        p.insert("admission_tier".into(), DataValue::Str("rooted".into()));
        p.insert("provenance_score".into(), DataValue::Num(Num::Float(1.0)));
        p.insert(
            "contradiction_score".into(),
            DataValue::Num(Num::Float(1.0)),
        );
        p.insert("predicate_score".into(), DataValue::Num(Num::Float(1.0)));
        p.insert("topology_score".into(), DataValue::Num(Num::Float(1.0)));
        p.insert("temporal_score".into(), DataValue::Num(Num::Float(1.0)));
        p.insert("certificate_hash".into(), DataValue::Str("abc".into()));
        p.insert("failure_reason".into(), DataValue::Str("".into()));
        p.insert("rooter_version".into(), DataValue::Str("0.9.0".into()));
        store
            .query(
                r#"?[id, claim_id, trial_at, admission_tier, provenance_score, contradiction_score, predicate_score, topology_score, temporal_score, certificate_hash, failure_reason, rooter_version] <- [[
                    $id, $claim_id, $trial_at, $admission_tier, $provenance_score, $contradiction_score, $predicate_score, $topology_score, $temporal_score, $certificate_hash, $failure_reason, $rooter_version
                ]]
                :put trial_verdicts {id => claim_id, trial_at, admission_tier, provenance_score, contradiction_score, predicate_score, topology_score, temporal_score, certificate_hash, failure_reason, rooter_version}"#,
                p,
            )
            .unwrap();

        // Certificate insert.
        let mut p = BTreeMap::new();
        p.insert("hash".into(), DataValue::Str("abc".into()));
        p.insert("claim_id".into(), DataValue::Str("c1".into()));
        p.insert("created_at".into(), DataValue::Num(Num::Float(0.0)));
        p.insert("inputs".into(), DataValue::Str("{}".into()));
        p.insert("outputs".into(), DataValue::Str("{}".into()));
        p.insert("version".into(), DataValue::Str("0.9.0".into()));
        p.insert("source_hash".into(), DataValue::Str("h".into()));
        store
            .query(
                r#"?[hash, claim_id, created_at, probe_inputs_json, probe_outputs_json, rooter_version, source_content_hash] <- [[
                    $hash, $claim_id, $created_at, $inputs, $outputs, $version, $source_hash
                ]]
                :put verification_certificates {hash => claim_id, created_at, probe_inputs_json, probe_outputs_json, rooter_version, source_content_hash}"#,
                p,
            )
            .unwrap();

        // Derivation edge insert.
        let mut p = BTreeMap::new();
        p.insert("parent".into(), DataValue::Str("p1".into()));
        p.insert("child".into(), DataValue::Str("c1".into()));
        p.insert("rule".into(), DataValue::Str("test".into()));
        store
            .query(
                r#"?[parent_claim_id, child_claim_id, derivation_rule] <- [[$parent, $child, $rule]]
                :put derivation_edges {parent_claim_id, child_claim_id => derivation_rule}"#,
                p,
            )
            .unwrap();
    }

    #[test]
    fn insert_claim_populates_source_path_from_sources_table() {
        // Regression for C2: pre-fix every claim landed with
        // source_path = "" because insert_claim hardcoded an empty
        // string instead of resolving from the sources table.
        use thinkingroot_core::types::{Claim, ClaimType, Source, SourceType, WorkspaceId};
        let store = mem_store();
        let src = Source::new("file:///tmp/foo.rs".to_string(), SourceType::File);
        let src_id = src.id;
        store.insert_source(&src).unwrap();

        let claim = Claim::new(
            "foo claims bar",
            ClaimType::Fact,
            src_id,
            WorkspaceId::new(),
        );
        let claim_id = claim.id;
        store.insert_claim(&claim).unwrap();

        let written = store.get_claim_source_path(&claim_id.to_string()).unwrap();
        assert_eq!(
            written, "file:///tmp/foo.rs",
            "single-row insert must populate source_path from sources, got {written:?}"
        );
    }

    #[test]
    fn insert_claims_batch_populates_source_path_from_sources_table() {
        // Regression for C2: pre-fix the batch path (used by Linker on
        // every compile) hardcoded source_path = "" for every row.
        use thinkingroot_core::types::{Claim, ClaimType, Source, SourceType, WorkspaceId};
        let store = mem_store();

        let src_a = Source::new("file:///abs/a.rs".to_string(), SourceType::File);
        let src_b = Source::new("file:///abs/b.rs".to_string(), SourceType::File);
        store.insert_source(&src_a).unwrap();
        store.insert_source(&src_b).unwrap();
        let ws = WorkspaceId::new();

        let claims = vec![
            Claim::new("alpha", ClaimType::Fact, src_a.id, ws),
            Claim::new("beta", ClaimType::Fact, src_b.id, ws),
            Claim::new("gamma", ClaimType::Fact, src_a.id, ws),
        ];
        let ids: Vec<String> = claims.iter().map(|c| c.id.to_string()).collect();
        store.insert_claims_batch(&claims).unwrap();

        let p0 = store.get_claim_source_path(&ids[0]).unwrap();
        let p1 = store.get_claim_source_path(&ids[1]).unwrap();
        let p2 = store.get_claim_source_path(&ids[2]).unwrap();
        assert_eq!(p0, "file:///abs/a.rs", "claim[0] source_path");
        assert_eq!(p1, "file:///abs/b.rs", "claim[1] source_path");
        assert_eq!(p2, "file:///abs/a.rs", "claim[2] source_path");
    }

    #[test]
    fn insert_claims_batch_with_missing_source_falls_back_to_empty() {
        // If the sources row hasn't been inserted yet (a misuse pattern
        // outside the v3 pipeline order), the column lands empty rather
        // than the batch failing.  The pipeline's contract is to insert
        // sources in Phase 6 before claims in Phase 7, so this branch is
        // a defensive fallback, not a hot path.
        use thinkingroot_core::types::{Claim, ClaimType, SourceId, WorkspaceId};
        let store = mem_store();
        let synthetic_source = SourceId::new(); // never inserted into sources
        let claims = vec![Claim::new(
            "ghost",
            ClaimType::Fact,
            synthetic_source,
            WorkspaceId::new(),
        )];
        let id = claims[0].id.to_string();
        store.insert_claims_batch(&claims).unwrap();
        assert_eq!(
            store.get_claim_source_path(&id).unwrap(),
            "",
            "missing source_id must produce empty source_path, not an error"
        );
    }

    #[test]
    fn fetch_source_uris_returns_known_only() {
        use thinkingroot_core::types::{Source, SourceType};
        let store = mem_store();
        let a = Source::new("file:///x/a.rs".to_string(), SourceType::File);
        let b = Source::new("file:///x/b.rs".to_string(), SourceType::File);
        store.insert_source(&a).unwrap();
        store.insert_source(&b).unwrap();

        let ids = vec![a.id.to_string(), b.id.to_string(), "ghost".to_string()];
        let map = store.fetch_source_uris(&ids).unwrap();
        assert_eq!(
            map.get(&a.id.to_string()).map(String::as_str),
            Some("file:///x/a.rs")
        );
        assert_eq!(
            map.get(&b.id.to_string()).map(String::as_str),
            Some("file:///x/b.rs")
        );
        assert!(
            !map.contains_key("ghost"),
            "unknown ids must not appear in result"
        );
    }

    #[test]
    fn insert_events_round_trip() {
        // Regression: pre-fix `insert_events` used the unsupported
        // `:put events { id => subject_entity_id: subj, ... }` rename
        // syntax and every call returned a CozoDB script error.  The
        // pipeline swallowed the error as a warning so the events
        // table sat empty in production.  This test exercises the
        // full round trip — insert two events, query them by time
        // range and by subject — to make sure the fix lands and
        // future regressions in the events relation surface in CI.
        use thinkingroot_core::types::ExtractedEvent;
        let mut store = mem_store();

        let events = vec![
            ExtractedEvent {
                id: "ev-1".to_string(),
                subject_entity_id: "alice".to_string(),
                verb: "visited".to_string(),
                object_entity_id: "paris".to_string(),
                timestamp: 1_700_000_000.0,
                normalized_date: "2023-11-14".to_string(),
                source_id: "src-1".to_string(),
                confidence: 0.9,
            },
            ExtractedEvent {
                id: "ev-2".to_string(),
                subject_entity_id: "bob".to_string(),
                verb: "decided".to_string(),
                object_entity_id: "".to_string(),
                timestamp: 1_700_001_000.0,
                normalized_date: "2023-11-14".to_string(),
                source_id: "src-1".to_string(),
                confidence: 0.7,
            },
        ];

        let inserted = store.insert_events(&events).unwrap();
        assert_eq!(inserted, 2, "both events must persist");

        let in_range = store
            .query_events_in_range(1_699_999_999.0, 1_700_002_000.0)
            .unwrap();
        assert_eq!(in_range.len(), 2);

        let alice_events = store.query_events_for_entity("alice").unwrap();
        assert_eq!(alice_events.len(), 1);
        assert_eq!(alice_events[0].verb, "visited");
        assert_eq!(alice_events[0].object_entity_id, "paris");
        assert_eq!(alice_events[0].normalized_date, "2023-11-14");

        // Re-inserting the same event id is an upsert (`:put` semantics).
        let again = store.insert_events(&events[..1]).unwrap();
        assert_eq!(again, 1);
        let still_two = store
            .query_events_in_range(1_699_999_999.0, 1_700_002_000.0)
            .unwrap();
        assert_eq!(
            still_two.len(),
            2,
            "upsert must not create duplicates on repeat insert"
        );
    }

    #[test]
    fn remove_source_cascades_to_events() {
        // Regression: `remove_source_by_id` cascades through claims,
        // claim_*_edges, contradictions, source_entity_relations,
        // sources, and orphan entities — but pre-fix it never
        // touched the `events` relation.  Once `insert_events` was
        // repaired (see `insert_events_round_trip`), every file
        // delete or rename leaked orphan event rows pointing at
        // entity ULIDs the orphan-entity sweep had already GCd, and
        // `query_events_in_range` / `query_events_for_entity` would
        // emit those orphans straight to the calendar UI.
        //
        // This test inserts a source + an event tagged with that
        // source, deletes the source by URI, and asserts every
        // event read path returns empty.
        use thinkingroot_core::types::ExtractedEvent;
        let mut store = mem_store();

        let source = thinkingroot_core::Source::new(
            "test://alice.md".into(),
            thinkingroot_core::types::SourceType::File,
        )
        .with_hash(thinkingroot_core::types::ContentHash("hash-events".into()));
        let source_id = source.id.to_string();
        store.insert_source(&source).unwrap();

        store
            .insert_events(&[
                ExtractedEvent {
                    id: "ev-cascade-1".into(),
                    subject_entity_id: "alice-ulid".into(),
                    verb: "visited".into(),
                    object_entity_id: "paris-ulid".into(),
                    timestamp: 1_700_000_000.0,
                    normalized_date: "2023-11-14".into(),
                    source_id: source_id.clone(),
                    confidence: 0.9,
                },
                ExtractedEvent {
                    id: "ev-cascade-2".into(),
                    subject_entity_id: "bob-ulid".into(),
                    verb: "decided".into(),
                    object_entity_id: "".into(),
                    timestamp: 1_700_001_000.0,
                    normalized_date: "2023-11-14".into(),
                    source_id: source_id.clone(),
                    confidence: 0.7,
                },
            ])
            .unwrap();
        // A second source with its own event — must survive the cascade.
        let other = thinkingroot_core::Source::new(
            "test://bob.md".into(),
            thinkingroot_core::types::SourceType::File,
        )
        .with_hash(thinkingroot_core::types::ContentHash("hash-other".into()));
        let other_id = other.id.to_string();
        store.insert_source(&other).unwrap();
        store
            .insert_events(&[ExtractedEvent {
                id: "ev-other".into(),
                subject_entity_id: "carol-ulid".into(),
                verb: "wrote".into(),
                object_entity_id: "".into(),
                timestamp: 1_700_500_000.0,
                normalized_date: "2023-11-20".into(),
                source_id: other_id,
                confidence: 0.8,
            }])
            .unwrap();

        let before = store
            .query_events_in_range(1_699_999_999.0, 1_701_000_000.0)
            .unwrap();
        assert_eq!(before.len(), 3, "preconditions: all three events present");

        let removed = store.remove_source_by_uri("test://alice.md").unwrap();
        assert_eq!(removed, 1);

        let after = store
            .query_events_in_range(1_699_999_999.0, 1_701_000_000.0)
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "events tied to the removed source must cascade out; \
             only the unrelated source's event must remain"
        );
        assert_eq!(after[0].id, "ev-other");

        // Per-entity read path agrees with the range read path.
        assert!(
            store.query_events_for_entity("alice-ulid").unwrap().is_empty(),
            "subject lookup for the removed source's entity must be empty"
        );
        assert!(
            store.query_events_for_entity("bob-ulid").unwrap().is_empty(),
            "subject lookup for the removed source's other entity must be empty"
        );
        assert_eq!(
            store
                .query_events_for_entity("carol-ulid")
                .unwrap()
                .len(),
            1,
            "the unrelated source's entity event must still be findable"
        );
    }

    /// Per-relation row counter for the cascade regression tests
    /// below.  Each branch picks an existing key column and asks
    /// CozoDB for a count — equivalent to `SELECT COUNT(*)` against
    /// each table.
    fn count_rows_in(store: &GraphStore, relation: &str) -> usize {
        let rows = match relation {
            "trial_verdicts" => store
                .db
                .run_script(
                    "?[count(id)] := *trial_verdicts{id}",
                    BTreeMap::new(),
                    ScriptMutability::Immutable,
                )
                .unwrap(),
            "verification_certificates" => store
                .db
                .run_script(
                    "?[count(hash)] := *verification_certificates{hash}",
                    BTreeMap::new(),
                    ScriptMutability::Immutable,
                )
                .unwrap(),
            "derivation_edges" => store
                .db
                .run_script(
                    "?[count(parent_claim_id)] := *derivation_edges{parent_claim_id, child_claim_id}",
                    BTreeMap::new(),
                    ScriptMutability::Immutable,
                )
                .unwrap(),
            "known_unknowns" => store
                .db
                .run_script(
                    "?[count(id)] := *known_unknowns{id}",
                    BTreeMap::new(),
                    ScriptMutability::Immutable,
                )
                .unwrap(),
            other => panic!("count_rows_in: unsupported relation `{other}`"),
        };
        count_from_rows(&rows.rows) as usize
    }

    /// Seed one source-bound claim and link it via
    /// `claim_source_edges` so `remove_source_by_uri` will pick it
    /// up.  Returns `(claim_id_string, source_id_string)`.
    fn seed_claim_with_source(
        store: &GraphStore,
        source_uri: &str,
        suffix: &str,
    ) -> (String, String) {
        let source = thinkingroot_core::Source::new(
            source_uri.into(),
            thinkingroot_core::types::SourceType::File,
        )
        .with_hash(thinkingroot_core::types::ContentHash(format!("hash-{suffix}")));
        let source_id = source.id.to_string();
        store.insert_source(&source).unwrap();

        let claim = thinkingroot_core::Claim::new(
            format!("statement for {suffix}"),
            thinkingroot_core::types::ClaimType::Fact,
            source.id,
            thinkingroot_core::types::WorkspaceId::new(),
        );
        let claim_id = claim.id.to_string();
        store.insert_claim(&claim).unwrap();
        store.link_claim_to_source(&claim_id, &source_id).unwrap();
        (claim_id, source_id)
    }

    #[test]
    fn remove_source_cascades_to_trial_verdicts() {
        let store = mem_store();
        let (claim_alice, _) = seed_claim_with_source(&store, "test://alice-tv.md", "tv-a");
        let (claim_bob, _) = seed_claim_with_source(&store, "test://bob-tv.md", "tv-b");

        // Insert one verdict per claim.  Tuple shape locked by
        // `insert_trial_verdicts_batch`: (id, claim_id, trial_at,
        // admission_tier, provenance, contradiction, predicate,
        // topology, temporal, certificate_hash, failure_reason,
        // rooter_version).
        let verdicts = vec![
            (
                "v-a".to_string(),
                claim_alice.clone(),
                1_700_000_000.0_f64,
                "rooted".to_string(),
                0.9_f64,
                0.0_f64,
                -1.0_f64,
                -1.0_f64,
                -1.0_f64,
                String::new(),
                String::new(),
                "test".to_string(),
            ),
            (
                "v-b".to_string(),
                claim_bob.clone(),
                1_700_000_001.0_f64,
                "attested".to_string(),
                0.5_f64,
                0.0_f64,
                -1.0_f64,
                -1.0_f64,
                -1.0_f64,
                String::new(),
                String::new(),
                "test".to_string(),
            ),
        ];
        store.insert_trial_verdicts_batch(&verdicts).unwrap();
        assert_eq!(count_rows_in(&store, "trial_verdicts"), 2);

        store.remove_source_by_uri("test://alice-tv.md").unwrap();
        assert_eq!(
            count_rows_in(&store, "trial_verdicts"),
            1,
            "verdict for the removed source's claim must cascade out"
        );
    }

    #[test]
    fn remove_source_cascades_to_derivation_edges() {
        let store = mem_store();
        let (parent_id, _) = seed_claim_with_source(&store, "test://alice-de.md", "de-p");
        let (child_id, _) = seed_claim_with_source(&store, "test://bob-de.md", "de-c");

        // Insert one derivation edge parent -> child.
        let mut params = BTreeMap::new();
        params.insert("p".into(), DataValue::Str(parent_id.clone().into()));
        params.insert("c".into(), DataValue::Str(child_id.clone().into()));
        store
            .query(
                r#"?[parent_claim_id, child_claim_id, derivation_rule] <- [[$p, $c, 'unit-test']]
                :put derivation_edges {parent_claim_id, child_claim_id => derivation_rule}"#,
                params,
            )
            .unwrap();
        assert_eq!(count_rows_in(&store, "derivation_edges"), 1);

        store.remove_source_by_uri("test://alice-de.md").unwrap();
        assert_eq!(
            count_rows_in(&store, "derivation_edges"),
            0,
            "edge naming the removed source's claim as parent must cascade"
        );

        // Re-create with the surviving claim as child to exercise
        // the child-side path.
        let (parent2_id, _) =
            seed_claim_with_source(&store, "test://alice-de2.md", "de-p2");
        let mut params = BTreeMap::new();
        params.insert("p".into(), DataValue::Str(parent2_id.into()));
        params.insert("c".into(), DataValue::Str(child_id.clone().into()));
        store
            .query(
                r#"?[parent_claim_id, child_claim_id, derivation_rule] <- [[$p, $c, 'unit-test']]
                :put derivation_edges {parent_claim_id, child_claim_id => derivation_rule}"#,
                params,
            )
            .unwrap();
        store.remove_source_by_uri("test://bob-de.md").unwrap();
        assert_eq!(
            count_rows_in(&store, "derivation_edges"),
            0,
            "edge naming the removed source's claim as child must cascade"
        );
    }

    #[test]
    fn remove_source_cascades_to_verification_certificates() {
        let store = mem_store();
        let (claim_id, _) =
            seed_claim_with_source(&store, "test://alice-cert.md", "cert-1");

        let mut params = BTreeMap::new();
        params.insert("h".into(), DataValue::Str("cert-hash-aaa".into()));
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        store
            .query(
                r#"?[hash, claim_id, created_at] <- [[$h, $cid, 1700000000.0]]
                :put verification_certificates {hash => claim_id, created_at}"#,
                params,
            )
            .unwrap();
        assert_eq!(count_rows_in(&store, "verification_certificates"), 1);

        store.remove_source_by_uri("test://alice-cert.md").unwrap();
        assert_eq!(
            count_rows_in(&store, "verification_certificates"),
            0,
            "certificate for the removed source's claim must cascade"
        );
    }

    #[test]
    fn remove_entity_cascades_to_known_unknowns() {
        let store = mem_store();

        // Seed two entities and a known_unknown for each.
        for (eid, gap_id) in [("ent-aa", "gap-aa"), ("ent-bb", "gap-bb")] {
            let mut params = BTreeMap::new();
            params.insert("eid".into(), DataValue::Str(eid.into()));
            store
                .query(
                    r#"?[id, canonical_name, entity_type] <- [[$eid, 'name', 'kind']]
                    :put entities {id => canonical_name, entity_type}"#,
                    params,
                )
                .unwrap();

            let mut params = BTreeMap::new();
            params.insert("gid".into(), DataValue::Str(gap_id.into()));
            params.insert("eid".into(), DataValue::Str(eid.into()));
            store
                .query(
                    r#"?[id, entity_id, pattern_id, expected_claim_type] <- [[$gid, $eid, 'pat-1', 'fact']]
                    :put known_unknowns {id => entity_id, pattern_id, expected_claim_type}"#,
                    params,
                )
                .unwrap();
        }
        assert_eq!(count_rows_in(&store, "known_unknowns"), 2);

        // Wire `ent-aa` to a claim through claim_entity_edges so
        // the orphan sweep in `remove_source_by_id` will GC it.
        let (claim_id, _) =
            seed_claim_with_source(&store, "test://alice-ku.md", "ku-1");
        let mut params = BTreeMap::new();
        params.insert("cid".into(), DataValue::Str(claim_id.into()));
        params.insert("eid".into(), DataValue::Str("ent-aa".into()));
        store
            .query(
                r#"?[claim_id, entity_id] <- [[$cid, $eid]]
                :put claim_entity_edges {claim_id, entity_id}"#,
                params,
            )
            .unwrap();

        store.remove_source_by_uri("test://alice-ku.md").unwrap();
        assert_eq!(
            count_rows_in(&store, "known_unknowns"),
            1,
            "gap row for the GC'd entity must cascade; the unrelated entity's row remains"
        );
    }

    /// AEP §7.3 verification: confirm the three Datalog rules in the
    /// Active Engram Protocol parse and execute against the actual
    /// CozoDB schema.  Verifies (a) the rules are syntactically valid
    /// Cozo, (b) field names match the live `claims` /
    /// `claim_temporal` / `known_unknowns` schemas, and (c) `now()`
    /// + `or` + `in <list>` + `!=` behave as the spec assumes.
    #[test]
    fn aep_datalog_rules_parse_and_execute() {
        let store = mem_store();

        // Seed: 4 claims spanning every admission_tier, plus matching
        // temporal + known_unknowns rows so the rules return non-trivial
        // result sets.
        let seed = r#"
            ?[id, statement, claim_type, source_id, admission_tier] <- [
                ['c-rooted',      'rooted claim',      'configuration', 's1', 'rooted'],
                ['c-attested',    'attested claim',    'configuration', 's1', 'attested'],
                ['c-quarantined', 'quarantined claim', 'configuration', 's1', 'quarantined'],
                ['c-rejected',    'rejected claim',    'configuration', 's1', 'rejected']
            ]
            :put claims {id => statement, claim_type, source_id, admission_tier}
        "#;
        store
            .db
            .run_default(seed)
            .expect("seed claims must succeed");

        // Temporal rows: one never-expires (valid_until = 0.0), one
        // far-future, one already-expired.
        let seed_temporal = r#"
            ?[claim_id, valid_from, valid_until, superseded_by] <- [
                ['c-rooted',      0.0, 0.0,          ''],
                ['c-attested',    0.0, 99999999999.0, ''],
                ['c-quarantined', 0.0, 1.0,           '']
            ]
            :put claim_temporal {claim_id => valid_from, valid_until, superseded_by}
        "#;
        store
            .db
            .run_default(seed_temporal)
            .expect("seed claim_temporal must succeed");

        // Two open known_unknowns + one closed.
        let seed_gaps = r#"
            ?[id, entity_id, pattern_id, expected_claim_type, confidence, status] <- [
                ['g1', 'e-auth', 'pat-a', 'rate_limiting_policy', 0.9, 'open'],
                ['g2', 'e-auth', 'pat-b', 'session_expiry',       0.8, 'open'],
                ['g3', 'e-db',   'pat-c', 'backup_policy',        0.7, 'closed']
            ]
            :put known_unknowns {id => entity_id, pattern_id,
                                 expected_claim_type, confidence, status}
        "#;
        store
            .db
            .run_default(seed_gaps)
            .expect("seed known_unknowns must succeed");

        // Also link c-rooted to entity e-auth so the §4 Pillar III
        // multi-relation join + `in <set>` filter has a row to return.
        store.link_claim_to_entity("c-rooted", "e-auth").unwrap();
        store
            .link_claim_to_entity("c-quarantined", "e-auth")
            .unwrap();

        // ── Rule 1: rule_trust_gate ────────────────────────────────
        // AEP spec verbatim, with the head renamed to `?` for the
        // standalone-query form.
        let r1 = store
            .db
            .run_default(
                r#"?[id] := *claims{id, admission_tier},
                            admission_tier != 'quarantined',
                            admission_tier != 'rejected'"#,
            )
            .expect("rule_trust_gate must parse and execute");
        let ids: Vec<String> = r1.rows.iter().map(|r| dv_to_string(&r[0])).collect();
        assert_eq!(
            ids.len(),
            2,
            "trust_gate must drop quarantined+rejected, keep rooted+attested"
        );
        assert!(ids.contains(&"c-rooted".to_string()));
        assert!(ids.contains(&"c-attested".to_string()));

        // ── Rule 2: rule_temporal_collapse ─────────────────────────
        // AEP spec verbatim: `valid_until = 0.0 or valid_until > now()`.
        let r2 = store
            .db
            .run_default(
                r#"?[id] := *claim_temporal{claim_id: id, valid_until},
                            valid_until = 0.0 or valid_until > now()"#,
            )
            .expect("rule_temporal_collapse must parse and execute");
        let ids: Vec<String> = r2.rows.iter().map(|r| dv_to_string(&r[0])).collect();
        assert_eq!(
            ids.len(),
            2,
            "temporal_collapse must keep never-expire+future, drop expired"
        );
        assert!(ids.contains(&"c-rooted".to_string()));
        assert!(ids.contains(&"c-attested".to_string()));

        // ── Rule 3: rule_gap_scan ──────────────────────────────────
        // AEP spec verbatim — note the inline-bind `status: 'open'`.
        let r3 = store
            .db
            .run_default(
                r#"?[entity_id, expected_claim_type, confidence] :=
                    *known_unknowns{entity_id, expected_claim_type,
                                    confidence, status: 'open'}"#,
            )
            .expect("rule_gap_scan must parse and execute");
        assert_eq!(
            r3.rows.len(),
            2,
            "gap_scan must return only open gaps (g1+g2), not closed g3"
        );

        // ── §4 Pillar III combined: multi-relation join + `in <set>` ──
        // The spec writes `entity_id in engram_0x7F9A_entity_set` —
        // verify the same shape using a Rust-side parameter `$set`
        // populated with the cluster.  This is the only rewrite
        // needed: Cozo `in` requires a list-typed expression on the
        // RHS, so the symbolic name in the spec resolves to a
        // parameter at execution time.
        let mut p = BTreeMap::new();
        p.insert(
            "set".into(),
            DataValue::List(vec![DataValue::Str("e-auth".into())]),
        );
        let r4 = store
            .db
            .run_script(
                r#"?[statement, confidence] :=
                    *claims{id, statement, confidence, admission_tier, claim_type},
                    *claim_entity_edges{claim_id: id, entity_id},
                    entity_id in $set,
                    claim_type = 'configuration',
                    admission_tier != 'quarantined',
                    admission_tier != 'rejected'"#,
                p,
                ScriptMutability::Immutable,
            )
            .expect("Pillar III combined rule must parse and execute");
        assert_eq!(
            r4.rows.len(),
            1,
            "combined rule must keep c-rooted (edge to e-auth, configuration, rooted) and drop c-quarantined"
        );
        assert_eq!(dv_to_string(&r4.rows[0][0]), "rooted claim");
    }
}
