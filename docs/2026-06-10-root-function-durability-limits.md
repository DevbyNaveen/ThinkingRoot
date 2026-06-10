# Root Function Durability — Guarantees & Limits (A5)

**Status: normative.** Every claim below is grounded in code (file:line as of
`feat/compile-workspace`, 2026-06-10). This is the published spec of the
journal's limits — the mechanism is sound; this document is what makes its
boundaries honest, to the rigor Convex/Temporal publish for theirs.

## The mechanism (what IS guaranteed)

- Every cognition op inside a run — `ctx.llm.ask`, `ctx.memory.recall`,
  `ctx.memory.remember`, `ctx.branch.fork/merge`, `ctx.prompt`, `ctx.step`,
  `Math.random` — is **journaled**: its result is recorded under a
  deterministic per-run step key (per-op sequence counters,
  `root_function_runtime.rs:1045-1047`), and a **replay of the same run id
  returns the journaled result instead of re-executing the op**
  (`root_function_runtime.rs:827-843`).
- Journal storage is the workspace graph itself: relation
  `root_function_steps {run_id, step_key => result_json, recorded_at}`
  (`graph.rs` schema; write at `root_function.rs:486`). It shares the
  graph's durability (fsync semantics of the underlying SQLite).
- **Graph writes are exactly-once by construction, not by the journal**:
  `ctx.memory.remember` uses a deterministic claim id derived from
  (run id, sequence), so a re-execution re-derives the same id and the
  write is idempotent. The journal makes replays *cheap*; the
  deterministic ids make retries *safe*.
- A **suspended** run (`ctx.cognition.ask`) persists its steps and its
  pending request (`engine.rs:3198-3209`); answering the token resumes the
  run, which replays from the top against the journal — proven by the
  full-surface exactly-once replay test (journaled `Math.random` +
  `remember` + `branch.fork` reproduce with no duplicates).

## The limits (what is NOT guaranteed)

### 1. The journal flushes AFTER the run returns — not per step
`engine.rs:3142` executes the whole run; `engine.rs:3198-3200` persists the
newly recorded steps afterwards. Consequences:

- **Process crash mid-run** (OOM-kill, CRIU failure, power): zero steps of
  that attempt are persisted. A retry with the same run id **re-executes
  every op**, including `ctx.llm.ask` — LLM calls are therefore
  **at-least-once**, not exactly-once, across process crashes. Graph writes
  stay safe (deterministic ids); your LLM bill and latency do not.
- Within a *completed or suspended* attempt, the journal is consistent —
  the flush is atomic per `:put` batch into the graph.

### 2. Journal flush failure is swallowed
`engine.rs:3199` is `let _ = record_function_steps(...)`. If the graph
write fails (disk full, lock poisoned), the run's RESULT is still returned
to the caller, but a later resume of a suspended run will find an **empty
or partial journal** and silently re-execute — including the LLM calls.
This is the sharpest known edge. Mitigation until fixed: treat a suspended
run on a workspace that has reported storage errors as restart-from-scratch.

### 3. No journal size limits exist today
- **Step count**: unbounded. A loop calling `ctx.llm.ask` 10,000 times
  journals 10,000 rows.
- **Step size**: `result_json` is unbounded — a recall over a large corpus
  or a long LLM answer is stored verbatim.
- **Practical bound**: the 30-second wall-clock timeout per execution
  (`engine.rs` invoke passes `30` to `run_js_journaled`) caps how much one
  attempt can journal, but a suspend/resume chain accumulates without
  limit across attempts.
- Recommendation (not yet enforced): treat >1,000 steps or >1 MB of
  `result_json` per run as a design smell; an enforced cap is future work
  and will be a hard error, not silent truncation.

### 4. Mid-run code-version semantics are UNDEFINED on redeploy
`invoke_function` always loads the **latest** version of the function
(`engine.rs:3042-3045`); `RootFunctionRun` does not record the version the
run started under. If a function is **redeployed while one of its runs is
suspended**, the resume replays the OLD attempt's journal against the NEW
body. Step keys are sequence-based — a body that reorders, adds, or
removes cognition ops will silently consume journaled results at the
wrong call sites. **Do not redeploy a function with suspended runs** unless
the new body's op sequence is a strict suffix-compatible extension.
Pinning the version per run (and refusing resume on mismatch) is the
planned fix; until it lands, this is an operator contract.

### 5. Execution timeout is fixed
30 seconds per execution attempt, set at the invoke site — not currently
configurable per function. A suspend/resume cycle resets the clock (each
resume is a fresh attempt).

### 6. Capability grants are loaded per attempt, not pinned per run
The stored CapSet (A1) is read at each invoke/resume (`engine.rs`, invoke
caps block). Narrowing a function's grants while one of its runs is
suspended means the RESUME runs under the NEW, narrower grants — a
journaled op that was allowed in attempt 1 may be denied in attempt 2.
This is fail-closed (security wins over replay fidelity) and intentional.

## Honest claims this doc licenses

| Say | Never say |
|---|---|
| "Durable exactly-once **graph effects** across retries (deterministic ids + journal)" | "exactly-once everything" |
| "LLM calls are journaled — replays and resumes never re-bill" | "LLM calls can never run twice" (crash-mid-run re-executes) |
| "Suspended runs survive restarts and resume from the journal" | "any crash anywhere loses nothing" |
| "Journal mechanism at par with Temporal/Restate/Convex; placement (inside the cognition graph) unique" | superiority claims on the journal mechanism itself |
