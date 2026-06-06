# ThinkingRoot Knowledge Hub — Scalable Infrastructure Architecture

**Date:** 2026-04-13  
**Status:** Infrastructure Design  
**Scale Target:** Multi-million knowledge packages, sub-50ms retrieval globally  
**Principle:** Every decision proven at production scale by an existing system

---

## The Latency Budget

An agent queries "What is positional encoding?" against a connected hub graph. Here's the latency budget for the entire round-trip:

```
Agent → MCP request                    ~1ms (local IPC)
MCP → QueryEngine                      ~1ms (local function call)
QueryEngine → local graph.db           ~5ms (CozoDB Datalog query)
QueryEngine → cached hub graph         ~5ms (same — it's local)
QueryEngine → cloud federated query    ~30-80ms (network + edge serve)
Vector similarity search               ~3-10ms (fastembed HNSW)
Merge + rank results                   ~2ms
Return to agent                        ~1ms
─────────────────────────────────────────
TOTAL (local + cached hub):            ~15ms
TOTAL (cloud federated):               ~50-100ms
```

**The key insight: hub graphs are cached locally.** Once you `root hub connect django/official`, the full graph is on your machine. Query latency is identical to a local workspace — single-digit milliseconds. Cloud federation only happens for cross-org queries, and even then the target is sub-100ms via edge serving.

---

## Three Tiers of Scale

The infrastructure is designed in three tiers. You start at Tier 1 and grow into Tier 3 as demand increases. Each tier is independently functional — you don't need Tier 3 infrastructure to launch.

### Tier 1: Launch (0 → 10K packages, 0 → 100K users)

This handles launch through the first 18 months. Budget: ~$500-2,000/month.

```
┌─────────────────────────────────────────────────────┐
│                    CLIENTS                          │
│  root hub connect · root publish · root hub search  │
│                    ↕ HTTPS                          │
├─────────────────────────────────────────────────────┤
│                 CDN (Cloudflare)                    │
│  Sparse index cached at edge (300+ PoPs)            │
│  Package blobs served from R2 (zero egress fees)    │
│  ETag + If-None-Match for conditional fetches       │
│                    ↕                                │
├──────────────┬──────────────────────────────────────┤
│  API Server  │  Single Axum instance (same as       │
│  (Rust)      │  thinkingroot-serve, extended)       │
│              │  Handles: publish, search, auth      │
│              │  Runs on: Fly.io or Railway           │
├──────────────┼──────────────────────────────────────┤
│  Metadata DB │  PostgreSQL (Neon or Supabase)       │
│              │  Tables: packages, versions, users,   │
│              │  stars, forks, downloads, search index │
├──────────────┼──────────────────────────────────────┤
│  Blob Store  │  Cloudflare R2 (S3-compatible)       │
│              │  Stores: graph.db, vectors.bin,       │
│              │  artifacts/, manifests                │
│              │  Why R2: zero egress fees (critical    │
│              │  for a download-heavy registry)       │
├──────────────┼──────────────────────────────────────┤
│  Search      │  PostgreSQL full-text search (pg_trgm │
│              │  + tsvector) — sufficient for 10K     │
│              │  packages                             │
├──────────────┼──────────────────────────────────────┤
│  Auth        │  JWT (self-issued) or Clerk           │
│              │  GitHub OAuth for publisher verify     │
└──────────────┴──────────────────────────────────────┘
```

**Why this works at Tier 1:**
- npm started with a single CouchDB instance. crates.io started with a single PostgreSQL + S3.
- 10K packages × 50 MB avg = 500 GB total storage. R2 cost: ~$7.50/month.
- Sparse index (all 10K manifests as JSON lines) = ~20 MB total. Fits entirely in CDN edge cache.
- A single Axum server can handle thousands of requests/second (Rust, async, zero GC pauses).

**Cost breakdown at 10K packages:**

| Component | Service | Monthly Cost |
|---|---|---|
| Compute | Fly.io (2 vCPU, 4GB RAM) | ~$30 |
| Database | Neon PostgreSQL (free tier → $19 pro) | ~$19 |
| Blob storage | Cloudflare R2 (500 GB) | ~$8 |
| CDN | Cloudflare (free tier covers most) | $0-20 |
| Auth | Clerk (free → $25) | ~$25 |
| Domain | thinkingroot.dev | ~$12/yr |
| **Total** | | **~$100-120/month** |

You can launch a globally distributed knowledge registry for $100/month. This is not theoretical — crates.io operated at similar scale on similar infrastructure for its first years.

---

### Tier 2: Growth (10K → 500K packages, 100K → 5M users)

This handles years 2-3. Budget: ~$5,000-20,000/month.

```
┌─────────────────────────────────────────────────────┐
│                    CLIENTS                          │
│                    ↕ HTTPS                          │
├─────────────────────────────────────────────────────┤
│              CDN (Cloudflare Pro/Biz)               │
│  Sparse index: per-package files cached globally    │
│  Package blobs: R2 with CDN in front               │
│  Request collapsing (npm pattern): prevents         │
│  thundering herd on popular package updates         │
│                    ↕                                │
├──────────────┬──────────────────────────────────────┤
│  API Cluster │  3-5 Axum instances behind LB        │
│  (Rust)      │  Horizontal scaling                  │
│              │  Health-checked, auto-restart         │
│              │  Deployed: Fly.io Machines or K8s     │
├──────────────┼──────────────────────────────────────┤
│  Metadata DB │  PostgreSQL (managed, 2 read replicas)│
│              │  Primary in us-east-1                 │
│              │  Read replicas in eu-west-1, ap-se-1  │
│              │  Query routing: writes → primary,     │
│              │  reads → nearest replica              │
├──────────────┼──────────────────────────────────────┤
│  Blob Store  │  Cloudflare R2 (multi-region)        │
│              │  Estimated: 5-25 TB                   │
│              │  R2 auto-replicates globally          │
├──────────────┼──────────────────────────────────────┤
│  Search      │  Meilisearch or Typesense cluster    │
│  Engine      │  (Rust-based, sub-50ms search)       │
│              │  Indexes: package name, description,  │
│              │  tags, entity names, domain            │
├──────────────┼──────────────────────────────────────┤
│  Vector      │  Qdrant (self-hosted, Rust, HNSW)    │
│  Search      │  For semantic hub search:             │
│              │  "find packages about attention"       │
│              │  Embed package descriptions + entity   │
│              │  names → vector index                  │
├──────────────┼──────────────────────────────────────┤
│  Cache       │  Redis (Upstash or Fly Redis)        │
│              │  Hot package manifests: ~100K entries  │
│              │  Query result cache: 60s TTL           │
│              │  Rate limiting: per-user publish quota │
├──────────────┼──────────────────────────────────────┤
│  Job Queue   │  Redis Streams or BullMQ             │
│              │  Publish validation (async)            │
│              │  Delta computation (async)             │
│              │  Health re-verification (scheduled)    │
├──────────────┼──────────────────────────────────────┤
│  Auth        │  Self-hosted JWT + GitHub OAuth       │
│              │  Org/namespace management              │
│              │  API token management for CI/CD        │
└──────────────┴──────────────────────────────────────┘
```

**Key architecture decisions at Tier 2:**

**1. CDN request collapsing (borrowed from npm):**
When a popular package updates (e.g., `react/official` pushes v19), thousands of connected agents will fetch the update simultaneously. Without request collapsing, the origin gets hammered. With it, Cloudflare coalesces concurrent requests for the same resource into a single origin fetch.

**2. Read replicas for metadata (not blobs):**
At 500K packages, PostgreSQL search becomes the bottleneck, not blob storage. Read replicas in 3 regions handle global search queries. Writes (publish) always go to primary — publish is infrequent (< 1% of traffic). Reads (search, manifest fetch) are 99% of traffic.

**3. Qdrant for semantic search:**
Beyond keyword search ("django"), users want semantic search ("how to handle authentication in web frameworks"). Qdrant (written in Rust, like ThinkingRoot) provides sub-10ms vector search over embedded package descriptions. This is the same technology ThinkingRoot already uses locally (fastembed HNSW).

**4. Delta computation as async jobs:**
When `naveen/transformer-survey` publishes v1.3.0, the hub computes a delta from v1.2.0 in the background (async worker). Connected agents fetch the small delta instead of the full graph. This is the single biggest bandwidth optimization.

**Cost at 500K packages:**

| Component | Monthly Cost |
|---|---|
| Compute (5 Axum instances) | ~$200 |
| PostgreSQL (managed + 2 replicas) | ~$300-500 |
| R2 (10 TB) | ~$150 |
| CDN (Cloudflare Business) | ~$200 |
| Qdrant (self-hosted, 3 nodes) | ~$300 |
| Redis (Upstash) | ~$50-100 |
| Search (Meilisearch cloud) | ~$100 |
| **Total** | **~$1,500-2,000/month** |

---

### Tier 3: Scale (500K → 10M+ packages, 5M → 100M+ users)

This is the Docker Hub / npm / Hugging Face scale. Budget: $50,000-200,000/month.

```
┌──────────────────────────────────────────────────────────────┐
│                       GLOBAL EDGE                            │
│                                                              │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐    │
│  │ us-east  │  │ eu-west  │  │ ap-south │  │ ap-north │    │
│  │          │  │          │  │          │  │          │    │
│  │ CF Worker│  │ CF Worker│  │ CF Worker│  │ CF Worker│    │
│  │ + R2     │  │ + R2     │  │ + R2     │  │ + R2     │    │
│  │ + Qdrant │  │ + Qdrant │  │ + Qdrant │  │ + Qdrant │    │
│  │ replica  │  │ replica  │  │ replica  │  │ replica  │    │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘    │
│       │              │              │              │          │
│       └──────────────┴──────┬───────┴──────────────┘          │
│                             │                                 │
├─────────────────────────────┤                                 │
│        ORIGIN CLUSTER       │                                 │
│                             │                                 │
│  ┌────────────────────┐     │                                 │
│  │ API Gateway (Axum) │     │   Handles:                      │
│  │ 10-20 instances    │     │   - Publish (write path)        │
│  │ Auto-scaling       │     │   - Auth (JWT validation)       │
│  └────────┬───────────┘     │   - Manifest generation         │
│           │                 │   - Delta computation            │
│  ┌────────┴───────────┐     │                                 │
│  │ PostgreSQL Cluster │     │                                 │
│  │ CockroachDB or     │     │   Multi-region, geo-partitioned │
│  │ Citus (sharded PG) │     │   Package metadata sharded by   │
│  └────────┬───────────┘     │   namespace first letter         │
│           │                 │                                 │
│  ┌────────┴───────────┐     │                                 │
│  │ Blob Storage       │     │                                 │
│  │ R2 (primary)       │     │   Multi-region replication       │
│  │ + S3 (backup)      │     │   Content-addressed dedup        │
│  └────────────────────┘     │                                 │
│                             │                                 │
│  ┌────────────────────┐     │                                 │
│  │ Event Stream       │     │                                 │
│  │ Kafka / Redpanda   │     │   Publish events → replication   │
│  │                    │     │   → CDN purge → delta compute    │
│  └────────────────────┘     │                                 │
└─────────────────────────────┘                                 │
└──────────────────────────────────────────────────────────────┘
```

**The critical Tier 3 pattern: Edge-served Knowledge**

At this scale, the biggest optimization is **serving knowledge queries at the edge** — not just blob downloads, but actual Datalog queries resolved at the nearest point of presence.

```
How it works:

1. Popular package "react/official" is connected by 2M agents
2. Hub pre-computes query results for top-100 common queries
   (e.g., "What is useState?", "How does reconciliation work?")
3. Results cached at Cloudflare edge (Workers KV or R2)
4. Agent MCP query hits CF Worker → edge cache hit → <10ms response
5. Cache miss → forward to nearest regional Qdrant replica → <30ms
6. Cold query → forward to origin → <100ms
```

**Query latency at Tier 3 scale:**

| Query Type | Path | Latency |
|---|---|---|
| Local cached hub graph | CozoDB on disk | **5-10ms** |
| Edge-cached popular query | CF Workers KV | **<10ms** |
| Semantic search (regional) | Qdrant replica | **15-30ms** |
| Full federated query (cross-org) | Origin cluster | **50-100ms** |
| Cold publish + index | Origin + async workers | **2-5s** |

---

## The Download Path (Optimized for Millions)

When an agent runs `root hub connect react/official`, here is the exact flow:

```
Step 1: Manifest fetch
  GET https://hub.thinkingroot.dev/index/re/ac/react/official
  → Cloudflare edge cache HIT (manifest is ~2KB, cached for 5 min)
  → Response: manifest.json with content hashes
  → Latency: 5-20ms from anywhere in the world

Step 2: Version resolution
  Client compares local cache vs manifest version
  → If up-to-date: DONE (zero download)
  → If outdated: fetch delta

Step 3: Delta or full download
  If delta available:
    GET https://hub.thinkingroot.dev/blobs/{delta-hash}.delta.zst
    → Cloudflare edge → R2 (zero egress fee)
    → Delta size: typically 1-5% of full graph
    → Client applies delta to local graph.db
  
  If first download:
    GET https://hub.thinkingroot.dev/blobs/{graph-hash}.graph.zst
    → Cloudflare edge → R2
    → Full graph download (zstd compressed, ~30% of raw size)
    → Client unpacks to ~/.thinkingroot/hub/react/official/

Step 4: Vector index
  Option A: Download vectors.bin (fast, ~10-50 MB)
  Option B: Regenerate locally from graph.db using fastembed (~30s)
  → Client chooses based on bandwidth vs compute preference

Step 5: Register in WorkspaceRegistry
  → Hub graph now queryable via MCP/REST alongside local workspaces
```

**Bandwidth optimization math:**

| Package size | Full download | zstd compressed | Delta (typical update) |
|---|---|---|---|
| Small (1K claims) | 5 MB | 1.5 MB | 50-100 KB |
| Medium (50K claims) | 100 MB | 30 MB | 1-3 MB |
| Large (500K claims) | 1 GB | 300 MB | 10-30 MB |

At 1M connected agents updating a popular package, delta sync saves **97-99% of bandwidth** compared to full re-download.

---

## The Publish Path (Optimized for Safety)

```
Step 1: Client-side validation
  root publish →
  ✓ graph.db exists and is valid CozoDB
  ✓ Health score ≥ 0.5
  ✓ No Restricted/Confidential claims in public publish
  ✓ Provenance chain complete
  ✓ BLAKE3 hashes computed

Step 2: Upload
  PUT https://hub.thinkingroot.dev/api/v1/packages/{name}/{version}
  Authorization: Bearer {token}
  Content-Type: application/zstd
  Body: KnowledgePack (tar.zst)
  → Goes directly to origin (not edge-cached — writes are rare)

Step 3: Server-side validation (async, <30s)
  Worker picks up job from queue:
  ✓ Verify BLAKE3 hashes match uploaded content
  ✓ Scan claims for policy violations
  ✓ Verify namespace ownership
  ✓ Check for duplicate content hash (dedup)
  ✓ Re-compute health score independently

Step 4: Index update
  ✓ Extract manifest → insert into PostgreSQL
  ✓ Update sparse index file for this package
  ✓ Purge CDN cache for this package's index entry (instant via Cloudflare API)
  ✓ Compute delta from previous version (async)
  ✓ Index package description + entities in Qdrant

Step 5: Notify
  ✓ Connected agents will see update on next `root hub update` or poll
  ✓ Webhook notification to subscribed users (optional)
```

**Publish rate estimation:**
- Year 1: ~100 publishes/day (10K packages, each updated monthly avg)
- Year 3: ~5,000 publishes/day (500K packages)
- At scale: ~50,000 publishes/day (manageable for a single Axum cluster — Axum handles 100K+ req/s on a single core)

---

## The Search Architecture (Sub-50ms Global)

Search is the make-or-break UX. If a developer types "machine learning frameworks" and waits 2 seconds, they leave.

### Three search layers, merged:

```
Layer 1: Keyword search (PostgreSQL pg_trgm + tsvector)
  → Matches: package name, description, tags
  → Latency: 5-15ms (indexed, read replica)
  → Example: "django" matches django/official

Layer 2: Semantic search (Qdrant HNSW)
  → Matches: conceptual similarity
  → Latency: 5-15ms (HNSW, in-memory)
  → Example: "web framework authentication" matches django/official,
    flask/official, express/official

Layer 3: Entity search (PostgreSQL JSON index)
  → Matches: entities inside packages
  → Latency: 10-20ms
  → Example: "OAuth2" finds packages containing OAuth2 entity

Merge: Reciprocal Rank Fusion (RRF)
  → Combines all three result sets
  → Deduplicates
  → Applies: health score boost (higher health = higher rank)
  → Applies: freshness boost (recently updated = higher rank)
  → Applies: popularity boost (more connections = higher rank)
  → Total latency: 20-40ms
```

---

## Why Cloudflare R2 (Not S3, Not GCS)

This is the single most important infrastructure decision for a download-heavy registry:

| Factor | S3 | GCS | R2 |
|---|---|---|---|
| Storage cost (per TB/mo) | $23 | $20 | $15 |
| **Egress cost** | **$90/TB** | **$120/TB** | **$0** |
| CDN integration | CloudFront (separate) | Cloud CDN (separate) | Built-in (Cloudflare) |
| S3 API compatible | Native | Via interop | Yes |

**At registry scale, egress is the dominant cost.** npm serves trillions of downloads. If ThinkingRoot Hub serves 10M package downloads/month at 50 MB avg, that's 500 TB of egress:
- S3: 500 TB × $90 = **$45,000/month** 💀
- R2: 500 TB × $0 = **$0/month** ✅

This is why Cloudflare R2 is non-negotiable for a public registry. Hugging Face recognized this same dynamic and actively uses CDN-optimized storage for model distribution.

---

## Edge-Served Knowledge Queries (The Novel Part)

Beyond blob downloads, at Tier 3 scale we can serve **actual knowledge queries at the edge** — no round-trip to origin.

```
Cloudflare Worker (at each edge PoP):

1. Agent sends MCP query via HTTPS
2. Worker checks edge cache (Workers KV):
   - Key: blake3(package_name + query_normalized)
   - Value: serialized search results
   - TTL: 60 seconds for popular packages
3. Cache HIT → return immediately (<5ms)
4. Cache MISS → forward to nearest regional Qdrant
   → compute results
   → store in edge cache
   → return (~30ms first time, <5ms subsequent)
```

**What gets edge-cached:**
- Top-100 queries per popular package (auto-detected from query logs)
- Package manifest + stats (updated on publish)
- Knowledge card content (static, long TTL)
- Entity list for a package (semi-static)

**What never gets edge-cached:**
- Full graph queries (too large, too varied)
- Write operations (publish, fork)
- Auth flows

---

## Data Model for Hub Backend (PostgreSQL)

```sql
-- Packages (one row per package name)
CREATE TABLE packages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT UNIQUE NOT NULL,  -- "naveen/transformer-survey"
    namespace   TEXT NOT NULL,         -- "naveen"
    short_name  TEXT NOT NULL,         -- "transformer-survey"
    description TEXT,
    license     TEXT,
    domain      TEXT,                  -- "research/machine-learning"
    tags        TEXT[],
    visibility  TEXT DEFAULT 'public', -- public, private, org
    owner_id    UUID REFERENCES users(id),
    stars       INT DEFAULT 0,
    forks       INT DEFAULT 0,
    downloads   BIGINT DEFAULT 0,
    created_at  TIMESTAMPTZ DEFAULT now(),
    updated_at  TIMESTAMPTZ DEFAULT now()
);

-- Versions (one row per version of a package)
CREATE TABLE versions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    package_id      UUID REFERENCES packages(id),
    version         TEXT NOT NULL,      -- "1.2.0"
    manifest_json   JSONB NOT NULL,     -- full manifest
    graph_hash      TEXT NOT NULL,      -- BLAKE3
    graph_size      BIGINT,             -- bytes
    vectors_hash    TEXT,
    vectors_size    BIGINT,
    claims_count    INT,
    entities_count  INT,
    relations_count INT,
    health_overall  REAL,
    health_freshness REAL,
    health_consistency REAL,
    health_coverage REAL,
    health_provenance REAL,
    compiler_version TEXT,
    published_at    TIMESTAMPTZ DEFAULT now(),
    
    UNIQUE (package_id, version)
);

-- Deltas (pre-computed diffs between consecutive versions)
CREATE TABLE deltas (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    package_id  UUID REFERENCES packages(id),
    from_version TEXT NOT NULL,
    to_version   TEXT NOT NULL,
    delta_hash   TEXT NOT NULL,       -- BLAKE3
    delta_size   BIGINT,
    claims_added INT,
    claims_removed INT,
    entities_added INT,
    entities_removed INT,
    computed_at  TIMESTAMPTZ DEFAULT now()
);

-- Users
CREATE TABLE users (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    username    TEXT UNIQUE NOT NULL,
    email       TEXT UNIQUE,
    github_id   TEXT,
    verified    BOOLEAN DEFAULT false,
    created_at  TIMESTAMPTZ DEFAULT now()
);

-- Connections (who is connected to what)
CREATE TABLE connections (
    user_id     UUID REFERENCES users(id),
    package_id  UUID REFERENCES packages(id),
    version     TEXT,
    connected_at TIMESTAMPTZ DEFAULT now(),
    PRIMARY KEY (user_id, package_id)
);

-- Full-text search index
CREATE INDEX idx_packages_search ON packages 
    USING GIN (to_tsvector('english', name || ' ' || COALESCE(description, '')));
CREATE INDEX idx_packages_tags ON packages USING GIN (tags);
CREATE INDEX idx_packages_domain ON packages (domain);
```

---

## Scaling Timeline

| Phase | When | Packages | Users | Infra Tier | Monthly Cost |
|---|---|---|---|---|---|
| Launch | Month 1-6 | 100 → 1K | 1K → 10K | Tier 1 | $100-200 |
| Seed | Month 6-12 | 1K → 10K | 10K → 100K | Tier 1 | $200-500 |
| Growth | Month 12-24 | 10K → 100K | 100K → 1M | Tier 2 | $2K-5K |
| Scale | Month 24-36 | 100K → 1M | 1M → 10M | Tier 2→3 | $10K-50K |
| Platform | Month 36+ | 1M+ | 10M+ | Tier 3 | $50K-200K |

**Revenue vs Cost at each stage:**

| Stage | Revenue (est.) | Infra Cost | Margin |
|---|---|---|---|
| Launch | $0 (free) | $200/mo | -$200/mo |
| Seed | $5K/mo (early Pro users) | $500/mo | +$4,500/mo |
| Growth | $50K/mo | $5K/mo | +$45K/mo |
| Scale | $200K/mo | $50K/mo | +$150K/mo |
| Platform | $500K+/mo | $200K/mo | +$300K/mo |

The business becomes margin-positive at the Seed stage because the product charges for private packages and cloud features while the infrastructure scales sub-linearly thanks to CDN caching and content-addressed dedup.

---

## Technology Stack Summary

| Layer | Technology | Why This Specific Choice |
|---|---|---|
| **API Server** | Axum (Rust) | Already used in thinkingroot-serve. Zero GC, async, 100K+ req/s per core |
| **Metadata DB** | PostgreSQL | Battle-tested, full-text search built-in, JSONB for manifests |
| **Blob Storage** | Cloudflare R2 | Zero egress — this alone saves $45K+/month at scale |
| **CDN** | Cloudflare | 300+ PoPs, request collapsing, instant purge, Workers for edge compute |
| **Edge Compute** | Cloudflare Workers | Sub-ms cold start, edge-cached knowledge queries |
| **Vector Search** | Qdrant (Rust) | Sub-10ms HNSW search, Rust-native, self-hostable |
| **Text Search** | Meilisearch (Rust) | Sub-50ms typo-tolerant search, Rust-native |
| **Cache** | Redis (Upstash) | Hot manifests, rate limiting, session cache |
| **Job Queue** | Redis Streams → Kafka (at scale) | Async publish validation, delta computation |
| **Content Hashing** | BLAKE3 | Already used throughout ThinkingRoot pipeline |
| **Compression** | zstd | Same as cargo, 30% better than gzip, streaming support |
| **Auth** | JWT + GitHub OAuth | Standard, stateless, horizontally scalable |

**Note:** 5 of 12 components are written in Rust (Axum, Qdrant, Meilisearch, BLAKE3, zstd). This is not accidental — ThinkingRoot is a Rust project, and Rust infrastructure components provide the predictable latency and memory efficiency needed for a registry at scale.

---

## The One Thing That Makes This Survive Millions

**Content-addressed deduplication across the entire registry.**

When two publishers independently compile the same source material:
- Publisher A compiles Django 5.0 docs → BLAKE3 claim hashes: `{abc, def, ghi, ...}`
- Publisher B compiles Django 5.0 docs → BLAKE3 claim hashes: `{abc, def, ghi, ...}`

The claims are content-identical. The hashes match. The hub stores ONE copy.

At scale with millions of packages, this deduplication is massive:
- 1,000 packages about Python → shared claims about core Python (stored once)
- 500 packages about React → shared claims about JSX, hooks, etc. (stored once)
- Forks share 95%+ content with parent → delta is 5%

This is the same property that makes Git efficient at petabyte scale. It's not a feature — it's the foundational architecture that makes the economics work.
