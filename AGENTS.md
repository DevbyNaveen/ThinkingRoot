# AGENTS.md - ThinkingRoot

## What This File Is For

Instructions for AI coding agents working in this repository, including Gemini Antigravity workspace agents.

## Project Context

ThinkingRoot is a deterministic, byte-grounded knowledge substrate for AI agents. It compiles source trees, documents, and workspaces into typed, content-addressed Witnesses and serves them through REST, MCP, Rust, Python, and TypeScript surfaces.

The binary name is `root`, not `thinkingroot`.

## Architecture

| Component | Path | Notes |
|---|---|---|
| Engine crates | `crates/` | Parse, extract, graph, compile, branch, serve, verify, trust crates |
| CLI | `crates/thinkingroot-cli/` | Binary name `root` |
| Desktop app | `apps/thinkingroot-desktop/` | Stand-alone Tauri 2 workspace |
| Landing page | `apps/thinkingroot-landing/` | Vite app; install command source lives in `App.jsx` |
| Python SDK | `thinkingroot-python/` | PyO3/maturin; excluded from default workspace members |
| TypeScript SDK | `sdks/typescript/` | Pure ESM SDK |
| Docs | `docs/` | Dated specs and shipped ledger |
| Claude rules | `.claude/rules/` | Path-scoped invariants; read relevant rules before editing matching code |

The proprietary cloud Hub lives in sibling repo `~/Desktop/thinkingroot-cloud/`. Keep the cross-repo path relationship intact because cloud services path-depend on this repo's crates.

## Core Rules

- No fake data. Empty states are fine; fabricated metrics, mounts, claims, or sync status are not.
- Verify paths, flags, and APIs against the working tree before recommending them.
- `root compile` is mechanical-only; no LLM extraction or grounding tribunal belongs in the compile path.
- Chat-time LLM logic lives in `thinkingroot-llm`.
- `thinkingroot-rooting` no longer exists. Do not reintroduce that crate name.
- `thinkingroot-verify` and `tr-verify` are different crates.
- Keep Tauri `productName = "ThinkingRoot"` with no spaces.
- Do not import helloroot's multi-agent framework into this repo.

## Common Commands

```bash
cargo test --workspace
cargo check --workspace
cargo bench -p thinkingroot-serve --bench incremental_smoke
root compile .
root serve --port 31760
root serve --mcp-stdio
root ask "what does this project do?"
```

## Editing Guidance

- Read `CLAUDE.md` before non-trivial edits. It is the canonical always-on project memory.
- Read the relevant `.claude/rules/*.md` file before changing a path covered by that rule.
- Preserve the Witness Mesh invariants: byte anchors, deterministic IDs, rule catalog discipline, and no silent fallbacks.
- New CLI subcommands need at least one integration test.
- Live network tests must be ignored unless explicitly gated by an environment variable.
- Do not push without explicit approval.

## Agent Behavior

- Be direct, grounded, and careful with claims.
- Prefer small, verified changes over broad rewrites.
- If a command, file, or API may have drifted, inspect it before answering.
- When blocked by missing credentials, network, or an external service, state the exact blocker and the closest local verification performed.
