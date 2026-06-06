# ThinkingRoot: OSS vs SaaS — Complete Feature Comparison

**Date:** 2026-04-10  
**Based on:** Phases 1–3 (built), Phase 3.5 (designed), Phase 4–5 (planned)  
**No speculation — every feature listed is either built or formally designed**

---

## Answer: What is open source?

**Phases 1 through 3.5 are fully open source** (this repo, Apache 2.0 or MIT).  
**Phases 4 and 5 are a separate private repo** that imports Phases 1–3.5 as dependencies.

```
Open Source (this repo)           Private (cloud backend)
────────────────────────          ────────────────────────
Phase 1: Core engine              Phase 4: SaaS platform
Phase 2: Serve + SDK              Phase 5: Enterprise
Phase 3: Onboarding
Phase 3.5: Ecosystem + KVC
```

**One binary is the source of truth.** The `root` binary is compiled from this open source repo and is identical for OSS and SaaS users. There is no separate "cloud binary." Cloud commands (`root login`, `root sync`) are open source code in this repo — they require a cloud backend endpoint to function, but the code is public. The lock is in the backend service, not the binary. This is the same model as the GitHub CLI (`gh` is open source; GitHub.com is the service).

---

## Feature Comparison

### Pipeline (Parse → Extract → Link → Compile → Verify)

| Feature | OSS | SaaS |
|---------|-----|------|
| Parse markdown, code, PDF, git history | ✓ local | ✓ cloud |
| Supported languages (tree-sitter AST) | Rust, Python, JS, TS, Go | same |
| LLM extraction | ✓ your API key, local | ✓ cloud workers, no key needed |
| 11 LLM providers (Bedrock, OpenAI, Anthropic, Ollama, Groq, DeepSeek, OpenRouter, Together, Perplexity, LiteLLM, custom) | ✓ all 11 | ✓ all 11 + managed keys |
| Entity resolution (4-level: exact, alias, cross-alias, fuzzy) | ✓ local | ✓ cloud |
| Contradiction detection (negation pairs, confidence comparison) | ✓ local | ✓ cloud |
| Incremental compilation (BLAKE3 content hashes) | ✓ local | ✓ cloud |
| Extraction cache (content-addressed, zero re-LLM on unchanged files) | ✓ local | ✓ cloud |
| 8 compiled artifact types (entity pages, architecture map, decision log, task pack, agent brief, runbook, contradiction report, health report) | ✓ local | ✓ cloud |
| Health scoring (freshness 30% + consistency 30% + coverage 20% + provenance 20%) | ✓ local | ✓ cloud |
| Auto-refresh (recompile on push via webhook) | ✗ manual only | ✓ |

---

### Knowledge Graph

| Feature | OSS | SaaS |
|---------|-----|------|
| Graph storage (CozoDB, Datalog queries) | ✓ local `.thinkingroot/graph.db` | ✓ persistent cloud graph |
| Vector search (fastembed AllMiniLML6V2, cosine similarity) | ✓ local | ✓ cloud-scale |
| Semantic search (vector + keyword fallback) | ✓ local | ✓ cloud |
| 9 CozoDB relations (sources, claims, entities, relations, aliases, edges, temporal, contradictions) | ✓ local | ✓ cloud |
| Type-safe IDs (ULID-backed, phantom-typed) | ✓ | ✓ |
| Claim temporal validity (valid_from, valid_until, superseded_by) | ✓ | ✓ |
| Cross-workspace federated queries | ✗ | ✓ |
| Persistent cloud graph (survives local machine loss) | ✗ | ✓ |

---

### Knowledge Version Control (Phase 3.5 — OSS)

| Feature | OSS | SaaS |
|---------|-----|------|
| `root branch <name>` — create isolated knowledge branch | ✓ | ✓ |
| Branch = snapshot copy of parent (SQLite hot backup) | ✓ | ✓ |
| Extraction cache shared via symlink (zero re-LLM on branch) | ✓ | ✓ |
| `root diff <branch>` — semantic Knowledge PR | ✓ terminal output | ✓ web UI |
| Semantic diff (BLAKE3 of normalized statement — deduplicates same fact extracted twice) | ✓ | ✓ |
| Contradiction-as-conflict detection | ✓ | ✓ |
| Auto-resolution (confidence delta > 0.15 → higher-confidence wins) | ✓ | ✓ |
| Health-score CI gate (blocks merge if health drops > 5%) | ✓ | ✓ |
| `root merge <branch>` — verified merge to main | ✓ | ✓ |
| `--propagate-deletions` flag | ✓ | ✓ |
| `root status` — branch summary | ✓ | ✓ |
| `root checkout <branch>` — set active branch (HEAD file) | ✓ | ✓ |
| `root serve --branch <name>` — serve a branch directly | ✓ | ✓ |
| Agent sandboxing (agents work in branches, humans review before merge) | ✓ | ✓ |
| Remote shared branches (team members pull/push branches) | ✗ | ✓ |
| Knowledge PR review in web UI | ✗ | ✓ |
| Branch access control (who can merge to main) | ✗ | ✓ |
| Branch history + analytics | ✗ | ✓ |
| Multi-level branching (branch from a branch) | ✗ Phase 3.5 | ✓ Phase 4 |

---

### Serving & API

| Feature | OSS | SaaS |
|---------|-----|------|
| REST API (Axum, 13 endpoints) | ✓ local | ✓ cloud |
| MCP server — HTTP SSE transport | ✓ local | ✓ cloud |
| MCP server — stdio transport | ✓ local | N/A |
| Multi-workspace mount (`root serve --path` repeatable) | ✓ | ✓ |
| Bearer token auth (`--api-key`) | ✓ | ✓ managed |
| `--no-rest` / `--no-mcp` flags | ✓ | N/A |
| `--install-service` (launchd macOS, systemd Linux, Windows service) | ✓ | N/A |
| Always-on cloud endpoint (no local server needed) | ✗ | ✓ |
| Federated serve (proxy across all org workspaces) | ✗ | ✓ |

---

### MCP Tools

| Tool | OSS | SaaS |
|------|-----|------|
| `search` | ✓ | ✓ |
| `query_claims` | ✓ | ✓ |
| `get_relations` | ✓ | ✓ |
| `compile` | ✓ local | ✓ cloud (no API key) |
| `health_check` | ✓ | ✓ |
| `create_branch` | ✓ Phase 3.5 | ✓ |
| `diff_branch` | ✓ Phase 3.5 | ✓ |
| `merge_branch` | ✓ Phase 3.5 | ✓ |

---

### CLI Commands

| Command | OSS | SaaS |
|---------|-----|------|
| `root compile <path>` | ✓ | ✓ (cloud workers) |
| `root health` | ✓ | ✓ |
| `root init` | ✓ | ✓ |
| `root query` | ✓ | ✓ |
| `root serve` | ✓ local | ✓ proxies cloud |
| `root setup` (5-step wizard) | ✓ | ✓ |
| `root connect` (7 AI tools: Claude Desktop, Cursor, VS Code, Windsurf, Zed, Cline, Continue.dev) | ✓ | ✓ |
| `root workspace add/list/remove` | ✓ | ✓ |
| `root branch` | ✓ Phase 3.5 | ✓ |
| `root diff` | ✓ Phase 3.5 | ✓ |
| `root merge` | ✓ Phase 3.5 | ✓ |
| `root status` | ✓ Phase 3.5 | ✓ |
| `root checkout` | ✓ Phase 3.5 | ✓ |
| `root snapshot` | ✓ Phase 3.5 | ✓ |
| `root login` | ✓ in binary — needs cloud backend | ✓ thinkingroot.dev |
| `root sync` | ✓ in binary — needs cloud backend | ✓ thinkingroot.dev |
| `root sync --branch <name>` | ✓ in binary — needs cloud backend | ✓ thinkingroot.dev |

---

### SDKs

| SDK | OSS | SaaS |
|-----|-----|------|
| Python SDK — native (PyO3, full pipeline + graph access) | ✓ | ✓ |
| Python SDK — HTTP client (REST API wrapper) | ✓ | ✓ |
| TypeScript SDK | ✗ Phase 3.5 | ✓ |
| GitHub Action (`thinkingroot/compile-action`) | ✗ Phase 3.5 | ✓ |
| VS Code extension | ✗ Phase 3.5 | ✓ |

---

### Configuration

| Feature | OSS | SaaS |
|---------|-----|------|
| Per-workspace config (`.thinkingroot/config.toml`) | ✓ | ✓ |
| Global config (`~/.config/thinkingroot/config.toml`) | ✓ | ✓ |
| Config merge hierarchy (workspace wins over global) | ✓ | ✓ |
| WorkspaceRegistry (`~/.config/thinkingroot/workspaces.toml`) | ✓ | ✓ |
| `[merge]` config (max_health_drop, block_on_contradictions, auto_resolve_threshold) | ✓ Phase 3.5 | ✓ |
| Cloud org config (team settings, connector credentials) | ✗ | ✓ |

---

### Collaboration (the SaaS moat)

| Feature | OSS | SaaS |
|---------|-----|------|
| Single developer, local machine | ✓ full capability | ✓ |
| Multiple developers, shared knowledge base | ✗ | ✓ |
| Web dashboard | ✗ | ✓ |
| Team roles (viewer, contributor, admin) | ✗ | ✓ |
| Knowledge PR approval workflow (web) | ✗ | ✓ |
| Notifications (branch ready to merge, health alert) | ✗ | ✓ |
| Connectors: GitHub webhooks (auto-refresh on push) | ✗ | ✓ |
| Connectors: Notion, Confluence, Jira, Linear | ✗ | ✓ Phase 4 |
| Cross-workspace search (org-wide) | ✗ | ✓ |
| Activity log and audit trail | ✗ | ✓ |
| SSO (SAML, OIDC) | ✗ | ✓ Phase 5 |
| Air-gapped / self-hosted enterprise deployment | ✗ | ✓ Phase 5 |
| SLA + support | ✗ | ✓ Phase 5 |

---

### Migration: Local → SaaS

| Step | Command | What happens |
|------|---------|--------------|
| 1. Authenticate | `root login` | Browser opens → JWT stored locally |
| 2. Push existing knowledge | `root sync` | Compiled graph uploaded — no re-LLM, no API key, local data untouched |
| 3. Continue working | `root compile` + `root sync` | Compile locally, push to cloud |
| Revert to local | `root logout` | JWT removed, all local commands unchanged |

**Key guarantee: claims not code.** `root sync` uploads extracted knowledge (claims, entities, relations) — never the raw source files. Proprietary code never leaves the local machine.

**Key guarantee: local-first.** The local `graph.db` is always intact. Cloud is a sync target. If the cloud goes down, all local commands (`root compile`, `root serve`, `root branch`, `root merge`) continue to work without interruption.

---

## The Positioning in One Sentence

**OSS = full local capability, single developer, data never leaves your machine.**  
**SaaS = same capability + team collaboration + cloud scale + connectors.**

The OSS tier is not crippled. A solo developer or a team that self-hosts gets the complete knowledge compiler, the full REST + MCP server, all 11 LLM providers, and — from Phase 3.5 — the full KVC branching system. Nothing essential is locked behind SaaS.

SaaS adds what you cannot do locally: multiple people sharing one brain, always-on cloud endpoint, webhooks that keep knowledge fresh automatically, and the web UI for reviewing Knowledge PRs as a team.

This is the Git / GitHub split. Git is not a reduced version of GitHub. GitHub is a collaboration platform built on top of Git.

---

## What "Ready for Phase 3.5" means

All of the following are complete and in the open source repo right now:

- ✓ thinkingroot-core (types, config, IDs, global config, workspace registry)
- ✓ thinkingroot-parse (markdown, code, PDF, git)
- ✓ thinkingroot-extract (LLM extraction, all 11 providers, extraction cache)
- ✓ thinkingroot-link (entity resolution, contradiction detection)
- ✓ thinkingroot-graph (CozoDB schema, all CRUD, vector store)
- ✓ thinkingroot-compile (8 artifact types, Tera templates)
- ✓ thinkingroot-verify (health scoring, staleness, orphan detection)
- ✓ thinkingroot-serve (REST API, MCP SSE + stdio, QueryEngine, pipeline)
- ✓ thinkingroot-cli (root binary, all commands through Phase 3)
- ✓ thinkingroot-python (PyO3 native + HTTP client)

Phase 3.5 adds one new crate (`thinkingroot-branch`) and extends the CLI, serve, and core crates. The foundation is solid.
