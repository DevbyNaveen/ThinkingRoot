# Integrity Snapshot — Operator Runbook (A7-SECURITY ⑥)

Rollback-to-known-good for poison or corruption discovered *after* it entered
the main graph. Snapshots are periodic, validated copies of `graph.db`;
restore swaps a chosen one back into place.

## What exists

- **Capture** (`maintenance::integrity_snapshot_once`, scheduled by
  `spawn_integrity_snapshots`): copies `graph.db` →
  `.thinkingroot/graph/integrity/graph.db.integrity-{millis}`, validates it
  structurally, prunes to `TR_INTEGRITY_SNAPSHOT_RETAIN`.
  - Enable: `TR_INTEGRITY_SNAPSHOTS=1`
  - Cadence: `TR_INTEGRITY_SNAPSHOT_SECS` (default 21600 = 6h)
  - Retention: `TR_INTEGRITY_SNAPSHOT_RETAIN` (default 7)
- **List** (`maintenance::list_integrity_snapshots`): pristine snapshots,
  newest-first, with their capture timestamp.
- **Restore** (`maintenance::restore_integrity_snapshot`): validate chosen
  snapshot → back up current `graph.db` as `graph.db.pre-restore-{millis}`
  (so a *wrong* rollback is itself reversible) → atomic swap.

## Restore procedure (OFFLINE — this is the hard rule)

The engine holds `graph.db` open while serving. Swapping it underneath a live
daemon corrupts in-flight reads. **Restore only with the engine stopped for
that workspace.**

Cloud (per-project container):
1. **Stop the engine for the project.** Provisioner: suspend/stop the
   container (do NOT just idle-GC — confirm the process is down).
2. Identify the snapshot. From the data volume:
   `ls -t .thinkingroot/graph/integrity/graph.db.integrity-*`
   The trailing number is unix-millis; pick the newest one *before* the
   poison entered (cross-reference the `retrieval_usage` / provenance log
   for when the bad claims appeared).
3. Restore — call `restore_integrity_snapshot(workspace_root, snapshot)`
   (engine library; exposed to ops via a maintenance binary / `az run-command`
   invoking it, NOT a live REST route — there is intentionally no
   restore-over-HTTP path, because that would run against a live engine).
4. **Recompile** to rebuild the vector index against the restored graph:
   `root compile` (vectors are not snapshotted — they are derivable).
5. Restart the engine; verify recall of a known-good fact and absence of the
   poisoned claim.

Self-hosted / desktop (`root serve`): same, with the OS process stopped:
1. `Ctrl-C` / stop `root serve`.
2–5 as above, then restart `root serve`.

## If the restore was wrong

The pre-restore backup is reversible: copy
`graph.db.pre-restore-{millis}` back over `graph.db` (engine stopped),
recompile, restart.

## What restore does NOT do

- Does not stop the engine for you (operational gate, by design).
- Does not restore vectors (recompile rebuilds them).
- Does not de-poison surgically — for a *single known* bad session, prefer
  provenance-scoped removal (`mcp://agent/{session}` + turn calendar) over a
  full rollback; snapshots are the blunt instrument for damage discovered too
  late or too diffuse to excise.

## Honest limits

- Structural validation is SQLite-header + page-size/file-size consistency
  (catches torn copies), not a full page-level integrity audit.
- Snapshot cadence bounds the worst-case data loss on rollback to one
  interval (default 6h): a restore discards everything written since the
  chosen snapshot. Pick cadence against your tolerance.
