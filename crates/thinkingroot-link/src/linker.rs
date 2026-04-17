use std::collections::HashMap;
use std::sync::Arc;

use thinkingroot_core::Result;
use thinkingroot_core::types::*;
use thinkingroot_extract::extractor::ExtractionOutput;
use thinkingroot_graph::graph::GraphStore;

use crate::resolution;

/// Callback fired after each entity is resolved (created or merged).
/// Arguments: (done, total)
pub type EntityProgressFn = Arc<dyn Fn(usize, usize) + Send + Sync>;

/// The Linker takes extraction output and builds the knowledge graph:
/// - Resolves duplicate entities
/// - Detects contradictions
/// - Writes everything to the graph store
pub struct Linker<'a> {
    graph: &'a GraphStore,
    progress: Option<EntityProgressFn>,
}

/// Output of the linking stage.
#[derive(Debug, Default)]
pub struct LinkOutput {
    pub entities_created: usize,
    pub entities_merged: usize,
    pub claims_linked: usize,
    pub relations_linked: usize,
    pub contradictions_detected: usize,
    /// Entity IDs that were created or merged in this linking run.
    /// Used by the pipeline for selective artifact compilation.
    pub affected_entity_ids: Vec<String>,
    /// Claim IDs inserted in this linking run (used for surgical vector updates).
    pub added_claim_ids: Vec<String>,
}

impl<'a> Linker<'a> {
    pub fn new(graph: &'a GraphStore) -> Self {
        Self {
            graph,
            progress: None,
        }
    }

    /// Attach a progress callback. Called once per entity resolved.
    /// Arguments: (done, total)
    pub fn with_progress(mut self, f: EntityProgressFn) -> Self {
        self.progress = Some(f);
        self
    }

    /// Link extracted knowledge into the graph.
    pub fn link(&self, extraction: ExtractionOutput) -> Result<LinkOutput> {
        let mut output = LinkOutput::default();

        // Phase 1: Entity resolution.
        let mut resolved_entities = self.graph.get_entities_with_aliases()?;
        let mut entity_id_map: HashMap<EntityId, EntityId> = HashMap::new();
        let total_entities = extraction.entities.len();
        let mut entity_done: usize = 0;

        for new_entity in extraction.entities {
            match resolution::resolve_entity(&new_entity, &resolved_entities) {
                Some(existing_id) => {
                    if let Some(existing) =
                        resolved_entities.iter_mut().find(|e| e.id == existing_id)
                    {
                        entity_id_map.insert(new_entity.id, existing_id);
                        resolution::merge_entities(existing, &new_entity);
                        output.entities_merged += 1;
                        output.affected_entity_ids.push(existing_id.to_string());
                    }
                }
                None => {
                    let new_id = new_entity.id;
                    entity_id_map.insert(new_id, new_id);
                    output.affected_entity_ids.push(new_id.to_string());
                    resolved_entities.push(new_entity);
                    output.entities_created += 1;
                }
            }
            entity_done += 1;
            if let Some(ref pf) = self.progress {
                pf(entity_done, total_entities);
            }
        }

        // Write resolved entities to graph in one batch (100x faster than individual inserts).
        self.graph.insert_entities_batch(&resolved_entities)?;

        // Phase 2: Link claims to sources and entities.
        // Build a name→resolved EntityId lookup (case-insensitive).
        let name_to_entity: HashMap<String, EntityId> = resolved_entities
            .iter()
            .flat_map(|e| {
                let mut names = vec![(e.canonical_name.to_lowercase(), e.id)];
                for alias in &e.aliases {
                    names.push((alias.to_lowercase(), e.id));
                }
                names
            })
            .collect();

        // Batch-insert all claims at once.
        self.graph.insert_claims_batch(&extraction.claims)?;
        output.claims_linked = extraction.claims.len();
        for claim in &extraction.claims {
            output.added_claim_ids.push(claim.id.to_string());
        }

        // Batch-insert claim→source edges.
        let source_edges: Vec<(String, String)> = extraction
            .claims
            .iter()
            .map(|c| (c.id.to_string(), c.source.to_string()))
            .collect();
        self.graph.link_claims_to_sources_batch(&source_edges)?;

        // Collect and batch-insert claim→entity edges.
        let mut entity_edges: Vec<(String, String)> = Vec::new();
        for claim in &extraction.claims {
            if let Some(entity_names) = extraction.claim_entity_names.get(&claim.id) {
                for name in entity_names {
                    if let Some(&entity_id) = name_to_entity.get(&name.to_lowercase()) {
                        entity_edges.push((claim.id.to_string(), entity_id.to_string()));
                    }
                }
            }
        }
        self.graph.link_claims_to_entities_batch(&entity_edges)?;

        // Phase 3: Link relations (with resolved entity IDs).
        // Deduplicate first: keep most-specific type per (from, to) pair.
        let mut deduped_relations = extraction.relations.clone();
        crate::relation_dedup::dedup_relations(&mut deduped_relations);
        let removed = extraction
            .relations
            .len()
            .saturating_sub(deduped_relations.len());
        if removed > 0 {
            tracing::debug!(
                "relation subsumption dedup: removed {} redundant relations",
                removed
            );
        }
        // Batch-insert all relations at once.
        let relation_tuples: Vec<(String, String, String, String, f64)> = deduped_relations
            .iter()
            .map(|sr| {
                let relation = &sr.relation;
                let from_id = entity_id_map
                    .get(&relation.from)
                    .copied()
                    .unwrap_or(relation.from);
                let to_id = entity_id_map
                    .get(&relation.to)
                    .copied()
                    .unwrap_or(relation.to);
                (
                    sr.source.to_string(),
                    from_id.to_string(),
                    to_id.to_string(),
                    format!("{:?}", relation.relation_type),
                    relation.strength.value(),
                )
            })
            .collect();
        self.graph.link_entities_for_source_batch(&relation_tuples)?;
        output.relations_linked = relation_tuples.len();

        // Phase 4: Contradiction detection.
        // Group claims by entity, then look for opposing statements.
        output.contradictions_detected = self.detect_contradictions(
            &extraction.claims,
            &extraction.claim_entity_names,
            &name_to_entity,
        )?;

        tracing::info!(
            "linking complete: {} entities ({} merged), {} claims, {} relations, {} contradictions",
            output.entities_created,
            output.entities_merged,
            output.claims_linked,
            output.relations_linked,
            output.contradictions_detected,
        );

        Ok(output)
    }

    /// Detect contradictions: claims about the same entity with opposing signals.
    /// Uses keyword heuristics for Phase 1 (semantic comparison in Phase 2 with embeddings).
    fn detect_contradictions(
        &self,
        claims: &[Claim],
        claim_entity_names: &HashMap<ClaimId, Vec<String>>,
        name_to_entity: &HashMap<String, EntityId>,
    ) -> Result<usize> {
        // Group claims by entity ID.
        let mut entity_claims: HashMap<EntityId, Vec<&Claim>> = HashMap::new();
        for claim in claims {
            if let Some(entity_names) = claim_entity_names.get(&claim.id) {
                for name in entity_names {
                    if let Some(&eid) = name_to_entity.get(&name.to_lowercase()) {
                        entity_claims.entry(eid).or_default().push(claim);
                    }
                }
            }
        }

        let mut count = 0;

        // Only specific semantic oppositions — NOT generic words like "is"/"has"/"true"
        // which appear in almost every sentence and generate massive false positives.
        let negation_pairs = [
            ("uses", "does not use"),
            ("supports", "does not support"),
            ("requires", "does not require"),
            ("enabled", "disabled"),
            ("deprecated", "active"),
            ("removed", "added"),
            ("optional", "required"),
            ("synchronous", "asynchronous"),
            ("mutable", "immutable"),
            ("public", "private"),
            ("internal", "external"),
            ("production", "development"),
            ("legacy", "current"),
        ];

        // Cap per-entity comparisons to prevent O(n²) explosion on high-degree entities.
        // An entity with 1000 claims would generate 500k pairs — nearly all false positives.
        const MAX_CLAIMS_PER_ENTITY: usize = 50;

        for group in entity_claims.values() {
            // For large groups, take the highest-confidence claims only.
            let window: &[&Claim] = if group.len() > MAX_CLAIMS_PER_ENTITY {
                &group[..MAX_CLAIMS_PER_ENTITY]
            } else {
                group
            };

            for i in 0..window.len() {
                for j in (i + 1)..window.len() {
                    let a = &window[i];
                    let b = &window[j];
                    let a_lower = a.statement.to_lowercase();
                    let b_lower = b.statement.to_lowercase();

                    let is_contradiction = negation_pairs.iter().any(|(pos, neg)| {
                        (a_lower.contains(pos) && b_lower.contains(neg))
                            || (a_lower.contains(neg) && b_lower.contains(pos))
                    });

                    let conf_a = a.confidence.value();
                    let conf_b = b.confidence.value();

                    if is_contradiction {
                        let contradiction =
                            Contradiction::new(a.id, b.id).with_explanation(format!(
                                "Potential conflict: \"{}\" vs \"{}\"",
                                truncate(&a.statement, 80),
                                truncate(&b.statement, 80),
                            ));
                        self.graph.insert_contradiction(
                            &contradiction.id.to_string(),
                            &a.id.to_string(),
                            &b.id.to_string(),
                            contradiction.explanation.as_deref().unwrap_or(""),
                        )?;

                        if (conf_a - conf_b).abs() > 0.15 {
                            if conf_a > conf_b {
                                self.graph
                                    .supersede_claim(&b.id.to_string(), &a.id.to_string())?;
                            } else {
                                self.graph
                                    .supersede_claim(&a.id.to_string(), &b.id.to_string())?;
                            }
                        }

                        count += 1;
                    }

                    // Jaccard pass: same sentence structure, different key terms.
                    // Example: "uses PostgreSQL" vs "uses MySQL" — same structure, different DB.
                    // Threshold raised to 0.80 (was 0.60) to avoid false positives on
                    // related-but-not-contradictory facts about the same entity.
                    if !is_contradiction {
                        let a_words: std::collections::HashSet<&str> =
                            a_lower.split_whitespace().collect();
                        let b_words: std::collections::HashSet<&str> =
                            b_lower.split_whitespace().collect();

                        let intersection = a_words.intersection(&b_words).count();
                        let union = a_words.union(&b_words).count();
                        let jaccard = if union > 0 {
                            intersection as f64 / union as f64
                        } else {
                            0.0
                        };

                        if jaccard > 0.80 && jaccard < 0.95 && a.claim_type == b.claim_type {
                            let contradiction =
                                Contradiction::new(a.id, b.id).with_explanation(format!(
                                    "Potential conflict (Jaccard={jaccard:.2}): \"{}\" vs \"{}\"",
                                    truncate(&a.statement, 80),
                                    truncate(&b.statement, 80),
                                ));
                            self.graph.insert_contradiction(
                                &contradiction.id.to_string(),
                                &a.id.to_string(),
                                &b.id.to_string(),
                                contradiction.explanation.as_deref().unwrap_or(""),
                            )?;

                            if (conf_a - conf_b).abs() > 0.15 {
                                if conf_a > conf_b {
                                    self.graph
                                        .supersede_claim(&b.id.to_string(), &a.id.to_string())?;
                                } else {
                                    self.graph
                                        .supersede_claim(&a.id.to_string(), &b.id.to_string())?;
                                }
                            }

                            count += 1;
                        }
                    }
                }
            }
        }

        Ok(count)
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find the largest char boundary at or before max bytes.
        let boundary = s.char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= max)
            .last()
            .unwrap_or(0);
        &s[..boundary]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store() -> GraphStore {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("thinkingroot-link-{unique}"));
        fs::create_dir_all(&path).unwrap();
        GraphStore::init(&path).unwrap()
    }

    #[test]
    fn linker_merges_against_existing_graph_entities() {
        let store = temp_store();
        let existing = Entity::new("PostgreSQL", EntityType::Database);
        store.insert_entity(&existing).unwrap();

        let source = Source::new("test://new.md".into(), SourceType::File);
        store.insert_source(&source).unwrap();

        let mut extraction = ExtractionOutput::default();
        extraction
            .entities
            .push(Entity::new("Postgresql", EntityType::Database));

        let claim = Claim::new(
            "Postgresql stores transaction data",
            ClaimType::Fact,
            source.id,
            WorkspaceId::new(),
        );
        extraction
            .claim_entity_names
            .insert(claim.id, vec!["Postgresql".into()]);
        extraction.claims.push(claim);

        let linker = Linker::new(&store);
        let result = linker.link(extraction).unwrap();

        assert_eq!(result.entities_created, 0);
        assert_eq!(result.entities_merged, 1);

        let entities = store.get_all_entities().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].1, "PostgreSQL");

        let aliases = store.get_aliases_for_entity(&entities[0].0).unwrap();
        assert!(aliases.iter().any(|alias| alias == "Postgresql"));
    }
}
