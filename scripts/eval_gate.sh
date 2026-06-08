#!/usr/bin/env bash
# E5 eval gate — the recall guardrail for the ThinkingRoot Compile plan.
#
# Runs LongMemEval-500 against a compiled workspace and FAILS (exit 1) if
# accuracy drops below the locked baseline (91.2% = 456/500), and — when a
# paraphrase fixture is supplied — if the 4-case paraphrase set is not 4/4.
#
# This is the gate that any recall-perturbing change (E5 int8 quantization,
# E6 content-hash embedding cache) MUST pass before it ships. It runs on the
# Azure VM where the gte-modernbert embedder + the LLM judge are staged; it
# cannot run on a dev box without the staged model bundle.
#
# Usage:
#   ROOT_BIN=./target/release/root \
#   LONGMEMEVAL_DATASET=/data/longmemeval_500.jsonl \
#   WS_PATH=/var/lib/tr/eval-ws \
#   [JUDGE_DEPLOYMENT=gpt-4o] \
#   [PARAPHRASE_DATASET=/data/paraphrase_4.jsonl] \
#   [THRESHOLD=91.2] \
#   scripts/eval_gate.sh
set -euo pipefail

ROOT_BIN="${ROOT_BIN:-root}"
WS_PATH="${WS_PATH:-.}"
THRESHOLD="${THRESHOLD:-91.2}"
LONGMEMEVAL_DATASET="${LONGMEMEVAL_DATASET:-}"
PARAPHRASE_DATASET="${PARAPHRASE_DATASET:-}"
JUDGE_ARG=()
if [[ -n "${JUDGE_DEPLOYMENT:-}" ]]; then
  JUDGE_ARG=(--judge-deployment "$JUDGE_DEPLOYMENT")
fi

if [[ -z "$LONGMEMEVAL_DATASET" ]]; then
  echo "FAIL: LONGMEMEVAL_DATASET is required (path to the LongMemEval-500 JSONL)" >&2
  exit 2
fi

strip_ansi() { sed $'s/\x1b\\[[0-9;]*m//g'; }

# Parse "Overall: N/M = …" → echoes "N M"; empty if not found.
parse_overall() {
  strip_ansi <"$1" \
    | grep -oE 'Overall:[[:space:]]*[0-9]+/[0-9]+' \
    | tail -1 \
    | grep -oE '[0-9]+/[0-9]+' \
    | tr '/' ' '
}

run_and_check() {
  local label="$1" dataset="$2" min_pct="$3" require_perfect="$4"
  local log
  log="$(mktemp)"
  echo "── $label : $dataset ──"
  "$ROOT_BIN" eval --dataset "$dataset" --path "$WS_PATH" --limit 0 "${JUDGE_ARG[@]}" \
    | tee "$log"
  local pair correct total pct
  pair="$(parse_overall "$log")"
  if [[ -z "$pair" ]]; then
    echo "FAIL [$label]: could not parse an 'Overall: N/M' line from eval output" >&2
    rm -f "$log"; return 1
  fi
  read -r correct total <<<"$pair"
  rm -f "$log"
  pct="$(awk -v c="$correct" -v t="$total" 'BEGIN { if (t==0){print 0} else {printf "%.2f", (c*100.0)/t} }')"
  echo "  → $label: $correct/$total = ${pct}%"

  if [[ "$require_perfect" == "yes" ]]; then
    if [[ "$correct" -ne "$total" ]]; then
      echo "FAIL [$label]: must be perfect ($total/$total), got $correct/$total" >&2
      return 1
    fi
    return 0
  fi
  # accuracy >= min_pct ?
  awk -v p="$pct" -v m="$min_pct" 'BEGIN { exit !(p+1e-9 >= m) }' || {
    echo "FAIL [$label]: ${pct}% < ${min_pct}% baseline — recall regression, change blocked" >&2
    return 1
  }
}

rc=0
run_and_check "LongMemEval-500" "$LONGMEMEVAL_DATASET" "$THRESHOLD" "no" || rc=1

if [[ -n "$PARAPHRASE_DATASET" ]]; then
  run_and_check "Paraphrase-4" "$PARAPHRASE_DATASET" "100" "yes" || rc=1
else
  echo "── Paraphrase-4 : SKIPPED (set PARAPHRASE_DATASET to enforce 4/4) ──"
fi

if [[ "$rc" -eq 0 ]]; then
  echo "PASS: eval gate cleared (≥ ${THRESHOLD}% + paraphrase intact)."
else
  echo "GATE FAILED — do not ship the recall-perturbing change." >&2
fi
exit "$rc"
