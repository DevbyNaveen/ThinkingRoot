use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use thinkingroot_bench::{Fixture, Scale};

fn bench_entity_lookup_by_name(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/entity_lookup_by_name");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.find_entity_by_name(&fix.sample_entity_name));
        });
    }
    group.finish();
}

fn bench_entity_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/entity_search");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        let keyword = &fix.sample_entity_name[..fix.sample_entity_name.len().min(8)];
        let keyword = keyword.to_string();
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.search_entities(&keyword));
        });
    }
    group.finish();
}

fn bench_claims_for_entity(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/claims_for_entity");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.get_claims_for_entity(&fix.sample_entity_id));
        });
    }
    group.finish();
}

fn bench_claims_by_type(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/claims_by_type");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.get_claims_by_type("Fact"));
        });
    }
    group.finish();
}

fn bench_relations_for_entity(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/relations_for_entity");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.get_relations_for_entity(&fix.sample_entity_name));
        });
    }
    group.finish();
}

fn bench_all_entities(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/all_entities");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.get_all_entities());
        });
    }
    group.finish();
}

fn bench_all_relations(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/all_relations");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.get_all_relations());
        });
    }
    group.finish();
}

fn bench_contradictions(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/contradictions");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.get_contradictions());
        });
    }
    group.finish();
}

fn bench_source_hash_exists(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/source_hash_exists");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.source_hash_exists("hash_42"));
        });
    }
    group.finish();
}

fn bench_get_counts(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/get_counts");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.get_counts());
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_entity_lookup_by_name,
    bench_entity_search,
    bench_claims_for_entity,
    bench_claims_by_type,
    bench_relations_for_entity,
    bench_all_entities,
    bench_all_relations,
    bench_contradictions,
    bench_source_hash_exists,
    bench_get_counts,
);
criterion_main!(benches);
