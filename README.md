<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/branding/logo_white.png">
  <img alt="ThinkingRoot Logo" src="assets/branding/logo_black.png" width="300" />
</picture>

<br/>

**ThinkingRoot is a deterministic, byte-grounded knowledge substrate for AI agents.**

*Compile a codebase, document set, or any directory of files into a `.tr` pack of typed, content-addressed **Witnesses**. Mount the pack and any AI agent (Claude, GPT, Gemini, a local model) queries it through REST or MCP in milliseconds — with every answer citing exact source bytes.*

**No LLM in the compile path.** Compile is mechanical: tree-sitter, LSP, doctags, regex against a versioned rule catalog. The agent's own LLM does the talking at chat time; ThinkingRoot does the grounding.

<br/>

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.91%2B-orange.svg)](https://www.rust-lang.org)
[![MCP Compatible](https://img.shields.io/badge/MCP-2024--11--05-green.svg)](https://modelcontextprotocol.io)

</div>

---

## Install

**macOS / Linux** — one-line installer (downloads the right pre-built binary, verifies SHA256, drops `root` in `/usr/local/bin`, installs ThinkingRoot Desktop, registers the engine as a login agent):

```bash
curl -fsSL https://thinkingroot.com/install.sh | sh
```

**Windows** (PowerShell):

```powershell
irm https://thinkingroot.com/install.ps1 | iex
```

Direct downloads (no Terminal):

- macOS: <https://thinkingroot.com/download/mac>
- Linux: <https://thinkingroot.com/download/linux>
- Windows: <https://thinkingroot.com/download/windows>

The first time you open a direct-downloaded `.dmg`/`.exe` you'll see a Gatekeeper / SmartScreen warning — right-click → **Open** on macOS, **More info → Run anyway** on Windows. The curl/PowerShell paths skip the warning entirely.

**Build from source** (Rust 1.91+, edition 2024):

```bash
git clone https://github.com/DevbyNaveen/ThinkingRoot.git
cd ThinkingRoot
cargo build --release
sudo mv target/release/root /usr/local/bin/root
```

The binary name is `root`, not `thinkingroot`.

---

## 60-second quickstart

```bash
root setup            # interactive wizard — chat-time LLM provider + MCP wiring
root compile .        # mechanical compile, no LLM, no network
root ask "what does this project do?"
```

`root compile` is deterministic: same source bytes + same rule-catalog version produce a byte-identical `.tr` pack. Verify with `root verify <pack.tr>`.

---

## What's in a `.tr` pack

An uncompressed outer tar containing:

| Member | What it is |
|---|---|
| `manifest.toml` | Canonical TOML manifest (`tr/3.2` format version) |
| `source.tar.zst` | The full source files, zstd-compressed inside an inner tar — lossless |
| `claims.jsonl` | Byte-anchored structural claims, one per line |
| `witnesses.cbor` | The **Witness Mesh** — typed, content-addressed rows derived from primary bytes by a named rule from the catalog |
| `rule_catalog.toml` | Frozen snapshot of the catalog used at compile time (tree-sitter grammar versions pinned from `Cargo.lock`) |
| `signature.sig` *(optional)* | Sigstore bundle for signed packs |

`pack_hash = BLAKE3(manifest_canonical || source_archive || claims_jsonl || witnesses_cbor || rule_catalog_toml)` — every byte that determines a pack's content is folded into its hash.

A `Witness` is identified by `id = BLAKE3(rule || canonical_cbor(spans))` — same input bytes + same rule = same id, byte-for-byte across machines. Cross-workspace dedup falls out for free.

---

## How the compile works

```
sources → tree-sitter / LSP / doctags / regex → Witnesses → CozoDB substrate + .tr pack
                       (mechanical, no LLM)
```

**Pipeline phases** (see `.claude/rules/engine-pipeline.md` + `compile-completeness.md`):

| Phase | What it does |
|---|---|
| 1 | Walker reads source files; respects `.gitignore`, skips noisy directories |
| 2 | Per-chunk structural extraction through the **56-rule catalog** (tree-sitter, LSP, rustdoc/jsdoc/javadoc, markdown, test-assertion miners, opt-in `@claim`/`SAFETY:` comments) |
| 3 | Fingerprint cutoff — incremental compile skips unchanged sources |
| 4 | Cascade-remove changed/deleted sources from the 16 structural tables |
| 5 | Incremental entity-relation update |
| 6 | Source insert + byte-store write |
| 6.45 | **Witness Mesh persist** — dedup, SAFETY-rule cross-check, deterministic sort, batch insert |
| 6.7 | Per-source rebuild of structural rows in one Cozo `multi_transaction` |
| 7 / 7e | Linking + cross-source callee/code-link resolution |
| 9 | Audit — byte coverage, orphan rows, Witness anchor verification |
| 10 | Pack write + (optional) Sigstore sign |

**There is no grounding tribunal and no admission gate in the compile path.** The 4-judge tribunal and the 5-probe Rooting gate that earlier versions ran existed because the LLM paraphrase was unreliable. The Witness Mesh substrate has no paraphrase — every row is byte-for-byte derived from source — so admission is by construction. The surviving 1-judge replacement (`witness_verifier::verify_witness_anchor`) is a single BLAKE3 comparison at pack-open time.

**Incremental compile** is sub-second on real workspaces: p95 = 98 ms for a 1-line edit on a 100-source workspace (CI-gated by `cargo bench -p thinkingroot-serve --bench incremental_smoke`).

---

## Query surfaces

| Surface | When to use |
|---|---|
| **MCP** (`root serve --mcp-stdio`) | Editor / agent integration — Claude Code, Cursor, Codex, Windsurf, Zed |
| **REST** (`root serve`, default port 31760) | HTTP / SSE — `/api/v1/ws/{ws}/{search,witnesses,brief,ask,...}` |
| **Rust** (`thinkingroot-serve::engine::QueryEngine`) | In-process embedding |
| **Python** (`pip install thinkingroot`) | `Brain.open(path)` / `Brain.remote(url)` |
| **TypeScript** (`npm install @thinkingroot/sdk`) | `Brain.remote(url)` (Node 18+, ESM) |

Every surface is workspace-scoped and returns the same shapes for AEP probes, hybrid retrieval, witness listing, and brief synthesis. The chat-time LLM (the agent's own model that paraphrases Witness span text into prose at query time) is the **only** LLM in the system.

---

## Knowledge version control

`root branch` gives the substrate the same primitives Git gives source code:

```bash
root branch create alice/fix-pricing
root checkout alice/fix-pricing
# … agent contributes claims/witnesses to this branch …
root branch diff main alice/fix-pricing
root branch merge alice/fix-pricing --into main
```

Branches are APFS-clonefile / Linux-FICLONE-backed when supported (~10 ms create), with TTL, protection rules, agent-sandbox templates, vector-embedding contradiction detection, dry-run merge, live SSE branch-event streaming, bitemporal as-of queries, and branch-as-pack export/import. Full contract: `.claude/rules/branch-system.md`.

---

## Architecture in one diagram

```
┌──────────────────────────────────────────────────────────────┐
│ root CLI · Tauri Desktop · Python · TypeScript SDK           │
└──────────────────────────────────────────────────────────────┘
                              │
                  cortex.lock (singleton discovery)
                              │
┌──────────────────────────────────────────────────────────────┐
│ thinkingroot-serve   (daemon — REST + MCP + chat-time LLM)   │
│   intelligence::{synthesizer, react, hybrid, builtin_tools}  │
│   engine::QueryEngine   (in-proc reader API)                 │
│   pipeline              (compile + incremental + audit)      │
└──────────────────────────────────────────────────────────────┘
                              │
┌────────────────┬────────────┴──────────────┬─────────────────┐
│ -extract       │ -graph (CozoDB)           │ -branch         │
│  rule catalog  │  16 structural tables     │  registry       │
│  mesh assembly │  witnesses + edges        │  diff / merge   │
│  LSP backends  │  aep + hybrid queries     │  TTL / clonefile│
└────────────────┴────────────┬──────────────┴─────────────────┘
                              │
            -parse · -core · -ground (witness_verifier)
                              │
                       byte-anchored source
```

22 workspace crates + Python (PyO3) + TypeScript SDKs.

---

## CLI reference

```
root setup                                first-run wizard
root compile <path> [--watch] [--json]    mechanical compile to .tr
root verify  <pack.tr>                    check pack hash, signatures, anchors
root mount   <pack.tr>                    mount a pack as a live workspace
root unmount <name>
root ask     "<question>"                 chat-time synthesis over the substrate
root query   "<datalog>"                  raw Cozo Datalog
root brief                                workspace TL;DR for an agent
root branch  {create,checkout,merge,diff,tag,...}
root migrate {--to-completeness-contract,
              --to-water-flow,
              --to-witness-mesh} [--dry-run]
root serve   [--mcp-stdio | --port 31760] daemon entry point
```

`root <cmd> --help` for the full surface.

---

## Repo layout

| Path | What |
|---|---|
| `crates/` | 22 engine crates + `thinkingroot-python` (PyO3, excluded from default-members) |
| `apps/thinkingroot-desktop/` | Tauri 2 desktop app — stand-alone workspace |
| `apps/thinkingroot-landing/` | Marketing site |
| `sdks/typescript/` | Pure-TypeScript SDK (Node 18+, ESM) |
| `docs/` | Design specs, dated. Current shipped ledger: `docs/SHIPPED.md`. |
| `.claude/rules/` | Path-scoped engine invariants — see `CLAUDE.md` |
| `benchmarks/` | LongMemEval workspace + perf benches |

The cloud SaaS Hub lives in a sibling repo (`thinkingroot-cloud`, proprietary). Both must live under the same parent directory because cloud `services/registry` path-deps `tr-format`.

---

## Toolchain

- **Rust 1.91+, edition 2024.** Workspace crates pinned at `0.9.1`; `tr-format` bumped to `1.0.0` to signal the `tr/3.2` wire-format extension.
- **CozoDB** for the graph substrate; **fastembed** + `AllMiniLML6V2` (384-dim, cosine) for the in-memory vector recall tier.
- **Tauri 2** + React for the desktop.
- **No GPU required.** Embedding is CPU.

---

## Testing

```bash
cargo test --workspace                      # full engine suite
cargo check --workspace                     # also validates thinkingroot-python
cargo bench -p thinkingroot-serve --bench incremental_smoke
```

`thinkingroot-python` is excluded from default-members; build wheels via `maturin build --release` in the package directory.

---

## License

MIT. See `LICENSE-MIT`.

<!-- THINKINGROOT:BEGIN -->
_Auto-generated by ThinkingRoot 0.9.1 on 2026-05-18T04:00:43Z. Edits between these markers will be overwritten on the next compile — put hand-written content above or below this block._

## Overview

- **820** sources · **24408** claims · **12598** entities
- Trust: 0% rooted, 100% attested, 0% quarantined, 0% rejected
- 63 open contradictions

## Top entities

- pnpm-lock.yaml — 2931 claims
- desktop-schema.json — 1877 claims
- macOS-schema.json — 1877 claims
- acl-manifests.json — 1435 claims
- DevbyNaveen — 1311 claims
- definitions.Identifier.oneOf[0].const — 854 claims
- definitions.Identifier.oneOf[0].description — 852 claims
- definitions.Identifier.oneOf[0].markdownDescription — 852 claims
- definitions.Identifier.oneOf[1].type — 848 claims
- tauri.ts — 260 claims

## Sources

| Path | Claims |
| --- | ---: |
| /Users/naveen/Desktop/thinkingroot/apps/thinkingroot-desktop/ui/pnpm-lock.yaml | 2246 |
| /Users/naveen/Desktop/thinkingroot/apps/thinkingroot-desktop/src-tauri/gen/schemas/desktop-schema.json | 1877 |
| /Users/naveen/Desktop/thinkingroot/apps/thinkingroot-desktop/src-tauri/gen/schemas/macOS-schema.json | 1877 |
| /Users/naveen/Desktop/thinkingroot/apps/thinkingroot-desktop/src-tauri/gen/schemas/acl-manifests.json | 1435 |
| /Users/naveen/Desktop/thinkingroot/apps/thinkingroot-landing/pnpm-lock.yaml | 685 |
| /Users/naveen/Desktop/thinkingroot/apps/thinkingroot-desktop/ui/src/lib/tauri.ts | 411 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-serve/src/rest.rs | 305 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-llm/src/llm.rs | 246 |
| /Users/naveen/Desktop/thinkingroot/marketingposition.md | 206 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-serve/src/intelligence/playground_tools.rs | 154 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-serve/src/intelligence/hybrid.rs | 153 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-serve/src/mcp/tools.rs | 142 |
| /Users/naveen/Desktop/thinkingroot/AUDIT.md | 140 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-serve/src/intelligence/synthesizer.rs | 127 |
| git://f2c5c5549dc1ae17da960bc4986a104f569e627e | 125 |
| /Users/naveen/Desktop/thinkingroot/apps/thinkingroot-desktop/src-tauri/resources/js/turndown.js | 123 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-serve/src/intelligence/engram.rs | 120 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-serve/src/intelligence/builtin_tools.rs | 119 |
| git://50da85cb3ff857bebaa67ea685d39fd3fdeb4565 | 118 |
| /Users/naveen/Desktop/thinkingroot/crates/thinkingroot-cli/src/pack_cmd.rs | 117 |

## Branches

- `Fortesting` — kind: Feature, merge policy: Manual
- `stream/836225bb-1881-464c-9e38-21027322324b` — kind: Stream { session\_id: "836225bb-1881-464c-9e38-21027322324b" }, merge policy: AutoOnSessionEnd
- `stream/a9da07a2-cc29-4d00-85d1-bdd958137c53` — kind: Stream { session\_id: "a9da07a2-cc29-4d00-85d1-bdd958137c53" }, merge policy: AutoOnSessionEnd
- `stream/fdf49718-048a-42c1-ae2e-359a4e34b9b6` — kind: Stream { session\_id: "fdf49718-048a-42c1-ae2e-359a4e34b9b6" }, merge policy: AutoOnSessionEnd
- `stream/6cd6d418-b11a-4d77-a9f2-c536ccae6ae3` — kind: Stream { session\_id: "6cd6d418-b11a-4d77-a9f2-c536ccae6ae3" }, merge policy: AutoOnSessionEnd

<!-- THINKINGROOT:END -->
