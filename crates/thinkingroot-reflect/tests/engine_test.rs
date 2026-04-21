//! Phase 9 Reflect — engine contract tests.
//!
//! These tests exercise the full pattern-discovery → gap-generation →
//! gap-resolution cycle against a real CozoDB-backed `GraphStore`.

use tempfile::tempdir;

use thinkingroot_core::{
    Claim, ClaimType, ContentHash, Entity, EntityType, Source, SourceType, TrustLevel,
    WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;
use thinkingroot_reflect::{
    count_open_gaps, list_open_gaps, GapStatus, ReflectConfig, ReflectEngine,
};

/// Test fixture helpers — seed a workspace with N service entities and
/// configurable claim coverage per entity.
struct Fixture {
    graph: GraphStore,
    workspace: WorkspaceId,
    source_id: thinkingroot_core::SourceId,
}

impl Fixture {
    fn new(dir: &std::path::Path) -> Self {
        let graph = GraphStore::init(dir).unwrap();
        let workspace = WorkspaceId::new();
        let source = Source::new("file:///fixture.md".into(), SourceType::Document)
            .with_trust(TrustLevel::Trusted)
            .with_hash(ContentHash("fx".into()));
        let source_id = source.id;
        graph.insert_source(&source).unwrap();
        Self {
            graph,
            workspace,
            source_id,
        }
    }

    fn add_entity(&self, canonical: &str, entity_type: EntityType) -> String {
        let entity = Entity::new(canonical, entity_type);
        let id = entity.id.to_string();
        self.graph.insert_entity(&entity).unwrap();
        id
    }

    fn add_claim(&self, entity_id: &str, statement: &str, claim_type: ClaimType) -> String {
        let claim = Claim::new(statement, claim_type, self.source_id, self.workspace);
        let cid = claim.id.to_string();
        self.graph.insert_claim(&claim).unwrap();
        self.graph
            .link_claim_to_source(&cid, &self.source_id.to_string())
            .unwrap();
        self.graph.link_claim_to_entity(&cid, entity_id).unwrap();
        cid
    }
}

/// Build a corpus of N services where `covered` of them have an extra
/// claim of type `extra_type`, and the remaining N - covered are missing
/// it. All services share `common_type`. Returns the ids of the
/// uncovered services.
fn seed_service_pattern(
    fx: &Fixture,
    total: usize,
    covered: usize,
    common_type: ClaimType,
    extra_type: ClaimType,
    uncovered_name_prefix: &str,
) -> Vec<String> {
    let mut uncovered_ids = Vec::new();
    for i in 0..total {
        let name = if i < covered {
            format!("CoveredService{i}")
        } else {
            format!("{uncovered_name_prefix}{i}")
        };
        let eid = fx.add_entity(&name, EntityType::Service);
        fx.add_claim(&eid, &format!("{name} has endpoints"), common_type);
        if i < covered {
            fx.add_claim(&eid, &format!("{name} uses JWT"), extra_type);
        } else {
            uncovered_ids.push(eid);
        }
    }
    uncovered_ids
}

#[test]
fn reflect_discovers_pattern_and_flags_missing_entities() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());

    // 40 services have an ApiSignature claim. 37 also have a Requirement
    // claim (≈92.5% co-occurrence). The remaining 3 don't — those are
    // the expected gaps.
    let uncovered = seed_service_pattern(
        &fx,
        40,
        37,
        ClaimType::ApiSignature,
        ClaimType::Requirement,
        "GapService",
    );
    assert_eq!(uncovered.len(), 3);

    let engine = ReflectEngine::new(ReflectConfig {
        min_sample_size: 30,
        min_frequency: 0.70,
        max_patterns: 500,
    });
    let result = engine.reflect(&fx.graph).unwrap();

    assert!(
        !result.patterns.is_empty(),
        "expected at least one pattern; got 0"
    );
    let pattern = result
        .patterns
        .iter()
        .find(|p| {
            p.entity_type == "Service"
                && p.condition_claim_type == "ApiSignature"
                && p.expected_claim_type == "Requirement"
        })
        .expect("the ApiSignature→Requirement pattern must be discovered");
    assert!(
        (pattern.frequency - 37.0 / 40.0).abs() < 1e-9,
        "frequency should be 37/40 = 0.925, got {}",
        pattern.frequency
    );
    assert_eq!(pattern.sample_size, 40);

    assert_eq!(result.gaps_created, 3, "one gap per uncovered service");
    assert_eq!(result.gaps_resolved, 0);
    assert_eq!(result.open_gaps_total, 3);

    let open = list_open_gaps(&fx.graph, None, 0.70).unwrap();
    assert_eq!(open.len(), 3);
    for gap in &open {
        assert_eq!(gap.entity_type, "Service");
        assert_eq!(gap.expected_claim_type, "Requirement");
        assert!(gap.confidence > 0.70);
        assert!(gap.entity_name.starts_with("GapService"));
        assert!(gap.reason.contains("92%") || gap.reason.contains("93%"));
    }
}

#[test]
fn reflect_skips_patterns_below_sample_threshold() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());

    // Only 10 services — below default min_sample_size=30.
    seed_service_pattern(
        &fx,
        10,
        10,
        ClaimType::ApiSignature,
        ClaimType::Requirement,
        "G",
    );

    let engine = ReflectEngine::new(ReflectConfig::default());
    let result = engine.reflect(&fx.graph).unwrap();
    assert!(
        result.patterns.is_empty(),
        "patterns below threshold must be dropped; got {:?}",
        result.patterns
    );
    assert_eq!(result.gaps_created, 0);
}

#[test]
fn reflect_skips_patterns_below_frequency_threshold() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());

    // 40 services, only 20 have Requirement (50% — below 70%).
    seed_service_pattern(
        &fx,
        40,
        20,
        ClaimType::ApiSignature,
        ClaimType::Requirement,
        "G",
    );

    let engine = ReflectEngine::new(ReflectConfig::default());
    let result = engine.reflect(&fx.graph).unwrap();
    let hit = result.patterns.iter().find(|p| {
        p.condition_claim_type == "ApiSignature" && p.expected_claim_type == "Requirement"
    });
    assert!(
        hit.is_none(),
        "50%-frequency pattern must be filtered out under default 70% threshold"
    );
}

#[test]
fn reflect_resolves_gap_after_claim_added() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());

    let uncovered = seed_service_pattern(
        &fx,
        40,
        37,
        ClaimType::ApiSignature,
        ClaimType::Requirement,
        "GapService",
    );
    let engine = ReflectEngine::new(ReflectConfig::default());
    let r1 = engine.reflect(&fx.graph).unwrap();
    assert_eq!(r1.gaps_created, 3);
    assert_eq!(r1.open_gaps_total, 3);

    // Satisfy one gap by adding a Requirement claim to one of the uncovered.
    let target = &uncovered[0];
    fx.add_claim(target, "GapService has a requirement spec", ClaimType::Requirement);

    let r2 = engine.reflect(&fx.graph).unwrap();
    assert_eq!(
        r2.gaps_created, 0,
        "second run should not create new gaps against the same pattern"
    );
    assert_eq!(r2.gaps_resolved, 1, "exactly one gap should have resolved");
    assert_eq!(r2.open_gaps_total, 2);
}

#[test]
fn reflect_is_idempotent_across_runs() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    seed_service_pattern(
        &fx,
        40,
        37,
        ClaimType::ApiSignature,
        ClaimType::Requirement,
        "GapService",
    );

    let engine = ReflectEngine::new(ReflectConfig::default());
    let r1 = engine.reflect(&fx.graph).unwrap();
    let r2 = engine.reflect(&fx.graph).unwrap();

    // Pattern set is stable.
    assert_eq!(r1.patterns.len(), r2.patterns.len());
    for (a, b) in r1.patterns.iter().zip(r2.patterns.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.entity_type, b.entity_type);
        assert_eq!(a.condition_claim_type, b.condition_claim_type);
        assert_eq!(a.expected_claim_type, b.expected_claim_type);
        assert_eq!(a.sample_size, b.sample_size);
        assert!((a.frequency - b.frequency).abs() < 1e-9);
    }

    // Second run creates no new gaps.
    assert_eq!(r2.gaps_created, 0);
    assert_eq!(r1.open_gaps_total, r2.open_gaps_total);
}

#[test]
fn list_open_gaps_scopes_by_entity_name() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    seed_service_pattern(
        &fx,
        40,
        37,
        ClaimType::ApiSignature,
        ClaimType::Requirement,
        "Gap",
    );
    let engine = ReflectEngine::new(ReflectConfig::default());
    engine.reflect(&fx.graph).unwrap();

    let all = list_open_gaps(&fx.graph, None, 0.0).unwrap();
    assert_eq!(all.len(), 3);

    // Scoped — only one of the three service names will match.
    let one = list_open_gaps(&fx.graph, Some("Gap37"), 0.0).unwrap();
    assert_eq!(one.len(), 1, "name filter must return just the one match");
    assert_eq!(one[0].entity_name, "Gap37");
}

#[test]
fn count_open_gaps_matches_list_len() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    seed_service_pattern(
        &fx,
        40,
        37,
        ClaimType::ApiSignature,
        ClaimType::Requirement,
        "Gap",
    );
    let engine = ReflectEngine::new(ReflectConfig::default());
    engine.reflect(&fx.graph).unwrap();

    let n = count_open_gaps(&fx.graph).unwrap();
    let list = list_open_gaps(&fx.graph, None, 0.0).unwrap();
    assert_eq!(n, list.len());
    assert_eq!(n, 3);
}

#[test]
fn gap_status_parses_roundtrip() {
    for s in ["open", "resolved", "dismissed"] {
        assert_eq!(GapStatus::from_str(s).map(|x| x.as_str()), Some(s));
    }
    assert!(GapStatus::from_str("something-else").is_none());
}

#[test]
fn dismissed_gap_not_reraised_by_reflect() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    seed_service_pattern(
        &fx,
        40,
        37,
        ClaimType::ApiSignature,
        ClaimType::Requirement,
        "GapService",
    );
    let engine = ReflectEngine::new(ReflectConfig::default());
    engine.reflect(&fx.graph).unwrap();

    // Three gaps were created by reflect. Dismiss the first one.
    let open = list_open_gaps(&fx.graph, None, 0.0).unwrap();
    assert_eq!(open.len(), 3);
    let all = fx.graph.reflect_load_known_unknowns().unwrap();
    assert_eq!(all.len(), 3);
    let target_gap_id = all[0].0.clone();

    thinkingroot_reflect::dismiss_gap(&fx.graph, &target_gap_id).unwrap();
    let open_after_dismiss = list_open_gaps(&fx.graph, None, 0.0).unwrap();
    assert_eq!(
        open_after_dismiss.len(),
        2,
        "dismissed gap must drop from open list"
    );

    // A second reflect run must not re-raise the dismissed gap.
    let r2 = engine.reflect(&fx.graph).unwrap();
    assert_eq!(
        r2.gaps_created, 0,
        "dismissed gap must not be re-raised; got {} new gaps",
        r2.gaps_created
    );
    let still_open = list_open_gaps(&fx.graph, None, 0.0).unwrap();
    assert_eq!(still_open.len(), 2, "dismissed gap must stay dismissed");
}

#[test]
fn dismiss_is_idempotent_on_missing_id() {
    let dir = tempdir().unwrap();
    let fx = Fixture::new(dir.path());
    // Dismissing a nonexistent gap id is a no-op, not an error.
    thinkingroot_reflect::dismiss_gap(&fx.graph, "ku-nonexistent-deadbeef").unwrap();
}
