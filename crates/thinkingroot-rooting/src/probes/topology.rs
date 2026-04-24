//! Topology probe — structural co-occurrence check for derived claims.
//!
//! A claim derived from parents A and B should talk about entities that the
//! parents actually touch together. If the parents share zero entities in the
//! graph, the derivation lacks structural grounding and we demote the child
//! to `Quarantined`.
//!
//! Non-derived claims (extracted directly from source) have no parents, so
//! the probe returns `skipped`. Week 4 v1 only checks intersection size; a
//! future pass can score path-length or co-occurrence frequency.
//!
//! Non-fatal probe: failure → `Quarantined`, not `Rejected`.

use std::collections::HashSet;

use super::{Probe, ProbeContext, ProbeName, ProbeResult};
use crate::{Result, RootingError};

pub(crate) struct TopologyProbe;

impl Probe for TopologyProbe {
    const NAME: ProbeName = ProbeName::Topology;
    const FATAL: bool = false;

    fn run(&self, ctx: &ProbeContext<'_>) -> Result<ProbeResult> {
        let derivation = match ctx.derivation {
            Some(d) => d,
            None => {
                return Ok(ProbeResult::skipped(
                    ProbeName::Topology,
                    "claim is not derived",
                ));
            }
        };

        if derivation.parent_claim_ids.is_empty() {
            return Ok(ProbeResult::skipped(
                ProbeName::Topology,
                "derivation has no parent claim ids",
            ));
        }

        // Collect each parent's entity set from claim_entity_edges.
        let mut parent_entity_sets: Vec<HashSet<String>> =
            Vec::with_capacity(derivation.parent_claim_ids.len());
        for parent_id in &derivation.parent_claim_ids {
            let entities = ctx
                .graph
                .get_entity_ids_for_claim(&parent_id.to_string())
                .map_err(|e| RootingError::Graph(format!("get_entity_ids_for_claim: {e}")))?;
            parent_entity_sets.push(entities.into_iter().collect());
        }

        // A single-parent derivation passes trivially so long as the parent
        // has any entities — there's nothing to intersect against.
        if parent_entity_sets.len() == 1 {
            let passed = !parent_entity_sets[0].is_empty();
            return Ok(ProbeResult {
                name: ProbeName::Topology,
                score: if passed { 1.0 } else { 0.0 },
                passed,
                detail: if passed {
                    "single-parent derivation — entities present".into()
                } else {
                    "single parent has no linked entities".into()
                },
            });
        }

        // Multi-parent derivation: require at least one shared entity.
        let mut shared = parent_entity_sets[0].clone();
        for set in parent_entity_sets.iter().skip(1) {
            shared = shared.intersection(set).cloned().collect();
            if shared.is_empty() {
                break;
            }
        }

        let passed = !shared.is_empty();
        Ok(ProbeResult {
            name: ProbeName::Topology,
            score: if passed { 1.0 } else { 0.0 },
            passed,
            detail: if passed {
                format!(
                    "{} parent(s) share {} entity (entities)",
                    derivation.parent_claim_ids.len(),
                    shared.len()
                )
            } else {
                "parents share no entities — derivation lacks structural support".into()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RootingConfig;
    use crate::source_store::FileSystemSourceStore;
    use thinkingroot_core::types::{
        Claim, ClaimType, DerivationProof, Entity, EntityType, Source, SourceType, WorkspaceId,
    };

    fn env() -> (
        tempfile::TempDir,
        thinkingroot_graph::graph::GraphStore,
        FileSystemSourceStore,
        RootingConfig,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let graph = thinkingroot_graph::graph::GraphStore::init(dir.path()).unwrap();
        let store = FileSystemSourceStore::new(dir.path()).unwrap();
        let config = RootingConfig::default();
        (dir, graph, store, config)
    }

    fn seed_parent(
        graph: &thinkingroot_graph::graph::GraphStore,
        statement: &str,
        entity_ids: &[&str],
    ) -> Claim {
        let source = Source::new("file:///s.rs".into(), SourceType::File);
        graph.insert_source(&source).unwrap();
        let claim = Claim::new(statement, ClaimType::Fact, source.id, WorkspaceId::new());
        graph.insert_claim(&claim).unwrap();
        for eid in entity_ids {
            graph
                .link_claim_to_entity(&claim.id.to_string(), eid)
                .unwrap();
        }
        claim
    }

    fn seed_entity(graph: &thinkingroot_graph::graph::GraphStore, name: &str) -> Entity {
        let entity = Entity::new(name, EntityType::Service);
        graph.insert_entity(&entity).unwrap();
        entity
    }

    fn build_ctx<'a>(
        graph: &'a thinkingroot_graph::graph::GraphStore,
        store: &'a FileSystemSourceStore,
        config: &'a RootingConfig,
        claim: &'a Claim,
        derivation: Option<&'a DerivationProof>,
    ) -> ProbeContext<'a> {
        ProbeContext {
            claim,
            predicate: None,
            derivation,
            graph,
            store,
            config,
        }
    }

    #[test]
    fn skipped_when_claim_not_derived() {
        let (_dir, graph, store, config) = env();
        let src = Source::new("file:///x.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let claim = Claim::new("x", ClaimType::Fact, src.id, WorkspaceId::new());
        let ctx = build_ctx(&graph, &store, &config, &claim, None);
        let result = TopologyProbe.run(&ctx).unwrap();
        assert!(result.passed);
        assert_eq!(result.score, -1.0);
    }

    #[test]
    fn passes_when_parents_share_entity() {
        let (_dir, graph, store, config) = env();
        let service_entity = seed_entity(&graph, "PaymentService");
        let library_entity = seed_entity(&graph, "Stripe");
        let other_entity = seed_entity(&graph, "Cache");

        let parent_a = seed_parent(
            &graph,
            "parent a",
            &[
                &service_entity.id.to_string(),
                &library_entity.id.to_string(),
            ],
        );
        let parent_b = seed_parent(
            &graph,
            "parent b",
            &[&service_entity.id.to_string(), &other_entity.id.to_string()],
        );

        let derivation = DerivationProof {
            parent_claim_ids: vec![parent_a.id, parent_b.id],
            derivation_rule: "test-rule".into(),
        };

        let src = Source::new("file:///d.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let derived = Claim::new("derived", ClaimType::Fact, src.id, WorkspaceId::new());

        let ctx = build_ctx(&graph, &store, &config, &derived, Some(&derivation));
        let result = TopologyProbe.run(&ctx).unwrap();
        assert!(result.passed);
        assert!(result.detail.contains("share"));
    }

    #[test]
    fn fails_when_parents_share_no_entities() {
        let (_dir, graph, store, config) = env();
        let entity_a = seed_entity(&graph, "AuthService");
        let entity_b = seed_entity(&graph, "BillingService");

        let parent_a = seed_parent(&graph, "parent a", &[&entity_a.id.to_string()]);
        let parent_b = seed_parent(&graph, "parent b", &[&entity_b.id.to_string()]);

        let derivation = DerivationProof {
            parent_claim_ids: vec![parent_a.id, parent_b.id],
            derivation_rule: "test-rule".into(),
        };

        let src = Source::new("file:///d.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let derived = Claim::new("derived", ClaimType::Fact, src.id, WorkspaceId::new());

        let ctx = build_ctx(&graph, &store, &config, &derived, Some(&derivation));
        let result = TopologyProbe.run(&ctx).unwrap();
        assert!(!result.passed);
        assert!(result.detail.contains("share no entities"));
    }

    #[test]
    fn single_parent_passes_when_parent_has_entities() {
        let (_dir, graph, store, config) = env();
        let entity = seed_entity(&graph, "Cache");
        let parent = seed_parent(&graph, "parent", &[&entity.id.to_string()]);

        let derivation = DerivationProof {
            parent_claim_ids: vec![parent.id],
            derivation_rule: "test-rule".into(),
        };

        let src = Source::new("file:///d.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let derived = Claim::new("derived", ClaimType::Fact, src.id, WorkspaceId::new());

        let ctx = build_ctx(&graph, &store, &config, &derived, Some(&derivation));
        let result = TopologyProbe.run(&ctx).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn single_parent_without_entities_fails() {
        let (_dir, graph, store, config) = env();
        let parent = seed_parent(&graph, "empty parent", &[]);

        let derivation = DerivationProof {
            parent_claim_ids: vec![parent.id],
            derivation_rule: "test-rule".into(),
        };

        let src = Source::new("file:///d.rs".into(), SourceType::File);
        graph.insert_source(&src).unwrap();
        let derived = Claim::new("derived", ClaimType::Fact, src.id, WorkspaceId::new());

        let ctx = build_ctx(&graph, &store, &config, &derived, Some(&derivation));
        let result = TopologyProbe.run(&ctx).unwrap();
        assert!(!result.passed);
    }
}
