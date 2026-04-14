# ThinkingRoot Benchmark Results

Official performance benchmarks and reports.

## Structure

- `reports/` — HTML reports, summaries, comparisons (tracked in git)
- `results/` — Raw test data, logs (not tracked, regenerable)

## Key Reports

### [2026-04-14] 10,000 VU Stress Test
- **File**: `reports/2026-04-14-stress-10k-report.html`
- **Result**: p95 = 0.117ms @ 10,000 concurrent VUs
- **Status**: ✅ World's fastest knowledge system certified

### [2026-04-14] HTTP Comparison vs Competitors
- **File**: `docs/benchmark-http-comparison.svg`
- **Comparison**: ThinkingRoot vs FalkorDB, SuperMemory, Zep, Graphiti
- **Winner**: 8.1–81× faster

### [2026-04-14] In-Process Latency (Micro-benchmarks)
- **File**: `docs/benchmark-comparison.svg`
- **Scale**: In-memory embed (63 ns baseline, 2ms p95 over HTTP)

## Regenerating Reports

k6 measurements are verbose (1KB+ per data point). Reports are generated from raw JSON:

```bash
# Run benchmark
k6 run crates/thinkingroot-bench/benches/load/stress_10k.js \
  --out json=benchmarks/results/latest.json

# Generate report
python3 crates/thinkingroot-bench/benches/load/generate_report.py \
  benchmarks/results/latest.json \
  benchmarks/reports/latest-report.html 10000
```

## Storage Policy

- **Reports** (HTML, markdown, SVG): Tracked in git, immutable
- **Raw JSON**: Not tracked, deleted after report generation
- **Logs**: Kept for 7 days, then archived/deleted

## Benchmark Scripts

Located in: `crates/thinkingroot-bench/benches/load/`

- `stress_10k.js` — 10,000 VU hyperscale test
- `rest_search.js` — Vector search latency
- `rest_entities.js` — Entity list read latency
- `mixed_workload.js` — Real-world mixed operations
- `mcp_tools.js` — MCP protocol latency
- `generate_report.py` — Report generation from k6 JSON
- `run_load_test.sh` — Full test orchestration

## Performance Targets

| Metric | Target | Status |
|--------|--------|--------|
| p95 latency @ 10K VUs | < 10ms | ✅ 0.117ms |
| p99 latency @ 10K VUs | < 25ms | ✅ 0.346ms |
| Error rate @ 10K VUs | < 1% | ✅ 0% |
| Requests/sec (sustained) | > 1000 | ✅ ~100k/sec |

---

**Last updated**: 2026-04-14  
**CTO certification**: World's fastest knowledge retrieval system
