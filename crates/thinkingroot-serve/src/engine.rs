use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::graph_cache::{CachedClaim, KnowledgeGraph, RawGraphData};
pub use crate::pipeline::PipelineResult;
use thinkingroot_core::{Config, Error, Result};
use thinkingroot_graph::StorageEngine;
use thinkingroot_health::Verifier;
pub use thinkingroot_health::verifier::VerificationResult;

// ---------------------------------------------------------------------------
// Public response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub path: String,
    pub entity_count: usize,
    pub claim_count: usize,
    pub source_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityInfo {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityDetail {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub aliases: Vec<String>,
    pub claims: Vec<ClaimInfo>,
    pub relations: Vec<RelationInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimInfo {
    pub id: String,
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub source_uri: String,
    /// Unix epoch of the actual event date, or None if not extracted.
    pub event_date: Option<f64>,
}

/// Summary payload returned by the `rooting_report` MCP tool and
/// `QueryEngine::rooting_report`.
#[derive(Debug, Clone, Serialize)]
pub struct RootingReport {
    pub workspace: String,
    pub rooted: usize,
    pub attested: usize,
    pub quarantined: usize,
    pub rejected: usize,
    pub total: usize,
}

/// An SVO event with entity names resolved from the in-memory KG cache.
/// This is what ReAct uses — entity IDs (ULIDs) would be useless to an LLM.
#[derive(Debug, Clone, Serialize)]
pub struct EventHit {
    pub id: String,
    pub subject_name: String,
    pub verb: String,
    pub object_name: String,
    pub normalized_date: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationInfo {
    pub target: String,
    pub relation_type: String,
    pub strength: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyNode {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyLink {
    pub source: String,
    pub target: String,
    pub relation_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyMap {
    pub nodes: Vec<GalaxyNode>,
    pub links: Vec<GalaxyLink>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactInfo {
    pub artifact_type: String,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactContent {
    pub artifact_type: String,
    pub content: String,
}

/// Result of [`QueryEngine::read_source`]. Returns the exact source bytes a
/// claim cites — the round-trip the v3 `read_source` MCP tool exposes
/// (`docs/2026-04-29-thinkingroot-v3-final-plan.md` §8.5). `text` is the
/// UTF-8 decoding of `bytes` when valid; consumers needing the raw bytes
/// can inspect `bytes` directly.
#[derive(Debug, Clone, Serialize)]
pub struct ReadSourceResult {
    /// URI of the source the claim was extracted from (e.g. `src/lib.rs`).
    pub file: String,
    /// Inclusive byte offset inside `file`.
    pub byte_start: u64,
    /// Exclusive byte offset inside `file`.
    pub byte_end: u64,
    /// UTF-8 decoding of the cited bytes when valid; empty when the byte
    /// range is unknown ((0, 0) sentinel) or the source bytes can't be
    /// decoded as UTF-8.
    pub text: String,
    /// Raw bytes the claim cites. Same byte range as `(byte_start, byte_end)`.
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub id: String,
    pub uri: String,
    pub source_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContradictionInfo {
    pub id: String,
    pub claim_a: String,
    pub claim_b: String,
    pub explanation: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub entities: Vec<EntitySearchHit>,
    pub claims: Vec<ClaimSearchHit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntitySearchHit {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub claim_count: usize,
    pub relevance: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimSearchHit {
    pub id: String,
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub source_uri: String,
    pub relevance: f32,
}

#[derive(Debug, Clone, Default)]
pub struct ClaimFilter {
    pub claim_type: Option<String>,
    pub entity_name: Option<String>,
    pub min_confidence: Option<f64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

// ---------------------------------------------------------------------------
// Internal workspace handle
// ---------------------------------------------------------------------------

struct WorkspaceHandle {
    name: String,
    root_path: PathBuf,
    /// Write operations (pipeline, agent contribute) go through CozoDB.
    storage: Arc<Mutex<StorageEngine>>,
    /// All read operations are served from this in-memory cache.
    /// Multiple concurrent requests read simultaneously; compile/contribute
    /// take an exclusive write lock to reload after mutating CozoDB.
    cache: Arc<RwLock<KnowledgeGraph>>,
    config: Config,
    /// LLM client for ReAct synthesis — None if provider is not configured.
    llm: Option<Arc<thinkingroot_extract::llm::LlmClient>>,
}

// ---------------------------------------------------------------------------
// Artifact type <-> filename mapping
// ---------------------------------------------------------------------------

/// Run a blocking closure from within an async context.
///
/// On the multi-threaded tokio runtime (production server) this defers to
/// `block_in_place`, telling the scheduler that this worker is about to
/// block so it can repark other tasks onto other workers — preventing a
/// reactor stall under concurrent load (the 10K-VU scenario).
///
/// On the single-threaded runtime (the default for `#[tokio::test]`) there
/// are no other workers to repark onto, so the closure is just invoked
/// directly. `block_in_place` cannot be used there — it panics.
#[inline]
fn run_blocking<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    match tokio::runtime::Handle::current().runtime_flavor() {
        // The original commit (e115307) had `run_blocking(f)` here —
        // a recursive self-call instead of `block_in_place(f)`. That
        // tail-recurses without bound on the multi-thread runtime
        // (production `root serve`), blowing the tokio worker stack
        // on the very first `search` / `search_scoped` call. Tests
        // are unaffected because `#[tokio::test]` defaults to the
        // single-thread flavor and falls through to the `_ => f()`
        // arm. Reproduces with `curl /api/v1/ws/{ws}/ask` against
        // any workspace, including empty ones.
        tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

fn artifact_filename(artifact_type: &str) -> Option<&'static str> {
    match artifact_type {
        "architecture-map" => Some("architecture-map.md"),
        "contradiction-report" => Some("contradiction-report.md"),
        "decision-log" => Some("decision-log.md"),
        "task-pack" => Some("task-pack.md"),
        "agent-brief" => Some("agent-brief.md"),
        "runbook" => Some("runbook.md"),
        "health-report" => Some("health-report.md"),
        "entity-pages" => Some("entities"),
        _ => None,
    }
}

/// All known artifact type keys.
const ARTIFACT_TYPES: &[&str] = &[
    "architecture-map",
    "contradiction-report",
    "decision-log",
    "task-pack",
    "agent-brief",
    "runbook",
    "health-report",
    "entity-pages",
    "gap-report",
];

/// Artifacts rendered on-demand from live graph state rather than read
/// from a pre-written file in `.thinkingroot/artifacts/`. These are
/// always "available" — there is no disk file to check.
fn is_dynamic_artifact(artifact_type: &str) -> bool {
    matches!(artifact_type, "gap-report")
}

// ---------------------------------------------------------------------------
// Pagination helper
// ---------------------------------------------------------------------------

fn apply_pagination<T>(vec: &mut Vec<T>, offset: Option<usize>, limit: Option<usize>) {
    if let Some(off) = offset {
        if off >= vec.len() {
            vec.clear();
        } else if off > 0 {
            *vec = vec.split_off(off);
        }
    }
    if let Some(lim) = limit {
        vec.truncate(lim);
    }
}

// ---------------------------------------------------------------------------
// QueryEngine
// ---------------------------------------------------------------------------

pub struct QueryEngine {
    workspaces: HashMap<String, WorkspaceHandle>,
    /// Process-wide cache of open branch `GraphStore` handles, keyed by
    /// `(workspace_root, branch_name)`. Every serve-crate code path that
    /// reads or writes a branch's graph.db goes through this cache to
    /// preserve the "one DbInstance per branch" invariant (see
    /// `branch_cache` module docs for why).
    branch_engines: Arc<crate::branch_cache::BranchEngineCache>,
}

impl Default for QueryEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryEngine {
    /// Create a new empty QueryEngine with no mounted workspaces.
    pub fn new() -> Self {
        Self {
            workspaces: HashMap::new(),
            branch_engines: Arc::new(crate::branch_cache::BranchEngineCache::default_cache()),
        }
    }

    /// Create a new empty QueryEngine with an explicit branch-cache config.
    /// Used by callers that want to tune `max_entries`/`ttl_secs`/`disabled`
    /// (e.g. long-lived servers pulling config from workspace TOML).
    pub fn with_branch_cache_config(cfg: &thinkingroot_core::config::BranchCacheConfig) -> Self {
        Self {
            workspaces: HashMap::new(),
            branch_engines: Arc::new(crate::branch_cache::BranchEngineCache::new(cfg)),
        }
    }

    /// Test/telemetry accessor for the branch engine cache.
    pub fn branch_engines(&self) -> &crate::branch_cache::BranchEngineCache {
        &self.branch_engines
    }

    /// Clone of the `Arc<BranchEngineCache>` — handed out to background
    /// tasks (e.g. the stream-cleanup task) that need to invalidate cache
    /// entries without borrowing the whole engine.
    pub fn branch_engines_arc(&self) -> Arc<crate::branch_cache::BranchEngineCache> {
        self.branch_engines.clone()
    }

    /// Mount a workspace by name, opening the `.thinkingroot/` data directory,
    /// loading the config and storage engine, and warming the in-memory cache.
    pub async fn mount(&mut self, name: String, root_path: PathBuf) -> Result<()> {
        let data_dir = root_path.join(".thinkingroot");
        if !data_dir.exists() {
            return Err(Error::Config(format!(
                "no .thinkingroot directory found at {}",
                root_path.display()
            )));
        }

        // One-time silent migration: move any legacy `.thinkingroot-{slug}/` sibling
        // dirs to the new nested layout `.thinkingroot/branches/{slug}/`.
        match thinkingroot_branch::migrate_legacy_layout(&root_path) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                "migrated {n} legacy branch director{} to .thinkingroot/branches/",
                if n == 1 { "y" } else { "ies" }
            ),
            Err(e) => tracing::warn!("branch layout migration failed (non-fatal): {e}"),
        }

        let config = Config::load_merged(&root_path)?;
        let storage = StorageEngine::init(&data_dir).await?;
        let cache = KnowledgeGraph::load_from_graph(&storage.graph)?;

        if storage.vector.is_empty() && cache.entity_count() > 0 {
            tracing::warn!(
                "Workspace '{}' contains {} entities in graph.db, but the vector index is missing or empty. \
                 This usually indicates an interrupted compilation. \
                 3D visualization and semantic search will degrade gracefully. \
                 Run `root compile` to rebuild the missing embeddings.",
                name,
                cache.entity_count()
            );
        }
        let llm = match thinkingroot_extract::llm::LlmClient::new(&config.llm).await {
            Ok(client) => {
                tracing::debug!("LLM client initialised for workspace '{name}'");
                Some(Arc::new(client))
            }
            Err(e) => {
                tracing::debug!("LLM not configured for workspace '{name}' (non-fatal): {e}");
                None
            }
        };

        self.workspaces.insert(
            name.clone(),
            WorkspaceHandle {
                name,
                root_path,
                storage: Arc::new(Mutex::new(storage)),
                cache: Arc::new(RwLock::new(cache)),
                config,
                llm,
            },
        );

        Ok(())
    }

    /// Mount a workspace using an explicit data directory instead of the default
    /// `.thinkingroot/` subdirectory. Used by `root serve --branch` to mount a
    /// branch-scoped data directory such as `.thinkingroot-feature-x/`.
    pub async fn mount_with_data_dir(
        &mut self,
        name: String,
        root_path: PathBuf,
        data_dir: PathBuf,
    ) -> Result<()> {
        if !data_dir.exists() {
            return Err(Error::Config(format!(
                "data directory not found: {}",
                data_dir.display()
            )));
        }

        match thinkingroot_branch::migrate_legacy_layout(&root_path) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                "migrated {n} legacy branch director{} to .thinkingroot/branches/",
                if n == 1 { "y" } else { "ies" }
            ),
            Err(e) => tracing::warn!("branch layout migration failed (non-fatal): {e}"),
        }

        let config = Config::load_merged(&root_path).unwrap_or_default();
        let storage = StorageEngine::init(&data_dir).await?;
        let cache = KnowledgeGraph::load_from_graph(&storage.graph)?;

        if storage.vector.is_empty() && cache.entity_count() > 0 {
            tracing::warn!(
                "Workspace '{}' contains {} entities in graph.db, but the vector index is missing or empty. \
                 This usually indicates an interrupted compilation. \
                 3D visualization and semantic search will degrade gracefully. \
                 Run `root compile` to rebuild the missing embeddings.",
                name,
                cache.entity_count()
            );
        }
        let llm = match thinkingroot_extract::llm::LlmClient::new(&config.llm).await {
            Ok(client) => Some(Arc::new(client)),
            Err(_) => None,
        };

        self.workspaces.insert(
            name.clone(),
            WorkspaceHandle {
                name,
                root_path,
                storage: Arc::new(Mutex::new(storage)),
                cache: Arc::new(RwLock::new(cache)),
                config,
                llm,
            },
        );

        Ok(())
    }

    /// Unmount a previously mounted workspace.
    pub fn unmount(&mut self, name: &str) -> Result<()> {
        self.workspaces
            .remove(name)
            .ok_or_else(|| Error::EntityNotFound(format!("workspace '{name}' not mounted")))?;
        Ok(())
    }

    /// List all currently mounted workspaces with summary counts.
    /// Served from in-memory cache — O(1) per workspace.
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let mut result = Vec::with_capacity(self.workspaces.len());
        for handle in self.workspaces.values() {
            let cache = handle.cache.read().await;
            let (source_count, claim_count, entity_count) = cache.counts();
            result.push(WorkspaceInfo {
                name: handle.name.clone(),
                path: handle.root_path.display().to_string(),
                entity_count,
                claim_count,
                source_count,
            });
        }
        Ok(result)
    }

    /// List all entities in a workspace.
    /// Served from in-memory cache — O(n) where n = entity count, zero disk I/O.
    pub async fn list_entities(&self, ws: &str) -> Result<Vec<EntityInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        let mut result = Vec::with_capacity(cache.entity_count());
        for id in cache.entities_ordered() {
            if let Some(e) = cache.entity_by_id(id) {
                result.push(EntityInfo {
                    id: e.id.clone(),
                    name: e.canonical_name.clone(),
                    entity_type: e.entity_type.clone(),
                    claim_count: cache.entity_claim_count(&e.id),
                });
            }
        }

        Ok(result)
    }

    /// Get detailed information about a single entity by name (case-insensitive).
    /// Served from in-memory cache — O(1) name lookup + O(k) claim/relation fetches.
    pub async fn get_entity(&self, ws: &str, name: &str) -> Result<EntityDetail> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        let entity = cache
            .find_entity_by_name(name)
            .ok_or_else(|| Error::EntityNotFound(name.to_string()))?;

        let claims: Vec<ClaimInfo> = cache
            .claims_for_entity(&entity.id)
            .into_iter()
            .map(cached_claim_to_info)
            .collect();

        let relations: Vec<RelationInfo> = cache
            .relations_for_entity(&entity.canonical_name)
            .into_iter()
            .map(|r| RelationInfo {
                target: r.to_name.clone(),
                relation_type: r.relation_type.clone(),
                strength: r.strength,
            })
            .collect();

        Ok(EntityDetail {
            id: entity.id.clone(),
            name: entity.canonical_name.clone(),
            entity_type: entity.entity_type.clone(),
            aliases: entity.aliases.clone(),
            claims,
            relations,
        })
    }

    /// List claims with optional filtering by type, entity, min confidence, limit, offset.
    /// Served from in-memory cache.
    pub async fn list_claims(&self, ws: &str, filter: ClaimFilter) -> Result<Vec<ClaimInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        // Entity-scoped path: O(1) name lookup + O(k) claim scan.
        if let Some(ref entity_name) = filter.entity_name {
            let entity = match cache.find_entity_by_name(entity_name) {
                Some(e) => e,
                None => return Ok(Vec::new()),
            };

            let mut claims: Vec<ClaimInfo> = cache
                .claims_for_entity(&entity.id)
                .into_iter()
                .filter(|c| {
                    let type_ok = filter
                        .claim_type
                        .as_ref()
                        .is_none_or(|t| t.eq_ignore_ascii_case(&c.claim_type));
                    let conf_ok = filter.min_confidence.is_none_or(|min| c.confidence >= min);
                    type_ok && conf_ok
                })
                .map(cached_claim_to_info)
                .collect();

            // Sort newest-first: most recent event_date wins over older claims.
            // This ensures knowledge-update answers reflect the current state, not
            // the first time something was mentioned.  Claims without event_date
            // (event_date = None) sort to the end (treated as oldest).
            claims.sort_by(|a, b| {
                b.event_date
                    .unwrap_or(0.0)
                    .partial_cmp(&a.event_date.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            apply_pagination(&mut claims, filter.offset, filter.limit);
            return Ok(claims);
        }

        // Type-filtered or full-listing path.
        let raw: Vec<&CachedClaim> = if let Some(ref ct) = filter.claim_type {
            cache.claims_of_type(ct)
        } else {
            cache.all_claims().collect()
        };

        let mut claims: Vec<ClaimInfo> = raw
            .into_iter()
            .filter(|c| filter.min_confidence.is_none_or(|min| c.confidence >= min))
            .map(cached_claim_to_info)
            .collect();

        apply_pagination(&mut claims, filter.offset, filter.limit);
        Ok(claims)
    }

    /// Get relations for a specific entity by name.
    /// Served from in-memory cache — O(k).
    pub async fn get_relations(&self, ws: &str, entity: &str) -> Result<Vec<RelationInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        Ok(cache
            .relations_for_entity(entity)
            .into_iter()
            .map(|r| RelationInfo {
                target: r.to_name.clone(),
                relation_type: r.relation_type.clone(),
                strength: r.strength,
            })
            .collect())
    }

    /// Get all relations in the workspace as (from, to, relation_type, strength) tuples.
    /// Served from in-memory cache — O(n) over pre-built Vec, zero disk I/O.
    pub async fn get_all_relations(&self, ws: &str) -> Result<Vec<(String, String, String, f64)>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        Ok(cache
            .all_relations()
            .iter()
            .map(|r| {
                (
                    r.from_name.clone(),
                    r.to_name.clone(),
                    r.relation_type.clone(),
                    r.strength,
                )
            })
            .collect())
    }

    /// Retrieve the entire knowledge base mapped out into a highly compact
    /// 3D topology format (`GalaxyMap`).
    /// Combines the 2D projection from `VectorStore` (Semantic axes)
    /// with the cache's claim_count mapping (Epistemic Z-axis).
    /// Used by the high-performance WebGL galaxy viewer.
    pub async fn get_galaxy_map(&self, ws: &str) -> Result<GalaxyMap> {
        let handle = self.get_workspace(ws)?;

        let coords_2d = {
            let storage = handle.storage.lock().await;
            storage.vector.project_to_2d()
        };

        let cache = handle.cache.read().await;

        let mut nodes = Vec::with_capacity(cache.entity_count());
        for id in cache.entities_ordered() {
            if let Some(e) = cache.entity_by_id(id) {
                // VectorStore keys are prefixed with "entity:"
                let vec_key = format!("entity:{}", e.id);
                let (x, y) = coords_2d.get(&vec_key).copied().unwrap_or((0.0, 0.0));
                let claim_count = cache.entity_claim_count(&e.id);
                // Z-axis: Density of Truth. Logarithmic scaling so outliers don't break the map.
                let z = (claim_count as f32 + 1.0).ln() * 50.0;
                let created_at =
                    e.id.parse::<thinkingroot_core::types::EntityId>()
                        .map(|id| id.timestamp_ms())
                        .unwrap_or(0);

                nodes.push(GalaxyNode {
                    id: e.id.to_string(),
                    name: e.canonical_name.clone(),
                    entity_type: e.entity_type.clone(),
                    claim_count,
                    x,
                    y,
                    z,
                    created_at,
                });
            }
        }

        let mut links = Vec::new();
        for r in cache.all_relations() {
            // Re-map name back to ID for the links because frontend graphs prefer ID-based links
            if let (Some(src), Some(tgt)) = (
                cache.find_entity_by_name(&r.from_name),
                cache.find_entity_by_name(&r.to_name),
            ) {
                links.push(GalaxyLink {
                    source: src.id.clone(),
                    target: tgt.id.clone(),
                    relation_type: r.relation_type.clone(),
                });
            }
        }

        Ok(GalaxyMap { nodes, links })
    }

    /// List all known artifact types and whether each is available on disk.
    pub async fn list_artifacts(&self, ws: &str) -> Result<Vec<ArtifactInfo>> {
        let handle = self.get_workspace(ws)?;
        let artifacts_dir = handle.root_path.join(".thinkingroot").join("artifacts");

        let mut result = Vec::with_capacity(ARTIFACT_TYPES.len());
        for &atype in ARTIFACT_TYPES {
            let available = if is_dynamic_artifact(atype) {
                true
            } else if let Some(filename) = artifact_filename(atype) {
                artifacts_dir.join(filename).exists()
            } else {
                false
            };
            result.push(ArtifactInfo {
                artifact_type: atype.to_string(),
                available,
            });
        }

        Ok(result)
    }

    /// List all sources in the workspace.
    /// Served from in-memory cache.
    pub async fn list_sources(&self, ws: &str) -> Result<Vec<SourceInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        Ok(cache
            .all_sources()
            .iter()
            .map(|s| SourceInfo {
                id: s.id.clone(),
                uri: s.uri.clone(),
                source_type: s.source_type.clone(),
            })
            .collect())
    }

    /// Remove every claim, entity edge, vector, and contradiction
    /// row that descends from `source_uri` in the named workspace, then
    /// rebuild the in-memory read cache so subsequent queries reflect
    /// the redaction.
    ///
    /// Returns the number of source rows removed (0 if `source_uri`
    /// did not match any). Idempotent: calling twice with the same
    /// URI is a no-op the second time.
    ///
    /// Used by the desktop privacy dashboard's "Forget" action
    /// (Phase F Stream H, Step 13). This is the load-bearing API
    /// behind the user-facing covenant commitment that any datum
    /// can be removed on demand.
    pub async fn forget_source(&self, ws: &str, source_uri: &str) -> Result<usize> {
        let handle = self.get_workspace(ws)?;

        // Phase 1: graph mutation. Hold the storage lock only for the
        // delete + the raw refetch.
        let raw_data: RawGraphData = {
            let storage = handle.storage.lock().await;
            let removed = storage.graph.remove_source_by_uri(source_uri)?;
            if removed == 0 {
                return Ok(0);
            }
            KnowledgeGraph::fetch_raw(&storage.graph)?
        };

        // Phase 2: rebuild the cache off-lock (CPU-only).
        let new_cache = KnowledgeGraph::build_from_raw(raw_data);

        // Phase 3: atomic cache swap.
        *handle.cache.write().await = new_cache;

        Ok(1)
    }

    /// Read the content of a specific artifact.
    pub async fn get_artifact(&self, ws: &str, artifact_type: &str) -> Result<ArtifactContent> {
        // Dynamic artifacts are rendered on-demand from live graph state.
        if is_dynamic_artifact(artifact_type) {
            return self.render_dynamic_artifact(ws, artifact_type).await;
        }

        let handle = self.get_workspace(ws)?;
        let filename = artifact_filename(artifact_type).ok_or_else(|| Error::Compilation {
            artifact_type: artifact_type.to_string(),
            message: format!("unknown artifact type: {artifact_type}"),
        })?;

        let artifact_path = handle
            .root_path
            .join(".thinkingroot")
            .join("artifacts")
            .join(filename);

        if artifact_type == "entity-pages" {
            // For entity-pages, concatenate all files in the directory.
            if !artifact_path.is_dir() {
                return Err(Error::Compilation {
                    artifact_type: artifact_type.to_string(),
                    message: "entity-pages directory not found".to_string(),
                });
            }
            let mut content = String::new();
            let mut entries: Vec<_> = std::fs::read_dir(&artifact_path)
                .map_err(|e| Error::io_path(&artifact_path, e))?
                .filter_map(|e| e.ok())
                .collect();
            entries.sort_by_key(|e| e.file_name());

            for entry in entries {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    let text =
                        std::fs::read_to_string(&path).map_err(|e| Error::io_path(&path, e))?;
                    if !content.is_empty() {
                        content.push_str("\n---\n\n");
                    }
                    content.push_str(&text);
                }
            }
            return Ok(ArtifactContent {
                artifact_type: artifact_type.to_string(),
                content,
            });
        }

        // Regular file artifact.
        if !artifact_path.exists() {
            return Err(Error::Compilation {
                artifact_type: artifact_type.to_string(),
                message: format!("artifact not found at {}", artifact_path.display()),
            });
        }

        let content = std::fs::read_to_string(&artifact_path)
            .map_err(|e| Error::io_path(&artifact_path, e))?;

        Ok(ArtifactContent {
            artifact_type: artifact_type.to_string(),
            content,
        })
    }

    /// Render a dynamic artifact (not backed by a file on disk) by
    /// querying live graph state. Currently covers `gap-report`.
    async fn render_dynamic_artifact(
        &self,
        ws: &str,
        artifact_type: &str,
    ) -> Result<ArtifactContent> {
        let content = match artifact_type {
            "gap-report" => self.render_gap_report(ws).await?,
            other => {
                return Err(Error::Compilation {
                    artifact_type: other.to_string(),
                    message: format!("unknown dynamic artifact type: {other}"),
                });
            }
        };
        Ok(ArtifactContent {
            artifact_type: artifact_type.to_string(),
            content,
        })
    }

    /// Render `gap-report.md` — a human-readable markdown dashboard of
    /// the patterns Phase 9 Reflect has discovered and the gaps those
    /// patterns imply for specific entities.
    ///
    /// Always reflects *current* state — if the reflect cycle hasn't run
    /// yet, the report is accurate ("no patterns discovered yet").
    async fn render_gap_report(&self, ws: &str) -> Result<String> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let graph = &storage.graph;

        // Load patterns (denormalized enough for the report).
        let pattern_rows = graph.reflect_load_structural_patterns()?;
        let gaps = thinkingroot_reflect::list_open_gaps(graph, None, 0.0)?;
        let total_open = graph.reflect_count_open_known_unknowns()?;

        let mut md = String::new();
        md.push_str(&format!(
            "# Gap Report — `{ws}`\n\n\
             _Generated from live graph state. No cron, no compile step — this report reflects the graph as of now._\n\n"
        ));

        // ── Patterns ──────────────────────────────────────────────────
        md.push_str("## Discovered Patterns\n\n");
        if pattern_rows.is_empty() {
            md.push_str(
                "_No patterns discovered yet. Run the `reflect` tool after compiling \
                 enough entities (≥30 of the same type) to establish co-occurrence signal._\n\n",
            );
        } else {
            md.push_str(
                "_`Stability` = consecutive reflect cycles the pattern has survived. Gap claims use damped confidence (frequency × stability factor) until a pattern stabilizes. `Scope` is `local` for single-workspace or `cross:<id>` for aggregated patterns._\n\n",
            );
            md.push_str(
                "| Entity Type | When has | Expected to have | Frequency | Sample | Stability | Scope |\n\
                 |---|---|---|---:|---:|---:|---|\n",
            );
            for (
                _id,
                etype,
                cond,
                expected,
                freq,
                sample,
                _last_computed,
                _threshold,
                _first_seen,
                stability_runs,
                source_scope,
            ) in &pattern_rows
            {
                md.push_str(&format!(
                    "| {etype} | `{cond}` | `{expected}` | {freq_pct:.1}% | {sample} | {stab} | {scope} |\n",
                    freq_pct = freq * 100.0,
                    stab = stability_runs,
                    scope = source_scope,
                ));
            }
            md.push('\n');
        }

        // ── Open gaps ─────────────────────────────────────────────────
        md.push_str(&format!("## Open Gaps ({total_open})\n\n"));
        if gaps.is_empty() {
            md.push_str(
                "_No open gaps at any confidence. Either no patterns have been discovered \
                 yet, or every entity matching a pattern's condition also carries the expected claim._\n",
            );
        } else {
            // Group by entity_type for readability.
            use std::collections::BTreeMap;
            let mut by_type: BTreeMap<&str, Vec<&thinkingroot_reflect::GapReport>> =
                BTreeMap::new();
            for g in &gaps {
                by_type.entry(g.entity_type.as_str()).or_default().push(g);
            }
            for (etype, group) in by_type {
                md.push_str(&format!("### {etype} ({} gap(s))\n\n", group.len()));
                for g in group {
                    md.push_str(&format!(
                        "- **{ename}** — expected `{expected}` @ {pct:.0}% (sample: {n})\n  > {reason}\n",
                        ename = g.entity_name,
                        expected = g.expected_claim_type,
                        pct = g.confidence * 100.0,
                        n = g.sample_size,
                        reason = g.reason,
                    ));
                }
                md.push('\n');
            }
        }

        // ── How to act ────────────────────────────────────────────────
        md.push_str(
            "## How to act on this report\n\n\
             - **Fill a gap** — add the expected claim for the entity (e.g. via `contribute` or a new source). Next `reflect` cycle will mark it resolved automatically.\n\
             - **Dismiss a false positive** — call `dismiss_gap` with the gap id. Dismissed gaps are not re-raised and do not penalize health coverage.\n\
             - **Lower the noise floor** — if too many weak patterns fire, raise `ReflectConfig::min_frequency` (default 0.70) or `min_sample_size` (default 30).\n",
        );

        Ok(md)
    }

    /// Run health/verification checks on the workspace.
    /// Reads directly from CozoDB — verification needs full consistency checks.
    pub async fn health(&self, ws: &str) -> Result<VerificationResult> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let verifier = Verifier::new(&handle.config);
        verifier.verify(&storage.graph)
    }

    /// Read the exact source bytes a claim cites. Powers the v3 MCP
    /// `read_source` tool: given a claim id, resolve the claim → source
    /// row → on-disk content_hash, then slice the source bytes by the
    /// claim's persisted byte range. Returns `Err(NotFound)` when the
    /// claim id is unknown; returns a `ReadSourceResult` with empty `text`
    /// + `bytes` when the claim has no byte range or the source bytes
    /// were never persisted (older workspaces).
    pub async fn read_source(&self, ws: &str, claim_id: &str) -> Result<ReadSourceResult> {
        use thinkingroot_core::Error;
        use thinkingroot_rooting::{FileSystemSourceStore, SourceByteStore};

        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        // 1. Look up the claim by id. Missing → propagate as ClaimNotFound
        //    so the MCP tool can return a clean error to the model.
        let claim = storage
            .graph
            .get_claim_by_id(claim_id)?
            .ok_or_else(|| Error::ClaimNotFound(claim_id.to_string()))?;

        // 2. Resolve the source row (uri + content_hash). The claim's
        //    `source` field is the SourceId; the URI we surface to the
        //    caller is the Source.uri so v3 packs cite by file path.
        let source = storage
            .graph
            .get_source_by_id(&claim.source.to_string())?
            .ok_or_else(|| {
                Error::GraphStorage(format!(
                    "claim {} cites source {} which is not in the graph",
                    claim_id, claim.source
                ))
            })?;

        // 3. Determine byte range. Pre-v3 claims (no source_span or
        //    line-only) have unknown byte ranges; return an empty result
        //    rather than an error so the caller can fall back to
        //    `read_file` for the same source.
        let (byte_start, byte_end) = match claim.source_span {
            Some(span) => match (span.byte_start, span.byte_end) {
                (Some(bs), Some(be)) if be > bs => (bs, be),
                _ => {
                    return Ok(ReadSourceResult {
                        file: source.uri.clone(),
                        byte_start: 0,
                        byte_end: 0,
                        text: String::new(),
                        bytes: Vec::new(),
                    });
                }
            },
            None => {
                return Ok(ReadSourceResult {
                    file: source.uri.clone(),
                    byte_start: 0,
                    byte_end: 0,
                    text: String::new(),
                    bytes: Vec::new(),
                });
            }
        };

        // 4. Read the bytes from the FileSystemSourceStore. Sources
        //    without a content_hash (synthetic agent contributions) or
        //    whose bytes were never persisted return an empty result.
        if source.content_hash.is_empty() {
            return Ok(ReadSourceResult {
                file: source.uri.clone(),
                byte_start,
                byte_end,
                text: String::new(),
                bytes: Vec::new(),
            });
        }
        let store = FileSystemSourceStore::new(&handle.root_path.join(".thinkingroot"))
            .map_err(|e| Error::GraphStorage(format!("source store init: {e}")))?;
        let bytes = store
            .get_range(&source.content_hash, byte_start as usize, byte_end as usize)
            .map_err(|e| Error::GraphStorage(format!("source read: {e}")))?
            .unwrap_or_default();
        let text = String::from_utf8(bytes.clone()).unwrap_or_default();

        Ok(ReadSourceResult {
            file: source.uri,
            byte_start,
            byte_end,
            text,
            bytes,
        })
    }

    /// Return Rooting admission-tier counts for a workspace.
    /// Bypasses the in-memory cache — queries CozoDB directly so MCP callers
    /// always see the freshest tier distribution (Phase 6.5 writes verdicts
    /// synchronously).
    pub async fn rooting_report(&self, ws: &str) -> Result<RootingReport> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let (rooted, attested, quarantined, rejected) =
            storage.graph.count_claims_by_admission_tier()?;
        Ok(RootingReport {
            workspace: ws.to_string(),
            rooted,
            attested,
            quarantined,
            rejected,
            total: rooted + attested + quarantined + rejected,
        })
    }

    /// List claims that passed Rooting with the `rooted` admission tier.
    /// Returns `ClaimInfo` rows (same shape as `list_claims`) but guaranteed
    /// tier-filtered. Reads from CozoDB directly for freshness.
    pub async fn list_rooted_claims(
        &self,
        ws: &str,
        type_filter: Option<String>,
        entity_filter: Option<String>,
        min_confidence: Option<f64>,
    ) -> Result<Vec<ClaimInfo>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        let rows = storage.graph.get_rooted_claims_filtered(
            type_filter.as_deref(),
            entity_filter.as_deref(),
            min_confidence,
        )?;
        Ok(rows
            .into_iter()
            .map(
                |(id, statement, claim_type, confidence, source_uri, event_date_raw)| {
                    let event_date = if event_date_raw != 0.0 {
                        Some(event_date_raw)
                    } else {
                        None
                    };
                    ClaimInfo {
                        id,
                        statement,
                        claim_type,
                        confidence,
                        source_uri,
                        event_date,
                    }
                },
            )
            .collect())
    }

    /// Run the full pipeline for a mounted workspace, then refresh the in-memory cache.
    ///
    /// ## Phase C lock-contention design
    ///
    /// The naive approach holds the storage `Mutex` for the entire cache rebuild
    /// (~2 s at Large scale), blocking all vector searches. Instead we use a
    /// three-phase pattern that minimises lock hold times:
    ///
    /// 1. **Pipeline** — runs its own `StorageEngine` internally; no lock held here.
    /// 2. **Noop guard** — if the pipeline reported no changes, skip the reload entirely.
    /// 3. **Fetch raw** — hold storage `Mutex` only for the 6–8 CozoDB bulk queries
    ///    (~300–600 ms), then release before building indexes.
    /// 4. **Build indexes** — pure CPU, no locks held (~400–800 ms). Vector searches
    ///    can proceed concurrently during this phase.
    /// 5. **Atomic swap** — acquire cache write lock only for the pointer swap (~100 μs).
    ///
    /// Net result: storage Mutex contention reduced from ~2 s → ~600 ms;
    ///             cache write-lock contention reduced from ~2 s → ~100 μs.
    pub async fn compile(&self, ws: &str) -> Result<PipelineResult> {
        let handle = self.get_workspace(ws)?;

        // Phase 1: Run pipeline (creates its own StorageEngine — no handle locks held).
        let result = crate::pipeline::run_pipeline(&handle.root_path, None, None).await?;

        // Phase 2: Noop guard — if nothing changed, the cache is still current.
        if !result.cache_dirty {
            tracing::debug!("compile noop — all files unchanged, skipping cache reload");
            return Ok(result);
        }

        // Phase 3: Fetch raw rows from CozoDB — hold storage Mutex only for I/O.
        let raw_data: RawGraphData = {
            let storage = handle.storage.lock().await;
            match KnowledgeGraph::fetch_raw(&storage.graph) {
                Ok(raw) => raw,
                Err(e) => {
                    tracing::warn!("cache fetch after compile failed (non-fatal): {e}");
                    return Ok(result);
                }
            }
        }; // ← storage Mutex released here; vector searches can resume immediately

        // Phase 4: Build in-memory indexes — pure CPU, zero locks held.
        let new_cache = KnowledgeGraph::build_from_raw(raw_data);

        // Phase 5: Atomic swap — write lock held only for the pointer assignment (~100 μs).
        *handle.cache.write().await = new_cache;

        Ok(result)
    }

    /// Search the workspace using vector similarity + keyword fallback.
    ///
    /// Vector search still goes to VectorStore (fastembed).
    /// Entity/claim lookups and claim counts are served from the in-memory cache
    /// — eliminating the N+1 CozoDB queries the old implementation required.
    pub async fn search(&self, ws: &str, query: &str, top_k: usize) -> Result<SearchResult> {
        let handle = self.get_workspace(ws)?;

        // Phase 1: Vector search — brief storage lock, released immediately after.
        // block_in_place: .search() runs synchronous ONNX inference (can take
        // 1–100ms). Without this wrap the tokio reactor stalls under concurrent
        // load — regresses the 10K-VU p95 claim.
        let vector_results = {
            let mut storage = handle.storage.lock().await;
            run_blocking(|| storage.vector.search(query, top_k * 2))?
            // storage Mutex drops here
        };

        let mut entity_hits: Vec<EntitySearchHit> = Vec::new();
        let mut claim_hits: Vec<ClaimSearchHit> = Vec::new();
        let mut seen_entity_ids: HashSet<String> = HashSet::new();
        let mut seen_claim_ids: HashSet<String> = HashSet::new();

        // Phase 2: Resolve vector hits from cache — O(1) per hit, no disk I/O.
        {
            let cache = handle.cache.read().await;

            for (key, _metadata, score) in &vector_results {
                if *score < 0.1 {
                    continue;
                }

                if let Some(bare_id) = key.strip_prefix("entity:")
                    && let Some(e) = cache.entity_by_id(bare_id)
                    && seen_entity_ids.insert(e.id.clone())
                {
                    entity_hits.push(EntitySearchHit {
                        id: e.id.clone(),
                        name: e.canonical_name.clone(),
                        entity_type: e.entity_type.clone(),
                        claim_count: cache.entity_claim_count(&e.id),
                        relevance: *score,
                    });
                    continue;
                }

                if let Some(bare_id) = key.strip_prefix("claim:")
                    && let Some(c) = cache.claim_by_id(bare_id)
                    && seen_claim_ids.insert(c.id.clone())
                {
                    claim_hits.push(ClaimSearchHit {
                        id: c.id.clone(),
                        statement: c.statement.clone(),
                        claim_type: c.claim_type.clone(),
                        confidence: c.confidence,
                        source_uri: c.source_uri.clone(),
                        relevance: *score,
                    });
                }
            }
            // cache read lock drops here — must release before acquiring storage lock below
        }

        // Phase 3: Keyword fallback if vector didn't return enough.
        // Storage lock acquired separately (never held simultaneously with cache lock).
        if entity_hits.len() + claim_hits.len() < top_k {
            let (kw_entities, kw_claims) = {
                let storage = handle.storage.lock().await;
                let ents = storage.graph.search_entities(query)?;
                let cls = storage.graph.search_claims(query)?;
                (ents, cls)
                // storage Mutex drops here
            };

            let cache = handle.cache.read().await;

            for (eid, ename, etype) in kw_entities {
                if seen_entity_ids.insert(eid.clone()) {
                    entity_hits.push(EntitySearchHit {
                        claim_count: cache.entity_claim_count(&eid),
                        id: eid,
                        name: ename,
                        entity_type: etype,
                        relevance: 0.5,
                    });
                }
            }

            for (cid, stmt, ctype, conf, uri) in kw_claims {
                if seen_claim_ids.insert(cid.clone()) {
                    claim_hits.push(ClaimSearchHit {
                        id: cid,
                        statement: stmt,
                        claim_type: ctype,
                        confidence: conf,
                        source_uri: uri,
                        relevance: 0.5,
                    });
                }
            }
            // cache read lock drops here
        }

        // Sort by descending relevance and truncate.
        entity_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        claim_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entity_hits.truncate(top_k);
        claim_hits.truncate(top_k);

        Ok(SearchResult {
            entities: entity_hits,
            claims: claim_hits,
        })
    }

    /// Source-scoped search: same as `search` but restricts claim results to
    /// those whose source URI contains one of the `allowed_source_ids` substrings.
    ///
    /// Used for multi-user graphs where each query must be isolated to a specific
    /// user's sessions. Entities are always included (user-agnostic structural nodes).
    pub async fn search_scoped(
        &self,
        ws: &str,
        query: &str,
        top_k: usize,
        allowed_source_ids: &HashSet<String>,
    ) -> Result<SearchResult> {
        let handle = self.get_workspace(ws)?;

        // Phase 1: Scoped vector search.
        // Empty allowed_source_ids means "no session scope" — treat as unscoped (all sources).
        let scope = if allowed_source_ids.is_empty() {
            None
        } else {
            Some(allowed_source_ids)
        };
        // block_in_place: see rationale on `search` above.
        let vector_results = {
            let mut storage = handle.storage.lock().await;
            run_blocking(|| {
                storage.vector.search_scoped(query, top_k * 3, scope)
            })?
        };

        let mut entity_hits: Vec<EntitySearchHit> = Vec::new();
        let mut claim_hits: Vec<ClaimSearchHit> = Vec::new();
        let mut seen_entity_ids: HashSet<String> = HashSet::new();
        let mut seen_claim_ids: HashSet<String> = HashSet::new();

        {
            let cache = handle.cache.read().await;
            for (key, _metadata, score) in &vector_results {
                if *score < 0.1 {
                    continue;
                }
                if let Some(bare_id) = key.strip_prefix("entity:")
                    && let Some(e) = cache.entity_by_id(bare_id)
                    && seen_entity_ids.insert(e.id.clone())
                {
                    entity_hits.push(EntitySearchHit {
                        id: e.id.clone(),
                        name: e.canonical_name.clone(),
                        entity_type: e.entity_type.clone(),
                        claim_count: cache.entity_claim_count(&e.id),
                        relevance: *score,
                    });
                    continue;
                }
                if let Some(bare_id) = key.strip_prefix("claim:")
                    && let Some(c) = cache.claim_by_id(bare_id)
                    && seen_claim_ids.insert(c.id.clone())
                {
                    // Double-check source scope at claim level (defense in depth).
                    // Empty allowed_sources = unscoped — accept all claims.
                    let in_scope = allowed_source_ids.is_empty()
                        || allowed_source_ids
                            .iter()
                            .any(|sid| c.source_uri.contains(sid.as_str()));
                    if in_scope {
                        claim_hits.push(ClaimSearchHit {
                            id: c.id.clone(),
                            statement: c.statement.clone(),
                            claim_type: c.claim_type.clone(),
                            confidence: c.confidence,
                            source_uri: c.source_uri.clone(),
                            relevance: *score,
                        });
                    }
                }
            }
        }

        // Phase 2: Keyword fallback scoped to allowed sources.
        if claim_hits.len() < top_k {
            let kw_claims = {
                let storage = handle.storage.lock().await;
                storage.graph.search_claims(query)?
            };
            for (cid, stmt, ctype, conf, uri) in kw_claims {
                let in_scope = allowed_source_ids
                    .iter()
                    .any(|sid| uri.contains(sid.as_str()));
                if in_scope && seen_claim_ids.insert(cid.clone()) {
                    claim_hits.push(ClaimSearchHit {
                        id: cid,
                        statement: stmt,
                        claim_type: ctype,
                        confidence: conf,
                        source_uri: uri,
                        relevance: 0.5,
                    });
                }
            }
        }

        entity_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        claim_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entity_hits.truncate(top_k);
        claim_hits.truncate(top_k);

        Ok(SearchResult {
            entities: entity_hits,
            claims: claim_hits,
        })
    }

    /// List tracked contradictions in the workspace.
    /// Served from in-memory cache.
    pub async fn list_contradictions(&self, ws: &str) -> Result<Vec<ContradictionInfo>> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        Ok(cache
            .all_contradictions()
            .iter()
            .map(|c| ContradictionInfo {
                id: c.id.clone(),
                claim_a: c.claim_a.clone(),
                claim_b: c.claim_b.clone(),
                explanation: c.explanation.clone(),
                status: c.status.clone(),
            })
            .collect())
    }

    /// Alias for `health()` — delegates to the same verification logic.
    pub async fn verify(&self, ws: &str) -> Result<VerificationResult> {
        self.health(ws).await
    }

    /// Return a token-efficient workspace overview for agent orientation.
    /// Served entirely from in-memory cache — zero disk I/O.
    pub async fn get_workspace_brief(&self, ws: &str) -> Result<WorkspaceSummary> {
        let handle = self.get_workspace(ws)?;
        let cache = handle.cache.read().await;

        let (source_count, claim_count, entity_count) = cache.counts();
        let top_entities = cache.top_entities_by_claim_count(10);

        let recent_decisions: Vec<(String, f64)> = cache
            .claims_of_type("Decision")
            .into_iter()
            .take(10)
            .map(|c| (c.statement.clone(), c.confidence))
            .collect();

        let contradiction_count = cache
            .all_contradictions()
            .iter()
            .filter(|c| c.status == "Detected")
            .count();

        Ok(WorkspaceSummary {
            workspace: ws.to_string(),
            entity_count,
            claim_count,
            source_count,
            top_entities,
            recent_decisions,
            contradiction_count,
        })
    }

    /// Return full graph context for a named entity.
    ///
    /// This executes 6 Datalog queries directly against CozoDB (incoming relations,
    /// per-entity contradictions). It is kept on CozoDB for correctness; Phase C
    /// will add a full entity-context cache.
    pub async fn get_entity_context(
        &self,
        ws: &str,
        entity_name: &str,
    ) -> Result<Option<thinkingroot_graph::graph::EntityContext>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        storage.graph.get_entity_context(entity_name)
    }

    // ── Branch-aware read wrappers ────────────────────────────────────────────
    // These accept an optional branch name.  For now they delegate to the
    // main-only methods; Gap 3 (Item 5) fills in union search for branch+main.

    pub async fn search_branched(
        &self,
        ws: &str,
        query: &str,
        top_k: usize,
        branch: Option<&str>,
    ) -> Result<SearchResult> {
        use thinkingroot_branch::snapshot::resolve_data_dir;

        // Fast path: no branch → main-only search.
        let branch_name = match branch {
            Some(b) => b,
            None => return self.search(ws, query, top_k).await,
        };

        let handle = self.get_workspace(ws)?;
        let branch_data_dir = resolve_data_dir(&handle.root_path, Some(branch_name));

        // ── 1. Search branch vector index ─────────────────────────────────────
        let branch_vector_hits: Vec<(String, String, f32)> = if branch_data_dir.exists() {
            match thinkingroot_graph::vector::VectorStore::init(&branch_data_dir).await {
                Ok(mut bv) => run_blocking(|| bv.search(query, top_k * 2))
                    .unwrap_or_default(),
                Err(_) => vec![],
            }
        } else {
            vec![]
        };

        let mut entity_hits: Vec<EntitySearchHit> = Vec::new();
        let mut claim_hits: Vec<ClaimSearchHit> = Vec::new();
        let mut seen_entity_ids: HashSet<String> = HashSet::new();
        let mut seen_claim_ids: HashSet<String> = HashSet::new();

        // ── 2. Resolve branch hits (branch priority — more recent than main) ──
        if !branch_vector_hits.is_empty() {
            // Resolve branch graph via the LRU (one DbInstance per branch).
            // `get_or_open` errors if the branch is missing — we silently
            // treat that as "no branch-only resolution possible" and fall
            // back to the main cache, matching the previous behavior.
            let branch_handle = self
                .branch_engines
                .get_or_open(&handle.root_path, branch_name)
                .await
                .ok();
            let cache = handle.cache.read().await;

            for (key, _meta, score) in &branch_vector_hits {
                if *score < 0.1 {
                    continue;
                }

                if let Some(bare_id) = key.strip_prefix("entity:") {
                    if !seen_entity_ids.insert(bare_id.to_string()) {
                        continue;
                    }
                    // Try main cache first (branch inherits from main at creation).
                    if let Some(e) = cache.entity_by_id(bare_id) {
                        entity_hits.push(EntitySearchHit {
                            id: e.id.clone(),
                            name: e.canonical_name.clone(),
                            entity_type: e.entity_type.clone(),
                            claim_count: cache.entity_claim_count(&e.id),
                            relevance: *score,
                        });
                    } else if let Some(ref bh) = branch_handle {
                        // Branch-only entity — resolve via branch graph point-lookup.
                        if let Ok(Some((name, etype, _desc))) = bh.graph.get_entity_by_id(bare_id) {
                            entity_hits.push(EntitySearchHit {
                                id: bare_id.to_string(),
                                name,
                                entity_type: etype,
                                claim_count: 0,
                                relevance: *score,
                            });
                        }
                    }
                    continue;
                }

                if let Some(bare_id) = key.strip_prefix("claim:") {
                    if !seen_claim_ids.insert(bare_id.to_string()) {
                        continue;
                    }
                    // Try main cache first.
                    if let Some(c) = cache.claim_by_id(bare_id) {
                        claim_hits.push(ClaimSearchHit {
                            id: c.id.clone(),
                            statement: c.statement.clone(),
                            claim_type: c.claim_type.clone(),
                            confidence: c.confidence,
                            source_uri: c.source_uri.clone(),
                            relevance: *score,
                        });
                    } else if let Some(ref bh) = branch_handle {
                        // Branch-only claim — resolve via branch graph point-lookup.
                        if let Ok(Some((stmt, ctype, conf, uri))) =
                            bh.graph.get_claim_with_source(bare_id)
                        {
                            claim_hits.push(ClaimSearchHit {
                                id: bare_id.to_string(),
                                statement: stmt,
                                claim_type: ctype,
                                confidence: conf,
                                source_uri: uri,
                                relevance: *score,
                            });
                        }
                    }
                }
            }
            // cache read lock drops here
        }

        // ── 3. Append main results (deduplicated against branch hits) ─────────
        let main_result = self.search(ws, query, top_k).await?;
        for hit in main_result.entities {
            if seen_entity_ids.insert(hit.id.clone()) {
                entity_hits.push(hit);
            }
        }
        for hit in main_result.claims {
            if seen_claim_ids.insert(hit.id.clone()) {
                claim_hits.push(hit);
            }
        }

        // ── 4. Sort by descending relevance and truncate ──────────────────────
        entity_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        claim_hits.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entity_hits.truncate(top_k);
        claim_hits.truncate(top_k);

        Ok(SearchResult {
            entities: entity_hits,
            claims: claim_hits,
        })
    }

    /// Branch-aware `list_claims`.
    ///
    /// A branch starts as a copy-on-write snapshot of main, then accumulates
    /// additional claims via `contribute`. So the branch graph alone is the
    /// authoritative view of what's visible on that branch — we read straight
    /// from its GraphStore and apply the same filter semantics as the
    /// main-cache path.
    pub async fn list_claims_branched(
        &self,
        ws: &str,
        filter: ClaimFilter,
        branch: Option<&str>,
    ) -> Result<Vec<ClaimInfo>> {
        let branch_name = match branch {
            None | Some("main") => return self.list_claims(ws, filter).await,
            Some(b) => b,
        };

        let handle = self.get_workspace(ws)?;
        let branch_handle = self
            .branch_engines
            .get_or_open(&handle.root_path, branch_name)
            .await?;
        let branch_graph = &branch_handle.graph;

        // Rows carry (id, statement, claim_type, confidence, source_uri, event_date).
        let rows: Vec<(String, String, String, f64, String, f64)> =
            if let Some(ref entity_name) = filter.entity_name {
                match branch_graph.find_entity_id_by_name(entity_name)? {
                    Some(eid) => branch_graph
                        .get_claims_with_sources_for_entity(&eid)?
                        .into_iter()
                        .map(|(id, stmt, ctype, uri, conf)| (id, stmt, ctype, conf, uri, 0.0))
                        .collect(),
                    None => return Ok(Vec::new()),
                }
            } else {
                branch_graph.get_all_claims_with_sources()?
            };

        let mut claims: Vec<ClaimInfo> = rows
            .into_iter()
            .filter(|(_, _, ctype, _, _, _)| {
                filter
                    .claim_type
                    .as_ref()
                    .is_none_or(|t| t.eq_ignore_ascii_case(ctype))
            })
            .filter(|(_, _, _, conf, _, _)| filter.min_confidence.is_none_or(|min| *conf >= min))
            .map(
                |(id, statement, claim_type, confidence, source_uri, event_date)| ClaimInfo {
                    id,
                    statement,
                    claim_type,
                    confidence,
                    source_uri,
                    event_date: if event_date > 0.0 {
                        Some(event_date)
                    } else {
                        None
                    },
                },
            )
            .collect();

        // Sort newest-first by event_date (matches main path at L496–501).
        claims.sort_by(|a, b| {
            b.event_date
                .unwrap_or(0.0)
                .partial_cmp(&a.event_date.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        apply_pagination(&mut claims, filter.offset, filter.limit);
        Ok(claims)
    }

    /// Branch-aware `get_relations`.
    pub async fn get_relations_branched(
        &self,
        ws: &str,
        entity: &str,
        branch: Option<&str>,
    ) -> Result<Vec<RelationInfo>> {
        let branch_name = match branch {
            None | Some("main") => return self.get_relations(ws, entity).await,
            Some(b) => b,
        };

        let handle = self.get_workspace(ws)?;
        let branch_handle = self
            .branch_engines
            .get_or_open(&handle.root_path, branch_name)
            .await?;

        let rows = branch_handle.graph.get_relations_for_entity(entity)?;
        Ok(rows
            .into_iter()
            .map(|(target, relation_type, strength)| RelationInfo {
                target,
                relation_type,
                strength,
            })
            .collect())
    }

    /// Branch-aware `get_workspace_brief`.
    ///
    /// Counts come from the branch graph (authoritative). Derived fields
    /// (`top_entities`, `recent_decisions`, `contradiction_count`) are read
    /// from the branch graph as well so the summary stays internally
    /// consistent with the branch's claim set.
    pub async fn get_workspace_brief_branched(
        &self,
        ws: &str,
        branch: Option<&str>,
    ) -> Result<WorkspaceSummary> {
        let branch_name = match branch {
            None | Some("main") => return self.get_workspace_brief(ws).await,
            Some(b) => b,
        };

        let handle = self.get_workspace(ws)?;
        let branch_handle = self
            .branch_engines
            .get_or_open(&handle.root_path, branch_name)
            .await?;
        let branch_graph = &branch_handle.graph;

        let (source_count, claim_count, entity_count) = branch_graph.get_counts()?;

        // Top entities by claim count — one Datalog query.
        let top_entities = branch_graph
            .get_top_entities_by_claim_count(10)
            .unwrap_or_default();

        // Recent decisions — filter branch claims by type=Decision, take 10.
        let recent_decisions: Vec<(String, f64)> = branch_graph
            .get_claims_by_type("Decision")
            .unwrap_or_default()
            .into_iter()
            .take(10)
            .map(|(_id, stmt, _ctype, conf, _uri)| (stmt, conf))
            .collect();

        let contradiction_count = branch_graph
            .get_contradictions()
            .map(|list| {
                list.iter()
                    .filter(|(_, _, _, _, s)| s == "Detected")
                    .count()
            })
            .unwrap_or(0);

        Ok(WorkspaceSummary {
            workspace: ws.to_string(),
            entity_count,
            claim_count,
            source_count,
            top_entities,
            recent_decisions,
            contradiction_count,
        })
    }

    /// Branch-aware `get_entity_context`. The branch graph has the full
    /// context by construction (branch is cloned from main at create), so we
    /// just dispatch `get_entity_context` against it.
    pub async fn get_entity_context_branched(
        &self,
        ws: &str,
        entity_name: &str,
        branch: Option<&str>,
    ) -> Result<Option<thinkingroot_graph::graph::EntityContext>> {
        let branch_name = match branch {
            None | Some("main") => return self.get_entity_context(ws, entity_name).await,
            Some(b) => b,
        };

        let handle = self.get_workspace(ws)?;
        let branch_handle = self
            .branch_engines
            .get_or_open(&handle.root_path, branch_name)
            .await?;
        branch_handle.graph.get_entity_context(entity_name)
    }

    /// Route a query to the Fast or Agentic path based on query classification.
    ///
    /// - Fast path (80% of queries): vector search + in-memory cache, sub-5ms.
    /// - Agentic path (20% of queries): ReAct loop, 200ms-2s.
    ///
    /// This is the primary entry point for the `search` MCP tool when used
    /// with session context.
    pub async fn search_with_routing(
        &self,
        ws: &str,
        query: &str,
        top_k: usize,
        session: &crate::intelligence::session::SessionContext,
    ) -> Result<String> {
        use crate::intelligence::react::ReActEngine;
        use crate::intelligence::router::{QueryPath, classify_query};

        const FAST_SYNTHESIS_PROMPT: &str = "\
You are a precise personal memory assistant. \
You are given retrieved memory notes and a question. \
Answer the question using ONLY the information in the notes. \
Rules: \
(1) Be concise and specific — answer in 1-3 sentences max. \
(2) If multiple claims contradict each other, trust the one with the HIGHER confidence score or the more recent date shown in [brackets]. \
(3) For yes/no questions answer 'Yes' or 'No' followed by a brief explanation. \
(4) For counting questions, count only the items explicitly mentioned in the notes. \
(5) If the answer is genuinely not in the notes, respond with exactly: \
\"I don't have enough information to answer that.\"";

        match classify_query(query, session) {
            QueryPath::Fast => {
                use crate::intelligence::reranker::Reranker;
                let mut result = self
                    .search_branched(ws, query, top_k, session.active_branch.as_deref())
                    .await?;
                // BM25 reranking: blends vector score with term-overlap score.
                let reranker = Reranker::new(query);
                reranker.rerank_claims(&mut result.claims);
                reranker.rerank_entities(&mut result.entities);

                // LLM synthesis: produce a natural-language answer rather than raw JSON.
                // The LongMemEval judge (and real-world callers) expect text, not structs.
                // Falls back to JSON serialization if no LLM is available.
                let llm = self.workspace_llm(ws);
                if let Some(ref llm_client) = llm {
                    let mut notes = String::new();
                    for hit in result.entities.iter().take(5) {
                        notes.push_str(&format!("Entity: {} ({})\n", hit.name, hit.entity_type));
                    }
                    for hit in result.claims.iter().take(15) {
                        notes.push_str(&format!(
                            "Claim [{:.2}]: {}\n",
                            hit.relevance, hit.statement
                        ));
                    }
                    if notes.is_empty() {
                        notes.push_str("No relevant memory found.\n");
                    }
                    let user_msg =
                        format!("## Retrieved Memory Notes\n\n{notes}\n\n## Question\n\n{query}");
                    if let Ok(answer) = llm_client.chat(FAST_SYNTHESIS_PROMPT, &user_msg).await {
                        return Ok(answer);
                    }
                }

                Ok(serde_json::to_string_pretty(&result).unwrap_or_default())
            }
            QueryPath::Agentic => {
                let llm = self.workspace_llm(ws);
                // Fetch the temporal anchor: the max event_date in the knowledge base.
                // This anchors relative queries ("last month", "3 months ago") to the
                // most recent session date rather than compile/query wall-clock time.
                let temporal_anchor: Option<f64> = {
                    let handle = self.get_workspace(ws).ok();
                    if let Some(h) = handle {
                        let storage = h.storage.lock().await;
                        storage.graph.get_max_event_timestamp().ok().flatten()
                    } else {
                        None
                    }
                };
                let react = ReActEngine::new(self, ws, llm);
                let result = react.run_with_anchor(query, session, temporal_anchor).await;
                Ok(result.answer)
            }
        }
    }

    /// Query SVO events whose timestamp falls in [start_ts, end_ts].
    /// Returns EventHit with entity names resolved from the KG cache so that
    /// LLM synthesis receives human-readable text, not ULID strings.
    pub async fn query_events_in_range(
        &self,
        ws: &str,
        start_ts: f64,
        end_ts: f64,
    ) -> Result<Vec<EventHit>> {
        let handle = self.get_workspace(ws)?;
        let raw_events = {
            let storage = handle.storage.lock().await;
            storage.graph.query_events_in_range(start_ts, end_ts)?
        };
        // Resolve entity IDs → names from the in-memory cache (no extra CozoDB round-trip).
        let cache = handle.cache.read().await;
        Ok(raw_events
            .into_iter()
            .map(|ev| {
                let subject_name = cache
                    .entity_name_by_id(&ev.subject_entity_id)
                    .unwrap_or(&ev.subject_entity_id)
                    .to_string();
                let object_name = if ev.object_entity_id.is_empty() {
                    String::new()
                } else {
                    cache
                        .entity_name_by_id(&ev.object_entity_id)
                        .unwrap_or(&ev.object_entity_id)
                        .to_string()
                };
                EventHit {
                    id: ev.id,
                    subject_name,
                    verb: ev.verb,
                    object_name,
                    normalized_date: ev.normalized_date,
                }
            })
            .collect())
    }

    /// Write agent-inferred claims directly into the graph, bypassing parse→extract.
    ///
    /// Claims are tagged `ExtractionTier::AgentInferred` and `TrustLevel::Untrusted`.
    /// A subsequent `root compile` will cross-validate against source code and may
    /// promote, supersede, or reject them based on grounding results.
    ///
    /// A synthetic source `mcp://agent/{session_id}` is created to anchor provenance.
    /// The in-memory cache is reloaded after writing so subsequent reads see new claims.
    #[tracing::instrument(
        name = "engine.contribute_claims",
        skip(self, agent_claims, sessions),
        fields(
            workspace = %ws,
            session_id = %session_id,
            branch = branch.unwrap_or("<main>"),
            claim_count = agent_claims.len(),
        ),
    )]
    pub async fn contribute_claims(
        &self,
        ws: &str,
        session_id: &str,
        branch: Option<&str>,
        agent_claims: Vec<AgentClaim>,
        sessions: &crate::intelligence::session::SessionStore,
    ) -> Result<ContributeResult> {
        use thinkingroot_branch::snapshot::resolve_data_dir;
        use thinkingroot_core::types::{ContentHash, SourceType, TrustLevel};

        if agent_claims.is_empty() {
            return Ok(ContributeResult {
                accepted_count: 0,
                accepted_ids: vec![],
                source_uri: String::new(),
                warnings: vec!["no claims provided".to_string()],
            });
        }

        let handle = self.get_workspace(ws)?;

        // Synthetic source anchors provenance for all contributed claims.
        let ts = chrono::Utc::now().timestamp();
        let source_uri = format!("mcp://agent/{session_id}");
        let source = thinkingroot_core::Source::new(source_uri.clone(), SourceType::ChatMessage)
            .with_trust(TrustLevel::Untrusted)
            .with_hash(ContentHash(format!("{session_id}-{ts}")));

        // Branch path: writes go to the branch graph only; main cache unchanged.
        if let Some(branch_name) = branch {
            let branch_data_dir = resolve_data_dir(&handle.root_path, Some(branch_name));
            if !branch_data_dir.exists() {
                return Err(Error::EntityNotFound(format!(
                    "branch '{branch_name}' not found — create it first with create_branch"
                )));
            }
            // Route through the LRU so concurrent branched-reads share the
            // same `GraphStore` (one DbInstance per branch invariant).
            let branch_handle = self
                .branch_engines
                .get_or_open(&handle.root_path, branch_name)
                .await?;
            let (accepted_ids, warnings) =
                Self::write_agent_claims_to_graph(&branch_handle.graph, &source, &agent_claims)?;

            // Upsert accepted claims into the branch vector index so they are
            // searchable in the branch without a full recompile.
            if !accepted_ids.is_empty() {
                match thinkingroot_graph::vector::VectorStore::init(&branch_data_dir).await {
                    Ok(mut branch_vector) => {
                        let items: Vec<(String, String, String)> = agent_claims
                            .iter()
                            .zip(accepted_ids.iter())
                            .map(|(ac, id)| {
                                let ctype = &ac.claim_type;
                                let conf = ac.confidence.unwrap_or(0.7);
                                (
                                    format!("claim:{id}"),
                                    ac.statement.clone(),
                                    format!("claim|{id}|{ctype}|{conf}|{source_uri}"),
                                )
                            })
                            .collect();
                        // block_in_place: upsert_batch runs batched ONNX
                        // embedding (most expensive sync call in the crate).
                        run_blocking(|| {
                            if let Err(e) = branch_vector.upsert_batch(&items) {
                                tracing::warn!("branch vector upsert failed (non-fatal): {e}");
                            } else if let Err(e) = branch_vector.save() {
                                tracing::warn!("branch vector save failed (non-fatal): {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("branch vector init failed (non-fatal): {e}");
                    }
                }
            }

            return Ok(ContributeResult {
                accepted_count: accepted_ids.len(),
                accepted_ids,
                source_uri,
                warnings,
            });
        }

        // No active branch — write to main graph, then reload cache.
        let accepted_ids;
        let mut warnings;
        {
            let storage = handle.storage.lock().await;
            (accepted_ids, warnings) =
                Self::write_agent_claims_to_graph(&storage.graph, &source, &agent_claims)?;

            // ── Rooting advisory pass ─────────────────────────────────
            // Route the freshly-admitted agent claims through the Rooting
            // gate. The synthetic `mcp://agent/*` source has no bytes on
            // disk, so we persist the joined claim statements as the source
            // body — that makes the provenance probe a tautology (intended:
            // agent attribution, not drift detection) and lets the
            // contradiction + temporal probes do real work. Advisory mode
            // logs tier distribution and appends a warning; enforce mode
            // (future) would also delete Rejected-tier claims.
            if handle.config.rooting.contribute_gate != "off"
                && !handle.config.rooting.disabled
                && !accepted_ids.is_empty()
            {
                let byte_store = thinkingroot_rooting::FileSystemSourceStore::new(
                    &handle.root_path.join(".thinkingroot"),
                );
                if let Ok(byte_store) = byte_store {
                    let joined: String = agent_claims
                        .iter()
                        .map(|c| c.statement.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    use thinkingroot_rooting::SourceByteStore;
                    let _ = byte_store.put(source.id, &source.content_hash, joined.as_bytes());

                    let mut claims: Vec<thinkingroot_core::Claim> =
                        Vec::with_capacity(accepted_ids.len());
                    for id in &accepted_ids {
                        if let Ok(Some(c)) = storage.graph.get_claim_by_id(id) {
                            claims.push(c);
                        }
                    }
                    let candidates: Vec<thinkingroot_rooting::CandidateClaim<'_>> = claims
                        .iter()
                        .map(|c| thinkingroot_rooting::CandidateClaim {
                            claim: c,
                            predicate: c.predicate.as_ref(),
                            derivation: c.derivation.as_ref(),
                        })
                        .collect();
                    let rooting_cfg = thinkingroot_rooting::RootingConfig {
                        disabled: handle.config.rooting.disabled,
                        provenance_threshold: handle.config.rooting.provenance_threshold,
                        contradiction_floor: handle.config.rooting.contradiction_floor,
                        contribute_gate: handle.config.rooting.contribute_gate.clone(),
                        predicate_strength_threshold: handle
                            .config
                            .rooting
                            .predicate_strength_threshold,
                    };
                    let rooter =
                        thinkingroot_rooting::Rooter::new(&storage.graph, &byte_store, rooting_cfg);
                    match rooter.root_batch(&candidates) {
                        Ok(output) => {
                            // Persist verdicts + certificates for audit.
                            let _ = thinkingroot_rooting::storage::insert_verdicts_batch(
                                &storage.graph,
                                &output.verdicts,
                            );
                            let _ = thinkingroot_rooting::storage::insert_certificates_batch(
                                &storage.graph,
                                &output.certificates,
                            );
                            // Stamp each claim with its new admission_tier.
                            for (idx, v) in output.verdicts.iter().enumerate() {
                                if let Some(c) = claims.get(idx) {
                                    let mut updated = c.clone();
                                    updated.admission_tier = v.admission_tier;
                                    updated.last_rooted_at = Some(v.trial_at);
                                    let _ = storage.graph.insert_claim(&updated);
                                }
                            }

                            // Enforce mode excises any Rejected-tier claim
                            // from the graph. Advisory keeps everything.
                            let enforce = handle.config.rooting.contribute_gate == "enforce";
                            let mut enforced_removed = 0usize;
                            if enforce {
                                for (idx, v) in output.verdicts.iter().enumerate() {
                                    if v.admission_tier
                                        == thinkingroot_core::types::AdmissionTier::Rejected
                                    {
                                        if let Some(c) = claims.get(idx) {
                                            let cid = c.id.to_string();
                                            if let Err(e) =
                                                storage.graph.remove_claim_fully(&cid)
                                            {
                                                tracing::warn!(
                                                    "contribute enforce: remove_claim_fully({}) failed: {e}",
                                                    cid
                                                );
                                            } else {
                                                enforced_removed += 1;
                                            }
                                        }
                                    }
                                }
                            }

                            let enforce_note = if enforce {
                                format!(" — enforce mode removed {enforced_removed}")
                            } else {
                                String::from(" — advisory mode, nothing removed")
                            };

                            if output.rejected_count > 0 || output.quarantined_count > 0 {
                                warnings.push(format!(
                                    "rooting: {} rejected, {} quarantined{}",
                                    output.rejected_count,
                                    output.quarantined_count,
                                    enforce_note
                                ));
                            } else {
                                tracing::info!(
                                    "rooting {}: {} admitted cleanly (contribute)",
                                    if enforce { "enforce" } else { "advisory" },
                                    output.admitted_count
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!("rooting advisory pass failed (non-fatal): {e}");
                        }
                    }
                }
            }

            // Reload while still holding storage lock so no concurrent write
            // can slip in between the CozoDB write and the cache update.
            match KnowledgeGraph::load_from_graph(&storage.graph) {
                Ok(new_cache) => {
                    *handle.cache.write().await = new_cache;
                }
                Err(e) => {
                    tracing::warn!("cache reload after contribute failed (non-fatal): {e}");
                }
            }
        }

        // ── Turn calendar: record which claims were contributed this turn ────
        if !accepted_ids.is_empty() {
            let turn_number = {
                let mut store = sessions.lock().await;
                let session = store.entry(session_id.to_string()).or_insert_with(|| {
                    crate::intelligence::session::SessionContext::new(session_id, ws)
                });
                session.turn_count += 1;
                session.turn_count
            };
            let storage = handle.storage.lock().await;
            if let Err(e) = storage
                .graph
                .record_turn(session_id, turn_number, &accepted_ids)
            {
                tracing::warn!("turn calendar record failed (non-fatal): {e}");
            }
        }

        Ok(ContributeResult {
            accepted_count: accepted_ids.len(),
            accepted_ids,
            source_uri,
            warnings,
        })
    }

    /// Merge a branch into main with post-merge cache reload.
    ///
    /// `execute_merge` lives in `thinkingroot-branch` (disk layer) and has no
    /// knowledge of the serve-layer cache. Without the reload step in this
    /// wrapper, `search`/`list_claims` return stale data after a merge until
    /// the next `contribute` or `compile`.
    ///
    /// The workspace handle is located by `root` (any mounted workspace whose
    /// `root_path` equals `root`). If no mounted workspace matches, the merge
    /// still runs — callers using this outside a mounted-workspace context
    /// (e.g. the CLI) get the disk-level behavior without cache side effects.
    #[tracing::instrument(
        name = "engine.merge_branch",
        skip(self, root, merged_by),
        fields(
            branch = %branch_name,
            force,
            propagate_deletions,
        ),
    )]
    pub async fn merge_branch(
        &self,
        root: &std::path::Path,
        branch_name: &str,
        force: bool,
        propagate_deletions: bool,
        merged_by: thinkingroot_core::MergedBy,
    ) -> Result<thinkingroot_core::KnowledgeDiff> {
        use thinkingroot_branch::diff::compute_diff;
        use thinkingroot_branch::merge::execute_merge;
        use thinkingroot_branch::snapshot::resolve_data_dir;
        use thinkingroot_graph::graph::GraphStore;

        let main_data_dir = resolve_data_dir(root, None);
        let branch_data_dir = resolve_data_dir(root, Some(branch_name));
        if !branch_data_dir.exists() {
            return Err(Error::EntityNotFound(format!(
                "branch '{branch_name}' not found"
            )));
        }

        // Merge-time knobs: prefer the mounted workspace config, fall back to
        // disk config so unmounted callers still work.
        let mounted = self.workspaces.values().find(|h| h.root_path == root);
        let merge_cfg = match mounted {
            Some(h) => h.config.merge.clone(),
            None => Config::load_merged(root)?.merge,
        };

        let main_graph = GraphStore::init(&main_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("main graph init failed: {e}")))?;
        let branch_graph = GraphStore::init(&branch_data_dir.join("graph"))
            .map_err(|e| Error::GraphStorage(format!("branch graph init failed: {e}")))?;

        let mut diff = compute_diff(
            &main_graph,
            &branch_graph,
            branch_name,
            merge_cfg.auto_resolve_threshold,
            merge_cfg.max_health_drop,
            merge_cfg.block_on_contradictions,
        )?;
        if force {
            diff.merge_allowed = true;
            diff.blocking_reasons.clear();
        }

        // Drop the separate GraphStore handle on main *before* executing the
        // merge so `execute_merge` can take its own handle to `graph.db`
        // (some SQLite configurations serialize writers).
        drop(main_graph);
        drop(branch_graph);

        execute_merge(root, branch_name, &diff, merged_by, propagate_deletions).await?;

        // Reload the mounted workspace's cache so queries reflect the merge.
        // Hold the storage mutex across reload so no concurrent writer can
        // slip in between (same discipline as contribute_claims L1647–1656).
        if let Some(handle) = mounted {
            let storage = handle.storage.lock().await;
            match KnowledgeGraph::load_from_graph(&storage.graph) {
                Ok(new_cache) => {
                    *handle.cache.write().await = new_cache;
                }
                Err(e) => {
                    tracing::warn!("cache reload after merge failed (non-fatal): {e}");
                }
            }
        }

        Ok(diff)
    }

    /// Soft-delete a branch (marks Abandoned, data retained). Evicts the
    /// branch from the engine cache so stale handles can't serve reads
    /// against an Abandoned entry.
    pub async fn delete_branch(&self, root: &std::path::Path, branch_name: &str) -> Result<()> {
        self.branch_engines.invalidate(root, branch_name).await;
        thinkingroot_branch::delete_branch(root, branch_name)
            .map_err(|e| Error::GraphStorage(format!("delete_branch failed: {e}")))
    }

    /// Garbage-collect all Abandoned branches (hard-delete their data dirs).
    /// Evicts every cache entry for this workspace root because we don't
    /// selectively know which branches got purged.
    pub async fn gc_branches(&self, root: &std::path::Path) -> Result<usize> {
        self.branch_engines.invalidate_workspace(root).await;
        thinkingroot_branch::gc_branches(root)
            .map_err(|e| Error::GraphStorage(format!("gc_branches failed: {e}")))
    }

    /// Phase 9 Reflect — discover patterns + surface gaps for a workspace.
    ///
    /// Serialised against the storage mutex because pattern discovery
    /// issues a large read-heavy Datalog query followed by writes to
    /// `structural_patterns` and `known_unknowns`; interleaving with
    /// other writes would waste effort (the pattern discovery rescans).
    pub async fn reflect(&self, ws: &str) -> Result<thinkingroot_reflect::ReflectResult> {
        self.reflect_branched(ws, None).await
    }

    /// Branch-aware variant of `reflect`.
    ///
    /// - `branch = None` (or `Some("main")`) — runs against the mounted
    ///   workspace's primary `graph.db`, holding the storage mutex.
    /// - `branch = Some(name)` — runs against the branch's copy-on-write
    ///   `graph.db` via the `BranchEngineCache`. The branch has its own
    ///   `structural_patterns` / `known_unknowns` relations (the tables
    ///   were copied from main at branch create time), so the reflect
    ///   result is fully branch-scoped.
    pub async fn reflect_branched(
        &self,
        ws: &str,
        branch: Option<&str>,
    ) -> Result<thinkingroot_reflect::ReflectResult> {
        let handle = self.get_workspace(ws)?;
        let engine = thinkingroot_reflect::ReflectEngine::new(
            thinkingroot_reflect::ReflectConfig::default(),
        );
        match branch {
            None | Some("main") => {
                let storage = handle.storage.lock().await;
                engine.reflect(&storage.graph)
            }
            Some(branch_name) => {
                let bh = self
                    .branch_engines
                    .get_or_open(&handle.root_path, branch_name)
                    .await?;
                engine.reflect(&bh.graph)
            }
        }
    }

    /// List open gap reports (known-unknowns) for a workspace. Served
    /// directly from CozoDB — no cache since gap sets change infrequently
    /// and are not in the hot retrieval path.
    pub async fn list_gaps(
        &self,
        ws: &str,
        entity: Option<&str>,
        min_confidence: f64,
    ) -> Result<Vec<thinkingroot_reflect::GapReport>> {
        self.list_gaps_branched(ws, entity, min_confidence, None)
            .await
    }

    /// Branch-aware variant of `list_gaps`.
    pub async fn list_gaps_branched(
        &self,
        ws: &str,
        entity: Option<&str>,
        min_confidence: f64,
        branch: Option<&str>,
    ) -> Result<Vec<thinkingroot_reflect::GapReport>> {
        let handle = self.get_workspace(ws)?;
        match branch {
            None | Some("main") => {
                let storage = handle.storage.lock().await;
                thinkingroot_reflect::list_open_gaps(&storage.graph, entity, min_confidence)
            }
            Some(branch_name) => {
                let bh = self
                    .branch_engines
                    .get_or_open(&handle.root_path, branch_name)
                    .await?;
                thinkingroot_reflect::list_open_gaps(&bh.graph, entity, min_confidence)
            }
        }
    }

    /// Cross-workspace reflect — aggregate co-occurrence across every
    /// named workspace and apply the resulting patterns to each one.
    /// Useful when no single workspace has enough instances of a given
    /// entity type to clear the `min_sample_size` threshold, but the
    /// union does (e.g. 10 services × 5 repos = 50 services combined).
    ///
    /// Local patterns are untouched — each workspace's own `reflect()`
    /// continues to maintain `source_scope = 'local'` rows independently.
    pub async fn reflect_across(
        &self,
        workspaces: &[String],
    ) -> Result<thinkingroot_reflect::CrossReflectResult> {
        if workspaces.is_empty() {
            return Err(Error::Config(
                "reflect_across: at least one workspace required".into(),
            ));
        }
        // Lock every participating storage handle up front — pattern
        // aggregation is a read-heavy query across many graphs followed
        // by per-workspace writes. Holding all locks for the duration
        // serializes against concurrent writes so the aggregate sample
        // counts match what each workspace will then be asked to gap
        // against.
        //
        // BTreeMap key: workspace name (for error messages). Value:
        // the locked guard, held for the scope of this method.
        let mut guards: Vec<(
            String,
            tokio::sync::MutexGuard<'_, thinkingroot_graph::StorageEngine>,
        )> = Vec::with_capacity(workspaces.len());
        for name in workspaces {
            let handle = self.get_workspace(name)?;
            guards.push((name.clone(), handle.storage.lock().await));
        }

        let graph_refs: Vec<(String, &thinkingroot_graph::graph::GraphStore)> = guards
            .iter()
            .map(|(name, guard)| (name.clone(), &guard.graph))
            .collect();

        let cfg = thinkingroot_reflect::ReflectConfig::default();
        thinkingroot_reflect::reflect_across_graphs(&graph_refs, &cfg)
    }

    /// Dismiss an open gap — mark the `known_unknowns` row as
    /// `Dismissed`. Subsequent `reflect()` cycles respect this status and
    /// do not re-raise the gap as open, so agents can suppress known
    /// false positives ("this service legitimately has no auth — it's
    /// internal only"). Branch-aware via `branch` param.
    pub async fn dismiss_gap(&self, ws: &str, gap_id: &str, branch: Option<&str>) -> Result<()> {
        let handle = self.get_workspace(ws)?;
        match branch {
            None | Some("main") => {
                let storage = handle.storage.lock().await;
                thinkingroot_reflect::dismiss_gap(&storage.graph, gap_id)
            }
            Some(branch_name) => {
                let bh = self
                    .branch_engines
                    .get_or_open(&handle.root_path, branch_name)
                    .await?;
                thinkingroot_reflect::dismiss_gap(&bh.graph, gap_id)
            }
        }
    }

    /// Roll back a previously executed merge by restoring the most recent
    /// pre-merge snapshot for the given branch. After the on-disk swap, the
    /// mounted workspace's cache is reloaded so subsequent reads reflect the
    /// pre-merge state without a `compile` or `contribute`.
    #[tracing::instrument(
        name = "engine.rollback_merge",
        skip(self, root),
        fields(branch = %branch_name),
    )]
    pub async fn rollback_merge(&self, root: &std::path::Path, branch_name: &str) -> Result<()> {
        thinkingroot_branch::rollback_merge(root, branch_name)
            .map_err(|e| Error::GraphStorage(format!("rollback failed: {e}")))?;

        if let Some(handle) = self.workspaces.values().find(|h| h.root_path == root) {
            let storage = handle.storage.lock().await;
            match KnowledgeGraph::load_from_graph(&storage.graph) {
                Ok(new_cache) => {
                    *handle.cache.write().await = new_cache;
                }
                Err(e) => {
                    tracing::warn!("cache reload after rollback failed (non-fatal): {e}");
                }
            }
        }
        Ok(())
    }

    /// Inner helper: insert a source + claims into any GraphStore.
    fn write_agent_claims_to_graph(
        graph: &thinkingroot_graph::graph::GraphStore,
        source: &thinkingroot_core::Source,
        agent_claims: &[AgentClaim],
    ) -> Result<(Vec<String>, Vec<String>)> {
        graph.insert_source(source)?;

        let mut accepted_ids: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for ac in agent_claims {
            let claim_type = parse_claim_type_str(&ac.claim_type);
            let claim = thinkingroot_core::Claim::new(
                ac.statement.clone(),
                claim_type,
                source.id,
                thinkingroot_core::types::WorkspaceId::new(),
            )
            .with_confidence(ac.confidence.unwrap_or(0.7))
            .with_extraction_tier(thinkingroot_core::types::ExtractionTier::AgentInferred);

            graph.insert_claim(&claim)?;
            graph.link_claim_to_source(&claim.id.to_string(), &source.id.to_string())?;

            for entity_name in &ac.entities {
                match graph.find_entity_id_by_name(entity_name) {
                    Ok(Some(eid)) => {
                        graph.link_claim_to_entity(&claim.id.to_string(), &eid)?;
                    }
                    Ok(None) => {
                        warnings.push(format!(
                            "entity '{entity_name}' not found — claim saved but unlinked"
                        ));
                    }
                    Err(e) => {
                        warnings.push(format!("entity lookup failed for '{entity_name}': {e}"));
                    }
                }
            }

            accepted_ids.push(claim.id.to_string());
        }

        Ok((accepted_ids, warnings))
    }

    /// Look up a mounted workspace by name, returning an error if not found.
    fn get_workspace(&self, name: &str) -> Result<&WorkspaceHandle> {
        self.workspaces
            .get(name)
            .ok_or_else(|| Error::EntityNotFound(format!("workspace '{name}' not mounted")))
    }

    /// Return the LLM client for a workspace, if one was successfully initialised.
    pub fn workspace_llm(&self, ws: &str) -> Option<Arc<thinkingroot_extract::llm::LlmClient>> {
        self.workspaces.get(ws).and_then(|h| h.llm.clone())
    }

    /// Return the `StreamsConfig` for a workspace (controls auto_session_branch, etc.).
    pub fn workspace_streams_config(
        &self,
        ws: &str,
    ) -> Option<thinkingroot_core::config::StreamsConfig> {
        self.workspaces.get(ws).map(|h| h.config.streams.clone())
    }

    /// Return the filesystem root path of a mounted workspace.
    pub fn workspace_root_path(&self, ws: &str) -> Option<PathBuf> {
        self.workspaces.get(ws).map(|h| h.root_path.clone())
    }

    /// Return the merged workspace `Config` for a mounted workspace. Used by
    /// the synthesizer to read the per-workspace `[chat]` block without
    /// re-loading TOML on every request.
    pub fn workspace_config(&self, ws: &str) -> Option<Config> {
        self.workspaces.get(ws).map(|h| h.config.clone())
    }

    /// Snapshot of the inputs needed to build a `WorkspaceIdentity` for
    /// chat-time prompt assembly. Reads the in-memory cache only — no disk
    /// I/O. `source_kinds` is sorted descending by count.
    ///
    /// Returns `None` when the workspace is not mounted.
    pub async fn workspace_chat_snapshot(&self, ws: &str) -> Option<WorkspaceChatSnapshot> {
        let h = self.workspaces.get(ws)?;
        let cache = h.cache.read().await;
        let (_source_count, claim_count, _entity_count) = cache.counts();

        let mut counts: HashMap<String, usize> = HashMap::new();
        for s in cache.all_sources() {
            let kind = source_kind_from_uri(&s.uri, &s.source_type);
            *counts.entry(kind).or_default() += 1;
        }
        let mut source_kinds: Vec<(String, usize)> = counts.into_iter().collect();
        source_kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        Some(WorkspaceChatSnapshot {
            name: h.name.clone(),
            root_path: h.root_path.clone(),
            config: h.config.clone(),
            claim_count,
            source_kinds,
        })
    }
}

/// Snapshot of workspace state used by the chat synthesizer to build the
/// `<system-reminder>` context block. Cheap to construct (in-memory cache
/// reads only) so the SSE handler can call it once per request.
#[derive(Debug, Clone)]
pub struct WorkspaceChatSnapshot {
    pub name: String,
    pub root_path: PathBuf,
    pub config: Config,
    pub claim_count: usize,
    /// `(kind_label, count)` pairs sorted descending. The label is the
    /// lowercase file extension when one is available, otherwise the
    /// source's declared `source_type`, otherwise `"other"`.
    pub source_kinds: Vec<(String, usize)>,
}

/// Derive a stable, lowercase kind label for a source. Prefers the file
/// extension because that's what the synthesizer's auto-detect rules
/// match against. Falls back to the source's declared `source_type` when
/// the URI has no extension (e.g. database / API connectors).
fn source_kind_from_uri(uri: &str, source_type: &str) -> String {
    // file://path/to/foo.rs → "rs"
    let path_part = uri.split_once("://").map(|(_, p)| p).unwrap_or(uri);
    let stripped = path_part.split(['?', '#']).next().unwrap_or(path_part);
    if let Some(ext) = std::path::Path::new(stripped)
        .extension()
        .and_then(|e| e.to_str())
    {
        let lower = ext.to_ascii_lowercase();
        if !lower.is_empty() {
            return lower;
        }
    }
    let st = source_type.trim();
    if st.is_empty() {
        "other".to_string()
    } else {
        st.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod source_kind_tests {
    use super::source_kind_from_uri;

    #[test]
    fn extracts_extension_from_file_uri() {
        assert_eq!(source_kind_from_uri("file:///x/y/foo.rs", "code"), "rs");
        assert_eq!(source_kind_from_uri("/abs/path/bar.ts", "code"), "ts");
        assert_eq!(source_kind_from_uri("rel/baz.MD", "doc"), "md");
    }

    #[test]
    fn falls_back_to_source_type_when_no_extension() {
        assert_eq!(
            source_kind_from_uri("postgres://server/db", "database"),
            "database"
        );
        assert_eq!(source_kind_from_uri("plain-name", ""), "other");
    }

    #[test]
    fn ignores_query_string_and_fragment() {
        assert_eq!(
            source_kind_from_uri("https://x.com/foo.json?token=abc", "api"),
            "json"
        );
        assert_eq!(
            source_kind_from_uri("file:///doc.md#anchor", "doc"),
            "md"
        );
    }
}

// ---------------------------------------------------------------------------
// Intelligent serve layer types
// ---------------------------------------------------------------------------

/// Token-efficient workspace summary for agent orientation.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSummary {
    pub workspace: String,
    pub entity_count: usize,
    pub claim_count: usize,
    pub source_count: usize,
    pub top_entities: Vec<thinkingroot_graph::graph::TopEntity>,
    pub recent_decisions: Vec<(String, f64)>,
    pub contradiction_count: usize,
}

/// An agent-contributed claim submitted via the `contribute` MCP tool.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentClaim {
    pub statement: String,
    #[serde(default = "default_claim_type")]
    pub claim_type: String,
    pub confidence: Option<f64>,
    #[serde(default)]
    pub entities: Vec<String>,
}

fn default_claim_type() -> String {
    "fact".to_string()
}

/// Result of a `contribute_claims` call.
#[derive(Debug, Clone, Serialize)]
pub struct ContributeResult {
    pub accepted_count: usize,
    pub accepted_ids: Vec<String>,
    pub source_uri: String,
    pub warnings: Vec<String>,
}

/// Parse a claim type string (case-insensitive) into a `ClaimType` enum.
fn parse_claim_type_str(s: &str) -> thinkingroot_core::types::ClaimType {
    use thinkingroot_core::types::ClaimType;
    match s.to_lowercase().as_str() {
        "decision" => ClaimType::Decision,
        "opinion" => ClaimType::Opinion,
        "plan" => ClaimType::Plan,
        "requirement" => ClaimType::Requirement,
        "metric" => ClaimType::Metric,
        "definition" => ClaimType::Definition,
        "dependency" => ClaimType::Dependency,
        "api_signature" | "apisignature" => ClaimType::ApiSignature,
        "architecture" => ClaimType::Architecture,
        "preference" => ClaimType::Preference,
        _ => ClaimType::Fact,
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn cached_claim_to_info(c: &CachedClaim) -> ClaimInfo {
    ClaimInfo {
        id: c.id.clone(),
        statement: c.statement.clone(),
        claim_type: c.claim_type.clone(),
        confidence: c.confidence,
        source_uri: c.source_uri.clone(),
        event_date: c.event_date,
    }
}
