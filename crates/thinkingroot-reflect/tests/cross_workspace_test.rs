//! Cross-workspace reflect — aggregate co-occurrence discovery across
//! multiple graphs, applied per-workspace.

use tempfile::tempdir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, Entity, EntityType, Source, SourceType, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_reflect::{ReflectConfig, reflect_across_graphs};

struct Fx {
    graph: GraphStore,
    workspace: WorkspaceId,
    source_id: thinkingroot_core::SourceId,
}

impl Fx {
    fn new(dir: &std::path::Path) -> Self {
        let graph = GraphStore::init(dir).unwrap();
        let workspace = WorkspaceId::new();
        let source = Source::new("file:///fx.md".into(), SourceType::Document)
            .with_trust(TrustLevel::Trusted)
            .with_hash(ContentHash("h".into()));
        let source_id = source.id;
        graph.insert_source(&source).unwrap();
        Self {
            graph,
            workspace,
            source_id,
        }
    }

    fn add_service(&self, name: &str, with_extra: bool, extra_type: ClaimType) -> String {
        let entity = Entity::new(name, EntityType::Service);
        let eid = entity.id.to_string();
        self.graph.insert_entity(&entity).unwrap();
        let c1 = Claim::new(
            &format!("{name} has endpoints"),
            ClaimType::ApiSignature,
            self.source_id,
            self.workspace,
        );
        let c1_id = c1.id.to_string();
        self.graph.insert_claim(&c1).unwrap();
        self.graph
            .link_claim_to_source(&c1_id, &self.source_id.to_string())
            .unwrap();
        self.graph.link_claim_to_entity(&c1_id, &eid).unwrap();
        if with_extra {
            let c2 = Claim::new(
                &format!("{name} has spec"),
                extra_type,
                self.source_id,
                self.workspace,
            );
            let c2_id = c2.id.to_string();
            self.graph.insert_claim(&c2).unwrap();
            self.graph
                .link_claim_to_source(&c2_id, &self.source_id.to_string())
                .unwrap();
            self.graph.link_claim_to_entity(&c2_id, &eid).unwrap();
        }
        eid
    }
}

/// Seed `total` services in a fixture, where `covered` have an extra
/// Requirement claim. Returns the uncovered entity ids.
fn seed(fx: &Fx, prefix: &str, total: usize, covered: usize) -> Vec<String> {
    let mut uncovered = Vec::new();
    for i in 0..total {
        let name = if i < covered {
            format!("{prefix}-ok-{i}")
        } else {
            format!("{prefix}-gap-{i}")
        };
        let eid = fx.add_service(&name, i < covered, ClaimType::Requirement);
        if i >= covered {
            uncovered.push(eid);
        }
    }
    uncovered
}

#[test]
fn aggregate_pattern_fires_below_per_workspace_threshold() {
    // Each workspace has 15 services (below default min_sample_size=30).
    // In isolation, no local pattern would fire. Combined (30+ services)
    // the pattern clears threshold.
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let fx_a = Fx::new(dir_a.path());
    let fx_b = Fx::new(dir_b.path());

    // 15 services each; 13 covered, 2 uncovered — 26 total condition
    // entities, 26/30 > 70% co-occurrence.
    seed(&fx_a, "ws-a", 15, 13);
    seed(&fx_b, "ws-b", 15, 13);

    // Neither workspace alone has enough to trigger a local pattern.
    let engine = thinkingroot_reflect::ReflectEngine::new(ReflectConfig::default());
    let r_a = engine.reflect(&fx_a.graph).unwrap();
    let r_b = engine.reflect(&fx_b.graph).unwrap();
    assert!(
        r_a.patterns.is_empty() && r_b.patterns.is_empty(),
        "each workspace alone must be below threshold; got a={}, b={}",
        r_a.patterns.len(),
        r_b.patterns.len()
    );

    // Cross-workspace aggregation should surface the pattern.
    let cross = reflect_across_graphs(
        &[("ws-a".into(), &fx_a.graph), ("ws-b".into(), &fx_b.graph)],
        &ReflectConfig::default(),
    )
    .unwrap();
    assert!(
        !cross.aggregate_patterns.is_empty(),
        "cross reflect must discover the aggregate pattern; got {:?}",
        cross.aggregate_patterns
    );
    let p = cross
        .aggregate_patterns
        .iter()
        .find(|p| {
            p.condition_claim_type == "ApiSignature" && p.expected_claim_type == "Requirement"
        })
        .expect("ApiSignature→Requirement pattern must be in aggregate");
    assert_eq!(p.sample_size, 30, "aggregate sample = 15 + 15 = 30");
    assert!(p.source_scope.starts_with("cross:"));

    // Gaps propagate to both workspaces individually.
    let r_a = cross.per_workspace.get("ws-a").unwrap();
    let r_b = cross.per_workspace.get("ws-b").unwrap();
    assert_eq!(r_a.gaps_created, 2, "ws-a had 2 uncovered services");
    assert_eq!(r_b.gaps_created, 2, "ws-b had 2 uncovered services");
}

#[test]
fn cross_reflect_scope_is_order_independent() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let fx_a = Fx::new(dir_a.path());
    let fx_b = Fx::new(dir_b.path());
    seed(&fx_a, "a", 15, 13);
    seed(&fx_b, "b", 15, 13);

    let ab = reflect_across_graphs(
        &[("a".into(), &fx_a.graph), ("b".into(), &fx_b.graph)],
        &ReflectConfig::default(),
    )
    .unwrap();
    let ba = reflect_across_graphs(
        &[("b".into(), &fx_b.graph), ("a".into(), &fx_a.graph)],
        &ReflectConfig::default(),
    )
    .unwrap();
    assert_eq!(
        ab.scope_id, ba.scope_id,
        "scope id must be order-independent"
    );
}

#[test]
fn cross_reflect_does_not_touch_local_patterns() {
    let dir_local = tempdir().unwrap();
    let dir_cross = tempdir().unwrap();
    let fx_local = Fx::new(dir_local.path());
    let fx_cross = Fx::new(dir_cross.path());

    // ws-local has enough for a local pattern on its own (40 services).
    seed(&fx_local, "local", 40, 37);
    // ws-cross is only 15 services — below per-workspace threshold.
    seed(&fx_cross, "cross", 15, 13);

    // Establish a local pattern on ws-local.
    let engine = thinkingroot_reflect::ReflectEngine::new(ReflectConfig::default());
    let local_result = engine.reflect(&fx_local.graph).unwrap();
    assert!(!local_result.patterns.is_empty());
    let local_pattern_id = local_result.patterns[0].id.clone();

    // Run cross reflect bringing in ws-cross.
    let _ = reflect_across_graphs(
        &[
            ("local".into(), &fx_local.graph),
            ("cross".into(), &fx_cross.graph),
        ],
        &ReflectConfig::default(),
    )
    .unwrap();

    // Local pattern must still be present in ws-local after cross reflect.
    let after = fx_local.graph.reflect_load_structural_patterns().unwrap();
    assert!(
        after
            .iter()
            .any(|r| r.0 == local_pattern_id && r.10 == "local"),
        "local pattern must survive a cross reflect cycle"
    );
    // And ws-local should ALSO have the cross-scope pattern.
    assert!(
        after.iter().any(|r| r.10.starts_with("cross:")),
        "cross pattern must be written to ws-local too"
    );
}

#[test]
fn cross_reflect_stability_runs_increment() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let fx_a = Fx::new(dir_a.path());
    let fx_b = Fx::new(dir_b.path());
    seed(&fx_a, "a", 15, 13);
    seed(&fx_b, "b", 15, 13);

    let first = reflect_across_graphs(
        &[("a".into(), &fx_a.graph), ("b".into(), &fx_b.graph)],
        &ReflectConfig::default(),
    )
    .unwrap();
    let target_p = first
        .aggregate_patterns
        .iter()
        .find(|p| {
            p.condition_claim_type == "ApiSignature" && p.expected_claim_type == "Requirement"
        })
        .unwrap();
    assert_eq!(target_p.stability_runs, 1);

    let second = reflect_across_graphs(
        &[("a".into(), &fx_a.graph), ("b".into(), &fx_b.graph)],
        &ReflectConfig::default(),
    )
    .unwrap();
    let target_p2 = second
        .aggregate_patterns
        .iter()
        .find(|p| {
            p.condition_claim_type == "ApiSignature" && p.expected_claim_type == "Requirement"
        })
        .unwrap();
    assert_eq!(
        target_p2.stability_runs, 2,
        "cross-scope patterns must track stability across runs"
    );
}
