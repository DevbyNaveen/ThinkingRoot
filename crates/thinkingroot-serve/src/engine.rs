use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;

pub use crate::pipeline::PipelineResult;
use thinkingroot_core::{Config, Error, Result};
use thinkingroot_graph::StorageEngine;
use thinkingroot_verify::Verifier;
pub use thinkingroot_verify::verifier::VerificationResult;

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
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationInfo {
    pub target: String,
    pub relation_type: String,
    pub strength: f64,
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
    storage: Arc<Mutex<StorageEngine>>,
    config: Config,
}

// ---------------------------------------------------------------------------
// Artifact type <-> filename mapping
// ---------------------------------------------------------------------------

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
];

// ---------------------------------------------------------------------------
// QueryEngine
// ---------------------------------------------------------------------------

pub struct QueryEngine {
    workspaces: HashMap<String, WorkspaceHandle>,
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
        }
    }

    /// Mount a workspace by name, opening the `.thinkingroot/` data directory
    /// and loading the config and storage engine.
    pub async fn mount(&mut self, name: String, root_path: PathBuf) -> Result<()> {
        let data_dir = root_path.join(".thinkingroot");
        if !data_dir.exists() {
            return Err(Error::Config(format!(
                "no .thinkingroot directory found at {}",
                root_path.display()
            )));
        }

        let config = Config::load_merged(&root_path)?;
        let storage = StorageEngine::init(&data_dir).await?;

        self.workspaces.insert(
            name.clone(),
            WorkspaceHandle {
                name,
                root_path,
                storage: Arc::new(Mutex::new(storage)),
                config,
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
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let mut result = Vec::with_capacity(self.workspaces.len());
        for handle in self.workspaces.values() {
            let storage = handle.storage.lock().await;
            let (source_count, claim_count, entity_count) = storage.graph.get_counts()?;
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
    pub async fn list_entities(&self, ws: &str) -> Result<Vec<EntityInfo>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        let entities = storage.graph.get_all_entities()?;
        let mut result = Vec::with_capacity(entities.len());

        for (id, name, entity_type) in &entities {
            let claims = storage.graph.get_claims_for_entity(id)?;
            result.push(EntityInfo {
                id: id.clone(),
                name: name.clone(),
                entity_type: entity_type.clone(),
                claim_count: claims.len(),
            });
        }

        Ok(result)
    }

    /// Get detailed information about a single entity by name (case-insensitive).
    pub async fn get_entity(&self, ws: &str, name: &str) -> Result<EntityDetail> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        let entities = storage.graph.get_all_entities()?;
        let lower_name = name.to_lowercase();

        let (entity_id, entity_name, entity_type) = entities
            .iter()
            .find(|(_, n, _)| n.to_lowercase() == lower_name)
            .ok_or_else(|| Error::EntityNotFound(name.to_string()))?;

        // Get claims with source information.
        let raw_claims = storage
            .graph
            .get_claims_with_sources_for_entity(entity_id)?;

        let claims: Vec<ClaimInfo> = raw_claims
            .into_iter()
            .map(
                |(id, statement, claim_type, source_uri, confidence)| ClaimInfo {
                    id,
                    statement,
                    claim_type,
                    confidence,
                    source_uri,
                },
            )
            .collect();

        // Get aliases.
        let aliases = storage.graph.get_aliases_for_entity(entity_id)?;

        // Get relations.
        let raw_relations = storage.graph.get_relations_for_entity(entity_name)?;
        let relations: Vec<RelationInfo> = raw_relations
            .into_iter()
            .map(|(target, relation_type, strength)| RelationInfo {
                target,
                relation_type,
                strength,
            })
            .collect();

        Ok(EntityDetail {
            id: entity_id.clone(),
            name: entity_name.clone(),
            entity_type: entity_type.clone(),
            aliases,
            claims,
            relations,
        })
    }

    /// List claims with optional filtering by type, min confidence, limit, and offset.
    pub async fn list_claims(&self, ws: &str, filter: ClaimFilter) -> Result<Vec<ClaimInfo>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        // If filtering by entity name, find the entity first.
        if let Some(ref entity_name) = filter.entity_name {
            let entities = storage.graph.get_all_entities()?;
            let lower_name = entity_name.to_lowercase();
            let entity = entities
                .iter()
                .find(|(_, n, _)| n.to_lowercase() == lower_name);

            if let Some((entity_id, _, _)) = entity {
                let raw = storage
                    .graph
                    .get_claims_with_sources_for_entity(entity_id)?;

                let mut claims: Vec<ClaimInfo> = raw
                    .into_iter()
                    .filter(|(_, _, ct, _, conf)| {
                        let type_ok = filter
                            .claim_type
                            .as_ref()
                            .is_none_or(|t| t.eq_ignore_ascii_case(ct));
                        let conf_ok = filter.min_confidence.is_none_or(|min| *conf >= min);
                        type_ok && conf_ok
                    })
                    .map(
                        |(id, statement, claim_type, source_uri, confidence)| ClaimInfo {
                            id,
                            statement,
                            claim_type,
                            confidence,
                            source_uri,
                        },
                    )
                    .collect();

                let offset = filter.offset.unwrap_or(0);
                if offset > 0 && offset < claims.len() {
                    claims = claims.split_off(offset);
                } else if offset >= claims.len() {
                    claims.clear();
                }
                if let Some(limit) = filter.limit {
                    claims.truncate(limit);
                }
                return Ok(claims);
            } else {
                return Ok(Vec::new());
            }
        }

        // No entity filter — use type-based or full listing.
        let raw = if let Some(ref claim_type) = filter.claim_type {
            storage.graph.get_claims_by_type(claim_type)?
        } else {
            storage.graph.get_all_claims_with_sources()?
        };

        let mut claims: Vec<ClaimInfo> = raw
            .into_iter()
            .filter(|(_, _, _, conf, _)| filter.min_confidence.is_none_or(|min| *conf >= min))
            .map(
                |(id, statement, claim_type, confidence, source_uri)| ClaimInfo {
                    id,
                    statement,
                    claim_type,
                    confidence,
                    source_uri,
                },
            )
            .collect();

        let offset = filter.offset.unwrap_or(0);
        if offset > 0 && offset < claims.len() {
            claims = claims.split_off(offset);
        } else if offset >= claims.len() {
            claims.clear();
        }
        if let Some(limit) = filter.limit {
            claims.truncate(limit);
        }

        Ok(claims)
    }

    /// Get relations for a specific entity by name.
    pub async fn get_relations(&self, ws: &str, entity: &str) -> Result<Vec<RelationInfo>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        let raw = storage.graph.get_relations_for_entity(entity)?;
        Ok(raw
            .into_iter()
            .map(|(target, relation_type, strength)| RelationInfo {
                target,
                relation_type,
                strength,
            })
            .collect())
    }

    /// Get all relations in the workspace as (from, to, relation_type, strength) tuples.
    pub async fn get_all_relations(&self, ws: &str) -> Result<Vec<(String, String, String, f64)>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        let raw = storage.graph.get_all_relations()?;
        Ok(raw
            .into_iter()
            .map(|(from, to, rtype, _from_type, _to_type, strength)| (from, to, rtype, strength))
            .collect())
    }

    /// List all known artifact types and whether each is available on disk.
    pub async fn list_artifacts(&self, ws: &str) -> Result<Vec<ArtifactInfo>> {
        let handle = self.get_workspace(ws)?;
        let artifacts_dir = handle.root_path.join(".thinkingroot").join("artifacts");

        let mut result = Vec::with_capacity(ARTIFACT_TYPES.len());
        for &atype in ARTIFACT_TYPES {
            let available = if let Some(filename) = artifact_filename(atype) {
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
    pub async fn list_sources(&self, ws: &str) -> Result<Vec<SourceInfo>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        Ok(storage
            .graph
            .get_all_sources()?
            .into_iter()
            .map(|(id, uri, source_type)| SourceInfo {
                id,
                uri,
                source_type,
            })
            .collect())
    }

    /// Read the content of a specific artifact.
    pub async fn get_artifact(&self, ws: &str, artifact_type: &str) -> Result<ArtifactContent> {
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

    /// Run health/verification checks on the workspace.
    pub async fn health(&self, ws: &str) -> Result<VerificationResult> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;
        let verifier = Verifier::new(&handle.config);
        verifier.verify(&storage.graph)
    }

    /// Run the full pipeline for a mounted workspace.
    pub async fn compile(&self, ws: &str) -> Result<PipelineResult> {
        let handle = self.get_workspace(ws)?;
        crate::pipeline::run_pipeline(&handle.root_path).await
    }

    /// Search the workspace using vector similarity + keyword fallback.
    pub async fn search(&self, ws: &str, query: &str, top_k: usize) -> Result<SearchResult> {
        let handle = self.get_workspace(ws)?;
        let mut storage = handle.storage.lock().await;

        let mut entity_hits: Vec<EntitySearchHit> = Vec::new();
        let mut claim_hits: Vec<ClaimSearchHit> = Vec::new();
        let mut seen_entity_ids: HashSet<String> = HashSet::new();
        let mut seen_claim_ids: HashSet<String> = HashSet::new();

        // 1) Try vector search first.
        let vector_results = storage.vector.search(query, top_k * 2)?;

        // Hoist lookups outside the loop to avoid O(n) graph queries per result.
        let all_entities = storage.graph.get_all_entities()?;
        let all_claims = storage.graph.get_all_claims_with_sources()?;

        for (key, _metadata, score) in &vector_results {
            if *score < 0.1 {
                continue;
            }

            // Vector keys are stored with type prefixes ("entity:{id}", "claim:{id}").
            // Strip the prefix to get the bare graph ID for lookup.
            if let Some(bare_id) = key.strip_prefix("entity:") {
                if let Some((eid, ename, etype)) =
                    all_entities.iter().find(|(eid, _, _)| eid == bare_id)
                {
                    if seen_entity_ids.insert(eid.clone()) {
                        let claims = storage.graph.get_claims_for_entity(eid)?;
                        entity_hits.push(EntitySearchHit {
                            id: eid.clone(),
                            name: ename.clone(),
                            entity_type: etype.clone(),
                            claim_count: claims.len(),
                            relevance: *score,
                        });
                    }
                }
                continue;
            }

            if let Some(bare_id) = key.strip_prefix("claim:") {
                if let Some((cid, stmt, ctype, conf, uri)) =
                    all_claims.iter().find(|(cid, _, _, _, _)| cid == bare_id)
                {
                    if seen_claim_ids.insert(cid.clone()) {
                        claim_hits.push(ClaimSearchHit {
                            id: cid.clone(),
                            statement: stmt.clone(),
                            claim_type: ctype.clone(),
                            confidence: *conf,
                            source_uri: uri.clone(),
                            relevance: *score,
                        });
                    }
                }
            }
        }

        // 2) Keyword fallback if vector didn't return enough.
        if entity_hits.len() + claim_hits.len() < top_k {
            let kw_entities = storage.graph.search_entities(query)?;
            for (eid, ename, etype) in kw_entities {
                if seen_entity_ids.insert(eid.clone()) {
                    let claims = storage.graph.get_claims_for_entity(&eid)?;
                    entity_hits.push(EntitySearchHit {
                        id: eid,
                        name: ename,
                        entity_type: etype,
                        claim_count: claims.len(),
                        relevance: 0.5, // default relevance for keyword matches
                    });
                }
            }

            let kw_claims = storage.graph.search_claims(query)?;
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

    /// List tracked contradictions in the workspace.
    pub async fn list_contradictions(&self, ws: &str) -> Result<Vec<ContradictionInfo>> {
        let handle = self.get_workspace(ws)?;
        let storage = handle.storage.lock().await;

        Ok(storage
            .graph
            .get_contradictions()?
            .into_iter()
            .map(
                |(id, claim_a, claim_b, explanation, status)| ContradictionInfo {
                    id,
                    claim_a,
                    claim_b,
                    explanation,
                    status,
                },
            )
            .collect())
    }

    /// Alias for `health()` — delegates to the same verification logic.
    pub async fn verify(&self, ws: &str) -> Result<VerificationResult> {
        self.health(ws).await
    }

    /// Look up a mounted workspace by name, returning an error if not found.
    fn get_workspace(&self, name: &str) -> Result<&WorkspaceHandle> {
        self.workspaces
            .get(name)
            .ok_or_else(|| Error::EntityNotFound(format!("workspace '{name}' not mounted")))
    }
}
