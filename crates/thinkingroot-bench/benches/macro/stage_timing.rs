use tempfile::TempDir;
use thinkingroot_bench::{Fixture, Scale};
use thinkingroot_core::{Claim, ClaimType, Entity, EntityType, SourceId, WorkspaceId};
use thinkingroot_graph::graph::GraphStore;

// ---------------------------------------------------------------------------
// graph_insert_entities
// ---------------------------------------------------------------------------

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn graph_insert_entities(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;
    let count = scale.entity_count();

    // Pre-generate entities outside the measured body
    let entities: Vec<Entity> = (0..count)
        .map(|i| {
            let name = format!("bench-entity-{i}");
            Entity::new(&name, EntityType::Service)
        })
        .collect();

    bencher.bench_local(move || {
        let tmpdir = TempDir::new().expect("failed to create tempdir");
        let graph = GraphStore::init(tmpdir.path()).expect("failed to init GraphStore");
        for entity in &entities {
            graph.insert_entity(entity).expect("failed to insert entity");
        }
        // keep tmpdir alive until end of measured body
        let _ = tmpdir;
    });
}

// ---------------------------------------------------------------------------
// graph_insert_claims
// ---------------------------------------------------------------------------

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn graph_insert_claims(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;
    let count = scale.claim_count();

    // Pre-generate a source id and workspace id for all claims
    let source_id = SourceId::new();
    let workspace_id = WorkspaceId::new();

    // Pre-generate claims outside the measured body
    let claims: Vec<Claim> = (0..count)
        .map(|i| {
            Claim::new(
                format!("Bench claim {i}: module_{i} is established"),
                ClaimType::Fact,
                source_id,
                workspace_id,
            )
            .with_confidence(0.9)
        })
        .collect();

    bencher.bench_local(move || {
        let tmpdir = TempDir::new().expect("failed to create tempdir");
        let graph = GraphStore::init(tmpdir.path()).expect("failed to init GraphStore");
        for claim in &claims {
            graph.insert_claim(claim).expect("failed to insert claim");
        }
        let _ = tmpdir;
    });
}

// ---------------------------------------------------------------------------
// graph_link_relations
// ---------------------------------------------------------------------------

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn graph_link_relations(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;

    // Fixture is already populated with entities and claims — build it once
    let fix = Fixture::generate(scale);
    let len = fix.entity_names.len();
    let iterations = scale.relation_count().min(len * 2);

    bencher.bench_local(|| {
        for i in 0..iterations {
            let from = &fix.entity_names[i % len];
            let to = &fix.entity_names[(i + 1) % len];
            let _ = fix.graph.link_entities(from, to, "depends_on", 0.5_f64);
        }
    });
}

// ---------------------------------------------------------------------------
// entity_resolution_scan  (Small + Medium only — Large is too slow)
// ---------------------------------------------------------------------------

#[divan::bench(args = [Scale::Small, Scale::Medium])]
fn entity_resolution_scan(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;
    let fix = Fixture::generate(scale);

    bencher.bench_local(|| {
        fix.graph
            .get_entities_with_aliases()
            .expect("get_entities_with_aliases failed")
    });
}

fn main() {
    divan::main();
}
