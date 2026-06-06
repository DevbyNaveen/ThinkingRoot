# Rooting Ship Plan — Production, Evidence, Novelty, Paper

**Date:** 2026-04-23
**Owner:** Naveen (CTO)
**Horizon:** 14 working days → submission-grade artifact
**Status:** Executing (engine code-complete, evidence + paper pending)

---

## 1. Mission

Close the gap between "Rooting works in the repo" and "Rooting is defensible, production-grade, and published." At end of this plan:

- **Engine:** v0.1.0-rooting tagged, benchmarked, migration-safe.
- **Evidence:** ablation + injection studies prove write-quality delta; predicate-strength scoring eliminates the 98.6% artifact.
- **Novelty:** reproducible-search appendix + 20-system prior-art table + arXiv + Zenodo DOI = priority locked.
- **Paper:** honest, defensible, venue-ready for NeurIPS 2026 workshop or equivalent.

Everything below is grounded in the current repo (verified 2026-04-23). No invented infrastructure.

---

## 2. What Actually Exists Right Now

| Asset | Path | State |
|---|---|---|
| Rooting crate (5 probes + cert + byte store) | `crates/thinkingroot-rooting/` | 2,700 LOC, 48 unit + 4 integration tests green |
| Phase 6.5 pipeline integration | `crates/thinkingroot-serve/src/pipeline.rs:676` | Wired, env flag `TR_ROOTING_DISABLED` |
| CLI `root rooting` subcommand | `crates/thinkingroot-cli/src/rooting_cmd.rs` | Present |
| Divan overhead bench | `crates/thinkingroot-bench/benches/macro/rooting_overhead.rs` | Present, not yet run on current build |
| LongMemEval harness | `crates/thinkingroot-cli/src/eval_cmd.rs` | 520 LOC, produced the 91.2% / 456-of-500 result (Round 6, 2026-04-17) |
| LongMemEval compiled workspace | `longmemeval-workspace/` | 940 session files, `.thinkingroot/config.toml` pins Azure gpt-4-1-mini |
| LongMemEval data | `longmemeval-data/longmemeval_s.jsonl` | 500 questions |
| Reflect (Phase 9) engine | `crates/thinkingroot-reflect/` | 946 LOC, pattern decay + cross-workspace aggregation (latest commit `ba4d313`) |
| Paper | `compag-paper/compag.tex` | 14 pages, 238 KB PDF, zero overfull hbox warnings |
| Paper figures generator | `compag-paper/figures/generate.py` | 5 figures, current numbers |
| Paper bib | `compag-paper/compag.bib` | 33 refs across 8 categories |
| Benchmark record of 91.2% run | `benchmarks/BENCHMARK_RECORD.md` | Full per-category breakdown |

Nothing in this plan depends on infrastructure that does not exist.

---

## 3. Success Criteria (What "Done" Means)

An item counts as done only when **all** of its acceptance rows below pass. No partial credit, no "I'll get to it later."

### 3.1 Engine (Workstream A)
- [ ] `cargo bench -p thinkingroot-bench --bench rooting_overhead` runs, total overhead ≤ 10 % of compile time, numbers written to `benchmarks/macro/rooting_overhead_2026-04.md`.
- [ ] Snapshot-restore migration test: copy `longmemeval-workspace/.thinkingroot` → `/tmp/migration-test`, run migration, assert `root health` passes and all claim counts match pre-migration.
- [ ] `root rooting re-run --all` on `longmemeval-workspace` finishes, emits a report, no panics, tier distribution persists across restart.
- [ ] `contribute` in advisory mode logs rejections but does not drop claims; in enforce mode drops; integration test covers both.
- [ ] `CHANGELOG.md` has a `## v0.1.0-rooting — 2026-05-0X` section with migration guide + rollback instructions.

### 3.2 Evidence (Workstream B)
- [ ] **B1 Predicate-strength scoring:** any claim whose predicate coverage score falls below `predicate_strength_threshold` (default 0.6) is demoted from Rooted → Attested. Unit tests cover the three predicate languages. Emitted in `TrialVerdict.predicate_strength: f32`.
- [ ] **B2 Ablation:** `root eval longmemeval --rooting-mode=on|off|advisory` runs end-to-end on `longmemeval-workspace`, writes `benchmarks/BENCHMARK_ROOTING_ABLATION.md` with three columns: accuracy delta, hallucination-rate delta, latency delta.
- [ ] **B3 Injection:** `crates/thinkingroot-rooting/tests/fixtures/injection/` contains ~500 synthetic adversarial claims across 4 classes (fabricated-source, contradictory, stale-span, weak-predicate). A dedicated test binary runs them through the gate, emits per-class rejection rate to `benchmarks/BENCHMARK_ROOTING_INJECTION.md`. Target: ≥ 95 % rejection on fabricated-source, ≥ 80 % on contradictory.
- [ ] **B4 Honest tier distribution:** rerun on `longmemeval-workspace` and real ThinkingRoot workspace after B1, write `benchmarks/ROOTING_TIER_HONEST_2026-04.md` with active-probe vs skipped-probe columns.

### 3.3 Novelty Defense (Workstream C)
- [ ] **C1 Reproducible-search appendix:** `compag-paper/appendix_prior_art_search.md` lists every search query (Google Scholar, arXiv, Semantic Scholar, ACL Anthology, DBLP), top-10 results per query, and a one-sentence reason each result does NOT satisfy the R-column (source-corpus re-execution).
- [ ] **C2 20-system prior-art table:** `compag-paper/tables/prior_art_20.tex` with S/C/V/R/A columns, cited in the paper's Related Work. Current table has 9 systems; needs 11 more, each with a verbatim quote from their paper or README confirming the R-column verdict.
- [ ] **C3 Falsifiable claim:** single-sentence version of the novelty claim present verbatim in both the abstract and §1. Must state: "No prior system performs deterministic re-execution of a derived claim's executable predicate against the original source corpus as a prerequisite for admission."
- [ ] **C4 GitHub release:** tag `v0.1.0-rooting` pushed, release notes attach the CHANGELOG + benchmark files from B2/B3/B4.
- [ ] **C5 Zenodo DOI:** release archived via Zenodo-GitHub integration; DOI cited in paper.
- [ ] **C6 arXiv submission:** paper submitted to arXiv cs.AI + cs.DB; arXiv ID obtained; citation added to README.
- [ ] **C7 Monitoring:** Google Scholar alerts set for three terms: `"source-corpus re-execution"`, `"predicate admission control"`, `"claim re-verification"`. arXiv subject alerts for cs.AI + cs.DB memory/KG papers.

### 3.4 Paper (Workstream D)
- [ ] **D1** "5 probes" language replaced with "2 fatal + 1 central + 2 advisory" throughout.
- [ ] **D2** Abstract + §1 lead with guardrail-not-guarantee framing.
- [ ] **D3** 98.6 % Rooted figure broken into active-probe vs skipped-probe in §Evaluation, with B4 table.
- [ ] **D4** Ablation table (from B2) inserted in §Evaluation.
- [ ] **D5** Injection table (from B3) inserted in §Evaluation, new subsection "Adversarial Robustness."
- [ ] **D6** Reproducible-search appendix (C1) attached.
- [ ] **D7** Related Work: prior-art table expanded to 20 systems (C2).
- [ ] **D8** Venue-specific formatting (NeurIPS 2026 workshop template if target confirmed; otherwise keep current single-column research format).
- [ ] Final PDF regenerates with zero overfull hbox warnings, page count ≤ 18.

---

## 4. Dependency Graph

```
                ┌──────────────┐
                │  A1 bench    │───┐
                └──────────────┘   │
                ┌──────────────┐   │
                │  A2 snapshot │───┤ (parallel, no blockers)
                └──────────────┘   │
                ┌──────────────┐   │
                │  A3 re-run   │───┤
                └──────────────┘   │
                ┌──────────────┐   │
                │  A4 gate test│───┤
                └──────────────┘   │
                                   ▼
B1 (predicate-strength) ──► B4 (honest tiers) ──► D3 ┐
                                                     │
B2 (ablation)         ───────────────────────────► D4 ┤
                                                     │
B3 (injection)        ───────────────────────────► D5 ┤
                                                     ├──► D1/D2/D8 ──► Final PDF
C1 (search appendix)  ───────────────────────────► D6 ┤                   │
                                                     │                   │
C2 (prior-art 20×)    ───────────────────────────► D7 ┘                   │
                                                                          ▼
                            C3 (falsifiable claim) ──► A5 CHANGELOG ──► C4 tag ──► C5 DOI ──► C6 arXiv ──► C7 alerts
```

Critical path: **B1 → B4 → D3 → Final PDF**.
Second-longest: **B2 (ablation) → D4 → Final PDF**.

---

## 5. Fourteen-Day Sprint Calendar

Days are working days. Weekends optional but budget assumes 5-day weeks.

### Day 1 — Mon 2026-04-27
- **Morning:** A1 divan bench run + write `benchmarks/macro/rooting_overhead_2026-04.md`.
- **Afternoon:** B1 start — add `predicate_strength: f32` to `TrialVerdict`, wire through the three predicate engines, default threshold 0.6. Unit tests per language.

### Day 2 — Tue 2026-04-28
- **Morning:** B1 finish — demote Rooted → Attested when strength < threshold. Re-run `cargo test -p thinkingroot-rooting` (must stay green, plus new tests).
- **Afternoon:** A2 migration snapshot test — copy workspace, run migration, verify health.

### Day 3 — Wed 2026-04-29
- **Morning:** A3 `root rooting re-run --all` on `longmemeval-workspace`, capture before/after tier counts in `benchmarks/ROOTING_TIER_HONEST_2026-04.md` (this also produces B4).
- **Afternoon:** A4 advisory-vs-enforce gate integration test.

### Day 4 — Thu 2026-04-30
- **All day:** B2 Ablation. Add `--rooting-mode=on|off|advisory` flag to `eval_cmd.rs`. Run three full LongMemEval-500 passes (≈ 90 min each on Azure gpt-4-1-mini). Write `benchmarks/BENCHMARK_ROOTING_ABLATION.md`.

### Day 5 — Fri 2026-05-01
- **Morning:** B2 analysis + delta commentary.
- **Afternoon:** B3 injection corpus start — implement the 4 attack classes as fixture generators, author ~500 synthetic claims.

### Day 6 — Mon 2026-05-04
- **Morning:** B3 finish — test binary runs corpus through gate, writes `benchmarks/BENCHMARK_ROOTING_INJECTION.md`.
- **Afternoon:** C1 reproducible-search appendix — run all queries, screenshot top-10s, draft one-sentence misses per result.

### Day 7 — Tue 2026-05-05
- **All day:** C1 finish + C2 prior-art table expansion (9 → 20 systems). For each new system: read source paper or README, record S/C/V/R/A verdict with verbatim quote.

### Day 8 — Wed 2026-05-06
- **Morning:** C3 falsifiable claim — tighten wording, paste into abstract + §1.
- **Afternoon:** D1 + D2 — edit paper to "2 fatal + 1 central + 2 advisory" language and guardrail-not-guarantee framing.

### Day 9 — Thu 2026-05-07
- **All day:** D3 + D4 + D5 — insert B2/B3/B4 tables into §Evaluation, rewrite surrounding prose.

### Day 10 — Fri 2026-05-08
- **All day:** D6 + D7 — attach C1 appendix, expand Related Work with C2 table.

### Day 11 — Mon 2026-05-11
- **Morning:** D8 — decide venue (NeurIPS 2026 workshop MemAgents or ICLR 2027 rolling). Apply template.
- **Afternoon:** A5 CHANGELOG + migration guide.

### Day 12 — Tue 2026-05-12
- **Morning:** Final paper pass — read end-to-end, fix typos, regenerate figures, rebuild PDF with tectonic.
- **Afternoon:** C4 GitHub release — tag `v0.1.0-rooting`, attach benchmark files, write release notes.

### Day 13 — Wed 2026-05-13
- **Morning:** C5 Zenodo DOI — enable GitHub-Zenodo integration, trigger archive, grab DOI, add to paper.
- **Afternoon:** C6 arXiv submission prep — cover letter, category selection (cs.AI + cs.DB), co-author confirmation.

### Day 14 — Thu 2026-05-14
- **Morning:** C6 arXiv submit.
- **Afternoon:** C7 alerts — configure Scholar + arXiv keyword alerts. README updated with arXiv + DOI + tag links.

**Day 15 (slack) — Fri 2026-05-15:** buffer for anything that slipped. Use for venue submission if NeurIPS workshop CFP is live.

---

## 6. Risk Register

| # | Risk | Sev | Trigger signal | Mitigation |
|---|---|---|---|---|
| R1 | Ablation shows Rooting delta < 1 pp on LongMemEval | High | B2 table numerically small | Frame Rooting as *write-time* guardrail, not *read-time* accuracy booster; the injection study (B3) becomes primary evidence. LongMemEval is the wrong corpus for write-gate defense — acknowledge this directly. |
| R2 | B3 injection rejection rate on "contradictory" class is low (<50 %) | Med | Contradiction probe depends on prior claims being in graph at trial time | Seed the graph first with true claims, then inject contradictions. Document limitation: contradiction probe is only as strong as prior graph state. |
| R3 | Divan bench shows overhead > 10 % | Low | A1 numbers | Profile with `cargo flamegraph`; most likely fix is parallelizing non-fatal probes via rayon (already in workspace). |
| R4 | Migration 3 corrupts a populated workspace | High | A2 snapshot test fails | Workspace tarball backups before every migration run; `root rooting re-run --all` idempotent so re-running is safe. |
| R5 | Reviewer finds a 21st system we missed | Med | Post-submission feedback | Keep C1 appendix *reproducible* — reviewer can re-run the same searches. Zenodo DOI plus the tag timestamp gives priority even if table is incomplete. |
| R6 | Azure LLM budget runs out mid-ablation | Low | B2 day 4 | ThinkingRoot config supports swapping provider; fall back to OpenAI direct or Anthropic if quota blocks the run. Budget ≈ 3 × 500 questions × ~2 k tokens each ≈ 3 M tokens, ~$6 on gpt-4-1-mini. |
| R7 | Priority scooped mid-sprint | Med | arXiv keyword hit during C7 prep | C4 tag ships day 12 — tag alone is defensible priority. If another group publishes first, pivot framing to "independent rediscovery + strongest engineering" rather than "only." |

---

## 7. Verification Gates

Per-workstream CI checks that must pass before the workstream counts done.

### A. Engine
```bash
# A1
cargo bench -p thinkingroot-bench --bench rooting_overhead
# A2
./scripts/migration_snapshot_test.sh longmemeval-workspace
# A3
./target/release/root rooting re-run --all --path longmemeval-workspace
# A4
cargo test -p thinkingroot-rooting --test contribute_gate
cargo test -p thinkingroot-serve --test rest_test
```

### B. Evidence
```bash
# B1
cargo test -p thinkingroot-rooting predicate_strength
# B2
./target/release/root eval longmemeval --rooting-mode=off  | tee off.txt
./target/release/root eval longmemeval --rooting-mode=on   | tee on.txt
./target/release/root eval longmemeval --rooting-mode=advisory | tee adv.txt
# B3
cargo test -p thinkingroot-rooting --test injection_corpus
# B4
./target/release/root rooting report --path longmemeval-workspace --honest
```

### C. Novelty
```bash
# C4
git tag v0.1.0-rooting && git push origin v0.1.0-rooting
# C5
# Trigger via Zenodo web UI after tag push
# C6
# arxiv.org/submit — requires moderation delay ~1 day
```

### D. Paper
```bash
cd compag-paper
python figures/generate.py
tectonic compag.tex
# expect: 0 overfull hbox, 0 undefined refs, page count ≤ 18
```

---

## 8. Final Deliverables Checklist

At end of Day 14, the following artifacts exist:

### Repo
- [ ] `v0.1.0-rooting` git tag pushed
- [ ] `CHANGELOG.md` with Rooting section + migration guide
- [ ] `benchmarks/macro/rooting_overhead_2026-04.md`
- [ ] `benchmarks/BENCHMARK_ROOTING_ABLATION.md`
- [ ] `benchmarks/BENCHMARK_ROOTING_INJECTION.md`
- [ ] `benchmarks/ROOTING_TIER_HONEST_2026-04.md`
- [ ] `README.md` updated with arXiv + DOI + tag links

### Paper
- [ ] `compag-paper/compag.pdf` final (≤ 18 pages, 0 overfull hbox)
- [ ] `compag-paper/appendix_prior_art_search.md`
- [ ] `compag-paper/tables/prior_art_20.tex`
- [ ] arXiv ID locked
- [ ] Zenodo DOI locked
- [ ] Venue target named (NeurIPS 2026 workshop or ICLR 2027 rolling)

### External
- [ ] arXiv preprint live
- [ ] Zenodo archive live
- [ ] Google Scholar alerts configured for 3 terms
- [ ] arXiv subject alerts configured for cs.AI + cs.DB

---

## 9. Rollback Plan

If anything goes catastrophically wrong:

- **Engine breakage after v0.1.0-rooting tag:** `git tag -d v0.1.0-rooting && git push --delete origin v0.1.0-rooting`. Cut `v0.1.1-rooting` with the fix.
- **Paper pulled after arXiv submission:** arXiv allows withdraw within 24 h of moderation; after that, supersede with v2. Never delete; versions accumulate.
- **Migration 3 corrupts a user workspace:** restore from tarball backup (automatic pre-migration). Re-run migration after fix. `.thinkingroot/graph.db.pre-rooting.bak` always written before first run.
- **Zenodo DOI attached to wrong archive:** Zenodo supports versioning; publish a v2 with corrected file set. Cite v2 in paper, leave v1 as historical record.

---

## 10. Out of Scope (Do Not Touch This Sprint)

- SaaS hub / dashboard (private Phase 4 repo)
- HelloRoot multi-agent bundle (separate phase, not a paper blocker)
- Cross-user / multi-tenant memory
- LLM-in-the-loop verification (Rooting stays deterministic)
- Phase 9 Reflect improvements beyond what shipped at `ba4d313`
- New probe types (P6+) — five is enough for v1
- Any work on CompAG branding as a headline claim

If any of these creep into the sprint, they get deferred to post-submission.

---

## 11. What I Commit To

As CTO on this plan:

1. **No hallucinated numbers.** Every metric in the paper is reproducible from commands in §7. If a number cannot be regenerated on demand, it does not go in the paper.
2. **No silent failures.** Every verification gate in §7 runs in CI or in a scripted runner before its workstream counts as done.
3. **Honest framing.** The paper leads with "guardrail, not guarantee." The 98.6 % figure is broken out into its components. The predicate-gaming limitation is acknowledged in §Limitations.
4. **Priority first, publication second.** Day 12 tag + Day 13 DOI lock priority before the Day 14 arXiv submit. If the sprint slips, the tag still ships on time.
5. **One brand.** All artifacts say "ThinkingRoot" as parent. HelloRoot, if mentioned at all, is a bundled flagship — never a separate product.

---

## 12. First Action (Day 1, Monday 2026-04-27, 09:00)

```bash
cd /Users/naveen/Desktop/thinkingroot
cargo bench -p thinkingroot-bench --bench rooting_overhead 2>&1 | tee benchmarks/macro/rooting_overhead_2026-04.md
```

If overhead ≤ 10 %, mark A1 done and move to B1. If overhead > 10 %, profile immediately — do not proceed.
