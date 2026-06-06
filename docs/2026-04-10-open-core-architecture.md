# ThinkingRoot Open Core Architecture

**Date:** 2026-04-10  
**Status:** Design decision — Phase 4 planning reference

---

## The Model

ThinkingRoot follows the **Open Core** model: the CLI binary (`root`) is fully open source, and cloud infrastructure (Phase 4) is the private, proprietary backend.

This is the same model used by HashiCorp (Terraform + Terraform Cloud), Grafana, and GitLab CE/EE.

---

## Feature Split: OSS vs SaaS

| Capability | OSS (local-first) | SaaS (cloud) |
|---|---|---|
| Compilation | `root compile` — manual, local, needs LLM API key | Cloud workers — triggered on push, no key needed |
| Graph storage | `.thinkingroot/graph.db` — per-repo, local | Central cloud graph — persistent, cross-repo |
| Workspaces | One `root serve` per machine | Federated — all repos in one org graph |
| Auto-refresh | Manual only | Webhook → queue → recompile → live graph |
| Search | Local vector store | Cross-repo, cross-workspace, org-scale |
| Access control | None (localhost) | Teams, roles, SSO |
| MCP wiring | `root connect` — local tools only | `root connect --webhook` — cloud-backed |
| Dashboard | None | Web UI at thinkingroot.dev |

Everything in the OSS column works fully offline. SaaS features are additive — they require `root login` and an active cloud backend.

---

## Binary Architecture

The OSS binary is the single runtime for both local and cloud use. There is no separate "cloud binary."

```
OSS binary (this repo)               Cloud backend (Phase 4, private repo)
────────────────────────             ──────────────────────────────────────
root compile        ──── root sync ──→  upload compiled graph
root serve          ──── root serve --federated ←── proxy org queries
root connect        ──── root connect github --webhook ──→  register push webhook
                         root login  ──→  exchange credentials for JWT
                                          stored at ~/.config/thinkingroot/auth.toml
```

### How cloud features gate at runtime

```rust
// All SaaS features gate on stored auth token
if let Some(auth) = CloudAuth::load()? {
    // Cloud path — calls api.thinkingroot.dev
} else {
    // Local path — no degraded mode, feature simply unavailable
}
```

`CloudAuth` is loaded from `~/.config/thinkingroot/auth.toml`. No token = no cloud features. Token present = cloud API calls are made with it.

---

### `root login` — the full interactive flow

The entire OSS → SaaS migration is one command. `root login` handles authentication and offers to sync existing workspaces immediately.

```
$ root login

  ThinkingRoot Cloud

  How would you like to authenticate?
  › Log in with browser
    Paste an API token

  ✓ Opening https://thinkingroot.dev/auth ...
  ⠸ Waiting for authentication...

  ✓ Logged in as naveen@company.com (org: acme-corp)

  ─────────────────────────────────────────────────
  You have 3 registered workspaces:

    my-repo      ~/projects/my-repo        (843 claims)
    backend-api  ~/projects/backend-api    (1,204 claims)
    docs-site    ~/projects/docs-site      (312 claims)

  Sync them to the cloud now? (Y/n) › y

  ⠸ Syncing my-repo ...      ✓ done  (843 claims, 124 entities)
  ⠸ Syncing backend-api ...  ✓ done  (1,204 claims, 287 entities)
  ⠸ Syncing docs-site ...    ✓ done  (312 claims, 44 entities)

  ✓ All workspaces synced

  Your team can now access knowledge at:
    https://thinkingroot.dev/acme-corp

  Next time you compile, sync automatically:
    root compile ./my-repo && root sync
  ─────────────────────────────────────────────────
```

**Implementation steps inside `root login`:**
1. Interactive prompt: browser OAuth or paste token
2. If browser: open `https://thinkingroot.dev/auth?cli=1`, poll for callback JWT
3. Store JWT + org name at `~/.config/thinkingroot/auth.toml`
4. Read `WorkspaceRegistry` — list all registered workspaces with claim counts
5. Offer immediate sync (Y/n) — if yes, call `root sync --all`
6. Print next-steps hint

**`root logout`:**
```
$ root logout
  ✓ Logged out. Local workspaces and knowledge are untouched.
```
Deletes `~/.config/thinkingroot/auth.toml`. All local commands continue to work.

---

## Federated Workspaces

**OSS:** `root serve` mounts multiple local paths. Each workspace is a separate local graph.

**SaaS:** `root serve --federated` becomes a thin proxy:

```
root serve --federated
  → checks auth token
  → calls cloud /api/v1/org/{org}/query
  → returns merged results from all repos the org has synced
```

Local `root serve` remains fully useful for private local work. Federated is additive.

---

## Auto-Refresh (Webhook-Based Recompilation)

**OSS:**
```bash
root connect github    # wires MCP to Claude Desktop — local only
```

**SaaS:**
```bash
root connect github --webhook   # registers push webhook on cloud backend
                                # push → cloud queues root compile → graph updated live
```

The `--webhook` flag requires an auth token. Without login, the flag is documented but rejected with a clear error.

---

## Security Model

The binary is fully open source. The lock is in the **backend**, not the binary.

### How it works

```
root login  →  user authenticates at thinkingroot.dev
            →  backend issues signed JWT (short-lived + refresh token)
            →  stored at ~/.config/thinkingroot/auth.toml

root sync   →  JWT sent in Authorization header
            →  backend validates JWT signature (private key never in source)
            →  org scoping enforced server-side
            →  invalid/expired token → 401 → nothing works
```

### Why open sourcing the client code is not a security risk

The API endpoints (e.g. `https://api.thinkingroot.dev/sync`) are visible in source. This is intentional and safe because:

- Every request requires a valid JWT signed by the backend's private key
- Forging or bypassing the local auth check does not produce a valid JWT
- Tokens are org-scoped — one user's token cannot access another org's data
- Rate limiting and HTTPS enforced on all endpoints

| Attempt | Result |
|---|---|
| Modify binary to skip local auth check | Still requires valid JWT for every API call — backend rejects |
| Replay another user's token | Account compromise — standard API security (HTTPS + short expiry mitigates) |
| Build own backend, point binary to it | Self-hosting — valid enterprise use case, not a threat |

### What must never be in source (Phase 4 private repo only)

- JWT signing private key
- Database credentials and connection strings
- Webhook secrets (GitHub, GitLab)
- Stripe or billing API keys
- Internal service-to-service tokens

---

## Phase 4 Private Repo Scope

The Phase 4 private repo builds and owns:

- Cloud compilation workers (async job queue, `root compile` in cloud)
- Central graph storage (persistent CozoDB or equivalent at scale)
- Federated query engine (cross-repo, cross-workspace search)
- Webhook processing (GitHub/GitLab push → recompilation queue)
- Auth service (JWT issuance, refresh, revocation)
- Web dashboard (thinkingroot.dev)
- Team and access control
- Connectors (Notion, Confluence, Jira, Linear)
- Billing integration

The Phase 4 repo imports Phase 1–3.5 (this repo) as a dependency.

---

## Local → SaaS Migration

When a local OSS user is ready to move to SaaS, migration is three commands and zero data loss.

### What gets synced — claims, not source code

`root sync` uploads the **compiled knowledge** (claims, entities, relations) to the cloud — NOT the raw source files. Proprietary source code never leaves the local machine. The cloud receives only what the LLM extracted: facts, entities, and relationships.

### Migration path

```bash
# Step 1: authenticate (one-time)
root login
# → opens browser to thinkingroot.dev/auth
# → JWT stored at ~/.config/thinkingroot/auth.toml

# Step 2: push existing local knowledge to cloud
root sync
# → uploads .thinkingroot/graph.db state (claims + entities + relations)
# → cloud creates persistent graph from it
# → NO re-compilation, NO LLM calls, NO API key needed for this step
# → local data is untouched

# Step 3: continue working exactly as before
root compile ./new-docs     # still compiles locally
root sync                   # push new knowledge to cloud after each compile
```

After migration, the workflow is: compile locally → sync to cloud. The local graph.db remains the local cache. The cloud graph becomes the persistent, shareable, always-on copy.

### Multi-workspace migration

```bash
root sync                          # sync active workspace
root sync --workspace my-org       # sync a specific workspace by name
root sync --all                    # sync all registered workspaces
```

### Branch migration

Local branches are not synced by default — they stay private until the user explicitly shares them:

```bash
root sync --branch feature/graphql
# → pushes branch to cloud
# → team members can review Knowledge PR in the web UI
# → local branch directory untouched
```

### Reverting to local-only

At any point the user can stop using the cloud:

```bash
root logout
# → removes JWT from ~/.config/thinkingroot/auth.toml
# → all subsequent commands run fully local
# → local .thinkingroot/ is completely untouched
# → nothing was ever deleted from the local machine
```

This is the **local-first guarantee**: the local graph is always the source of truth. The cloud is a sync target. If the cloud goes down, all local commands continue to work.

### What changes after migration

| | Before (OSS local) | After (SaaS) |
|--|---------------------|--------------|
| `root compile` | compiles locally | still compiles locally |
| `root serve` | serves locally | still serves locally (+ cloud endpoint available) |
| `root branch/diff/merge` | local only | local + team can see branches in web UI |
| Knowledge persistence | `.thinkingroot/graph.db` only | local + cloud backup |
| Team access | none | teammates can query cloud endpoint |
| Source code exposure | none | none — only claims are synced |

## Self-Hosting

Enterprise customers can run the Phase 4 backend on their own infrastructure. The OSS binary points to their instance via:

```toml
# ~/.config/thinkingroot/config.toml
[cloud]
endpoint = "https://thinkingroot.internal.mycompany.com"
```

Self-hosting gives enterprises the full SaaS feature set (federated workspaces, web UI, team access) while keeping data completely on-premise. This is Phase 5 tier.
