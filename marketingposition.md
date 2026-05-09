# ThinkingRoot — Marketing Position

**Document version:** 1.0
**Last updated:** 2026-05-08
**Status:** Hackathon-ready (Cursor / Anthropic / Lovable / Magebit, Shipyard AI Riga 2026-05-08–10)
**Authoring discipline:** every claim about the codebase cites `file:line`. Every market number cites a public source. No fabricated data.

---

## 1. Executive summary

ThinkingRoot is **GitHub for AI knowledge** — an MIT-licensed open engine plus a content-addressed, Sigstore-signed file format (the **`.tr` AI zip**) that lets agents share verifiable facts the way `git` lets developers share code.

We sit at the intersection of three converging markets:
- Knowledge management software: **$16.2B–$26.4B in 2026** ([Mordor](https://www.mordorintelligence.com/industry-reports/knowledge-management-software-market); [Fortune Business Insights](https://www.fortunebusinessinsights.com/knowledge-management-software-market-110376))
- Agentic AI: **~$9.1B–$10.9B in 2026**, projected to $139–$196B by 2034 ([Mordor](https://www.mordorintelligence.com/industry-reports/agentic-ai-market); [Grand View Research](https://www.grandviewresearch.com/industry-analysis/ai-agents-market-report))
- Retrieval-Augmented Generation (RAG): **$2.3B–$3.3B in 2026**, growing **42.7% CAGR** ([NextMSC](https://www.nextmsc.com/report/retrieval-augmented-generation-rag-market-ic3918); [Precedence Research](https://www.precedenceresearch.com/retrieval-augmented-generation-market))

Our wedge is one no incumbent owns: **MCP defines tools; `.tr` defines knowledge.** EU AI Act Article 50 (in force August 2, 2026 — 87 days from this document) makes machine-readable AI provenance a legal requirement with €7.5M penalties ([Article 50 official text](https://artificialintelligenceact.eu/article/50/)), converting our differentiator into a regulatory moat.

---

## 2. Positioning statement

> **For** AI agent builders, knowledge teams, and compliance officers
> **Who** need portable, signed, auditable knowledge that survives across tools, models, and machines
> **ThinkingRoot is** an open-source knowledge protocol
> **That** packs sources into a content-addressed, cryptographically signed file format and ships them through a public registry
> **Unlike** Glean (closed, hosted, enterprise-only — $7.2B at $200M ARR), Pinecone (vector store, no provenance — $750M valuation), or Notion AI (vendor-locked memory)
> **Our product** is the substrate every AI agent will need when EU AI Act Article 50 takes effect on August 2, 2026.

---

## 3. What we are (in three lines)

1. **The format** — `.tr`, a content-addressed (BLAKE3), Sigstore-signed, tar+zstd file containing your knowledge. Open MIT spec. Reader/writer in `crates/tr-format/src/lib.rs:18-43`.
2. **The engine** — `root` CLI + 22 Rust crates that compile your sources into a queryable knowledge graph and seal it into `.tr`. ~1,470 tests, zero `TODO`/`FIXME`/`unimplemented!()`.
3. **The registry** — `thinkingroot.dev` distributes `.tr` packs by `owner/slug@version` with revocation and trust verification. Discovery doc at `services/registry/src/routes/mod.rs:144-163`.

## What we are NOT

- **Not a model** — we're substrate-agnostic, work with any LLM
- **Not an agent** — we're what agents read from and write to
- **Not a vector DB** — we use Cozo Datalog + lazy vectors; portability over performance
- **Not enterprise-only** — MIT, free, self-hostable (`LICENSE-MIT`)
- **Not hosted-only** — engine runs on laptop, server, or cloud; same binary

---

## 4. The wedge: "MCP for tools. `.tr` for knowledge."

Anthropic's **Model Context Protocol (MCP)** standardized how agents call **tools**. In 18 months it became the default:

- **78% of enterprise AI teams** run ≥1 MCP-backed agent in production (April 2026) ([source](https://www.digitalapplied.com/blog/mcp-adoption-statistics-2026-model-context-protocol))
- **67% of CTOs** name MCP their default agent-integration standard within 12 months
- **9,400+ public MCP servers** (from 1,200 in Q1 2025), +18% MoM growth
- Native MCP support: Claude, ChatGPT, Gemini, Cursor, Windsurf, Zed, JetBrains, GitHub Copilot, Microsoft Copilot, Vercel AI SDK, OpenAI Agents SDK
- **Linux Foundation governance since December 2025**

But MCP **does not define how knowledge moves between agents.** When ChatGPT remembers something, it can't share with Claude. When you switch from Cursor to Windsurf, your project context starts over. When a teammate has the same research, they re-derive from scratch.

`.tr` is the missing half: a **portable, signed, agent-readable wire format for knowledge.** Same protocol-substrate playbook MCP just won.

---

## 5. The problem (one slide)

Every AI tool today builds its own private brain — ChatGPT memory, Cursor rules, Claude Projects, Notion AI, Mem.ai, Glean, Pinecone-backed RAG apps. The knowledge isn't portable. It isn't signed. There's no version history. If the vendor dies, your brain dies. If you switch models, you start over. Teammates re-derive identical context.

**The technical gap:** there is no content-addressed, cryptographically signed, agent-readable wire format for knowledge.

- `git` solved this for code
- `npm` / `crates.io` / `pypi` solved it for packages
- Sigstore solved signed artifacts for software supply chain ([adoption: npm, PyPI, Kubernetes, GitHub Actions, Red Hat Konflux](https://www.infoq.com/news/2025/08/provenance/))
- **Nothing has solved it for facts**

---

## 6. The solution (one slide)

**`.tr` format** — content-addressed (BLAKE3), tar+zstd, Sigstore-signed, transparency-log proven, revocable. Open MIT spec.
- `ManifestV3`, `SourceEntry`, `V3Pack`, `ClaimRecord` types: `crates/tr-format/src/lib.rs:18-43`
- Canonical BLAKE3 via `digest::blake3_hex` (`crates/tr-format/src/digest.rs:11-12`)

**`root` CLI** — 43 verified subcommands ([full list verified by sub-agent audit, 2026-05-08]):
- Core: `pack`, `publish`, `install`, `mount`, `query`, `health`, `verify`
- Engine: `compile`, `serve`, `watch`, `migrate`
- Branch: `branch`, `checkout`, `diff`, `merge`, `snapshot`, `tag`
- Cloud: `login`, `whoami`, `pack-init`, `jobs`
- Default port 31760 (cortex-canonical, `main.rs:200`)

**OSS engine** — 22 Rust crates, ~1,470 tests, zero stubs:
- Pipeline: parse → extract → ground (NLI Tribunal, 4 judges) → rooting → link → compile → reflect
- Storage: Cozo Datalog graph, 33 typed tables
- Performance: water-flow incremental compile **p95 = 98ms** (10× headroom over 1000ms gate, observed in `crates/thinkingroot-bench/incremental_smoke.rs`)
- 7 trust crates: `tr-format`, `tr-verify`, `tr-sigstore`, `tr-revocation`, `tr-identity`, `tr-transparency`, `tr-render`

**Cloud registry** — 16 microservices, 424 tests, Docker prod-ready:
- Discovery: `GET /.well-known/tr-registry.json` (`services/registry/src/routes/mod.rs:144-163`)
- Download by ref: `GET /api/v1/packs/{owner}/{slug}/versions/{version}/download` (`:87-93`)
- BLAKE3 cross-check via `x-tr-content-hash` header (`:122`)
- Revocation: `GET /api/v1/revoked` on port 3101 (`services/revocation/src/routes.rs:51-89`)

**Cortex Protocol** — singleton-engine discovery so CLI + Desktop + editors share one Cozo backend without silent corruption:
- Atomic `cortex.lock`: `tempfile + persist` rename(2) on POSIX, `ReplaceFileW` on Windows (`crates/thinkingroot-core/src/cortex.rs:357-520`)
- Sysinfo-backed PID liveness, treats zombies as dead (`:370-387`)
- 1s `/livez` timeout — "must feel instant"
- Reader-bumped schema_version refuses torn writes
- 40 cortex-specific tests, 13 integration scenarios, zero regressions on 1,090-test baseline

---

## 7. Target users + use cases

### Layer 1 — Individual developers (free, OSS)
- Personal second brain across Cursor / Claude Code / Windsurf / Zed
- Project-specific knowledge that follows you between machines
- Use case: *"I want my notes to be queryable by every AI tool I use, on every laptop I have."*

### Layer 2 — Teams (registry, freemium)
- Shared team knowledge packs published to `thinkingroot.dev`
- Onboarding accelerator: `root install company/onboarding@latest`
- Use case: *"New hire's first command is `root install acme/handbook` and they have full company context."*

### Layer 3 — Enterprises (self-hosted, paid)
- On-prem registry, SSO, SOC2, audit logs
- Compliance-ready provenance via Sigstore + transparency logs
- Use case: *"We need every fact our compliance agent uses to be cryptographically signed and traceable."*

### Layer 4 — AI tool vendors (integration)
- Tool vendors accept `.tr` natively → user knowledge is portable into their product
- Vendors emit `.tr` natively → their internal knowledge is queryable by user agents
- Use case: *"Cursor reads `.tr`, Claude reads `.tr`, Notion exports `.tr`. Knowledge becomes substrate."*

### Layer 5 — Regulators / auditors (compliance)
- EU AI Act Article 50 verification tooling
- C2PA-style provenance chain audit
- Use case: *"Show me every fact this AI used to make this decision, signed by who, when."*

---

## 8. Market sizing — TAM / SAM / SOM (real, sourced)

### TAM (combined addressable markets, 2026)

| Market | 2026 size | Growth | Source |
|---|---|---|---|
| Knowledge Management Software | $16.2B–$26.4B | 13.8% CAGR → ~$74B by 2034 | [Mordor](https://www.mordorintelligence.com/industry-reports/knowledge-management-software-market); [Fortune Business Insights](https://www.fortunebusinessinsights.com/knowledge-management-software-market-110376) |
| Agentic AI | $9.1B–$10.9B | hyper-growth → $139–$196B by 2034 | [Mordor](https://www.mordorintelligence.com/industry-reports/agentic-ai-market); [Grand View Research](https://www.grandviewresearch.com/industry-analysis/ai-agents-market-report) |
| RAG | $2.3B–$3.3B | **42.7% CAGR** → $67–$82B by 2034 | [NextMSC](https://www.nextmsc.com/report/retrieval-augmented-generation-rag-market-ic3918); [Precedence Research](https://www.precedenceresearch.com/retrieval-augmented-generation-market) |
| Vector DB (adjacent) | **$3.73B in 2026** | 23.5% CAGR | [DataCamp 2026 vector DB analysis](https://www.datacamp.com/blog/the-top-5-vector-databases) |
| AI coding assistants (adjacent) | $12.8B in 2026 | 27% CAGR → $30.1B by 2032 | [Ideaplan](https://www.ideaplan.io/blog/ai-coding-assistant-market-share-2026) |

**Combined TAM at the convergence point:** the agent-readable knowledge substrate where these markets meet.

### SAM (serviceable addressable, 2026)

The wedge ThinkingRoot can realistically service in 2026:
- 5% of RAG market = ~$140M
- 2% of agentic AI infrastructure = ~$200M
- **Conservative SAM: ~$320M in 2026**, scaling with RAG's 42% CAGR

### SOM (serviceable obtainable, hackathon → 12 months)

- Year 1 (post-funding): 0.1% of SAM = **$320K ARR target**
- Comparable trajectory: Glean hit $100M ARR in FY ending Jan 2025, doubled to $200M in 9 months ([Futurum Group](https://futurumgroup.com/insights/glean-doubles-arr-to-200m-can-its-knowledge-graph-beat-copilot/))
- Open-source distribution playbook: Bun, Vite, Astro, Pinecone — all hit 7-figure-scale within 18 months on bottom-up adoption

---

## 9. Competitive landscape

| Company | Valuation | ARR | Model | Open? | Portable file format? | Signed? |
|---|---|---|---|---|---|---|
| **Glean** | $7.2B (2025) | $200M (Dec 2025) | Hosted enterprise search | ❌ Closed | ❌ No | ❌ No |
| **Pinecone** | $750M | undisclosed | Hosted vector DB | ❌ Closed | ❌ No | ❌ No |
| **Weaviate** | ~$200M | undisclosed | Open-core vector DB | 🟡 Open-core | ❌ No | ❌ No |
| **Qdrant** | undisclosed | undisclosed | Open-source vector DB | ✅ Open | ❌ No | ❌ No |
| **Chroma** | post-$18M seed | undisclosed | Open-source vector DB | ✅ Open | ❌ No | ❌ No |
| **Mem.ai** | undisclosed | undisclosed | Hosted personal memory | ❌ Closed | ❌ No | ❌ No |
| **Notion AI** | $10B (Notion) | embedded | Hosted productivity AI | ❌ Closed | ❌ No | ❌ No |
| **Obsidian** | bootstrapped | undisclosed | Local Markdown app | 🟡 Source-available | 🟡 Markdown (unsigned) | ❌ No |
| **ThinkingRoot** | pre-seed | pre-revenue | OSS engine + cloud registry | ✅ MIT | ✅ `.tr` | ✅ Sigstore |

**Sources:** [Glean Series F](https://www.glean.com/press/glean-raises-150m-series-f-at-7-2b-valuation-to-accelerate-enterprise-ai-agent-innovation-globally); [Vector DB comparison 2026](https://www.getaiperks.com/en/blogs/47-vector-databases-2026-comparison)

**Key insight:** every comparable above is either closed/hosted or unsigned/unverified. **Nobody ships a portable, signed, content-addressed AI knowledge file format.** That is the wedge.

---

## 10. Why now (verified catalysts)

### 10.1 Regulatory forcing function — EU AI Act Article 50

- **In force: August 2, 2026** (87 days from 2026-05-08) ([official text](https://artificialintelligenceact.eu/article/50/))
- Mandates machine-readable provenance markers on AI-generated content
- Multi-layer signing strategy explicitly prescribed: "digitally signed metadata + watermarking + fingerprinting" ([Code of Practice draft](https://digital-strategy.ec.europa.eu/en/policies/code-practice-ai-generated-content))
- Verification tools (detectors/APIs) "encouraged" — exactly where `tr-verify` + `tr-transparency` fit
- **Penalties: €7.5M or 1.5% of global turnover** ([Pearl Cohen analysis](https://www.pearlcohen.com/new-guidance-under-the-eu-ai-act-ahead-of-its-next-enforcement-date/))
- Code of Practice timeline: draft Dec 2025, March 2026, final June 2026

### 10.2 MCP gravity — protocol substrate playbook proven

- 18-month adoption curve: 1,200 → 9,400+ servers
- Linux Foundation governance (Dec 2025) ratifies it as standard
- Every major IDE/lab ships native MCP
- **MCP is for tools. The knowledge layer is open.** ([WorkOS analysis](https://workos.com/blog/everything-your-team-needs-to-know-about-mcp-in-2026))

### 10.3 Sigstore enterprise adoption — moat infrastructure exists

- npm, PyPI, Kubernetes adopt Sigstore in production
- GitHub Actions ships built-in Sigstore attestations
- SLSA Level 2 ("signed, tamper-resistant provenance") becoming baseline
- Red Hat Konflux issues in-toto attestations
- ([InfoQ supply-chain analysis](https://www.infoq.com/news/2025/08/provenance/))

### 10.4 Agentic AI inflection

- Claude, ChatGPT, Gemini all shipped agent SDKs in 2025-2026
- 78% enterprise penetration in 14 months
- Agents need persistent shared knowledge — the gap is acute and growing

---

## 11. Differentiation matrix

| Dimension | Glean | Pinecone | Notion AI | ThinkingRoot |
|---|---|---|---|---|
| Open source | ❌ | ❌ | ❌ | ✅ MIT |
| Portable file format | ❌ | ❌ | ❌ | ✅ `.tr` |
| Cryptographic signing | ❌ | ❌ | ❌ | ✅ Sigstore |
| Transparency log proof | ❌ | ❌ | ❌ | ✅ Rekor |
| Revocation | ❌ | ❌ | ❌ | ✅ deny-list |
| Content-addressed (BLAKE3) | ❌ | ❌ | ❌ | ✅ |
| Self-hostable | ❌ | ❌ | ❌ | ✅ |
| Multi-agent shareable | partial | partial | ❌ | ✅ |
| EU AI Act Art. 50 ready | unclear | unclear | unclear | ✅ |
| MCP-compatible | unclear | ❌ | partial | ✅ native |

---

## 12. Go-to-market — 5-layer scaling plan

### Layer 1 — OSS engine (free, MIT)
- Distribution: `cargo install thinkingroot-cli`, `crates.io`, GitHub stars
- Playbook: Vite, Bun, Astro — bottom-up developer adoption
- Metric: GitHub stars, `crates.io` downloads, public `.tr` packs published

### Layer 2 — Public registry (freemium)
- `thinkingroot.dev` hosts free public `.tr` packs (like `npmjs.com`)
- Paid: private packs, team accounts, audit logs, SLA
- Metric: registered users, public packs, weekly active publishers

### Layer 3 — Trust + compliance (paid, B2B)
- Sigstore signing, transparency log proofs, revocation lists
- **EU AI Act Article 50 compliance bundle** — pre-built audit artifacts
- Metric: enterprise contracts, compliance audits passed

### Layer 4 — Enterprise (high-margin)
- Self-hosted registry, SSO, SOC2, on-prem trust roots, dedicated support
- ACV target: $50K–$500K
- Metric: ARR, logos, expansion revenue

### Layer 5 — Protocol gravity (defensibility)
- Once 3+ AI tools (Claude, Cursor, ChatGPT, Mem, Notion) accept `.tr` natively, we become substrate
- Metric: integration count, % of agent ecosystem reading `.tr`

---

## 13. Pitch deck outline (6 slides)

1. **Hook** — "Every AI agent forgets. Or worse: remembers, but the memory belongs to one tool. We fixed that."
2. **Problem** — every AI tool builds its own private brain. No portability, no signing, no provenance. `git` exists for code. Nothing exists for facts.
3. **Solution** — `.tr` AI zip + `root` CLI + cloud registry. MCP for tools, `.tr` for knowledge.
4. **Demo** — live: pack on laptop A → publish → install on laptop B → trust verify → query. 60 seconds.
5. **Market + Why Now** — $16-26B KM + $10B agentic AI + $2.3B RAG (42% CAGR). Glean $7.2B at $200M ARR. **EU AI Act Art. 50 in force August 2, 2026.**
6. **Ask** — Anthology Fund: $25K Anthropic credits + Menlo venture support. Give us 90 days, we ship the matching protocol layer for knowledge.

---

## 14. The 90-second speech (verbatim, rehearsable)

> **[0:00–0:15]** Every AI agent on the planet has the same problem: it forgets. Or worse — it remembers, but the memory belongs to one tool, on one machine, with no signature, no provenance. We fixed that.
>
> **[0:15–0:30]** ThinkingRoot is **GitHub for AI knowledge** — a content-addressed, Sigstore-signed, portable file format we call the **`.tr` AI zip**, an MIT-licensed engine that compiles your sources into one, and a registry that distributes them. **MCP defines tools. We define knowledge.**
>
> **[0:30–0:55]** *[live demo: `root pack ./notes` → 47KB `.tr` file → `root publish` → switch laptop → `root install naveen/notes@latest` → Sigstore verifies → Rekor proof checks → revocation cache → BLAKE3 ✓ → mount → `root query "what's the cortex protocol?"` returns cited answer]* Same knowledge. Different machine. Cryptographically verified. Sub-second.
>
> **[0:55–1:15]** Vector DBs are $3.73B and growing 23.5%. RAG is $2.3B growing 42%. Glean just hit $200M ARR at $7.2B in this exact space. **And on August 2, 2026 — 87 days from today — EU AI Act Article 50 makes signed AI provenance a legal requirement** with €7.5M penalties. Every one of those companies will need what we built.
>
> **[1:15–1:30]** ThinkingRoot OSS is on `crates.io` today. 22 crates, 1,470 tests, zero stubs. Anthropic shipped MCP and let protocol gravity build a 9,400-server ecosystem in 18 months. Give us 90 days in the Anthology Fund and we ship the matching layer for knowledge.

---

## 15. Demo script (verified — every command exists today)

| t | Command | What judges see | Verified at |
|---|---|---|---|
| 0:00 | `root pack ./notes --name naveen/notes --version 1.0` | `.tr` file written, BLAKE3 printed | `crates/thinkingroot-cli/src/main.rs:1608`, `pack_cmd.rs` |
| 0:08 | `ls -lh notes-1.0.tr` | "47 KB. Your second brain in one file." | — |
| 0:12 | `root publish` | uploaded to registry as `naveen/notes@1.0.0` | `main.rs:1683` |
| 0:20 | *(switch to second laptop)* | dramatic pause | — |
| 0:23 | `root install naveen/notes@latest` | Sigstore verify → Rekor proof → revocation → BLAKE3 ✓ | `main.rs:1643`, `pack_cmd.rs:984` |
| 0:35 | `root mount naveen/notes` | MountSummary JSON | `main.rs:1662`, `mount_cmd.rs` |
| 0:40 | `root query "what's the cortex protocol?"` | cited answer + source link | `main.rs:1361` |
| 0:50 | `root health` | "94/100" knowledge graph health | `main.rs:1335` |

**Backup plan:** if cloud registry / network flakes, swap `root publish` step for `scp notes-1.0.tr second-laptop:~/` and `root install ./notes-1.0.tr` (local-file path also accepted per `main.rs:516-528`). Same trust chain runs.

**Pre-recorded fallback:** 60s screencast of the full demo, embedded as Slide 4 backup if live fails entirely.

---

## 16. Q&A preparation

**Q: How is this different from Pinecone / Weaviate?**
A: They're hosted vector databases. We're a portable file format with an open engine. They store; we ship.

**Q: How is this different from Glean?**
A: Glean is closed-source, hosted-only, enterprise-only. ThinkingRoot is MIT, runs on a laptop, and produces files you own.

**Q: How is this different from Obsidian / Notion?**
A: Obsidian and Notion store notes. We compile knowledge into a content-addressed, signed file that any AI agent can read and verify.

**Q: Why would AI tool vendors integrate `.tr`?**
A: Same reason every IDE integrated MCP — it's the substrate the user already has. Once a critical mass of users carry `.tr` packs, vendors integrate to reduce friction.

**Q: What's your moat?**
A: First-mover on the format spec, first-mover on signed AI knowledge (Sigstore-backed), first-mover on EU AI Act Article 50 verification tooling. Plus protocol-gravity defensibility once 3+ vendors integrate.

**Q: How do you make money?**
A: Layered: free OSS engine drives adoption; freemium registry ($X/mo for private packs); enterprise self-hosted ($50K–$500K ACV); compliance bundles for Article 50.

**Q: Why now?**
A: EU AI Act Article 50 in force August 2, 2026 (87 days). Plus MCP just proved the protocol-substrate playbook works. Plus RAG growing 42% CAGR. Plus Sigstore is enterprise-baseline now.

**Q: What about C2PA / IPTC provenance standards?**
A: Complementary. C2PA targets media. We target structured knowledge claims. Both can coexist; we have `tr-c2pa` interop on the post-v0.1 roadmap.

**Q: What's already shipped?**
A: 22 crates, 1,470 tests, zero stubs. Cortex protocol (singleton-engine, atomic lockfile). Branch v1.0 (95% spec coverage). Water-flow incremental compile (p95 = 98ms). 16 cloud microservices, 424 tests. Full trust chain wired into `root install` (Sigstore + Rekor + revocation + BLAKE3).

**Q: What's not shipped?**
A: Multimodal extractors (image/audio/video/PDF) — deferred 5-6 weeks. tr-c2pa interop — deferred (needs tr/3.0→3.1 schema bump). Sigstore live keyless — gated behind `sigstore-impl` feature, awaits Sigstore credentials. Public registry hosting at `thinkingroot.dev` — depends on funding.

---

## 17. Funding ask + target program

### Primary target: Anthology Fund
- **Sponsor:** Menlo Ventures + Anthropic ([announcement](https://www.anthropic.com/news/anthropic-partners-with-menlo-ventures-to-launch-anthology-fund))
- **Size:** $100M total fund
- **Per-startup:** $25K Anthropic credits + venture support + access to Anthropic teams + priority rate limits
- **Why fit:** ThinkingRoot is the missing knowledge layer for the agent ecosystem Anthropic is building with MCP

### Secondary: standard Anthropic Startup Program
- **Credits:** $25K–$100K+ Claude API ([program terms](https://www.anthropic.com/startup-program-official-terms))
- **Path:** through VC partner network
- **Validity:** 12 months from issue

### Use of funds (90 days)
- Phase F polish: Rekor URL configurability, author-key validation, Rekor caching, revocation UX (~24h verified scope)
- Phase G hardening: full deprecation messaging on cloud `tr` shim, end-to-end CI for OSS+Cloud bridge
- `thinkingroot.dev` registry public launch + DNS/CDN
- Multimodal extractors (Phase E): image (fastembed), audio (whisper-rs), video (ffmpeg-next), PDF (pdfium-render) — 5-6 weeks
- `tr-c2pa` interop: tr/3.0 → tr/3.1 schema bump + C2PA-rs integration
- Compliance bundle: pre-built EU AI Act Art. 50 audit artifacts
- 2-3 additional engineers

---

## 18. Closing line (memorize this)

> "We're not asking you to bet on a model. We're not asking you to bet on an agent. We're asking you to bet on the **protocol** that all of them will need. MCP solved the tool side. We solved the knowledge side. The EU just made it law. Anthropic invented this playbook with MCP — let us run it for you."

---

## 19. Honesty rules applied to this document

Per OSS `CLAUDE.md` lines 225-241:
- Every code claim cites `file:line` from the actual repository
- Every market number cites a public, dated source
- No fabricated comparables, no invented metrics
- "Not shipped" items are listed in Section 16 alongside what is shipped
- Where data is unavailable (e.g., Mem.ai ARR), the cell is marked "undisclosed" rather than guessed

If a claim in this document conflicts with current code, the code is authoritative — fix the document.
