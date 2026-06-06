# Three-Way Comparison: Obsidian vs Obsidian+Mind vs ThinkingRoot
*April 2026 | Based on live GitHub data, READMEs, codebase survey*

---

## The Core Question Each Tool Answers

| Tool | Core question |
|---|---|
| **Obsidian** | "Where do I put my thoughts?" |
| **Obsidian + obsidian-mind** | "How do I make Claude remember everything about my work?" |
| **ThinkingRoot** | "How do an AI agent understand my codebase without me explaining it?" |

---

## Full Feature Matrix

| Capability | Obsidian | Obsidian + Mind | ThinkingRoot |
|---|---|---|---|
| **Writing interface** | Full rich editor | Full rich editor | None |
| **Human types notes** | Yes | Yes (with routing) | No |
| **AI writes notes** | Via plugin only | Yes (hook-validated) | No |
| **Code → knowledge** | No | No | Yes (full pipeline) |
| **Graph view** | Live, from `[[links]]` | Live, from `[[links]]` | Read-only D3 (compiled) |
| **Backlinks** | First-class | First-class | Missing (outbound only) |
| **Typed relations** | No (untyped wikilinks) | No (untyped wikilinks) | Yes (13 types: DependsOn, Calls, Implements…) |
| **Confidence scores** | No | No | Yes (0.0–1.0 per claim) |
| **Claim validity window** | No | No | Yes (valid_from / valid_until) |
| **Contradiction detection** | No | No | Automatic |
| **Health scoring** | No | No | Yes (freshness × consistency × coverage × provenance) |
| **Staleness detection** | No | No | Yes (configurable days threshold) |
| **AI session memory** | No | Yes (hooks inject 2K tokens) | Partial (via compiled artifacts) |
| **Knowledge routing** | No | Yes (5 hooks + classifier) | No |
| **Token-efficient loading** | No | Yes (tiered: 2K always, QMD on-demand) | No |
| **Slash commands** | Via plugins | 18 built-in commands | CLI only |
| **Subagents** | No | 9 specialized subagents | No |
| **Knowledge versioning** | Manual git | Manual git | Built-in KVC (branch/diff/merge) |
| **Semantic diffs** | No | No | Yes (new_claims, contradictions, entities) |
| **Health-gated merges** | No | No | Yes (blocks merge if health drops) |
| **PDF parsing** | Via plugin | Via plugin | Native (Stage 1) |
| **Code parsing** | No | No | Native (all languages) |
| **Git history parsing** | No | Via Slack/GitHub scripts | Native |
| **Semantic search** | Via plugin | Via QMD | Built-in (fastembed) |
| **REST API** | No | No | Full (15+ endpoints) |
| **MCP server** | Via plugin | Via plugin | Native (stdio + SSE) |
| **Python SDK** | No | No | Yes (PyO3) |
| **Storage format** | Plain `.md` (portable) | Plain `.md` (portable) | CozoDB SQLite (binary) |
| **Requires compilation** | No | No | Yes (`root compile`) |
| **Requires LLM credentials** | No | Yes (Claude Code) | Yes (extraction stage) |
| **Live updates** | Real-time | Real-time | Batch (recompile) |
| **Mobile app** | Yes | Yes | No |
| **Plugin ecosystem** | 1800+ | 1800+ | API surface only |
| **Price** | Free core | Free (Claude Code needed) | Free (open source) |

---

## How Each One Builds Its Knowledge Graph

### Obsidian
You write a note. You add `[[AuthService]]` to link it. Done. Obsidian scans every `.md` for `[[links]]` and draws an edge. No AI, no extraction, no pipeline. The graph is exactly as rich as the human who built it.

### Obsidian + obsidian-mind
Claude Code is your co-author. When you say "we decided to use Postgres", a `UserPromptSubmit` hook classifies that as a Decision and routes it to `brain/Key Decisions.md` with a wikilink back to the active project. When you mention a person, it upserts `org/people/<Name>.md`. At session start, a `SessionStart` hook injects ~2K tokens of lightweight context (goals, active projects, git summary, task list). The vault grows through conversation — you talk, Claude writes structured notes.

**The vault is still manually curated, but Claude is doing the filing.**

```
Human talks → UserPromptSubmit hook classifies → Claude writes to correct folder
                                                    ↓
                                       PostToolUse validates (frontmatter + wikilinks)
                                                    ↓
                                       Graph grows from [[wikilinks]]
```

### ThinkingRoot
You point it at a codebase. The pipeline runs: parse every file → chunk → LLM extracts entities, claims, relations → link + deduplicate → compile artifacts → serve via REST + MCP. The graph is entirely machine-derived. No human writes anything to it.

```
Code/docs → parse → LLM extract → typed graph (CozoDB) → compile → serve
                                        ↓
              EntityTypes (13) + RelationTypes (13) + ClaimTypes (10)
              + confidence + validity + grounding score
```

---

## The Memory Problem: Three Different Solutions

All three are really about giving AI persistent memory. But each solves it differently:

| Approach | How | Quality | Effort |
|---|---|---|---|
| **Obsidian** | None (user does everything) | Human-quality, curated | High (you write it all) |
| **obsidian-mind** | Hooks route conversations to structured notes | Human-quality + AI-filed | Medium (you talk, Claude files) |
| **ThinkingRoot** | LLM extracts from existing code/docs | Machine-quality, automated | Low (just point and compile) |

obsidian-mind's `SessionStart` hook is doing the same job as ThinkingRoot's compiled artifacts — both are injecting context into an AI session. The difference: obsidian-mind injects *what you told Claude*, ThinkingRoot injects *what was already in the code*.

---

## Depth of Knowledge

This is where the tools diverge most sharply.

**Obsidian / obsidian-mind** know what you *said*:
```
brain/Key Decisions.md:
- We chose Postgres because the team knows it [[2026-03-15]]
```

**ThinkingRoot** knows what the *code actually does*:
```
Entity: UserRepository
  - Claim [ApiSignature]: fn find_by_email(email: &str) -> Result<User> (confidence: 0.92)
  - Claim [Dependency]: depends on PostgresPool (confidence: 0.88)
  - Relation: DependsOn → PostgreSQL (strength: 0.9)
  - Relation: Calls → bcrypt::verify (strength: 0.85)
  - Claim [Architecture]: implements Repository pattern (confidence: 0.76)
```

obsidian-mind has no idea `UserRepository` exists unless you told Claude about it. ThinkingRoot found it automatically.

Conversely, ThinkingRoot has no idea *why* you chose Postgres — that context lives in your brain/Key Decisions.md.

---

## Side-by-Side: Specific Scenarios

### Scenario 1: New engineer joins the team
- **Obsidian**: Useless unless someone already wrote documentation in the vault
- **Obsidian + Mind**: Useful if the vault has architecture notes and decisions from past conversations
- **ThinkingRoot**: Immediately useful — run `root compile` on the codebase, get entity pages, architecture map, dependency graph, all decisions extracted from code comments and docs

**Winner: ThinkingRoot**

### Scenario 2: I want to capture today's architecture discussion with Claude
- **Obsidian**: Write a note manually after the conversation
- **Obsidian + Mind**: Talk to Claude, hooks auto-route to `reference/` with wikilinks
- **ThinkingRoot**: Not designed for this — no way to inject a conversation into the graph

**Winner: Obsidian + Mind**

### Scenario 3: "What does AuthService depend on?"
- **Obsidian**: Only if you wrote it down (and remembered to link it)
- **Obsidian + Mind**: Only if you discussed it in a previous session and Claude filed it
- **ThinkingRoot**: `get_relations("AuthService")` → typed list with DependsOn, Calls, ConfiguredBy edges extracted from actual code

**Winner: ThinkingRoot**

### Scenario 4: "Something changed in the auth module — what knowledge is stale?"
- **Obsidian**: No way to know
- **Obsidian + Mind**: No way to know (unless you manually link notes to code files)
- **ThinkingRoot**: `root verify` → freshness score drops for auth-related claims, staleness warnings surface automatically

**Winner: ThinkingRoot**

### Scenario 5: "Write my performance review"
- **Obsidian**: Manually compile notes
- **Obsidian + Mind**: `/om-review` command aggregates brag doc, competency evidence, 1:1 notes, win history — generates review draft
- **ThinkingRoot**: Cannot do this — no concept of wins, people, competencies

**Winner: Obsidian + Mind (by a mile)**

### Scenario 6: "Did we contradict ourselves in the codebase?"
- **Obsidian**: No
- **Obsidian + Mind**: No
- **ThinkingRoot**: Automatic — ContradictionReport artifact flags conflicting claims

**Winner: ThinkingRoot**

---

## Honest Weaknesses

### Obsidian alone
- The graph is only as good as the human who built it
- No AI understanding of what you wrote
- Knowledge goes stale silently
- Every note is an island until you explicitly link it

### Obsidian + obsidian-mind
- Still requires LLM to be running (Claude Code, not a background service)
- Knowledge quality depends on how well you talk to Claude — garbage in, garbage out
- Graph is still untyped wikilinks — semantically flat
- No contradiction detection (you can contradict yourself across 50 notes and never know)
- No code extraction — the vault knows what you *said*, not what the code *does*
- Session hooks add overhead (~300 tokens/message for classification)
- Fragile: rename a folder and the routing breaks

### ThinkingRoot
- No writing interface — you can't capture a thought, a decision, a meeting note
- One-directional: extraction only, no write-back
- Batch pipeline — not live, requires recompile
- Knowledge is machine-extracted — misses intent, context, human judgment
- Missing backlinks (incoming relations not queryable)
- Binary storage — not portable, can't open in another tool
- Needs LLM credentials to extract anything

---

## The Gap That Matters Most

Neither obsidian-mind nor ThinkingRoot alone is complete. The perfect system would be:

```
Human conversations + decisions → obsidian-mind captures → plain .md files
          +
Codebase + docs → ThinkingRoot extracts → typed graph
          =
One coherent knowledge graph where:
  - Entity "AuthService" has both:
      - Machine-extracted claims (what it does, what it calls)
      - Human-authored context (why it exists, who owns it, past incidents)
```

The bridge is **one template change in TR** + **one output path flag**:
```
root compile ./my-repo --output ./my-obsidian-vault/knowledge/
```

TR writes `[[wikilinks]]` in its entity pages → Obsidian renders the combined graph — machine knowledge + human knowledge in one view.

---

## When to Use Which

| Your situation | Use |
|---|---|
| Writing, thinking, personal knowledge base | Obsidian alone |
| Want Claude to remember your work decisions, people, projects | Obsidian + Mind |
| AI agents need to understand your codebase | ThinkingRoot |
| Need contradiction detection, health scoring, typed relations | ThinkingRoot |
| Need performance review automation, meeting notes, brag doc | Obsidian + Mind |
| Need REST API / MCP server for your knowledge graph | ThinkingRoot |
| Want Git-like versioning of knowledge | ThinkingRoot (KVC built-in) |
| Want both code knowledge + human context in one graph | Wait for the bridge (or build it) |

---

*Sources: GitHub API (live star counts April 2026), breferrari/obsidian-mind README, basicmachines-co/basic-memory README, ThinkingRoot codebase survey (commit 7eaaeb9, branch feat/grounding-tribunal)*
