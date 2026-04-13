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

        // Write resolved entities to graph.
        for entity in &resolved_entities {
            self.graph.insert_entity(entity)?;
        }

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

        for claim in &extraction.claims {
            self.graph.insert_claim(claim)?;
            output.added_claim_ids.push(claim.id.to_string());
            self.graph
                .link_claim_to_source(&claim.id.to_string(), &claim.source.to_string())?;

            // Link claim to its referenced entities.
            if let Some(entity_names) = extraction.claim_entity_names.get(&claim.id) {
                for name in entity_names {
                    if let Some(&entity_id) = name_to_entity.get(&name.to_lowercase()) {
                        self.graph
                            .link_claim_to_entity(&claim.id.to_string(), &entity_id.to_string())?;
                    }
                }
            }

            output.claims_linked += 1;
        }

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
        for sourced_relation in &deduped_relations {
            let relation = &sourced_relation.relation;
            let from_id = entity_id_map
                .get(&relation.from)
                .copied()
                .unwrap_or(relation.from);
            let to_id = entity_id_map
                .get(&relation.to)
                .copied()
                .unwrap_or(relation.to);

            self.graph.link_entities_for_source(
                &sourced_relation.source.to_string(),
                &from_id.to_string(),
                &to_id.to_string(),
                &format!("{:?}", relation.relation_type),
                relation.strength.value(),
            )?;
            output.relations_linked += 1;
        }

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
        let negation_pairs = [
            ("is", "is not"),
            ("uses", "does not use"),
            ("has", "does not have"),
            ("supports", "does not support"),
            ("requires", "does not require"),
            ("enabled", "disabled"),
            ("true", "false"),
            ("yes", "no"),
            ("deprecated", "active"),
            ("removed", "added"),
            // New patterns:
            ("migrated from", "uses"),
            ("replaced by", "depends on"),
            ("optional", "required"),
            ("synchronous", "asynchronous"),
            ("mutable", "immutable"),
            ("public", "private"),
            ("internal", "external"),
            ("production", "development"),
            ("legacy", "current"),
            ("before", "after"),
        ];

        for group in entity_claims.values() {
            for i in 0..group.len() {
                for j in (i + 1)..group.len() {
                    let a = &group[i];
                    let b = &group[j];
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

                        // Auto-supersession: if confidence difference > 0.15,
                        // supersede the lower-confidence claim.
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

                    // Second pass: claims with high word overlap but different key terms
                    // may indicate contradictions the keyword pairs missed.
                    // Example: "uses PostgreSQL" vs "uses MySQL" — same structure, different DB.
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

                        // High overlap (same structure) but not identical (different values)
                        // suggests a potential contradiction.
                        if jaccard > 0.6 && jaccard < 0.95 && a.claim_type == b.claim_type {
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
    if s.len() <= max { s } else { &s[..max] }
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
