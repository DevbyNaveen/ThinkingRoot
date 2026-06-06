# Phase 4 — Cloud CLI Specification

**Date:** 2026-04-10  
**Status:** Planned — implementation starts after Phase 3.5 merges to main  
**Repo:** Private (Phase 4 backend) + OSS contributions to thinkingroot-cli  
**Note:** All CLI commands below are open source code in `thinkingroot-cli`. They require the Phase 4 cloud backend to function.

---

## New Commands

### `root login`

Authenticate with thinkingroot.dev (or a self-hosted instance). Stores JWT locally. Offers to sync existing workspaces immediately.

**Interactive flow:**
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

**Flags:**
```bash
root login                          # browser OAuth (default)
root login --token <token>          # non-interactive, paste token directly
root login --endpoint <url>         # self-hosted backend (enterprise)
```

**What it does internally:**
1. Interactive prompt: browser or token
2. If browser: opens `https://thinkingroot.dev/auth?cli=1`, polls for JWT callback
3. Stores JWT + org + endpoint at `~/.config/thinkingroot/auth.toml`
4. Reads `WorkspaceRegistry` — lists registered workspaces with claim counts
5. Offers immediate sync (Y/n) — calls `root sync --all` if yes
6. Prints next-steps hint

**Auth file format:**
```toml
# ~/.config/thinkingroot/auth.toml
token = "eyJhbGciOiJIUzI1NiIs..."
org = "acme-corp"
email = "naveen@company.com"
endpoint = "https://api.thinkingroot.dev"   # or self-hosted URL
expires_at = "2026-07-10T09:14:00Z"
```

---

### `root logout`

Remove stored credentials. All local commands continue to work unchanged.

```bash
root logout
# Output:
#   ✓ Logged out. Local workspaces and knowledge are untouched.
```

Deletes `~/.config/thinkingroot/auth.toml`. Local `graph.db`, artifacts, branches — all untouched.

---

### `root sync`

Push compiled local knowledge to the cloud. Uploads claims, entities, and relations — never raw source files.

```bash
root sync                              # sync active workspace (from HEAD or cwd)
root sync --workspace my-repo         # sync specific workspace by name
root sync --all                        # sync all registered workspaces
root sync --branch feature/graphql    # push a branch for team Knowledge PR review
root sync --dry-run                    # show what would be synced, no upload
```

**What gets synced (from graph.db):**
- Claims (statement, type, confidence, source URI, created_at)
- Entities (canonical name, type, aliases, description)
- Relations (from, to, type, strength)
- Contradictions (unresolved only)
- Health score snapshot

**What never gets synced:**
- Raw source files (`.rs`, `.md`, `.py`, etc.)
- The graph.db file itself (contents are serialised, not the SQLite file)
- LLM API keys or credentials
- Local config secrets

**Incremental sync:** only new/changed items since last sync are uploaded. First sync uploads everything; subsequent syncs are fast.

**Output:**
```
$ root sync

  ⠸ Syncing my-repo ...
    + 28 new claims
    + 6 new entities
    ~ 3 updated entities (new aliases)
    ✓ done in 1.2s

  Cloud graph updated: https://thinkingroot.dev/acme-corp/my-repo
```

---

### `root serve --federated`

Serve a federated view — queries span all workspaces the org has synced to the cloud.

```bash
root serve --federated                 # proxy to cloud, all org workspaces
root serve --federated --port 3001     # custom port
```

Requires `root login`. Acts as a local proxy that forwards queries to the cloud federated endpoint. All existing MCP tools and REST endpoints work identically — the workspace scope just expands to org-wide.

---

### `root connect github --webhook`

Register a GitHub webhook on the cloud backend. When code is pushed to the repo, the cloud automatically recompiles and syncs the knowledge graph.

```bash
root connect github --webhook          # requires root login
# Output:
#   ✓ Webhook registered: https://api.thinkingroot.dev/hooks/github/acme-corp/my-repo
#   ✓ Add to GitHub: Settings → Webhooks → paste URL above
#   ✓ On every push to main, ThinkingRoot will recompile automatically
```

This is separate from `root connect github` (OSS, which wires MCP to tools). The `--webhook` flag activates the auto-refresh pipeline on the cloud backend.

---

## Modified Commands (cloud-aware after login)

### `root compile` with `--sync` flag

```bash
root compile ./my-repo --sync
# compiles locally, then automatically calls root sync
# shortcut for: root compile ./my-repo && root sync
```

### `root diff` with cloud output

After login, `root diff` posts the Knowledge PR to the cloud so teammates can view it in the web UI:

```bash
root diff feature/graphql
# terminal output as usual
# + if logged in: "Knowledge PR posted: https://thinkingroot.dev/acme-corp/pr/14"
```

### `root sync --branch`

Push a local branch to cloud for team review:

```bash
root sync --branch feature/graphql
# Output:
#   ✓ Branch synced to cloud
#   Knowledge PR: https://thinkingroot.dev/acme-corp/my-repo/pr/14
#   Share with your team for review before merging
```

---

## CloudAuth Struct (thinkingroot-core addition)

```rust
// crates/thinkingroot-core/src/global_config.rs (extend)

pub struct CloudAuth {
    pub token: String,
    pub org: String,
    pub email: String,
    pub endpoint: String,          // default: "https://api.thinkingroot.dev"
    pub expires_at: DateTime<Utc>,
}

impl CloudAuth {
    /// Load from ~/.config/thinkingroot/auth.toml
    /// Returns None if not logged in
    pub fn load() -> Option<Self>

    /// Save after successful login
    pub fn save(&self) -> Result<()>

    /// Delete on logout
    pub fn clear() -> Result<()>

    /// Check if token is still valid
    pub fn is_valid(&self) -> bool {
        self.expires_at > Utc::now()
    }
}
```

---

## Sync API (cloud backend contract)

The cloud backend must expose these endpoints for `root sync` to call:

```
POST /api/v1/sync/workspace
  Body: { workspace_name, claims[], entities[], relations[], health_score }
  Auth: Bearer <JWT>
  Returns: { ok, synced_at, delta: { new_claims, new_entities } }

POST /api/v1/sync/branch
  Body: { workspace_name, branch_name, claims[], entities[], relations[] }
  Auth: Bearer <JWT>
  Returns: { ok, pr_url }

GET  /api/v1/sync/status
  Auth: Bearer <JWT>
  Returns: { workspaces: [{ name, last_synced, claim_count }] }
```

---

## Config Changes

New `[cloud]` section in both per-workspace and global config:

```toml
# ~/.config/thinkingroot/config.toml  (global)
[cloud]
endpoint = "https://api.thinkingroot.dev"   # override for self-hosted
auto_sync = false                            # if true, root compile triggers root sync automatically
```

Auth token is stored separately in `auth.toml` (not in config.toml) so config can be committed to version control safely without leaking credentials.

---

## Phase 4 CLI Implementation Scope

**Changes to OSS repo (`thinkingroot-cli`):**
- New commands: `root login`, `root logout`, `root sync`
- Modified commands: `root compile --sync`, `root diff` (cloud PR posting), `root connect github --webhook`
- Modified commands: `root serve --federated`
- New struct in `thinkingroot-core`: `CloudAuth`
- New config section: `[cloud]` in `GlobalConfig`

**Changes to private Phase 4 repo:**
- Cloud backend API endpoints (`/api/v1/sync/*`)
- OAuth flow + JWT issuance
- Federated query engine
- Webhook processing pipeline
- Web UI for Knowledge PRs

---

## User Journey: OSS → SaaS in Full

```
Day 1 (OSS):
  root setup                    # configure LLM provider
  root compile ./my-repo        # extract knowledge locally
  root serve                    # serve locally for MCP tools

Day 30 (team joins, needs SaaS):
  root login                    # one command
  → browser opens, authenticate
  → 3 workspaces detected, offer sync
  → sync all (Y)
  → done in ~10 seconds

Day 31 (SaaS workflow):
  root compile ./my-repo        # still local (fast)
  root sync                     # push to cloud (team sees it)
  root branch feature/x         # local branch
  root sync --branch feature/x  # share for team review
  root merge feature/x          # merge after approval

Day N (enterprise, air-gapped):
  root login --endpoint https://thinkingroot.internal.company.com
  → everything same, data stays on-premise
```
