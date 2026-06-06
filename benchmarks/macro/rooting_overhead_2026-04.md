# Rooting Overhead Benchmark — 2026-04-23

Harness: `crates/thinkingroot-bench/benches/macro/rooting_overhead.rs`
Runner: `divan` (comfy Rust benchmarking framework)
Profile: `bench` (optimized + debuginfo)
Host: macOS 25.3.0, Darwin (native build)
Timer precision: 41 ns

## Run Command

```
cargo bench -p thinkingroot-bench --bench rooting_overhead
```

## Results — `root_batch` @ N = 100 candidate claims

| Metric  | Time     |
|---------|----------|
| Fastest | 23.46 ms |
| Median  | 24.22 ms |
| Mean    | 24.11 ms |
| Slowest | 25.18 ms |
| Samples | 100      |
| Iters   | 100      |

**Per-claim cost (median):** 24.22 ms / 100 = **242 µs / claim**

## Overhead Analysis

Per-claim rooting cost is 242 µs. Pipeline stages that precede rooting:

| Stage          | Typical cost per claim | Source                                           |
|----------------|------------------------|--------------------------------------------------|
| LLM extraction | 50 – 200 ms            | Azure gpt-4-1-mini, `extractor.rs` request tier  |
| Grounding      | 5 – 15 ms              | `thinkingroot-ground`, local NLI + lexical judge |
| Linking        | 1 – 3 ms               | `thinkingroot-link`, entity resolution           |
| **Rooting**    | **0.242 ms**           | This bench                                       |

LLM extraction dominates the pipeline by 2–3 orders of magnitude. Rooting's contribution to end-to-end compile time is < 1 %, well under the 10 % target in the ship plan.

## Probe Breakdown (from paper figure 4, divan micro-split)

| Probe         | Cost per claim | Fatal |
|---------------|----------------|-------|
| Provenance    | 20 µs          | yes   |
| Contradiction | 120 µs         | yes   |
| Predicate     | 35 µs          | no    |
| Topology      | 40 µs          | no    |
| Temporal      | 25 µs          | no    |
| **Sum**       | **240 µs**     | —     |

Sum matches batch median (242 µs) within noise. Bench number in this file is the authoritative production figure.

## Verdict

✅ A1 PASS — Rooting overhead ≤ 10 % of compile time. Proceeding to B1.
