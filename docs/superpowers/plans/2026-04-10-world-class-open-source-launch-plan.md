# ThinkingRoot World-Class Open-Source Launch Plan

**Date:** 2026-04-10 (updated 2026-04-10 with open core decision)
**Status:** Proposed

---

## 0. Open Core Decision (Added 2026-04-10)

**Decision: ThinkingRoot is open core.** This decision was made after deep research into federated knowledge graph architectures and competitive analysis.

**What this means for this launch plan:**

```
Phase 1 + Phase 2 engine  →  MIT open source, forever free
Single workspace features →  MIT open source, forever free
Federated workspaces      →  Proprietary (Phase 3 of build sequence)
Bridge Graph              →  Proprietary
Multi-workspace queries   →  Proprietary
Continuous cloud          →  Proprietary
Enterprise features       →  Proprietary
```

**What this does NOT change about this plan:**

- All Phase 1 hardening tracks (P1-A through P1-F) are for the open-source engine. No change.
- All Phase 2 hardening tracks (P2-A through P2-E) are for the open-source engine. No change.
- All launch gates remain the same — they gate the open-source launch.
- "Phase 3+" in this plan refers to features that **stay open source** (safety engine, TypeScript SDK, VS Code extension). These are deliberately not behind the proprietary wall.

**What is now explicitly out of scope for the open-source launch:**

The federated workspace architecture (Bridge Graph, three-tier entity resolution cascade, federated query planner, cross-workspace community detection, sensitivity propagation) is the proprietary moat. It is described in full in `docs/2026-04-08-engram-knowledge-compiler-design.md` Section 9, but it is NOT part of the open-source launch tracks. It is a separate product track that begins after the open-source launch.

**Open core boundary principle:**
> Single workspace (local) → Open Source, MIT, forever free  
> Multiple workspaces (linked) → Proprietary, paid

This is the same model as HashiCorp Terraform (local plan/apply open; remote state management paid) and Neo4j (embedded open; enterprise clustering paid).

---

## 1. What The Existing Plan Already Says

After reviewing:

- [design doc](/Users/naveen/Desktop/thinkingroot/docs/2026-04-08-engram-knowledge-compiler-design.md)
- [Phase 2 design spec](/Users/naveen/Desktop/thinkingroot/docs/superpowers/specs/2026-04-09-phase2-serve-sdk-design.md)
- [Phase 2 implementation plan](/Users/naveen/Desktop/thinkingroot/docs/superpowers/plans/2026-04-09-phase2-serve-sdk.md)
- [README roadmap](/Users/naveen/Desktop/thinkingroot/README.md)
- [changelog](/Users/naveen/Desktop/thinkingroot/CHANGELOG.md)

the intended sequencing is clear:

- **Phase 1** = core engine milestone: `root ./repo` works end-to-end.
- **Phase 2** = serve + SDK milestone: full open-source release on GitHub.
- **Phase 3** = ecosystem + safety milestone: public launch, GitHub Action, VS Code extension, safety engine.

## 2. Core Insight

The repo is already strong enough for an **alpha open-source release**, but it is not yet at the quality bar implied by:

- "incremental compilation" as a core architectural property
- "full open-source release" for Phase 2
- a "public launch" that will survive Hacker News / Product Hunt scrutiny

So the right plan is:

1. **Finish the real contract of Phase 1**
2. **Finish the real contract of Phase 2**
3. **Pull a thin launch slice from Phase 3**

Do **not** block open source on the full Phase 3 safety engine, TypeScript SDK, or VS Code extension. Those remain Phase 3. But do block the public launch on correctness, parity, docs honesty, benchmarks, and install quality.

---

## 3. Current Gap Map

### Phase 1 gaps

1. **Incremental compilation is not yet graph-correct**
   - Recompiles can accumulate duplicate sources, claims, and entities across runs.
   - Entity resolution currently happens inside the current extraction batch, not against the persisted graph.

2. **Stable identity is not strong enough**
   - Source identity is regenerated during parse instead of being anchored to stable document identity + workspace.

3. **Extraction throughput is below the intended bar**
   - Config exposes concurrency, but extraction is still mostly serial in practice.

4. **Compiled artifacts are partially scaffolded**
   - Some reports use placeholders or weak lookup paths instead of graph-accurate content.

5. **Verification is simpler than product language**
   - Staleness, contradictions, orphan checks, and supersession warnings exist.
   - Poisoning, leakage, and stronger provenance/coverage enforcement are not yet first-class.

6. **Phase 1 source scope is narrower than the design doc**
   - Codebase supports markdown/code/pdf/git commits.
   - Design language also mentions web pages, Slack/chat, GitHub issues/PRs.

### Phase 2 gaps

1. **REST compile endpoint is stubbed**
2. **MCP compile tool is stubbed**
3. **Python native compile path does not fully match the CLI pipeline**
4. **Python graph API is incomplete**
   - `get_sources()` and contradiction access are not true graph queries yet.
5. **Test coverage is not yet at Phase 2 spec level**
   - The spec called for REST E2E, MCP stdio/SSE E2E, multi-workspace, Python native, Python client, and auth coverage.

### Launch gaps

1. **Docs overstate completeness**
   - README and roadmap say Phase 1 and Phase 2 are complete.
2. **README/design drift exists**
   - Storage implementation and some capabilities differ from older planning language.
3. **No published benchmark proof yet**
4. **No world-class demo path yet**
   - Fresh install, compile fixture, inspect artifacts, query via REST/MCP/Python should be a polished single flow.
5. **Packaging and release confidence need hardening**
   - `cargo install`, Python wheel flow, feature-flag builds, and smoke tests should all be validated in CI.

---

## 4. Plan Structure

The plan is split into:

- **Phase 1 Hardening**: make the compiler engine trustworthy
- **Phase 2 Hardening**: make every interface truthful and production-grade
- **Launch Slice**: docs, benchmarks, packaging, demo, CI, release discipline

---

## 5. Phase 1 Hardening

### Track P1-A: Deterministic Source Identity

**Goal:** the same document in the same workspace keeps the same logical identity across runs.

**Deliverables**

- Introduce a stable source key derived from workspace + canonical URI.
- Separate:
  - logical source identity
  - content hash version
  - compile-run identity
- Ensure parsers populate canonical relative path metadata consistently.

**Acceptance**

- Recompiling an unchanged repo 10 times does not change source, claim, or entity counts.
- Renaming a file is treated as a source move, not silent duplication.

### Track P1-B: True Incremental Graph Mutation

**Goal:** changed files update the graph instead of piling onto it.

**Deliverables**

- Add graph operations to:
  - fetch source by stable key
  - delete or replace claims/edges derived from one source
  - rebuild only affected embeddings and artifacts
- Make the pipeline:
  - detect unchanged sources
  - replace changed-source derived graph state
  - remove deleted-source derived graph state

**Acceptance**

- Counts remain stable across repeated runs.
- Editing one file only changes claims/entities/relations tied to that file.
- Deleting a source removes its downstream claims and edges.

### Track P1-C: Cross-Run Entity Resolution

**Goal:** new extractions merge against the existing graph, not just the current batch.

**Deliverables**

- Resolve new entities against persisted canonical names and aliases.
- Add conflict-safe alias persistence and dedupe rules.
- Add tests for:
  - same entity across multiple compiles
  - same entity across renamed files
  - fuzzy matches over time

**Acceptance**

- No entity count drift when recompiling equivalent content.
- Alias graph remains stable after repeated runs.

### Track P1-D: Artifact Fidelity

**Goal:** artifacts are trustworthy enough to be the product demo surface.

**Deliverables**

- Fix contradiction report to dereference claim IDs correctly.
- Populate entity pages with real aliases and relations.
- Populate architecture map decisions from graph queries.
- Add artifact-level tests against fixture outputs.

**Acceptance**

- Every artifact contains valid graph-backed content, not placeholders.
- Fixture snapshots are stable and human-legible.

### Track P1-E: Verification Fidelity

**Goal:** either implement the promised checks or narrow public claims.

**Deliverables**

- Upgrade provenance to check actual source linkage quality, not just counts.
- Implement real coverage warnings for low-evidence entities.
- Decide explicitly:
  - implement poisoning/leakage scaffolding now, or
  - move them out of Phase 1/2 public claims until Phase 3

**Acceptance**

- Health sub-scores are explainable from graph state.
- README and CLI wording match actual checks.

### Track P1-F: Performance and Benchmarks

**Goal:** meet the "compiler" bar in speed and developer feel.

**Deliverables**

- Make extraction truly concurrent.
- Add benchmark corpus:
  - small repo
  - medium monorepo slice
  - mixed docs + code fixture
- Measure:
  - first compile latency
  - incremental latency
  - artifact generation latency
  - search latency

**Acceptance**

- Benchmark script lands in repo.
- Public numbers are only published once reproducible.

---

## 6. Phase 2 Hardening

### Track P2-A: QueryEngine Pipeline Parity

**Goal:** the shared query engine fully satisfies the Phase 2 spec.

**Deliverables**

- Add `QueryEngine::compile(ws)` that runs the real pipeline.
- Reuse the same pipeline entry point used by CLI compile.
- Remove duplicate pipeline logic across interfaces where possible.

**Acceptance**

- CLI, REST, MCP, and Python native compile all return equivalent pipeline results.

### Track P2-B: REST and MCP Contract Completion

**Goal:** every advertised public interface works.

**Deliverables**

- Implement REST `POST /api/v1/ws/{ws}/compile`.
- Implement MCP `compile`.
- Confirm auth, error codes, and response envelopes match the spec.

**Acceptance**

- No advertised endpoint or tool returns `NOT_IMPLEMENTED`.

### Track P2-C: Python Native Parity

**Goal:** `thinkingroot.compile()` is a first-class interface, not a partial fork.

**Deliverables**

- Route Python compile through the same pipeline path as CLI/REST/MCP.
- Implement real `get_sources()` and graph-backed contradiction access.
- Validate native API output shapes against the Phase 2 spec.

**Acceptance**

- Python native compile produces the same graph integrity and artifacts as CLI compile.

### Track P2-D: Search and Workspace Quality

**Goal:** search and mounted-workspace behavior feel reliable in demos and real use.

**Deliverables**

- Add tests for:
  - vector-disabled builds
  - multi-workspace isolation
  - entity-name alias lookups
  - search ranking and fallback behavior

**Acceptance**

- Search behaves consistently with and without vector features enabled.

### Track P2-E: Full Phase 2 Test Matrix

**Goal:** achieve the testing strategy promised in the Phase 2 design spec.

**Deliverables**

- REST E2E
- MCP stdio E2E
- MCP SSE E2E
- multi-workspace tests
- auth matrix
- Python native tests
- Python client tests

**Acceptance**

- A clean CI matrix covers all declared Phase 2 interfaces.

---

## 7. Launch Slice For A World-Class Public Release

This is the minimum launch work to add on top of hardened Phase 1 and Phase 2.

### Launch-L1: Docs Honesty Pass

**Deliverables**

- Update README roadmap status from "complete" to launch-accurate wording until all gates pass.
- Align docs with actual storage implementation and supported sources.
- Split "shipped now" vs "planned next" clearly.

### Launch-L2: Golden Demo Path

**Deliverables**

- Add one polished demo fixture repo.
- Add one scriptable demo flow:
  - compile
  - inspect artifacts
  - query REST
  - query MCP
  - query Python
- Record expected outputs in docs.

### Launch-L3: Install and Release Confidence

**Deliverables**

- Smoke-test:
  - `cargo install`
  - `cargo build --no-default-features`
  - `maturin build`
  - Python import + query
- Add release checklist for crates.io/PyPI/GitHub release.

### Launch-L4: Benchmarks and Proof

**Deliverables**

- Publish benchmark methodology before publishing aggressive claims.
- Add one honest comparison page:
  - what ThinkingRoot does now
  - what is intentionally Phase 3+

### Launch-L5: Contribution and Trust Signals

**Deliverables**

- Tighten contributing guide
- Add issue labels/templates if missing
- Add architecture diagrams
- Add known limitations section
- Add release notes with "alpha" or "beta" framing

---

## 8. What To Defer

### Open-source Phase 3 (after open-source launch, still MIT):
- Full safety engine
- Quarantine pipeline
- Belief revision engine
- TypeScript SDK
- VS Code extension

These ship as open source, but they don't block the Phase 1+2 open-source launch.

### Proprietary Phase 3+ (federated workspaces — NOT in open-source launch):
- **Bridge Graph** (cross-workspace edge database)
- **Three-tier entity resolution cascade** (registry → structural → semantic)
- **Federated query planner** (workspace algebra, parallel sub-queries)
- **Cross-workspace community detection** (Leiden algorithm across workspace boundaries)
- **Cross-workspace contradiction detection**
- **Sensitivity propagation across bridge edges**
- Deep connector ecosystem (Slack, Jira, Linear, Confluence)
- Continuous background compilation (cloud-only)

These are the **proprietary moat**. They represent months of infrastructure work that no competitor currently has. They should not appear in open-source launch marketing — they are a separate product track that begins after the open-source launch is proven.

**Why federated workspaces are proprietary, not open source:**
- They require infrastructure (Bridge Graph registry, cloud credentials management, hosted workspace graphs) that cannot work purely locally
- They represent the primary revenue driver for the Team and Enterprise tiers
- The open-source single-workspace engine is complete and valuable on its own
- Making the federation layer proprietary creates a natural and defensible upgrade path

---

## 9. Recommended Sequencing

### Sprint 1: Correctness Foundation

- deterministic source identity
- incremental graph mutation
- cross-run entity resolution
- repeated-compile stability tests

### Sprint 2: Artifact and Verifier Trust

- contradiction report fixes
- architecture/entity artifact fidelity
- verifier score honesty and missing checks decision
- benchmark harness scaffolding

### Sprint 3: Phase 2 Parity

- QueryEngine compile
- REST compile
- MCP compile
- Python native parity
- missing Python graph methods

### Sprint 4: Test Matrix and Launch Packaging

- REST/MCP/Python E2E
- install smoke tests
- docs honesty pass
- demo repo + benchmark publication
- release checklist

---

## 10. Launch Gates

Do not call the launch "world-class" until all of these are true:

1. Repeated compile of the same repo is count-stable.
2. Incremental compile only changes affected graph state.
3. No public Phase 2 endpoint/tool is stubbed.
4. Python native compile matches CLI compile behavior.
5. Artifact outputs are graph-correct and citation-backed.
6. README claims match shipped behavior exactly.
7. CI covers REST, MCP, Python native, Python client, and multi-workspace flows.
8. Install flows are smoke-tested for Rust and Python users.
9. Benchmarks are reproducible and published honestly.
10. The demo path works from a fresh machine with minimal setup.

---

## 11. Recommended Release Positioning

### Before all gates pass

Release as:

- **Open-source alpha (MIT)**
- "core compiler and serve stack are available now"
- "single workspace, local, zero infrastructure"
- "some interfaces and verification logic are still being hardened"

### After all gates pass

Release as:

- **Public beta** or **v0.2 launch (MIT)**
- strong enough for broad developer adoption
- honest about Phase 3 safety and ecosystem work still in progress
- clear about open core boundary: "single workspace = free forever; federated workspaces = coming paid feature"

### Do not claim yet

- fully safe multi-agent write access (Phase 4 safety engine)
- full contradiction resolution workflow (paid platform)
- complete source connector coverage (paid platform)
- federated workspaces across projects (proprietary, in development)
- verified benchmark advantages unless published

### Open core framing for all marketing

Every piece of launch communication should include:

> "ThinkingRoot is open core. The single-workspace compiler is MIT — free forever for personal and team use on one project. Multi-project federation with the Bridge Graph is a paid feature for teams with multiple codebases."

This sets expectations correctly and prevents "bait and switch" perception when federated workspaces ship as paid.

---

## 12. First 10 Concrete Tasks

1. Add stable source identity model and source lookup by canonical URI.
2. Add graph deletion/replacement for all claims and edges derived from one source.
3. Resolve entities against persisted graph state, not just the current batch.
4. Add repeated-compile regression tests to prove no duplicate drift.
5. Fix contradiction report to resolve claims by claim ID correctly.
6. Implement `QueryEngine::compile(ws)` by reusing the CLI pipeline.
7. Wire REST `compile` and MCP `compile` to the shared pipeline.
8. Route Python native compile through the shared pipeline and implement `get_sources()`.
9. Build the full Phase 2 E2E test matrix from the design spec.
10. Update README, roadmap, and changelog to match the actual shipped state until launch gates are passed.

---

## 13. Final Recommendation

Do **not** rewrite the roadmap.

Your roadmap is directionally right.

What is needed now is a **Phase 1 + Phase 2 hardening pass** that makes the shipped repository truly satisfy the promises already written in the design docs. Once that is done, add a **thin launch slice** from Phase 3 for distribution, proof, and trust.

That path preserves your original plan and gets you to a launch that feels disciplined, credible, and world-class.

### Open core addition (2026-04-10)

The open core decision adds **one new track** to the roadmap that runs in parallel with and after the open-source launch:

```
Open-source tracks (MIT):
  P1-A → P1-F    [Phase 1 hardening]
  P2-A → P2-E    [Phase 2 hardening]
  L1 → L5        [Launch slice]
  Phase 3 OSS    [Safety engine, TypeScript SDK, VS Code extension]

Proprietary track (post-launch):
  Fed-1          [Registry Layer + Tier 1 entity resolution]
  Fed-2          [Tier 2 structural resolution via import analysis]
  Fed-3          [Bridge Graph schema + CozoDB instance]
  Fed-4          [Tier 3 semantic resolution - DITTO]
  Fed-5          [Federated query planner]
  Fed-6          [Sensitivity propagation]
  Fed-7          [Leiden community detection across workspaces]
  Cloud-1        [Platform API, auth, billing]
  Cloud-2        [Dashboard, contradiction UI]
  Cloud-3        [Source connectors, continuous compilation]
```

The proprietary track does not begin until the open-source launch gates are all passed. Ship the open-source product first. Build the proprietary moat second. This sequence is right.
