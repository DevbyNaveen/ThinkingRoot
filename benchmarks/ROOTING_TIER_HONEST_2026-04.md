# Rooting Tier Distribution — Honest Breakdown (2026-04-23)

Real-world tier distribution produced by running
`root rooting re-run --all` against the LongMemEval-500 compiled
workspace (`longmemeval-workspace/`) — 940 compiled session files
producing **95,584 claims**.

This file is the *write-time gate* measurement the paper cites in
§Evaluation. It is not a read-time accuracy number (that is the
91.2% LongMemEval result in `BENCHMARK_RECORD.md`).

## Raw numbers

| Tier | Count | Share |
|---|---:|---:|
| **Rooted** | 94,374 | 98.73 % |
| **Attested** | 0 | 0.00 % |
| **Quarantined** | 0 | 0.00 % |
| **Rejected** | 1,210 | 1.27 % |
| Total claims | **95,584** | 100.00 % |

Supporting counts (queried from CozoDB directly after the re-run):

| Metric | Value |
|---|---:|
| `trial_verdicts` rows | 95,584 |
| `verification_certificates` rows | 94,374 |
| Claims with a predicate attached | **0** |
| Contradiction records in graph | 1,457 |
| Source rows | 990 |
| Sources with `content_hash` ≠ '' | 990 (100 %) |

Run parameters: release binary `target/release/root rooting re-run --all`,
wall-clock 3 min 42 s, `RootingConfig::default()`
(provenance_threshold = 0.70, contradiction_floor = 0.85,
predicate_strength_threshold = 0.60).

## Why this is the *honest* breakdown

The original paper figure reported 98.6 % "Rooted" on a 7,103-claim
ThinkingRoot workspace. Read in isolation, that number implies the gate
admits claims because it has *verified* them. This re-run on the larger
95,584-claim LongMemEval workspace makes the real picture visible:

**Zero claims carry a predicate.** The LongMemEval compile ran before
predicate extraction was wired into the LLM prompts, so every claim
has `predicate_json = ''`. Consequently, the predicate probe is
*skipped* on every claim; the 98.73 % Rooted figure reflects only the
two fatal probes (provenance + contradiction) plus the temporal-default
(valid_from ≤ now, which is true by construction).

In other words: at the time this workspace was built, Rooting was
operating as a **two-probe safety net**, not a five-probe admission
gate. That is exactly the framing the paper should adopt — "guardrail
not guarantee" — and the B1 strength-scoring work (landed
2026-04-23) is specifically designed so that *new* compiles with
predicates won't inherit this ambiguity.

## What the numbers actually prove

1. **Fatal probes operate correctly at scale.** All 1,210 rejections
   come from the Contradiction probe, every one identifiable in the
   graph as a real contradiction record (there are 1,457 contradictions
   total; rejections cover the subset where the incumbent crosses the
   0.85 confidence floor). Sample `failure_reason`s are uniformly
   `"contradiction failed"`.
2. **Provenance survived upgrade.** 100 % of sources retained their
   `content_hash`, so the provenance probe had full byte-level
   coverage. Zero provenance failures means no source drifted between
   compile and re-run — the workspace is internally consistent with
   the snapshot used for the 91.2 % LongMemEval run.
3. **Certificate budget is real.** 94,374 certificates persisted in
   `verification_certificates`. Each can be re-verified by re-running
   the probes against the stored source bytes + stated predicate —
   reviewers or downstream consumers do *not* have to trust the
   platform, they can re-derive the same BLAKE3 hash.
4. **No silent passes.** The gate admitted 98.73 % *and* wrote a
   verdict row for every admission *and* issued a certificate. The
   1.27 % rejected were recorded as verdicts without certificates,
   which is the correct accounting: an unadmitted claim gets a trial
   record but no attestation.

## Three-line paper framing

> Rooting admitted 98.73 % of claims and rejected 1.27 % on the 95,584-claim
> LongMemEval-500 workspace. **All admissions on this workspace are
> "temporal-rooted" rather than predicate-verified**, because the workspace
> predates predicate extraction; the figure therefore reports the work
> done by the two fatal probes (provenance, contradiction) plus the
> temporal default. New compiles emit predicates via the LLM prompt
> extension landed in this release, and the predicate probe + B1
> strength scoring will split this number into predicate-verified vs.
> temporal-only in future runs.

## What changes after predicate extraction is live

For new workspaces compiled under `root` ≥ v0.1.0-rooting, each
claim will carry an LLM-emitted predicate. Expected distribution on a
typical code pack (extrapolated from unit test coverage; to be
re-measured after the next full compile):

- A minority of claims will have high-strength predicates that match a
  unique call site in source → **Rooted** with predicate evidence
- Most claims will have medium-strength predicates → **Rooted** at
  lower confidence
- Gamed / overly-broad predicates → **Attested** (B1 demotes them,
  proven at 100 % in `BENCHMARK_ROOTING_INJECTION.md` Class D)
- Predicates that fail against current source → **Quarantined**
- Provenance or contradiction failures → **Rejected**

The next full-pack compile will produce the read for this breakdown.
Until then, this file is the authoritative honest reading and should
be cited instead of the old isolated 98.6 % figure.

## Reproducing this file

```
# Backup first (the re-run is idempotent, but paranoia is cheap).
cp longmemeval-workspace/.thinkingroot/graph/graph.db \
   /tmp/graph.db.backup_$(date +%F)

# Build and run.
cargo build --release -p thinkingroot-cli --bin root
./target/release/root rooting re-run --all \
   --path longmemeval-workspace

# Verify:
./target/release/root rooting report --path longmemeval-workspace
```

Supporting CozoDB probe (the counts above):
`/tmp/cozo_probe/src/main.rs` in this session — a 50-line standalone
sqlite-cozo probe that queries the graph directly for predicate
counts, contradiction totals, and source-hash coverage. Bundled into
the repo would require a new small crate; the probe is kept out-of-tree
because the same numbers are reachable via `root rooting report` plus
one-off cozo queries in the MCP layer.
