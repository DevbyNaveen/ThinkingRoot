# Distribution Channels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish ThinkingRoot across 4 distribution channels — curl install, PyPI, Homebrew tap, and crates.io — so any developer can install the tool or SDK with a single command.

**Architecture:** GitHub Releases is the single source of truth for all binaries and checksums. The curl script, Homebrew formula, and crates.io all pull from the same versioned release assets. PyPI publishes the Python SDK as a maturin-compiled wheel via a separate workflow. All channels trigger automatically on a `v*` git tag push.

**Tech Stack:** GitHub Actions, maturin + PyO3/maturin-action, pypa/gh-action-pypi-publish (OIDC), mislav/bump-homebrew-formula-action, cargo-release, bash (POSIX-compatible install script)

---

## File Structure

### Files created or modified in this repo (`thinkingroot/thinkingroot`)

| File | Action | Purpose |
|---|---|---|
| `.github/workflows/release.yml` | Modify | Add SHA256 checksum generation + upload; remove `draft: true` |
| `.github/workflows/publish-pypi.yml` | Create | Build multi-platform wheels + publish to PyPI via OIDC |
| `.github/workflows/publish-crates.yml` | Create | Publish all 11 library crates to crates.io in dependency order |
| `install.sh` | Create | curl one-liner install script for Linux + macOS |
| `Cargo.toml` | Modify | Add `version` to internal workspace deps; add `keywords`/`categories` |
| `crates/thinkingroot-core/Cargo.toml` | Modify | Add `keywords`, `categories`, `repository` inheritance |
| `crates/thinkingroot-graph/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-parse/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-extract/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-link/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-compile/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-verify/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-serve/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-safety/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-branch/Cargo.toml` | Modify | Same |
| `crates/thinkingroot-cli/Cargo.toml` | Modify | Same + `publish = false` for cdylib |
| `thinkingroot-python/Cargo.toml` | Modify | Add `publish = false` (cdylib cannot go to crates.io) |
| `thinkingroot-python/pyproject.toml` | Modify | Switch `version` to `dynamic = ["version"]` |

### New repo created (`thinkingroot/homebrew-thinkingroot`)

| File | Purpose |
|---|---|
| `Formula/root.rb` | Homebrew formula — downloads pre-built binary from GitHub Releases |
| `.github/workflows/bump.yml` | Auto-updates formula URL + SHA256 on each new release |
| `README.md` | Install instructions |

---

## Pre-flight: One-Time Account Setup

These are done once by the repo owner before running any task. They cannot be automated.

### PyPI OIDC Trusted Publisher

- [ ] Create account at https://pypi.org if you don't have one
- [ ] Go to https://pypi.org/manage/account/publishing/
- [ ] Click "Add a new pending publisher"
- [ ] Fill in:
  - PyPI Project Name: `thinkingroot`
  - Owner: `thinkingroot`
  - Repository name: `thinkingroot`
  - Workflow filename: `publish-pypi.yml`
  - Environment name: `release`
- [ ] Click "Add"

### crates.io Token

- [ ] Go to https://crates.io/settings/tokens
- [ ] Click "New Token", name it `github-actions`, scope: publish-new + publish-update
- [ ] Copy the token value
- [ ] In GitHub repo → Settings → Secrets → Actions → New repository secret
  - Name: `CARGO_REGISTRY_TOKEN`
  - Value: (paste token)

### Homebrew PAT

- [ ] Go to https://github.com/settings/tokens → Generate new token (classic)
- [ ] Scopes: `repo` + `workflow`
- [ ] Name: `HOMEBREW_BUMP_PAT`
- [ ] Copy token
- [ ] In GitHub repo → Settings → Secrets → Actions → New repository secret
  - Name: `HOMEBREW_PR_PAT`
  - Value: (paste token)

### GitHub Environment for PyPI

- [ ] In GitHub repo → Settings → Environments → New environment
- [ ] Name: `release`
- [ ] No protection rules needed for now

---

## Task 1: Harden GitHub Release (checksums + auto-publish)

The existing `release.yml` creates a draft release and does not generate checksums. Homebrew requires SHA256; the install script requires checksums.txt; auto-publish means users can install immediately after a tag push.

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Read the current release.yml**

```bash
cat .github/workflows/release.yml
```

- [ ] **Step 2: Replace release.yml with the hardened version**

Replace the entire file with:

```yaml
name: Release

on:
  push:
    tags:
      - 'v[0-9]+.[0-9]+.[0-9]+'

env:
  CARGO_TERM_COLOR: always

jobs:
  # ── Build CLI binaries for all platforms ──────────────────────────────────
  build:
    name: Build (${{ matrix.target }})
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            artifact: root-linux-amd64
          - os: ubuntu-latest
            target: aarch64-unknown-linux-gnu
            artifact: root-linux-arm64
            cross: true
          - os: macos-latest
            target: x86_64-apple-darwin
            artifact: root-macos-amd64
          - os: macos-latest
            target: aarch64-apple-darwin
            artifact: root-macos-arm64
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            artifact: root-windows-amd64.exe

    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      - uses: Swatinem/rust-cache@v2

      - name: Install cross (Linux ARM64)
        if: matrix.cross
        run: cargo install cross --git https://github.com/cross-rs/cross

      - name: Build (native)
        if: '!matrix.cross'
        run: cargo build --release --no-default-features --target ${{ matrix.target }} -p thinkingroot-cli

      - name: Build (cross)
        if: matrix.cross
        run: cross build --release --no-default-features --target ${{ matrix.target }} -p thinkingroot-cli

      - name: Rename binary
        shell: bash
        run: |
          BIN=target/${{ matrix.target }}/release/root
          [ -f "${BIN}.exe" ] && BIN="${BIN}.exe"
          cp "$BIN" ${{ matrix.artifact }}

      - uses: actions/upload-artifact@v4
        with:
          name: ${{ matrix.artifact }}
          path: ${{ matrix.artifact }}

  # ── Generate checksums and create GitHub Release ───────────────────────────
  release:
    name: GitHub Release
    runs-on: ubuntu-latest
    needs: build
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4

      - uses: actions/download-artifact@v4
        with:
          path: artifacts/
          merge-multiple: true

      - name: Generate checksums.txt
        run: |
          cd artifacts
          sha256sum root-linux-amd64 \
                    root-linux-arm64 \
                    root-macos-amd64 \
                    root-macos-arm64 \
                    root-windows-amd64.exe \
            > checksums.txt
          cat checksums.txt

      - name: Create release
        uses: softprops/action-gh-release@v2
        with:
          files: |
            artifacts/root-linux-amd64
            artifacts/root-linux-arm64
            artifacts/root-macos-amd64
            artifacts/root-macos-arm64
            artifacts/root-windows-amd64.exe
            artifacts/checksums.txt
          generate_release_notes: true
          draft: false
          fail_on_unmatched_files: true

  # ── Trigger Homebrew formula bump ─────────────────────────────────────────
  homebrew-bump:
    name: Bump Homebrew formula
    runs-on: ubuntu-latest
    needs: release
    steps:
      - uses: mislav/bump-homebrew-formula-action@v3
        with:
          formula-name: root
          homebrew-tap: thinkingroot/homebrew-thinkingroot
          tag-name: ${{ github.ref_name }}
          download-url: https://github.com/thinkingroot/thinkingroot/releases/download/${{ github.ref_name }}/root-macos-arm64
        env:
          COMMITTER_TOKEN: ${{ secrets.HOMEBREW_PR_PAT }}
```

- [ ] **Step 3: Verify the YAML is valid**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))" && echo "YAML valid"
```

Expected: `YAML valid`

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): add checksums.txt, remove draft, trigger homebrew bump"
```

---

## Task 2: curl install script

Every developer who reads your README will use this. It must work on macOS (arm64 + x86_64) and Linux (amd64 + arm64) with zero dependencies beyond `curl` or `wget`.

**Files:**
- Create: `install.sh`

- [ ] **Step 1: Create install.sh**

```bash
cat > install.sh << 'INSTALLEOF'
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
INSTALLEOF
chmod +x install.sh
```

- [ ] **Step 2: Verify the script is POSIX-valid with shellcheck**

```bash
# Install shellcheck if not present: brew install shellcheck / apt install shellcheck
shellcheck -s sh install.sh
```

Expected: No output (no errors).

If shellcheck is not installed, skip — it will be caught in CI.

- [ ] **Step 3: Smoke test the OS/arch detection locally**

```bash
# Source just the detection functions and run them
sh -c '
  detect_os() {
    case "$(uname -s)" in
      Linux)  echo "linux"  ;;
      Darwin) echo "macos"  ;;
    esac
  }
  detect_arch() {
    arch="$(uname -m)"
    case "$arch" in
      x86_64|amd64) echo "amd64" ;;
      aarch64|arm64) echo "arm64" ;;
    esac
  }
  echo "OS:   $(detect_os)"
  echo "ARCH: $(detect_arch)"
'
```

Expected on an Apple Silicon Mac:
```
OS:   macos
ARCH: arm64
```

- [ ] **Step 4: Commit**

```bash
git add install.sh
git commit -m "feat(dist): add curl install script with SHA256 verification"
```

---

## Task 3: PyPI — `pip install thinkingroot`

Publishes the Python SDK as pre-compiled wheels for all platforms. Uses OIDC Trusted Publishing — no long-lived tokens stored anywhere.

**Files:**
- Modify: `thinkingroot-python/pyproject.toml`
- Create: `.github/workflows/publish-pypi.yml`

- [ ] **Step 1: Update pyproject.toml to use dynamic version from Cargo.toml**

Replace the `version = "0.1.0"` line in `thinkingroot-python/pyproject.toml`:

```toml
[build-system]
requires = ["maturin>=1.0,<2.0"]
build-backend = "maturin"

[project]
name = "thinkingroot"
dynamic = ["version"]
description = "Knowledge compiler for AI agents — Python SDK"
readme = "../README.md"
requires-python = ">=3.9"
license = { text = "MIT OR Apache-2.0" }
authors = [{ name = "Naveen", email = "naveen@thinkingroot.dev" }]
keywords = ["knowledge-graph", "ai", "llm", "mcp", "rag"]
classifiers = [
  "Development Status :: 4 - Beta",
  "Intended Audience :: Developers",
  "License :: OSI Approved :: MIT License",
  "Programming Language :: Python :: 3",
  "Programming Language :: Python :: 3.9",
  "Programming Language :: Python :: 3.10",
  "Programming Language :: Python :: 3.11",
  "Programming Language :: Python :: 3.12",
  "Programming Language :: Python :: 3.13",
  "Programming Language :: Rust",
  "Topic :: Scientific/Engineering :: Artificial Intelligence",
]
dependencies = ["httpx>=0.27"]

[project.urls]
Homepage = "https://thinkingroot.dev"
Repository = "https://github.com/thinkingroot/thinkingroot"
Documentation = "https://docs.thinkingroot.dev"

[project.optional-dependencies]
dev = ["pytest", "pytest-asyncio"]

[tool.maturin]
features = ["pyo3/extension-module"]
python-source = "python"
module-name = "thinkingroot._thinkingroot"
```

- [ ] **Step 2: Verify maturin still builds locally**

```bash
cd thinkingroot-python
python3 -m venv .venv-test
source .venv-test/bin/activate
pip install maturin
maturin develop --release --no-default-features
python -c "import thinkingroot; print('import OK')"
deactivate
rm -rf .venv-test
cd ..
```

Expected: `import OK`

- [ ] **Step 3: Create .github/workflows/publish-pypi.yml**

```yaml
name: Publish Python SDK to PyPI

on:
  push:
    tags:
      - 'v[0-9]+.[0-9]+.[0-9]+'

env:
  CARGO_TERM_COLOR: always

jobs:
  # ── Build wheels for all platforms ──────────────────────────────────────────
  build-wheels:
    name: Build wheel (${{ matrix.target }})
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          # Linux x86_64 — manylinux for broad compatibility
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            manylinux: 2_28
          # Linux aarch64 — cross-compiled in manylinux container
          - os: ubuntu-latest
            target: aarch64-unknown-linux-gnu
            manylinux: 2_28
          # macOS Intel
          - os: macos-13
            target: x86_64-apple-darwin
          # macOS Apple Silicon
          - os: macos-latest
            target: aarch64-apple-darwin
          # Windows x86_64
          - os: windows-latest
            target: x86_64-pc-windows-msvc

    steps:
      - uses: actions/checkout@v4

      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: thinkingroot-python -> target

      - name: Build wheels
        uses: PyO3/maturin-action@v1
        with:
          target: ${{ matrix.target }}
          manylinux: ${{ matrix.manylinux || 'auto' }}
          working-directory: thinkingroot-python
          args: --release --no-default-features --out dist
          sccache: true

      - uses: actions/upload-artifact@v4
        with:
          name: wheels-${{ matrix.target }}
          path: thinkingroot-python/dist

  # ── Build pure-Python sdist (source distribution) ───────────────────────────
  build-sdist:
    name: Build sdist
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Build sdist
        uses: PyO3/maturin-action@v1
        with:
          command: sdist
          working-directory: thinkingroot-python
          args: --out dist

      - uses: actions/upload-artifact@v4
        with:
          name: wheels-sdist
          path: thinkingroot-python/dist

  # ── Publish to PyPI via OIDC Trusted Publishing ──────────────────────────────
  publish-pypi:
    name: Publish to PyPI
    runs-on: ubuntu-latest
    needs: [build-wheels, build-sdist]
    environment: release
    permissions:
      id-token: write   # Required for OIDC — no token secret needed
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: dist
          pattern: wheels-*
          merge-multiple: true

      - name: List wheels to be published
        run: ls -lh dist/

      - name: Publish to PyPI
        uses: pypa/gh-action-pypi-publish@release/v1
        with:
          packages-dir: dist/
          skip-existing: true
```

- [ ] **Step 4: Verify YAML is valid**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/publish-pypi.yml'))" && echo "YAML valid"
```

Expected: `YAML valid`

- [ ] **Step 5: Commit**

```bash
git add thinkingroot-python/pyproject.toml .github/workflows/publish-pypi.yml
git commit -m "feat(dist): add PyPI publish workflow with OIDC trusted publishing"
```

---

## Task 4: Homebrew Tap — `brew install thinkingroot/tap/root`

Requires creating a **new separate GitHub repository** named `thinkingroot/homebrew-thinkingroot`. Homebrew taps must follow the `homebrew-*` naming convention.

**This task has two parts: (A) set up the new tap repo, (B) wire it into the release workflow.**

### Part A — Create the tap repository

- [ ] **Step 1: Create the new repo on GitHub**

```bash
gh repo create thinkingroot/homebrew-thinkingroot \
  --public \
  --description "Homebrew tap for ThinkingRoot" \
  --clone
cd homebrew-thinkingroot
mkdir -p Formula .github/workflows
```

- [ ] **Step 2: Create Formula/root.rb**

Replace `SHA256_MACOS_ARM64`, `SHA256_MACOS_AMD64`, `SHA256_LINUX_AMD64`, `SHA256_LINUX_ARM64` with real checksums from the first real release. The bump workflow will keep them updated automatically after that.

```ruby
# Formula/root.rb
class Root < Formula
  desc "Knowledge compiler for AI agents — parse, extract, link, compile, verify, serve"
  homepage "https://thinkingroot.dev"
  license "MIT OR Apache-2.0"
  version "0.1.0"

  on_macos do
    on_arm64 do
      url "https://github.com/thinkingroot/thinkingroot/releases/download/v#{version}/root-macos-arm64"
      sha256 "REPLACE_WITH_REAL_SHA256_AFTER_FIRST_RELEASE"
    end
    on_intel do
      url "https://github.com/thinkingroot/thinkingroot/releases/download/v#{version}/root-macos-amd64"
      sha256 "REPLACE_WITH_REAL_SHA256_AFTER_FIRST_RELEASE"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/thinkingroot/thinkingroot/releases/download/v#{version}/root-linux-amd64"
      sha256 "REPLACE_WITH_REAL_SHA256_AFTER_FIRST_RELEASE"
    end
    on_arm64 do
      url "https://github.com/thinkingroot/thinkingroot/releases/download/v#{version}/root-linux-arm64"
      sha256 "REPLACE_WITH_REAL_SHA256_AFTER_FIRST_RELEASE"
    end
  end

  def install
    bin.install stable.url.split("/").last => "root"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/root --version")
  end
end
```

- [ ] **Step 3: Create .github/workflows/bump.yml in the tap repo**

```yaml
name: Bump formula on new release

on:
  workflow_dispatch:
    inputs:
      tag_name:
        description: "Release tag (e.g. v0.2.0)"
        required: true
        type: string
  repository_dispatch:
    types: [new-release]

jobs:
  bump:
    runs-on: ubuntu-latest
    steps:
      - name: Bump Homebrew formula
        uses: mislav/bump-homebrew-formula-action@v3
        with:
          formula-name: root
          homebrew-tap: thinkingroot/homebrew-thinkingroot
          tag-name: ${{ github.event.inputs.tag_name || github.event.client_payload.tag }}
          download-url: https://github.com/thinkingroot/thinkingroot/releases/download/${{ github.event.inputs.tag_name || github.event.client_payload.tag }}/root-macos-arm64
        env:
          COMMITTER_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **Step 4: Create README.md in the tap repo**

```markdown
# homebrew-thinkingroot

Homebrew tap for [ThinkingRoot](https://thinkingroot.dev) — a knowledge compiler for AI agents.

## Install

```bash
brew install thinkingroot/tap/root
```

This installs the `root` CLI. Verify:

```bash
root --version
root --help
```

## Update

```bash
brew update && brew upgrade root
```

## Manual tap

```bash
brew tap thinkingroot/tap
brew install root
```
```

- [ ] **Step 5: Commit and push the tap repo**

```bash
git add Formula/root.rb .github/workflows/bump.yml README.md
git commit -m "feat: initial Homebrew tap for ThinkingRoot root CLI"
git push -u origin main
```

### Part B — Get real SHA256 values for the formula

After the first real `v*` tag is pushed to the main repo and the GitHub Release is published:

- [ ] **Step 6: Download each binary and compute its SHA256**

```bash
VERSION="v0.1.0"
BASE="https://github.com/thinkingroot/thinkingroot/releases/download/${VERSION}"

for asset in root-macos-arm64 root-macos-amd64 root-linux-amd64 root-linux-arm64; do
  curl -fsSL "${BASE}/${asset}" -o "/tmp/${asset}"
  echo "${asset}:"
  shasum -a 256 "/tmp/${asset}" | cut -d' ' -f1
  rm "/tmp/${asset}"
done
```

- [ ] **Step 7: Replace the placeholder SHA256 values in Formula/root.rb**

Edit `Formula/root.rb` and replace each `REPLACE_WITH_REAL_SHA256_AFTER_FIRST_RELEASE` with the actual hash from Step 6.

- [ ] **Step 8: Commit the updated formula**

```bash
cd homebrew-thinkingroot
git add Formula/root.rb
git commit -m "root 0.1.0"
git push
```

- [ ] **Step 9: Test the formula locally**

```bash
brew tap thinkingroot/tap
brew install thinkingroot/tap/root
root --version
brew audit --strict thinkingroot/tap/root
```

Expected: `root --version` prints the version; `brew audit` reports no errors.

---

## Task 5: crates.io — `cargo install thinkingroot-cli`

Publishes all 11 Rust library crates plus the CLI to crates.io. Must be published in strict dependency order (leaf crates first). Requires adding `version` to internal workspace dependencies and `publish = false` to the Python cdylib.

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: all 11 crate `Cargo.toml` files (add `keywords`, `categories`, `repository.workspace`)
- Modify: `thinkingroot-python/Cargo.toml` (add `publish = false`)
- Create: `.github/workflows/publish-crates.yml`

- [ ] **Step 1: Add `version` to internal workspace deps in root Cargo.toml**

In `Cargo.toml`, find the `[workspace.dependencies]` section and update the internal crate entries to include `version`:

```toml
# Internal crates — version required for crates.io publishing
thinkingroot-core    = { path = "crates/thinkingroot-core",    version = "0.1.0" }
thinkingroot-graph   = { path = "crates/thinkingroot-graph",   version = "0.1.0", default-features = false }
thinkingroot-parse   = { path = "crates/thinkingroot-parse",   version = "0.1.0" }
thinkingroot-extract = { path = "crates/thinkingroot-extract", version = "0.1.0" }
thinkingroot-link    = { path = "crates/thinkingroot-link",    version = "0.1.0" }
thinkingroot-compile = { path = "crates/thinkingroot-compile", version = "0.1.0" }
thinkingroot-verify  = { path = "crates/thinkingroot-verify",  version = "0.1.0" }
thinkingroot-serve   = { path = "crates/thinkingroot-serve",   version = "0.1.0", default-features = false }
thinkingroot-safety  = { path = "crates/thinkingroot-safety",  version = "0.1.0" }
thinkingroot-branch  = { path = "crates/thinkingroot-branch",  version = "0.1.0", default-features = false }
```

- [ ] **Step 2: Add `publish = false` to thinkingroot-python/Cargo.toml**

Add this line after `[package]` in `thinkingroot-python/Cargo.toml`:

```toml
publish = false  # cdylib — distributed via PyPI via maturin, not crates.io
```

- [ ] **Step 3: Add `keywords`, `categories`, `repository` to workspace Cargo.toml**

In the `[workspace.package]` section of `Cargo.toml`, add:

```toml
[workspace.package]
version = "0.1.0"
edition = "2024"
authors = ["Naveen <naveen@thinkingroot.dev>"]
license = "MIT OR Apache-2.0"
repository = "https://github.com/thinkingroot/thinkingroot"
homepage = "https://thinkingroot.dev"
documentation = "https://docs.thinkingroot.dev"
rust-version = "1.85"
keywords = ["knowledge-graph", "llm", "ai", "mcp", "rag"]
categories = ["development-tools", "database", "parser-implementations"]
```

- [ ] **Step 4: Add `repository.workspace`, `homepage.workspace`, `documentation.workspace`, `keywords.workspace`, `categories.workspace` to each crate Cargo.toml**

Add these lines to the `[package]` section of ALL 11 crate `Cargo.toml` files:

```toml
repository.workspace = true
homepage.workspace   = true
documentation.workspace = true
keywords.workspace   = true
categories.workspace = true
```

Files to edit (11 total):
```
crates/thinkingroot-core/Cargo.toml
crates/thinkingroot-graph/Cargo.toml
crates/thinkingroot-parse/Cargo.toml
crates/thinkingroot-extract/Cargo.toml
crates/thinkingroot-link/Cargo.toml
crates/thinkingroot-compile/Cargo.toml
crates/thinkingroot-verify/Cargo.toml
crates/thinkingroot-serve/Cargo.toml
crates/thinkingroot-safety/Cargo.toml
crates/thinkingroot-branch/Cargo.toml
crates/thinkingroot-cli/Cargo.toml
```

- [ ] **Step 5: Verify cargo check still passes after metadata changes**

```bash
cargo check --workspace --no-default-features
```

Expected: No errors.

- [ ] **Step 6: Dry-run publish for thinkingroot-core (leaf crate, no internal deps)**

```bash
cargo publish --dry-run -p thinkingroot-core --no-default-features
```

Expected: `Packaging thinkingroot-core v0.1.0` with no errors. A warning about `path` dependencies being replaced with `version` is expected and correct.

- [ ] **Step 7: Create .github/workflows/publish-crates.yml**

```yaml
name: Publish to crates.io

on:
  push:
    tags:
      - 'v[0-9]+.[0-9]+.[0-9]+'

env:
  CARGO_TERM_COLOR: always

jobs:
  publish:
    name: Publish crates
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2

      - name: Publish in dependency order
        env:
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
        run: |
          set -e

          publish() {
            echo "==> Publishing $1"
            cargo publish -p "$1" --no-default-features
            # crates.io needs ~20s to index before dependents can resolve the dep
            echo "Waiting for crates.io to index $1..."
            sleep 25
          }

          # Tier 1: no internal deps
          publish thinkingroot-core

          # Tier 2: depend only on core
          publish thinkingroot-graph
          publish thinkingroot-parse

          # Tier 3: depend on core + graph/parse
          publish thinkingroot-extract
          publish thinkingroot-link

          # Tier 4: depend on tiers 1-3
          publish thinkingroot-compile
          publish thinkingroot-verify
          publish thinkingroot-safety

          # Tier 5: depends on all above
          publish thinkingroot-serve

          # Tier 6: depends on serve
          publish thinkingroot-branch

          # Tier 7: CLI binary — depends on everything
          publish thinkingroot-cli

          echo "All crates published successfully."
```

- [ ] **Step 8: Verify YAML is valid**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/publish-crates.yml'))" && echo "YAML valid"
```

Expected: `YAML valid`

- [ ] **Step 9: Commit all crates.io changes**

```bash
git add \
  Cargo.toml \
  crates/thinkingroot-core/Cargo.toml \
  crates/thinkingroot-graph/Cargo.toml \
  crates/thinkingroot-parse/Cargo.toml \
  crates/thinkingroot-extract/Cargo.toml \
  crates/thinkingroot-link/Cargo.toml \
  crates/thinkingroot-compile/Cargo.toml \
  crates/thinkingroot-verify/Cargo.toml \
  crates/thinkingroot-serve/Cargo.toml \
  crates/thinkingroot-safety/Cargo.toml \
  crates/thinkingroot-branch/Cargo.toml \
  crates/thinkingroot-cli/Cargo.toml \
  thinkingroot-python/Cargo.toml \
  .github/workflows/publish-crates.yml
git commit -m "feat(dist): add crates.io metadata and publish workflow"
```

---

## Task 6: Wire and validate everything with a test release

After all tasks above are complete, do a full end-to-end dry run to confirm all 4 channels are ready before the first real public tag.

**Files:** None (validation only)

- [ ] **Step 1: Run full test suite to confirm nothing broke**

```bash
cargo test --workspace --no-default-features
```

Expected: All tests pass, 0 failures.

- [ ] **Step 2: Verify maturin build still works**

```bash
cd thinkingroot-python
maturin build --release --no-default-features
ls target/wheels/
cd ..
```

Expected: A `.whl` file present in `thinkingroot-python/target/wheels/`.

- [ ] **Step 3: Dry-run the full crates.io publish sequence**

```bash
for crate in \
  thinkingroot-core \
  thinkingroot-graph \
  thinkingroot-parse \
  thinkingroot-extract \
  thinkingroot-link \
  thinkingroot-compile \
  thinkingroot-verify \
  thinkingroot-safety \
  thinkingroot-serve \
  thinkingroot-branch \
  thinkingroot-cli; do
  echo "==> Dry-run: $crate"
  cargo publish --dry-run -p "$crate" --no-default-features 2>&1 | grep -v "^warning:" | head -5
done
```

Expected: Each crate prints `Packaging <name> v0.1.0` with no errors.

- [ ] **Step 4: Validate install.sh detects your local environment correctly**

```bash
# Test detection only — do NOT actually download (no VERSION set + early exit)
sh -c '
  detect_os() { case "$(uname -s)" in Linux) echo linux;; Darwin) echo macos;; esac; }
  detect_arch() { case "$(uname -m)" in x86_64) echo amd64;; aarch64|arm64) echo arm64;; esac; }
  echo "Asset name: root-$(detect_os)-$(detect_arch)"
'
```

Expected on Apple Silicon: `Asset name: root-macos-arm64`

- [ ] **Step 5: Verify all 4 workflow files are present**

```bash
ls .github/workflows/
```

Expected output includes:
```
ci.yml
release.yml
publish-pypi.yml
publish-crates.yml
```

- [ ] **Step 6: Final commit and push to main**

```bash
git status  # should be clean
git log --oneline -5
git push origin main
```

- [ ] **Step 7: Push the v0.1.0 tag to trigger all channels**

```bash
git tag v0.1.0
git push origin v0.1.0
```

This will trigger:
- `release.yml` → builds 5 binaries → creates GitHub Release with `checksums.txt` → triggers Homebrew bump
- `publish-pypi.yml` → builds 5 platform wheels + sdist → publishes to PyPI
- `publish-crates.yml` → publishes 11 crates in order to crates.io

- [ ] **Step 8: Monitor all 3 workflows in GitHub Actions**

```bash
gh run list --workflow release.yml --limit 3
gh run list --workflow publish-pypi.yml --limit 3
gh run list --workflow publish-crates.yml --limit 3
```

- [ ] **Step 9: After all workflows succeed, verify each channel**

```bash
# curl install
curl -fsSL https://raw.githubusercontent.com/thinkingroot/thinkingroot/main/install.sh | sh

# PyPI
pip install thinkingroot
python -c "import thinkingroot; print(thinkingroot.__version__)"

# Homebrew
brew install thinkingroot/tap/root
root --version

# crates.io
cargo install thinkingroot-cli
root --version
```

---

## Self-Review

### Spec coverage check

| Channel | Task covers it |
|---|---|
| curl install script | Task 2 — `install.sh` with OS/arch detection, SHA256, PATH handling |
| SHA256 checksums in GitHub Releases | Task 1 — `checksums.txt` generated and uploaded |
| PyPI `pip install thinkingroot` | Task 3 — maturin-action 5-platform wheel + sdist + OIDC publish |
| Homebrew `brew install thinkingroot/tap/root` | Task 4 — new tap repo, formula, auto-bump workflow |
| crates.io `cargo install thinkingroot-cli` | Task 5 — metadata, publish order, workflow |
| End-to-end validation | Task 6 — dry runs + live tag push |

### Placeholder scan

No TBDs. The only deferred values are the SHA256 placeholders in `Formula/root.rb` — these are explicitly documented in Task 4 Step 6/7 with exact commands to compute and fill them after the first real release. This is correct: the values cannot exist until the release exists.

### Dependency chain

- Task 1 must come before Task 4 (Homebrew bump workflow in `release.yml` depends on the Homebrew tap existing)
- Task 4 Part B depends on the first real release (which requires Task 1, 3, 5 to be merged first)
- Tasks 2, 3, 5 are independent of each other and can be done in any order
- Task 6 depends on all previous tasks
