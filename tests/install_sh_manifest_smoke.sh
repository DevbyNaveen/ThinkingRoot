#!/bin/sh
# Integration test: run install.sh against a local file-URL fake
# release, then assert the install manifest is well-formed with the
# expected fields.  Designed to run on macOS + Linux CI.

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Sandbox: isolated INSTALL_DIR + XDG_CONFIG_HOME so we never touch
# the user's real install.
SANDBOX="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  if [ -n "${SERVER_PID}" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$SANDBOX"
}
trap cleanup EXIT INT TERM

INSTALL_DIR="${SANDBOX}/bin"
mkdir -p "$INSTALL_DIR"
export INSTALL_DIR

# Override config dir to sandbox.  resolve_config_dir() in install.sh
# honours XDG_CONFIG_HOME on both Linux and macOS now (per Task 8).
export XDG_CONFIG_HOME="${SANDBOX}/config"

# ── Build a fake `root` binary ────────────────────────────────────────────────
#
# Supports `--version` and `hash-file <path>` — enough for install.sh's
# post-install verification step plus the blake3sum helper.
FAKE_BIN="${SANDBOX}/fake-root"
cat > "$FAKE_BIN" <<'EOF'
#!/bin/sh
case "$1" in
  --version) echo "root 0.9.1-test" ;;
  hash-file)
    # Deterministic dummy BLAKE3 hex (64 chars).  install.sh just
    # needs SOMETHING parseable as a checksum; full correctness is
    # validated by the Rust unit tests, not this smoke test.
    echo "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
    ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$FAKE_BIN"

# ── Stage the fake release ───────────────────────────────────────────────────
RELEASE_DIR="${SANDBOX}/release"
mkdir -p "$RELEASE_DIR"

OS="$(uname -s | tr '[:upper:]' '[:lower:]' | sed s/darwin/macos/)"
ARCH_RAW="$(uname -m)"
case "$ARCH_RAW" in
  x86_64|amd64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) echo "FAIL: unsupported test arch $ARCH_RAW" >&2; exit 1 ;;
esac
ASSET_NAME="root-${OS}-${ARCH}"
cp "$FAKE_BIN" "${RELEASE_DIR}/${ASSET_NAME}"

# Compute SHA-256 — install.sh verifies this.
if command -v sha256sum >/dev/null 2>&1; then
  SHA256="$(sha256sum "${RELEASE_DIR}/${ASSET_NAME}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  SHA256="$(shasum -a 256 "${RELEASE_DIR}/${ASSET_NAME}" | awk '{print $1}')"
else
  echo "FAIL: need sha256sum or shasum to run this test" >&2
  exit 1
fi
echo "${SHA256}  ${ASSET_NAME}" > "${RELEASE_DIR}/checksums.txt"

# macOS Intel ships a tar.gz bundle; produce one if needed.
if [ "$OS" = "macos" ] && [ "$ARCH" = "amd64" ]; then
  BUNDLE_NAME="root-${OS}-${ARCH}.tar.gz"
  (cd "$RELEASE_DIR" && tar -czf "$BUNDLE_NAME" "$ASSET_NAME")
  if command -v sha256sum >/dev/null 2>&1; then
    SHA256="$(sha256sum "${RELEASE_DIR}/${BUNDLE_NAME}" | awk '{print $1}')"
  else
    SHA256="$(shasum -a 256 "${RELEASE_DIR}/${BUNDLE_NAME}" | awk '{print $1}')"
  fi
  echo "${SHA256}  ${BUNDLE_NAME}" >> "${RELEASE_DIR}/checksums.txt"
fi

# ── Start a local http.server ────────────────────────────────────────────────
if ! command -v python3 >/dev/null 2>&1; then
  echo "FAIL: python3 required for smoke test" >&2
  exit 1
fi

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("", 0)); print(s.getsockname()[1]); s.close()')"
( cd "$RELEASE_DIR" && python3 -m http.server "$PORT" >/dev/null 2>&1 ) &
SERVER_PID=$!

# Wait for the server to bind (poll for up to 5s).
ready=0
i=0
while [ $i -lt 50 ]; do
  if python3 -c "
import socket, sys
s = socket.socket()
try:
    s.connect(('127.0.0.1', $PORT))
    print('ready')
except Exception:
    sys.exit(1)
" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.1
  i=$((i+1))
done
if [ "$ready" != "1" ]; then
  echo "FAIL: http.server did not bind on port $PORT" >&2
  exit 1
fi

# ── Verify install.sh's TR_TEST_BASE_URL hook is present ──────────────────────
if ! grep -q "TR_TEST_BASE_URL" "${REPO_ROOT}/install.sh"; then
  echo "FAIL: install.sh does not honour TR_TEST_BASE_URL — patch it" >&2
  echo "(insert: BASE_URL=\"\${TR_TEST_BASE_URL:-\$BASE_URL}\" after the BASE_URL= line)" >&2
  exit 1
fi

# ── Run install.sh ───────────────────────────────────────────────────────────
export VERSION="v0.9.1-test"
export TR_TEST_BASE_URL="http://localhost:${PORT}"
# Skip NLI model download — the http.server only serves the binary +
# checksums.txt, not the ~83 MB ONNX model. install.sh honours this
# via the wrapper added in Task 11.
export TR_SKIP_NLI=1

sh "${REPO_ROOT}/install.sh" || {
  echo "INSTALL.SH FAILED — see output above." >&2
  exit 1
}

# ── Assertions ───────────────────────────────────────────────────────────────
MANIFEST="${XDG_CONFIG_HOME}/thinkingroot/install-manifest.json"
[ -f "$MANIFEST" ] || { echo "FAIL: manifest not written at $MANIFEST"; exit 1; }

# Pure-shell JSON probes — no jq required.
grep -q '"id": "cli-script"' "$MANIFEST" \
  || { echo "FAIL: cli-script entry missing"; cat "$MANIFEST"; exit 1; }
grep -q "\"path\": \"${INSTALL_DIR}/root\"" "$MANIFEST" \
  || { echo "FAIL: path mismatch"; cat "$MANIFEST"; exit 1; }
grep -q '"preferred": "cli-script"' "$MANIFEST" \
  || { echo "FAIL: preferred wrong"; cat "$MANIFEST"; exit 1; }
grep -q '"schema_version": 1' "$MANIFEST" \
  || { echo "FAIL: schema_version wrong"; cat "$MANIFEST"; exit 1; }

CHECKSUMS_CACHE="${XDG_CONFIG_HOME}/thinkingroot/checksums-cache.txt"
[ -f "$CHECKSUMS_CACHE" ] || { echo "FAIL: checksums cache not written at $CHECKSUMS_CACHE"; exit 1; }

# Mode 0600 on Unix.
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) ;;  # Skip mode check on Windows
  *)
    if command -v stat >/dev/null 2>&1; then
      # BSD stat (macOS) and GNU stat differ in flags.
      MODE="$(stat -f '%Lp' "$MANIFEST" 2>/dev/null || stat -c '%a' "$MANIFEST" 2>/dev/null)"
      [ "$MODE" = "600" ] || { echo "FAIL: manifest mode is $MODE, expected 600"; exit 1; }
    fi
    ;;
esac

echo "PASS: install manifest smoke"
