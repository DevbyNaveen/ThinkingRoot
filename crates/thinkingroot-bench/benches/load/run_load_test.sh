#!/usr/bin/env bash
# run_load_test.sh — Orchestrates k6 load tests against a live ThinkingRoot server.
#
# Usage:
#   ./run_load_test.sh [--scale small|medium|large] [--port PORT]
#
# Defaults: scale=small, port=9876

set -euo pipefail

# ── Defaults ────────────────────────────────────────────────────────────────
SCALE="small"
PORT="9876"
WORKSPACE="bench-workspace"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../../../" && pwd)"
RESULTS_DIR="${REPO_ROOT}/target/bench-results"
BINARY="${REPO_ROOT}/target/release/root"

# ── Argument parsing ─────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --scale)
      SCALE="$2"; shift 2 ;;
    --port)
      PORT="$2"; shift 2 ;;
    *)
      echo "Unknown flag: $1" >&2
      echo "Usage: $0 [--scale small|medium|large] [--port PORT]" >&2
      exit 1 ;;
  esac
done

# ── Validate scale ────────────────────────────────────────────────────────────
case "$SCALE" in
  small)  FILE_COUNT=50 ;;
  medium) FILE_COUNT=500 ;;
  large)  FILE_COUNT=5000 ;;
  *)
    echo "Invalid scale '${SCALE}'. Must be small, medium, or large." >&2
    exit 1 ;;
esac

BASE_URL="http://127.0.0.1:${PORT}"

echo "========================================"
echo "  ThinkingRoot k6 Load Test Orchestrator"
echo "  Scale: ${SCALE} (${FILE_COUNT} files)"
echo "  Port:  ${PORT}"
echo "  Base:  ${BASE_URL}"
echo "========================================"

# ── Step 1: Check k6 is installed ────────────────────────────────────────────
if ! command -v k6 &>/dev/null; then
  echo "ERROR: k6 is not installed or not on PATH." >&2
  echo "Install: https://k6.io/docs/getting-started/installation/" >&2
  exit 1
fi
echo "[1/9] k6 found: $(k6 version | head -1)"

# ── Step 2: Build release binary ─────────────────────────────────────────────
echo "[2/9] Building release binary..."
cd "${REPO_ROOT}"
cargo build --release -p thinkingroot-cli
echo "      Binary: ${BINARY}"

# ── Step 3: Create temp workspace dir ────────────────────────────────────────
echo "[3/9] Creating temporary workspace..."
TMPDIR_BASE="$(mktemp -d)"
WORKSPACE_PATH="${TMPDIR_BASE}/${WORKSPACE}"
mkdir -p "${WORKSPACE_PATH}/src"

cleanup() {
  echo ""
  echo "[9/9] Cleaning up..."
  if [[ -n "${SERVER_PID:-}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    echo "      Stopping server (PID ${SERVER_PID})..."
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  rm -rf "${TMPDIR_BASE}"
  echo "      Done."
}
trap cleanup EXIT INT TERM

# ── Step 4: Generate synthetic Rust source files ──────────────────────────────
echo "[4/9] Generating ${FILE_COUNT} synthetic source files..."

cat > "${WORKSPACE_PATH}/src/main.rs" << 'RSEOF'
//! Bench workspace main entry point.
//! This file is auto-generated for load testing purposes.

fn main() {
    println!("bench workspace");
}
RSEOF

for i in $(seq 1 "${FILE_COUNT}"); do
  cat > "${WORKSPACE_PATH}/src/module_${i}.rs" << RSEOF
//! Auto-generated module ${i} for load-test workspace.
//!
//! This module handles authentication, database access, cache management,
//! and various service operations. It depends on the configuration system
//! and exposes a handler interface used by the HTTP middleware layer.

use std::collections::HashMap;

/// Module ${i} configuration.
pub struct Config${i} {
    pub name: String,
    pub timeout_ms: u64,
    pub max_retries: u32,
}

impl Config${i} {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            timeout_ms: 5000,
            max_retries: 3,
        }
    }
}

/// Service handler for module ${i}.
pub fn handle_request_${i}(params: HashMap<String, String>) -> Result<String, String> {
    let key = params.get("key").ok_or("missing key")?;
    Ok(format!("module_${i}: processed {}", key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_${i}() {
        let cfg = Config${i}::new("test");
        assert_eq!(cfg.name, "test");
        assert_eq!(cfg.max_retries, 3);
    }
}
RSEOF
done

echo "      Generated ${FILE_COUNT} files in ${WORKSPACE_PATH}/src/"

# ── Step 5: Init and compile workspace ────────────────────────────────────────
echo "[5/9] Initialising workspace..."
"${BINARY}" init "${WORKSPACE_PATH}"

echo "      Compiling workspace (this may take a while for large scale)..."
"${BINARY}" compile "${WORKSPACE_PATH}" || {
  echo "WARNING: compile exited non-zero (LLM extraction may be unavailable — continuing)" >&2
}

# ── Step 6: Start server ──────────────────────────────────────────────────────
echo "[6/9] Starting server on port ${PORT}..."
mkdir -p "${RESULTS_DIR}"

"${BINARY}" serve \
  --port "${PORT}" \
  --path "${WORKSPACE_PATH}" \
  --no-mcp \
  > "${RESULTS_DIR}/server.log" 2>&1 &
SERVER_PID=$!
echo "      Server PID: ${SERVER_PID}"

# ── Step 7: Wait for server to be healthy ─────────────────────────────────────
echo "[7/9] Waiting for server to be ready (up to 30s)..."
READY=0
for attempt in $(seq 1 30); do
  if curl -sf "${BASE_URL}/api/v1/workspaces" >/dev/null 2>&1; then
    READY=1
    echo "      Server ready after ${attempt}s"
    break
  fi
  sleep 1
done

if [[ "${READY}" -eq 0 ]]; then
  echo "ERROR: Server did not become ready within 30 seconds." >&2
  echo "       Check ${RESULTS_DIR}/server.log for details." >&2
  exit 1
fi

# ── Step 8: Run k6 tests ──────────────────────────────────────────────────────
echo "[8/9] Running k6 load tests..."
mkdir -p "${RESULTS_DIR}"

K6_COMMON_ENV="BASE_URL=${BASE_URL},WORKSPACE=${WORKSPACE}"

run_k6() {
  local script_name="$1"
  local script_path="${SCRIPT_DIR}/${script_name}"
  local output_file="${RESULTS_DIR}/${script_name%.js}.json"

  echo ""
  echo "  --- ${script_name} ---"
  k6 run \
    --env BASE_URL="${BASE_URL}" \
    --env WORKSPACE="${WORKSPACE}" \
    --out "json=${output_file}" \
    "${script_path}" \
    && echo "  PASSED: ${script_name}" \
    || echo "  FAILED (threshold violation): ${script_name}"
}

run_k6 rest_search.js
run_k6 rest_entities.js
run_k6 mcp_tools.js
run_k6 mixed_workload.js

echo ""
echo "========================================"
echo "  Load test complete."
echo "  Results saved to: ${RESULTS_DIR}/"
echo "========================================"
