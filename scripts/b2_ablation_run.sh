#!/usr/bin/env bash
# B2 — LongMemEval Rooting ablation.
#
# Runs the 500-question LongMemEval-S benchmark twice against the same
# compiled workspace, toggling only whether Rooting filters Rejected-tier
# claims out of retrieval. Produces two JSON-parseable logs under
# benchmarks/ablation/ and a summary markdown.
#
# Cost: ~$6 on Azure gpt-4.1-mini, ~3 hours wall clock total.
# Output: benchmarks/ablation/2026-04-24/{off,on}.log and
#         benchmarks/BENCHMARK_ROOTING_ABLATION.md
set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"

ROOT_BIN="${ROOT_BIN:-$REPO_ROOT/target/release/root}"
WS="$REPO_ROOT/longmemeval-workspace"
DATASET="$REPO_ROOT/longmemeval-data/longmemeval_s.jsonl"
# Output dir tags the run date + deployment from the workspace config so
# multiple runs with different models do not clobber each other.
DEPLOY=$(grep -E "^deployment\s*=" "$REPO_ROOT/longmemeval-workspace/.thinkingroot/config.toml" \
  | head -1 | awk -F'"' '{print $2}')
TAG_DATE=$(date +%F)
OUT_DIR="$REPO_ROOT/benchmarks/ablation/${TAG_DATE}-${DEPLOY}"
mkdir -p "$OUT_DIR"

if [[ ! -x "$ROOT_BIN" ]]; then
  echo "error: $ROOT_BIN not executable" >&2
  exit 1
fi
if [[ -z "${AZURE_OPENAI_API_KEY:-}" ]]; then
  echo "error: set AZURE_OPENAI_API_KEY in env" >&2
  exit 2
fi

echo "=== B2 ablation start $(date -Iseconds) ==="
echo "ROOT_BIN = $ROOT_BIN"
echo "WORKSPACE = $WS"
echo "DATASET = $DATASET"
echo "OUT_DIR = $OUT_DIR"
echo

# ---------- Mode OFF (baseline) ---------------------------------------------
echo "=== mode=off (baseline: retrieval sees all 95,584 claims) ==="
OFF_START=$(date +%s)
"$ROOT_BIN" eval \
  --dataset "$DATASET" \
  --path "$WS" \
  --rooting-mode off 2>&1 | tee "$OUT_DIR/off.log"
OFF_END=$(date +%s)
OFF_DUR=$((OFF_END - OFF_START))
echo "=== mode=off completed in ${OFF_DUR}s ==="
echo

# ---------- Mode ON (enforce) -----------------------------------------------
echo "=== mode=on (Rooting filter: retrieval excludes 1,210 Rejected claims) ==="
ON_START=$(date +%s)
"$ROOT_BIN" eval \
  --dataset "$DATASET" \
  --path "$WS" \
  --rooting-mode on 2>&1 | tee "$OUT_DIR/on.log"
ON_END=$(date +%s)
ON_DUR=$((ON_END - ON_START))
echo "=== mode=on completed in ${ON_DUR}s ==="
echo

# ---------- Extract + summarize ---------------------------------------------
OFF_SCORE=$(grep -Eo 'Overall: [0-9]+/[0-9]+ = [0-9.]+%' "$OUT_DIR/off.log" | tail -1)
ON_SCORE=$(grep -Eo 'Overall: [0-9]+/[0-9]+ = [0-9.]+%' "$OUT_DIR/on.log" | tail -1)

REPORT="$REPO_ROOT/benchmarks/BENCHMARK_ROOTING_ABLATION.md"
cat > "$REPORT" <<REPORT_EOF
# Rooting Ablation on LongMemEval-500 — $(date +%Y-%m-%d)

Two full 500-question LongMemEval-S runs against the same compiled
workspace (\`longmemeval-workspace/\`, 95,584 claims, 990 sources,
Azure gpt-4.1-mini for both synthesis and judging), toggling only
whether Rooting filters Rejected-tier claims out of retrieval.

Harness: \`scripts/b2_ablation_run.sh\`
Raw logs: \`$OUT_DIR/off.log\`, \`$OUT_DIR/on.log\`

## Headline

| Mode | Overall | Wall clock |
|------|---------|------------|
| off (retrieval sees all 95,584 claims) | $OFF_SCORE | ${OFF_DUR}s |
| on  (retrieval excludes 1,210 Rejected claims) | $ON_SCORE | ${ON_DUR}s |

## Per-category breakdown

### Mode=off (baseline)
\`\`\`
$(grep -E "^\s+[a-z-]+\s+[0-9]+/[0-9]+" "$OUT_DIR/off.log" | tail -20 || true)
\`\`\`

### Mode=on (Rooting filter)
\`\`\`
$(grep -E "^\s+[a-z-]+\s+[0-9]+/[0-9]+" "$OUT_DIR/on.log" | tail -20 || true)
\`\`\`

## Interpretation

This workspace was compiled before predicate extraction was wired into
the LLM prompts, so the only claims Rooting can reject are those
flagged by the Contradiction probe (1,210 of 95,584, 1.27%).
The ablation therefore measures a narrow but specific question: does
removing those flagged-contradictory claims change end-to-end
LongMemEval accuracy?

An improvement or no-regression result in the on-mode column
demonstrates that Rooting's fatal-probe rejections are at worst benign
to read-time accuracy, at best a positive signal. A regression would
indicate the gate is dropping load-bearing claims, which would warrant
tightening the contradiction-probe confidence floor
(\`contradiction_floor\`, default 0.85).

## Reproduction

\`\`\`
cargo build --release -p thinkingroot-cli --bin root
AZURE_OPENAI_API_KEY=<key> ./scripts/b2_ablation_run.sh
\`\`\`
REPORT_EOF

echo "=== ablation summary written to $REPORT ==="
echo "off=$OFF_SCORE (${OFF_DUR}s)"
echo "on=$ON_SCORE  (${ON_DUR}s)"
