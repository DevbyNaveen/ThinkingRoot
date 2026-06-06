# Benchmark Testing Suite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a world-class benchmark suite to ThinkingRoot proving sub-10ms retrieval latency across 3 scale tiers, with Criterion micro-benchmarks, Divan macro-benchmarks, k6 load tests, and automated CI regression detection.

**Architecture:** A new `thinkingroot-bench` crate in the workspace with three benchmark tiers: Criterion for statistical micro-benchmarks of hot paths (graph queries, vector search, parsing, serialization), Divan for ergonomic macro-benchmarks of pipeline stages at scale, and k6 scripts for HTTP/MCP load testing. Automated fixture generation populates CozoDB + vector index at Small/Medium/Large scale tiers. Two CI workflows provide per-PR regression warnings and nightly authoritative runs.

**Tech Stack:** Criterion 0.6 (micro), Divan 0.1 (macro), k6 (load), rand 0.9 (fixtures), tokio (async runtime)

**Spec:** `docs/superpowers/specs/2026-04-13-benchmark-testing-design.md`

---

## File Structure

### New Files

| File | Responsibility |
|------|---------------|
| `crates/thinkingroot-bench/Cargo.toml` | Crate manifest with Criterion + Divan deps, feature flags, bench targets |
| `crates/thinkingroot-bench/src/lib.rs` | Re-exports scale + fixtures modules |
| `crates/thinkingroot-bench/src/scale.rs` | `Scale` enum (Small/Medium/Large) with counts |
| `crates/thinkingroot-bench/src/fixtures.rs` | `Fixture` struct + automated graph/vector population |
| `crates/thinkingroot-bench/benches/micro/graph_queries.rs` | Criterion: entity/claim/relation lookup benchmarks |
| `crates/thinkingroot-bench/benches/micro/vector_search.rs` | Criterion: embed + cosine search benchmarks |
| `crates/thinkingroot-bench/benches/micro/parser_throughput.rs` | Criterion: tree-sitter parsing by language/size |
| `crates/thinkingroot-bench/benches/micro/serialization.rs` | Criterion: JSON + MessagePack round-trip |
| `crates/thinkingroot-bench/benches/macro/pipeline_e2e.rs` | Divan: full pipeline at 3 scales |
| `crates/thinkingroot-bench/benches/macro/stage_timing.rs` | Divan: individual stage benchmarks |
| `crates/thinkingroot-bench/benches/macro/cache_effectiveness.rs` | Divan: BLAKE3 fingerprint cache scenarios |
| `crates/thinkingroot-bench/benches/load/rest_search.js` | k6: search endpoint load test |
| `crates/thinkingroot-bench/benches/load/rest_entities.js` | k6: entity listing throughput |
| `crates/thinkingroot-bench/benches/load/mcp_tools.js` | k6: MCP tool/call concurrency |
| `crates/thinkingroot-bench/benches/load/mixed_workload.js` | k6: realistic mixed traffic |
| `crates/thinkingroot-bench/benches/load/run_load_test.sh` | Orchestrator: build, serve, k6, report |
| `.github/workflows/bench-pr.yml` | Per-PR fast micro-benchmark CI |
| `.github/workflows/bench-nightly.yml` | Nightly full suite CI |

### Modified Files

| File | Change |
|------|--------|
| `Cargo.toml` (workspace root) | Add `thinkingroot-bench` to `members` and `default-members`, add `criterion` + `divan` + `rand` to workspace deps, add `[profile.bench]` |

---

## Task 1: Workspace Setup — Cargo.toml and Crate Scaffold

**Files:**
- Modify: `Cargo.toml` (workspace root, lines 3-34 for members, lines 48-128 for deps)
- Create: `crates/thinkingroot-bench/Cargo.toml`
- Create: `crates/thinkingroot-bench/src/lib.rs`

- [ ] **Step 1: Add bench crate to workspace members and deps**

In `/Users/naveen/Desktop/thinkingroot/Cargo.toml`, add `"crates/thinkingroot-bench"` to both `members` and `default-members` arrays. Add workspace dependencies for `criterion`, `divan`, and `rand`. Add a bench profile.

```toml
# Add to members array (after "crates/thinkingroot-cli"):
    "crates/thinkingroot-bench",

# Add to default-members array (after "crates/thinkingroot-cli"):
    "crates/thinkingroot-bench",

# Add to [workspace.dependencies] section:
criterion = { version = "0.6", default-features = false, features = ["html_reports"] }
divan = "0.1"
rand = "0.9"

# Add after [profile.dev]:
[profile.bench]
opt-level = 3
debug = true    # enable flamegraph / profiling symbols
lto = "thin"
```

- [ ] **Step 2: Create bench crate Cargo.toml**

Create `crates/thinkingroot-bench/Cargo.toml`:

```toml
[package]
name = "thinkingroot-bench"
description = "Benchmark suite for ThinkingRoot — micro (Criterion), macro (Divan), load (k6)"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true
rust-version.workspace = true
publish = false

[features]
default = ["vector"]
vector = ["thinkingroot-graph/vector", "thinkingroot-serve/vector"]

[dependencies]
thinkingroot-core = { workspace = true }
thinkingroot-graph = { workspace = true }
thinkingroot-parse = { workspace = true }
thinkingroot-serve = { workspace = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
rmp-serde = { workspace = true }
chrono = { workspace = true }
blake3 = { workspace = true }
rand = { workspace = true }
tempfile = "3"

[dev-dependencies]
criterion = { workspace = true }
divan = { workspace = true }

# ── Criterion micro-benchmarks ──────────────────────────────
[[bench]]
name = "graph_queries"
harness = false
path = "benches/micro/graph_queries.rs"

[[bench]]
name = "vector_search"
harness = false
path = "benches/micro/vector_search.rs"
required-features = ["vector"]

[[bench]]
name = "parser_throughput"
harness = false
path = "benches/micro/parser_throughput.rs"

[[bench]]
name = "serialization"
harness = false
path = "benches/micro/serialization.rs"

# ── Divan macro-benchmarks ──────────────────────────────────
[[bench]]
name = "pipeline_e2e"
harness = false
path = "benches/macro/pipeline_e2e.rs"

[[bench]]
name = "stage_timing"
harness = false
path = "benches/macro/stage_timing.rs"

[[bench]]
name = "cache_effectiveness"
harness = false
path = "benches/macro/cache_effectiveness.rs"
```

- [ ] **Step 3: Create lib.rs scaffold**

Create `crates/thinkingroot-bench/src/lib.rs`:

```rust
pub mod fixtures;
pub mod scale;

pub use fixtures::Fixture;
pub use scale::Scale;
```

- [ ] **Step 4: Verify workspace compiles**

Run: `cargo check --workspace --no-default-features`

Expected: Compilation errors for missing `scale.rs` and `fixtures.rs` — that's fine, we'll create them next. But the workspace dependency resolution should succeed.

- [ ] **Step 5: Commit**

```bash
git add crates/thinkingroot-bench/Cargo.toml crates/thinkingroot-bench/src/lib.rs Cargo.toml
git commit -m "feat(bench): scaffold thinkingroot-bench crate with workspace integration"
```

---

## Task 2: Scale Enum and Fixture Generator

**Files:**
- Create: `crates/thinkingroot-bench/src/scale.rs`
- Create: `crates/thinkingroot-bench/src/fixtures.rs`

- [ ] **Step 1: Create the Scale enum**

Create `crates/thinkingroot-bench/src/scale.rs`:

```rust
use std::fmt;

/// Scale tiers for benchmark fixture generation.
///
/// Each tier defines how many entities, claims, relations, and embeddings
/// to generate. Benchmarks run across all three to produce latency-vs-scale
/// curves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scale {
    /// ~500 entities, ~2K claims — single microservice repo
    Small,
    /// ~5K entities, ~20K claims — monorepo with 50-100 services
    Medium,
    /// ~50K entities, ~200K claims — Fortune 500 knowledge base
    Large,
}

impl Scale {
    pub fn entity_count(self) -> usize {
        match self {
            Scale::Small => 500,
            Scale::Medium => 5_000,
            Scale::Large => 50_000,
        }
    }

    pub fn claim_count(self) -> usize {
        match self {
            Scale::Small => 2_000,
            Scale::Medium => 20_000,
            Scale::Large => 200_000,
        }
    }

    pub fn relation_count(self) -> usize {
        match self {
            Scale::Small => 1_000,
            Scale::Medium => 10_000,
            Scale::Large => 100_000,
        }
    }

    pub fn embedding_count(self) -> usize {
        match self {
            Scale::Small => 800,
            Scale::Medium => 8_000,
            Scale::Large => 80_000,
        }
    }

    /// Return all scale tiers, useful for parameterised benchmarks.
    pub fn all() -> &'static [Scale] {
        &[Scale::Small, Scale::Medium, Scale::Large]
    }

    /// Read from BENCH_SCALE env var, defaulting to Small for fast iteration.
    pub fn from_env() -> Scale {
        match std::env::var("BENCH_SCALE").as_deref() {
            Ok("small") => Scale::Small,
            Ok("medium") => Scale::Medium,
            Ok("large") => Scale::Large,
            _ => Scale::Small,
        }
    }
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scale::Small => write!(f, "small"),
            Scale::Medium => write!(f, "medium"),
            Scale::Large => write!(f, "large"),
        }
    }
}
```

- [ ] **Step 2: Create the Fixture generator**

Create `crates/thinkingroot-bench/src/fixtures.rs`:

```rust
use std::path::Path;

use rand::Rng;
use tempfile::TempDir;

use thinkingroot_core::{
    Claim, ClaimType, Confidence, ContentHash, Entity, EntityType, Id, Relation,
    RelationType, Sensitivity, Source, SourceType, Strength, TrustLevel, WorkspaceId,
};
use thinkingroot_graph::graph::GraphStore;

use crate::Scale;

/// A fully populated benchmark fixture: a GraphStore loaded with realistic
/// synthetic data at a given scale tier, plus sample values for lookup
/// benchmarks.
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
    /// Generate a fixture at the given scale. This populates a CozoDB
    /// instance with realistic synthetic entities, claims, relations, and
    /// contradictions.
    pub fn generate(scale: Scale) -> Self {
        let tmpdir = TempDir::new().expect("failed to create tempdir");
        let graph_dir = tmpdir.path().join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();
        let graph = GraphStore::init(&graph_dir).expect("failed to init graph");

        let mut rng = rand::rng();
        let workspace_id = WorkspaceId::new();

        // ── Generate sources ──────────────────────────────────────────
        let source_count = scale.entity_count() / 5; // ~1 source per 5 entities
        let mut source_ids = Vec::with_capacity(source_count);
        for i in 0..source_count {
            let source = Source::new(
                format!("src/module_{i}.rs"),
                SourceType::File,
            )
            .with_hash(ContentHash::from_bytes(format!("hash_{i}").as_bytes()))
            .with_trust(TrustLevel::Trusted)
            .with_size(rng.random_range(500..50_000));
            source_ids.push(source.id.to_string());
            graph.insert_source(&source).unwrap();
        }

        // ── Generate entities ─────────────────────────────────────────
        let entity_count = scale.entity_count();
        let mut entity_ids = Vec::with_capacity(entity_count);
        let mut entity_names = Vec::with_capacity(entity_count);

        let adjectives = [
            "auth", "billing", "cache", "data", "event", "fast", "graph",
            "http", "index", "json", "key", "log", "mesh", "net", "ops",
            "parse", "query", "route", "store", "token", "user", "vault",
        ];
        let nouns = [
            "service", "handler", "manager", "engine", "worker", "pipeline",
            "processor", "resolver", "validator", "scheduler", "monitor",
            "gateway", "adapter", "proxy", "bridge", "controller",
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

            let entity = Entity::new(&name, etype)
                .with_description(format!("Benchmark entity {i} ({adj} {noun})"));
            entity_ids.push(entity.id.to_string());
            entity_names.push(name);
            graph.insert_entity(&entity).unwrap();
        }

        // ── Generate claims ───────────────────────────────────────────
        let claim_count = scale.claim_count();
        let mut claim_ids = Vec::with_capacity(claim_count);

        // Distribution: Fact 40%, Decision 15%, Architecture 10%, Dependency 10%,
        // ApiSignature 8%, Requirement 7%, Definition 5%, Plan 3%, Metric 2%
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

            // Normal-ish confidence centred at 0.75, clamped [0.3, 1.0]
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

            // Link claim to source
            graph
                .link_claim_to_source(&claim_id_str, &source_ids[source_idx])
                .unwrap();

            // Link claim to a related entity
            let entity_idx = i % entity_ids.len();
            graph
                .link_claim_to_entity(&claim_id_str, &entity_ids[entity_idx])
                .unwrap();
        }

        // ── Generate relations (power-law) ────────────────────────────
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
            // Power-law: ~5% hub entities get most connections
            let from_idx = if rng.random_range(0..100_u32) < 5 {
                rng.random_range(0..entity_count.min(25)) // hub
            } else {
                rng.random_range(0..entity_count)
            };
            let to_idx = rng.random_range(0..entity_count);
            if from_idx == to_idx {
                continue;
            }
            let rel_type = rel_types[i % rel_types.len()];
            let _ = graph.link_entities(&entity_ids[from_idx], &entity_ids[to_idx], rel_type);
        }

        // ── Generate contradictions (small fraction) ──────────────────
        let contradiction_count = claim_count / 100; // 1% of claims
        for i in 0..contradiction_count {
            let a = &claim_ids[i * 2 % claim_ids.len()];
            let b = &claim_ids[(i * 2 + 1) % claim_ids.len()];
            let _ = graph.insert_contradiction(a, b, &format!("Contradiction {i}: conflicting claims"));
        }

        // ── Pick sample values for benchmarks ─────────────────────────
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
```

- [ ] **Step 3: Verify the crate compiles**

Run: `cargo check -p thinkingroot-bench --no-default-features`

Expected: Clean compilation with no errors.

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-bench/src/scale.rs crates/thinkingroot-bench/src/fixtures.rs
git commit -m "feat(bench): add Scale enum and Fixture generator with realistic data distribution"
```

---

## Task 3: Graph Query Micro-Benchmarks (Criterion)

**Files:**
- Create: `crates/thinkingroot-bench/benches/micro/graph_queries.rs`

- [ ] **Step 1: Create the graph query benchmarks**

Create `crates/thinkingroot-bench/benches/micro/graph_queries.rs`:

```rust
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
        group.bench_with_input(BenchmarkId::from_parameter(scale), &keyword, |b, kw| {
            b.iter(|| fix.graph.search_entities(kw));
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
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, _fix| {
            b.iter(|| fix.graph.get_claims_by_type("fact"));
        });
    }
    group.finish();
}

fn bench_relations_for_entity(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/relations_for_entity");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, fix| {
            b.iter(|| fix.graph.get_relations_for_entity(&fix.sample_entity_id));
        });
    }
    group.finish();
}

fn bench_all_entities(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/all_entities");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, _fix| {
            b.iter(|| fix.graph.get_all_entities());
        });
    }
    group.finish();
}

fn bench_all_relations(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/all_relations");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, _fix| {
            b.iter(|| fix.graph.get_all_relations());
        });
    }
    group.finish();
}

fn bench_contradictions(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/contradictions");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, _fix| {
            b.iter(|| fix.graph.get_contradictions());
        });
    }
    group.finish();
}

fn bench_source_hash_exists(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/source_hash_exists");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, _fix| {
            b.iter(|| fix.graph.source_hash_exists("hash_42"));
        });
    }
    group.finish();
}

fn bench_get_counts(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/get_counts");
    for &scale in Scale::all() {
        let fix = Fixture::generate(scale);
        group.bench_with_input(BenchmarkId::from_parameter(scale), &fix, |b, _fix| {
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
```

- [ ] **Step 2: Verify the bench target compiles**

Run: `cargo bench -p thinkingroot-bench --bench graph_queries --no-default-features --no-run`

Expected: Clean compilation. The `--no-run` flag just compiles without executing.

- [ ] **Step 3: Run the benchmark at small scale**

Run: `BENCH_SCALE=small cargo bench -p thinkingroot-bench --bench graph_queries --no-default-features -- --warm-up-time 1 --measurement-time 3`

Expected: Benchmark output with timing for each graph operation. All `entity_lookup_by_name` should be well under 1ms.

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-bench/benches/micro/graph_queries.rs
git commit -m "feat(bench): add Criterion graph query micro-benchmarks across 3 scale tiers"
```

---

## Task 4: Vector Search Micro-Benchmarks (Criterion)

**Files:**
- Create: `crates/thinkingroot-bench/benches/micro/vector_search.rs`

- [ ] **Step 1: Create the vector search benchmarks**

Create `crates/thinkingroot-bench/benches/micro/vector_search.rs`:

```rust
use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use thinkingroot_bench::Scale;

/// Lightweight in-process vector index for benchmarking cosine similarity
/// search without requiring the fastembed ONNX runtime. Uses pre-computed
/// random vectors to isolate the search algorithm from embedding cost.
struct BenchVectorIndex {
    vectors: Vec<(String, Vec<f32>)>,
    dim: usize,
}

impl BenchVectorIndex {
    fn generate(count: usize, dim: usize) -> Self {
        let mut rng = rand::rng();
        let vectors: Vec<(String, Vec<f32>)> = (0..count)
            .map(|i| {
                let vec: Vec<f32> = (0..dim).map(|_| rand::Rng::random_range(&mut rng, -1.0..1.0_f32)).collect();
                (format!("vec_{i}"), vec)
            })
            .collect();
        Self { vectors, dim }
    }

    fn cosine_search(&self, query: &[f32], top_k: usize) -> Vec<(String, f32)> {
        let mut scores: Vec<(String, f32)> = self
            .vectors
            .iter()
            .map(|(id, vec)| {
                let dot: f32 = query.iter().zip(vec.iter()).map(|(a, b)| a * b).sum();
                let norm_q: f32 = query.iter().map(|x| x * x).sum::<f32>().sqrt();
                let norm_v: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                let sim = if norm_q * norm_v > 0.0 {
                    dot / (norm_q * norm_v)
                } else {
                    0.0
                };
                (id.clone(), sim)
            })
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        scores.truncate(top_k);
        scores
    }
}

fn bench_cosine_search_top5(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector/cosine_search_top5");
    let dim = 384;
    for &scale in Scale::all() {
        let index = BenchVectorIndex::generate(scale.embedding_count(), dim);
        let query: Vec<f32> = (0..dim).map(|i| (i as f32 / dim as f32) - 0.5).collect();
        group.bench_with_input(BenchmarkId::from_parameter(scale), &(), |b, _| {
            b.iter(|| index.cosine_search(&query, 5));
        });
    }
    group.finish();
}

fn bench_cosine_search_top10(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector/cosine_search_top10");
    let dim = 384;
    for &scale in Scale::all() {
        let index = BenchVectorIndex::generate(scale.embedding_count(), dim);
        let query: Vec<f32> = (0..dim).map(|i| (i as f32 / dim as f32) - 0.5).collect();
        group.bench_with_input(BenchmarkId::from_parameter(scale), &(), |b, _| {
            b.iter(|| index.cosine_search(&query, 10));
        });
    }
    group.finish();
}

fn bench_cosine_search_top50(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector/cosine_search_top50");
    let dim = 384;
    for &scale in Scale::all() {
        let index = BenchVectorIndex::generate(scale.embedding_count(), dim);
        let query: Vec<f32> = (0..dim).map(|i| (i as f32 / dim as f32) - 0.5).collect();
        group.bench_with_input(BenchmarkId::from_parameter(scale), &(), |b, _| {
            b.iter(|| index.cosine_search(&query, 50));
        });
    }
    group.finish();
}

fn bench_upsert_single(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector/upsert_single");
    let dim = 384;
    let vec: Vec<f32> = (0..dim).map(|i| i as f32 / dim as f32).collect();
    group.bench_function("upsert", |b| {
        let mut map: HashMap<String, (Vec<f32>, String)> = HashMap::new();
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            map.insert(
                format!("id_{counter}"),
                (vec.clone(), format!("meta_{counter}")),
            );
        });
    });
    group.finish();
}

fn bench_upsert_batch_100(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector/upsert_batch_100");
    let dim = 384;
    let batch: Vec<(String, Vec<f32>, String)> = (0..100)
        .map(|i| {
            let vec: Vec<f32> = (0..dim).map(|j| (i * dim + j) as f32 / 1000.0).collect();
            (format!("id_{i}"), vec, format!("meta_{i}"))
        })
        .collect();
    group.bench_function("batch_100", |b| {
        b.iter(|| {
            let mut map: HashMap<String, (Vec<f32>, String)> = HashMap::with_capacity(100);
            for (id, vec, meta) in &batch {
                map.insert(id.clone(), (vec.clone(), meta.clone()));
            }
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_cosine_search_top5,
    bench_cosine_search_top10,
    bench_cosine_search_top50,
    bench_upsert_single,
    bench_upsert_batch_100,
);
criterion_main!(benches);
```

- [ ] **Step 2: Verify compilation**

Run: `cargo bench -p thinkingroot-bench --bench vector_search --no-default-features --no-run`

Expected: Clean compilation.

- [ ] **Step 3: Run the benchmark**

Run: `cargo bench -p thinkingroot-bench --bench vector_search --no-default-features -- --warm-up-time 1 --measurement-time 3`

Expected: Cosine search top-10 at Small scale (800 vectors) should be well under 5ms.

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-bench/benches/micro/vector_search.rs
git commit -m "feat(bench): add Criterion vector search micro-benchmarks (cosine similarity + upsert)"
```

---

## Task 5: Parser Throughput Micro-Benchmarks (Criterion)

**Files:**
- Create: `crates/thinkingroot-bench/benches/micro/parser_throughput.rs`

- [ ] **Step 1: Create the parser benchmarks**

Create `crates/thinkingroot-bench/benches/micro/parser_throughput.rs`:

```rust
use std::io::Write;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tempfile::NamedTempFile;

/// Generate a synthetic Rust source file of the given line count.
fn generate_rust_source(lines: usize) -> String {
    let mut src = String::with_capacity(lines * 40);
    src.push_str("use std::collections::HashMap;\n\n");
    let funcs = lines / 10;
    for i in 0..funcs {
        src.push_str(&format!(
            "/// Documentation for function_{i}\n\
             pub fn function_{i}(input: &str) -> Result<String, Box<dyn std::error::Error>> {{\n\
             \tlet mut result = HashMap::new();\n\
             \tresult.insert(\"key_{i}\", input.to_string());\n\
             \tfor j in 0..{i} {{\n\
             \t\tresult.insert(&format!(\"iter_{{j}}\"), j.to_string());\n\
             \t}}\n\
             \tOk(format!(\"{{:?}}\", result))\n\
             }}\n\n"
        ));
    }
    src
}

/// Generate a synthetic Python source file of the given line count.
fn generate_python_source(lines: usize) -> String {
    let mut src = String::with_capacity(lines * 30);
    src.push_str("import os\nimport json\nfrom typing import Dict, List, Optional\n\n");
    let funcs = lines / 8;
    for i in 0..funcs {
        src.push_str(&format!(
            "def function_{i}(data: Dict[str, str]) -> Optional[str]:\n\
             \t\"\"\"Process data for function_{i}.\"\"\"\n\
             \tresult = {{}}\n\
             \tfor key, value in data.items():\n\
             \t\tresult[key] = value.upper()\n\
             \treturn json.dumps(result)\n\n"
        ));
    }
    src
}

/// Generate a synthetic TypeScript source file of the given line count.
fn generate_typescript_source(lines: usize) -> String {
    let mut src = String::with_capacity(lines * 35);
    src.push_str("import {{ useState, useEffect }} from 'react';\n\n");
    let funcs = lines / 10;
    for i in 0..funcs {
        src.push_str(&format!(
            "interface Config{i} {{\n\
             \tname: string;\n\
             \tvalue: number;\n\
             \tenabled: boolean;\n\
             }}\n\n\
             export function handler{i}(config: Config{i}): string {{\n\
             \tif (!config.enabled) return '';\n\
             \treturn `${{config.name}}: ${{config.value}}`;\n\
             }}\n\n"
        ));
    }
    src
}

/// Generate a synthetic Markdown document of the given line count.
fn generate_markdown_source(lines: usize) -> String {
    let mut src = String::with_capacity(lines * 50);
    let sections = lines / 15;
    for i in 0..sections {
        src.push_str(&format!(
            "## Section {i}: Architecture Overview\n\n\
             This section describes the architecture of component {i}. \
             The system uses a microservices pattern with event-driven communication.\n\n\
             ### Key Points\n\n\
             - Service {i} handles authentication and authorization\n\
             - Uses PostgreSQL for persistent storage\n\
             - Redis for caching layer\n\
             - gRPC for inter-service communication\n\n\
             ```rust\n\
             fn process_{i}() -> Result<()> {{\n\
             \tOk(())\n\
             }}\n\
             ```\n\n"
        ));
    }
    src
}

fn write_temp_file(content: &str, extension: &str) -> NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(extension)
        .tempfile()
        .unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

fn bench_parse_rust(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser/rust");
    for &line_count in &[100, 500, 2000] {
        let src = generate_rust_source(line_count);
        let file = write_temp_file(&src, ".rs");
        group.bench_with_input(
            BenchmarkId::new("lines", line_count),
            &file,
            |b, f| {
                b.iter(|| thinkingroot_parse::parse_file(f.path()));
            },
        );
    }
    group.finish();
}

fn bench_parse_python(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser/python");
    for &line_count in &[100, 500, 2000] {
        let src = generate_python_source(line_count);
        let file = write_temp_file(&src, ".py");
        group.bench_with_input(
            BenchmarkId::new("lines", line_count),
            &file,
            |b, f| {
                b.iter(|| thinkingroot_parse::parse_file(f.path()));
            },
        );
    }
    group.finish();
}

fn bench_parse_typescript(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser/typescript");
    for &line_count in &[100, 500, 2000] {
        let src = generate_typescript_source(line_count);
        let file = write_temp_file(&src, ".ts");
        group.bench_with_input(
            BenchmarkId::new("lines", line_count),
            &file,
            |b, f| {
                b.iter(|| thinkingroot_parse::parse_file(f.path()));
            },
        );
    }
    group.finish();
}

fn bench_parse_markdown(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser/markdown");
    for &line_count in &[100, 500, 2000] {
        let src = generate_markdown_source(line_count);
        let file = write_temp_file(&src, ".md");
        group.bench_with_input(
            BenchmarkId::new("lines", line_count),
            &file,
            |b, f| {
                b.iter(|| thinkingroot_parse::parse_file(f.path()));
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_parse_rust,
    bench_parse_python,
    bench_parse_typescript,
    bench_parse_markdown,
);
criterion_main!(benches);
```

- [ ] **Step 2: Verify compilation**

Run: `cargo bench -p thinkingroot-bench --bench parser_throughput --no-default-features --no-run`

Expected: Clean compilation.

- [ ] **Step 3: Run the benchmark**

Run: `cargo bench -p thinkingroot-bench --bench parser_throughput --no-default-features -- --warm-up-time 1 --measurement-time 3`

Expected: 100-line Rust parse should be well under 2ms. 2000-line under 10ms.

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-bench/benches/micro/parser_throughput.rs
git commit -m "feat(bench): add Criterion parser throughput benchmarks for Rust/Python/TS/Markdown"
```

---

## Task 6: Serialization Micro-Benchmarks (Criterion)

**Files:**
- Create: `crates/thinkingroot-bench/benches/micro/serialization.rs`

- [ ] **Step 1: Create the serialization benchmarks**

Create `crates/thinkingroot-bench/benches/micro/serialization.rs`:

```rust
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use thinkingroot_core::{
    Claim, ClaimType, Confidence, Entity, EntityType, Sensitivity, Source, SourceType,
    WorkspaceId,
};

fn make_entity(i: usize) -> Entity {
    Entity::new(format!("bench-entity-{i}"), EntityType::Service)
        .with_description(format!("A benchmark entity for serialization testing #{i}"))
}

fn make_claim(i: usize) -> Claim {
    let ws = WorkspaceId::new();
    let src = thinkingroot_core::SourceId::new();
    Claim::new(
        format!("Claim {i}: The system processes requests within 10ms"),
        ClaimType::Fact,
        src,
        ws,
    )
    .with_confidence(0.85)
    .with_sensitivity(Sensitivity::Internal)
}

fn bench_entity_json_roundtrip(c: &mut Criterion) {
    let entity = make_entity(0);
    c.bench_function("serde/entity_json_roundtrip", |b| {
        b.iter(|| {
            let json = serde_json::to_string(&entity).unwrap();
            let _: Entity = serde_json::from_str(&json).unwrap();
        });
    });
}

fn bench_claim_json_roundtrip(c: &mut Criterion) {
    let claim = make_claim(0);
    c.bench_function("serde/claim_json_roundtrip", |b| {
        b.iter(|| {
            let json = serde_json::to_string(&claim).unwrap();
            let _: Claim = serde_json::from_str(&json).unwrap();
        });
    });
}

fn bench_entity_vec_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("serde/entity_vec_json");
    for &count in &[10, 100, 1000] {
        let entities: Vec<Entity> = (0..count).map(make_entity).collect();
        group.bench_with_input(
            BenchmarkId::new("serialize", count),
            &entities,
            |b, ents| {
                b.iter(|| serde_json::to_string(ents).unwrap());
            },
        );
        let json = serde_json::to_string(&entities).unwrap();
        group.bench_with_input(
            BenchmarkId::new("deserialize", count),
            &json,
            |b, j| {
                b.iter(|| serde_json::from_str::<Vec<Entity>>(j).unwrap());
            },
        );
    }
    group.finish();
}

fn bench_claim_msgpack_roundtrip(c: &mut Criterion) {
    let claim = make_claim(0);
    c.bench_function("serde/claim_msgpack_roundtrip", |b| {
        b.iter(|| {
            let packed = rmp_serde::to_vec(&claim).unwrap();
            let _: Claim = rmp_serde::from_slice(&packed).unwrap();
        });
    });
}

fn bench_claim_vec_msgpack(c: &mut Criterion) {
    let mut group = c.benchmark_group("serde/claim_vec_msgpack");
    for &count in &[10, 100, 1000] {
        let claims: Vec<Claim> = (0..count).map(make_claim).collect();
        group.bench_with_input(
            BenchmarkId::new("serialize", count),
            &claims,
            |b, cs| {
                b.iter(|| rmp_serde::to_vec(cs).unwrap());
            },
        );
    }
    group.finish();
}

fn bench_blake3_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("serde/blake3_hash");
    for &size_kb in &[1, 10, 100, 1000] {
        let data = vec![0x42u8; size_kb * 1024];
        group.bench_with_input(
            BenchmarkId::new("kb", size_kb),
            &data,
            |b, d| {
                b.iter(|| blake3::hash(d));
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_entity_json_roundtrip,
    bench_claim_json_roundtrip,
    bench_entity_vec_json,
    bench_claim_msgpack_roundtrip,
    bench_claim_vec_msgpack,
    bench_blake3_hash,
);
criterion_main!(benches);
```

- [ ] **Step 2: Verify compilation and run**

Run: `cargo bench -p thinkingroot-bench --bench serialization --no-default-features --no-run`

Expected: Clean compilation.

- [ ] **Step 3: Commit**

```bash
git add crates/thinkingroot-bench/benches/micro/serialization.rs
git commit -m "feat(bench): add Criterion serialization benchmarks (JSON, MessagePack, BLAKE3)"
```

---

## Task 7: Pipeline End-to-End Macro-Benchmarks (Divan)

**Files:**
- Create: `crates/thinkingroot-bench/benches/macro/pipeline_e2e.rs`

- [ ] **Step 1: Create the pipeline benchmarks**

Create `crates/thinkingroot-bench/benches/macro/pipeline_e2e.rs`:

```rust
use std::io::Write;
use std::path::Path;

use tempfile::TempDir;
use thinkingroot_bench::Scale;

/// Generate a synthetic workspace directory with Rust source files
/// at the given scale. Returns the TempDir (keeps it alive) and path.
fn generate_workspace(scale: Scale) -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Create .thinkingroot/config.toml
    let data_dir = root.join(".thinkingroot");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join("config.toml"),
        r#"
[workspace]
name = "bench-workspace"
data_dir = ".thinkingroot"

[llm]
default_provider = "bedrock"
extraction_model = "amazon.nova-micro-v1:0"

[extraction]
max_chunk_tokens = 4000
min_confidence = 0.5

[compilation]
enabled_artifacts = ["entity_page", "architecture_map"]
output_dir = "artifacts"

[verification]
staleness_days = 90

[parsers]
exclude_patterns = [".thinkingroot/**"]
respect_gitignore = false
max_file_size = 1048576
"#,
    )
    .unwrap();

    // Generate source files proportional to scale
    let file_count = match scale {
        Scale::Small => 50,
        Scale::Medium => 500,
        Scale::Large => 5000,
    };

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    for i in 0..file_count {
        let subdir = src_dir.join(format!("module_{}", i / 50));
        std::fs::create_dir_all(&subdir).unwrap();
        let mut f = std::fs::File::create(subdir.join(format!("file_{i}.rs"))).unwrap();
        writeln!(f, "//! Module file_{i}").unwrap();
        writeln!(f, "use std::collections::HashMap;\n").unwrap();
        for j in 0..20 {
            writeln!(
                f,
                "/// Handler for request type {j}\n\
                 pub fn handler_{i}_{j}(input: &str) -> String {{\n\
                 \tinput.to_uppercase()\n\
                 }}\n"
            )
            .unwrap();
        }
    }

    dir
}

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn parse_stage(bencher: divan::Bencher, scale: Scale) {
    let workspace = generate_workspace(scale);
    let config = thinkingroot_core::config::ParserConfig::default();
    bencher.bench_local(move || {
        thinkingroot_parse::parse_directory(workspace.path(), &config).unwrap()
    });
}

#[divan::bench(args = [Scale::Small, Scale::Medium])]
fn parse_and_index(bencher: divan::Bencher, scale: Scale) {
    let workspace = generate_workspace(scale);
    let config = thinkingroot_core::config::ParserConfig::default();
    bencher.bench_local(move || {
        let docs = thinkingroot_parse::parse_directory(workspace.path(), &config).unwrap();
        // Measure parse + graph insertion (no LLM extraction)
        let tmpdir = TempDir::new().unwrap();
        let graph_dir = tmpdir.path().join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();
        let graph = thinkingroot_graph::graph::GraphStore::init(&graph_dir).unwrap();
        for doc in &docs {
            let source = thinkingroot_core::Source::new(
                doc.uri.clone(),
                doc.source_type,
            )
            .with_hash(doc.content_hash.clone());
            let _ = graph.insert_source(&source);
        }
    });
}

fn main() {
    divan::main();
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo bench -p thinkingroot-bench --bench pipeline_e2e --no-default-features --no-run`

Expected: Clean compilation.

- [ ] **Step 3: Run at small scale**

Run: `cargo bench -p thinkingroot-bench --bench pipeline_e2e --no-default-features -- --sample-count 5`

Expected: `parse_stage/small` should complete in under 2 seconds.

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-bench/benches/macro/pipeline_e2e.rs
git commit -m "feat(bench): add Divan pipeline end-to-end macro-benchmarks at 3 scale tiers"
```

---

## Task 8: Stage Timing and Cache Effectiveness Macro-Benchmarks (Divan)

**Files:**
- Create: `crates/thinkingroot-bench/benches/macro/stage_timing.rs`
- Create: `crates/thinkingroot-bench/benches/macro/cache_effectiveness.rs`

- [ ] **Step 1: Create stage timing benchmarks**

Create `crates/thinkingroot-bench/benches/macro/stage_timing.rs`:

```rust
use tempfile::TempDir;
use thinkingroot_bench::{Fixture, Scale};
use thinkingroot_core::{Entity, EntityType, Relation, RelationType};
use thinkingroot_graph::graph::GraphStore;

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn graph_insert_entities(bencher: divan::Bencher, scale: Scale) {
    let count = scale.entity_count();
    let entities: Vec<Entity> = (0..count)
        .map(|i| Entity::new(format!("entity-{i}"), EntityType::Service))
        .collect();

    bencher.bench_local(move || {
        let tmpdir = TempDir::new().unwrap();
        let graph_dir = tmpdir.path().join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();
        let graph = GraphStore::init(&graph_dir).unwrap();
        for entity in &entities {
            graph.insert_entity(entity).unwrap();
        }
    });
}

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn graph_insert_claims(bencher: divan::Bencher, scale: Scale) {
    let count = scale.claim_count();
    let ws = thinkingroot_core::WorkspaceId::new();
    let src = thinkingroot_core::SourceId::new();
    let claims: Vec<thinkingroot_core::Claim> = (0..count)
        .map(|i| {
            thinkingroot_core::Claim::new(
                format!("Claim {i}"),
                thinkingroot_core::ClaimType::Fact,
                src,
                ws,
            )
            .with_confidence(0.8)
        })
        .collect();

    bencher.bench_local(move || {
        let tmpdir = TempDir::new().unwrap();
        let graph_dir = tmpdir.path().join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();
        let graph = GraphStore::init(&graph_dir).unwrap();
        for claim in &claims {
            graph.insert_claim(claim).unwrap();
        }
    });
}

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn graph_link_relations(bencher: divan::Bencher, scale: Scale) {
    let fix = Fixture::generate(scale);
    let relation_count = scale.relation_count().min(fix.entity_names.len() * 2);

    bencher.bench_local(move || {
        // Re-link relations on an existing populated graph
        for i in 0..relation_count {
            let from = &fix.entity_names[i % fix.entity_names.len()];
            let to = &fix.entity_names[(i + 1) % fix.entity_names.len()];
            let _ = fix.graph.link_entities(from, to, "depends_on");
        }
    });
}

#[divan::bench(args = [Scale::Small, Scale::Medium])]
fn entity_resolution_scan(bencher: divan::Bencher, scale: Scale) {
    let fix = Fixture::generate(scale);
    bencher.bench_local(move || {
        fix.graph.get_entities_with_aliases().unwrap()
    });
}

fn main() {
    divan::main();
}
```

- [ ] **Step 2: Create cache effectiveness benchmarks**

Create `crates/thinkingroot-bench/benches/macro/cache_effectiveness.rs`:

```rust
use thinkingroot_bench::Scale;
use thinkingroot_core::ContentHash;

#[divan::bench(args = [1, 10, 100, 1000])]
fn blake3_hash_kb(bencher: divan::Bencher, size_kb: usize) {
    let data = vec![0x42u8; size_kb * 1024];
    bencher.bench_local(move || {
        ContentHash::from_bytes(&data)
    });
}

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn fingerprint_check_all_cached(bencher: divan::Bencher, scale: Scale) {
    let fix = thinkingroot_bench::Fixture::generate(scale);
    // All hashes exist in the graph — simulates 100% cache hit
    bencher.bench_local(move || {
        for i in 0..100 {
            let hash = format!(
                "{}",
                blake3::hash(format!("hash_{i}").as_bytes())
            );
            let _ = fix.graph.source_hash_exists(&hash);
        }
    });
}

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn fingerprint_check_none_cached(bencher: divan::Bencher, scale: Scale) {
    let fix = thinkingroot_bench::Fixture::generate(scale);
    // All hashes are novel — simulates 0% cache hit
    bencher.bench_local(move || {
        for i in 0..100 {
            let _ = fix.graph.source_hash_exists(&format!("nonexistent_hash_{i}"));
        }
    });
}

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn fingerprint_check_mixed_80_20(bencher: divan::Bencher, scale: Scale) {
    let fix = thinkingroot_bench::Fixture::generate(scale);
    // 80% existing hashes, 20% novel
    bencher.bench_local(move || {
        for i in 0..100 {
            let hash = if i < 80 {
                // These hashes match what the fixture generator created
                format!(
                    "{}",
                    blake3::hash(format!("hash_{}", i % 50).as_bytes())
                )
            } else {
                format!("novel_hash_{i}")
            };
            let _ = fix.graph.source_hash_exists(&hash);
        }
    });
}

fn main() {
    divan::main();
}
```

- [ ] **Step 3: Verify both compile**

Run: `cargo bench -p thinkingroot-bench --bench stage_timing --bench cache_effectiveness --no-default-features --no-run`

Expected: Clean compilation.

- [ ] **Step 4: Commit**

```bash
git add crates/thinkingroot-bench/benches/macro/stage_timing.rs crates/thinkingroot-bench/benches/macro/cache_effectiveness.rs
git commit -m "feat(bench): add Divan stage timing and cache effectiveness macro-benchmarks"
```

---

## Task 9: k6 Load Test Scripts

**Files:**
- Create: `crates/thinkingroot-bench/benches/load/rest_search.js`
- Create: `crates/thinkingroot-bench/benches/load/rest_entities.js`
- Create: `crates/thinkingroot-bench/benches/load/mcp_tools.js`
- Create: `crates/thinkingroot-bench/benches/load/mixed_workload.js`
- Create: `crates/thinkingroot-bench/benches/load/run_load_test.sh`

- [ ] **Step 1: Create REST search load test**

Create `crates/thinkingroot-bench/benches/load/rest_search.js`:

```javascript
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const searchLatency = new Trend('search_latency', true);
const searchFailRate = new Rate('search_fail_rate');

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:9876';
const WS = __ENV.WORKSPACE || 'bench-workspace';

const QUERIES = [
    'authentication', 'database', 'cache', 'handler', 'service',
    'pipeline', 'resolver', 'validator', 'scheduler', 'gateway',
    'processor', 'monitor', 'adapter', 'controller', 'engine',
];

export const options = {
    stages: [
        { duration: '30s', target: 50 },    // ramp up
        { duration: '90s', target: 50 },     // steady state
        { duration: '30s', target: 200 },    // stress
        { duration: '30s', target: 0 },      // ramp down
    ],
    thresholds: {
        search_latency: ['p(95)<10', 'p(99)<25'],
        search_fail_rate: ['rate<0.001'],
        http_reqs: ['rate>100'],
    },
};

export default function () {
    const query = QUERIES[Math.floor(Math.random() * QUERIES.length)];
    const res = http.get(`${BASE_URL}/api/v1/ws/${WS}/search?q=${query}&top_k=10`);

    searchLatency.add(res.timings.duration);
    searchFailRate.add(res.status !== 200);

    check(res, {
        'status is 200': (r) => r.status === 200,
        'response has data': (r) => JSON.parse(r.body).ok === true,
    });
}
```

- [ ] **Step 2: Create REST entities load test**

Create `crates/thinkingroot-bench/benches/load/rest_entities.js`:

```javascript
import http from 'k6/http';
import { check } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const entityLatency = new Trend('entity_latency', true);
const entityFailRate = new Rate('entity_fail_rate');

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:9876';
const WS = __ENV.WORKSPACE || 'bench-workspace';

export const options = {
    stages: [
        { duration: '20s', target: 50 },
        { duration: '60s', target: 50 },
        { duration: '20s', target: 0 },
    ],
    thresholds: {
        entity_latency: ['p(95)<10', 'p(99)<25'],
        entity_fail_rate: ['rate<0.001'],
    },
};

export default function () {
    const res = http.get(`${BASE_URL}/api/v1/ws/${WS}/entities`);

    entityLatency.add(res.timings.duration);
    entityFailRate.add(res.status !== 200);

    check(res, {
        'status is 200': (r) => r.status === 200,
        'has entities': (r) => JSON.parse(r.body).ok === true,
    });
}
```

- [ ] **Step 3: Create MCP tools load test**

Create `crates/thinkingroot-bench/benches/load/mcp_tools.js`:

```javascript
import http from 'k6/http';
import { check } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const mcpLatency = new Trend('mcp_latency', true);
const mcpFailRate = new Rate('mcp_fail_rate');

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:9876';

const QUERIES = [
    'authentication flow', 'database schema', 'cache invalidation',
    'API gateway', 'service mesh', 'event pipeline',
];

function makeJsonRpc(method, params) {
    return JSON.stringify({
        jsonrpc: '2.0',
        id: Math.floor(Math.random() * 100000),
        method: method,
        params: params,
    });
}

export const options = {
    stages: [
        { duration: '20s', target: 30 },
        { duration: '60s', target: 30 },
        { duration: '20s', target: 100 },
        { duration: '20s', target: 0 },
    ],
    thresholds: {
        mcp_latency: ['p(95)<15', 'p(99)<30'],
        mcp_fail_rate: ['rate<0.001'],
    },
};

export default function () {
    // Weighted distribution: 80% search, 15% query_claims, 5% health_check
    const roll = Math.random();
    let payload;

    if (roll < 0.80) {
        const query = QUERIES[Math.floor(Math.random() * QUERIES.length)];
        payload = makeJsonRpc('tools/call', {
            name: 'search',
            arguments: { query: query, top_k: 10 },
        });
    } else if (roll < 0.95) {
        payload = makeJsonRpc('tools/call', {
            name: 'query_claims',
            arguments: { claim_type: 'fact', limit: 20 },
        });
    } else {
        payload = makeJsonRpc('tools/call', {
            name: 'health_check',
            arguments: {},
        });
    }

    const params = { headers: { 'Content-Type': 'application/json' } };
    const res = http.post(`${BASE_URL}/mcp?sessionId=bench`, payload, params);

    mcpLatency.add(res.timings.duration);
    mcpFailRate.add(res.status !== 200 && res.status !== 202);

    check(res, {
        'status is 2xx': (r) => r.status >= 200 && r.status < 300,
    });
}
```

- [ ] **Step 4: Create mixed workload load test**

Create `crates/thinkingroot-bench/benches/load/mixed_workload.js`:

```javascript
import http from 'k6/http';
import { check } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const latency = new Trend('mixed_latency', true);
const failRate = new Rate('mixed_fail_rate');

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:9876';
const WS = __ENV.WORKSPACE || 'bench-workspace';

const QUERIES = [
    'authentication', 'database', 'cache', 'handler', 'service',
    'pipeline', 'resolver', 'validator', 'scheduler', 'gateway',
];

export const options = {
    stages: [
        { duration: '30s', target: 50 },
        { duration: '120s', target: 50 },
        { duration: '30s', target: 0 },
    ],
    thresholds: {
        mixed_latency: ['p(95)<15', 'p(99)<30'],
        mixed_fail_rate: ['rate<0.001'],
        http_reqs: ['rate>500'],
    },
};

export default function () {
    const roll = Math.random();
    let res;

    if (roll < 0.50) {
        // 50% search
        const q = QUERIES[Math.floor(Math.random() * QUERIES.length)];
        res = http.get(`${BASE_URL}/api/v1/ws/${WS}/search?q=${q}&top_k=10`);
    } else if (roll < 0.70) {
        // 20% entity list
        res = http.get(`${BASE_URL}/api/v1/ws/${WS}/entities`);
    } else if (roll < 0.85) {
        // 15% claims
        res = http.get(`${BASE_URL}/api/v1/ws/${WS}/claims?type=fact&limit=50`);
    } else if (roll < 0.95) {
        // 10% relations
        res = http.get(`${BASE_URL}/api/v1/ws/${WS}/relations`);
    } else {
        // 5% health
        res = http.get(`${BASE_URL}/api/v1/ws/${WS}/health`);
    }

    latency.add(res.timings.duration);
    failRate.add(res.status !== 200);

    check(res, {
        'status is 200': (r) => r.status === 200,
    });
}
```

- [ ] **Step 5: Create orchestrator script**

Create `crates/thinkingroot-bench/benches/load/run_load_test.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

# ── ThinkingRoot Load Test Orchestrator ─────────────────────
# Usage: ./run_load_test.sh [--scale small|medium|large] [--port 9876]
#
# Builds the release binary, generates a fixture workspace, starts
# the server, runs all k6 load tests, and collects results.

SCALE="small"
PORT="9876"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../../../" && pwd)"
RESULTS_DIR="$SCRIPT_DIR/../../../target/bench-results"

while [[ $# -gt 0 ]]; do
    case $1 in
        --scale) SCALE="$2"; shift 2 ;;
        --port)  PORT="$2"; shift 2 ;;
        *)       echo "Unknown option: $1"; exit 1 ;;
    esac
done

echo "=== ThinkingRoot Load Test ==="
echo "Scale:   $SCALE"
echo "Port:    $PORT"
echo ""

# ── Check prerequisites ────────────────────────────────────
if ! command -v k6 &>/dev/null; then
    echo "ERROR: k6 not found. Install: https://grafana.com/docs/k6/latest/set-up/install-k6/"
    exit 1
fi

# ── Build release binary ───────────────────────────────────
echo ">>> Building release binary..."
cargo build --release -p thinkingroot-cli --manifest-path "$PROJECT_ROOT/Cargo.toml"

ROOT_BIN="$PROJECT_ROOT/target/release/root"

# ── Generate fixture workspace ─────────────────────────────
WORKSPACE_DIR=$(mktemp -d)
trap 'rm -rf "$WORKSPACE_DIR"; kill $SERVER_PID 2>/dev/null || true' EXIT

echo ">>> Initializing fixture workspace at $WORKSPACE_DIR..."
"$ROOT_BIN" init "$WORKSPACE_DIR" 2>/dev/null || true

# Generate synthetic source files
mkdir -p "$WORKSPACE_DIR/src"
case $SCALE in
    small)  FILE_COUNT=50 ;;
    medium) FILE_COUNT=500 ;;
    large)  FILE_COUNT=5000 ;;
    *)      echo "Unknown scale: $SCALE"; exit 1 ;;
esac

echo ">>> Generating $FILE_COUNT source files..."
for i in $(seq 0 $((FILE_COUNT - 1))); do
    DIR="$WORKSPACE_DIR/src/module_$((i / 50))"
    mkdir -p "$DIR"
    cat > "$DIR/file_${i}.rs" <<RUST
//! Module file_${i}
pub fn handler_${i}(input: &str) -> String {
    input.to_uppercase()
}
RUST
done

# ── Compile the workspace ──────────────────────────────────
echo ">>> Compiling workspace (structural extraction only)..."
"$ROOT_BIN" compile "$WORKSPACE_DIR" 2>/dev/null || echo "  (compile completed with warnings)"

# ── Start the server ───────────────────────────────────────
echo ">>> Starting server on port $PORT..."
"$ROOT_BIN" serve --port "$PORT" --path "$WORKSPACE_DIR" --no-mcp &
SERVER_PID=$!
sleep 2

# Wait for health check
echo ">>> Waiting for server to be ready..."
for i in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$PORT/api/v1/workspaces" >/dev/null 2>&1; then
        echo "  Server ready!"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: Server failed to start within 30s"
        exit 1
    fi
    sleep 1
done

# ── Run k6 tests ──────────────────────────────────────────
mkdir -p "$RESULTS_DIR"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)

echo ""
echo ">>> Running REST search load test..."
k6 run --env BASE_URL="http://127.0.0.1:$PORT" \
    --summary-export "$RESULTS_DIR/rest-search-${TIMESTAMP}.json" \
    "$SCRIPT_DIR/rest_search.js" || true

echo ""
echo ">>> Running REST entities load test..."
k6 run --env BASE_URL="http://127.0.0.1:$PORT" \
    --summary-export "$RESULTS_DIR/rest-entities-${TIMESTAMP}.json" \
    "$SCRIPT_DIR/rest_entities.js" || true

echo ""
echo ">>> Running mixed workload load test..."
k6 run --env BASE_URL="http://127.0.0.1:$PORT" \
    --summary-export "$RESULTS_DIR/mixed-workload-${TIMESTAMP}.json" \
    "$SCRIPT_DIR/mixed_workload.js" || true

# ── Results ────────────────────────────────────────────────
echo ""
echo "=== Load Test Complete ==="
echo "Results saved to: $RESULTS_DIR/"
ls -la "$RESULTS_DIR/"*"${TIMESTAMP}"* 2>/dev/null || echo "(no result files)"
```

- [ ] **Step 6: Make the script executable**

Run: `chmod +x crates/thinkingroot-bench/benches/load/run_load_test.sh`

- [ ] **Step 7: Commit**

```bash
git add crates/thinkingroot-bench/benches/load/
git commit -m "feat(bench): add k6 load test scripts (REST search, entities, MCP tools, mixed workload)"
```

---

## Task 10: CI Workflows

**Files:**
- Create: `.github/workflows/bench-pr.yml`
- Create: `.github/workflows/bench-nightly.yml`

- [ ] **Step 1: Create per-PR benchmark workflow**

Create `.github/workflows/bench-pr.yml`:

```yaml
name: Benchmark (PR)

on:
  pull_request:
    paths:
      - 'crates/**/*.rs'
      - 'Cargo.toml'
      - 'Cargo.lock'

concurrency:
  group: bench-${{ github.ref }}
  cancel-in-progress: true

jobs:
  micro-bench:
    name: Micro-benchmarks
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: bench

      - name: Run micro-benchmarks (Small scale)
        run: |
          BENCH_SCALE=small cargo bench \
            -p thinkingroot-bench \
            --no-default-features \
            --bench graph_queries \
            --bench parser_throughput \
            --bench serialization \
            -- --output-format bencher | tee output.txt

      - name: Store benchmark result
        uses: benchmark-action/github-action-benchmark@v1
        with:
          name: ThinkingRoot Micro-Benchmarks
          tool: cargo
          output-file-path: output.txt
          alert-threshold: '120%'
          comment-on-alert: true
          fail-on-alert: false
          github-token: ${{ secrets.GITHUB_TOKEN }}
          comment-always: true
          save-data-file: false
```

- [ ] **Step 2: Create nightly benchmark workflow**

Create `.github/workflows/bench-nightly.yml`:

```yaml
name: Benchmark (Nightly)

on:
  schedule:
    - cron: '0 3 * * *'
  workflow_dispatch:
    inputs:
      scale:
        description: 'Benchmark scale (small/medium/large)'
        required: false
        default: 'medium'

jobs:
  full-bench:
    name: Full Benchmark Suite
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: bench-nightly

      # ── Criterion micro-benchmarks (all scales) ──────────
      - name: Criterion micro-benchmarks
        run: |
          cargo bench \
            -p thinkingroot-bench \
            --no-default-features \
            --bench graph_queries \
            --bench parser_throughput \
            --bench serialization \
            --bench vector_search \
            -- --output-format bencher | tee micro-output.txt

      # ── Divan macro-benchmarks ───────────────────────────
      - name: Divan macro-benchmarks
        run: |
          cargo bench \
            -p thinkingroot-bench \
            --no-default-features \
            --bench pipeline_e2e \
            --bench stage_timing \
            --bench cache_effectiveness

      # ── Publish to GitHub Pages ──────────────────────────
      - name: Publish benchmark results
        uses: benchmark-action/github-action-benchmark@v1
        with:
          name: ThinkingRoot Benchmarks
          tool: cargo
          output-file-path: micro-output.txt
          auto-push: true
          gh-pages-branch: gh-pages
          benchmark-data-dir-path: dev/bench
          github-token: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/bench-pr.yml .github/workflows/bench-nightly.yml
git commit -m "ci: add per-PR and nightly benchmark CI workflows"
```

---

## Task 11: Final Integration — Compile, Run, Verify

**Files:**
- None new — this task validates everything works together.

- [ ] **Step 1: Verify full workspace compiles**

Run: `cargo check --workspace --no-default-features`

Expected: Clean compilation with no errors or warnings.

- [ ] **Step 2: Run all micro-benchmarks at small scale**

Run: `BENCH_SCALE=small cargo bench -p thinkingroot-bench --no-default-features --bench graph_queries --bench parser_throughput --bench serialization --bench vector_search -- --warm-up-time 1 --measurement-time 3`

Expected: All benchmarks complete. Check that:
- `graph/entity_lookup_by_name/small` < 0.5ms
- `parser/rust/lines/100` < 2ms
- `serde/entity_json_roundtrip` < 0.1ms
- `vector/cosine_search_top10/small` < 5ms

- [ ] **Step 3: Run Divan macro-benchmarks at small scale**

Run: `cargo bench -p thinkingroot-bench --no-default-features --bench stage_timing --bench cache_effectiveness -- --sample-count 3`

Expected: All benchmarks complete without errors.

- [ ] **Step 4: Verify HTML reports generated**

Run: `ls target/criterion/report/index.html`

Expected: File exists. Open it in a browser to verify charts render.

- [ ] **Step 5: Run clippy on the bench crate**

Run: `cargo clippy -p thinkingroot-bench --no-default-features -- -D warnings`

Expected: No warnings.

- [ ] **Step 6: Commit any fixes**

If any compilation or lint fixes were needed:

```bash
git add -A
git commit -m "fix(bench): resolve compilation and lint issues in benchmark suite"
```
