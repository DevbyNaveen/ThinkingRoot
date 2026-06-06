# ThinkingRoot Benchmark Testing Suite — Design Spec

**Date:** 2026-04-13
**Status:** Approved
**Goal:** World-class benchmark suite proving sub-10ms retrieval latency across 3 orders of magnitude, with automated regression detection and a public performance dashboard.

---

## 1. Motivation

ThinkingRoot competes with top-tier MNCs in the knowledge infrastructure space. Performance — especially retrieval latency — is a key differentiator. This benchmark suite:

- **Proves** sub-10ms retrieval at all scale tiers (500 → 50K entities)
- **Prevents** regressions via automated CI benchmarks on every PR
- **Tracks** performance trends over time via nightly authoritative runs
- **Publishes** results on a public dashboard for credibility

---

## 2. Architecture Overview

### Three-Tool Stack

| Tier | Tool | Purpose |
|------|------|---------|
| **Micro-benchmarks** | Criterion.rs | Statistical rigor for sub-millisecond hot paths. Confidence intervals, outlier detection, HTML reports. |
| **Macro-benchmarks** | Divan | Ergonomic API for pipeline/stage-level measurements. Scale-tier parameterisation via `args`. |
| **Load testing** | k6 (Grafana) | HTTP/MCP load testing. Scripted scenarios, ramp-up patterns, p50/p95/p99 reporting. |

### Three Scale Tiers

| Tier | Entities | Claims | Relations | Embeddings |
|------|----------|--------|-----------|------------|
| **Small** | 500 | 2,000 | 1,000 | 800 |
| **Medium** | 5,000 | 20,000 | 10,000 | 8,000 |
| **Large** | 50,000 | 200,000 | 100,000 | 80,000 |

### Three Measurement Layers

Every retrieval benchmark is measured at three layers to show overhead transparency:

1. **Core engine** — `QueryEngine::search()` pure computation
2. **REST API** — HTTP request → JSON response end-to-end
3. **MCP tool call** — JSON-RPC `tools/call` → SSE response end-to-end

---

## 3. Crate Layout

```
crates/thinkingroot-bench/
├── Cargo.toml
├── benches/
│   ├── micro/
│   │   ├── graph_queries.rs        # CozoDB Datalog query benchmarks
│   │   ├── vector_search.rs        # FastEmbed embed + cosine similarity
│   │   ├── parser_throughput.rs    # Tree-sitter parse by language & file size
│   │   └── serialization.rs       # serde_json / rmp-serde round-trip
│   ├── macro/
│   │   ├── pipeline_e2e.rs         # Full 6-stage pipeline at 3 scale tiers
│   │   ├── stage_timing.rs         # Individual stage benchmarks
│   │   └── cache_effectiveness.rs  # BLAKE3 fingerprint cache-hit scenarios
│   └── load/
│       ├── rest_search.js          # Search endpoint load test
│       ├── rest_entities.js        # Entity listing throughput
│       ├── mcp_tools.js            # MCP tool/call dispatch throughput
│       ├── mixed_workload.js       # Realistic mixed read pattern
│       └── run_load_test.sh        # Orchestrator: build → serve → k6 → report
├── fixtures/                       # Generated at runtime, gitignored
├── src/
│   ├── lib.rs                      # Shared test utilities
│   ├── fixtures.rs                 # Fixture generation & loading
│   └── scale.rs                    # Scale enum + data generators
└── README.md                       # How to run benchmarks
```

### Cargo.toml Structure

```toml
[package]
name = "thinkingroot-bench"
version = "0.1.0"
edition = "2024"
rust-version = "1.91"
publish = false

[features]
default = ["vector"]
vector = ["thinkingroot-graph/vector", "thinkingroot-serve/vector"]

[dependencies]
thinkingroot-core = { workspace = true }
thinkingroot-graph = { workspace = true }
thinkingroot-parse = { workspace = true }
thinkingroot-link = { workspace = true }
thinkingroot-compile = { workspace = true }
thinkingroot-verify = { workspace = true }
thinkingroot-serve = { workspace = true }
tokio = { workspace = true }
serde_json = { workspace = true }
rmp-serde = { workspace = true }
tempfile = { workspace = true }
rand = "0.9"

[dev-dependencies]
criterion = { version = "0.6", features = ["html_reports"] }
divan = "0.1"

# Criterion bench targets
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

# Divan bench targets
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

---

## 4. Micro-Benchmarks (Criterion)

### 4.1 Graph Queries (`graph_queries.rs`)

| Benchmark | What it measures | Target |
|-----------|-----------------|--------|
| `entity_lookup_by_name` | Single entity fetch by canonical name | < 0.5ms |
| `entity_lookup_by_alias` | Alias resolution → entity | < 1ms |
| `claims_by_entity` | All claims linked to one entity | < 2ms |
| `claims_by_type_filtered` | Claims filtered by type + min_confidence | < 2ms |
| `relations_for_entity` | All relations from/to an entity | < 1ms |
| `full_subgraph_2hop` | Entity + relations + neighbors (2-hop) | < 5ms |
| `contradiction_detection` | Find contradictions for a claim set | < 3ms |

Each benchmark runs at all 3 scale tiers using Criterion's parameterised groups:
```rust
fn bench_entity_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("entity_lookup_by_name");
    for scale in [Scale::Small, Scale::Medium, Scale::Large] {
        let fixture = Fixture::load_or_generate(scale);
        group.bench_with_input(
            BenchmarkId::from_parameter(scale),
            &fixture,
            |b, fix| b.iter(|| fix.graph.get_entity_by_name(&fix.sample_entity_name)),
        );
    }
    group.finish();
}
```

### 4.2 Vector Search (`vector_search.rs`)

| Benchmark | What it measures | Target |
|-----------|-----------------|--------|
| `embed_single` | Single string → 384-dim vector | < 15ms |
| `embed_batch_10` | 10 strings batch embed | < 30ms |
| `embed_batch_100` | 100 strings batch embed | < 200ms |
| `cosine_search_top5` | Search top-5 over index | < 3ms |
| `cosine_search_top10` | Search top-10 over index | < 5ms |
| `cosine_search_top50` | Search top-50 over index | < 8ms |
| `upsert_single` | Insert one embedding | < 1ms |
| `upsert_batch_100` | Batch insert 100 embeddings | < 10ms |

Cosine search benchmarks use pre-computed random vectors (not real fastembed) so they run with `--no-default-features`. Embedding benchmarks require `vector` feature.

### 4.3 Parser Throughput (`parser_throughput.rs`)

| Benchmark | What it measures | Target |
|-----------|-----------------|--------|
| `parse_rust_small` | 100-line Rust file → DocumentIR | < 2ms |
| `parse_rust_large` | 2000-line Rust file → DocumentIR | < 10ms |
| `parse_python_small` | 100-line Python file | < 2ms |
| `parse_typescript_large` | 2000-line TypeScript file | < 10ms |
| `parse_markdown_prose` | 500-line markdown doc | < 3ms |
| `chunk_extraction` | DocumentIR → Chunks (AST walk) | < 1ms |

Source files are generated synthetically (realistic function definitions, imports, comments, docstrings) — not random characters.

### 4.4 Serialization (`serialization.rs`)

| Benchmark | What it measures | Target |
|-----------|-----------------|--------|
| `entity_json_roundtrip` | Entity → JSON → Entity | < 0.1ms |
| `claim_json_roundtrip` | Claim → JSON → Claim | < 0.1ms |
| `api_response_serialize` | ApiResponse<Vec<Entity>> (100 entities) | < 0.5ms |
| `msgpack_claim_roundtrip` | Claim → MessagePack → Claim | < 0.05ms |

---

## 5. Macro-Benchmarks (Divan)

### 5.1 Pipeline End-to-End (`pipeline_e2e.rs`)

| Benchmark | Small | Medium | Large |
|-----------|-------|--------|-------|
| `full_pipeline` (structural only, no LLM) | < 2s | < 10s | < 60s |
| `full_pipeline_cached` (re-run, 100% fingerprint hit) | < 0.5s | < 2s | < 10s |
| `incremental_1pct_change` (1% files changed) | < 0.5s | < 3s | < 15s |

```rust
#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn full_pipeline(bencher: divan::Bencher, scale: &Scale) {
    let workspace = generate_workspace(scale);
    bencher.bench_local(|| {
        run_pipeline(&workspace)
    });
}
```

### 5.2 Stage Timing (`stage_timing.rs`)

| Stage | Small | Medium | Large |
|-------|-------|--------|-------|
| `parse_stage` | < 0.5s | < 3s | < 15s |
| `link_stage` (entity resolution) | < 0.3s | < 2s | < 10s |
| `compile_stage` (artifact generation) | < 1s | < 5s | < 30s |
| `verify_stage` (health scoring) | < 0.2s | < 1s | < 5s |

Extract stage is excluded — it calls LLM APIs. The structural extraction path (tree-sitter-derived claims without LLM) is benchmarked instead.

### 5.3 Cache Effectiveness (`cache_effectiveness.rs`)

| Benchmark | What it measures |
|-----------|-----------------|
| `blake3_hash_throughput` | MB/s hashing rate |
| `fingerprint_check_all_cached` | Lookup time when 100% cache hit |
| `fingerprint_check_none_cached` | Lookup time when 0% cache hit |
| `fingerprint_check_mixed` | 80% hit / 20% miss (realistic) |

---

## 6. Load Testing (k6)

### 6.1 REST Search (`rest_search.js`)

```
Stages:
  0-30s:    ramp 1 → 50 VUs
  30-120s:  hold 50 VUs (steady state)
  120-150s: ramp 50 → 200 VUs (stress)
  150-180s: ramp down → 0 VUs

Thresholds:
  http_req_duration p95 < 10ms
  http_req_duration p99 < 25ms
  http_req_failed   < 0.1%
  http_reqs         > 1000/s  (throughput floor)
```

### 6.2 MCP Tool Call Load (`mcp_tools.js`)

Simulates concurrent AI agents:
- `search` tool: 80% of calls
- `query_claims`: 15%
- `health_check`: 5%

Same ramp-up pattern as REST. Thresholds: p95 < 15ms, p99 < 30ms.

### 6.3 Mixed Workload (`mixed_workload.js`)

Realistic traffic distribution:
- 50% search queries
- 20% entity lookups
- 15% claim listings
- 10% relation queries
- 5% health checks

### 6.4 Orchestrator Script (`run_load_test.sh`)

Fully automated:
1. `cargo build --release -p thinkingroot-cli`
2. Generate fixture workspace at specified scale
3. `root compile` the fixture workspace
4. Start `root serve --port 9876 --path <fixture>` in background
5. Wait for `GET /api/v1/workspaces` to return 200
6. Run all k6 scripts sequentially
7. Kill server process
8. Collect results → `results/load-test-{timestamp}.json`

```bash
./crates/thinkingroot-bench/benches/load/run_load_test.sh --scale medium
```

---

## 7. Automated Fixture Generation

### Scale Enum (`scale.rs`)

```rust
#[derive(Clone, Copy, Debug)]
pub enum Scale {
    Small,
    Medium,
    Large,
}

impl Scale {
    pub fn entity_count(&self) -> usize {
        match self { Small => 500, Medium => 5_000, Large => 50_000 }
    }
    pub fn claim_count(&self) -> usize {
        match self { Small => 2_000, Medium => 20_000, Large => 200_000 }
    }
    pub fn relation_count(&self) -> usize {
        match self { Small => 1_000, Medium => 10_000, Large => 100_000 }
    }
    pub fn embedding_count(&self) -> usize {
        match self { Small => 800, Medium => 8_000, Large => 80_000 }
    }
}
```

### Fixture Generator (`fixtures.rs`)

The `Fixture` struct holds a populated `GraphStore` + `VectorStore` ready for benchmarking:

```rust
pub struct Fixture {
    pub graph: GraphStore,
    pub vector: VectorStore,  // behind #[cfg(feature = "vector")]
    pub scale: Scale,
    pub sample_entity_name: String,   // for lookup benchmarks
    pub sample_claim_id: ClaimId,     // for claim benchmarks
    pub sample_query: String,         // for search benchmarks
    _tmpdir: TempDir,                 // cleanup on drop
}
```

**Data realism:**
- Entity names drawn from templates: `"{adj}-{noun}-service"`, `"Team {greek_letter}"`, `"{module}Api"`, `"lib{name}"`
- Claim types follow distribution: Fact 40%, Decision 15%, Architecture 10%, Dependency 10%, ApiSignature 8%, Requirement 7%, Definition 5%, Plan 3%, Metric 2%
- Relations follow power-law: ~5% of entities are hubs with 20+ connections, 80% have 1-3 connections
- Confidence values: normal distribution centred at 0.75, clamped [0.3, 1.0]

---

## 8. CI Integration

### 8.1 Per-PR: Fast Micro-Benchmarks

**File:** `.github/workflows/bench-pr.yml`

```yaml
name: Benchmark (PR)
on: pull_request

jobs:
  micro-bench:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2

      # Run micro-benchmarks at Small scale only
      - name: Run benchmarks (current)
        run: |
          BENCH_SCALE=small cargo bench -p thinkingroot-bench \
            --no-default-features \
            --bench graph_queries \
            --bench parser_throughput \
            --bench serialization \
            -- --save-baseline current

      # Compare against main baseline
      - name: Compare against main
        uses: benchmark-action/github-action-benchmark@v1
        with:
          tool: 'cargo'
          output-file-path: target/criterion/**/*.json
          alert-threshold: '120%'       # Warn at 20% regression
          comment-on-alert: true
          fail-on-alert: false          # Soft signal, not a gate
          github-token: ${{ secrets.GITHUB_TOKEN }}
```

**Time budget:** ~3 minutes. Runs `--no-default-features` (no ONNX) for speed.

### 8.2 Nightly: Full Suite

**File:** `.github/workflows/bench-nightly.yml`

```yaml
name: Benchmark (Nightly)
on:
  schedule:
    - cron: '0 3 * * *'       # 3am UTC daily
  workflow_dispatch:            # Manual trigger

jobs:
  full-bench:
    runs-on: self-hosted        # Consistent hardware for stable numbers
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable

      # Micro-benchmarks at all scales
      - name: Criterion micro-benchmarks
        run: cargo bench -p thinkingroot-bench --no-default-features

      # Macro-benchmarks at all scales
      - name: Divan macro-benchmarks
        run: cargo bench -p thinkingroot-bench --bench pipeline_e2e --bench stage_timing --bench cache_effectiveness

      # Load tests (requires k6)
      - name: Load tests
        run: |
          ./crates/thinkingroot-bench/benches/load/run_load_test.sh --scale medium

      # Publish to GitHub Pages dashboard
      - name: Publish results
        uses: benchmark-action/github-action-benchmark@v1
        with:
          tool: 'cargo'
          auto-push: true
          gh-pages-branch: gh-pages
          benchmark-data-dir-path: dev/bench
```

### 8.3 Public Dashboard

Static HTML on GitHub Pages (`gh-pages` branch) showing:

- **Latency-vs-scale curves** — flat line under 10ms across Small/Medium/Large
- **Trend over time** — catch gradual regressions across nightly runs
- **Per-component breakdown** — graph / vector / parse / REST / MCP tabs
- **Load test results** — throughput (req/s), p50/p95/p99 latency, error rate
- **Badge** — embeddable SVG badge: "retrieval p95: 3.2ms" for README

---

## 9. Developer Experience

```bash
# ── Quick start ──────────────────────────────────────────────
cargo bench -p thinkingroot-bench                      # Run all benchmarks

# ── Micro-benchmarks (Criterion) ────────────────────────────
cargo bench -p thinkingroot-bench --bench graph_queries
cargo bench -p thinkingroot-bench --bench vector_search       # Requires vector feature
cargo bench -p thinkingroot-bench --bench parser_throughput
cargo bench -p thinkingroot-bench --bench serialization

# ── Macro-benchmarks (Divan) ────────────────────────────────
cargo bench -p thinkingroot-bench --bench pipeline_e2e
cargo bench -p thinkingroot-bench --bench stage_timing
cargo bench -p thinkingroot-bench --bench cache_effectiveness

# ── Scale control ────────────────────────────────────────────
BENCH_SCALE=small cargo bench -p thinkingroot-bench    # Fast iteration
BENCH_SCALE=large cargo bench -p thinkingroot-bench    # Full scale

# ── Load tests ───────────────────────────────────────────────
./crates/thinkingroot-bench/benches/load/run_load_test.sh --scale medium

# ── Without vector/ONNX (fast, CI-compatible) ───────────────
cargo bench -p thinkingroot-bench --no-default-features

# ── View HTML reports ────────────────────────────────────────
open target/criterion/report/index.html
```

---

## 10. Feature Flag Handling

The `vector` feature follows the workspace convention:

```toml
# In thinkingroot-bench/Cargo.toml
[features]
default = ["vector"]
vector = ["thinkingroot-graph/vector", "thinkingroot-serve/vector"]
```

- `vector_search.rs` bench target has `required-features = ["vector"]`
- Cosine search benchmarks that use pre-computed vectors work without the feature
- CI per-PR runs `--no-default-features` for speed
- Nightly runs with default features (includes ONNX) on self-hosted runner

---

## 11. Latency Targets Summary

### Core Engine (< 10ms retrieval guarantee)

| Operation | Target | Layer |
|-----------|--------|-------|
| Entity lookup by name | < 0.5ms | Core |
| Entity lookup by alias | < 1ms | Core |
| Claims by entity | < 2ms | Core |
| Relations for entity | < 1ms | Core |
| 2-hop subgraph | < 5ms | Core |
| Vector search top-10 | < 5ms | Core |
| Contradiction detection | < 3ms | Core |

### REST API (core + HTTP overhead)

| Operation | Target |
|-----------|--------|
| Search endpoint p95 | < 10ms |
| Entity lookup p95 | < 5ms |
| Claims list p95 | < 8ms |

### MCP (core + JSON-RPC + SSE overhead)

| Operation | Target |
|-----------|--------|
| search tool p95 | < 12ms |
| query_claims tool p95 | < 10ms |
| health_check tool p95 | < 5ms |

### Pipeline (wall-clock)

| Operation | Small | Medium | Large |
|-----------|-------|--------|-------|
| Full pipeline (structural) | < 2s | < 10s | < 60s |
| Cached re-run | < 0.5s | < 2s | < 10s |

---

## 12. Non-Goals

- **LLM extraction benchmarks** — depends on external API latency, not our code
- **Benchmarking Python SDK** — PyO3 overhead is negligible; HTTP client benchmarks are covered by k6
- **Cross-platform benchmark comparison** — too many variables; nightly runs on one consistent machine
- **Flame-graph automation** — useful for ad-hoc profiling but not part of the automated suite

---

## 13. Dependencies Added

| Dependency | Version | Scope | Purpose |
|------------|---------|-------|---------|
| `criterion` | 0.6 | dev | Micro-benchmark framework with statistical analysis |
| `divan` | 0.1 | dev | Macro-benchmark framework with parameterised benches |
| `rand` | 0.9 | normal | Fixture data generation (realistic distributions) |
| `k6` | latest | external | HTTP load testing (installed separately, not a Rust dep) |

---

## 14. Success Criteria

1. All retrieval benchmarks (core engine) complete under 10ms at `Scale::Large` (50K entities)
2. Latency-vs-scale curve is flat (< 20% increase from Small → Large)
3. Per-PR CI benchmark runs in under 3 minutes
4. Nightly full suite completes in under 30 minutes
5. Public dashboard is live on GitHub Pages with auto-updating charts
6. Load tests prove > 1000 req/s at p95 < 10ms under 50 concurrent users
