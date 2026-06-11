# Root Function durability — guarantees & limits (A5)

*Spec §11 A5. The journal mechanism is sound; this is the published SPEC of its
limits, at the rigor Convex/Temporal document theirs. Grounded in code
(`thinkingroot-graph/src/root_function.rs`, `thinkingroot-serve/src/engine.rs`
`run_function_with_id_opts` / `answer_cognition`, `root_function_runtime.rs`).*

## The model

A Root Function run is **durably journaled, exactly-once on its effects**. Each
journaled operation (`ctx.step`, `ctx.memory.remember`, `ctx.branch.fork/merge`,
`ctx.llm.ask`, `Math.random`, `fetch`, `ctx.cognition.ask`) records one row in
the cozo relation `root_function_steps {run_id, step_key => result_json}`.

- **Replay determinism.** On a resumed run, `list_steps_for_run` preloads the
  journal; a step whose `step_key` is already recorded returns the journaled
  value instead of re-executing. So `Math.random`, `llm.ask`, and `fetch`
  reproduce their original results on replay (no re-call, no divergence).
- **Idempotent writes.** `record_function_steps` is a per-row `:put` keyed on
  `(run_id, step_key)` — re-recording a key is a no-op. `ctx.memory.remember`
  derives a **deterministic claim id** (`blake3(run_id|seq|statement)`), and
  `insert_claim`/vector `upsert` are `:put`/upsert — so a replayed `remember`
  re-writes the SAME id (a no-op), never a duplicate. Branch fork is idempotent
  on the branch name. Net: **exactly-once effect** even across replays.

## Guarantees

| Property | Guarantee |
|---|---|
| Effect semantics | Exactly-once for `remember` / `branch` (deterministic id / natural key). |
| Non-determinism | Captured & replayed (`random`, `llm.ask`, `fetch`, `cognition`). |
| Crash mid-run | Safe to retry with the same `run_id`: journaled steps short-circuit; un-journaled deterministic effects re-derive to the same ids (no dup). |
| Crash mid-journal-write | A `:put` is atomic per row; a partially-written batch loses only the unwritten rows, which replay re-derives. |
| Suspend/resume | `ctx.cognition.ask` suspends to a `pending_request`; answering replays the run from the top, journal-short-circuiting to the answer. |

## Limits (honest)

1. **No enforced journal size cap.** `result_json` and the per-run step count are
   unbounded today. A step that returns a large blob (e.g. a big `fetch` body)
   is journaled verbatim and reloaded on every replay. **Recommendation:** keep
   step results small (ids/scalars, not megabytes); summarise large payloads
   before returning them from a `ctx.step`. *Planned:* a configurable
   `TR_FN_MAX_STEP_BYTES` / `TR_FN_MAX_STEPS` guard that fails the step loudly
   rather than silently bloating the journal.

2. **Mid-run code-version semantics = resume-against-latest.** The initial run
   pins the function version it started on (`FnCtxMeta.version = func.version`).
   But **resume** (`answer_cognition`) re-resolves `get_function` → the *latest*
   deployed body and replays the journal against it. So if the function is
   redeployed between suspend and resume, the resumed run executes the NEW body,
   reusing journaled step results wherever `step_key`s still match. This differs
   from Temporal's per-run version pinning. **Implication:** treat a function
   body as append-compatible across a suspend window — don't reorder/rename the
   `ctx.step` keys of a function that may have in-flight suspended runs. *Planned:*
   pin the resume to the original `(name, version)` for strict replay.

3. **Journal is per-workspace, not cross-workspace transactional.** Effects on
   the workspace graph + the step journal are separate cozo writes; there is no
   single ACID transaction spanning "effect + its journal row". The idempotent
   ids (limit-1 mechanics above) are what make this safe under retry, not a 2PC.

4. **`run_id` reuse is the retry contract.** Exactly-once holds only when a retry
   reuses the SAME `run_id`. A fresh `run_id` (the default `invoke_function`)
   is a NEW run with its own journal — at-least-once across distinct ids is the
   caller's responsibility (use a stable `run_id` for idempotent retries).

5. **Capability/branch scope is fixed at invoke.** `CapSet` and (A2)
   `target_branch` are captured when the run starts; changing a function's
   grants mid-run does not affect the in-flight run.

## What this is NOT

Not a general workflow engine with versioned-replay guarantees across arbitrary
code edits (Temporal/Restate own that). It IS a durable, exactly-once execution
layer for *cognition operations co-located in the memory* — the placement
(journaling `recall`/`remember`/`fork`) is the differentiator, the journal
mechanism is at-par, and these limits are the honest edges of it.
