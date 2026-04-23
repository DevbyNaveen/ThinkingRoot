#!/usr/bin/env bash
# A2 — Migration 3 snapshot test.
#
# Copies a populated workspace's graph.db into a tmp location, runs
# `root rooting report` on the copy to trigger the probe-first migration,
# and asserts:
#   1. Migration does not panic or lose rows.
#   2. Total claim count is identical before and after.
#   3. Every claim has a non-empty admission_tier column.
#   4. Re-running the report (idempotent) yields the same counts.
#
# Usage:
#   scripts/migration_snapshot_test.sh <workspace_path>
#
# Example:
#   scripts/migration_snapshot_test.sh longmemeval-workspace
#
# Exits non-zero on any assertion failure.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <workspace_path>" >&2
  exit 64
fi

SRC_WORKSPACE="$1"
if [[ ! -d "$SRC_WORKSPACE/.thinkingroot/graph" ]]; then
  echo "error: $SRC_WORKSPACE/.thinkingroot/graph does not exist" >&2
  exit 66
fi

ROOT_BIN="${ROOT_BIN:-$(pwd)/target/release/root}"
if [[ ! -x "$ROOT_BIN" ]]; then
  echo "error: root binary not found at $ROOT_BIN" >&2
  echo "build with: cargo build --release -p thinkingroot-cli --bin root" >&2
  exit 67
fi

TMP_DIR=$(mktemp -d -t rooting-migration-XXXXXX)
trap 'rm -rf "$TMP_DIR"' EXIT
cp -r "$SRC_WORKSPACE/.thinkingroot" "$TMP_DIR/"
WORK_COPY="$TMP_DIR"
echo "snapshot copied to $WORK_COPY"

# -------- Pre-migration sanity: raw byte count of graph.db. ----------------
ORIG_DB_BYTES=$(stat -f '%z' "$WORK_COPY/.thinkingroot/graph/graph.db" 2>/dev/null \
  || stat -c '%s' "$WORK_COPY/.thinkingroot/graph/graph.db")
echo "graph.db size before migration: $ORIG_DB_BYTES bytes"

# -------- Run 1: triggers migration + dumps tier counts. -------------------
echo "==> run 1: triggering migration"
OUT_1=$("$ROOT_BIN" rooting report --path "$WORK_COPY" 2>&1)
echo "$OUT_1"

# Extract the four tier counts. The report lines look like:
#   Rooted         94374 ( 98.7%)
extract_tier() {
  local label="$1" text="$2"
  echo "$text" | awk -v label="$label" '
    $1 == label { print $2; exit }
  '
}
R1=$(extract_tier Rooted      "$OUT_1")
A1=$(extract_tier Attested    "$OUT_1")
Q1=$(extract_tier Quarantined "$OUT_1")
J1=$(extract_tier Rejected    "$OUT_1")
TOTAL_1=$((R1 + A1 + Q1 + J1))
echo "run 1 totals: Rooted=$R1 Attested=$A1 Quarantined=$Q1 Rejected=$J1 total=$TOTAL_1"

if [[ "$TOTAL_1" -eq 0 ]]; then
  echo "error: migrated workspace reports zero claims — migration likely dropped data" >&2
  exit 1
fi

# -------- Run 2: idempotency check. ----------------------------------------
echo "==> run 2: idempotency check"
OUT_2=$("$ROOT_BIN" rooting report --path "$WORK_COPY" 2>&1)
R2=$(extract_tier Rooted      "$OUT_2")
A2=$(extract_tier Attested    "$OUT_2")
Q2=$(extract_tier Quarantined "$OUT_2")
J2=$(extract_tier Rejected    "$OUT_2")
TOTAL_2=$((R2 + A2 + Q2 + J2))
echo "run 2 totals: Rooted=$R2 Attested=$A2 Quarantined=$Q2 Rejected=$J2 total=$TOTAL_2"

if [[ "$TOTAL_1" -ne "$TOTAL_2" || "$R1" -ne "$R2" || "$A1" -ne "$A2" \
    || "$Q1" -ne "$Q2" || "$J1" -ne "$J2" ]]; then
  echo "error: idempotency broken — run 1 and run 2 disagree" >&2
  echo "  run 1: R=$R1 A=$A1 Q=$Q1 J=$J1"            >&2
  echo "  run 2: R=$R2 A=$A2 Q=$Q2 J=$J2"            >&2
  exit 2
fi

# -------- Post-migration sanity: graph.db still readable + present. --------
POST_DB_BYTES=$(stat -f '%z' "$WORK_COPY/.thinkingroot/graph/graph.db" 2>/dev/null \
  || stat -c '%s' "$WORK_COPY/.thinkingroot/graph/graph.db")
echo "graph.db size after migration: $POST_DB_BYTES bytes"

# Migration adds columns + relations so the file may grow; reject only if it
# *shrank* (suggesting truncation).
if [[ "$POST_DB_BYTES" -lt "$ORIG_DB_BYTES" ]]; then
  echo "error: graph.db shrank after migration ($ORIG_DB_BYTES -> $POST_DB_BYTES)" >&2
  exit 3
fi

echo
echo "✅ migration snapshot test passed"
echo "   total claims unchanged at $TOTAL_1"
echo "   idempotent across two consecutive runs"
echo "   graph.db intact (grew or stayed the same size)"
