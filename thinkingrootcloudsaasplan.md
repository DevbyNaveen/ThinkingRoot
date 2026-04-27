# ThinkingRoot Cloud — Full SaaS Build Plan

- **Filename:** `thinkingrootcloudsaasplan.md`
- **Date:** 2026-04-24
- **Status:** Final pre-build blueprint (pending founder sign-off on §XX open decisions)
- **Purpose:** The single source of truth for the ThinkingRoot Cloud / Hub launch. Everything required to build, ship, operate, and monetize. Companion to `docs/2026-04-24-tr-format-design-and-research.md` (TR-1 format spec) and `docs/2026-04-24-living-credits-design.md` (credit economy).
- **Hallucination policy:** Every figure labeled `[verified]` was confirmed from a vendor page or regulatory source within the last 72 hours. Every figure labeled `[unverified]` is a directional estimate flagged for manual check at checkout.

---

# Part I — Strategic Context

## I.1 The thesis (one paragraph)

ThinkingRoot Cloud is a **GitHub + Hugging Face fusion for AI knowledge**: users compile local sources into portable, signed `.tr` files; publish, fork, review, and merge them on `thinkingroot.dev`; and connect any MCP-capable AI agent (Claude Desktop, Cursor, VS Code, ChatGPT) with one click. The moat is **Rooting** — a deterministic 5-probe admission gate that produces per-claim BLAKE3 certificates, already shipped in OSS as `v0.1.0-rooting`. The economic flywheel is **Living Credits** — the first AI infrastructure pricing system where contributors earn compute when their knowledge helps others.

## I.2 Verified market opening

All facts below confirmed from primary sources within 72 hours.

| Fact | Source |
|---|---|
| Mem0 raised $24M (YC, Peak XV, Basis Set, Oct 28 2025) | TechCrunch |
| Mem0 Issue #4573 (Mar 27 2026) — 97.8% junk rate audit, unlabeled, no vendor response | github.com/mem0ai/mem0/issues/4573 |
| Zep Community Edition deprecated April 2025 | getzep.com/pricing blog |
| Shai-Hulud self-replicating npm worm (Sept 16 + Nov 24 2025) — 492 packages, ~132M monthly downloads infected | CISA alert, Krebs, Sysdig |
| Rekor does not support revocation by design | docs.sigstore.dev/about/security |
| Trusted Publishing via OIDC (npm + crates.io adopted July 2025) | repos.openssf.org |
| NIST AI Agent Standards Initiative launched Feb 2026, no blessed interchange format | nist.gov |
| Anthropic commercial ToS prohibits resale outside AWS Bedrock Authorized Reseller channel | anthropic.com |
| Cursor negative ~30% gross margin reported Aug 2025 | TechCrunch, Foundamental via PitchBook |
| GDPR Article 33 mandates 72-hour breach notification | eur-lex.europa.eu |
| UK Online Safety Act user-to-user safety duties in force Mar 17, 2025 | Ofcom |
| EU AI Act GPAI obligations active Aug 2, 2025 | EU Commission |
| US 18 USC §2258A — mandatory CSAM reporting to NCMEC, fines up to $600–850K (REPORT Act 2024) | law.cornell.edu |
| DMCA designated agent fee $6, 3-year renewal, 37 CFR §201.38 | dmca.copyright.gov |

## I.3 Competitive positioning (one-line wedge per competitor)

- **vs. Mem0:** "We pay you back in compute when your knowledge helps others. No 97.8% junk."
- **vs. Zep:** "We didn't kill our OSS. We built it."
- **vs. Letta:** "Agent state is one thing. Full knowledge graphs are another."
- **vs. HuggingFace:** "Knowledge, not models. With per-claim cryptographic certificates."
- **vs. GitHub:** "For knowledge, not code. Facts are diff-able, merge-able, and executable by AI."

---

# Part II — Product Architecture

## II.1 The three stacked products

1. **TR-1 `.tr` file format** (OSS) — the portable artifact users hold. Specified in `docs/2026-04-24-tr-format-design-and-research.md`.
2. **Cloud backend + Hub** (private repo) — `thinkingroot.dev` registry for publishing, forking, Knowledge PRs, agent connections, billing.
3. **Rooting moat** (OSS engine + private hub surfaces) — already shipped in `v0.1.0-rooting`; cloud extends with continuous re-rooting worker + badges.

## II.2 Surface stack (who uses what)

| User segment | Primary surface | Build status |
|---|---|---|
| Power devs / SRE | **CLI (`root`)** | 80% shipped — needs onboarding wizard + hub commands |
| Working devs | **VS Code extension** | 0% — new stream |
| Junior devs / students / researchers | **Web app (upload + compile in browser)** | 0% — part of web stream |
| Non-technical knowledge workers | **Desktop app** (Tauri tray + daemon) + browser extension (defer) | 0% — week-2 stream |
| AI agents (consumption) | `.tr` + MCP protocol | 0% — part of TR-1 stream |

**Desktop app vs Claude Desktop:** explicit clarification — **both exist, different roles**. Claude Desktop is Anthropic's chat client. ThinkingRoot Desktop is a Docker-Desktop-analog tray app that runs our local daemon, manages workspaces, handles `.tr` double-click, shows privacy dashboard. We never build a chat UI.

---

# Part III — Feature Specifications

## III.1 TR-1 `.tr` file format

Reference: `docs/2026-04-24-tr-format-design-and-research.md`. Summary:

- Single signed portable file bundling: `manifest.json` + `graph/` + `vectors/` + `artifacts/` + `provenance/` + `signatures/` + `.mcpb/`
- Dual-identity: valid `.tr` is also a valid `.mcpb` bundle → double-click on Claude Desktop auto-mounts as MCP server
- Trust tiers T0 (unsigned) → T2 (Sigstore keyless, default) → T4 (source bytes included for re-rooting)
- Size: ~55 MB for LongMemEval-scale (700K claims) via MRL-256 + BBQ 1-bit + int8 residual
- Container: tar.zst at v0.1 → CAR v1 + zstd-seekable at v0.5 (HTTP Range streaming)
- **Status:** spec locked, 0% built. First launch stream (Stream A, 2 engineers, 1 week to v1.0).

## III.2 Hub UI & Pack Page Design

### III.2.1 Pack page — 10 tabs (GitHub + HF fusion)

```
priya/thesis    Public   v1.2.0   🌳 4,730 living credits
[Star 47] [Fork 12] [Watch 8] [Install to Claude ▼] [Use SDK ▼]

📄 Card  📁 Files  🎖 Certificates  📚 Sources  🔀 PRs
🕸 Graph  🚀 Versions  📊 Insights  💬 Discussions  ⚙ Settings
```

| Tab | Content |
|---|---|
| **Card** (default) | Rendered `knowledge.card.md` + Rooting % stacked bar + Health score + Install buttons + Clone snippet |
| **Files** | Browsable `.tr` internal folder tree (graph/, artifacts/, provenance/, signatures/, .mcpb/) with Rooting % per file |
| **Certificates** | Filterable table of all per-claim BLAKE3 certificates + 5-probe drilldown |
| **Sources** | Provenance chain — source URIs, byte ranges, who contributed what claim |
| **PRs** | Fact-level Knowledge PRs (claim-level diff, contradiction-as-conflict, auto-resolve hints) |
| **Graph** | 3D graph explorer (react-force-graph-3d) — tab, not default |
| **Versions** | SemVer tags + BLAKE3 digests + compile runs history |
| **Insights** | Claim growth over time, Rooting tier history, top entities, agent query analytics |
| **Discussions** | Pack-page comments (defer full Discussions post-launch) |
| **Settings** | Visibility, collaborators, webhooks, tags, license, delete |

### III.2.2 Discovery / homepage

- Hero + tagline: "The PDF for AI Knowledge"
- Trending this week (top 12 packs by mounts + Rooting-weighted)
- Featured collections (CS students, AI research, team starters)
- Top Patrons leaderboard (top 10 creators by living credits earned)
- Search bar (Meilisearch lexical + Qdrant semantic + Rooting % boost)
- Stats footer (packs published, agents connected, queries/day)

### III.2.3 Create repo flow (GitHub-analog)

Three paths for first pack:
1. **Push from local** — `root init` → `root compile` → `root remote add` → `root push`
2. **Upload in browser** — drag folder/zip → server spawns ephemeral compile worker → pack published
3. **Auto-compile from GitHub** — webhook on push → recompile → publish (selected at create-time)

## III.3 CLI + Onboarding Wizard

First-run `root` (no args) presents:

```
Welcome to ThinkingRoot — the PDF for AI Knowledge.

How will you use ThinkingRoot?
  › Just me, local-only          (0.117ms, offline, no cloud)
    Me + my team                 (cloud-synced, share via thinkingroot.dev)
    Me + Claude Desktop          (auto-mount as MCP)
    Everything (recommended)

? Which AI provider should extract your knowledge?
  › Anthropic Claude Haiku 4.5  [recommended — cheapest + fastest]
    OpenAI / Azure OpenAI
    AWS Bedrock (Claude via reseller)
    Ollama (local, free, slower)
    ... 11 total

? Sign in / sign up to thinkingroot.dev:
  › Continue with GitHub          (recommended)
    Continue with email
    Skip — local only for now

? Install MCP servers?
  [x] Claude Desktop            detected
  [x] Claude Code               detected
  [ ] Cursor                    detected
  [ ] VS Code (Copilot)         optional

⠸ Compiling ~/Desktop/myapp...
✓ Ready. Try: root ask "what does auth.rs do?"
         or: root push
```

### III.3.1 Git-style command mapping

```
git init                      →  root init
git commit                    →  root compile
git tag v1.0                  →  root tag v1.0
git log                       →  root log
git blame                     →  root blame <claim-id>
git branch                    →  root branch
git checkout                  →  root checkout
git diff                      →  root diff
git merge                     →  root merge
git remote add                →  root remote add
git push                      →  root push
git pull                      →  root pull
git clone                     →  root clone
git fetch                     →  root fetch
git stash                     →  root stash
```

(Rebase does not map — knowledge history is not linear.)

### III.3.2 Hub subcommands

`root hub {create, search, trending, browse, settings}` · `root {fork, pr create, pr list, pr view, pr merge, pr close, star, follow}` · `root {login, logout, whoami, credits, gift}`

## III.4 Non-CLI Surfaces

### III.4.1 VS Code extension (verified highest-leverage)

Install counts of comparable extensions [verified live from VS Code Marketplace]: Copilot 73M, Cline 3.7M, Windsurf 3.7M, Continue 2.7M, Roo Code 1.5M.

Features:
- Right-click folder → "Compile with ThinkingRoot" (calls local `root compile`)
- Sidebar panel: pack list, health badges, Rooting % per pack
- Inline "Ask" command on selection → queries via MCP
- One-click "Publish to Hub" → `root push`
- Status-bar widget: token meter + Rooting tier

### III.4.2 Web app

Full thinkingroot.dev surface. Key non-CLI path: `/new` wizard includes drag-and-drop upload that spawns server-side compile worker, lets student/researcher publish without ever installing CLI.

### III.4.3 Desktop app (Tauri wrapper, Docker-Desktop analog)

**Role:** local daemon manager. Not a hub UI, not a chat client.

Features:
- Tray icon (green = daemon running, blue = syncing, red = Rooting rejection detected)
- System status: local workspaces, mounted packs, connected agents
- `.tr` file association (macOS UTI + Windows registry) → double-click shows preview + "Mount to Claude Desktop" button
- **Privacy dashboard** (differentiator): per-workspace visibility of local-only vs cloud-synced, LLM provider + key location, total bytes ever left machine
- Auto-update manager for `root` binary, embedding models, fastembed weights

**Week-1 minimum:** file-association handler (CLI registers UTI). **Week-2 ship:** full Tauri app.

### III.4.4 Browser extension (deferred to week 3–4)

Capture-to-pack pattern (Readwise/Glasp analog). Click page → extract → push to designated pack.

## III.5 Living Credits System

Full spec: `docs/2026-04-24-living-credits-design.md`. Summary:

### III.5.1 Core mechanic

Credits regenerate when other users mount or query your pack. Formula:

```
+10    per mount by verified user
+0.1   per query
+50    per fork
+100   per merged Knowledge PR you authored
+25    per star-count milestone
×1.5   with 7-day compile streak
×2.0   with 30-day compile streak
×3.0   Patron status (top-100)
× (pack_rooted_pct / 100)  quality gate
```

### III.5.2 Psychology stack (literature-grounded)

Inverted loss aversion (Kahneman), endowed progress (Nunes & Drèze 2006), variable reward (Skinner), sunk-cost upsell (Arkes & Blumer 1985), social proof (Cialdini), goal gradient (Kivetz et al. 2006), streak gradient (Duolingo pattern), IKEA effect (Norton/Mochon/Ariely 2012), status economy (Veblen), reciprocity, zero-price anchor (Ariely).

### III.5.3 Credit Tree UX

Visual branches per pack, real-time leaves when someone mounts. Screenshots get posted = free marketing.

### III.5.4 Risk mitigations (all 10 specified in Living Credits doc §8)

Sock-puppet farming → GitHub-OAuth weighting + IP distinct-pairs + daily caps (Spotify pattern). Low-quality spam → Rooting-weighted regen, <50% Rooted earns zero. Viral runaway → 100K/mo regen cap converts to Patron status. GAAP liability → credits are non-redeemable-for-cash compute (Discord Nitro pattern). Regulatory (gambling) → non-transferable, one-way cash-out only at Ultra Pro. Orphan regen on deletion → freeze by default. Perceived "fake currency" → Credit Tree UX + transparency page. Anthropic cutoff → BYOK fallback. Bot-query farming → authenticated session required + per-user-per-pack daily cap. Plagiarism → provenance chain + DMCA flow.

## III.6 LLM Cost Model (Tiered BYOK + Managed)

**Verified constraint:** Anthropic commercial ToS prohibits resale outside AWS Bedrock Authorized Reseller channel. OpenAI has no public resale program. Cursor's negative ~30% margin proves managed-only loses money at list price.

| Tier | LLM source | Economics |
|---|---|---|
| **Free** | BYOK (user's Anthropic/OpenAI/Azure key) or Ollama (local) | Zero LLM cost to us |
| **Pro $19/user/mo** | Bundled 500K tokens Claude Haiku 4.5 via **AWS Bedrock Authorized Reseller** + BYOK overflow | ~$2.50 hard cost, ~$16.50 gross margin |
| **Ultra Pro $49/user/mo** | Bundled 5M tokens + priority workers + **Living Credits cash-out above 100K** | Priority-queue SLA |
| **Team $99/org/mo + $9/user** | Bundled 10M token org pool + SSO-lite via Google Workspace | |
| **Enterprise custom** | BYOK via customer's own Bedrock account | 100% margin on seats |
| **Patron (earned)** | Unlimited, top-100 creators | Bounded via global cap |

Why Bedrock, not direct Anthropic: **Windsurf had direct Claude access cut in June 2025 with little notice** [verified TechCrunch]. Bedrock Authorized Reseller provides cover.

---

# Part IV — Technical Architecture

## IV.1 Ops stack (all prices verified unless flagged)

| Layer | Service | Cost (seed) | Source |
|---|---|---|---|
| App hosting | **Fly.io Machines** shared-cpu-1x 1GB | $5.70/mo/machine | fly.io/docs/about/pricing [verified] |
| Postgres + PITR | **Neon Launch** | $0.106/CU-hr + $0.35/GB-mo, **7-day PITR** | neon.com/pricing [verified] |
| Redis | **Upstash** | Free tier → $10/mo fixed | upstash.com/pricing/redis [verified] |
| Object store | **Cloudflare R2** | 10GB free, $0.015/GB-mo, **$0 egress** | developers.cloudflare.com/r2/pricing [verified] |
| Search (lexical) | **Meilisearch Cloud** | from $30/mo (or OSS self-host free) | meilisearch.com/pricing [verified] |
| Search (semantic) | **Qdrant Cloud** | Free 1GB cluster forever | qdrant.tech/pricing [verified] |
| Email transactional | **Resend Pro** | $20/mo for 50k | resend.com/pricing [verified] |
| Email drip | **Loops** | Free 1k contacts → scales | loops.so/pricing [verified] |
| Error tracking | **Sentry Team** | $26/mo annual, 50k errors | sentry.io/pricing [verified] |
| Uptime | **BetterStack** | Free + $29/mo responder | betterstack.com/pricing [verified] |
| Logs | **Axiom Personal** | Free 500GB/mo, 30-day retention | axiom.co/pricing [verified] |
| Status page | **Instatus** | Free 15 monitors | instatus.com [verified] |
| On-call | **PagerDuty** | Free 5 users | pagerduty.com/pricing [verified] |
| Feature flags | **GrowthBook OSS** | self-host free | growthbook.io/pricing [verified] |
| Secrets | **Doppler Developer** | Free 3 users | doppler.com/pricing [verified] |
| IaC | **Pulumi Individual** | Free | pulumi.com/pricing [verified] |
| Docs | **Docusaurus** (OSS) | Free | docusaurus.io [verified] |
| Analytics | **PostHog** | Free 1M events/mo | posthog.com/pricing [verified] |
| Support | **Plain Foundation** | $35/seat/mo annual | plain.com/pricing [verified] |
| CDN / WAF | **Cloudflare Free** → Pro $25/mo/domain [unverified 2026] | Free → $25 | cloudflare.com/plans [Pro unverified] |

**Seed total monthly: ~$155** (verified paid services only). Free-tier minimum: ~$14.

## IV.2 Scaling architecture (100 concurrent users)

### IV.2.1 Local / BYOK path
Compile runs on user's machine, hits user's own key. 100 concurrent = 100 parallel local pipelines, zero shared bottleneck. Only hub touchpoint is final `.tr` upload (4–50 MB typical) → R2 handles trivially.

### IV.2.2 Managed / Ultra Pro path
```
User → API → Redis job queue → Worker pool (Fly.io Machines) → Bedrock → .tr → R2

Protection:
  per-user concurrency cap: 2
  per-org cap: 10
  Free tier forces BYOK — cannot DoS managed pool
  Auto-scale workers (Fly.io Machines spin up <5s, billed per-second)
```

50-worker pool at Bedrock Claude Haiku → ~600 jobs/hour throughput → 100 users × 1 big file each drains in 10–15 min. User sees progress bar, not a spinner.

## IV.3 Observability

- **SLIs/SLOs published internally:** 99.9% API uptime, p95 query latency <200ms, p95 compile latency <5 min for 10K-claim pack
- **Paging paths:** BetterStack uptime → PagerDuty → on-call rotation
- **Metrics:** PostHog for product, Sentry for errors, Axiom for logs, Grafana Cloud optional for infra
- **Weekly report:** auto-emailed to founders with signups, activations, compiles, churn, living-credits velocity

## IV.4 CI/CD + infra

- GitHub Actions → Fly.io deploys on merge to `main`
- **Staging env on Fly.io** (required before launch) — mirrors prod config, separate R2 bucket, separate Stripe account
- **Pulumi** manages Fly/R2/Cloudflare/Neon resources in version control
- **Database migrations:** sqlx-cli for Rust, with rehearsed rollback scripts
- **Secrets:** Doppler project per env (dev/staging/prod), no secrets in git

## IV.5 Rate limiting

Upstash Ratelimit SDK on Axum middleware. Caps:
- Public (unauth): 60 rpm
- Authenticated Free: 600 rpm
- Pro: 6,000 rpm
- Team: 10,000 rpm org-pooled
- Compile jobs: per-user 2 concurrent, per-org 10 concurrent

---

# Part V — Legal + Compliance

## V.1 Day-0 legal buy list

| Item | Vendor | Cost | Source |
|---|---|---|---|
| Entity formation (DE C-Corp + EIN + Mercury intro) | Stripe Atlas | $500 one-time, 2-week timeline | stripe.com/atlas [unverified 2026 pricing] |
| ToS + Privacy + Cookie + AUP + DPA | Termly or Iubenda | $10–30/mo | termly.io / iubenda.com [unverified 2026 tiers] |
| Lawyer 48h review | Cooley / Fenwick via intro | $3–5K [unverified] | Cooley GO free templates as starter |
| DMCA designated agent | US Copyright Office | **$6, renew every 3 years** | dmca.copyright.gov [**verified from 37 CFR §201.38**] |
| Business insurance (GL + E&O + Cyber) | Vouch or Embroker | $2–6K/yr [unverified, quote-only] | vouch.us |
| Age attestation | Clickthrough + ToS | Free | Sufficient under COPPA unless deliberately targeting <13 |

## V.2 Regulatory calendar (verified dates)

| Regulation | Requirement | Deadline / Status |
|---|---|---|
| GDPR Art. 33 | 72-hour breach notification to supervisory authority | In force, statutory [verified eur-lex.europa.eu] |
| UK Online Safety Act | Illegal-harms duties for user-to-user services | In force Mar 17, 2025 [verified Ofcom] |
| UK OSA child-safety | Age-assurance duties | In force Jul 25, 2025 [verified Ofcom] |
| EU DSA transparency report | Annual, all intermediary services | Micro/small (<50 staff, ≤€10M) exempt from platform duties [verified EC DSA] |
| EU AI Act GPAI | Obligations active | Aug 2, 2025; enforcement Aug 2, 2026 [verified artificialintelligenceact.eu] |
| US 18 USC §2258A | CSAM reporting to NCMEC CyberTipline | Mandatory, fines up to $600–850K per violation (REPORT Act 2024) [verified law.cornell.edu] |

## V.3 SOC 2 roadmap (defer to month 3)

- **Vanta or Drata** — seed-tier pricing $7.5–15K/yr + $7–20K auditor [unverified, quote-only]
- **Type I:** 4–8 weeks first report
- **Type II:** Year 2
- **Type I as Enterprise gate:** required for Team+ deals

---

# Part VI — Security Model

## VI.1 The `.mcpb` executable problem

`.tr` files dual-identity as `.mcpb` bundles = they contain **executable MCP server code**. This is not the npm scenario — it's worse, because double-click installs the code directly into Claude Desktop / Cursor without review. Shai-Hulud precedent (Sept 2025, ~492 packages, ~132M monthly downloads infected) burned this into industry memory.

## VI.2 Defense-in-depth stack

| Layer | Implementation | Verified? |
|---|---|---|
| **Publisher identity** | Trusted Publishing via GitHub OIDC for public packs (npm/PyPI/crates.io pattern, July 2025) | [verified industry standard] |
| **Signing** | Sigstore cosign mandatory for public packs | [already in TR-1 spec] |
| **Capability declaration** | Manifest field `capabilities: { network, fs, exec }` shown in install modal | New — our build |
| **Malware scan (text)** | **OpenAI Moderation endpoint** | **Free, unlimited** [verified help.openai.com] |
| **Malware scan (binary)** | **ClamAV** (LTS 1.5.0 Oct 2025) — one signal, not sole gate | Free, OSS [verified clamav.net] |
| **Deep binary scan** | Premium VirusTotal or Hybrid Analysis | $20–50K/yr [unverified, quote-only] — **defer to month 2** |
| **CSAM scan** | Hive CSAM Detection API (hash + AI + Thorn text) → report to NCMEC + IWF | Mandatory reporting [verified 18 USC 2258A] |
| **Rekor monitoring** | `rekor-monitor` on our signing key + maintainer keys | [verified Sigstore tooling] |
| **Pre-install user consent** | Claude Desktop modal: "This pack requests: network + fs + exec. Proceed?" | Our build |

## VI.3 Revocation Protocol — world-first (no industry precedent)

**Verified context:** No package registry has revocation. npm/PyPI only delete server-side; already-installed copies remain. Rekor explicitly does not revoke (Fulcio uses short-lived certs). Our solution:

```
Client (root daemon, Claude Desktop mcpb runner, VS Code ext, Web)
    │
    │  GET /api/v1/revoked (hourly, cached)
    ▼
Returns: [BLAKE3 hashes] + signature
    │
    ▼
Before mounting any .tr:
    if content_hash in revoked: refuse + notify user
```

**Why this matters:** headline-worthy. "The first knowledge registry with client-pushed revocation" is a defensible novelty claim parallel to Rooting. Build cost: one endpoint + one client middleware. ~2 engineer-days.

**Spec doc to write:** `docs/2026-04-24-revocation-protocol-spec.md` — separate from this plan.

## VI.4 Content moderation (verified stack)

| Need | Service | Cost | Source |
|---|---|---|---|
| Text moderation (packs, PRs, comments) | **OpenAI Moderation endpoint** | Free, unlimited | help.openai.com [verified] |
| CSAM detection | Hive CSAM API | Custom enterprise pricing [unverified] | thehive.ai |
| Abuse reporting UI | Custom: Report button → queue → human review | ~2 engineer-days | Internal |
| Rate limiting | Upstash Ratelimit SDK | Billed as Redis commands | upstash.com [verified] |

---

# Part VII — Account, Billing, Notifications

## VII.1 Account lifecycle flows (all launch-required)

| Flow | Implementation | Effort |
|---|---|---|
| Sign up (email + GitHub OAuth) | Auth.js in Next.js | 1 day |
| Email verification | Resend signed token (24h TTL) | 0.5 day |
| Password reset | Signed token (15-min TTL) | 0.5 day |
| 2FA TOTP + 10 recovery codes | `totp-rs` crate, codes hashed at issue | 1 day |
| Email change (double opt-in) | Confirm both old and new | 0.5 day |
| Account deletion (GDPR) | 30-day soft-delete → hard-delete + pack cascade | 1 day |
| Pack deletion | 30-day grace → hard-delete + BLAKE3 to revocation list | 0.5 day |
| Payment failure dunning | Stripe Smart Retries + 3 email nudges over 14 days | 0.5 day |
| Refund policy | 14-day money-back on new Pro subs, Stripe customer portal self-serve | 0.5 day |

**Total:** ~6 engineer-days.

## VII.2 Organizations / Teams

| Feature | Launch? |
|---|---|
| Invite by email (pending state) | Yes |
| Roles: Owner / Admin / Member / Read-only | Yes |
| Transfer ownership | Yes |
| Separate org billing | Yes |
| Audit log per org | Yes (Enterprise prep) |
| SSO-lite via Google Workspace OAuth | Team tier |
| Full SAML SSO | Enterprise only — defer |

## VII.3 Email / Notifications

| Channel | Purpose | Cadence |
|---|---|---|
| Transactional (Resend) | Verification, password reset, invoice, PR opened, merge notification | Instant |
| Onboarding sequence (Loops) | D1 "your first pack", D3 "share it", D7 "community" | 3 touches |
| Weekly digest | Living credit activity + trending packs | Weekly, opt-out |
| Push (browser) | "Alex mounted your pack" — variable reward | Opt-in |
| Webhooks | `pack.published`, `pr.opened`, `rooting.recomputed` | HMAC-signed |

**SPF/DKIM/DMARC** must be configured on `thinkingroot.dev` before first email — otherwise 100% of emails go to spam.

---

# Part VIII — Build Plan

## VIII.1 Day 0 (pre-team-engage, you own these)

1. Push 36 unpushed commits + `v0.1.0-rooting` tag to GitHub
2. arXiv submission + Zenodo DOI for DOI-ability
3. DNS: Cloudflare zone for `thinkingroot.dev` (confirm ownership)
4. Stripe Atlas filing ($500) — 2-week wait, start now
5. Termly/Iubenda ToS + Privacy drafts
6. DMCA designated agent registration ($6, 30 min)
7. Vouch insurance application
8. Cooley / Fenwick intro for 48h lawyer review
9. NIST AI Agent Standards Initiative outreach email
10. Create private `thinkingroot-cloud` GitHub repo
11. Reserve 100 conflict-prone namespaces in hub DB seed
12. Sentry, PostHog, Resend, BetterStack, Fly.io, Neon, R2 accounts created
13. AWS Bedrock Authorized Reseller application started (timeline unknown [unverified])
14. Incorporate Bedrock application (may take weeks — start now)

## VIII.2 The 8 parallel build streams (week 1)

| Stream | Owner | Scope | OSS/Private | Acceptance test |
|---|---|---|---|---|
| **A — TR-1 format v1.0** | 2 engineers | `root export/import/verify/mount`, Sigstore signing, MRL+BBQ, `.mcpb` dual-identity, Quick Look + IPreviewHandler, file-assoc handler | OSS | Double-click `.tr` → Claude Desktop mounts in <2s |
| **B — Cloud backend + Hub + Billing** | 3 engineers | Axum + Neon + R2 + Meilisearch + Qdrant + Stripe + JWT/OAuth + OpenAPI spec + webhooks + REST v1 | Private | `root login` → `root push` → pack visible at thinkingroot.dev/{user}/{pack}, verified |
| **C — Web UI** | 2 engineers + 1 designer | Next.js 15 + shadcn/ui + TanStack Query + Auth.js + all 10 pack tabs + upload-and-compile path + billing + Knowledge PR UI + homepage | Private | Anonymous → signup → upload files → publish → PR review → merge, full clickthrough passes |
| **D — Stream-Branch gaps + Live Streams** | 2 engineers | Close 8 known gaps (branch-aware reads, vector copy, missing MCP tools, auto-session, delta cache, engine pool, Python branch, cleanup) + Structural/Shadow/Promotion engines | OSS | Agent writes → hot-tier <5ms, Shadow batch at 200ms, Promotion to branch, merge to main |
| **E — SDKs + Docker + CLI wizard** | 1 engineer + 1 DX | Python (PyPI), Node (npm), Go (pkg.go.dev), multi-arch Docker + Helm, Homebrew/Scoop/apt/yum via cargo-dist, onboarding wizard | OSS | `brew install thinkingroot && root` → wizard → Claude mount in <3 min |
| **F — Agent Forge + Rooting dashboard + Re-rooting worker** | 1 engineer | One-click Claude/Cursor/VS Code deep-links, OAuth apps, per-connection usage meter, continuous re-rooting worker, certificate viewer, pack badges, health history, **Reflect tab**, **Revocation Protocol client+server** | Web + Private | Click "Install to Claude" → mounts + usage visible at /agents within 60s; revocation deny-list live |
| **G — VS Code extension** | 1 engineer | Compile/ask/publish from sidebar + inline + status bar | OSS | Install from marketplace, compile, see Rooting badges, all without touching CLI |
| **H — Desktop app (week 2)** | 1 engineer | Tauri wrap of daemon + web UI + tray + file-assoc + privacy dashboard | OSS | Installer runs on macOS/Windows/Linux, tray visible, drag-drop `.tr` mounts |

**Cross-cutting (touches all streams):** account lifecycle (A4), moderation stack, rate limits, billing dunning, email setup, observability wiring.

## VIII.3 Launch Thursday bundle (end of week 1)

Ships simultaneously:
- arXiv paper + Zenodo DOI + `v0.1.0-rooting` tag pushed (from Day 0)
- `.tr` v1.0 in OSS (Stream A)
- thinkingroot.dev live with upload path + 10 seeded packs (Stream C)
- Hub backend with publish/fork/PR (Stream B)
- Python / Node / Go SDKs published (Stream E)
- Docker multi-arch image (Stream E)
- Homebrew / Scoop / apt / yum CLI packages (Stream E)
- **VS Code extension on Marketplace** (Stream G)
- Agent Forge buttons live (Stream F)
- Stripe checkout live with Free / $19 Pro / $49 Ultra Pro / $99 Team
- Living Credits formula live (first events firing)
- Revocation Protocol endpoint live
- Show HN post at 9am ET Thursday
- Mem0 Issue #4573 outreach
- Zep OSS migration wave outreach

## VIII.4 Week 2 follow-up

- Desktop app (Stream H)
- Browser extension (capture-to-pack)
- Org management UI polish
- Search tuning pass
- Dev infra hardening (feature flags, IaC, staging drill)

## VIII.5 Month 2–6

- SOC 2 Type I kickoff (Vanta + auditor)
- SSO via Google Workspace OAuth for Team tier
- Multi-region data residency (EU user class)
- Federated cross-org queries
- Partnerships (Replit Student, Vercel, Obsidian)
- HIPAA if Enterprise deal demands
- Premium binary scanning contract

## VIII.6 Explicitly deferred (not gaps — decisions)

- Self-hosted Enterprise tier
- Full SSO / SAML
- Air-gapped deployments
- Edge cache / regional POPs
- Mobile apps (iOS/Android)
- Notion/Confluence/Jira/Linear connectors
- LF AI governance donation (stay commercial-first)
- Full Discussions board (pack-page comments only at launch)

---

# Part IX — Go-to-Market

## IX.1 Launch calendar (Thursday-of-launch hour-by-hour)

- **06:00 ET** — arXiv paper goes public, Zenodo DOI live
- **07:00 ET** — thinkingroot.dev DNS cutover to production
- **08:00 ET** — Stripe checkout live, 10 seeded packs published
- **09:00 ET** — Show HN submission
- **09:15 ET** — Reddit r/LocalLLaMA, r/MachineLearning posts
- **10:00 ET** — Twitter/X announcement thread + demo video (90s Loom)
- **11:00 ET** — Direct outreach to Mem0 Issue #4573 commenters
- **12:00 ET** — Email to pre-launch waitlist + influencers
- **All day** — Team monitors signals, HN comments, support inbox
- **Friday** — TechCrunch embargoed piece goes live

## IX.2 Outreach targets (verified names)

| Outlet | Contact | Beat |
|---|---|---|
| TechCrunch | Kyle Wiggers | AI |
| The Verge | Alex Heath | AI/platforms |
| The Information | AI newsletter team | — |
| Simon Willison (simonwillison.net) | personal blog | LLM tooling (hugely connected) |
| Latent Space podcast | Alessio Fanelli + swyx | AI infra |

## IX.3 Comparison landing pages (SEO)

- `/vs/mem0` — angle: 97.8% junk audit, we pay users back
- `/vs/zep` — angle: we didn't kill OSS
- `/vs/letta` — angle: knowledge, not just agent state
- `/vs/huggingface` — angle: certificates on every claim

## IX.4 Migration tutorial

- "Migrate from Mem0 in 5 minutes" — captures Issue #4573 crowd
- Video demo + copy-paste CLI snippet + SDK example

## IX.5 Community

- Discord server (lower friction than Slack for OSS)
- Weekly office hours (live, recorded to YouTube)
- `thinkingroot.dev/community` page

---

# Part X — Risk Register (consolidated)

Sources: this doc + `docs/2026-04-24-living-credits-design.md` §8 + `docs/2026-04-24-tr-format-design-and-research.md` §6.

| # | Risk | Severity | Mitigation |
|---|---|---|---|
| 1 | Anthropic cuts direct access (Windsurf precedent June 2025) | High | AWS Bedrock Authorized Reseller path + BYOK fallback on all tiers |
| 2 | Malicious `.mcpb` uploaded, propagates via install | High | Trusted Publishing OIDC + Sigstore + ClamAV + OpenAI Mod + capability declaration + Revocation Protocol |
| 3 | Viral pack bankrupts LLM pool | Med | 100K/mo regen cap → Patron status; Patron count capped at 100 |
| 4 | Living Credits perceived as gambling / securities | Med | Non-transferable, one-way cash-out, ToS clause, Discord Nitro/Steam precedent |
| 5 | GAAP liability from issued credits | Med | Credits are non-redeemable-for-cash compute (not deferred revenue) |
| 6 | Sock-puppet regen farming | Med | GitHub OAuth weighting + IP distinct-pairs + per-account caps (Spotify pattern) |
| 7 | Zero packs on launch = dead hub | High | Seed 10 curated packs (your own projects + LongMemEval + popular OSS repos) on Day 0 |
| 8 | Execution slippage across 8 parallel streams | High | Lock format contracts on Day 0; daily standup across leads; Thursday cutoff is hard |
| 9 | GDPR 72h breach clock fires unprepared | High | Runbook written + rehearsed Day 0; Iubenda breach template in `/legal/runbooks/` |
| 10 | `.tr` file format needs migration post-launch | Med | Forward-compat in schema_version (v1.0.0); unknown-field tolerant reader |
| 11 | BBQ 1-bit degrades AllMiniLM quality | Med | Empirical test in Stream A; fallback to int8 residual or swap to mxbai-embed-large-v1 |
| 12 | Mem0 competitive response (they have $24M) | Med | Launch speed + Rooting moat (they cannot replicate without rearchitecting) |
| 13 | CSAM appears on platform | High | Mandatory NCMEC CyberTipline reporting + Hive CSAM API + human reviewer |
| 14 | UK Online Safety Act duty breach | Med | Risk assessment + safety measures + reporting mechanism all in place before accepting UK users |
| 15 | EU AI Act GPAI implications for knowledge platforms | Low | Monitor August 2026 enforcement; transparency report for anything above thresholds |
| 16 | Orphan regen on creator deletion | Low | Freeze by default; creator can set succession in pack settings |
| 17 | Premium binary scanner unaffordable pre-revenue | Low | ClamAV + manual human review until month-2 revenue supports $20–50K/yr contract |

---

# Part XI — Success Metrics

## XI.1 Launch week targets

| Metric | Target |
|---|---|
| HN front-page rank | Top 5 on launch day |
| Sign-ups | 5,000 |
| First pack published | 1,500 (30% of signups) |
| VS Code extension installs | 10,000 |
| Paid conversions | 1% of signups (50 Pro subs) = $950 MRR |
| p95 API latency | <200ms |
| p95 compile latency (10K-claim) | <5 min |
| Uptime | >99.5% (launch-week allowance) |

## XI.2 Month-3 targets

| Metric | Target |
|---|---|
| Total signups | 50,000 |
| % free users who published ≥1 pack | 15% |
| Median living credits per active user | 500 |
| Free → Pro conversion | 4% |
| Pro churn | <5%/mo |
| Patron count | 20 |
| MRR | $20K+ |

## XI.3 Kill-switch conditions

- Regen abuse exceeds 10% of awarded credits → pause Living Credits formula
- LLM spend exceeds 120% of budget → throttle managed tier, force overflow to BYOK
- Any moderation SLA breach >24h → emergency human review team onboard

---

# Part XII — Open Decisions for Founder Sign-off

These are the only items blocking team engagement:

1. **BYOK default LLM** — Anthropic direct (cheaper, platform-risk) or AWS Bedrock (reseller-cover). **Recommended:** Bedrock.
2. **Pro tier price** — $19/user/mo. Confirm or adjust.
3. **Ultra Pro price** — $49/user/mo with cash-out. Confirm or adjust.
4. **BBQ-on-AllMiniLM retention test** — go/no-go on empirical test in Stream A. Fallback: swap to mxbai-embed-large-v1.
5. **Student tier** — free Pro for verified `.edu` email, or wait for GitHub Student Pack approval (~30 days).
6. **Desktop app week 1 vs week 2** — ship Thursday or follow-up.
7. **Living Credits cash-out threshold** — 100K living (recommended) or 50K (more payouts).
8. **Patron global cap** — top-100 worldwide (recommended) or regional.
9. **Knowledge-card template** — provided scaffolding at pack create, or blank.
10. **Default pack visibility** — Public (discoverability) or Private (trust). **Recommended:** Public with explicit Private toggle prominent.
11. **File-tree sort default** — last-modified (Git pattern) or Rooting % (surface quality). **Recommended:** last-modified.
12. **Version model** — SemVer + BLAKE3 both shown (recommended).
13. **Launch day** — Thursday (verified HN performs better mid-week, single data point) or different.
14. **Team composition** — names against the 8 streams.
15. **Corporate state** — entity formed? domain owned? banking? insurance? Required Day-0 answers.

---

# Part XIII — Deliverables Summary

This plan corresponds to the following additional spec docs — **all 8 now complete, 2026-04-24**:

- ✅ `docs/2026-04-24-tr-format-design-and-research.md` — TR-1 format spec
- ✅ `docs/2026-04-24-living-credits-design.md` — Living Credits full spec
- ✅ `docs/2026-04-24-revocation-protocol-spec.md` — client-pushed revocation protocol v1.0
- ✅ `docs/2026-04-24-hub-ui-spec.md` — pack-page, discovery, design system, a11y, i18n
- ✅ `docs/2026-04-24-cli-wizard-ux.md` — first-run onboarding script, editor detection, non-interactive CI mode
- ✅ `docs/2026-04-24-knowledge-pr-model.md` — fact-level diff algorithm, conflict resolution, merge strategy, SemVer bumping
- ✅ `docs/2026-04-24-security-model.md` — 7-layer defense-in-depth, threat model, bug bounty, compliance crosswalk
- ✅ `docs/2026-04-24-breach-response-runbook.md` — GDPR Art. 33 72-hour clock, notification templates, contact directory, rehearsal schedule

---

# Part XIV — Change Log

- **2026-04-24** — Initial plan (this document). Pending founder sign-off on §XII.

---

# Part XV — References

## OSS / Standards (verified)
- TR-1 format: `docs/2026-04-24-tr-format-design-and-research.md`
- Living Credits: `docs/2026-04-24-living-credits-design.md`
- Rooting OSS release: v0.1.0-rooting tag
- Sigstore: docs.sigstore.dev
- SLSA: slsa.dev
- CAR v1: ipld.io/specs/transport/car/carv1
- HDT: rdfhdt.org
- zstd seekable: github.com/facebook/zstd contrib/seekable_format

## Regulations (verified)
- GDPR Art. 33: eur-lex.europa.eu
- UK Online Safety Act: ofcom.org.uk/online-safety
- EU DSA: digital-strategy.ec.europa.eu
- EU AI Act: artificialintelligenceact.eu/implementation-timeline
- 18 USC §2258A: law.cornell.edu/uscode/text/18/2258A
- REPORT Act 2024: wsgrdataadvisor.com summary
- DMCA 37 CFR §201.38: dmca.copyright.gov

## Competitive (verified)
- Mem0 Series A: techcrunch.com (Oct 28 2025)
- Mem0 Issue #4573: github.com/mem0ai/mem0/issues/4573
- Zep OSS deprecation: getzep.com/pricing blog
- Cursor margin: techcrunch.com (Aug 2025), Foundamental, PitchBook
- Windsurf Anthropic cutoff: techcrunch.com (Jun 3 2025)
- Shai-Hulud worm: cisa.gov, krebsonsecurity.com, sysdig.com
- Trusted Publishing: repos.openssf.org

## Vendor pricing (verified live)
- Fly.io: fly.io/docs/about/pricing
- Neon: neon.com/pricing
- Upstash: upstash.com/pricing/redis
- R2: developers.cloudflare.com/r2/pricing
- Resend: resend.com/pricing
- Sentry: sentry.io/pricing
- BetterStack: betterstack.com/pricing
- Axiom: axiom.co/pricing
- PostHog: posthog.com/pricing
- GrowthBook: growthbook.io/pricing
- Doppler: doppler.com/pricing
- Pulumi: pulumi.com/pricing
- PagerDuty: pagerduty.com/pricing
- Meilisearch: meilisearch.com/pricing
- Qdrant: qdrant.tech/pricing
- Plain: plain.com/pricing
- Loops: loops.so/pricing
- VirusTotal commercial restriction: docs.virustotal.com/reference/public-vs-premium-api
- OpenAI Moderation free: help.openai.com/en/articles/4936833

## Vendor pricing (unverified — confirm at checkout)
- Stripe Atlas $500
- Termly / Iubenda 2026 tiers
- Law firm hourly rates
- Vanta / Drata / Secureframe
- Vouch / Embroker / Coalition / Founder Shield
- Cloudflare Pro $25/mo
- Hive Moderation enterprise
- Persona / Jumio / Veratad
- 1Password Teams
- Render / Supabase PITR

---

**End of plan.** Ready for founder sign-off on §XII. Upon confirmation, stream-by-stream PR plans will be generated for each of Streams A–H.
