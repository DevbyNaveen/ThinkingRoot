#!/usr/bin/env bash
# release-local.sh — laptop-driven ThinkingRoot release (no CI).
#
# Builds the `root` CLI and the Tauri desktop bundle for the host
# platform, generates a Tauri-compatible latest.json and SHA256
# checksums, and optionally publishes the resulting GitHub Release
# via `gh`. Designed to ship from a single developer machine without
# the GitHub Actions matrix.
#
# What it builds on the host you run it on:
#   macOS (arm64): root CLI + .dmg + .app.tar.gz + .app.tar.gz.sig
#   macOS (x64):   root CLI + .dmg + .app.tar.gz + .app.tar.gz.sig
#   Linux (amd64): root CLI + .AppImage + .AppImage.sig + .deb
#   Windows (x64): root CLI + -setup.exe + -setup.exe.sig + .msi
#
# Linux + Windows artifacts have to be produced on real machines for
# that OS — run this script on an Azure VM of the right OS, scp the
# resulting artifact directory back to your Mac, then run the
# combine + publish step below.
#
# Usage:
#   ./scripts/release-local.sh v0.9.2                # build only
#   ./scripts/release-local.sh v0.9.2 --publish      # build + gh release create
#   ./scripts/release-local.sh v0.9.2 --publish --models-tag models-v1
#
# Prereqs (verified up front):
#   * cargo + rustup with the host triple installed
#   * pnpm (for the Tauri frontend build)
#   * gh CLI authenticated to DevbyNaveen/releases (only for --publish)
#   * apps/thinkingroot-desktop/src-tauri/keys/updater.key on disk
#   * jq (for latest.json post-processing on Linux/Windows hosts)
#
# Exits non-zero on any pre-flight failure rather than producing a
# half-baked release directory.

set -euo pipefail

VERSION="${1:-}"
PUBLISH=0
MODELS_TAG=""

shift || true
while [ $# -gt 0 ]; do
  case "$1" in
    --publish) PUBLISH=1 ;;
    --models-tag) MODELS_TAG="$2"; shift ;;
    *) echo "Unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

if [ -z "$VERSION" ]; then
  cat >&2 <<USAGE
Usage: scripts/release-local.sh <version> [--publish] [--models-tag <tag>]

  <version>            e.g. v0.9.2 (must match ^v[0-9]+\\.[0-9]+\\.[0-9]+\$)
  --publish            invoke gh release create after building
  --models-tag <tag>   if set, also verify the models release exists

Example:
  scripts/release-local.sh v0.9.2 --publish
USAGE
  exit 2
fi

if ! printf '%s' "$VERSION" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "Bad version: $VERSION (expected vMAJOR.MINOR.PATCH)" >&2
  exit 2
fi

VERSION_BARE="${VERSION#v}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# ── Pre-flight ───────────────────────────────────────────────────────────────

need() { command -v "$1" >/dev/null 2>&1 || { echo "Missing tool: $1" >&2; exit 3; }; }
need cargo
need rustup
need pnpm

SIGN_KEY_PATH="$REPO_ROOT/apps/thinkingroot-desktop/src-tauri/keys/updater.key"
[ -f "$SIGN_KEY_PATH" ] || {
  echo "Missing Tauri signing key at $SIGN_KEY_PATH" >&2
  echo "Generate one with: tauri signer generate" >&2
  exit 3
}

if [ "$PUBLISH" -eq 1 ]; then
  need gh
  if ! gh auth status >/dev/null 2>&1; then
    echo "gh CLI not authenticated. Run: gh auth login" >&2
    exit 3
  fi
fi

# ── Host platform detection ──────────────────────────────────────────────────

HOST_OS="$(uname -s)"
HOST_ARCH="$(uname -m)"

case "$HOST_OS-$HOST_ARCH" in
  Darwin-arm64)   TARGET=aarch64-apple-darwin;       PLATFORM=macos-arm64; PRODUCT_ARCH=aarch64; STABLE_DMG=ThinkingRoot-mac.dmg ;;
  Darwin-x86_64)  TARGET=x86_64-apple-darwin;        PLATFORM=macos-x64;   PRODUCT_ARCH=x64;     STABLE_DMG=ThinkingRoot-mac-intel.dmg ;;
  Linux-x86_64)   TARGET=x86_64-unknown-linux-gnu;   PLATFORM=linux-amd64; PRODUCT_ARCH=amd64;   STABLE_DMG="" ;;
  Linux-aarch64)  TARGET=aarch64-unknown-linux-gnu;  PLATFORM=linux-arm64; PRODUCT_ARCH=arm64;   STABLE_DMG="" ;;
  *) echo "Unsupported host: $HOST_OS-$HOST_ARCH" >&2; exit 3 ;;
esac

if ! rustup target list --installed | grep -q "^$TARGET$"; then
  echo "Installing missing rust target $TARGET..."
  rustup target add "$TARGET"
fi

OUT_DIR="$REPO_ROOT/dist/$VERSION/$PLATFORM"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

echo "=> Building for $TARGET into $OUT_DIR"

# ── Build CLI ────────────────────────────────────────────────────────────────

echo "=> cargo build --release -p thinkingroot-cli --target $TARGET"
cargo build --release --target "$TARGET" -p thinkingroot-cli

CLI_BIN="$REPO_ROOT/target/$TARGET/release/root"
[ -f "$CLI_BIN" ] || [ -f "$CLI_BIN.exe" ] || { echo "CLI binary not found" >&2; exit 4; }

case "$PLATFORM" in
  macos-arm64) cp "$CLI_BIN" "$OUT_DIR/root-macos-arm64" ;;
  macos-x64)
    # macOS Intel ships as a tar.gz bundle alongside its ORT dylib
    # (cf. release.yml). For the laptop-driven flow the dylib has
    # to be staged manually by the operator; if it's not present we
    # ship the bare binary and let install.sh fail loudly.
    cp "$CLI_BIN" "$OUT_DIR/root-macos-amd64"
    ;;
  linux-amd64)   cp "$CLI_BIN" "$OUT_DIR/root-linux-amd64" ;;
  linux-arm64)   cp "$CLI_BIN" "$OUT_DIR/root-linux-arm64" ;;
esac

# ── Build Tauri desktop bundle ───────────────────────────────────────────────

echo "=> cargo tauri build (signed via local updater.key)"
(
  cd "$REPO_ROOT/apps/thinkingroot-desktop/src-tauri"
  # Stage the freshly-built CLI as the externalBin sidecar so Tauri
  # bundles the right `root` binary into the .app / .AppImage / .exe.
  mkdir -p binaries
  case "$PLATFORM" in
    macos-arm64|macos-x64)
      cp "$REPO_ROOT/target/$TARGET/release/root" \
        "binaries/thinkingroot-agent-runtime-$TARGET"
      chmod +x "binaries/thinkingroot-agent-runtime-$TARGET"
      ;;
    linux-*)
      cp "$REPO_ROOT/target/$TARGET/release/root" \
        "binaries/thinkingroot-agent-runtime-$TARGET"
      chmod +x "binaries/thinkingroot-agent-runtime-$TARGET"
      ;;
  esac

  TAURI_SIGNING_PRIVATE_KEY="$(cat "$SIGN_KEY_PATH")" \
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
    cargo tauri build --target "$TARGET"
)

# ── Collect Tauri output ─────────────────────────────────────────────────────

BUNDLE_DIR="$REPO_ROOT/apps/thinkingroot-desktop/src-tauri/target/$TARGET/release/bundle"
[ -d "$BUNDLE_DIR" ] || { echo "Bundle dir missing: $BUNDLE_DIR" >&2; exit 5; }

shopt -s nullglob
case "$PLATFORM" in
  macos-*)
    for f in "$BUNDLE_DIR/macos"/*.app.tar.gz "$BUNDLE_DIR/macos"/*.app.tar.gz.sig; do cp "$f" "$OUT_DIR/"; done
    for f in "$BUNDLE_DIR/dmg"/*.dmg; do cp "$f" "$OUT_DIR/"; done
    # Stable-named alias the landing page links to via /download/mac.
    if [ -n "$STABLE_DMG" ]; then
      DMG_SRC="$(ls "$OUT_DIR"/ThinkingRoot_*.dmg 2>/dev/null | head -1 || true)"
      [ -n "$DMG_SRC" ] && cp "$DMG_SRC" "$OUT_DIR/$STABLE_DMG"
    fi
    ;;
  linux-*)
    for f in "$BUNDLE_DIR/appimage"/*.AppImage "$BUNDLE_DIR/appimage"/*.AppImage.sig "$BUNDLE_DIR/deb"/*.deb; do cp "$f" "$OUT_DIR/"; done
    AI_SRC="$(ls "$OUT_DIR"/ThinkingRoot_*.AppImage 2>/dev/null | head -1 || true)"
    [ -n "$AI_SRC" ] && cp "$AI_SRC" "$OUT_DIR/ThinkingRoot-linux.AppImage"
    ;;
esac
shopt -u nullglob

# Copy the installer scripts so the GitHub Release ships them next to
# the binaries (the curl one-liner pulls them from the latest release).
cp "$REPO_ROOT/install.sh" "$OUT_DIR/"
cp "$REPO_ROOT/install.ps1" "$OUT_DIR/"

# ── checksums.txt ────────────────────────────────────────────────────────────

(
  cd "$OUT_DIR"
  : > checksums.txt
  for f in root-* ThinkingRoot* install.sh install.ps1; do
    [ -f "$f" ] || continue
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum "$f" >> checksums.txt
    else
      shasum -a 256 "$f" >> checksums.txt
    fi
  done
)
echo "=> checksums.txt:"
cat "$OUT_DIR/checksums.txt"

# ── latest.json (Tauri updater manifest) ─────────────────────────────────────

PUB_DATE="$(date -u +%Y-%m-%dT%H:%M:%S.000Z)"
BASE_URL="https://github.com/DevbyNaveen/releases/releases/download/$VERSION"

PLATFORM_KEY=""
SIG_FILE=""
URL_NAME=""
case "$PLATFORM" in
  macos-arm64)
    PLATFORM_KEY="darwin-aarch64"
    SIG_FILE="$(ls "$OUT_DIR"/ThinkingRoot_*_aarch64.app.tar.gz.sig 2>/dev/null | head -1 || true)"
    URL_NAME="$(ls "$OUT_DIR"/ThinkingRoot_*_aarch64.app.tar.gz 2>/dev/null | head -1 | xargs -n1 basename || true)"
    ;;
  macos-x64)
    PLATFORM_KEY="darwin-x86_64"
    SIG_FILE="$(ls "$OUT_DIR"/ThinkingRoot_*_x64.app.tar.gz.sig 2>/dev/null | head -1 || true)"
    URL_NAME="$(ls "$OUT_DIR"/ThinkingRoot_*_x64.app.tar.gz 2>/dev/null | head -1 | xargs -n1 basename || true)"
    ;;
  linux-amd64)
    PLATFORM_KEY="linux-x86_64"
    SIG_FILE="$(ls "$OUT_DIR"/ThinkingRoot_*_amd64.AppImage.sig 2>/dev/null | head -1 || true)"
    URL_NAME="$(ls "$OUT_DIR"/ThinkingRoot_*_amd64.AppImage 2>/dev/null | head -1 | xargs -n1 basename || true)"
    ;;
esac

if [ -n "$PLATFORM_KEY" ] && [ -f "$SIG_FILE" ] && [ -n "$URL_NAME" ]; then
  SIG_CONTENT="$(cat "$SIG_FILE")"
  cat > "$OUT_DIR/latest.json" <<EOF
{
  "version": "$VERSION_BARE",
  "notes": "ThinkingRoot $VERSION",
  "pub_date": "$PUB_DATE",
  "platforms": {
    "$PLATFORM_KEY": {
      "signature": "$SIG_CONTENT",
      "url": "$BASE_URL/$URL_NAME"
    }
  }
}
EOF
  echo "=> latest.json (single-platform; merge with other-host runs before publishing):"
  cat "$OUT_DIR/latest.json"
else
  echo "=> Skipping latest.json — no updater signature for this platform" >&2
fi

# ── Optional: gh release create ─────────────────────────────────────────────

if [ "$PUBLISH" -eq 1 ]; then
  echo
  echo "=> Publishing GitHub Release $VERSION on DevbyNaveen/releases"
  echo "   Source dir: $OUT_DIR"
  echo
  read -r -p "Proceed with gh release create? [y/N] " ans
  case "$ans" in
    y|Y|yes|YES)
      gh release create "$VERSION" \
        --repo DevbyNaveen/releases \
        --title "$VERSION" \
        --notes "ThinkingRoot $VERSION

Universal install:
\`\`\`
curl -fsSL https://thinkingroot.com/install.sh | sh
\`\`\`
or on Windows:
\`\`\`
irm https://thinkingroot.com/install.ps1 | iex
\`\`\`

Direct downloads also live under /download/{mac,linux,windows}." \
        "$OUT_DIR"/*
      ;;
    *)
      echo "Aborted."
      exit 0
      ;;
  esac
fi

echo
echo "=> Done. Artifacts:"
ls -la "$OUT_DIR"
echo
if [ "$PUBLISH" -eq 0 ]; then
  echo "Next:"
  echo "  - Repeat this script on each other host (Linux VM, Windows VM)."
  echo "  - Merge their dist/<version>/<platform>/ directories on this Mac."
  echo "  - Merge each platform's latest.json into one combined manifest."
  echo "  - Re-run with --publish to push the release."
fi
