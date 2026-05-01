#!/bin/sh
# ThinkingRoot installer
# Usage: curl -fsSL https://raw.githubusercontent.com/DevbyNaveen/ThinkingRoot/main/install.sh | sh
#
# Strict mode: fail fast on any error, undefined variable, or pipeline
# component failure. `set -e` alone leaves the script exposed to typos
# in variable names (silently expanding to empty strings — used to
# overwrite legitimate paths) and to grep/curl failures inside pipes
# returning success because the last command succeeded.
set -eu
# `pipefail` is POSIX-2024 but not in /bin/sh on macOS 10.x; tolerate
# the failure so we still get -eu on minimal shells.
(set -o pipefail 2>/dev/null) && set -o pipefail || true
# Guard against IFS-based command injection if the user has a
# pre-seeded IFS in their environment.
IFS='
'

REPO="DevbyNaveen/ThinkingRoot"
RELEASES_REPO="DevbyNaveen/releases"
NLI_MODELS_TAG="nli-models"
BINARY="root"
INSTALL_DIR="${INSTALL_DIR:-}"
# Optional minisign public key for verifying `checksums.txt`.  When
# unset the installer falls back to TLS-only trust on the checksum
# file — a CA-level MITM (or a release-pipeline compromise that can
# write to the GitHub release assets) would not be detected.  Setting
# this to a published TR key closes that gap end-to-end.  Override
# with: TR_MINISIGN_PUBKEY="RWQf6...rest..."  curl ... | sh
TR_MINISIGN_PUBKEY="${TR_MINISIGN_PUBKEY:-}"
# Set to "1" to require signature verification — installs abort if
# minisign is missing or the checksum file isn't signed. Default off
# so first-boot CI/CD pipelines without minisign installed still work.
TR_REQUIRE_SIGNATURE="${TR_REQUIRE_SIGNATURE:-0}"

# ── Helpers ──────────────────────────────────────────────────────────────────

say()     { printf '\033[1;32m==> %s\033[0m\n' "$*"; }
say_dim() { printf '\033[0;37m    %s\033[0m\n' "$*"; }
err()     { printf '\033[1;31mError: %s\033[0m\n' "$*" >&2; exit 1; }
warn()    { printf '\033[1;33mWarning: %s\033[0m\n' "$*" >&2; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || err "need '$1' (not found in PATH)"; }
is_cmd()   { command -v "$1" >/dev/null 2>&1; }

# ── OS / arch detection ───────────────────────────────────────────────────────

detect_os() {
  case "$(uname -s)" in
    Linux)  echo "linux"  ;;
    Darwin) echo "macos"  ;;
    *)      err "Unsupported OS: $(uname -s). Install manually from https://github.com/${REPO}/releases" ;;
  esac
}

detect_arch() {
  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64) echo "amd64" ;;
    aarch64|arm64)
      if [ "$(uname -s)" = "Darwin" ]; then
        rosetta=$(sysctl -q hw.optional.arm64 2>/dev/null | awk '{print $2}')
        [ "$rosetta" = "1" ] && echo "arm64" || echo "amd64"
      else
        echo "arm64"
      fi
      ;;
    *) err "Unsupported architecture: $arch. Install manually from https://github.com/${REPO}/releases" ;;
  esac
}

# Returns the NLI ONNX filename for this arch
nli_onnx_filename() {
  ARCH="$1"
  case "$ARCH" in
    arm64|aarch64) echo "model_qint8_arm64.onnx" ;;
    *)             echo "model_quint8_avx2.onnx" ;;
  esac
}

# ── Download helper (curl → wget fallback) ────────────────────────────────────

download() {
  url="$1"; dest="$2"
  if is_cmd curl; then
    curl --tlsv1.2 --proto '=https' -fSL --progress-bar "$url" -o "$dest"
  elif is_cmd wget; then
    wget --https-only -O "$dest" "$url"
  else
    err "Neither curl nor wget found. Install one and retry."
  fi
}

download_quiet() {
  url="$1"; dest="$2"
  if is_cmd curl; then
    curl --tlsv1.2 --proto '=https' -fsSL "$url" -o "$dest"
  elif is_cmd wget; then
    wget -q --https-only -O "$dest" "$url"
  else
    err "Neither curl nor wget found."
  fi
}

# ── SHA256 helper ─────────────────────────────────────────────────────────────

sha256() {
  file="$1"
  if is_cmd sha256sum; then
    sha256sum "$file" | cut -d' ' -f1
  elif is_cmd shasum; then
    shasum -a 256 "$file" | cut -d' ' -f1
  elif is_cmd openssl; then
    openssl dgst -sha256 "$file" | awk '{print $NF}'
  else
    err "No SHA256 tool found (tried sha256sum, shasum, openssl)."
  fi
}

# ── Install dir ───────────────────────────────────────────────────────────────

select_install_dir() {
  if [ -n "$INSTALL_DIR" ]; then
    echo "$INSTALL_DIR"
    return
  fi
  if [ -w /usr/local/bin ]; then
    echo "/usr/local/bin"
  elif [ -d "$HOME/.local/bin" ] && [ -w "$HOME/.local/bin" ]; then
    echo "$HOME/.local/bin"
  else
    mkdir -p "$HOME/.local/bin" || err "Cannot create $HOME/.local/bin"
    echo "$HOME/.local/bin"
  fi
}

# ── Model cache dir ───────────────────────────────────────────────────────────

model_cache_dir() {
  if [ "$(uname -s)" = "Darwin" ]; then
    echo "${HOME}/Library/Caches/thinkingroot/models"
  else
    echo "${HOME}/.cache/thinkingroot/models"
  fi
}

# ── Fetch latest version ──────────────────────────────────────────────────────

fetch_latest_version() {
  download_quiet \
    "https://api.github.com/repos/${RELEASES_REPO}/releases/latest" \
    /dev/stdout 2>/dev/null \
    | grep '"tag_name"' | cut -d'"' -f4
}

# ── Install NLI models ────────────────────────────────────────────────────────

install_nli_models() {
  ARCH="$1"
  MODEL_DIR="$(model_cache_dir)"
  ONNX_FILE="$(nli_onnx_filename "$ARCH")"
  BASE="https://github.com/${RELEASES_REPO}/releases/download/${NLI_MODELS_TAG}"

  mkdir -p "$MODEL_DIR" || err "Cannot create model cache dir: $MODEL_DIR"

  if [ -f "${MODEL_DIR}/${ONNX_FILE}" ]; then
    say_dim "NLI model already cached: ${MODEL_DIR}/${ONNX_FILE}"
  else
    say "Downloading NLI model (~83 MB, one-time)..."
    download "${BASE}/${ONNX_FILE}" "${MODEL_DIR}/${ONNX_FILE}" \
      || { warn "NLI model download failed — grounding will use judges 1-3 only. Re-run installer to retry."; return 0; }
    say_dim "Saved to ${MODEL_DIR}/${ONNX_FILE}"
  fi

  if [ -f "${MODEL_DIR}/tokenizer.json" ]; then
    say_dim "Tokenizer already cached."
  else
    say "Downloading tokenizer..."
    download_quiet "${BASE}/tokenizer.json" "${MODEL_DIR}/tokenizer.json" \
      || { warn "Tokenizer download failed — re-run installer to retry."; return 0; }
    say_dim "Saved to ${MODEL_DIR}/tokenizer.json"
  fi

  say "NLI models ready."
}

# ── Main ──────────────────────────────────────────────────────────────────────

main() {
  need_cmd uname

  OS="$(detect_os)"
  ARCH="$(detect_arch)"
  INSTALL_DIR="$(select_install_dir)"

  # macOS Intel ships as a tar.gz bundle (binary + ONNX Runtime dylib)
  if [ "$OS" = "macos" ] && [ "$ARCH" = "amd64" ]; then
    ASSET="${BINARY}-${OS}-${ARCH}.tar.gz"
    IS_BUNDLE=1
  else
    ASSET="${BINARY}-${OS}-${ARCH}"
    IS_BUNDLE=0
  fi

  say "Detecting latest version..."
  VERSION="${VERSION:-$(fetch_latest_version)}"
  [ -z "$VERSION" ] && err "Could not determine latest version. Set VERSION= env var manually."

  BASE_URL="https://github.com/${RELEASES_REPO}/releases/download/${VERSION}"
  ASSET_URL="${BASE_URL}/${ASSET}"
  CHECKSUM_URL="${BASE_URL}/checksums.txt"

  say "Installing ${BINARY} ${VERSION} for ${OS}/${ARCH}"

  TMP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TMP_DIR"' EXIT

  ASSET_PATH="${TMP_DIR}/${ASSET}"
  CHECKSUMS_PATH="${TMP_DIR}/checksums.txt"
  CHECKSUMS_SIG_PATH="${TMP_DIR}/checksums.txt.minisig"

  say "Downloading binary..."
  download "$ASSET_URL" "$ASSET_PATH"
  download_quiet "$CHECKSUM_URL" "$CHECKSUMS_PATH"

  # Optional signature verification.  When TR_MINISIGN_PUBKEY is set
  # AND `minisign` is installed, verify checksums.txt before trusting
  # any digest from it.  This closes the MITM-with-forged-TLS-cert
  # gap and the "release-pipeline compromise rewrites checksums.txt"
  # gap.  When the env var is unset the installer falls back to
  # TLS-only trust (current behaviour).
  if [ -n "$TR_MINISIGN_PUBKEY" ]; then
    if is_cmd minisign; then
      SIG_URL="${BASE_URL}/checksums.txt.minisig"
      if download_quiet "$SIG_URL" "$CHECKSUMS_SIG_PATH" 2>/dev/null; then
        say "Verifying checksum signature with minisign..."
        if minisign -V -P "$TR_MINISIGN_PUBKEY" \
            -m "$CHECKSUMS_PATH" -x "$CHECKSUMS_SIG_PATH" >/dev/null 2>&1; then
          say "Signature OK"
        else
          err "checksums.txt signature verification failed — refusing to install"
        fi
      else
        if [ "$TR_REQUIRE_SIGNATURE" = "1" ]; then
          err "TR_REQUIRE_SIGNATURE=1 but checksums.txt.minisig is missing"
        else
          warn "checksums.txt.minisig not published yet — falling back to TLS-only trust"
        fi
      fi
    else
      if [ "$TR_REQUIRE_SIGNATURE" = "1" ]; then
        err "TR_REQUIRE_SIGNATURE=1 but minisign is not installed (brew install minisign / apt install minisign)"
      else
        warn "minisign not installed — falling back to TLS-only trust on checksums.txt"
      fi
    fi
  elif [ "$TR_REQUIRE_SIGNATURE" = "1" ]; then
    err "TR_REQUIRE_SIGNATURE=1 but TR_MINISIGN_PUBKEY is unset"
  fi

  say "Verifying SHA256 checksum..."
  # Anchor the match: an unanchored grep would treat
  # `root-linux-amd64` as a substring of `root-linux-amd64.exe` and
  # pull the wrong line.  `grep -F` (literal, no regex meta) defends
  # against artifact names that ever contain `.` or `+`.
  EXPECTED="$(grep -F " ${ASSET}" "$CHECKSUMS_PATH" \
              | awk -v a=" ${ASSET}" '$0 ~ a"$" || $0 ~ "[*]"substr(a,2)"$" {print $1; exit}')"
  if [ -z "$EXPECTED" ]; then
    err "Checksum not found for ${ASSET} in checksums.txt"
  fi
  # Reject malformed digest entries (sha256 = exactly 64 hex chars).
  # Defense-in-depth in case checksums.txt itself was truncated mid-line.
  case "$EXPECTED" in
    *[!0-9a-fA-F]*) err "Malformed SHA256 in checksums.txt: ${EXPECTED}" ;;
  esac
  if [ "${#EXPECTED}" != 64 ]; then
    err "SHA256 must be 64 hex chars, got ${#EXPECTED}: ${EXPECTED}"
  fi
  ACTUAL="$(sha256 "$ASSET_PATH")"
  if [ "$EXPECTED" != "$ACTUAL" ]; then
    printf '\033[1;31mChecksum mismatch!\n  Expected: %s\n  Got:      %s\033[0m\n' \
      "$EXPECTED" "$ACTUAL" >&2
    exit 1
  fi
  say "Checksum OK"

  if [ "$IS_BUNDLE" = "1" ]; then
    # Extract binary + ONNX Runtime dylib to a staging dir first so a
    # corrupt tarball can't half-overwrite an existing install.  Move
    # into place atomically only after extraction succeeds.
    STAGE_DIR="${TMP_DIR}/stage"
    mkdir -p "$STAGE_DIR"
    tar -xzf "$ASSET_PATH" -C "$STAGE_DIR"
    [ -f "${STAGE_DIR}/${BINARY}" ] || err "tarball did not contain ${BINARY}"
    chmod +x "${STAGE_DIR}/${BINARY}"
    # Move every staged file into INSTALL_DIR with a tmp + rename.
    for src in "$STAGE_DIR"/*; do
      base="$(basename "$src")"
      mv "$src" "${INSTALL_DIR}/${base}.tr-installing" \
        || err "failed to stage ${base} into ${INSTALL_DIR}"
      mv "${INSTALL_DIR}/${base}.tr-installing" "${INSTALL_DIR}/${base}" \
        || err "failed to install ${base} into ${INSTALL_DIR}"
    done
    say "Installed: ${INSTALL_DIR}/${BINARY} (+ libonnxruntime dylib)"
  else
    chmod +x "$ASSET_PATH"
    # Atomic install: move to a sibling tmp path first, then rename
    # over the live binary.  Pre-fix a SIGINT during `mv` could leave
    # `INSTALL_DIR/root` truncated — half of the new bytes, half of
    # the old, which crashes on first invocation.
    STAGED="${INSTALL_DIR}/${BINARY}.tr-installing"
    mv "$ASSET_PATH" "$STAGED" \
      || err "failed to stage binary into ${INSTALL_DIR} (insufficient permissions?)"
    mv "$STAGED" "${INSTALL_DIR}/${BINARY}" \
      || { rm -f "$STAGED"; err "failed to install binary into ${INSTALL_DIR}"; }
    say "Installed: ${INSTALL_DIR}/${BINARY}"
  fi

  # PATH hint
  case "$INSTALL_DIR" in
    "$HOME/.local/bin")
      case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) say "Add to PATH: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
      esac
      ;;
  esac

  # ── Download NLI models ───────────────────────────────────────────────────
  install_nli_models "$ARCH"

  printf '\n'
  say "Done!"
  "${INSTALL_DIR}/${BINARY}" --version || true
  printf '\n'
  printf '    Get started:\n'
  printf '      root setup         # interactive wizard\n'
  printf '      root compile .     # compile your first knowledge base\n'
  printf '      root ask "what does this codebase do?"\n'
  printf '\n'
}

main "$@"
