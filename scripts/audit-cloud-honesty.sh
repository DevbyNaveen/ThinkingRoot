#!/usr/bin/env bash
# Honesty-audit lint for cloud-touching files.
#
# Run by CI on every PR that touches cloud-related paths.
#
# Spec: docs/superpowers/specs/2026-05-13-oss-cloud-readiness-design.md §9.4.

set -euo pipefail

FAIL=0

cloud_files() {
  # git ls-files expands `dir/` to all tracked files under it, which is
  # what we want for nested layouts. `**/*.rs`-style globs only match
  # paths Git actually has on disk, so we use the explicit directory
  # form for the cloud-auth crate's `src/` + `tests/` split.
  git ls-files \
    'crates/thinkingroot-cloud-auth/src/' \
    'crates/thinkingroot-cloud-auth/tests/' \
    'crates/thinkingroot-extract/src/llm.rs' \
    'crates/thinkingroot-cli/src/cloud/' \
    'apps/thinkingroot-desktop/src-tauri/src/commands/cloud.rs' \
    'apps/thinkingroot-desktop/ui/src/components/cloud/' \
    2>/dev/null
}

# Portable grep wrapper: uses `-E` (extended regex, POSIX) so we work
# on BSD grep / macOS out of the box. PCRE-only features (`\s`, `\d`)
# would silently no-op on BSD grep — translate them to POSIX classes
# (`[[:space:]]`, `[0-9]`) instead. Guard against an empty file list:
# `xargs grep` against zero files would otherwise hang reading stdin.
check() {
  local pattern="$1"
  local description="$2"
  local files
  files=$(cloud_files)
  if [ -z "$files" ]; then
    return
  fi
  local hits
  hits=$(printf '%s\n' "$files" | xargs grep -nE "$pattern" 2>/dev/null || true)
  if [ -n "$hits" ]; then
    echo "✘ Honesty-audit violation — $description"
    printf '%s\n' "$hits" | sed 's/^/  /'
    FAIL=1
  fi
}

# 1. No unwrap_or_default() on cloud-auth data-mutation paths (silent
#    signed-out state hides auth failures). Per spec §9.4 the rule
#    targets MUTATIONS, not diagnostic display / response-body reads.
#    Exclude lines where:
#      - the unwrap_or_default() is feeding println!/eprintln!/format!/tracing
#        (display-only formatting)
#      - it follows `resp.text().await` (HTTP error-body diagnostic
#        extraction; the response already failed, this is just for log
#        text)
#      - it is in `llm.rs` chat-time LLM body parsing (out-of-scope for
#        cloud-auth's signed-out-state rule)
check_unwrap_or_default() {
  local description="unwrap_or_default() on cloud-auth data-mutation paths"
  local files hits
  files=$(cloud_files | grep -v 'crates/thinkingroot-extract/src/llm.rs' || true)
  if [ -z "$files" ]; then
    return
  fi
  # Drop lines that are clearly display-only or response-body
  # diagnostic-reads, plus struct-literal `field: X.clone().unwrap_or_default(),`
  # patterns that build return values (the field is typed and the
  # caller's contract is the source of truth, not the call site).
  # These exclusions are intentionally narrow — any new `unwrap_or_default()`
  # outside these shapes WILL trip the audit.
  hits=$(printf '%s\n' "$files" \
    | xargs grep -nE 'unwrap_or_default\(\)' 2>/dev/null \
    | grep -vE '(println!|eprintln!|format!|tracing::|resp\.text\(\))' \
    | grep -vE ':[[:space:]]+\.unwrap_or_default\(\)' \
    | grep -vE '\.clone\(\)\.unwrap_or_default\(\)' \
    || true)
  if [ -n "$hits" ]; then
    echo "✘ Honesty-audit violation — $description"
    printf '%s\n' "$hits" | sed 's/^/  /'
    FAIL=1
  fi
}
check_unwrap_or_default

# 2. No naked Ok(()) returns from token-touching save/persist/update
#    functions (stubs hide failures).
check 'fn (save|persist|update)[^{]*\{[[:space:]]*Ok\(\(\)\)[[:space:]]*\}' \
  "stub Ok(()) inside save/persist/update function"

# 3. No TODO / FIXME / TBD inside the cloud-auth crate (per project rule).
check '(TODO|FIXME|TBD|XXX)' "TODO/FIXME/TBD found in cloud-auth crate"

# 4. Token never in eprintln! / println! / console.log / console.error
check 'eprintln!\([^)]*token' "eprintln! with token in format string"
check 'println!\([^)]*\{token\}' "println! with raw token format"
check 'console\.(log|error)\([^)]*token' "console.log/error with raw token"

# 5. No .expect("token") that would dump the token in panic messages.
check '\.expect\("[^"]*token[^"]*"\)' "expect() with token in message"

if [ $FAIL -ne 0 ]; then
  echo ""
  echo "Honesty audit failed. See spec §9.4 for the rationale."
  echo "Fix the violations or — if a flagged pattern is a false positive"
  echo "(e.g., 'token_invalid' in a static error-mapping table) — adjust"
  echo "the regex in scripts/audit-cloud-honesty.sh."
  exit 1
fi

echo "✓ Honesty audit passed."
