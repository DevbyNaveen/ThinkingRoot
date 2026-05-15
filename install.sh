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
# Model bundle tag — versioned separately from the engine release so a
# 0.9.x → 0.9.y bugfix doesn't force re-downloading ~340 MB. Bump when
# embed / rerank ONNX content changes.
MODELS_TAG="${MODELS_TAG:-models-v1}"
MODELS_VERSION="${MODELS_TAG#models-}"
BINARY="root"
INSTALL_DIR="${INSTALL_DIR:-}"
# Skip switches — defaults give the "no-headache" install the user
# expects from a curl-one-liner. Each is opt-in to disable.
#   TR_SKIP_SERVICE=1   skip login-agent registration (`root service install`)
#   TR_SKIP_APP=1       skip desktop .app/.AppImage download
#   TR_SKIP_MODELS=1    skip the ~340 MB embed + rerank ONNX bundle
#                       (vector retrieval + cross-encoder rerank then
#                       fail with a `root doctor --fix` hint until the
#                       user re-runs the installer)
TR_SKIP_SERVICE="${TR_SKIP_SERVICE:-0}"
TR_SKIP_APP="${TR_SKIP_APP:-0}"
TR_SKIP_MODELS="${TR_SKIP_MODELS:-0}"
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

# ── Download helper (curl → wget fallback) ────────────────────────────────────
#
# Production users always go through HTTPS — curl's `--proto '=https'`
# and wget's `--https-only` flags enforce this and refuse plaintext
# URLs even if a malicious redirect tries to downgrade.  The smoke
# test at tests/install_sh_manifest_smoke.sh serves a fake release
# over http://localhost:<port> from Python's http.server, so when
# TR_TEST_BASE_URL is set we relax the protocol flag for the
# in-process test only.  Production paths are unaffected because
# TR_TEST_BASE_URL is never set in the wild.

download() {
  url="$1"; dest="$2"
  if is_cmd curl; then
    if [ -n "${TR_TEST_BASE_URL:-}" ]; then
      curl -fSL --progress-bar "$url" -o "$dest"
    else
      curl --tlsv1.2 --proto '=https' -fSL --progress-bar "$url" -o "$dest"
    fi
  elif is_cmd wget; then
    if [ -n "${TR_TEST_BASE_URL:-}" ]; then
      wget -O "$dest" "$url"
    else
      wget --https-only -O "$dest" "$url"
    fi
  else
    err "Neither curl nor wget found. Install one and retry."
  fi
}

download_quiet() {
  url="$1"; dest="$2"
  if is_cmd curl; then
    if [ -n "${TR_TEST_BASE_URL:-}" ]; then
      curl -fsSL "$url" -o "$dest"
    else
      curl --tlsv1.2 --proto '=https' -fsSL "$url" -o "$dest"
    fi
  elif is_cmd wget; then
    if [ -n "${TR_TEST_BASE_URL:-}" ]; then
      wget -q -O "$dest" "$url"
    else
      wget -q --https-only -O "$dest" "$url"
    fi
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

# ── Fetch latest version ──────────────────────────────────────────────────────

fetch_latest_version() {
  download_quiet \
    "https://api.github.com/repos/${RELEASES_REPO}/releases/latest" \
    /dev/stdout 2>/dev/null \
    | grep '"tag_name"' | cut -d'"' -f4
}

# ── BLAKE3 of a freshly-installed binary ─────────────────────────────────────
#
# Shells out to the just-installed `root hash-file <path>` (a hidden
# subcommand added in the same slice).  Pre-Task-9 the subcommand
# doesn't exist; we tolerate its absence by emitting an empty string,
# and Slice F's binary-corruption auto-repair will detect + repair the
# missing checksum on first daemon start.  Honest fallback — never
# fabricate a checksum.

blake3sum() {
  target="$1"
  if "${INSTALL_DIR}/${BINARY}" hash-file "$target" 2>/dev/null; then
    return 0
  fi
  echo ""
}

# ── Config-dir resolution ─────────────────────────────────────────────────────
#
# Mirrors `dirs::config_dir()` in Rust:
#   - Linux/BSD: $XDG_CONFIG_HOME, falling back to $HOME/.config
#   - macOS:     $HOME/Library/Application Support (XDG ignored)
#   - Windows:   %APPDATA% (handled by the Rust side; install.sh runs
#                 under WSL/git-bash on Windows, where this branch
#                 returns the XDG default — Slice A doesn't ship a
#                 native Windows installer yet)

resolve_config_dir() {
  if [ "$OS" = "macos" ]; then
    if [ -n "${XDG_CONFIG_HOME:-}" ]; then
      # Honour XDG_CONFIG_HOME on macOS when the user sets it
      # explicitly — matches how `tests/install_sh_manifest_smoke.sh`
      # (added in Task 11) overrides the path for sandboxed tests.
      echo "$XDG_CONFIG_HOME"
    else
      echo "$HOME/Library/Application Support"
    fi
  else
    echo "${XDG_CONFIG_HOME:-$HOME/.config}"
  fi
}

# ── Install manifest registration ─────────────────────────────────────────────
#
# Writes/updates the `cli-script` entry in
# ~/.config/thinkingroot/install-manifest.json (or platform equivalent).
# Pure shell — no external JSON library required.  The manifest format
# is owned by crates/thinkingroot-core/src/install_manifest.rs; if the
# schema bumps, this function must update in lockstep.
#
# This is a CLI-install registration only.  An existing `desktop-bundle`
# entry, if any, is preserved across re-installs by re-emitting it from
# its read-back state.

write_install_manifest() {
  bin_path="$1"
  version="$2"
  checksum="$3"

  config_dir="$(resolve_config_dir)/thinkingroot"
  mkdir -p "$config_dir"
  chmod 700 "$config_dir" 2>/dev/null || true
  manifest_path="${config_dir}/install-manifest.json"
  manifest_tmp="${manifest_path}.tr-installing"

  installed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

  # Preserve any existing desktop-bundle entry across re-installs.
  # Pure-shell JSON manipulation is fragile, so we extract the
  # raw block as text. If extraction fails for any reason, we warn
  # and write a CLI-only manifest — the desktop will re-register
  # on its next launch.
  desktop_entry=""
  if [ -f "$manifest_path" ] && grep -q '"id": "desktop-bundle"' "$manifest_path" 2>/dev/null; then
    desktop_entry="$(awk '
      /\{/ { brace++; if (brace==2) capturing=1 }
      capturing { buf = buf $0 "\n" }
      /\}/ { brace--; if (capturing && brace<2) { capturing=0; if (buf ~ /"desktop-bundle"/) { printf "%s", buf; exit } else { buf = "" } } }
    ' "$manifest_path")"
    # Strip trailing newline + comma if the original entry was
    # followed by another binaries[] element (we re-emit with a comma
    # separator below, so we don't want a double comma).
    desktop_entry="$(printf '%s' "$desktop_entry" | sed 's/,[[:space:]]*$//')"
  fi

  # Compose the new manifest.  binaries[] order:
  # cli-script first, desktop-bundle second (if present).
  cli_entry=$(cat <<EOF
    {
      "id": "cli-script",
      "path": "${bin_path}",
      "version": "${version}",
      "installed_at": "${installed_at}",
      "checksum_blake3": "${checksum}"
    }
EOF
)

  if [ -n "$desktop_entry" ]; then
    binaries_block="${cli_entry},
${desktop_entry}"
  else
    binaries_block="${cli_entry}"
  fi

  # `preferred` rule: keep an existing `preferred` if present and
  # valid; otherwise default to "cli-script".  Pure-shell extraction
  # is best-effort here — if the existing manifest is corrupt, the
  # Rust side will reject it and Slice F will rebuild.
  preferred="\"cli-script\""
  if [ -f "$manifest_path" ]; then
    existing_pref="$(grep -E '"preferred":' "$manifest_path" | head -1 \
                     | sed -E 's/.*"preferred":[[:space:]]*([^,}]+).*/\1/' \
                     | tr -d '[:space:]')"
    if [ -n "$existing_pref" ] && [ "$existing_pref" != "null" ]; then
      preferred="$existing_pref"
    fi
  fi

  # `setup_complete_at` preserved similarly.
  setup_complete="null"
  if [ -f "$manifest_path" ]; then
    existing_setup="$(grep -E '"setup_complete_at":' "$manifest_path" | head -1 \
                       | sed -E 's/.*"setup_complete_at":[[:space:]]*([^,}]+).*/\1/' \
                       | tr -d '[:space:]')"
    if [ -n "$existing_setup" ] && [ "$existing_setup" != "null" ]; then
      setup_complete="$existing_setup"
    fi
  fi

  # Track 32 — model_bundle. When `install_model_bundle` succeeded
  # earlier in main(), it set MODEL_BUNDLE_* globals; we emit them
  # as a nested JSON block. When it skipped or failed (and no
  # globals are set), we emit `null` — `root doctor` then surfaces
  # a `models.bundle_present` fail and the user can re-run install.sh.
  if [ -n "${MODEL_BUNDLE_VERSION:-}" ]; then
    model_bundle_block=$(cat <<MBEOF
{
    "version": "${MODEL_BUNDLE_VERSION}",
    "embed": {
      "onnx_path": "${MODEL_BUNDLE_EMBED_ONNX_PATH}",
      "tokenizer_path": "${MODEL_BUNDLE_EMBED_TOKENIZER_PATH}",
      "onnx_blake3": "${MODEL_BUNDLE_EMBED_ONNX_BLAKE3}",
      "tokenizer_blake3": "${MODEL_BUNDLE_EMBED_TOKENIZER_BLAKE3}"
    },
    "rerank": {
      "onnx_path": "${MODEL_BUNDLE_RERANK_ONNX_PATH}",
      "tokenizer_path": "${MODEL_BUNDLE_RERANK_TOKENIZER_PATH}",
      "onnx_blake3": "${MODEL_BUNDLE_RERANK_ONNX_BLAKE3}",
      "tokenizer_blake3": "${MODEL_BUNDLE_RERANK_TOKENIZER_BLAKE3}"
    },
    "registered_at": "${installed_at}"
  }
MBEOF
)
  else
    model_bundle_block="null"
  fi

  cat > "$manifest_tmp" <<EOF
{
  "schema_version": 1,
  "binaries": [
${binaries_block}
  ],
  "preferred": ${preferred},
  "setup_complete_at": ${setup_complete},
  "model_bundle": ${model_bundle_block}
}
EOF
  mv "$manifest_tmp" "$manifest_path" \
    || { rm -f "$manifest_tmp"; err "failed to write install manifest at ${manifest_path}"; }
  chmod 600 "$manifest_path" 2>/dev/null || true
  say "Registered install manifest at ${manifest_path}"
}

# ── Desktop app install (optional, best-effort) ──────────────────────────────
#
# Downloads the bundled desktop .app (macOS) or .AppImage (Linux) from
# the same release tag and installs into a location that the system
# launcher / Spotlight / .desktop-file scanner already indexes:
#   macOS: `/Applications/ThinkingRoot Desktop.app` (system-wide if
#          writable, else `$HOME/Applications/...`).
#   Linux: `$HOME/.local/share/thinkingroot/ThinkingRoot.AppImage` +
#          a `.desktop` entry under `$HOME/.local/share/applications`
#          so GNOME/KDE launchers list it.
#
# Honest skip semantics: if the desktop asset is not in this release
# (some tagged releases ship CLI-only), we say so and continue — we
# do NOT fabricate a working install. The CLI is fully functional
# on its own; the desktop is a thin GUI on top.

install_desktop_macos() {
  asset_url="$1"
  asset_path="$2"

  if ! download_quiet "$asset_url" "$asset_path" 2>/dev/null; then
    say_dim "Desktop bundle not in this release — skipping (CLI is fully functional)."
    return 0
  fi
  case "$asset_path" in
    *.tar.gz|*.tgz) ;;
    *)
      warn "unexpected desktop asset extension on $asset_path — skipping"
      return 0
      ;;
  esac

  # Pick target dir: /Applications if writable (system-wide), else
  # ~/Applications (per-user; macOS Finder + Spotlight both index it).
  if [ -w /Applications ]; then
    APP_TARGET_DIR="/Applications"
  else
    APP_TARGET_DIR="$HOME/Applications"
    mkdir -p "$APP_TARGET_DIR" || { warn "cannot create $APP_TARGET_DIR — skipping desktop install"; return 0; }
  fi
  STAGE="$(mktemp -d)"
  tar -xzf "$asset_path" -C "$STAGE" || { rm -rf "$STAGE"; warn "desktop bundle extract failed — skipping"; return 0; }
  # The tarball is expected to contain exactly one top-level `*.app`
  # directory. Don't guess if there are zero or many.
  APP_PATH="$(find "$STAGE" -maxdepth 2 -type d -name '*.app' | head -1)"
  if [ -z "$APP_PATH" ]; then
    rm -rf "$STAGE"
    warn "desktop bundle did not contain a .app — skipping"
    return 0
  fi
  APP_NAME="$(basename "$APP_PATH")"
  FINAL_PATH="$APP_TARGET_DIR/$APP_NAME"
  # Replace any prior install in place. rm -rf is scoped to the
  # specific path we just resolved; no glob expansion hazards.
  rm -rf "$FINAL_PATH"
  mv "$APP_PATH" "$FINAL_PATH" || { rm -rf "$STAGE"; warn "failed to move .app to $FINAL_PATH"; return 0; }
  rm -rf "$STAGE"
  # Strip the quarantine xattr so first launch from Finder doesn't
  # trip Gatekeeper's "downloaded from internet" prompt. The bundle
  # arrived via curl which doesn't set quarantine, but a defensive
  # clear is safe and matches `xattr -dr` patterns used by signed
  # vendors during postinstall.
  xattr -dr com.apple.quarantine "$FINAL_PATH" 2>/dev/null || true
  say "Installed: $FINAL_PATH"
}

install_desktop_linux() {
  asset_url="$1"
  asset_path="$2"

  if ! download_quiet "$asset_url" "$asset_path" 2>/dev/null; then
    say_dim "Desktop bundle not in this release — skipping (CLI is fully functional)."
    return 0
  fi

  APP_TARGET_DIR="$HOME/.local/share/thinkingroot"
  mkdir -p "$APP_TARGET_DIR" || { warn "cannot create $APP_TARGET_DIR — skipping desktop install"; return 0; }
  FINAL_PATH="$APP_TARGET_DIR/ThinkingRoot.AppImage"
  mv "$asset_path" "$FINAL_PATH" || { warn "failed to move AppImage to $FINAL_PATH"; return 0; }
  chmod +x "$FINAL_PATH"

  # Write a `.desktop` entry so the GNOME/KDE/XFCE app menu lists
  # it. Idempotent — same path on every re-install.
  DESKTOP_DIR="$HOME/.local/share/applications"
  mkdir -p "$DESKTOP_DIR" || true
  cat > "$DESKTOP_DIR/thinkingroot-desktop.desktop" <<EOF
[Desktop Entry]
Name=ThinkingRoot Desktop
Comment=Local-first AI memory you can audit
Exec=$FINAL_PATH
Icon=thinkingroot
Terminal=false
Type=Application
Categories=Development;Utility;
EOF
  say "Installed: $FINAL_PATH"
  say_dim "Launcher entry: $DESKTOP_DIR/thinkingroot-desktop.desktop"
}

# ── Login-agent registration ─────────────────────────────────────────────────
#
# Calls `root service install` to write the OS-native login agent
# (launchd plist / systemd --user unit / Task Scheduler entry — see
# crates/thinkingroot-cli/src/service.rs). Idempotent. Best-effort:
# a failure here does NOT abort the install; the CLI still works,
# the user just won't get the daemon-at-login behaviour.

register_login_agent() {
  bin_path="$1"
  if "$bin_path" service install; then
    return 0
  fi
  warn "login-agent registration failed — run \`root service install\` manually if you want auto-start."
}

# ── Model bundle dir ─────────────────────────────────────────────────────────
#
# Canonical on-disk location for the embed + rerank ONNX bundle.
# Matches `crate::ort_session::default_model_bundle_dir()` in Rust so
# the engine finds whatever install.sh stages here without further
# coordination.

model_bundle_dir() {
  if [ "$(uname -s)" = "Darwin" ]; then
    echo "${HOME}/Library/Caches/thinkingroot/models"
  else
    echo "${HOME}/.cache/thinkingroot/models"
  fi
}

# ── Install model bundle (Track 32) ──────────────────────────────────────────
#
# Downloads the four-file model bundle (embed.onnx, embed.tokenizer.json,
# rerank.onnx, rerank.tokenizer.json) from
# `https://github.com/${RELEASES_REPO}/releases/download/${MODELS_TAG}/`
# and verifies each against the pinned SHA-256 from `SHA256SUMS` at the
# same URL.
#
# On success, sets nine MODEL_BUNDLE_* shell globals that
# `write_install_manifest` reads to populate the install-manifest's
# `model_bundle` field:
#
#   MODEL_BUNDLE_VERSION
#   MODEL_BUNDLE_EMBED_ONNX_PATH       MODEL_BUNDLE_EMBED_ONNX_BLAKE3
#   MODEL_BUNDLE_EMBED_TOKENIZER_PATH  MODEL_BUNDLE_EMBED_TOKENIZER_BLAKE3
#   MODEL_BUNDLE_RERANK_ONNX_PATH      MODEL_BUNDLE_RERANK_ONNX_BLAKE3
#   MODEL_BUNDLE_RERANK_TOKENIZER_PATH MODEL_BUNDLE_RERANK_TOKENIZER_BLAKE3
#
# Returns 0 on success, 1 on recoverable failure (no SUMS file yet,
# download error). Callers warn-and-continue on 1 so a transient
# bundle outage doesn't block the binary install.
#
# Idempotent: already-cached files with matching SHA-256 skip the
# network fetch. Corrupt cached files surface a hard error (refuse
# to install a tampered bundle).

install_model_bundle() {
  bin_path="$1"

  bundle_dir="$(model_bundle_dir)"
  mkdir -p "$bundle_dir" || { warn "Cannot create ${bundle_dir}"; return 1; }

  models_base="https://github.com/${RELEASES_REPO}/releases/download/${MODELS_TAG}"
  # Test override mirrors the existing BASE_URL convention.
  models_base="${TR_TEST_MODELS_BASE_URL:-$models_base}"

  sums_path="${bundle_dir}/SHA256SUMS"
  # Always re-fetch SHA256SUMS — it's tiny (~600 bytes) and tells us
  # whether the bundle tag has been republished.
  if ! download_quiet "${models_base}/SHA256SUMS" "$sums_path" 2>/dev/null; then
    warn "model bundle SHA256SUMS not reachable at ${models_base} — skipping model download"
    warn "vector retrieval + cross-encoder rerank will fail until the user re-runs install.sh"
    return 1
  fi

  for f in embed.onnx embed.tokenizer.json rerank.onnx rerank.tokenizer.json; do
    dest="${bundle_dir}/${f}"
    expected="$(awk -v f="$f" '$2 == f || $2 == "./" f { print $1; exit }' "$sums_path")"
    if [ -z "$expected" ]; then
      warn "no SHA256 entry for ${f} in SHA256SUMS — refusing to install ${f}"
      return 1
    fi

    if [ -f "$dest" ]; then
      cached_sum="$(sha256 "$dest")"
      if [ "$cached_sum" = "$expected" ]; then
        say_dim "${f} already cached"
        continue
      fi
      say_dim "${f} cache stale, refreshing"
      rm -f "$dest"
    fi

    say "Downloading ${f}..."
    download "${models_base}/${f}" "$dest" \
      || { warn "${f} download failed"; return 1; }

    actual="$(sha256 "$dest")"
    if [ "$actual" != "$expected" ]; then
      rm -f "$dest"
      err "SHA256 mismatch for ${f}: expected ${expected}, got ${actual}"
    fi
  done

  # Compute BLAKE3 anchors via the freshly-installed `root hash-file`
  # so the install-manifest records tamper-evident hashes that
  # `root doctor models.bundle_present` re-verifies on every run.
  embed_onnx_b3="$("${bin_path}" hash-file "${bundle_dir}/embed.onnx" 2>/dev/null)"
  embed_tok_b3="$("${bin_path}" hash-file "${bundle_dir}/embed.tokenizer.json" 2>/dev/null)"
  rerank_onnx_b3="$("${bin_path}" hash-file "${bundle_dir}/rerank.onnx" 2>/dev/null)"
  rerank_tok_b3="$("${bin_path}" hash-file "${bundle_dir}/rerank.tokenizer.json" 2>/dev/null)"

  if [ -z "$embed_onnx_b3" ] || [ -z "$rerank_onnx_b3" ]; then
    warn "BLAKE3 computation failed — model_bundle field will record empty hashes"
    warn "this disables tamper-detection but the bundle still works"
  fi

  MODEL_BUNDLE_VERSION="$MODELS_VERSION"
  MODEL_BUNDLE_EMBED_ONNX_PATH="${bundle_dir}/embed.onnx"
  MODEL_BUNDLE_EMBED_TOKENIZER_PATH="${bundle_dir}/embed.tokenizer.json"
  MODEL_BUNDLE_RERANK_ONNX_PATH="${bundle_dir}/rerank.onnx"
  MODEL_BUNDLE_RERANK_TOKENIZER_PATH="${bundle_dir}/rerank.tokenizer.json"
  MODEL_BUNDLE_EMBED_ONNX_BLAKE3="$embed_onnx_b3"
  MODEL_BUNDLE_EMBED_TOKENIZER_BLAKE3="$embed_tok_b3"
  MODEL_BUNDLE_RERANK_ONNX_BLAKE3="$rerank_onnx_b3"
  MODEL_BUNDLE_RERANK_TOKENIZER_BLAKE3="$rerank_tok_b3"

  say "Model bundle ${MODELS_TAG} verified."
  return 0
}

# ── checksums.txt caching ────────────────────────────────────────────────────
#
# Caches the verified checksums.txt to the config dir.  Slice F's
# corrupt-manifest disk-scan recovery uses this when it can't trust
# the manifest's own checksum (because the manifest is the corrupt
# thing).

cache_checksums_file() {
  src="$1"
  config_dir="$(resolve_config_dir)/thinkingroot"
  mkdir -p "$config_dir"
  dest="${config_dir}/checksums-cache.txt"
  dest_tmp="${dest}.tr-installing"
  cp "$src" "$dest_tmp" || err "failed to cache checksums.txt to ${dest_tmp}"
  mv "$dest_tmp" "$dest"
  chmod 600 "$dest" 2>/dev/null || true
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
  # Test-only override: the harness at tests/install_sh_manifest_smoke.sh
  # sets TR_TEST_BASE_URL to a local http.server URL.  Production users
  # never set this.
  BASE_URL="${TR_TEST_BASE_URL:-$BASE_URL}"
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

  # ── Install ONNX model bundle (Track 32) ────────────────────────────────
  # Downloads embed + rerank ONNX + tokenizer files BEFORE writing the
  # install manifest so the manifest captures their BLAKE3 anchors.
  # Loud-fail on a tampered download; warn-and-continue on a missing
  # SHA256SUMS file (network blip / model tag not yet published — the
  # CLI is fully functional sans bundle, retrieval just degrades).
  if [ "$TR_SKIP_MODELS" = "1" ]; then
    say_dim "Skipping model bundle download (TR_SKIP_MODELS=1)"
    say_dim "→ vector retrieval + cross-encoder rerank will fail until re-installed"
  else
    install_model_bundle "${INSTALL_DIR}/${BINARY}" \
      || say_dim "→ run \`root doctor --fix models.bundle_present\` (or re-run install.sh) to retry"
  fi

  # ── Register install in coordinating manifest ───────────────────────────────
  # Cache the verified checksums.txt for Slice F recovery; register
  # the binary + model bundle in the install manifest so CLI + desktop
  # + doctor agree on what's canonical.  See:
  #   docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md
  cache_checksums_file "$CHECKSUMS_PATH"
  manifest_checksum="$(blake3sum "${INSTALL_DIR}/${BINARY}")"
  write_install_manifest "${INSTALL_DIR}/${BINARY}" "$VERSION" "$manifest_checksum"

  # PATH hint
  case "$INSTALL_DIR" in
    "$HOME/.local/bin")
      case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) say "Add to PATH: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
      esac
      ;;
  esac

  # ── Install desktop app (best-effort) ─────────────────────────────────────
  if [ "$TR_SKIP_APP" = "1" ]; then
    say_dim "Skipping desktop app install (TR_SKIP_APP=1)"
  else
    case "$OS" in
      macos)
        # Tauri 2 emits `<productName>_<version>_<arch>.app.tar.gz`
        # for the updater channel. productName="ThinkingRoot" (set
        # in tauri.conf.json with no spaces to keep URLs clean).
        # Arch label matches Tauri's: aarch64 or x64.
        case "$ARCH" in
          arm64) TR_TAURI_ARCH="aarch64" ;;
          *)     TR_TAURI_ARCH="x64" ;;
        esac
        DESKTOP_ASSET="ThinkingRoot_${VERSION}_${TR_TAURI_ARCH}.app.tar.gz"
        install_desktop_macos \
          "${BASE_URL}/${DESKTOP_ASSET}" \
          "${TMP_DIR}/${DESKTOP_ASSET}"
        ;;
      linux)
        case "$ARCH" in
          arm64) TR_TAURI_ARCH="aarch64" ;;
          *)     TR_TAURI_ARCH="amd64" ;;
        esac
        DESKTOP_ASSET="ThinkingRoot_${VERSION}_${TR_TAURI_ARCH}.AppImage"
        install_desktop_linux \
          "${BASE_URL}/${DESKTOP_ASSET}" \
          "${TMP_DIR}/${DESKTOP_ASSET}"
        ;;
    esac
  fi

  # ── Register login agent ──────────────────────────────────────────────────
  if [ "$TR_SKIP_SERVICE" = "1" ]; then
    say_dim "Skipping login-agent registration (TR_SKIP_SERVICE=1)"
  else
    register_login_agent "${INSTALL_DIR}/${BINARY}"
  fi

  printf '\n'
  say "Done!"
  config_dir_check="$(resolve_config_dir)/thinkingroot"
  if [ -f "${config_dir_check}/install-manifest.json" ]; then
    say_dim "Install manifest: ${config_dir_check}/install-manifest.json"
  fi
  if "${INSTALL_DIR}/${BINARY}" doctor --quiet 2>/dev/null; then
    say "Doctor: all checks pass."
  else
    say_dim "Doctor flagged setup gaps; run \`root doctor\` for details or \`root setup\` to repair."
  fi
  "${INSTALL_DIR}/${BINARY}" --version || true
  printf '\n'
  printf '    Get started:\n'
  printf '      root setup            # interactive credentials wizard\n'
  printf '      root compile .        # compile your first knowledge base\n'
  printf '      root ask "what does this codebase do?"\n'
  printf '\n'
  printf '    Service management:\n'
  printf '      root service install     # register login agent (already done)\n'
  printf '      root service uninstall   # remove login agent\n'
  printf '\n'
}

main "$@"
