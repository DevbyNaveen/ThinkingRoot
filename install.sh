#!/bin/sh
# ThinkingRoot installer
# Usage: curl -fsSL https://raw.githubusercontent.com/thinkingroot/thinkingroot/main/install.sh | sh
# Or:    curl -fsSL https://thinkingroot.dev/install.sh | sh
set -e

REPO="thinkingroot/thinkingroot"
BINARY="root"
INSTALL_DIR="${INSTALL_DIR:-}"

# ── Helpers ──────────────────────────────────────────────────────────────────

say() { printf '\033[1;32m==> %s\033[0m\n' "$*"; }
err() { printf '\033[1;31mError: %s\033[0m\n' "$*" >&2; exit 1; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || err "need '$1' (not found in PATH)"; }
is_cmd()   { command -v "$1" >/dev/null 2>&1; }

# ── OS detection ─────────────────────────────────────────────────────────────

detect_os() {
  case "$(uname -s)" in
    Linux)  echo "linux"  ;;
    Darwin) echo "macos"  ;;
    *)      err "Unsupported OS: $(uname -s). Install manually from https://github.com/${REPO}/releases" ;;
  esac
}

# ── Architecture detection ────────────────────────────────────────────────────

detect_arch() {
  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64) echo "amd64" ;;
    aarch64|arm64)
      # On macOS, confirm it's real arm64 (not Rosetta)
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

# ── Download helper (curl with wget fallback) ─────────────────────────────────

download() {
  url="$1"; dest="$2"
  if is_cmd curl; then
    curl --tlsv1.2 --proto '=https' -fsSL "$url" -o "$dest"
  elif is_cmd wget; then
    wget -q --https-only -O "$dest" "$url"
  else
    err "Neither curl nor wget found. Install one and retry."
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

# ── Install dir selection ─────────────────────────────────────────────────────

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
    echo "$HOME/.local/bin"
    mkdir -p "$HOME/.local/bin"
  fi
}

# ── Fetch latest version tag from GitHub ─────────────────────────────────────

fetch_latest_version() {
  if is_cmd curl; then
    curl --tlsv1.2 --proto '=https' -fsSL \
      "https://api.github.com/repos/${REPO}/releases/latest" \
      | grep '"tag_name"' | cut -d'"' -f4
  elif is_cmd wget; then
    wget -q --https-only -O- \
      "https://api.github.com/repos/${REPO}/releases/latest" \
      | grep '"tag_name"' | cut -d'"' -f4
  else
    err "Neither curl nor wget found."
  fi
}

# ── Main ──────────────────────────────────────────────────────────────────────

main() {
  need_cmd uname

  OS="$(detect_os)"
  ARCH="$(detect_arch)"
  INSTALL_DIR="$(select_install_dir)"

  # Determine asset name matching release.yml naming convention
  ASSET="${BINARY}-${OS}-${ARCH}"

  say "Detecting latest version..."
  VERSION="${VERSION:-$(fetch_latest_version)}"
  [ -z "$VERSION" ] && err "Could not determine latest version. Set VERSION env var manually."

  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
  ASSET_URL="${BASE_URL}/${ASSET}"
  CHECKSUM_URL="${BASE_URL}/checksums.txt"

  say "Installing ${BINARY} ${VERSION} for ${OS}/${ARCH}"
  say "Downloading from: ${ASSET_URL}"

  TMP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TMP_DIR"' EXIT

  ASSET_PATH="${TMP_DIR}/${ASSET}"
  CHECKSUMS_PATH="${TMP_DIR}/checksums.txt"

  download "$ASSET_URL"     "$ASSET_PATH"
  download "$CHECKSUM_URL"  "$CHECKSUMS_PATH"

  say "Verifying SHA256 checksum..."
  EXPECTED="$(grep "$ASSET" "$CHECKSUMS_PATH" | cut -d' ' -f1)"
  [ -z "$EXPECTED" ] && err "Checksum not found for ${ASSET} in checksums.txt"
  ACTUAL="$(sha256 "$ASSET_PATH")"
  [ "$EXPECTED" != "$ACTUAL" ] && err "Checksum mismatch!\n  Expected: $EXPECTED\n  Got:      $ACTUAL"
  say "Checksum OK"

  chmod +x "$ASSET_PATH"
  mv "$ASSET_PATH" "${INSTALL_DIR}/${BINARY}"

  say "Installed to: ${INSTALL_DIR}/${BINARY}"

  # PATH hint if ~/.local/bin
  case "$INSTALL_DIR" in
    "$HOME/.local/bin")
      case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) say "Add to your shell profile: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
      esac
      ;;
  esac

  say "Done! Run: ${BINARY} --version"
  "${INSTALL_DIR}/${BINARY}" --version || true
}

main "$@"
