use rand::Rng;
use tempfile::TempDir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, ContradictionId, Entity, EntityType, Sensitivity, Source,
    SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;

use crate::Scale;

pub struct Fixture {
    pub graph: GraphStore,
    pub scale: Scale,
    pub sample_entity_name: String,
    pub sample_entity_id: String,
    pub sample_claim_id: String,
    pub sample_source_id: String,
    pub entity_names: Vec<String>,
    _tmpdir: TempDir,
}

impl Fixture {
    pub fn generate(scale: Scale) -> Self {
        let tmpdir = TempDir::new().expect("failed to create tempdir");
        let graph_dir = tmpdir.path().join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();
        let graph = GraphStore::init(&graph_dir).expect("failed to init graph");

        let mut rng = rand::rng();
        let workspace_id = WorkspaceId::new();

        // Generate sources
        let source_count = scale.entity_count() / 5;
        let mut source_ids = Vec::with_capacity(source_count);
        for i in 0..source_count {
            let source = Source::new(format!("src/module_{i}.rs"), SourceType::File)
                .with_hash(ContentHash::from_bytes(format!("hash_{i}").as_bytes()))
                .with_trust(TrustLevel::Trusted)
                .with_size(rng.random_range(500_u64..50_000_u64));
            source_ids.push(source.id.to_string());
            graph.insert_source(&source).unwrap();
        }

        // Generate entities
        let entity_count = scale.entity_count();
        let mut entity_ids = Vec::with_capacity(entity_count);
        let mut entity_names = Vec::with_capacity(entity_count);

        let adjectives = [
            "auth", "billing", "cache", "data", "event", "fast", "graph", "http", "index", "json",
            "key", "log", "mesh", "net", "ops", "parse", "query", "route", "store", "token",
            "user", "vault",
        ];
        let nouns = [
            "service",
            "handler",
            "manager",
            "engine",
            "worker",
            "pipeline",
            "processor",
            "resolver",
            "validator",
            "scheduler",
            "monitor",
            "gateway",
            "adapter",
            "proxy",
            "bridge",
            "controller",
        ];
        let entity_types = [
            EntityType::Service,
            EntityType::Module,
            EntityType::Function,
            EntityType::Api,
            EntityType::Library,
            EntityType::System,
            EntityType::File,
            EntityType::Config,
            EntityType::Team,
            EntityType::Concept,
        ];

        for i in 0..entity_count {
            let adj = adjectives[i % adjectives.len()];
            let noun = nouns[i % nouns.len()];
            let name = format!("{adj}-{noun}-{i}");
            let etype = entity_types[i % entity_types.len()];

            let entity =
                Entity::new(&name, etype).with_description(format!("Benchmark entity {i} ({adj} {noun})"));
            entity_ids.push(entity.id.to_string());
            entity_names.push(name);
            graph.insert_entity(&entity).unwrap();
        }

        // Generate claims with realistic type distribution
        let claim_count = scale.claim_count();
        let mut claim_ids = Vec::with_capacity(claim_count);

        let claim_type_weights: &[(ClaimType, u32)] = &[
            (ClaimType::Fact, 40),
            (ClaimType::Decision, 15),
            (ClaimType::Architecture, 10),
            (ClaimType::Dependency, 10),
            (ClaimType::ApiSignature, 8),
            (ClaimType::Requirement, 7),
            (ClaimType::Definition, 5),
            (ClaimType::Plan, 3),
            (ClaimType::Metric, 2),
        ];
        let total_weight: u32 = claim_type_weights.iter().map(|(_, w)| w).sum();

        for i in 0..claim_count {
            let mut roll = rng.random_range(0..total_weight);
            let mut ctype = ClaimType::Fact;
            for (ct, w) in claim_type_weights {
                if roll < *w {
                    ctype = *ct;
                    break;
                }
                roll -= w;
            }

            let conf: f64 = (0.75 + rng.random_range(-0.3..0.3_f64)).clamp(0.3, 1.0);
            let source_idx = i % source_ids.len();

            let claim = Claim::new(
                format!("Claim {i}: The {ctype:?} regarding module_{i} is established"),
                ctype,
                source_ids[source_idx].parse().unwrap(),
                workspace_id,
            )
            .with_confidence(conf)
            .with_sensitivity(Sensitivity::Internal);

            let claim_id_str = claim.id.to_string();
            claim_ids.push(claim_id_str.clone());
            graph.insert_claim(&claim).unwrap();

            graph
                .link_claim_to_source(&claim_id_str, &source_ids[source_idx])
                .unwrap();

            let entity_idx = i % entity_ids.len();
            graph
                .link_claim_to_entity(&claim_id_str, &entity_ids[entity_idx])
                .unwrap();
        }

        // Generate relations (power-law distribution)
        let relation_count = scale.relation_count();
        let rel_types = [
            "depends_on",
            "uses",
            "contains",
            "part_of",
            "calls",
            "configured_by",
            "tested_by",
            "implements",
            "owned_by",
            "related_to",
        ];

        for i in 0..relation_count {
            let from_idx = if rng.random_range(0..100_u32) < 5 {
                rng.random_range(0..entity_count.min(25))
            } else {
                rng.random_range(0..entity_count)
            };
            let to_idx = rng.random_range(0..entity_count);
            if from_idx == to_idx {
                continue;
            }
            let rel_type = rel_types[i % rel_types.len()];
            let strength: f64 = rng.random_range(0.3..1.0_f64);
            let _ = graph.link_entities(
                &entity_ids[from_idx],
                &entity_ids[to_idx],
                rel_type,
                strength,
            );
        }

        // Generate contradictions (1% of claims)
        let contradiction_count = claim_count / 100;
        for i in 0..contradiction_count {
            let a = &claim_ids[i * 2 % claim_ids.len()];
            let b = &claim_ids[(i * 2 + 1) % claim_ids.len()];
            let contra_id = ContradictionId::new().to_string();
            let _ = graph.insert_contradiction(
                &contra_id,
                a,
                b,
                &format!("Contradiction {i}: conflicting claims"),
            );
        }

        // Pick sample values
        let sample_entity_name = entity_names[entity_count / 2].clone();
        let sample_entity_id = entity_ids[entity_count / 2].clone();
        let sample_claim_id = claim_ids[claim_count / 2].clone();
        let sample_source_id = source_ids[source_count / 2].clone();

        Self {
            graph,
            scale,
            sample_entity_name,
            sample_entity_id,
            sample_claim_id,
            sample_source_id,
            entity_names,
            _tmpdir: tmpdir,
        }
    }
}
