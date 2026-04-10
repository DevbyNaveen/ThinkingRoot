```
╔════════════════════════════════════════════════════════════════╗
║                                                                ║
║   ████████╗██╗  ██╗██╗███╗   ██╗██╗  ██╗██╗███╗   ██╗ ██████╗  ║
║      ██╔══╝██║  ██║██║████╗  ██║██║ ██╔╝██║████╗  ██║██╔════╝  ║
║      ██║   ███████║██║██╔██╗ ██║█████╔╝ ██║██╔██╗ ██║██║  ███╗ ║
║      ██║   ██╔══██║██║██║╚██╗██║██╔═██╗ ██║██║╚██╗██║██║   ██║ |
║      ██║   ██║  ██║██║██║ ╚████║██║  ██╗██║██║ ╚████║╚██████╔╝ ║
║      ╚═╝   ╚═╝  ╚═╝╚═╝╚═╝  ╚═══╝╚═╝  ╚═╝╚═╝╚═╝  ╚═══╝ ╚═════╝  ║
║                                                                ║
║          R  O  O  T                                            ║
║          The open-source knowledge compiler for AI agents      ║
╚════════════════════════════════════════════════════════════════╝
```

<div align="center">

[![CI](https://github.com/thinkingroot/thinkingroot/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkingroot/thinkingroot/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20%7C%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![Crates.io](https://img.shields.io/crates/v/thinkingroot.svg)](https://crates.io/crates/thinkingroot)
[![PyPI](https://img.shields.io/pypi/v/thinkingroot.svg)](https://pypi.org/project/thinkingroot)
[![MCP Compatible](https://img.shields.io/badge/MCP-2024--11--05-green.svg)](https://modelcontextprotocol.io)

**[Documentation](https://docs.thinkingroot.dev) · [Quick Start](#quick-start) · [Python SDK](#python-sdk) · [MCP Server](#mcp-server) · [Discord](https://discord.gg/thinkingroot)**

</div>

---

## What is ThinkingRoot?

> *"You know how code has compilers? ThinkingRoot is a compiler for knowledge."*

ThinkingRoot takes your docs, code, chats, tickets, and PDFs — and compiles them into a **typed, verified, linked knowledge graph** that AI agents can read in **2K tokens instead of 50K**.

It runs continuously, like CI/CD for what your organization knows.

```
Your Repo / Docs / Slack / PDFs
         │
         ▼
┌─────────────────────────────────────────────────────────────┐
│  PARSE → EXTRACT → LINK → COMPILE → VERIFY → SERVE         │
│                                                             │
│  raw text    claims     graph    artifacts  health    API   │
│  + code   + entities  + links  + entity    score    + MCP  │
│            + relations          pages                      │
└─────────────────────────────────────────────────────────────┘
         │
         ▼
  .thinkingroot/
  ├── entities/AuthService.md     ← agent reads this
  ├── architecture-map.md
  ├── contradiction-report.md
  └── graph.db                    ← query via REST / MCP / Python
```

**Current solutions** (Mem0, Zep, LangMem, Cognee) treat this as a **retrieval problem** — store and search.  
ThinkingRoot treats it as a **compilation problem** — transform, verify, and serve optimized knowledge.

---

## Why ThinkingRoot?

| Problem | Current approach | ThinkingRoot |
|---|---|---|
| Agent re-reads 50K tokens per session | More context window | Compile once, serve 2K tokens |
| Contradictory info across sources | Hope the agent figures it out | Detect + resolve contradictions automatically |
| No idea if knowledge is stale | Manual review | Knowledge Health Score (freshness, consistency, coverage) |
| Agent trust all sources equally | — | Trust levels: Quarantined → Verified |
| Agent reads internal secrets | — | Sensitivity labels: Public → Restricted |
| Knowledge scattered everywhere | RAG | Typed graph: Claims → Entities → Relations → Artifacts |

---

## Features

### Core Pipeline

- **6-stage compilation:** Parse → Extract → Link → Compile → Verify → Serve
- **Incremental:** Only recompiles what changed (BLAKE3 content hashing, like `make`)
- **Multi-source:** Markdown, code (Rust/Python/TypeScript/Go/JS via tree-sitter), PDFs, git commits
- **Multi-provider LLM:** AWS Bedrock, OpenAI, Anthropic, Ollama, Groq, DeepSeek — one config line to switch

### Knowledge Model

- **Claims** — atomic, source-locked, typed, timestamped statements with confidence scores
- **Entities** — named things (people, systems, APIs, concepts) with aliases and type hierarchy
- **Relations** — typed directed edges with evidence and temporal validity
- **Contradictions** — detected automatically, resolved with audit trail
- **Artifacts** — compiled outputs: entity pages, architecture maps, decision logs, runbooks, task packs

### Quality & Safety

- **Knowledge Health Score** — freshness (30%) + consistency (30%) + coverage (20%) + provenance (20%)
- **7 verification checks** — staleness, contradiction, orphan, confidence decay, poisoning, leakage, coverage
- **Trust levels** — Quarantined / Untrusted / Unknown / Trusted / Verified per source
- **Sensitivity labels** — Public / Internal / Confidential / Restricted per claim

### Interfaces

- **CLI** — `root compile ./repo` | `root health` | `root serve`
- **REST API** — Axum-based, bearer auth, multi-workspace, JSON envelope
- **MCP Server** — Model Context Protocol 2024-11-05, SSE + stdio transports
- **Python SDK** — native bindings (PyO3) + async HTTP client

---

## Quick Start

### Install

**Homebrew — recommended for macOS and Linux**
```bash
brew install thinkingroot
```
This installs the `root` command. That's it.

**Python SDK**
```bash
pip install thinkingroot
```
Includes native bindings + HTTP client. No Rust toolchain needed.

**Cargo — if you have Rust installed**
```bash
cargo install thinkingroot
```

**From source**
```bash
git clone https://github.com/thinkingroot/thinkingroot
cd thinkingroot
cargo build --release
./target/release/root --help
```

> **Note:** `brew install thinkingroot` gives you the `root` command — same pattern as `brew install ripgrep` giving you `rg`.

### Compile your first knowledge base

```bash
# Initialize config in your repo
root init ./my-project

# Run the full pipeline (LLM required for extraction — see Configuration)
root compile ./my-project

# Check knowledge health
root health --path ./my-project
```

Output:

```
╭─ ThinkingRoot ────────────────────────────────────╮
│  Parsed    247 files                               │
│  Extracted 1,842 claims · 394 entities             │
│  Linked    891 relations · 12 contradictions       │
│  Compiled  8 artifacts                             │
│  Health    ████████░░  83% (freshness: 91%)        │
╰────────────────────────────────────────────────────╯
```

Your compiled knowledge is in `.thinkingroot/`:

```
.thinkingroot/
├── graph.db                        ← CozoDB graph database
├── artifacts/
│   ├── entities/AuthService.md     ← entity page
│   ├── architecture-map.md
│   ├── contradiction-report.md
│   └── health-report.md
└── config.toml
```

---

## Configuration

`root init` creates `.thinkingroot/config.toml` with sensible defaults. Edit to configure your LLM provider:

```toml
[llm]
default_provider = "openai"         # bedrock | openai | anthropic | ollama | groq | deepseek

[llm.providers.openai]
api_key_env = "OPENAI_API_KEY"      # reads from environment

[llm.providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"

[llm.providers.ollama]
base_url = "http://localhost:11434"
default_model = "mistral"           # fully local, no API key needed

[llm.providers.bedrock]
region = "us-east-1"
profile = "default"                 # uses ~/.aws/credentials

[extraction]
min_confidence = 0.5                # discard low-confidence claims
max_chunk_tokens = 4000

[verification]
staleness_days = 90                 # flag claims older than 90 days
```

Copy `.env.example` to `.env` and set your API keys.

---

## REST API

Start the server:

```bash
root serve --port 3000 --path ./my-project
# with auth:
root serve --port 3000 --path ./my-project --api-key secret123
# multi-workspace:
root serve --port 3000 --path ./backend --path ./frontend --path ./infra
```

Query the knowledge graph:

```bash
# list workspaces
curl http://localhost:3000/api/v1/workspaces

# semantic search
curl "http://localhost:3000/api/v1/ws/my-project/search?q=authentication+flow&top_k=5"

# get entity
curl http://localhost:3000/api/v1/ws/my-project/entities/AuthService

# query claims
curl "http://localhost:3000/api/v1/ws/my-project/claims?type=Decision&min_confidence=0.8"

# architecture map (markdown)
curl -H "Accept: text/markdown" \
     http://localhost:3000/api/v1/ws/my-project/artifacts/architecture_map

# health report
curl http://localhost:3000/api/v1/ws/my-project/health
```

All responses use a consistent envelope:

```json
{ "ok": true,  "data": {...}, "error": null   }
{ "ok": false, "data": null,  "error": { "code": "NOT_FOUND", "message": "..." } }
```

---

## MCP Server

ThinkingRoot speaks [Model Context Protocol](https://modelcontextprotocol.io) 2024-11-05.

**Claude Desktop / any MCP client:**

```bash
# stdio transport (recommended for desktop clients)
root serve --mcp-stdio --path ./my-project

# SSE transport (recommended for remote agents)
root serve --port 3000 --no-rest --path ./my-project
# MCP endpoint: http://localhost:3000/mcp/sse
```

Add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "thinkingroot": {
      "command": "root",
      "args": ["serve", "--mcp-stdio", "--path", "/path/to/your/project"]
    }
  }
}
```

**Available MCP tools:**

| Tool | Description |
|---|---|
| `search` | Semantic + keyword search over compiled knowledge |
| `query_claims` | Filter claims by type, entity, confidence, date |
| `get_relations` | Get entity relationship graph |
| `compile` | Trigger pipeline recompile |
| `health_check` | Get knowledge health score |

---

## Python SDK

```bash
pip install thinkingroot
```

**Native bindings (no server needed):**

```python
import thinkingroot

# Compile a project
thinkingroot.compile("./my-project")

# Open a compiled knowledge base
engine = thinkingroot.open("./my-project")
```

**HTTP client (requires `root serve`):**

```python
from thinkingroot.client import Client

c = Client(base_url="http://localhost:3000", api_key="optional")

# Workspaces
workspaces = c.workspaces()

# Search
results = c.search("how does authentication work?", top_k=5)

# Query claims
decisions = c.claims(type="Decision", min_confidence=0.8)

# Get entity with all its claims and relations
auth_service = c.entity("AuthService")

# Knowledge health
health = c.health()
print(f"Health: {health['data']['overall_score']}%")
```

**Error handling:**

```python
from thinkingroot import ThinkingRootError
from thinkingroot.client import APIError

try:
    result = c.entity("NonExistent")
except APIError as e:
    print(f"HTTP {e.status_code}: {e.code} — {e.message}")
except ThinkingRootError as e:
    print(f"Engine error: {e}")
```

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        thinkingroot-cli                          │
│                     (root binary, Clap CLI)                     │
└───────────────────────────┬─────────────────────────────────────┘
                            │
              ┌─────────────▼──────────────┐
              │      thinkingroot-serve     │
              │  REST (Axum) + MCP (SSE +  │
              │  stdio) + QueryEngine      │
              └──────┬────────────┬────────┘
                     │            │
        ┌────────────▼──┐  ┌──────▼──────────┐
        │   thinkingroot │  │  thinkingroot   │
        │   -compile     │  │  -verify        │
        └────────┬───────┘  └────────┬────────┘
                 │                   │
        ┌────────▼───────────────────▼────────┐
        │         thinkingroot-link            │
        │   (entity resolution, contradiction) │
        └──────────────────┬──────────────────┘
                           │
        ┌──────────────────▼──────────────────┐
        │       thinkingroot-extract           │
        │   (LLM: Claims/Entities/Relations)  │
        └──────────────────┬──────────────────┘
                           │
        ┌──────────────────▼──────────────────┐
        │        thinkingroot-parse            │
        │  (markdown, code, PDF, git → IR)    │
        └──────────────────┬──────────────────┘
                           │
        ┌──────────────────▼──────────────────┐
        │        thinkingroot-graph            │
        │   CozoDB (Datalog graph) +          │
        │   fastembed (neural vectors)        │
        └──────────────────┬──────────────────┘
                           │
        ┌──────────────────▼──────────────────┐
        │         thinkingroot-core            │
        │  (types, IDs, config, errors)       │
        └─────────────────────────────────────┘
```

**Tech stack:** Rust (edition 2024) · CozoDB (embedded Datalog graph) · fastembed/ONNX (local neural vectors) · Axum (HTTP) · PyO3 (Python bindings) · tree-sitter (code parsing) · tokio (async runtime)

---

## Supported File Types

| Category | Formats |
|---|---|
| **Documents** | Markdown (`.md`), PDF |
| **Code** | Rust, Python, TypeScript, JavaScript, Go |
| **Config** | TOML, YAML, JSON |
| **Version control** | Git commits, PR descriptions |

More parsers coming in Phase 3.

---

## CLI Reference

```
root <command> [options]

Commands:
  compile <path>    Run full 6-stage pipeline
  health            Show knowledge health score
  init              Initialize .thinkingroot/config.toml
  query <text>      Semantic search over compiled knowledge
  serve             Start REST API + MCP server

Options (compile):
  --path            Path to compile (default: current dir)

Options (serve):
  --port            HTTP port (default: 3000)
  --host            Bind address (default: 127.0.0.1)
  --api-key         Bearer auth token
  --path            Workspace path (repeatable for multi-workspace)
  --mcp-stdio       Use stdio MCP transport instead of HTTP
  --no-rest         Disable REST API routes
  --no-mcp          Disable MCP routes

Global:
  -v, --verbose     Debug logging
  --help            Show help
```

---

## Building from Source

**Prerequisites:** Rust 1.85+, (optional) Python 3.9+ with maturin for the Python SDK

```bash
git clone https://github.com/thinkingroot/thinkingroot
cd thinkingroot

# Build CLI (or just: brew install thinkingroot)
cargo build --release
./target/release/root --help

# Build without neural embedding (faster, no ONNX Runtime)
cargo build --release --no-default-features

# Run tests
cargo test

# Build Python SDK
cd thinkingroot-python
pip install maturin
maturin develop --release
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for crate architecture, feature flags, and contribution patterns.

---

## Roadmap

| Phase | Status | What |
|---|---|---|
| **Phase 1** — Core engine | ✅ Complete | 6-stage pipeline, CozoDB graph, fastembed vectors, CLI |
| **Phase 2** — Serve + SDK | ✅ Complete | REST API, MCP server (SSE + stdio), Python SDK |
| **Phase 3** — Ecosystem | 🔨 Building | TypeScript SDK, GitHub Action, VS Code extension, safety engine |
| **Phase 4** — Cloud | 🔒 Planned | Dashboard, source connectors, multi-tenant (separate repo) |
| **Phase 5** — Enterprise | 🔒 Planned | SSO, compliance, air-gapped deploy (separate repo) |

Phases 1–3 are fully open source (this repo). Star to follow progress.

---

## Contributing

We welcome contributions. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide.

**Quick path:**

1. Fork and clone
2. `cargo test` to verify everything passes
3. Make your change (see [CONTRIBUTING.md](CONTRIBUTING.md) for where things live)
4. `cargo fmt --all && cargo clippy --workspace -- -D warnings`
5. Submit a PR

For bugs use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.yml).  
For ideas use [GitHub Discussions](https://github.com/thinkingroot/thinkingroot/discussions).

---

## License

Licensed under either of:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.

---

<div align="center">

Built with care by [Naveen](https://github.com/naveen) and contributors.

*If ThinkingRoot saves your agents tokens, consider starring the repo.*

</div>
