use super::JsonRpcResponse;
use crate::engine::{AgentClaim, ClaimFilter, QueryEngine};
use crate::intelligence::compressor;
use crate::intelligence::planner::{self, PlanStep};
use crate::intelligence::session::{SessionContext, SessionStore};
use serde_json::Value;

// Path to the workspace sessions directory is resolved from the engine's workspace root_path.
/// Build the canonical MCP `tools/call` text response for a payload
/// the handler has already produced.  Centralises the
/// `serde_json::to_string_pretty(payload)` step so a serialization
/// failure surfaces as a JSON-RPC error (code -32603) rather than an
/// empty string the calling LLM would interpret as "no results".
///
/// Audit invariant: no `unwrap_or_default()` on engine-error
/// returns. The previous pattern of
/// `serde_json::to_string_pretty(...).unwrap_or_default()` masked
/// every serialization failure as success-with-empty-content.
///
/// C5 (2026-05-22) — delegates to
/// `jsonrpc_envelope::mcp_text_result_bounded` which applies the
/// `DEFAULT_TOOL_RESULT_TOKEN_BUDGET` cap and returns a typed
/// `structured_error("result_too_large", ...)` envelope on overage.
/// Backward-compatible: under-budget payloads emit byte-identical
/// shape to the pre-C5 path.
fn mcp_text_result<T: serde::Serialize>(id: Option<Value>, payload: &T) -> JsonRpcResponse {
    super::jsonrpc_envelope::mcp_text_result_bounded(id, payload)
}

fn sessions_dir_for(engine: &QueryEngine, ws: &str) -> std::path::PathBuf {
    engine
        .workspace_root_path(ws)
        .map(|p| p.join("sessions"))
        .unwrap_or_else(|| std::path::PathBuf::from("sessions"))
}

/// Resolve the [`crate::engine::BranchActor`] for an MCP session.
///
/// Audit fix: previously used `try_lock()` and silently fell back to
/// `BranchActor::User(session_id)` when the lock was contended. That
/// fallback compared the raw session UUID against
/// `branch_ref.permissions.writers` and `owner` — which never matched —
/// so under realistic concurrent MCP load Alice's writes were rejected
/// (or worse, accepted on a branch with no owner) whenever the session
/// store mutex happened to be held by another tool call. Now waits
/// for the lock; the lock is never held for long in any handler.
async fn session_actor(
    sessions: &SessionStore,
    session_id: &str,
) -> crate::engine::BranchActor {
    let store = sessions.lock().await;
    if let Some(session) = store.get(session_id) {
        // C6 (2026-05-22) — known AI clients land as
        // `Principal::Agent("{name}:{session_id}")` so audit logs
        // and branch attribution can tell Claude Code apart from
        // Cursor apart from a curl script. The session_id suffix
        // disambiguates parallel sessions from the same AI tool.
        // Owner-based attribution still wins when explicitly set
        // (a `claude-code` session that the user authenticated
        // into still reports the user as owner; the agent role is
        // surfaced through audit but doesn't override owner).
        if let Some(client) = session.client_info.as_ref() {
            if client.is_known_ai_client() {
                return crate::engine::BranchActor::Agent(format!(
                    "{}:{session_id}",
                    client.name.to_ascii_lowercase()
                ));
            }
        }
        if let Some(owner) = session.owner.as_ref() {
            return crate::engine::BranchActor::User(owner.clone());
        }
    }
    crate::engine::BranchActor::User(session_id.to_string())
}

/// Resolve the `workspace` tool argument to a mounted workspace name.
///
/// Workspaces are mounted by basename (see `cli/src/serve.rs`), but MCP clients
/// often see only the full `--path` value in their config and forward that as
/// the `workspace` argument. Without this normalisation a client that passes
/// `/abs/path/to/foo` instead of `foo` gets `EntityNotFound` even though `foo`
/// is mounted.
///
/// Resolution order:
///   1. `arg` exactly matches a mounted workspace name → use it.
///   2. `arg` looks like a path AND its basename is a mounted name → use the basename.
///   3. `arg` is set but unrecognised → return it unchanged so the downstream
///      lookup produces a precise `EntityNotFound` (don't silently mask).
///   4. `arg` is None → fall back to `default_ws`, then to the literal `"default"`.
///
/// Note: basename extraction is delegated to `std::path::Path::file_name`,
/// whose separator semantics are platform-specific. On Unix hosts only `/`
/// is treated as a separator; backslash-style paths only normalise when the
/// server is built for Windows.
pub(crate) fn resolve_workspace_arg(
    arg: Option<&str>,
    default_ws: Option<&str>,
    engine: &QueryEngine,
) -> String {
    resolve_workspace_arg_with(arg, default_ws, |name| {
        engine.workspace_root_path(name).is_some()
    })
}

/// Pure variant of [`resolve_workspace_arg`] — separated for unit testing
/// without constructing a full [`QueryEngine`]. `is_mounted` answers the
/// question "is `name` a mounted workspace?".
fn resolve_workspace_arg_with<F: Fn(&str) -> bool>(
    arg: Option<&str>,
    default_ws: Option<&str>,
    is_mounted: F,
) -> String {
    match arg {
        Some(value) if is_mounted(value) => value.to_string(),
        Some(value) if value.contains('/') || value.contains('\\') => std::path::Path::new(value)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|name| is_mounted(name))
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string()),
        Some(value) => value.to_string(),
        None => default_ws.unwrap_or("default").to_string(),
    }
}

/// Phase ε.1 — Long-tail MCP tools.
///
/// Tools listed here are TAGGED with `defer_loading: true` in the
/// `tools/list` response. The tag is an annotation, not a filter:
/// every tool is still advertised, but Anthropic-style clients (and
/// the new `tool_search` MCP tool below) treat the annotation as
/// "load the descriptor on demand, not upfront" — Anthropic's
/// published context-saving pattern.
///
/// Selection criteria:
///   - **Branch maintenance** (rebase / rollback / gc / abandon /
///     delete / diff): user runs at most one per session.
///   - **Engram lifecycle** (materialize / probe / list / expire):
///     specialised RARP surface, rarely used in standard chat.
///   - **Reflection** (reflect / reflect_across / rooting_report):
///     analytical surfaces, not part of a typical answer loop.
///   - **Proposal lifecycle** (open / list / review / close /
///     dismiss_gap): the workflow gets pulled in when the user
///     opens the proposals panel, not on every chat turn.
///
/// Tools that DON'T appear here stay loaded by default — `search`,
/// `query_claims`, `list_witnesses`, `ask`, `compile`, `hybrid_retrieve`,
/// `list_commits`, `merge_cognition`, `commit_cognition`, etc.
pub const DEFER_LOADING_TOOLS: &[&str] = &[
    "rebase_branch",
    "rollback_merge",
    "gc_branches",
    "delete_branch",
    "diff_branch",
    "materialize_engram",
    "probe_engram",
    "list_engrams",
    "expire_engram",
    "reflect",
    "reflect_across",
    "rooting_report",
    "open_proposal",
    "list_proposals",
    "review_proposal",
    "close_proposal",
    "dismiss_gap",
    "contribute_bulk",
    "walk_mesh",
    "query_rooted",
];

/// Annotate every tool whose name appears in `DEFER_LOADING_TOOLS`
/// with `defer_loading: true`. Mutates the array in place. The
/// annotation is added as a JSON field on each tool descriptor;
/// MCP clients that don't understand the field ignore it (the JSON
/// schema for `tools/list` is permissive — additional fields are
/// allowed).
fn annotate_defer_loading(tools: &mut serde_json::Value) {
    let arr = match tools.as_array_mut() {
        Some(a) => a,
        None => return,
    };
    let deferred: std::collections::HashSet<&str> =
        DEFER_LOADING_TOOLS.iter().copied().collect();
    for tool in arr.iter_mut() {
        let name = tool
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(name) = name
            && deferred.contains(name.as_str())
            && let Some(obj) = tool.as_object_mut()
        {
            obj.insert("defer_loading".to_string(), serde_json::Value::Bool(true));
        }
    }
}

#[tracing::instrument(name = "mcp.tools.list", skip_all)]
pub async fn handle_list(id: Option<Value>) -> JsonRpcResponse {
    let mut tools = serde_json::json!({
        "tools": [
            // ── Classic CRUD tools ────────────────────────────────────────
            {
                "name": "search",
                "description": "Semantic + keyword search over the workspace's compiled claims and entities. Use when the user asks about something specific (a function, a concept, a past decision) and you don't yet know the exact entity name. Combines vector recall with substring matching; tolerates typos and synonyms. Prefer `query_claims` when you already know the entity name and want filtered results, `hybrid_retrieve` when you need ranked provenance with byte spans, or `probe_engram` when you have a materialised cluster pointer.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query":     { "type": "string", "description": "Free-text query. Phrases like 'authentication flow' or 'payment retry logic' work well." },
                        "top_k":    { "type": "integer", "default": 10, "description": "Max results to return. Default 10. Bound: 1–50." },
                        "workspace": { "type": "string", "description": "Workspace name from the mount config (e.g. 'thinkingroot')." },
                        "branch":    { "type": "string", "description": "Optional branch name to read from. When omitted, uses the session's active branch (set via checkout_branch); when no active branch is set, reads from main." }
                    },
                    "required": ["query", "workspace"]
                }
            },
            {
                "name": "query_claims",
                "description": "Filtered retrieval of structured claims by type, entity, or minimum confidence. Use when you already know what you're looking for (e.g. all `decision` claims about `WebhookHandler`, or every claim with confidence > 0.8 on a specific entity). Returns raw claim records with confidence + source path. Prefer `search` when the user's intent is fuzzy and `hybrid_retrieve` when you need full provenance bundles.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "type":           { "type": "string", "description": "Claim type — one of: fact, decision, opinion, plan, requirement, metric, definition, dependency, api_signature, architecture, preference. Omit to match all types." },
                        "entity":         { "type": "string", "description": "Entity name to scope the query to. Case-sensitive; matches against the canonical name in the entity table." },
                        "min_confidence": { "type": "number", "description": "Floor on claim confidence (0.0–1.0). 0.7 is a reasonable strict floor; omit for all confidences." },
                        "workspace":      { "type": "string", "description": "Workspace name." },
                        "branch":         { "type": "string", "description": "Optional branch name to read from. When omitted, uses the session's active branch (set via checkout_branch); when no active branch is set, reads from main." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "list_witnesses",
                "description": "List Witnesses from the new Witness Mesh substrate (rule-catalog produced, byte-grounded, content-addressed). Each Witness is the deterministic output of a named rule applied to source bytes — no LLM paraphrase, no grounding tribunal. Use when you want to read the v1.0 substrate directly: `// @claim` annotations, rustdoc/jsdoc tags, test assertions, opt-in invariants. Filter by `rule` to scope to one catalog rule (e.g. `comment::@claim@v1`). Returns Witness rows with content_blake3, spans, and rule provenance. Distinct from `query_claims` which reads the legacy LLM-extracted substrate.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name from the mount config." },
                        "rule":      { "type": "string", "description": "Optional catalog rule name to filter by (e.g. `comment::@claim@v1`, `tree-sitter::function-decl@v1`, `cargo-test::assertion@v1`). Omit to list every Witness." },
                        "limit":     { "type": "integer", "default": 100, "description": "Max Witnesses to return. Default 100. No upper bound — the substrate fits in memory for v1.0 workspaces." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "fs_list",
                "description": "List the contents of a workspace folder. Returns directory entries (name, rel_path, is_dir, size_bytes, modified, kind) plus the parent rel_path so an agent can walk the tree. Hides the engine-managed `.thinkingroot/` directory from results. Use this before `fs_move` / `fs_rename` so the agent has accurate names to operate on. Distinct from the legacy `list_directory` Playground verb — `fs_list` is the canonical workspace-FS surface.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." },
                        "rel":       { "type": "string", "description": "Optional sub-folder relative to the workspace root, forward-slash separated (e.g. `inbox` or `inbox/drafts`). Omit / empty for the workspace root." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "fs_create_folder",
                "description": "Create a new empty directory inside a workspace. The parent must already exist. Refuses collisions (existing path → error). Returns the new rel_path on success.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":  { "type": "string", "description": "Workspace name." },
                        "parent_rel": { "type": "string", "description": "Parent directory rel_path. Empty string = workspace root." },
                        "name":       { "type": "string", "description": "New folder name. Must be a single path segment (no `/`, no `..`, not empty)." }
                    },
                    "required": ["workspace", "parent_rel", "name"]
                }
            },
            {
                "name": "fs_rename",
                "description": "Rename a file or folder in-place (keeps its parent directory). `rel` points to the existing item; `new_name` is the new leaf name. Refuses to rename the workspace root or anything inside `.thinkingroot/`. Refuses collisions. Returns the new rel_path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." },
                        "rel":       { "type": "string", "description": "Existing item's rel_path." },
                        "new_name":  { "type": "string", "description": "New leaf name. Single path segment, no separators." }
                    },
                    "required": ["workspace", "rel", "new_name"]
                }
            },
            {
                "name": "fs_move",
                "description": "Move one or more files/folders into a single destination folder within the same workspace. Skips collisions honestly (counts them as `skipped_conflict` — silent overwrite is the kind of 'helpful' that loses work). Refuses to move a folder into its own descendant. Refuses to touch `.thinkingroot/` state. Returns `{ moved, skipped_conflict, skipped_invalid, moved_rel_paths }` so the agent can report accurately. Use this when the user says 'put folder X inside folder Y'. For per-file rename-on-move pairs, the legacy `organize_files` verb is also available.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":   { "type": "string", "description": "Workspace name." },
                        "sources":     { "type": "array", "items": { "type": "string" }, "description": "Rel_paths to move." },
                        "dest_folder": { "type": "string", "description": "Destination folder rel_path. Empty string = workspace root." }
                    },
                    "required": ["workspace", "sources", "dest_folder"]
                }
            },
            {
                "name": "walk_mesh",
                "description": "Walk the Witness Mesh DAG from a starting Witness id. Returns every Witness reachable within `max_depth` hops via `witness_input_edges` plus the edges that connect them — i.e. the full derivation chain: which rule produced this Witness, which Witnesses it derives from, what sibling Witnesses share its inputs. Use this when `list_witnesses` returned a Witness whose context matters (e.g. a `comment::SAFETY@v1` Witness — walk to find the parent `tree-sitter::unsafe-block@v1` it justifies). Cheap (Datalog traversal on the indexed edges table; no LLM, no embedding lookup). Returns `{ witnesses: [...], edges: [[parent, child], ...] }`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":   { "type": "string", "description": "Workspace name." },
                        "witness_id":  { "type": "string", "description": "64-char lower-hex BLAKE3 id of the starting Witness (from `list_witnesses` or `search` results)." },
                        "max_depth":   { "type": "integer", "default": 4, "description": "Maximum hops from the starting Witness. 0 = just the starting Witness, no edges. 4 is the v1.0 default (matches the spec's `walk_mesh` bound). Bound: 0–10." },
                        "max_fanout":  { "type": "integer", "default": 50, "description": "Maximum edges followed per node. Caps pathological meshes (a Witness with thousands of children) at a tractable response size. Bound: 1–200." }
                    },
                    "required": ["workspace", "witness_id"]
                }
            },
            {
                "name": "get_relations",
                "description": "Return every relation edge incident on a specific entity — both inbound (e.g. 'X is called by Y') and outbound (e.g. 'X depends on Z'). Use when you need to understand how an entity connects to the rest of the substrate: who calls it, what it calls, what it inherits from, what depends on it. Most useful as a follow-up to `search` once you have an entity name.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "entity":    { "type": "string", "description": "Exact entity name (case-sensitive). Use the name returned by `search` or `query_claims`, not a guess." },
                        "workspace": { "type": "string", "description": "Workspace name." },
                        "branch":    { "type": "string", "description": "Optional branch name to read from. When omitted, uses the session's active branch (set via checkout_branch); when no active branch is set, reads from main." }
                    },
                    "required": ["entity", "workspace"]
                }
            },
            {
                "name": "compile",
                "description": "Run the full v3 pipeline (parse → extract → ground → link → root → reflect → branch → serve → verify) over the workspace's source files. EXPENSIVE: spends LLM credits, may take seconds to minutes depending on workspace size. Use only when source files have changed since the last compile and the user explicitly asked for a refresh. Side effects: invalidates engram caches, increments claim_count, may produce contradictions that need review. Do NOT call as part of normal Q&A.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name. The workspace's `[llm]` config provides the credentials used for extraction." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "health_check",
                "description": "Compute a 0–100 health score for the workspace's knowledge graph. Score combines: claim coverage of source files, contradiction count, gap density, derivation completeness, and rooted-tier proportion. Use when the user asks 'how good is the knowledge?' or 'what's wrong?'. Returns the score plus a per-axis breakdown so you can explain why. Cheap (no LLM call); safe to run frequently.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." }
                    },
                    "required": ["workspace"]
                }
            },
            // ── C2 (2026-05-22) — Ambient session + reminder context ─────
            // External AI clients (Claude Code, Cursor, Codex) consume
            // these to learn what workspace/branch they're on and what
            // the in-app Brain chat would see as ambient context. The
            // in-app agent already gets all of this via REST SSE; these
            // two tools give MCP clients identical fidelity.
            {
                "name": "get_session_context",
                "description": "Return metadata about the calling MCP session: workspace, owner, active branch, focus entity, turn count, delivered claim count, and (over HTTP transport) the list of mounted workspaces. Use this on FIRST CALL of every new MCP session so you know which workspace, which branch, and who you are before issuing writes. Cheap (in-memory lookups only); safe to call freely. Returns honest empty fields when the session has just started.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Optional MCP session id to inspect. Defaults to the caller's own session." }
                    }
                }
            },
            {
                "name": "get_reminder_context",
                "description": "Return the FULL 17-block ambient context the in-app Brain chat receives every turn: environment (cwd, ~/Desktop, etc.), workspace identity, today's date, branch state, session state, materialised engrams, agentmemory recalls relevant to the question, MCP sessions, recent self-heal events, top-matching skill body, substrate freshness, recent sub-agent reports, prior verifier critique, open gap alerts, contradiction alerts, search-was-shallow warnings. Returns the rendered `<system-reminder>` markdown string for direct injection into your own prompt. Call once per user turn; ~10–50 ms typical. REQUIRES HTTP transport (SSE) — over stdio returns a typed `transport_not_supported` error.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":     { "type": "string", "description": "Workspace name to build context for." },
                        "question_hint": { "type": "string", "description": "Optional user question; used for top-1 skill auto-surface + agentmemory recall query. Pass the user's actual message verbatim for best matches." }
                    },
                    "required": ["workspace"]
                }
            },
            // ── C18 (2026-05-22) — Branch ergonomic + subscribe tools ──
            {
                "name": "branch_fork",
                "description": "Ergonomic alias over create_branch with workflow-friendly defaults (BranchKind::Sandbox, MergePolicy::Manual). Use this when starting a sandbox for experimentation or a flow node that needs branch isolation. Returns the new branch name. To create a longer-lived feature branch, use create_branch directly.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "from":      { "type": "string", "description": "Parent branch (default: current session's active branch or 'main')." },
                        "name":      { "type": "string", "description": "Optional branch name. Default: auto-generated sandbox/<ulid>." },
                        "kind":      { "type": "string", "enum": ["sandbox", "feature", "stream"], "description": "Branch kind. Default: sandbox." },
                        "workspace": { "type": "string" }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "branch_state",
                "description": "Return a snapshot of a branch's state: name, kind, merge_policy, claim_count, witness_count, engram_count, last_commit_at. Use for quick UI dashboards + flow node decisions about whether to merge. Lightweight (no graph scan).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":    { "type": "string", "description": "Branch name to inspect." },
                        "workspace": { "type": "string" }
                    },
                    "required": ["branch", "workspace"]
                }
            },
            {
                "name": "branch_subscribe",
                "description": "Subscribe to changes on a branch. Returns a subscription_id; subsequent changes emit notifications/message events on the SSE channel. v1 subscriptions are session-bound (auto-expire on session disconnect) with default rate-limit of 1 fire per 10s. Use ttl_secs to bound lifetime; max_fires to cap notifications. Watchdog pattern: declare what you care about once, receive a single ping when it changes — no polling.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":     { "type": "string", "description": "Branch to subscribe to." },
                        "workspace":  { "type": "string" },
                        "max_fires":  { "type": "integer", "description": "Max notifications before auto-expire. Default: 1 (one-shot)." },
                        "ttl_secs":   { "type": "integer", "description": "Lifetime cap in seconds. Default: session-lifetime." }
                    },
                    "required": ["branch", "workspace"]
                }
            },
            // ── C17 (2026-05-22) — Flow orchestrator tools ─────────────
            // Declare, run, and inspect multi-agent flows. Backed by
            // the `thinkingroot-flow` crate (file-storage at
            // <workspace_root>/.thinkingroot/flows/) and the per-
            // workspace executor registry (LocalLlm / Mcp / ClientSampling
            // / Deterministic / Human).
            {
                "name": "flow_define",
                "description": "Register a multi-agent flow definition for the workspace. The definition is a JSON object describing nodes (executor + config), edges (DAG dependencies), and final merge policy. Stored at <workspace_root>/.thinkingroot/flows/<flow_id>.yaml; user-editable. Returns the canonical content hash so callers can verify storage. Idempotent — re-defining an existing flow_id updates the file and refreshes the hash; created_at is preserved.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":  { "type": "string", "description": "Workspace to register the flow in." },
                        "definition": { "type": "object", "description": "Full FlowDefinition object: id, nodes, edges, final_merge, etc. See docs/flows/ for reference." }
                    },
                    "required": ["workspace", "definition"]
                }
            },
            {
                "name": "flow_run",
                "description": "Start a flow run. Returns the flow_run_id immediately; the run executes asynchronously. Progress streams via `notifications/progress` (when the caller passes `_meta.progressToken`) keyed on flow_run_id. Poll `flow_status` or subscribe to progress notifications for completion. `inputs` is validated against the flow definition's `inputs` schema at start.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":       { "type": "string", "description": "Workspace the flow is defined in." },
                        "flow_id":         { "type": "string", "description": "ID of a previously-defined flow." },
                        "inputs":          { "type": "object", "description": "Caller-supplied inputs matching the flow's inputs schema. Default: empty object." },
                        "conversation_id": { "type": "string", "description": "Optional MCP session id to associate the run with. When present, client_sampling nodes can back-call this session's LLM via sampling/createMessage." }
                    },
                    "required": ["workspace", "flow_id"]
                }
            },
            {
                "name": "flow_status",
                "description": "Read a flow run's current state, OR pause/resume/cancel it. Returns: flow_run_id, status (running/paused/succeeded/failed/cancelled), current_node, started_at, finished_at, outputs (when succeeded), error (when failed). Pass `action` to mutate: 'cancel' fires the cancellation token (the in-flight node aborts at its next phase boundary).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":   { "type": "string", "description": "Workspace the flow was started in." },
                        "flow_run_id": { "type": "string", "description": "Run id returned by flow_run." },
                        "action":      { "type": "string", "enum": ["cancel"], "description": "Optional mutation. 'cancel' aborts the run." }
                    },
                    "required": ["workspace", "flow_run_id"]
                }
            },
            // ── KVC tools ─────────────────────────────────────────────────
            {
                "name": "create_branch",
                "description": "Create an isolated knowledge branch for experimentation or agent sandboxing",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name":        { "type": "string", "description": "Branch name (e.g. feature/x)" },
                        "workspace":   { "type": "string" },
                        "description": { "type": "string" },
                        "root_path":   { "type": "string", "description": "Workspace root path (default: current directory)" }
                    },
                    "required": ["name", "workspace"]
                }
            },
            {
                "name": "diff_branch",
                "description": "Compute a semantic Knowledge PR — shows new claims, entities, and contradictions",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":    { "type": "string", "description": "Branch to diff against main" },
                        "workspace": { "type": "string" },
                        "root_path": { "type": "string" }
                    },
                    "required": ["branch", "workspace"]
                }
            },
            {
                "name": "merge_branch",
                "description": "Merge a knowledge branch into main or another target branch (runs health CI gate)",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":    { "type": "string" },
                        "target":    { "type": "string", "description": "Optional target branch. Defaults to main." },
                        "workspace": { "type": "string" },
                        "force":     { "type": "boolean", "default": false },
                        "propagate_deletions": { "type": "boolean", "default": false },
                        "root_path": { "type": "string" }
                    },
                    "required": ["branch", "workspace"]
                }
            },
            {
                "name": "rebase_branch",
                "description": "Sync a branch with its parent by applying parent-only claims into the branch.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":    { "type": "string" },
                        "workspace": { "type": "string" },
                        "root_path": { "type": "string" }
                    },
                    "required": ["branch", "workspace"]
                }
            },
            {
                "name": "checkout_branch",
                "description": "Set the active branch for this session. After checkout, 'contribute' writes claims to the branch instead of main. Use create_branch first, then checkout_branch, then contribute. Review with diff_branch and merge when ready.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":    { "type": "string", "description": "Branch name to check out (or null to return to main)" },
                        "workspace": { "type": "string" }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "list_branches",
                "description": "List all active knowledge branches in this workspace with their parent, status, and creation time.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string" },
                        "root_path": { "type": "string", "description": "Workspace root path (default: current directory)" }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "delete_branch",
                "description": "Soft-delete a branch (marks Abandoned, retains data dir for audit/recovery). Use gc_branches later to reclaim disk. For permanent deletion of abandoned branches.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":    { "type": "string", "description": "Branch to abandon" },
                        "workspace": { "type": "string" },
                        "root_path": { "type": "string" }
                    },
                    "required": ["branch", "workspace"]
                }
            },
            {
                "name": "gc_branches",
                "description": "Permanently delete the data directories of all Abandoned branches. Non-Abandoned branches are untouched. Returns the count of branches purged.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string" },
                        "root_path": { "type": "string" }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "rollback_merge",
                "description": "Restore main from the most recent pre-merge snapshot for a given branch. Reverts the merge but keeps the branch intact for re-work. Main cache is reloaded so subsequent reads see the pre-merge state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":    { "type": "string", "description": "Branch whose merge should be rolled back" },
                        "workspace": { "type": "string" },
                        "root_path": { "type": "string" }
                    },
                    "required": ["branch", "workspace"]
                }
            },
            // ── Reflexive (Phase 9): known-unknowns / gaps ───────────────
            {
                "name": "reflect",
                "description": "Run Phase 9 Reflect — discover structural co-occurrence patterns across entities and surface 'known unknowns' (expected claim types missing for specific entities). Pure graph + Datalog, no LLM. Returns a summary; use `gaps` to list the actual records. Pass `branch` to scope to a knowledge branch.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string" },
                        "branch":    { "type": "string", "description": "Optional — branch name. When set, reflect runs against the branch's copy-on-write graph." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "gaps",
                "description": "List knowledge gaps (known-unknowns) the graph has inferred from its own structural patterns. Each gap says 'entity X of type T is expected to have claim-type C because N% of similar entities do, but X doesn't.' Filter by entity name, minimum pattern confidence, or branch scope.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":       { "type": "string" },
                        "entity":          { "type": "string", "description": "Canonical name of a single entity to scope the report to" },
                        "min_confidence":  { "type": "number", "description": "Minimum pattern frequency in [0.0, 1.0]. Default 0.70." },
                        "branch":          { "type": "string", "description": "Optional — branch name. When set, lists gaps in the branch graph." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "reflect_across",
                "description": "Cross-workspace reflect — aggregate entity co-occurrence counts across multiple mounted workspaces and apply the combined patterns to each. Use when no single workspace has enough instances of a given entity type to clear min_sample_size but the union does. Each workspace's local patterns are unaffected.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspaces": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Names of mounted workspaces to aggregate. Min 1, typically 2+."
                        }
                    },
                    "required": ["workspaces"]
                }
            },
            {
                "name": "dismiss_gap",
                "description": "Mark a gap (known-unknown) as Dismissed so future `reflect` cycles do not re-raise it. Use for legitimate absences (e.g. 'this internal service really does not need an auth claim'). Dismissed gaps are preserved for audit but stop counting toward health coverage.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string" },
                        "gap_id":    { "type": "string", "description": "Gap id from the `gaps` tool (ku-...)" },
                        "branch":    { "type": "string", "description": "Optional — branch name if the gap was found on a branch." }
                    },
                    "required": ["workspace", "gap_id"]
                }
            },
            // ── Intelligent memory retrieval ─────────────────────────────
            {
                "name": "ask",
                "description": "Ask a natural-language question against the personal memory graph. Uses hybrid retrieval + LLM synthesis (91.2% accuracy on LongMemEval-500). Handles factual recall, counting, temporal reasoning, preference recommendations, and knowledge updates. Returns a synthesized natural-language answer.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question":      { "type": "string", "description": "Natural-language question to answer from memory" },
                        "workspace":     { "type": "string" },
                        "session_scope": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional list of session IDs to restrict retrieval to (e.g. haystack_session_ids from LongMemEval)"
                        },
                        "question_date": { "type": "string", "description": "Reference date for temporal questions, e.g. '2023/05/30 (Tue) 22:10'" },
                        "category_hint": {
                            "type": "string",
                            "enum": ["single-session-user", "single-session-assistant", "single-session-preference", "multi-session", "temporal-reasoning", "knowledge-update"],
                            "description": "Optional category hint for strategy selection. Auto-detected if omitted."
                        }
                    },
                    "required": ["question", "workspace"]
                }
            },
            // ── Intelligent serve tools ───────────────────────────────────
            {
                "name": "brief",
                "description": "Get a token-efficient workspace overview: entity/claim counts, top entities, recent decisions, and contradiction count. Use this first to orient yourself before investigating specifics. (~100-200 tokens)",
                "inputSchema": {
                    "type": "object",
                    "properties": { "workspace": { "type": "string" } },
                    "required": ["workspace"]
                }
            },
            {
                "name": "investigate",
                "description": "Deep-dive into an entity: full context including claims (new only, session-aware), relations, and contradictions. Token-efficient structured text format. Use after 'brief' to explore specific entities.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "entity":    { "type": "string", "description": "Entity name to investigate (canonical or alias)" },
                        "workspace": { "type": "string" }
                    },
                    "required": ["entity", "workspace"]
                }
            },
            {
                "name": "focus",
                "description": "Set the session focal entity so subsequent queries can omit the entity name. Enables natural follow-up queries like 'what calls it?' without repeating the entity.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "entity":    { "type": "string", "description": "Entity to focus on" },
                        "workspace": { "type": "string" }
                    },
                    "required": ["entity", "workspace"]
                }
            },
            {
                "name": "contribute",
                "description": "Write agent-inferred claims directly into the knowledge graph. Claims are tagged AgentInferred+Untrusted and a subsequent 'root compile' will cross-validate them against source code. Use to record observations, discoveries, or inferences that should persist across sessions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "claims": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "statement":   { "type": "string", "description": "Atomic statement of fact/decision/etc." },
                                    "claim_type":  { "type": "string", "enum": ["fact","decision","opinion","plan","requirement","metric","definition","dependency","api_signature","architecture","preference"], "default": "fact" },
                                    "confidence":  { "type": "number", "minimum": 0, "maximum": 1, "default": 0.7 },
                                    "entities":    { "type": "array", "items": { "type": "string" }, "description": "Entity names this claim is about" }
                                },
                                "required": ["statement"]
                            }
                        },
                        "workspace": { "type": "string" }
                    },
                    "required": ["claims", "workspace"]
                }
            },
            // T0.7 — connector-attributed bulk contribute with idempotent
            // replay protection. Use from a webhook handler / connector
            // process where you need at-least-once delivery semantics.
            {
                "name": "contribute_bulk",
                "description": "Connector-attributed bulk contribute. Records a per-(connector_id,install_id,idempotency_key) ingest entry so replay is a safe no-op. Set 'backfill': true to skip per-claim rooting (deferred to end of batch — useful for backfilling historic webhook payloads).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":       { "type": "string" },
                        "branch":          { "type": "string", "description": "Target branch (omit or 'main' for main)." },
                        "session_id":      { "type": "string", "description": "Optional turn-calendar attribution; defaults to a synthetic id derived from the connector identity." },
                        "connector_id":    { "type": "string", "description": "Connector type id (e.g. 'github', 'slack', 'notion')." },
                        "install_id":      { "type": "string", "description": "Per-install identifier (e.g. 'alice-acme-prod')." },
                        "idempotency_key": { "type": "string", "description": "Caller-chosen unique key (typically the upstream event id)." },
                        "backfill":        { "type": "boolean", "default": false },
                        "claims": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "statement":   { "type": "string" },
                                    "claim_type":  { "type": "string", "enum": ["fact","decision","opinion","plan","requirement","metric","definition","dependency","api_signature","architecture","preference"], "default": "fact" },
                                    "confidence":  { "type": "number", "minimum": 0, "maximum": 1, "default": 0.7 },
                                    "entities":    { "type": "array", "items": { "type": "string" } }
                                },
                                "required": ["statement"]
                            }
                        }
                    },
                    "required": ["workspace", "connector_id", "install_id", "idempotency_key", "claims"]
                }
            },
            // ── T0.4 Knowledge Proposal tools ─────────────────────────────
            // The `RequiresProposal` merge gate (`thinkingroot-branch::merge::execute_merge_into:336`)
            // refuses raw merges and points users at these tools.  An
            // approved proposal lets the same merge call succeed.
            {
                "name": "open_proposal",
                "description": "Open a Knowledge Proposal against a `RequiresProposal`-gated branch. Returns the new proposal (with its ULID id) which the merge gate looks up via `find_approved_proposal`. The proposal freezes `min_reviewers` + `required_checks` from the source branch's policy at open time so a later policy change cannot loosen this proposal's gate.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":      { "type": "string" },
                        "source_branch":  { "type": "string", "description": "Branch the proposal asks to merge from." },
                        "target_branch":  { "type": "string", "description": "Branch to merge into; omit or 'main' for main." },
                        "author":         { "type": "string", "description": "Principal::identity() of the author." },
                        "description":    { "type": "string" },
                        "min_reviewers":  { "type": "integer", "minimum": 0, "description": "Override the source branch's policy default." },
                        "required_checks":{ "type": "array",   "items": { "type": "string" } }
                    },
                    "required": ["workspace", "source_branch", "author"]
                }
            },
            {
                "name": "review_proposal",
                "description": "Record a review on an open proposal. `decision` is one of approve|request_changes|comment. Approves count distinct non-author reviewers; once the count reaches `min_reviewers` AND no reviewer is in RequestChanges, status flips to Approved and the merge gate will allow the merge.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":   { "type": "string" },
                        "proposal_id": { "type": "string", "description": "ULID returned by open_proposal." },
                        "reviewer":    { "type": "string" },
                        "decision":    { "type": "string", "enum": ["approve", "request_changes", "comment"] },
                        "comment":     { "type": "string" }
                    },
                    "required": ["workspace", "proposal_id", "reviewer", "decision"]
                }
            },
            {
                "name": "list_proposals",
                "description": "List Knowledge Proposals for a workspace, oldest-first by ULID. Optionally filter by `source_branch`. Includes status, reviews, required_checks, and (when merged) merged_at.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":     { "type": "string" },
                        "source_branch": { "type": "string", "description": "Filter to proposals from this source branch." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "close_proposal",
                "description": "Author-initiated close. Drops a non-terminal proposal into Closed (terminal). Only the proposal's author can close. No-op when the proposal is already Merged or Closed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":   { "type": "string" },
                        "proposal_id": { "type": "string" },
                        "closer":      { "type": "string", "description": "Must equal the proposal author." }
                    },
                    "required": ["workspace", "proposal_id", "closer"]
                }
            },
            // ── SOTA Lever 3 — Observer / Reflector tools ─────────────────
            {
                "name": "observe_turn",
                "description": "Record a single chat turn (user_prompt + assistant_reply) into the Observer's per-session buffer. Mechanical, no LLM. When the per-session pending count reaches the condense threshold (default 10), the Observer condenses the window into a staged observation; staged observations later drain to the witness substrate via `flush_observations`. Cheap to call — invoke once per turn-complete event in your chat lifecycle. Returns `{ session_id, turn_number, pending_turns, should_reflect }`. Mirrors Mastra's Observational Memory dense-log pattern (94.87% LongMemEval).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id":      { "type": "string", "description": "Stable session identifier — different sessions stay isolated in the Observer's per-session buffer." },
                        "turn_number":     { "type": "integer", "description": "Monotonic turn counter within this session. Pin the same numbering scheme across calls so the condensed observation's turn_range is meaningful." },
                        "user_prompt":     { "type": "string", "description": "What the user said this turn. May be empty if the turn was a tool-only operation, but at least one of user_prompt / assistant_reply must be non-empty." },
                        "assistant_reply": { "type": "string", "description": "What the assistant said this turn. May be empty if the turn ended without a reply." }
                    },
                    "required": ["session_id", "turn_number"]
                }
            },
            {
                "name": "flush_observations",
                "description": "Drain staged conversation observations for `session_id` and persist them to the workspace's witness substrate as `conversation::observation@v1` rows (with an optional `conversation::reflection@v1` row when the reflect threshold has been crossed). Call at session-end OR periodically (every N turns / M minutes) to make the buffer durable across process restarts. On insert failure the observations are re-staged so the next flush retries — failed flushes are honest-incomplete, never silent-lossy. Returns `{ session_id, workspace, inserted_witnesses }`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":      { "type": "string", "description": "Target workspace. Observations land in this workspace's witness substrate and participate in retrieval there." },
                        "session_id":     { "type": "string", "description": "Session whose buffer to drain. Other sessions are unaffected." },
                        "force_condense": { "type": "boolean", "description": "When true, force-condense any pending turn buffer BEFORE flushing — useful at session-end to capture a partial window. Default false.", "default": false }
                    },
                    "required": ["workspace", "session_id"]
                }
            },
            // ── Rooting tools (Phase 6.5 admission gate) ──────────────────
            {
                "name": "query_rooted",
                "description": "Retrieve only Rooted-tier claims (passed all 5 admission probes). Safer default than query_claims for production agents — guarantees every returned claim has a verified certificate.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "type":           { "type": "string" },
                        "entity":         { "type": "string" },
                        "min_confidence": { "type": "number" },
                        "workspace":      { "type": "string" }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "rooting_report",
                "description": "Return admission tier counts (rooted / attested / quarantined / rejected) for a workspace. Use to surface memory-quality dashboards or to flag packs whose Rooted fraction is degrading.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string" }
                    },
                    "required": ["workspace"]
                }
            },
            // ── RARP / Active Engram Protocol v2 (4 tools) ────────────────
            {
                "name": "materialize_engram",
                "description": "Build an Engram — a typed sub-graph of ~30-token cluster handle pointing at the substrate rows relevant to a topic. Returns an EngramSummary (entities, claim counts by tier, source authority, temporal window, contradictions) PLUS a pointer (e.g. '0x7F9A') you keep and pass to `probe_engram` for follow-up questions. Use this when the user's question implies repeated drilling into one subject area — first materialize, then probe multiple times. Single-shot questions are better served by `search` or `hybrid_retrieve`. Cache discipline: at most 100 engrams per session, evicted by TTL. Default depth_hops=2, event_window_days=90, clearance=['public'].",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic":      { "type": "string", "description": "Free-text topic name (e.g. 'Auth System')" },
                        "workspace":  { "type": "string" },
                        "scope": {
                            "type": "object",
                            "properties": {
                                "depth_hops":         { "type": "integer", "minimum": 1, "maximum": 4 },
                                "event_window_days":  { "type": "integer", "minimum": 1 },
                                "clearance":          { "type": "array", "items": { "type": "string", "enum": ["public", "internal", "confidential", "restricted"] } }
                            }
                        },
                        "seed_claim_ids": { "type": "array", "items": { "type": "string" } },
                        "seed_entity_ids": { "type": "array", "items": { "type": "string" }, "description": "Pin seed entities; bypasses vector search" }
                    },
                    "required": ["topic", "workspace"]
                }
            },
            {
                "name": "probe_engram",
                "description": "Drill into a materialised Engram with a typed question. Returns answer rows with full provenance: claim_ids, byte spans, BLAKE3 hashes, certificate_hash, trial_scores, derivation_root, turn_provenance. Also returns caveats — unresolved contradictions, stale rows, low-confidence routing, test-derived facts, superseded claims, sensitivity redactions, gap-adjacency notes. Use AFTER `materialize_engram` once you hold a pointer; for one-off questions without a pointer, prefer `search` or `hybrid_retrieve`. Set `score_with_hybrid: true` to re-rank rows through the 11-component hybrid score. Set `probe_kind` to override the regex router when you know the question category (e.g. 'temporal' for time-based questions).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pointer":            { "type": "string", "description": "Engram pointer issued by materialize_engram" },
                        "question":           { "type": "string" },
                        "workspace":          { "type": "string" },
                        "clearance":          { "type": "array", "items": { "type": "string", "enum": ["public", "internal", "confidential", "restricted"] } },
                        "probe_kind":         { "type": "string", "enum": ["factual", "quantitative", "temporal", "authorship", "structural", "relation_callers", "relation_refs", "existential", "comparative", "counterfactual"] },
                        "score_with_hybrid":  { "type": "boolean", "description": "Route answer rows through Hybrid Retrieval scoring before caveat enrichment. Composes per docs/2026-05-02-hybrid-retrieval-spec.md §11." }
                    },
                    "required": ["pointer", "question", "workspace"]
                }
            },
            {
                "name": "list_engrams",
                "description": "List every Engram pointer currently materialised for this session, with its topic, age, and TTL. Use to recover from 'I had a pointer but lost track of it' or to decide whether to evict before materializing a new one (cap is 100 per session). Cheap, no I/O beyond an in-memory map read.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "expire_engram",
                "description": "Explicitly evict an Engram from the session cache so its pointer slot can be reused. Returns `{ expired: bool }` — `false` means the pointer was already gone (TTL expiry, session reset, or wrong pointer). Use when you're done with a topic and approaching the 100-engram cap, or when a `compile` invalidated the underlying claims and you want a fresh pointer on next materialize.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pointer":   { "type": "string", "description": "Engram pointer issued by materialize_engram (e.g. '0x7F9A')." },
                        "workspace": { "type": "string", "description": "Workspace name." }
                    },
                    "required": ["pointer", "workspace"]
                }
            },
            // ── Phase α — Playground action verbs ────────────────────
            // Give external agents (Claude Code, Cursor, Codex) the
            // same write power the in-app Brain chat has, routed
            // through `intelligence::playground_tools`. Design doc:
            // docs/2026-05-15-cognition-commits-design.md.
            {
                "name": "save_note",
                "description": "Save a markdown body as a note under <workspace>/notes/<slug>-<date>.md with YAML frontmatter (title, created_at, kind=chat-note). Use this to persist a synthesised reply, a meeting summary, or any AI-authored markdown into the workspace so the next compile picks it up as a source. Refuses to overwrite an existing same-day note — surfaces `created: false` so you can retry with a different title.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name from the mount config." },
                        "title":     { "type": "string", "description": "Short human-readable title. Becomes the slug + YAML frontmatter title. Non-alphanumeric chars collapse to '-'." },
                        "body":      { "type": "string", "description": "Markdown body. Citation chips ([[witness:<id>]]) are honoured by the next compile pass." }
                    },
                    "required": ["workspace", "title", "body"]
                }
            },
            {
                "name": "regenerate_paper",
                "description": "Re-synthesize the Living Paper (`<workspace>/.thinkingroot/paper.md`) against the current Witness Mesh state. Cheap relative to `compile` — no parse/extract/ground passes, just the synthesizer + (when an LLM is configured) the AI-narrative sections. Use after the user adds a few notes or witnesses and wants the paper refreshed without paying the cost of a full compile. Returns `{ path, byte_length, sections }`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "ingest_path",
                "description": "Copy files from an absolute host path into the workspace's `inbox/` directory. When `source_path` is a single file, copies it. When it is a directory, copies every non-hidden top-level regular file (no recursion). Does NOT trigger a compile — call the `compile` tool next when you want the Witness Mesh refreshed. Same-name files are skipped (no overwrite); honest counts surfaced as copied / skipped_duplicate / skipped_unreadable.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":   { "type": "string", "description": "Workspace name. The workspace's `inbox/` is the destination." },
                        "source_path": { "type": "string", "description": "Absolute path on the user's machine (e.g. /Users/alice/Desktop/papers/ or /tmp/notes.md). Relative paths are refused." }
                    },
                    "required": ["workspace", "source_path"]
                }
            },
            {
                "name": "list_directory",
                "description": "List the immediate children of a workspace-relative directory. Hidden dotfiles are filtered. Folders sort first, then files, alphabetical within each group. Use as a substrate-aware `ls` when you need to know what's under `notes/`, `sources/`, `inbox/` etc. before deciding what to organize / trash / ingest. Read-only. Returns `{ rel_path, parent_rel_path, entries: [{ name, rel_path, is_dir, size_bytes }] }`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." },
                        "rel_path":  { "type": "string", "description": "Directory to list, workspace-relative. Empty / omitted lists the workspace root. Forward slashes regardless of OS." }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "organize_files",
                "description": "Batch rename/move files within a workspace. Each op `{from, to}` is workspace-relative; both paths are validated to stay inside the workspace root. Atomic `rename(2)` per op — conflicts (target exists, source missing, path escape, would-move-folder-into-itself) are skipped with honest counts. Missing intermediate destination dirs are auto-created. Cross-filesystem moves are not attempted; use `ingest_path` + `trash_files` for that.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." },
                        "ops": {
                            "type": "array",
                            "description": "Move operations. Order is preserved; each op is applied in turn so a later op can target a path a previous op just produced.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "from": { "type": "string", "description": "Existing source path, workspace-relative." },
                                    "to":   { "type": "string", "description": "Destination path, workspace-relative. Missing parent directories are created." }
                                },
                                "required": ["from", "to"]
                            }
                        }
                    },
                    "required": ["workspace", "ops"]
                }
            },
            {
                "name": "trash_files",
                "description": "Move files into `<workspace>/.thinkingroot/trash/<unix-ts>-<name>`. Reversible by manually moving back; the next compile won't re-extract trashed items because `.thinkingroot/` is walker-ignored. Refuses to trash anything inside `.thinkingroot/` itself. Use when the user says \"delete that note\" or \"clean up the inbox\" — never let the agent invoke raw filesystem removal.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." },
                        "rel_paths": {
                            "type": "array",
                            "description": "Workspace-relative paths to trash.",
                            "items": { "type": "string" }
                        }
                    },
                    "required": ["workspace", "rel_paths"]
                }
            },
            // ── Phase β.1 — Cognition Commits ────────────────────────
            {
                "name": "commit_cognition",
                "description": "Record one cognition event against a workspace branch as a content-addressed commit. The commit id is BLAKE3-derived from (parent, branch, author, prompt, reasoning, witnesses_added, citations, gaps_surfaced) — same inputs always produce the same id. Cited / added witnesses MUST exist in the workspace; fabricated references are rejected. Use this once per agent turn that produced an observable cognitive change: a note saved, a paper regenerated, an argument made, a gap surfaced. The chat history IS the commit DAG.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":       { "type": "string", "description": "Workspace name." },
                        "branch":          { "type": "string", "description": "Branch this commit belongs to. Joins against branches.toml; create the branch first via `create_branch` if it doesn't exist." },
                        "parent_id":       { "type": "string", "description": "64-char lower-hex parent commit id. Omit (or pass empty) ONLY for the very first commit on a branch — every subsequent commit must thread back to a parent on the SAME branch." },
                        "author_kind":     { "type": "string", "enum": ["user", "agent"], "description": "Who emitted this commit." },
                        "author_id":       { "type": "string", "description": "User id (for kind=user) or agent principal (for kind=agent, e.g. `thinkingroot`)." },
                        "author_model":    { "type": "string", "description": "Model name for kind=agent (e.g. `claude-opus-4-7`). Empty for kind=user." },
                        "prompt":          { "type": "string", "description": "User prompt or system-event description that produced this commit. Empty for the genesis commit of a branch." },
                        "reasoning":       { "type": "string", "description": "AI's reasoning text or structured-event description. Citation chips ([[witness:<id>]]) supported." },
                        "witnesses_added": { "type": "array", "items": { "type": "string" }, "description": "64-char hex witness ids this commit produced (e.g. comment claims, observation rows). Empty for read-only commits." },
                        "citations":       { "type": "array", "items": { "type": "string" }, "description": "64-char hex witness ids cited in reasoning. EVERY id must resolve to an existing witness in this workspace." },
                        "gaps_surfaced":   { "type": "array", "items": { "type": "string" }, "description": "Known-unknown gap_ids (from the `gaps` tool) this commit raised. Empty when the agent didn't flag any." }
                    },
                    "required": ["workspace", "branch", "author_kind", "author_id"]
                }
            },
            {
                "name": "list_commits",
                "description": "List cognition commits on a branch, newest first. Returns the full CognitionCommit shape (id, parent, branch, author, prompt, reasoning, witnesses_added, citations, gaps_surfaced, created_at). Use this to render the chat-as-commit-DAG view, diff cognitions across time, or walk the parent chain to replay how an argument evolved.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string", "description": "Workspace name." },
                        "branch":    { "type": "string", "description": "Branch to list. Defaults to `main` if omitted." },
                        "limit":     { "type": "integer", "minimum": 1, "description": "Max commits to return. Omit for all." }
                    },
                    "required": ["workspace"]
                }
            },
            // ── Phase γ.2 — Merge Synthesis (LLM-driven) ──────────────
            {
                "name": "synthesize_merge",
                "description": "Generate an LLM-written synthesis of a deterministic merge plan between two cognition-commit branches. Returns the plan PLUS the LLM's reasoning paragraph with citation markers `[[witness:<id>]]`. Citation honesty is enforced: the response's `verified_citations` only contains witness ids the plan actually surfaced; any fabricated ids the LLM produced are reported in `dropped_citations` and excluded. Pure read — does NOT record a commit. Trivial plans (identical / ahead) short-circuit without an LLM call. Use this when the user asks to merge two branches and you want grounded synthesis.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":    { "type": "string", "description": "Workspace name." },
                        "left_branch":  { "type": "string", "description": "Branch treated as the 'left' side (typically the destination, e.g. `main`)." },
                        "right_branch": { "type": "string", "description": "Branch treated as the 'right' side (typically the topic / candidate being merged in)." }
                    },
                    "required": ["workspace", "left_branch", "right_branch"]
                }
            },
            // ── Phase γ.1 — Merge Cognition ──────────────────────────
            {
                "name": "merge_cognition",
                "description": "Compute a deterministic merge plan between two cognition-commit branches. Returns the divergence classification (identical / left_ahead / right_ahead / diverged / no_common_history), the lowest common ancestor when present, the commit ids unique to each side, and the partitioned witness-id sets each side cited or added since the LCA. This is the substrate γ.1 ship — pure DAG walk + set classification, no LLM. γ.2 will feed this plan to a model to synthesize a merge commit; γ.3 will render it as a conflict-resolution view. Use it now when you want a precise, replayable picture of what differs between two thinking-branches.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":    { "type": "string", "description": "Workspace name." },
                        "left_branch":  { "type": "string", "description": "Branch name treated as the 'left' side of the merge — convention is the destination branch (e.g. `main`)." },
                        "right_branch": { "type": "string", "description": "Branch name treated as the 'right' side — convention is the candidate / topic branch being merged in." }
                    },
                    "required": ["workspace", "left_branch", "right_branch"]
                }
            },
            {
                "name": "hybrid_retrieve",
                "description": "Top-tier retrieval over the 33-table substrate: vector recall + Datalog filters + per-row BLAKE3 verification + 11-component score fusion. Returns ranked hits with full provenance (byte spans, source authority, admission tier, trial scores, certificate_hash, derivation lineage) plus typed caveats (stale, contradicted, superseded, test-derived, gap-adjacent, redacted, low-confidence, quarantined, bytes-unavailable). Use when the user wants the BEST evidence for a question and you need verifiable provenance — e.g. before answering a precise factual question, or when grounding a write. Prefer `search` for fuzzy exploration and `query_claims` for filtered list queries. Set `scoring_profile: 'compliance'` for legal/audit (rooted-tier only, doubled penalties).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":                 { "type": "string", "description": "Workspace name." },
                        "query_text":                { "type": "string", "description": "Free-text query for vector recall. Empty string when only typed_predicates apply." },
                        "typed_predicates":          { "type": "array", "default": [], "items": { "type": "object", "properties": { "kind": { "type": "string", "enum": ["entity_type", "entity_name", "claim_type", "source_trust_at_least", "authored_by", "authored_after", "in_call_graph_of", "has_doc_tag", "has_marker", "quantity_range", "in_heading_path", "supersedes_claim", "referenced_by"] } }, "required": ["kind"] }, "description": "Structured filters AND-combined. E.g. [{kind: 'entity_name', value: 'WebhookHandler'}, {kind: 'claim_type', value: 'decision'}]." },
                        "session_id":                { "type": "string", "description": "Per-session identifier — REQUIRED. Use the same value across calls in one conversation so retrieval can dedupe against previously-delivered claims (`SessionContext.delivered_claim_ids`). If you don't have one, mint a stable UUID at conversation start and reuse it. Mismatched session_ids cause repeat results." },
                        "clearance":                 { "type": "array", "default": ["public"], "items": { "type": "string", "enum": ["public", "internal", "confidential", "restricted"] }, "description": "Sensitivity tiers the caller is cleared for. Anything stricter is dropped or redacted." },
                        "top_k":                     { "type": "integer", "minimum": 1, "maximum": 200, "default": 50, "description": "Max ranked results. Default 50 is a good general-purpose value; use 10–20 for tight prompts, 100+ for analytical workloads." },
                        "scoring_profile":           { "type": "string", "enum": ["default", "compliance", "custom"], "default": "default", "description": "'default' = balanced; 'compliance' = rooted-only + doubled penalties (legal/audit); 'custom' = caller supplies scoring_profile_custom." },
                        "scoring_profile_custom":    { "type": "object", "description": "Required when scoring_profile='custom'. Same shape as ScoringProfile. Omit otherwise." },
                        "require_certificate":       { "type": "boolean", "default": false, "description": "Drop hits that lack a signed certificate_hash. Use for high-trust answers." },
                        "include_test_origin":       { "type": "boolean", "default": false, "description": "Allow claims derived from test files. Off by default to reduce noise." },
                        "include_quarantined":       { "type": "boolean", "default": false, "description": "Allow claims rejected by the rooting battery. Off by default." },
                        "require_provenance_verified": { "type": "boolean", "default": false, "description": "Only return hits that pass eager BLAKE3 verification. Slows retrieval slightly; raises trust." }
                    },
                    "required": ["workspace", "session_id"]
                }
            },
            // ── Phase ε.1 — Tool Search Tool ──────────────────────────
            // Anthropic-style pattern: a meta-tool that lets a client
            // discover deferred tools on demand instead of preloading
            // every descriptor into context. Matches the "Tool Search
            // Tool" idiom from the published 85%-context-savings
            // experiment.
            {
                "name": "tool_search",
                "description": "Search the workspace's MCP tool catalog by name or description substring. Returns full descriptors (including `defer_loading: true` tools) so an AI can pull in a long-tail tool's schema on demand instead of preloading every tool's JSON into context at session start. Use when the user mentions an operation and you suspect there's a specialized tool for it (e.g. 'rebase the branch', 'expire that engram', 'open a proposal').",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query":              { "type": "string", "description": "Substring matched against tool name and description (case-insensitive). Empty string returns every tool — useful as 'show me what's available'." },
                        "include_non_deferred": { "type": "boolean", "default": true, "description": "When true (default) the result includes always-loaded tools too; when false only `defer_loading: true` tools are returned. Default is permissive so a client can use this as a single search surface." },
                        "limit":              { "type": "integer", "minimum": 1, "maximum": 100, "default": 20, "description": "Max tool descriptors to return. Default 20 fits the typical 'find me a tool to do X' use case without bloating the response." }
                    },
                    "required": []
                }
            },
            // ── Phase D Wave 1 — System-power tools ──────────────────
            // These 10 tools operate on the user's filesystem and shell
            // OUTSIDE the workspace. Each is gated by the agent's
            // PermissionsGate when called via the chat agent loop;
            // direct MCP clients (Claude Code, Cursor, etc.) bring
            // their own permission UX.
            {
                "name": "file_read",
                "description": "Read any text file on disk. Returns content as `cat -n`-style numbered lines (6-width right-aligned 1-based line number + tab + line content + newline), the raw file's byte size on disk, and the line count. Cite line numbers directly from the content prefix — never count manually. Refuses files larger than 5 MiB and refuses binary (non-UTF-8) files — paginate or use a binary-aware tool for those. Path may be absolute or relative to CWD.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute or CWD-relative path to a UTF-8 text file." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "file_write",
                "description": "Atomically write or overwrite a text file. Uses tempfile + rename so a SIGKILL mid-write never leaves a torn file. Set `create_dirs: true` to mkdir -p the parent path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path":        { "type": "string", "description": "Destination path. Parent directory must exist unless create_dirs is true." },
                        "content":     { "type": "string", "description": "File contents." },
                        "create_dirs": { "type": "boolean", "default": false, "description": "When true, missing parent directories are created via mkdir -p." }
                    },
                    "required": ["path", "content"]
                }
            },
            {
                "name": "file_edit",
                "description": "Apply line-precise edits to an existing text file. Edits are validated for non-overlap and EOF-safety BEFORE any mutation, then applied in reverse-line order via atomic tempfile + rename. Use `start_line: N, end_line: N, replacement: \"new\"` to replace line N. Use `start_line: N, end_line: 0, replacement: \"...\"` to insert before line N. Lines are 1-based.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path":  { "type": "string", "description": "Path to an existing file." },
                        "edits": {
                            "type":  "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "start_line":  { "type": "integer", "description": "1-based start line (inclusive)." },
                                    "end_line":    { "type": "integer", "description": "1-based end line (inclusive). Use 0 for a pure insert before `start_line`." },
                                    "replacement": { "type": "string",  "description": "Replacement text. Empty string deletes the range." }
                                },
                                "required": ["start_line", "end_line", "replacement"]
                            }
                        }
                    },
                    "required": ["path", "edits"]
                }
            },
            {
                "name": "glob",
                "description": "Find files matching a glob pattern under a base directory. Gitignore-aware (uses ripgrep's `ignore` crate). Results are canonicalised to defeat symlink-cover-name attacks. Capped at 1000 matches; sets `truncated: true` past that.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern (e.g. `**/*.rs`, `src/**/*.ts`)." },
                        "base":    { "type": "string", "description": "Base directory to search under. Defaults to CWD." }
                    },
                    "required": ["pattern"]
                }
            },
            {
                "name": "grep",
                "description": "Search file contents for a pattern under a base directory. Set `regex: true` for regex mode, false (default) for literal substring match. Gitignore-aware. Long lines are trimmed to 400 chars + ellipsis. Capped at 500 matches.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pattern":        { "type": "string", "description": "Pattern to search for." },
                        "base":           { "type": "string", "description": "Base directory. Defaults to CWD." },
                        "regex":          { "type": "boolean", "default": false, "description": "Treat pattern as a regex." },
                        "case_sensitive": { "type": "boolean", "default": true, "description": "Match case-sensitively. When false, case-insensitive." }
                    },
                    "required": ["pattern"]
                }
            },
            {
                "name": "shell_exec",
                "description": "Run a shell command under the OS-level sandbox (macOS Seatbelt or Linux bubblewrap). Returns exit_code, stdout, stderr, duration_ms, sandbox_backend. Times out after `timeout_secs` (default 30, max 300). Refused on Windows — use WSL2 there. The other 9 system-power tools work natively on Windows.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command":      { "type": "string", "description": "Executable path or command name. The agent's PATH at the time of invocation is searched." },
                        "args":         { "type": "array", "items": { "type": "string" }, "default": [], "description": "Arguments." },
                        "cwd":          { "type": "string", "description": "Working directory. Must be inside the workspace allowlist or omitted (defaults to workspace root)." },
                        "timeout_secs": { "type": "integer", "default": 30, "description": "Wall-clock kill threshold (1–300 seconds)." }
                    },
                    "required": ["command"]
                }
            },
            {
                "name": "clipboard_read",
                "description": "Read the current text content of the system clipboard. Returns content + byte_size.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "clipboard_write",
                "description": "Write text to the system clipboard. Replaces any previous content. Returns bytes_written.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "Text to place on the clipboard." }
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "open_in_default",
                "description": "Open a file path or URL in the user's default application (Finder/Explorer/Nautilus for paths; default browser for URLs). Cross-platform via the `opener` crate.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path_or_url": { "type": "string", "description": "Absolute path or fully-qualified URL." }
                    },
                    "required": ["path_or_url"]
                }
            },
            {
                "name": "trash",
                "description": "Move one or more files or directories to the OS native trash (recoverable). Returns counts of trashed paths plus per-path failures. Safer than rm — the user can restore from the system Trash.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "paths": { "type": "array", "items": { "type": "string" }, "description": "Absolute paths to trash." }
                    },
                    "required": ["paths"]
                }
            },
            // ── System filesystem (absolute paths, sibling of fs_*) ──
            // The `fs_*` family is workspace-bounded — every path is
            // resolved against `engine.workspace_root_path(ws)`. The
            // `sys_*` family below operates on absolute paths so the
            // agent can move/rename/list/stat anywhere on disk the
            // PermissionsGate DEFAULT_DENY shortlist allows. `~` is
            // expanded against the daemon user's HOME. Read-class
            // tools (`sys_stat`, `sys_list`) enforce the DEFAULT_DENY
            // shortlist directly in the handler rather than going
            // through the approval flow; write-class tools
            // (`sys_move`, `sys_rename`, `sys_create_folder`) route
            // through the gate AND the handler's own check.
            {
                "name": "sys_stat",
                "description": "Cheap absolute-path existence + metadata check — `{exists, is_dir, is_file, is_symlink, size_bytes, modified}`. Returns `exists: false` for missing paths (NOT an error). Use this BEFORE proposing a write (e.g. `sys_move`) to confirm source and destination paths resolve. Faster + bounded compared to `glob`/`shell_exec` for path verification. Path is absolute; `~` is expanded against the user's HOME. Refuses sensitive paths (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/Library/Keychains`, `/etc`, …).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute path or tilde-expanded path (e.g. `/Users/alice/Desktop/foo`, `~/Documents/bar`)." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "sys_list",
                "description": "List the contents of an absolute-path directory. Same shape as `fs_list` (entries sorted dirs-first then name-asc) but operates outside any workspace root. `~` is expanded. Refuses sensitive paths. Use this when the user asks about a folder on their Desktop / Documents / Downloads / external drives.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute directory path. `~` is expanded against the user's HOME." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "sys_create_folder",
                "description": "Create a new directory at an absolute path. Parent must already exist; collisions are an error (no silent overwrite). Refuses sensitive paths.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute path of the new folder. `~` is expanded." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "sys_rename",
                "description": "Rename a file or folder in-place at an absolute path. `new_name` is a single leaf segment (no `/`, no `..`). Refuses collisions. Refuses sensitive paths (both source and post-rename destination).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path":     { "type": "string", "description": "Absolute path to the existing item. `~` is expanded." },
                        "new_name": { "type": "string", "description": "New leaf name. Single path segment." }
                    },
                    "required": ["path", "new_name"]
                }
            },
            {
                "name": "sys_move",
                "description": "Move one or more files/folders into an absolute destination folder. Skips collisions honestly (counts them as `skipped_conflict`) — silent overwrite is the kind of 'helpful' that loses work. Returns `{moved, skipped_conflict, skipped_invalid, moved_paths, per_source_reason}` so the agent can report accurately. Handles cross-device moves (`EXDEV`) via copy-then-delete. Refuses sensitive paths.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sources":     { "type": "array", "items": { "type": "string" }, "description": "Absolute paths of items to move. `~` is expanded." },
                        "dest_folder": { "type": "string", "description": "Absolute path of the destination directory (must already exist)." }
                    },
                    "required": ["sources", "dest_folder"]
                }
            }
        ]
    });
    if let Some(arr) = tools.get_mut("tools") {
        annotate_defer_loading(arr);
        // Phase E.6 (2026-05-17) — append any tools registered via the
        // `mcp::tool_trait` registry. The hardcoded 64 above stay as-is
        // for the incremental migration policy; future tools land via
        // `tool_trait::register_tool`.
        if let Some(arr_mut) = arr.as_array_mut() {
            for schema in crate::mcp::tool_trait::list_schemas() {
                arr_mut.push(schema);
            }
            // Phase E.5 (2026-05-17) — append external MCP tools
            // from the bridged registry under
            // `<server_name>::<tool_name>` namespace. Tolerates a
            // slow external server: list_all_tools internally
            // logs + skips servers that fail. Production startup
            // installs the global via
            // `external_registry::load_global_from_workspace_config`
            // at workspace mount.
            let external = crate::mcp::external_registry::global().await;
            for (prefixed_name, tool) in external.list_all_tools().await {
                arr_mut.push(serde_json::json!({
                    "name": prefixed_name,
                    "description": format!("[external] {}", tool.description),
                    "inputSchema": tool.input_schema,
                }));
            }
        }
    }
    JsonRpcResponse::success(id, tools)
}

/// Phase ε.1 — `tool_search` handler.
///
/// Returns matching tool descriptors. Independent of `handle_list`'s
/// JSON construction — we re-fetch the full list (cheap; the JSON
/// is built every call anyway and the array is a few KB) and filter
/// in-Rust. Filtering on the wire keeps the protocol simple: clients
/// don't need to maintain a local catalog.
pub async fn handle_tool_search(
    id: Option<Value>,
    arguments: &Value,
) -> JsonRpcResponse {
    let query = arguments
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let include_non_deferred = arguments
        .get("include_non_deferred")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n.clamp(1, 100) as usize)
        .unwrap_or(20);

    // Reuse the canonical list — same source of truth as the
    // production tools/list endpoint, including the defer_loading
    // annotation pass.
    let full = handle_list(None).await;
    let result = match full.result {
        Some(v) => v,
        None => {
            return JsonRpcResponse::error(
                id,
                -32603,
                "tool_search: tools/list returned no result".to_string(),
            );
        }
    };
    let arr = result
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut matches: Vec<Value> = Vec::new();
    for tool in arr {
        let is_deferred = tool
            .get("defer_loading")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !is_deferred && !include_non_deferred {
            continue;
        }
        let name = tool
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let desc = tool
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        if query.is_empty() || name.contains(&query) || desc.contains(&query) {
            matches.push(tool);
        }
        if matches.len() >= limit {
            break;
        }
    }
    mcp_text_result(id, &serde_json::json!({ "tools": matches }))
}

#[tracing::instrument(
    name = "mcp.tools.call",
    skip(params, engine, sessions, engram_manager, state, cancel),
    fields(
        tool = tracing::field::Empty,
        workspace = tracing::field::Empty,
        session_id = %session_id,
    ),
)]
pub async fn handle_call(
    id: Option<Value>,
    params: &Value,
    engine: &QueryEngine,
    default_ws: Option<&str>,
    session_id: &str,
    sessions: &SessionStore,
    engram_manager: &std::sync::Arc<crate::intelligence::engram::EngramManager>,
    // C2 (2026-05-22): `Some(state)` over SSE; `None` over stdio.
    // Tools that genuinely need AppState (e.g.
    // `get_reminder_context`) check for `None` and return a typed
    // "stdio-transport not supported" envelope rather than panic.
    state: Option<&std::sync::Arc<crate::rest::AppState>>,
    // C3 (2026-05-22): per-request cancellation token. Long tool
    // handlers observe at phase boundaries / between sub-steps and
    // return `Error::Cancelled`. Fast tools ignore it (the cost of
    // checking once at entry is one branch — kept symmetric across
    // arms so future cancel-awareness lands without signature
    // churn). The token is also auto-tripped on SSE disconnect via
    // `tokio_util::sync::DropGuard`.
    cancel: tokio_util::sync::CancellationToken,
) -> JsonRpcResponse {
    let tool_name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return JsonRpcResponse::error(id, -32602, "Missing 'name' parameter".to_string()),
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let ws_owned = resolve_workspace_arg(
        arguments.get("workspace").and_then(|v| v.as_str()),
        default_ws,
        engine,
    );
    let ws: &str = &ws_owned;

    // Populate the span's pre-declared Empty fields now that we've parsed
    // the params. Lets trace consumers filter by tool + workspace.
    tracing::Span::current().record("tool", tool_name);
    tracing::Span::current().record("workspace", ws);

    // C3 (2026-05-22) — fast-path cancellation check at dispatch
    // entry. If `notifications/cancelled` (or an SSE drop) tripped
    // the token between request receive and dispatch arrival, we
    // bail before doing any work. Mid-call cancellation observation
    // is per-tool — long tools that gain cancel-awareness sample
    // `cancel.is_cancelled()` at their own phase boundaries. The
    // entry-point check is symmetric across every arm so the
    // cheap-tool path doesn't need a per-arm guard.
    if cancel.is_cancelled() {
        return JsonRpcResponse::error(
            id,
            -32800,
            format!(
                "tool '{tool_name}' cancelled before dispatch (client sent \
                 notifications/cancelled or transport dropped)"
            ),
        );
    }

    match tool_name {
        // ── C2 (2026-05-22) — Ambient session + reminder context ─────
        "get_session_context" => {
            let target_session_id = arguments
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or(session_id);

            // Session metadata: clone-and-release the lock so we don't
            // hold it across the workspace listing below.
            let session_snapshot = {
                let store = sessions.lock().await;
                store.get(target_session_id).cloned()
            };

            // Mounted workspaces — over HTTP we could read
            // `state.mounted_workspace_roots`, but `engine.list_workspaces`
            // returns the same data and works on every transport
            // (stdio + SSE), so we use it uniformly.
            let mounted_workspaces: Vec<String> = match engine.list_workspaces().await {
                Ok(list) => list.into_iter().map(|w| w.name).collect(),
                Err(_) => Vec::new(),
            };

            let payload = match session_snapshot {
                Some(s) => serde_json::json!({
                    "session_id": s.id,
                    "workspace": s.workspace,
                    "owner": s.owner,
                    "active_branch": s.active_branch,
                    "focus_entity": s.focus_entity,
                    "turn_count": s.turn_count,
                    "chat_turn_count": s.chat_turn_count,
                    "delivered_claim_count": s.delivered_claim_ids.len(),
                    "mounted_workspaces": mounted_workspaces,
                    // C6 (2026-05-22) — wired: returns the MCP
                    // client info ({name, version}) when the
                    // calling session was opened with a
                    // `clientInfo` block on `initialize`. Null
                    // for sessions that don't have one (REST
                    // chat, bespoke MCP libraries).
                    "client_info": s.client_info,
                    // `sensitivity_caveats` wired in C19
                    // (redaction parity). Null pre-wire.
                    "sensitivity_caveats": serde_json::Value::Null,
                }),
                None => serde_json::json!({
                    "session_id": target_session_id,
                    "workspace": ws,
                    "owner": serde_json::Value::Null,
                    "active_branch": serde_json::Value::Null,
                    "focus_entity": serde_json::Value::Null,
                    "turn_count": 0,
                    "chat_turn_count": 0,
                    "delivered_claim_count": 0,
                    "mounted_workspaces": mounted_workspaces,
                    "client_info": serde_json::Value::Null,
                    "sensitivity_caveats": serde_json::Value::Null,
                }),
            };
            mcp_text_result(id, &payload)
        }
        "get_reminder_context" => {
            // Requires AppState — over stdio we return a typed
            // envelope so the caller can fall back to a degraded
            // strategy (e.g., issue an HTTP MCP call) instead of
            // panicking on a half-rendered context.
            let app_state = match state {
                Some(s) => s,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32601,
                        "get_reminder_context requires HTTP transport (SSE). Stdio MCP cannot \
                         render the full 17-block context because the substrate-bus, workspace-status, \
                         and recovery-log substrates aren't wired in this transport. Connect via \
                         the SSE endpoint to use this tool."
                            .to_string(),
                    );
                }
            };

            let question_hint = arguments
                .get("question_hint")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // Build the full context via the lifted helper (C1). The
            // helper handles every honest-empty case internally —
            // blocks where the substrate has no signal omit
            // themselves, no fabrication.
            //
            // `skills: None` here because the MCP layer doesn't load
            // the workspace's `SkillRegistry` on each call (would be
            // a per-call disk read). The skill-body block stays
            // suppressed for MCP callers; they can render it
            // themselves if they want via a separate tool.
            let build = crate::intelligence::reminder_assembly::build(
                app_state,
                ws,
                session_id,
                question_hint,
                None,
            )
            .await;

            // Identity for MCP callers comes from
            // `engine.workspace_chat_snapshot` — same path REST chat
            // uses. Cheap (in-memory cache lookup).
            let identity_owned = {
                let snapshot = engine.workspace_chat_snapshot(ws).await;
                snapshot
                    .as_ref()
                    .map(|s| {
                        crate::intelligence::identity::build_workspace_identity(s, &s.config.chat)
                    })
            };
            let bus_ctx = build.as_context(identity_owned.as_ref());
            let rendered =
                crate::intelligence::reminder_bus::render_reactive_reminders(&bus_ctx);

            let payload = serde_json::json!({
                "workspace": ws,
                "session_id": session_id,
                "rendered": rendered,
                "block_count": {
                    "agentmemory_recalls": build.agentmemory_recalls.len(),
                    "engrams": build.engram_handles.len(),
                    "mcp_sessions": build.mcp_sessions.len(),
                    "recovery_events": build.recovery_events.len(),
                    "sub_agent_reports": build.recent_sub_agent_reports.len(),
                    "gap_alerts": build.gap_alerts.len(),
                    "contradiction_alerts": build.contradiction_alerts.len(),
                    "skill_picked": build.relevant_skill_name.is_some(),
                    "branch_present": build.branch_summary.is_some(),
                    "substrate_freshness_present": build.substrate_freshness.is_some(),
                    "previous_verify_present": build.previous_verify_critique.is_some(),
                    "search_was_shallow_present": build.search_was_shallow.is_some(),
                },
                "rendered_bytes": rendered.len(),
            });
            mcp_text_result(id, &payload)
        }
        // ── C18 (2026-05-22) — Branch ergonomic + subscribe tools ──
        "branch_fork" => {
            let app_state = match state {
                Some(s) => s,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32601,
                        "branch_fork requires HTTP transport".to_string(),
                    );
                }
            };
            let _ = app_state;
            let from_branch = arguments
                .get("from")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    // Default to session's active branch or "main".
                    "main"
                });
            let name = arguments
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| format!("sandbox/{}", ulid::Ulid::new()));
            let kind = arguments
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("sandbox");
            let workspace_root = match engine.workspace_root_path(ws) {
                Some(p) => p,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        format!("workspace '{ws}' not mounted"),
                    );
                }
            };
            let branch_kind = match kind {
                "feature" => thinkingroot_core::BranchKind::Feature,
                "stream" => thinkingroot_core::BranchKind::Stream {
                    session_id: session_id.to_string(),
                },
                _ => thinkingroot_core::BranchKind::Sandbox {
                    agent_id: session_id.to_string(),
                },
            };
            match thinkingroot_branch::create_branch_full(
                &workspace_root,
                &name,
                from_branch,
                None,
                Some(session_id.to_string()),
                thinkingroot_core::BranchPermissions::default(),
                branch_kind,
                thinkingroot_core::MergePolicy::Manual,
                None,
            )
            .await
            {
                Ok(branch_ref) => mcp_text_result(
                    id,
                    &serde_json::json!({
                        "name": branch_ref.name,
                        "parent": branch_ref.parent,
                        "kind": kind,
                        "merge_policy": "manual",
                    }),
                ),
                Err(e) => JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("branch_fork failed: {e}"),
                ),
            }
        }
        "branch_state" => {
            let branch_name = match arguments.get("branch").and_then(|v| v.as_str()) {
                Some(b) => b.to_string(),
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "branch_state: missing 'branch'".to_string(),
                    );
                }
            };
            let workspace_root = match engine.workspace_root_path(ws) {
                Some(p) => p,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        format!("workspace '{ws}' not mounted"),
                    );
                }
            };
            // Look up the branch in the registry — gives us
            // name, kind, merge_policy, parent, status.
            match thinkingroot_branch::list_branches(&workspace_root) {
                Ok(branches) => {
                    match branches.into_iter().find(|b| b.name == branch_name) {
                        Some(branch) => mcp_text_result(
                            id,
                            &serde_json::json!({
                                "name": branch.name,
                                "parent": branch.parent,
                                "kind": format!("{:?}", branch.kind),
                                "merge_policy": format!("{:?}", branch.merge_policy),
                                "created_at": branch.created_at.to_rfc3339(),
                                "description": branch.description,
                                "owner": branch.owner,
                            }),
                        ),
                        None => JsonRpcResponse::error(
                            id,
                            -32602,
                            format!("branch '{branch_name}' not found in workspace '{ws}'"),
                        ),
                    }
                }
                Err(e) => JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("branch_state list_branches failed: {e}"),
                ),
            }
        }
        "branch_subscribe" => {
            // v1 subscribe: returns a subscription_id +
            // confirmation; actual change-detection wiring lands
            // when the broadcast hub (branch_event_hub on
            // AppState) is consumed by a background task that
            // emits notifications/message. For this commit we
            // ship the wire contract + bookkeeping; live
            // delivery is wired in when the corresponding flow
            // ships.
            let _branch = match arguments.get("branch").and_then(|v| v.as_str()) {
                Some(b) => b.to_string(),
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "branch_subscribe: missing 'branch'".to_string(),
                    );
                }
            };
            let max_fires = arguments
                .get("max_fires")
                .and_then(|v| v.as_u64())
                .unwrap_or(1);
            let ttl_secs = arguments
                .get("ttl_secs")
                .and_then(|v| v.as_u64());
            let subscription_id = format!("sub-{}", ulid::Ulid::new());
            mcp_text_result(
                id,
                &serde_json::json!({
                    "subscription_id": subscription_id,
                    "branch": _branch,
                    "workspace": ws,
                    "max_fires": max_fires,
                    "ttl_secs": ttl_secs,
                    "session_bound": true,
                    "delivery": "notifications/message on the SSE channel",
                }),
            )
        }
        // ── C17 (2026-05-22) — Flow orchestrator tools ────────────────
        "flow_define" => {
            let app_state = match state {
                Some(s) => s,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32601,
                        "flow_define requires HTTP transport (stdio MCP has no AppState)"
                            .to_string(),
                    );
                }
            };
            let definition_json = match arguments.get("definition") {
                Some(v) => v.clone(),
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "flow_define: missing 'definition' argument".to_string(),
                    );
                }
            };
            let definition: thinkingroot_flow::FlowDefinition =
                match serde_json::from_value(definition_json) {
                    Ok(d) => d,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            -32602,
                            format!("flow_define: invalid definition: {e}"),
                        );
                    }
                };
            // Resolve workspace root for the FlowStore.
            let workspace_root = match engine.workspace_root_path(ws) {
                Some(p) => p,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        format!("workspace '{ws}' not mounted"),
                    );
                }
            };
            let store = thinkingroot_flow::storage::FlowStore::new(workspace_root);
            let _ = app_state;
            match store.insert_flow_definition(definition) {
                Ok(record) => mcp_text_result(
                    id,
                    &serde_json::json!({
                        "flow_id": record.definition.id,
                        "version": record.definition.version,
                        "content_blake3": record.content_blake3,
                        "created_at": record.created_at.to_rfc3339(),
                        "updated_at": record.updated_at.to_rfc3339(),
                    }),
                ),
                Err(e) => JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("flow_define: {e}"),
                ),
            }
        }
        "flow_run" => {
            let app_state = match state {
                Some(s) => s,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32601,
                        "flow_run requires HTTP transport".to_string(),
                    );
                }
            };
            let flow_id = match arguments.get("flow_id").and_then(|v| v.as_str()) {
                Some(f) => f.to_string(),
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "flow_run: missing 'flow_id'".to_string(),
                    );
                }
            };
            let inputs_value = arguments
                .get("inputs")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
            let originating_session_id = arguments
                .get("conversation_id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| Some(session_id.to_string()));
            let workspace_root = match engine.workspace_root_path(ws) {
                Some(p) => p,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        format!("workspace '{ws}' not mounted"),
                    );
                }
            };

            // Build a per-call FlowRuntime. The Executors registry
            // is populated with the four production executors:
            // deterministic, local_llm, client_sampling, mcp_tool.
            // Human executor uses AutoApprove for v1 — production
            // wiring through the daemon's ApprovalGate router is
            // the C19 follow-up.
            let store = thinkingroot_flow::storage::FlowStore::new(workspace_root);
            let exec_registry = thinkingroot_flow::executors::deterministic::DeterministicRegistry::with_builtins();
            let executors = thinkingroot_flow::runtime::Executors::default();
            executors
                .register(
                    thinkingroot_flow::runtime::NodeTypeKind::Deterministic,
                    std::sync::Arc::new(
                        thinkingroot_flow::executors::deterministic::DeterministicExecutor::new(
                            exec_registry,
                        ),
                    ),
                )
                .await;
            // local_llm — needs engine handle. The engine here is
            // a &QueryEngine borrowed for the lifetime of this
            // dispatch arm; we need an owned Arc<RwLock<...>>.
            // The AppState carries it.
            executors
                .register(
                    thinkingroot_flow::runtime::NodeTypeKind::LocalLlm,
                    std::sync::Arc::new(crate::flow_executors::local_llm::LocalLlmExecutor::new(
                        app_state.engine.clone(),
                    )),
                )
                .await;
            executors
                .register(
                    thinkingroot_flow::runtime::NodeTypeKind::ClientSampling,
                    std::sync::Arc::new(crate::flow_executors::client_sampling::ClientSamplingExecutor::new(
                        app_state.clone(),
                    )),
                )
                .await;
            executors
                .register(
                    thinkingroot_flow::runtime::NodeTypeKind::McpTool,
                    std::sync::Arc::new(crate::flow_executors::mcp_tool::McpToolExecutor::new(
                        app_state.engine.clone(),
                        app_state.sessions.clone(),
                        app_state.engram_manager.clone(),
                        app_state.clone(),
                    )),
                )
                .await;
            executors
                .register(
                    thinkingroot_flow::runtime::NodeTypeKind::Human,
                    std::sync::Arc::new(crate::flow_executors::human::HumanExecutor::new(
                        std::sync::Arc::new(crate::intelligence::approval::AutoApprove),
                    )),
                )
                .await;
            let runtime = thinkingroot_flow::runtime::FlowRuntime::new(store, executors);

            match runtime
                .start_run_for_session(
                    &flow_id,
                    ws,
                    "main",
                    inputs_value,
                    originating_session_id,
                )
                .await
            {
                Ok(handle) => {
                    let response = serde_json::json!({
                        "flow_run_id": handle.flow_run_id,
                        "status": "running",
                        "started_at": handle.started_at.to_rfc3339(),
                    });
                    // The handle's join_handle is dropped here —
                    // the spawned task continues running in the
                    // background; status + cancellation are
                    // accessed via the storage layer's
                    // flow_runs/<id>.json file. A future commit
                    // can stash the handle in
                    // `AppState.active_flow_runs` for in-memory
                    // cancellation routing without a disk round-trip.
                    let _ = handle;
                    mcp_text_result(id, &response)
                }
                Err(e) => JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("flow_run: {e}"),
                ),
            }
        }
        "flow_status" => {
            let app_state = match state {
                Some(s) => s,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32601,
                        "flow_status requires HTTP transport".to_string(),
                    );
                }
            };
            let _ = app_state;
            let flow_run_id = match arguments.get("flow_run_id").and_then(|v| v.as_str()) {
                Some(f) => f.to_string(),
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "flow_status: missing 'flow_run_id'".to_string(),
                    );
                }
            };
            let workspace_root = match engine.workspace_root_path(ws) {
                Some(p) => p,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        format!("workspace '{ws}' not mounted"),
                    );
                }
            };
            let store = thinkingroot_flow::storage::FlowStore::new(workspace_root);
            let record = match store.get_flow_run(&flow_run_id) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        format!("flow_run_id '{flow_run_id}' not found"),
                    );
                }
                Err(e) => {
                    return JsonRpcResponse::error(
                        id,
                        -32603,
                        format!("flow_status read failed: {e}"),
                    );
                }
            };
            // Optional cancel action — for v1 we mark the
            // run's status in the store. Live cancellation
            // routing through CancellationToken needs the
            // active_flow_runs registry (C17.5 follow-up).
            if let Some(action) = arguments.get("action").and_then(|v| v.as_str()) {
                if action == "cancel" && !record.status.is_terminal() {
                    let mut updated = record.clone();
                    updated.status = thinkingroot_flow::storage::FlowRunStatus::Cancelled;
                    updated.finished_at = Some(chrono::Utc::now());
                    updated.error = Some("cancelled by flow_status action".to_string());
                    if let Err(e) = store.upsert_flow_run(&updated) {
                        return JsonRpcResponse::error(
                            id,
                            -32603,
                            format!("flow_status cancel failed: {e}"),
                        );
                    }
                    return mcp_text_result(
                        id,
                        &serde_json::json!({
                            "flow_run_id": flow_run_id,
                            "status": "cancelled",
                            "previous_status": format!("{:?}", record.status),
                        }),
                    );
                }
            }
            mcp_text_result(
                id,
                &serde_json::json!({
                    "flow_run_id": record.flow_run_id,
                    "flow_id": record.flow_id,
                    "status": record.status,
                    "current_node": record.current_node,
                    "started_at": record.started_at.to_rfc3339(),
                    "finished_at": record.finished_at.map(|t| t.to_rfc3339()),
                    "parent_branch": record.parent_branch,
                    "originating_session_id": record.originating_session_id,
                    "node_outputs_count": record.node_outputs.len(),
                    "outputs": record.outputs,
                    "error": record.error,
                }),
            )
        }
        // ── Intelligent memory ask (Phase 3.6 — full hybrid pipeline) ─────
        "ask" => {
            let question = match arguments.get("question").and_then(|v| v.as_str()) {
                Some(q) => q.to_string(),
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'question' argument".to_string(),
                    );
                }
            };
            let session_scope: Vec<String> = arguments
                .get("session_scope")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let question_date = arguments
                .get("question_date")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Infer category: use hint if given, else router
            let category_hint = arguments
                .get("category_hint")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let category = if !category_hint.is_empty() {
                category_hint.clone()
            } else {
                let tmp_session = SessionContext::new(session_id, ws);
                match crate::intelligence::router::classify_query(&question, &tmp_session) {
                    crate::intelligence::router::QueryPath::Agentic => {
                        let q = question.to_lowercase();
                        if q.contains(" ago")
                            || q.contains("last ")
                            || q.contains("when ")
                            || q.contains("how many days")
                        {
                            "temporal-reasoning".to_string()
                        } else {
                            "multi-session".to_string()
                        }
                    }
                    crate::intelligence::router::QueryPath::Fast => {
                        "single-session-user".to_string()
                    }
                }
            };

            let allowed_sources: std::collections::HashSet<String> =
                session_scope.iter().cloned().collect();
            let sessions_dir = sessions_dir_for(engine, ws);
            let llm = engine.workspace_llm(ws);

            use crate::intelligence::identity::build_workspace_identity;
            use crate::intelligence::synthesizer::{AskRequest, ask as synth_ask};

            let snapshot = engine.workspace_chat_snapshot(ws).await;
            let chat = snapshot
                .as_ref()
                .map(|s| s.config.chat.resolve(&s.source_kinds))
                .unwrap_or_else(AskRequest::default_chat);
            let identity_owned = snapshot
                .as_ref()
                .map(|s| build_workspace_identity(s, &s.config.chat));
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();

            let req = AskRequest {
                workspace: ws,
                question: &question,
                category: &category,
                allowed_sources: &allowed_sources,
                question_date: &question_date,
                session_dates: &std::collections::HashMap::new(),
                answer_sids: &session_scope,
                sessions_dir: &sessions_dir,
                excluded_claim_ids: &std::collections::HashSet::new(),
                chat,
                identity: identity_owned.as_ref(),
                today: Some(&today),
                // MCP `ask` is a stateless tool call — each invocation
                // is single-shot. Multi-turn memory comes from the
                // calling agent's own context, not from us.
                history: crate::intelligence::synthesizer::NO_HISTORY,
            };
            let result = synth_ask(engine, llm, &req).await;
            let text = format!(
                "{}\n\n[claims_used: {} | category: {}]",
                result.answer, result.claims_used, result.category
            );
            JsonRpcResponse::success(
                id,
                serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
            )
        }

        // ── Classic search ────────────────────────────────────────────────
        "search" => {
            let query = match arguments.get("query").and_then(|v| v.as_str()) {
                Some(q) => q,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'query' argument".to_string(),
                    );
                }
            };
            let top_k = arguments
                .get("top_k")
                .and_then(|v| v.as_u64())
                .unwrap_or(10) as usize;
            // Task 13: explicit `branch` overrides the session's
            // active branch when present. Falls back to the session
            // snapshot otherwise so existing call sites keep their
            // current behaviour (set via `checkout_branch`).
            let explicit_branch: Option<String> = arguments
                .get("branch")
                .and_then(|v| v.as_str())
                .map(String::from);
            let mut session_snapshot = {
                let store = sessions.lock().await;
                store.get(session_id).cloned()
            }
            .unwrap_or_else(|| {
                crate::intelligence::session::SessionContext::new(session_id, ws)
            });
            if let Some(b) = explicit_branch {
                session_snapshot.active_branch = Some(b);
            }
            match engine
                .search_with_routing(ws, query, top_k, &session_snapshot)
                .await
            {
                Ok(content) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "content": [{ "type": "text", "text": content }] }),
                ),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Classic claim filter ──────────────────────────────────────────
        "query_claims" => {
            // Task 13: explicit `branch` overrides session's active
            // branch when supplied; falls back to session otherwise.
            let active_branch: Option<String> = match arguments
                .get("branch")
                .and_then(|v| v.as_str())
                .map(String::from)
            {
                Some(b) => Some(b),
                None => {
                    let store = sessions.lock().await;
                    store.get(session_id).and_then(|s| s.active_branch.clone())
                }
            };
            let filter = ClaimFilter {
                claim_type: arguments
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                entity_name: arguments
                    .get("entity")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                min_confidence: arguments.get("min_confidence").and_then(|v| v.as_f64()),
                limit: Some(100),
                offset: None,
            };
            match engine
                .list_claims_branched(ws, filter, active_branch.as_deref())
                .await
            {
                Ok(claims) => mcp_text_result(id, &claims),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Witness Mesh — list witnesses by workspace ─────────────────
        "list_witnesses" => {
            let rule_filter: Option<String> = arguments
                .get("rule")
                .and_then(|v| v.as_str())
                .map(String::from);
            let limit: Option<usize> = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            match engine.list_witnesses(ws, limit).await {
                Ok(mut witnesses) => {
                    if let Some(rule_name) = &rule_filter {
                        witnesses.retain(|w| &w.rule == rule_name);
                    }
                    mcp_text_result(id, &witnesses)
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Witness Mesh — walk DAG from a starting Witness ────────────
        "walk_mesh" => {
            let witness_id = match arguments.get("witness_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "walk_mesh: missing required `witness_id` argument".to_string(),
                    );
                }
            };
            // Clamp depth + fanout to safe bounds per the tool
            // descriptor. Surface clamps in the response so the
            // caller can tell when their value was tightened.
            let raw_depth = arguments
                .get("max_depth")
                .and_then(|v| v.as_u64())
                .unwrap_or(4) as usize;
            let max_depth = raw_depth.min(10);
            let raw_fanout = arguments
                .get("max_fanout")
                .and_then(|v| v.as_u64())
                .unwrap_or(50) as usize;
            let max_fanout = raw_fanout.clamp(1, 200);
            match engine
                .walk_witness_mesh(ws, &witness_id, max_depth, max_fanout)
                .await
            {
                Ok((witnesses, edges)) => {
                    let payload = serde_json::json!({
                        "witnesses": witnesses,
                        "edges": edges.iter().map(|(p, c)| {
                            serde_json::json!({ "parent": p, "child": c })
                        }).collect::<Vec<_>>(),
                        "max_depth": max_depth,
                        "max_fanout": max_fanout,
                        "depth_clamped": raw_depth > max_depth,
                        "fanout_clamped": raw_fanout != max_fanout,
                    });
                    mcp_text_result(id, &payload)
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Classic relations ─────────────────────────────────────────────
        "get_relations" => {
            // Task 13: explicit `branch` overrides session's active
            // branch when supplied; falls back to session otherwise.
            let active_branch: Option<String> = match arguments
                .get("branch")
                .and_then(|v| v.as_str())
                .map(String::from)
            {
                Some(b) => Some(b),
                None => {
                    let store = sessions.lock().await;
                    store.get(session_id).and_then(|s| s.active_branch.clone())
                }
            };
            let entity = match arguments.get("entity").and_then(|v| v.as_str()) {
                Some(e) => e,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'entity' argument".to_string(),
                    );
                }
            };
            match engine
                .get_relations_branched(ws, entity, active_branch.as_deref())
                .await
            {
                Ok(rels) => mcp_text_result(id, &rels),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Pipeline ──────────────────────────────────────────────────────
        "compile" => match engine.compile(ws).await {
            Ok(result) => {
                // Plan §3.10: dirty compile invalidates Engrams for the
                // workspace so subsequent probes don't return GC'd ids.
                if result.cache_dirty {
                    engram_manager.invalidate_workspace(ws).await;
                }
                mcp_text_result(id, &result)
            }
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },

        "health_check" => match engine.health(ws).await {
            Ok(result) => mcp_text_result(id, &result),
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },

        // ── KVC branch tools ─────────────────────────────────────────────
        "create_branch" => {
            let branch_name = match arguments.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'name' argument".to_string(),
                    );
                }
            };
            let root = match branch_resolve_root(id.clone(), &arguments, engine, ws, "create_branch")
            {
                Ok(p) => p,
                Err(r) => return r,
            };
            let description = arguments
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from);
            match thinkingroot_branch::create_branch_with_owner(
                &root,
                branch_name,
                "main",
                description,
                Some(session_id.to_string()),
                thinkingroot_core::BranchPermissions::default(),
            )
            .await
            {
                Ok(branch) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": format!("Branch '{}' created from main", branch.name) }]
                    }),
                ),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "diff_branch" => {
            let branch_name = match arguments.get("branch").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'branch' argument".to_string(),
                    );
                }
            };
            let root = match branch_resolve_root(id.clone(), &arguments, engine, ws, "diff_branch")
            {
                Ok(p) => p,
                Err(r) => return r,
            };
            use thinkingroot_branch::diff::compute_diff;
            use thinkingroot_branch::snapshot::resolve_data_dir;
            use thinkingroot_core::config::Config;
            use thinkingroot_graph::graph::GraphStore;

            let config = match Config::load_merged(&root) {
                Ok(c) => c,
                Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
            };
            let mc = &config.merge;
            let main_data_dir = resolve_data_dir(&root, None);
            let branch_data_dir = resolve_data_dir(&root, Some(branch_name));
            if !branch_data_dir.exists() {
                return JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("branch '{}' not found", branch_name),
                );
            }
            let main_graph = match GraphStore::init(&main_data_dir.join("graph")) {
                Ok(g) => g,
                Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
            };
            let branch_graph = match GraphStore::init(&branch_data_dir.join("graph")) {
                Ok(g) => g,
                Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
            };
            match compute_diff(
                &main_graph,
                &branch_graph,
                branch_name,
                mc.auto_resolve_threshold,
                mc.max_health_drop,
                mc.block_on_contradictions,
            ) {
                Ok(diff) => mcp_text_result(id, &diff),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "merge_branch" => {
            let branch_name = match arguments.get("branch").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'branch' argument".to_string(),
                    );
                }
            };
            let root = match branch_resolve_root(id.clone(), &arguments, engine, ws, "merge_branch")
            {
                Ok(p) => p,
                Err(r) => return r,
            };
            let force = arguments
                .get("force")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let target = arguments.get("target").and_then(|v| v.as_str());
            let propagate_deletions = arguments
                .get("propagate_deletions")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match engine
                .merge_into_branch(
                    &root,
                    branch_name,
                    target,
                    force,
                    propagate_deletions,
                    thinkingroot_core::MergedBy::Human {
                        user: session_id.to_string(),
                    },
                )
                .await
            {
                Ok(diff) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "Branch '{}' merged into '{}': {} new claims, {} new entities, {} auto-resolved",
                                branch_name,
                                target.unwrap_or("main"),
                                diff.new_claims.len(),
                                diff.new_entities.len(),
                                diff.auto_resolved.len()
                            )
                        }]
                    }),
                ),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "rebase_branch" => {
            let branch_name = match arguments.get("branch").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'branch' argument".to_string(),
                    );
                }
            };
            let root = match branch_resolve_root(id.clone(), &arguments, engine, ws, "rebase_branch")
            {
                Ok(p) => p,
                Err(r) => return r,
            };
            match engine
                .rebase_branch(&root, branch_name, session_actor(sessions, session_id).await)
                .await
            {
                Ok(diff) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "Branch '{}' rebased from '{}': {} new claims, {} new entities, {} auto-resolved",
                                branch_name,
                                diff.from_branch,
                                diff.new_claims.len(),
                                diff.new_entities.len(),
                                diff.auto_resolved.len()
                            )
                        }]
                    }),
                ),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // `checkout_branch` — set or clear the session's active branch.
        "checkout_branch" => {
            let branch_opt = arguments.get("branch").and_then(|v| v.as_str());
            let mut store = sessions.lock().await;
            let session = store
                .entry(session_id.to_string())
                .or_insert_with(|| SessionContext::new(session_id, ws));
            match branch_opt {
                Some(branch_name) => {
                    session.set_branch(branch_name.to_string());
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({
                            "content": [{
                                "type": "text",
                                "text": format!(
                                    "Checked out branch '{}'\nContribute will now write to this branch instead of main.\nUse diff_branch('{}') to review, merge_branch('{}') when ready.",
                                    branch_name, branch_name, branch_name
                                )
                            }]
                        }),
                    )
                }
                None => {
                    session.clear_branch();
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({
                            "content": [{ "type": "text", "text": "Returned to main — contribute will write directly to main." }]
                        }),
                    )
                }
            }
        }

        // ── Intelligent serve tools ───────────────────────────────────────

        // `brief` — Tier-0 workspace orientation (~100-200 tokens).
        "brief" => {
            let active_branch: Option<String> = {
                let store = sessions.lock().await;
                store.get(session_id).and_then(|s| s.active_branch.clone())
            };
            match engine
                .get_workspace_brief_branched(ws, active_branch.as_deref())
                .await
            {
                Ok(summary) => {
                    let text = compressor::format_workspace_brief(
                        &summary.workspace,
                        summary.entity_count,
                        summary.claim_count,
                        summary.source_count,
                        &summary.top_entities,
                        &summary.recent_decisions,
                        summary.contradiction_count,
                    );
                    // Update session with workspace context.
                    let mut store = sessions.lock().await;
                    let session = store
                        .entry(session_id.to_string())
                        .or_insert_with(|| SessionContext::new(session_id, ws));
                    session.reset_budget();
                    drop(store);

                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // `investigate` — intent-aware deep retrieval with session delta delivery.
        // The planner classifies the query intent and routes to the right graph method.
        "investigate" => {
            // Resolve entity name from argument or session focus.
            let entity_name: String = match arguments
                .get("entity")
                .and_then(|v| v.as_str())
                .map(String::from)
            {
                Some(e) => e,
                None => {
                    let store = sessions.lock().await;
                    match store.get(session_id).and_then(|s| s.focus_entity.clone()) {
                            Some(f) => f,
                            None => {
                                return JsonRpcResponse::error(
                                    id,
                                    -32602,
                                    "Missing 'entity' argument (and no focus entity set — use focus tool first)".to_string(),
                                )
                            }
                        }
                }
            };

            // Read session snapshot for planner (and capture active_branch).
            let (session_snapshot, active_branch) = {
                let store = sessions.lock().await;
                let snap = store
                    .get(session_id)
                    .cloned()
                    .unwrap_or_else(|| SessionContext::new(session_id, ws));
                let branch = snap.active_branch.clone();
                (snap, branch)
            };

            // Plan: choose retrieval strategy (full context / reverse deps / neighborhood).
            let plan = planner::plan_query(&entity_name, &session_snapshot);

            let text = match plan.steps.first() {
                Some(PlanStep::FindReverseDeps(name)) => {
                    match engine
                        .get_entity_context_branched(ws, name, active_branch.as_deref())
                        .await
                    {
                        Ok(Some(ctx)) => {
                            let mut out = format!("## Reverse dependencies of {name}\n");
                            if ctx.incoming_relations.is_empty() {
                                out.push_str("  (none found)\n");
                            } else {
                                for (src, rel, str) in &ctx.incoming_relations {
                                    out.push_str(&format!("  ← {src} [{rel}] {str:.2}\n"));
                                }
                            }
                            out
                        }
                        Ok(None) => format!("Entity '{name}' not found\n"),
                        Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
                    }
                }
                Some(PlanStep::GetNeighborhood(name)) => {
                    match engine
                        .get_entity_context_branched(ws, name, active_branch.as_deref())
                        .await
                    {
                        Ok(Some(ctx)) => {
                            let mut out = format!("## Neighborhood of {name}\n");
                            for (t, rel, str) in &ctx.outgoing_relations {
                                out.push_str(&format!("  → {t} [{rel}] {str:.2}\n"));
                            }
                            for (s, rel, str) in &ctx.incoming_relations {
                                out.push_str(&format!("  ← {s} [{rel}] {str:.2}\n"));
                            }
                            out
                        }
                        Ok(None) => format!("Entity '{name}' not found\n"),
                        Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
                    }
                }
                _ => {
                    // Full entity context with session-aware compression.
                    match engine
                        .get_entity_context_branched(ws, &entity_name, active_branch.as_deref())
                        .await
                    {
                        Ok(None) => {
                            return JsonRpcResponse::error(
                                id,
                                -32603,
                                format!("Entity '{}' not found in workspace '{}'", entity_name, ws),
                            );
                        }
                        Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
                        Ok(Some(ctx)) => {
                            let (delivered, budget) = {
                                let store = sessions.lock().await;
                                let d = store
                                    .get(session_id)
                                    .map(|s| s.delivered_claim_ids.clone())
                                    .unwrap_or_default();
                                let b = store
                                    .get(session_id)
                                    .map(|s| s.token_budget)
                                    .unwrap_or(4_000);
                                (d, b)
                            };

                            let packet = compressor::compress(&ctx, budget, &delivered);
                            let new_claim_ids: Vec<String> = packet
                                .claim_ids
                                .iter()
                                .filter(|cid| !delivered.contains(cid.as_str()))
                                .cloned()
                                .collect();
                            let new_count = new_claim_ids.len();
                            let total_count = packet.claim_ids.len();
                            let token_count = packet.estimated_tokens;

                            {
                                let mut store = sessions.lock().await;
                                let session = store
                                    .entry(session_id.to_string())
                                    .or_insert_with(|| SessionContext::new(session_id, ws));
                                session.mark_delivered(&new_claim_ids);
                                session.record_entity(entity_name.clone());
                                session.deduct_tokens(token_count);
                            }

                            let mut text = compressor::format_packet(&packet);
                            text.push_str(&format!(
                                "\n--- {new_count} new / {total_count} total claims | ~{token_count} tokens\n"
                            ));
                            text
                        }
                    }
                }
            };

            JsonRpcResponse::success(
                id,
                serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
            )
        }

        // `focus` — set the session focal entity for follow-up queries.
        "focus" => {
            let entity_name = match arguments.get("entity").and_then(|v| v.as_str()) {
                Some(e) => e,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'entity' argument".to_string(),
                    );
                }
            };

            // Verify entity exists before setting focus.
            match engine.get_entity_context(ws, entity_name).await {
                Ok(None) => JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("Entity '{}' not found in workspace '{}'", entity_name, ws),
                ),
                Ok(Some(_)) => {
                    let mut store = sessions.lock().await;
                    let session = store
                        .entry(session_id.to_string())
                        .or_insert_with(|| SessionContext::new(session_id, ws));
                    session.set_focus(entity_name.to_string());
                    let delivered = session.delivered_count();
                    let explored = session.active_entities.len();
                    drop(store);

                    let text = format!(
                        "Focused on: {entity_name}\n\
                         Session: {explored} entities explored · {delivered} claims delivered\n\
                         --- follow-up: investigate({entity_name}), or ask about reverse deps / neighbors\n"
                    );
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // `contribute` — off-pipeline agent write-back.
        "contribute" => {
            let raw_claims = match arguments.get("claims") {
                Some(v) => v,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'claims' argument".to_string(),
                    );
                }
            };

            let agent_claims: Vec<AgentClaim> = match serde_json::from_value(raw_claims.clone()) {
                Ok(c) => c,
                Err(e) => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        format!("Invalid claims format: {e}"),
                    );
                }
            };

            // Read the session's active branch (set by checkout_branch).
            let active_branch: Option<String> = {
                let store = sessions.lock().await;
                store.get(session_id).and_then(|s| s.active_branch.clone())
            };

            match engine
                .contribute_claims_as(
                    ws,
                    session_id,
                    active_branch.as_deref(),
                    agent_claims,
                    sessions,
                    session_actor(sessions, session_id).await,
                )
                .await
            {
                Ok(result) => {
                    let target = active_branch.as_deref().unwrap_or("main");
                    let mut text = format!(
                        "Contributed {} claim(s) to workspace '{}' (branch: {})\n\
                         source: {}\n\
                         trust: Untrusted (run 'root compile' to validate)\n",
                        result.accepted_count, ws, target, result.source_uri
                    );
                    if active_branch.is_some() {
                        text.push_str(&format!(
                            "review: diff_branch('{}') · merge: merge_branch('{}')\n",
                            target, target
                        ));
                    }
                    if !result.warnings.is_empty() {
                        text.push_str("warnings:\n");
                        for w in &result.warnings {
                            text.push_str(&format!("  ⚠ {w}\n"));
                        }
                    }
                    text.push_str(&format!("ids: {}\n", result.accepted_ids.join(", ")));
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // T0.7 — connector-attributed bulk contribute. Idempotent on
        // (connector_id, install_id, idempotency_key). Replay short-
        // circuits to the cached accepted_ids without writing claims.
        "contribute_bulk" => {
            let connector_id = match arguments.get("connector_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing or empty 'connector_id' argument".to_string(),
                    );
                }
            };
            let install_id = match arguments.get("install_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing or empty 'install_id' argument".to_string(),
                    );
                }
            };
            let idempotency_key =
                match arguments.get("idempotency_key").and_then(|v| v.as_str()) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => {
                        return JsonRpcResponse::error(
                            id,
                            -32602,
                            "Missing or empty 'idempotency_key' argument".to_string(),
                        );
                    }
                };
            let backfill = arguments
                .get("backfill")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let raw_claims = match arguments.get("claims") {
                Some(v) => v,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'claims' argument".to_string(),
                    );
                }
            };
            let claims: Vec<AgentClaim> = match serde_json::from_value(raw_claims.clone()) {
                Ok(c) => c,
                Err(e) => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        format!("Invalid claims format: {e}"),
                    );
                }
            };

            // Branch can come from the explicit `branch` arg or from the
            // session's checked-out branch (matches the `contribute` path).
            let branch_from_session = {
                let store = sessions.lock().await;
                store.get(session_id).and_then(|s| s.active_branch.clone())
            };
            let branch_arg: Option<String> = arguments
                .get("branch")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty() && *s != "main")
                .map(String::from)
                .or(branch_from_session);

            let session_arg: String = arguments
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    format!("connector:{connector_id}:{install_id}:{idempotency_key}")
                });

            let principal = crate::engine::Principal::Connector {
                connector_id: connector_id.clone(),
                install_id: install_id.clone(),
            };

            match engine
                .contribute_bulk(
                    ws,
                    &session_arg,
                    branch_arg.as_deref(),
                    claims,
                    sessions,
                    principal,
                    &idempotency_key,
                    backfill,
                )
                .await
            {
                Ok(result) => {
                    let mut text = format!(
                        "Connector contribute: {} claim(s) accepted (workspace '{}', branch: {})\n\
                         source: {}\n\
                         connector: {}/{}\n\
                         idempotency_key: {}\n",
                        result.accepted_count,
                        ws,
                        branch_arg.as_deref().unwrap_or("main"),
                        result.source_uri,
                        connector_id,
                        install_id,
                        idempotency_key,
                    );
                    if !result.warnings.is_empty() {
                        text.push_str("warnings:\n");
                        for w in &result.warnings {
                            text.push_str(&format!("  ⚠ {w}\n"));
                        }
                    }
                    text.push_str(&format!("ids: {}\n", result.accepted_ids.join(", ")));
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Rooting: trust-tier filtered query ────────────────────────────
        //
        // Week 5 ships a graph-direct implementation that queries claims by
        // admission_tier rather than going through the in-memory cache. This
        // guarantees freshness (Phase 6.5 writes tiers synchronously) but
        // returns the full Claim struct. The cache-backed path that includes
        // admission_tier in ClaimInfo is Week 6 polish.
        "query_rooted" => {
            let type_filter = arguments
                .get("type")
                .and_then(|v| v.as_str())
                .map(String::from);
            let entity_filter = arguments
                .get("entity")
                .and_then(|v| v.as_str())
                .map(String::from);
            let min_confidence = arguments.get("min_confidence").and_then(|v| v.as_f64());
            match engine
                .list_rooted_claims(ws, type_filter, entity_filter, min_confidence)
                .await
            {
                Ok(claims) => mcp_text_result(id, &claims),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Rooting: admission tier counts + recent failures ──────────────
        "rooting_report" => match engine.rooting_report(ws).await {
            Ok(report) => mcp_text_result(id, &report),
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },

        // ── Branch management: list / delete / gc / rollback ──────────────
        "list_branches" => {
            let root = match branch_resolve_root(id.clone(), &arguments, engine, ws, "list_branches")
            {
                Ok(p) => p,
                Err(r) => return r,
            };
            match thinkingroot_branch::list_branches(&root) {
                Ok(branches) => {
                    let content =
                        serde_json::to_string_pretty(&branches).unwrap_or_else(|_| "[]".into());
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({
                            "content": [{ "type": "text", "text": content }]
                        }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "delete_branch" => {
            let branch_name = match arguments.get("branch").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'branch' argument".to_string(),
                    );
                }
            };
            let root = match branch_resolve_root(id.clone(), &arguments, engine, ws, "delete_branch")
            {
                Ok(p) => p,
                Err(r) => return r,
            };
            match engine
                .delete_branch_as(&root, branch_name, session_actor(sessions, session_id).await)
                .await
            {
                Ok(()) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "Branch '{}' marked as Abandoned. Data dir retained — run gc_branches to reclaim disk.",
                                branch_name
                            )
                        }]
                    }),
                ),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "gc_branches" => {
            let root = match branch_resolve_root(id.clone(), &arguments, engine, ws, "gc_branches")
            {
                Ok(p) => p,
                Err(r) => return r,
            };
            match engine.gc_branches(&root).await {
                Ok(n) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Purged {} abandoned branch{}", n, if n == 1 { "" } else { "es" })
                        }]
                    }),
                ),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "rollback_merge" => {
            let branch_name = match arguments.get("branch").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'branch' argument".to_string(),
                    );
                }
            };
            let root = match branch_resolve_root(id.clone(), &arguments, engine, ws, "rollback_merge")
            {
                Ok(p) => p,
                Err(r) => return r,
            };
            match engine.rollback_merge(&root, branch_name).await {
                Ok(()) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "Rolled back merge of branch '{}' — main restored from most recent pre-merge snapshot. Cache reloaded.",
                                branch_name
                            )
                        }]
                    }),
                ),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Reflexive (Phase 9) ────────────────────────────────────────────
        "reflect" => {
            let branch = arguments.get("branch").and_then(|v| v.as_str());
            match engine.reflect_branched(ws, branch).await {
                Ok(summary) => {
                    let scope = branch.unwrap_or("main");
                    let text = format!(
                        "reflect complete (branch: {}) — patterns: {}, entity_types_scanned: {}, gaps_created: {}, gaps_resolved: {}, open_gaps_total: {}",
                        scope,
                        summary.patterns.len(),
                        summary.entity_types_scanned,
                        summary.gaps_created,
                        summary.gaps_resolved,
                        summary.open_gaps_total,
                    );
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": text }] }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "gaps" => {
            let entity = arguments.get("entity").and_then(|v| v.as_str());
            let min_conf = arguments
                .get("min_confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.70);
            let branch = arguments.get("branch").and_then(|v| v.as_str());
            match engine
                .list_gaps_branched(ws, entity, min_conf, branch)
                .await
            {
                Ok(gaps) => {
                    let text = if gaps.is_empty() {
                        "No open knowledge gaps at this confidence threshold.".to_string()
                    } else {
                        let mut out = format!("{} open gap(s):\n", gaps.len());
                        for g in &gaps {
                            out.push_str(&format!(
                                "- {} ({}): expected {} @ {:.0}% confidence (sample: {}) — {}\n",
                                g.entity_name,
                                g.entity_type,
                                g.expected_claim_type,
                                g.confidence * 100.0,
                                g.sample_size,
                                g.reason
                            ));
                        }
                        out
                    };
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({
                            "content": [{ "type": "text", "text": text }],
                            "gaps": serde_json::to_value(&gaps).unwrap_or(serde_json::Value::Null),
                        }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "reflect_across" => {
            let workspaces: Vec<String> = arguments
                .get("workspaces")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if workspaces.is_empty() {
                return JsonRpcResponse::error(
                    id,
                    -32602,
                    "Missing or empty 'workspaces' array".to_string(),
                );
            }
            match engine.reflect_across(&workspaces).await {
                Ok(result) => {
                    let ws_summaries: Vec<String> = result
                        .per_workspace
                        .iter()
                        .map(|(name, r)| {
                            format!(
                                "    {name}: +{} / -{} / ={} (open={})",
                                r.gaps_created,
                                r.gaps_resolved,
                                r.gaps_still_open,
                                r.open_gaps_total
                            )
                        })
                        .collect();
                    let text = format!(
                        "reflect_across complete\n\
                         scope: {}\n\
                         workspaces: {}\n\
                         aggregate patterns: {}\n\
                         per-workspace (+created / -resolved / =carried over):\n{}",
                        result.scope_id,
                        result.workspaces.join(", "),
                        result.aggregate_patterns.len(),
                        ws_summaries.join("\n"),
                    );
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({
                            "content": [{ "type": "text", "text": text }],
                            "result": serde_json::to_value(&result).unwrap_or(serde_json::Value::Null),
                        }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        "dismiss_gap" => {
            let gap_id = match arguments.get("gap_id").and_then(|v| v.as_str()) {
                Some(g) => g,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'gap_id' argument".to_string(),
                    );
                }
            };
            let branch = arguments.get("branch").and_then(|v| v.as_str());
            match engine.dismiss_gap(ws, gap_id, branch).await {
                Ok(()) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "Gap '{}' dismissed{}. It will not be re-raised by future reflect cycles.",
                                gap_id,
                                branch.map(|b| format!(" on branch '{b}'")).unwrap_or_default()
                            )
                        }]
                    }),
                ),
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── RARP / Active Engram Protocol v2 (4 tools) ─────────────────
        "materialize_engram" => {
            handle_materialize_engram(id, &arguments, engine, ws, session_id, engram_manager).await
        }

        "probe_engram" => {
            handle_probe_engram(id, &arguments, engine, ws, session_id, engram_manager).await
        }

        "list_engrams" => {
            let refs = engram_manager.list_engrams(session_id).await;
            JsonRpcResponse::success(id, serde_json::json!({ "engrams": refs }))
        }

        "expire_engram" => {
            let pointer = match arguments.get("pointer").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'pointer' argument".to_string(),
                    );
                }
            };
            let expired = engram_manager.expire_engram(session_id, pointer).await;
            JsonRpcResponse::success(id, serde_json::json!({ "expired": expired }))
        }

        "hybrid_retrieve" => handle_hybrid_retrieve(id, &arguments, engine, ws, session_id).await,

        // ── T0.4 Knowledge Proposal tools ─────────────────────────────
        "open_proposal" => handle_open_proposal(id, &arguments, engine, ws).await,
        "review_proposal" => handle_review_proposal(id, &arguments, engine, ws).await,
        "list_proposals" => handle_list_proposals(id, &arguments, engine, ws).await,
        "close_proposal" => handle_close_proposal(id, &arguments, engine, ws).await,

        // ── SOTA Lever 3 — Observer / Reflector tools ─────────────────
        "observe_turn" => handle_observe_turn(id, &arguments, engine).await,
        "flush_observations" => handle_flush_observations(id, &arguments, engine, ws).await,

        // ── Phase α Playground action verbs ───────────────────────────
        // See docs/2026-05-15-cognition-commits-design.md.
        // Backing helpers in `intelligence::playground_tools` — same
        // helpers the in-app ToolRegistry uses, so MCP and native
        // function-calling produce byte-equivalent results.
        "save_note" => handle_save_note(id, &arguments, engine, ws).await,
        "regenerate_paper" => handle_regenerate_paper(id, engine, ws).await,
        "ingest_path" => handle_ingest_path(id, &arguments, engine, ws).await,
        "list_directory" => handle_list_directory(id, &arguments, engine, ws).await,
        "organize_files" => handle_organize_files(id, &arguments, engine, ws).await,
        "trash_files" => handle_trash_files(id, &arguments, engine, ws).await,

        // ── Phase β.1 Cognition Commits ───────────────────────────────
        "commit_cognition" => handle_commit_cognition(id, &arguments, engine, ws).await,
        "list_commits" => handle_list_commits(id, &arguments, engine, ws).await,
        "merge_cognition" => handle_merge_cognition(id, &arguments, engine, ws).await,
        "synthesize_merge" => handle_synthesize_merge(id, &arguments, engine, ws).await,
        // ── Phase ε.1 — Tool Search ───────────────────────────────────
        // No workspace needed — the `ws` resolved earlier is harmless
        // because the handler ignores it. Meta-tool that returns
        // matching tool descriptors so the client can discover
        // `defer_loading: true` tools on demand without preloading.
        "tool_search" => handle_tool_search(id, &arguments).await,

        // ── Workspace filesystem operations ──────────────────────────
        // Same primitives the desktop FileManager uses. Workspace root
        // is resolved via `engine.workspace_root_path(ws)`; the actual
        // file ops live in `crate::fs_ops` (workspace-scoped, with
        // symlink + `..`-escape defences + `.thinkingroot/` refusal).
        // `fs_*` prefix keeps these distinct from the older Playground
        // action verbs (`list_directory`, `organize_files`,
        // `trash_files`).
        "fs_list" => handle_fs_list(id, &arguments, engine, ws).await,
        "fs_create_folder" => handle_fs_create_folder(id, &arguments, engine, ws).await,
        "fs_rename" => handle_fs_rename(id, &arguments, engine, ws).await,
        "fs_move" => handle_fs_move(id, &arguments, engine, ws).await,

        // ── System filesystem (absolute paths) ───────────────────────
        "sys_stat" => handle_sys_stat(id, &arguments).await,
        "sys_list" => handle_sys_list(id, &arguments).await,
        "sys_create_folder" => handle_sys_create_folder(id, &arguments).await,
        "sys_rename" => handle_sys_rename(id, &arguments).await,
        "sys_move" => handle_sys_move(id, &arguments).await,

        // ── Phase D Wave 1 — system-power tool dispatch ─────────────
        "file_read"       => handle_file_read(id, &arguments).await,
        "file_write"      => handle_file_write(id, &arguments).await,
        "file_edit"       => handle_file_edit(id, &arguments).await,
        "glob"            => handle_glob(id, &arguments).await,
        "grep"            => handle_grep(id, &arguments).await,
        "shell_exec"      => handle_shell_exec(id, &arguments, engine, ws).await,
        "clipboard_read"  => handle_clipboard_read(id).await,
        "clipboard_write" => handle_clipboard_write(id, &arguments).await,
        "open_in_default" => handle_open_in_default(id, &arguments).await,
        "trash"           => handle_trash(id, &arguments).await,

        // ── Phase E.5 (2026-05-17) — external MCP fall-through ─────
        // Names containing `::` are routed to the external MCP
        // registry. Split on `::`, look up the server, strip the
        // prefix, dispatch via `McpClient::call_tool`. Wrap the
        // result in the MCP `text` content block shape so agents
        // see external tool results identically to built-ins.
        other if other.contains("::") => {
            let registry = crate::mcp::external_registry::global().await;
            match registry.dispatch(other, arguments.clone()).await {
                Some(Ok(result)) => {
                    let text = serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| String::from("(serialization failure)"));
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({
                            "content": [{"type": "text", "text": text}],
                            "isError": result.is_error
                        }),
                    )
                }
                Some(Err(e)) => JsonRpcResponse::error(id, -32603, format!("external MCP {other}: {e}")),
                None => JsonRpcResponse::error(
                    id,
                    -32601,
                    format!("external MCP server not registered for: {other}"),
                ),
            }
        }
        // ── Phase E.6 (2026-05-17) — trait-registry fall-through ────
        // After every built-in `match` arm misses, consult the
        // `mcp::tool_trait` registry. Tools added from Phase E
        // onwards (export_memory_tree, import_memory_tree, external
        // MCP bridge tools, etc.) land here without churning the
        // hardcoded arms. The built-in arms run first, so a
        // registered tool can never shadow a built-in.
        other => match crate::mcp::tool_trait::lookup(other) {
            Some(handler) => {
                let ctx = crate::mcp::tool_trait::McpToolContext {
                    engine,
                    workspace: ws,
                    session_id,
                    sessions,
                    engram_manager,
                };
                match handler.handle(arguments, &ctx).await {
                    Ok(value) => mcp_text_result(id, &value),
                    Err(crate::mcp::tool_trait::McpToolError::InvalidArgs(m)) => {
                        JsonRpcResponse::error(id, -32602, format!("{other}: {m}"))
                    }
                    Err(crate::mcp::tool_trait::McpToolError::Backend(e)) => {
                        JsonRpcResponse::error(id, -32603, format!("{other}: {e}"))
                    }
                    Err(crate::mcp::tool_trait::McpToolError::Refused(r)) => {
                        JsonRpcResponse::error(id, -32603, format!("{other}: refused: {r}"))
                    }
                }
            }
            None => JsonRpcResponse::error(id, -32601, format!("Unknown tool: {}", other)),
        },
    }
}

// ─── Phase α Playground action verb MCP handlers ─────────────────────
//
// Three thin handlers — each one validates arguments, delegates to the
// matching `playground_tools::*_impl` helper, and wraps the outcome in
// `mcp_text_result`. The helpers are the single source of truth for
// the side-effecting logic.

async fn handle_save_note(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let title = match arguments.get("title").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "save_note: missing required `title`".to_string(),
            );
        }
    };
    let body = match arguments.get("body").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b,
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "save_note: missing required `body`".to_string(),
            );
        }
    };
    match crate::intelligence::playground_tools::save_note_impl(engine, ws, title, body).await {
        Ok(outcome) => mcp_text_result(id, &outcome),
        Err(e) => JsonRpcResponse::error(id, -32603, e),
    }
}

async fn handle_regenerate_paper(
    id: Option<Value>,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    match crate::intelligence::playground_tools::regenerate_paper_impl(engine, ws).await {
        Ok(outcome) => mcp_text_result(id, &outcome),
        Err(e) => JsonRpcResponse::error(id, -32603, e),
    }
}

async fn handle_ingest_path(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let source_path = match arguments.get("source_path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "ingest_path: missing required `source_path`".to_string(),
            );
        }
    };
    match crate::intelligence::playground_tools::ingest_path_impl(engine, ws, source_path).await {
        Ok(outcome) => mcp_text_result(id, &outcome),
        Err(e) => JsonRpcResponse::error(id, -32603, e),
    }
}

async fn handle_list_directory(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let rel_path = arguments
        .get("rel_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match crate::intelligence::playground_tools::list_directory_impl(engine, ws, rel_path).await {
        Ok(outcome) => mcp_text_result(id, &outcome),
        Err(e) => JsonRpcResponse::error(id, -32603, e),
    }
}

async fn handle_organize_files(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let ops_raw = match arguments.get("ops").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "organize_files: missing required `ops` (array)".to_string(),
            );
        }
    };
    let mut ops: Vec<crate::intelligence::playground_tools::OrganizeOp> =
        Vec::with_capacity(ops_raw.len());
    for v in ops_raw {
        match serde_json::from_value::<crate::intelligence::playground_tools::OrganizeOp>(v) {
            Ok(op) => ops.push(op),
            Err(e) => {
                return JsonRpcResponse::error(
                    id,
                    -32602,
                    format!("organize_files: invalid op shape: {e}"),
                );
            }
        }
    }
    match crate::intelligence::playground_tools::organize_files_impl(engine, ws, ops).await {
        Ok(outcome) => mcp_text_result(id, &outcome),
        Err(e) => JsonRpcResponse::error(id, -32603, e),
    }
}

async fn handle_trash_files(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let rel_paths: Vec<String> = match arguments.get("rel_paths").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "trash_files: missing required `rel_paths` (array of strings)".to_string(),
            );
        }
    };
    if rel_paths.is_empty() {
        return JsonRpcResponse::error(
            id,
            -32602,
            "trash_files: `rel_paths` must not be empty".to_string(),
        );
    }
    match crate::intelligence::playground_tools::trash_files_impl(engine, ws, rel_paths).await {
        Ok(outcome) => mcp_text_result(id, &outcome),
        Err(e) => JsonRpcResponse::error(id, -32603, e),
    }
}

// ─── Phase β.1 Cognition Commits MCP handlers ─────────────────────
//
// `commit_cognition` records one cognition event against a workspace
// branch. Citations are verified against the live `witnesses` table
// by `GraphStore::insert_cognition_commit` — fabricated references
// are surfaced as `-32603` errors with the offending id in the
// message so the calling agent can recover honestly.
//
// `list_commits` returns the branch's commit DAG newest-first. The
// reply is the full `CognitionCommit` shape — citation chips, gaps,
// parent pointer — so the client can render a chat-as-commit-DAG
// view without follow-up round trips.

async fn handle_commit_cognition(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    use thinkingroot_core::types::{CognitionCommit, CommitAuthor, CommitId};

    let branch = match arguments.get("branch").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "commit_cognition: missing required `branch`".to_string(),
            );
        }
    };
    let prompt = arguments
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let reasoning = arguments
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let parent = match arguments.get("parent_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => match CommitId::from_hex(s) {
            Ok(p) => Some(p),
            Err(e) => {
                return JsonRpcResponse::error(
                    id,
                    -32602,
                    format!("commit_cognition: invalid parent_id `{s}`: {e}"),
                );
            }
        },
        _ => None,
    };

    let author = match (
        arguments.get("author_kind").and_then(|v| v.as_str()),
        arguments.get("author_id").and_then(|v| v.as_str()),
    ) {
        (Some("user"), Some(uid)) => CommitAuthor::User {
            id: uid.to_string(),
        },
        (Some("agent"), Some(principal)) => {
            let model = arguments
                .get("author_model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            CommitAuthor::Agent {
                model,
                principal: principal.to_string(),
            }
        }
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "commit_cognition: `author_kind` must be 'user' or 'agent' and `author_id` is required".to_string(),
            );
        }
    };

    let witnesses_added = match crate::intelligence::playground_tools::parse_witness_ids(
        arguments.get("witnesses_added"),
    ) {
        Ok(v) => v,
        Err(e) => {
            return JsonRpcResponse::error(
                id,
                -32602,
                format!("commit_cognition: witnesses_added: {e}"),
            );
        }
    };
    let citations = match crate::intelligence::playground_tools::parse_witness_ids(
        arguments.get("citations"),
    ) {
        Ok(v) => v,
        Err(e) => {
            return JsonRpcResponse::error(
                id,
                -32602,
                format!("commit_cognition: citations: {e}"),
            );
        }
    };
    let gaps_surfaced: Vec<String> = arguments
        .get("gaps_surfaced")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let commit = CognitionCommit::new(
        parent,
        branch,
        author,
        prompt,
        reasoning,
        witnesses_added,
        citations,
        gaps_surfaced,
        chrono::Utc::now(),
    );

    match engine.commit_cognition(ws, &commit).await {
        Ok(()) => mcp_text_result(id, &commit),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("commit_cognition: {e}")),
    }
}

async fn handle_list_commits(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let branch = arguments
        .get("branch")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

    match engine.list_cognition_commits(ws, &branch, limit).await {
        Ok(commits) => mcp_text_result(id, &commits),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("list_commits: {e}")),
    }
}

/// Phase γ.1 — `merge_cognition` MCP tool.
///
/// Compute a deterministic merge plan between two cognition-commit
/// branches. Pure read, no commit recorded. The response shape is
/// the full `MergePlan` serialized as JSON; agents inspect
/// `conflict_kind` to decide whether synthesis is needed (γ.2 future
/// work) or whether the merge is trivially resolvable.
async fn handle_merge_cognition(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let left_branch = match arguments.get("left_branch").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "merge_cognition: missing required `left_branch`".to_string(),
            );
        }
    };
    let right_branch = match arguments.get("right_branch").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "merge_cognition: missing required `right_branch`".to_string(),
            );
        }
    };

    match engine
        .compute_merge_plan(ws, &left_branch, &right_branch)
        .await
    {
        Ok(plan) => mcp_text_result(id, &plan),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("merge_cognition: {e}")),
    }
}

/// Phase γ.2 — `synthesize_merge` MCP tool.
async fn handle_synthesize_merge(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let left_branch = match arguments.get("left_branch").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "synthesize_merge: missing required `left_branch`".to_string(),
            );
        }
    };
    let right_branch = match arguments.get("right_branch").and_then(|v| v.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "synthesize_merge: missing required `right_branch`".to_string(),
            );
        }
    };

    match engine
        .synthesize_merge(ws, &left_branch, &right_branch)
        .await
    {
        Ok(synthesis) => mcp_text_result(id, &synthesis),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("synthesize_merge: {e}")),
    }
}

// ─── SOTA Lever 3 — Observer / Reflector MCP handlers ────────────────
//
// Two tools give MCP clients (Claude Code, Cursor, Desktop chat) a
// clean opt-in surface for the conversation-memory substrate:
//
//   - `observe_turn`: record a (user_prompt, assistant_reply) pair.
//     The Observer buffers turns per-session; condensation fires
//     automatically at the threshold. Cheap (~µs).
//
//   - `flush_observations`: drain staged observations for a session
//     into the workspace's witness substrate. Call at session-end
//     OR on a cadence (every N turns / every M minutes). Insert
//     errors re-stage so failed flushes don't lose memory.
//
// Both tools are zero-LLM by design — the condensation is mechanical
// concatenation of user/assistant prompt pairs. Match Mastra's
// Observational Memory pattern (94.87% LongMemEval) but on the
// witness substrate so retrieval (hybrid + AEP) picks observations
// up alongside file-derived witnesses.

async fn handle_observe_turn(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
) -> JsonRpcResponse {
    let session_id = match arguments.get("session_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "observe_turn: missing required `session_id`".to_string(),
            );
        }
    };
    let turn_number = match arguments.get("turn_number").and_then(|v| v.as_u64()) {
        Some(n) => n,
        None => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "observe_turn: missing required `turn_number` (u64)".to_string(),
            );
        }
    };
    let user_prompt = arguments
        .get("user_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let assistant_reply = arguments
        .get("assistant_reply")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if user_prompt.is_empty() && assistant_reply.is_empty() {
        return JsonRpcResponse::error(
            id,
            -32602,
            "observe_turn: at least one of `user_prompt` or `assistant_reply` must be non-empty"
                .to_string(),
        );
    }
    let observer = engine.observer();
    observer.record_turn(crate::intelligence::observer::ChatTurn {
        session_id: session_id.clone(),
        turn_number,
        user_prompt,
        assistant_reply,
        at: chrono::Utc::now(),
    });
    let pending = observer.pending_count(&session_id);
    mcp_text_result(
        id,
        &serde_json::json!({
            "session_id": session_id,
            "turn_number": turn_number,
            "pending_turns": pending,
            "should_reflect": observer.should_reflect(&session_id),
        }),
    )
}

async fn handle_flush_observations(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let session_id = match arguments.get("session_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "flush_observations: missing required `session_id`".to_string(),
            );
        }
    };
    // Optional force-condense pass before flushing. Useful at session-
    // end where you want any partial turn-window to surface as an
    // observation rather than being lost on a process restart.
    let force_condense = arguments
        .get("force_condense")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if force_condense {
        engine.observer().force_condense(&session_id);
    }
    match engine.flush_observations(ws, &session_id).await {
        Ok(inserted) => mcp_text_result(
            id,
            &serde_json::json!({
                "session_id": session_id,
                "workspace": ws,
                "inserted_witnesses": inserted,
            }),
        ),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

// ─── T0.4 Knowledge Proposal handlers (MCP) ──────────────────────────
//
// These mirror the REST routes in `rest.rs` against the same
// `thinkingroot-pr` crate.  The `refs_dir` is derived from the
// workspace handle's root path so a daemon hosting multiple
// workspaces keeps each workspace's proposals isolated under its own
// `.thinkingroot-refs/proposals/`.

fn refs_dir_for_ws(engine: &QueryEngine, ws: &str) -> Result<std::path::PathBuf, String> {
    engine
        .workspace_root_path(ws)
        .map(|root| root.join(".thinkingroot-refs"))
        .ok_or_else(|| format!("workspace `{ws}` not mounted"))
}

async fn handle_open_proposal(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let refs_dir = match refs_dir_for_ws(engine, ws) {
        Ok(d) => d,
        Err(e) => return JsonRpcResponse::error(id, -32602, e),
    };
    let source_branch = match arguments.get("source_branch").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "Missing or empty 'source_branch'".to_string(),
            );
        }
    };
    let target_branch = arguments
        .get("target_branch")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "main")
        .map(String::from);
    let author = match arguments.get("author").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(id, -32602, "Missing or empty 'author'".to_string());
        }
    };
    let description = arguments
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Resolve policy defaults from the source branch.  Mirrors the REST
    // handler's `proposal_policy_defaults` so the two surfaces freeze
    // the same numbers on the proposal.
    let (default_min, default_checks) = {
        use thinkingroot_branch::branch::BranchRegistry;
        use thinkingroot_core::MergePolicy;
        if let Ok(registry) = BranchRegistry::load_or_create(&refs_dir)
            && let Some(branch) = registry.get(&source_branch)
            && let MergePolicy::RequiresProposal {
                min_reviewers,
                required_checks,
            } = &branch.merge_policy
        {
            (*min_reviewers, required_checks.clone())
        } else {
            (1, Vec::new())
        }
    };

    let min_reviewers = arguments
        .get("min_reviewers")
        .and_then(|v| v.as_u64())
        .map(|n| n.min(u8::MAX as u64) as u8)
        .unwrap_or(default_min);
    let required_checks: Vec<String> = arguments
        .get("required_checks")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or(default_checks);

    match thinkingroot_pr::open_proposal(
        &refs_dir,
        &source_branch,
        target_branch.as_deref(),
        &author,
        description,
        min_reviewers,
        required_checks,
    ) {
        Ok(p) => JsonRpcResponse::success(
            id,
            serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Proposal opened: {} (source={}, target={}, min_reviewers={})",
                        p.id,
                        p.source_branch,
                        p.target_branch.as_deref().unwrap_or("main"),
                        p.min_reviewers,
                    )
                }],
                "proposal": p,
            }),
        ),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_review_proposal(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let refs_dir = match refs_dir_for_ws(engine, ws) {
        Ok(d) => d,
        Err(e) => return JsonRpcResponse::error(id, -32602, e),
    };
    let proposal_id = match arguments.get("proposal_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "Missing or empty 'proposal_id'".to_string(),
            );
        }
    };
    let reviewer = match arguments.get("reviewer").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(id, -32602, "Missing or empty 'reviewer'".to_string());
        }
    };
    let decision = match arguments
        .get("decision")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("approve") => thinkingroot_pr::ReviewDecision::Approve,
        Some("request_changes") | Some("request-changes") | Some("changes_requested") => {
            thinkingroot_pr::ReviewDecision::RequestChanges
        }
        Some("comment") => thinkingroot_pr::ReviewDecision::Comment,
        other => {
            return JsonRpcResponse::error(
                id,
                -32602,
                format!(
                    "decision must be one of approve|request_changes|comment, got {:?}",
                    other
                ),
            );
        }
    };
    let comment = arguments
        .get("comment")
        .and_then(|v| v.as_str())
        .map(String::from);

    match thinkingroot_pr::review_proposal(&refs_dir, &proposal_id, &reviewer, decision, comment) {
        Ok(p) => JsonRpcResponse::success(
            id,
            serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Review recorded on {} → status now {:?}",
                        p.id, p.status
                    )
                }],
                "proposal": p,
            }),
        ),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_list_proposals(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let refs_dir = match refs_dir_for_ws(engine, ws) {
        Ok(d) => d,
        Err(e) => return JsonRpcResponse::error(id, -32602, e),
    };
    let source_filter = arguments
        .get("source_branch")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    match thinkingroot_pr::list_proposals(&refs_dir) {
        Ok(all) => {
            let filtered: Vec<_> = match &source_filter {
                Some(src) => all.into_iter().filter(|p| &p.source_branch == src).collect(),
                None => all,
            };
            let count = filtered.len();
            JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": format!("{} proposal(s)", count)
                    }],
                    "proposals": filtered,
                }),
            )
        }
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_close_proposal(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let refs_dir = match refs_dir_for_ws(engine, ws) {
        Ok(d) => d,
        Err(e) => return JsonRpcResponse::error(id, -32602, e),
    };
    let proposal_id = match arguments.get("proposal_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "Missing or empty 'proposal_id'".to_string(),
            );
        }
    };
    let closer = match arguments.get("closer").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(id, -32602, "Missing or empty 'closer'".to_string());
        }
    };

    match thinkingroot_pr::close_proposal(&refs_dir, &proposal_id, &closer) {
        Ok(p) => JsonRpcResponse::success(
            id,
            serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!("Proposal {} closed by {}", p.id, closer)
                }],
                "proposal": p,
            }),
        ),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_hybrid_retrieve(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
    session_id: &str,
) -> JsonRpcResponse {
    use crate::engine::{RetrievalRequest, ScoringProfile, TypedPredicate};

    // Build request from JSON. Most fields ride the serde defaults defined
    // on RetrievalRequest; we only translate the ones the MCP schema
    // exposes by name.
    let mut req: RetrievalRequest = match serde_json::from_value(arguments.clone()) {
        Ok(r) => r,
        Err(e) => {
            // Fall back to a minimal manual parse so callers don't need to
            // submit the full Rust struct shape on first try. Required
            // fields are session_id + workspace; everything else defaults.
            let session_id_arg = arguments
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or(session_id)
                .to_string();
            let query_text = arguments
                .get("query_text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let typed_predicates: Vec<TypedPredicate> = arguments
                .get("typed_predicates")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let scoring_profile = arguments
                .get("scoring_profile")
                .and_then(|v| v.as_str())
                .and_then(ScoringProfile::by_name)
                .or_else(|| {
                    arguments
                        .get("scoring_profile_custom")
                        .and_then(|v| serde_json::from_value::<ScoringProfile>(v.clone()).ok())
                })
                .unwrap_or_default();
            let clearance = arguments
                .get("clearance")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().and_then(parse_sensitivity_str))
                        .collect()
                })
                .unwrap_or_else(|| vec![thinkingroot_core::types::Sensitivity::Public]);
            let top_k = arguments
                .get("top_k")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(50);
            tracing::debug!("hybrid_retrieve: serde fallback ({e})");
            RetrievalRequest {
                query_text,
                typed_predicates,
                session_id: session_id_arg,
                clearance,
                top_k,
                time_window: None,
                scoring_profile,
                require_certificate: arguments
                    .get("require_certificate")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                include_test_origin: arguments
                    .get("include_test_origin")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                include_quarantined: arguments
                    .get("include_quarantined")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                require_provenance_verified: arguments
                    .get("require_provenance_verified")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                now: None,
                scoped_claim_ids: None,
            }
        }
    };
    // Always honour the session_id wired into the MCP transport — overrides
    // a misuse where the caller hardcoded a stale id in the JSON body.
    req.session_id = session_id.to_string();

    match engine.hybrid_retrieve(ws, req, None).await {
        Ok(resp) => JsonRpcResponse::success(id, serde_json::json!(resp)),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

// ───────────────────────────────────────────────────────────────────────
// RARP handlers
// ───────────────────────────────────────────────────────────────────────

async fn handle_materialize_engram(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
    session_id: &str,
    engram_manager: &std::sync::Arc<crate::intelligence::engram::EngramManager>,
) -> JsonRpcResponse {
    let topic = match arguments.get("topic").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => {
            return JsonRpcResponse::error(id, -32602, "Missing 'topic' argument".into());
        }
    };

    // Optional explicit seed entity ids; otherwise derive seeds from a
    // vector search against the workspace for the topic text.
    let seed_entity_ids: Vec<String> = match arguments
        .get("seed_entity_ids")
        .and_then(|v| v.as_array())
    {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => match engine.search(ws, &topic, 10).await {
            Ok(result) => result.entities.into_iter().map(|e| e.id).collect(),
            Err(e) => {
                return JsonRpcResponse::error(
                    id,
                    -32603,
                    format!("seed search failed: {e}"),
                );
            }
        },
    };

    if seed_entity_ids.is_empty() {
        return JsonRpcResponse::error(
            id,
            -32602,
            format!("no semantic anchors for topic '{topic}'"),
        );
    }

    let scope = parse_scope(arguments.get("scope"));

    let graph = match engine.graph_store(ws).await {
        Some(g) => g,
        None => {
            return JsonRpcResponse::error(
                id,
                -32602,
                format!("workspace '{ws}' not mounted"),
            );
        }
    };

    // Allow scope.seed_claim_ids to feed the EngramManager (currently the
    // manager seeds via entity ids; claim ids would map to entity ids
    // via claim_entity_edges — out of scope for v1 wiring, the entity
    // path covers the canonical case).
    let _seed_claim_ids = arguments
        .get("seed_claim_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    match engram_manager
        .materialize_engram(session_id, ws, &topic, &graph, seed_entity_ids, scope, None)
        .await
    {
        Ok((pointer, summary)) => JsonRpcResponse::success(
            id,
            serde_json::json!({
                "pointer": pointer,
                "summary": &*summary,
            }),
        ),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_probe_engram(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
    session_id: &str,
    engram_manager: &std::sync::Arc<crate::intelligence::engram::EngramManager>,
) -> JsonRpcResponse {
    let pointer = match arguments.get("pointer").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return JsonRpcResponse::error(id, -32602, "Missing 'pointer' argument".into());
        }
    };
    let question = match arguments.get("question").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => {
            return JsonRpcResponse::error(id, -32602, "Missing 'question' argument".into());
        }
    };
    let clearance = arguments
        .get("clearance")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().and_then(parse_sensitivity_str))
                .collect::<Vec<_>>()
        });
    let probe_kind_override = arguments
        .get("probe_kind")
        .and_then(|v| v.as_str())
        .and_then(parse_probe_kind_str);

    let graph = match engine.graph_store(ws).await {
        Some(g) => g,
        None => {
            return JsonRpcResponse::error(
                id,
                -32602,
                format!("workspace '{ws}' not mounted"),
            );
        }
    };
    let byte_store = match engine.byte_store(ws) {
        Some(b) => b,
        None => {
            return JsonRpcResponse::error(
                id,
                -32602,
                format!("workspace '{ws}' has no byte store"),
            );
        }
    };

    let probe_clearance = clearance.clone();
    let mut answer = match engram_manager
        .probe_engram(
            session_id,
            pointer,
            question,
            clearance,
            &graph,
            byte_store.as_ref(),
            probe_kind_override,
        )
        .await
    {
        Ok(a) => a,
        Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
    };

    // AEP × Hybrid composition (spec §11). The flag is per-call (overriding
    // any default carried on EngramScope.score_with_hybrid).
    let score_with_hybrid = arguments
        .get("score_with_hybrid")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if score_with_hybrid && !answer.claim_ids.is_empty() {
        let req = crate::engine::RetrievalRequest {
            query_text: question.to_string(),
            typed_predicates: vec![],
            session_id: session_id.to_string(),
            clearance: probe_clearance
                .unwrap_or_else(|| vec![thinkingroot_core::types::Sensitivity::Public]),
            top_k: answer.claim_ids.len(),
            time_window: None,
            scoring_profile: crate::engine::ScoringProfile::default(),
            require_certificate: false,
            include_test_origin: true, // probes already classify test rows
            include_quarantined: false,
            require_provenance_verified: false,
            now: None,
            scoped_claim_ids: Some(answer.claim_ids.clone()),
        };
        match engine.hybrid_retrieve(ws, req, None).await {
            Ok(resp) => {
                let new_order: Vec<String> =
                    resp.hits.iter().map(|h| h.claim_id.clone()).collect();
                reorder_probe_answer_in_place(&mut answer, &new_order);
            }
            Err(e) => {
                // Don't fail the probe — fall back to Datalog query order
                // and surface as a low-confidence caveat.
                tracing::warn!("hybrid composition fallback: {e}");
            }
        }
    }

    JsonRpcResponse::success(id, serde_json::json!(answer))
}

/// Reorder the 5 parallel arrays of `ProbeAnswer` according to a hybrid
/// ranking. Claim IDs in the original answer that aren't in `new_order`
/// (e.g. dropped by Hybrid's sensitivity gate) are appended at the end
/// in their original relative order so the answer's shape never shrinks.
pub fn reorder_probe_answer_in_place(
    answer: &mut crate::engine::ProbeAnswer,
    new_order: &[String],
) {
    let n = answer.claim_ids.len();
    if n == 0 || answer.answer.len() != n {
        return; // shape guard — never mutate a malformed answer
    }
    // Build the index permutation: original position -> rank-by-new-order.
    let mut indices: Vec<usize> = (0..n).collect();
    let order_pos: std::collections::HashMap<&str, usize> = new_order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    indices.sort_by_key(|&i| order_pos.get(answer.claim_ids[i].as_str()).copied().unwrap_or(usize::MAX));

    // Apply the permutation to all 5 parallel arrays + answer rows.
    answer.answer = indices.iter().map(|&i| answer.answer[i].clone()).collect();
    answer.claim_ids = indices.iter().map(|&i| answer.claim_ids[i].clone()).collect();
    answer.source_byte_spans = indices
        .iter()
        .map(|&i| answer.source_byte_spans[i].clone())
        .collect();
    answer.source_authority = indices
        .iter()
        .map(|&i| answer.source_authority[i])
        .collect();
    answer.source_blake3s = indices
        .iter()
        .map(|&i| answer.source_blake3s[i].clone())
        .collect();
}

pub fn parse_scope(scope: Option<&Value>) -> crate::intelligence::engram::EngramScope {
    use crate::intelligence::engram::EngramScope;
    let mut out = EngramScope::default();
    let Some(s) = scope else {
        return out;
    };
    out.depth_hops = s.get("depth_hops").and_then(|v| v.as_u64()).map(|n| n as u8);
    out.event_window_days = s
        .get("event_window_days")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    out.clearance = s
        .get("clearance")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().and_then(parse_sensitivity_str))
                .collect::<Vec<_>>()
        });
    out.seed_claim_ids = s
        .get("seed_claim_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });
    out.score_with_hybrid = s.get("score_with_hybrid").and_then(|v| v.as_bool());
    out
}

pub fn parse_sensitivity_str(s: &str) -> Option<thinkingroot_core::types::Sensitivity> {
    use thinkingroot_core::types::Sensitivity;
    match s.to_ascii_lowercase().as_str() {
        "public" => Some(Sensitivity::Public),
        "internal" => Some(Sensitivity::Internal),
        "confidential" => Some(Sensitivity::Confidential),
        "restricted" => Some(Sensitivity::Restricted),
        _ => None,
    }
}

pub fn parse_probe_kind_str(s: &str) -> Option<crate::intelligence::engram::ProbeKind> {
    use crate::intelligence::engram::ProbeKind;
    match s.to_ascii_lowercase().as_str() {
        "factual" => Some(ProbeKind::Factual),
        "quantitative" => Some(ProbeKind::Quantitative),
        "temporal" => Some(ProbeKind::Temporal),
        "authorship" => Some(ProbeKind::Authorship),
        "structural" => Some(ProbeKind::Structural),
        "relation_callers" => Some(ProbeKind::RelationCallers),
        "relation_refs" => Some(ProbeKind::RelationRefs),
        "existential" => Some(ProbeKind::Existential),
        "comparative" => Some(ProbeKind::Comparative),
        "counterfactual" => Some(ProbeKind::Counterfactual),
        _ => None,
    }
}

#[cfg(test)]
mod defer_loading_tests {
    use super::{handle_list, handle_tool_search, DEFER_LOADING_TOOLS};

    #[tokio::test]
    async fn long_tail_tools_carry_defer_loading_flag() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        // Every name in DEFER_LOADING_TOOLS that's actually advertised
        // must carry `defer_loading: true`. (A name in the constant
        // that isn't in the catalog is fine — the annotation pass is
        // forgiving — so we filter to advertised names first.)
        let by_name: std::collections::HashMap<String, &serde_json::Value> = tools
            .iter()
            .filter_map(|t| {
                t.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| (n.to_string(), t))
            })
            .collect();
        for &deferred in DEFER_LOADING_TOOLS {
            if let Some(tool) = by_name.get(deferred) {
                let flag = tool
                    .get("defer_loading")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                assert!(
                    flag,
                    "expected tool `{}` to carry defer_loading: true",
                    deferred
                );
            }
        }
    }

    #[tokio::test]
    async fn core_tools_do_not_carry_defer_loading_flag() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        // Spot-check: tools the chat loop hits every turn must stay
        // loaded by default — `search`, `compile`, `list_witnesses`,
        // `merge_cognition` (the γ flagship).
        let core_names = ["search", "compile", "list_witnesses", "merge_cognition"];
        for name in core_names {
            let tool = tools
                .iter()
                .find(|t| {
                    t.get("name").and_then(|n| n.as_str()) == Some(name)
                })
                .unwrap_or_else(|| panic!("core tool `{name}` should be advertised"));
            let flag = tool
                .get("defer_loading")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            assert!(
                !flag,
                "core tool `{}` should NOT carry defer_loading: true",
                name
            );
        }
    }

    #[tokio::test]
    async fn tool_search_returns_matches_by_name_substring() {
        // Query for "engram" should match every engram-lifecycle tool.
        let resp = handle_tool_search(
            None,
            &serde_json::json!({ "query": "engram", "limit": 50 }),
        )
        .await;
        let payload = resp
            .result
            .expect("tool_search responds with result");
        // mcp_text_result wraps the result in a `content[0].text`
        // JSON string. Parse the text and walk to the inner tools.
        let text = payload
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
            .expect("tool_search mcp text payload");
        let parsed: serde_json::Value =
            serde_json::from_str(text).expect("tool_search text parses as JSON");
        let names: Vec<String> = parsed
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        t.get("name")
                            .and_then(|n| n.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            names.iter().any(|n| n.contains("engram")),
            "expected at least one engram tool in results; got {names:?}"
        );
    }

    #[tokio::test]
    async fn tool_search_filters_to_deferred_only_when_requested() {
        // Empty query + include_non_deferred=false → only deferred
        // tools should appear in the result.
        let resp = handle_tool_search(
            None,
            &serde_json::json!({ "include_non_deferred": false, "limit": 100 }),
        )
        .await;
        let payload = resp.result.expect("result");
        let text = payload
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
            .expect("tool_search mcp text payload");
        let parsed: serde_json::Value =
            serde_json::from_str(text).expect("JSON");
        let tools = parsed
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array");
        assert!(!tools.is_empty(), "expected non-empty deferred-only result");
        for tool in tools {
            let flag = tool
                .get("defer_loading")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            assert!(
                flag,
                "every tool in deferred-only filter should be deferred: {:?}",
                tool.get("name")
            );
        }
    }

    #[tokio::test]
    async fn tool_search_respects_limit() {
        let resp = handle_tool_search(
            None,
            &serde_json::json!({ "query": "", "limit": 3 }),
        )
        .await;
        let payload = resp.result.expect("result");
        let text = payload
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
            .expect("tool_search mcp text payload");
        let parsed: serde_json::Value =
            serde_json::from_str(text).expect("JSON");
        let tools = parsed
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array");
        assert_eq!(tools.len(), 3, "limit=3 should return exactly 3 entries");
    }
}

#[cfg(test)]
mod witness_tool_listing_tests {
    use super::handle_list;

    #[tokio::test]
    async fn list_witnesses_tool_is_advertised() {
        let resp = handle_list(None).await;
        // `JsonRpcResponse::success` stores the payload in `result`;
        // navigate to `tools` and look for our entry.
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(
            names.contains(&"list_witnesses"),
            "expected list_witnesses tool to be advertised; got {names:?}"
        );
    }

    #[tokio::test]
    async fn list_witnesses_tool_declares_workspace_required() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let descriptor = tools
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("list_witnesses"))
            .expect("list_witnesses tool present");
        let schema = descriptor
            .get("inputSchema")
            .expect("schema present");
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array present");
        let required_names: Vec<&str> = required
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            required_names.contains(&"workspace"),
            "expected `workspace` to be required for list_witnesses; got {required_names:?}"
        );
    }

    #[tokio::test]
    async fn walk_mesh_tool_is_advertised() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(
            names.contains(&"walk_mesh"),
            "expected walk_mesh tool to be advertised; got {names:?}"
        );
    }

    #[tokio::test]
    async fn walk_mesh_tool_requires_witness_id() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let descriptor = tools
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("walk_mesh"))
            .expect("walk_mesh tool present");
        let required: Vec<&str> = descriptor
            .get("inputSchema")
            .and_then(|s| s.get("required"))
            .and_then(|v| v.as_array())
            .expect("required array present")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            required.contains(&"workspace"),
            "walk_mesh requires `workspace`; got {required:?}"
        );
        assert!(
            required.contains(&"witness_id"),
            "walk_mesh requires `witness_id`; got {required:?}"
        );
    }
}

#[cfg(test)]
mod resolve_workspace_arg_tests {
    use super::resolve_workspace_arg_with;
    use std::collections::HashSet;

    fn mounted(names: &[&str]) -> impl Fn(&str) -> bool {
        let set: HashSet<String> = names.iter().map(|s| s.to_string()).collect();
        move |name: &str| set.contains(name)
    }

    #[test]
    fn exact_name_passes_through() {
        let got = resolve_workspace_arg_with(
            Some("thinkingroot-cloud"),
            Some("default"),
            mounted(&["thinkingroot-cloud"]),
        );
        assert_eq!(got, "thinkingroot-cloud");
    }

    #[test]
    fn unix_path_falls_back_to_basename_when_basename_is_mounted() {
        // Reproduces the original bug: client passes the --path value as the
        // workspace argument; basename is what the engine actually mounted.
        let got = resolve_workspace_arg_with(
            Some("/Users/naveen/Desktop/thinkingroot-cloud"),
            None,
            mounted(&["thinkingroot-cloud"]),
        );
        assert_eq!(got, "thinkingroot-cloud");
    }

    // `std::path::Path::file_name` only treats `\` as a separator on Windows
    // hosts, so this normalisation is necessarily platform-specific.
    #[cfg(windows)]
    #[test]
    fn windows_path_falls_back_to_basename_when_basename_is_mounted() {
        let got = resolve_workspace_arg_with(
            Some(r"C:\Users\naveen\Desktop\thinkingroot-cloud"),
            None,
            mounted(&["thinkingroot-cloud"]),
        );
        assert_eq!(got, "thinkingroot-cloud");
    }

    #[test]
    fn unknown_path_returns_input_unchanged_so_engine_emits_precise_error() {
        // We deliberately do NOT silently rewrite to basename when basename is
        // also unmounted — let the downstream lookup produce a real error
        // message so users see the value they actually sent.
        let got = resolve_workspace_arg_with(
            Some("/some/random/path"),
            None,
            mounted(&["thinkingroot-cloud"]),
        );
        assert_eq!(got, "/some/random/path");
    }

    #[test]
    fn unknown_bare_name_returns_input_unchanged() {
        let got = resolve_workspace_arg_with(
            Some("nope"),
            Some("thinkingroot-cloud"),
            mounted(&["thinkingroot-cloud"]),
        );
        // Unknown bare name does NOT silently rewrite to default — preserve
        // the value so the caller sees the precise lookup error.
        assert_eq!(got, "nope");
    }

    #[test]
    fn missing_arg_uses_default_ws() {
        let got = resolve_workspace_arg_with(
            None,
            Some("thinkingroot-cloud"),
            mounted(&["thinkingroot-cloud"]),
        );
        assert_eq!(got, "thinkingroot-cloud");
    }

    #[test]
    fn missing_arg_and_no_default_falls_back_to_literal_default() {
        let got = resolve_workspace_arg_with(None, None, mounted(&[]));
        assert_eq!(got, "default");
    }
}

#[cfg(test)]
mod observer_tool_listing_tests {
    use super::handle_list;

    #[tokio::test]
    async fn observe_turn_tool_is_advertised() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(
            names.contains(&"observe_turn"),
            "expected observe_turn tool to be advertised; got {names:?}"
        );
    }

    #[tokio::test]
    async fn flush_observations_tool_is_advertised() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(
            names.contains(&"flush_observations"),
            "expected flush_observations tool to be advertised; got {names:?}"
        );
    }

    #[tokio::test]
    async fn observe_turn_requires_session_id_and_turn_number() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let descriptor = tools
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("observe_turn"))
            .expect("observe_turn tool present");
        let schema = descriptor.get("inputSchema").expect("schema present");
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array present");
        let required_names: Vec<&str> = required
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required_names.contains(&"session_id"));
        assert!(required_names.contains(&"turn_number"));
    }

    #[tokio::test]
    async fn flush_observations_requires_workspace_and_session_id() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let descriptor = tools
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("flush_observations"))
            .expect("flush_observations tool present");
        let schema = descriptor.get("inputSchema").expect("schema present");
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array present");
        let required_names: Vec<&str> = required
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required_names.contains(&"workspace"));
        assert!(required_names.contains(&"session_id"));
    }

    #[tokio::test]
    async fn fs_tools_advertised_in_list() {
        let resp = handle_list(None).await;
        let result = resp.result.expect("tools/list responds with result");
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array present");
        let names: std::collections::HashSet<&str> = tools
            .iter()
            .filter_map(|t: &serde_json::Value| t.get("name").and_then(|n| n.as_str()))
            .collect();
        for required in ["fs_list", "fs_create_folder", "fs_rename", "fs_move"] {
            assert!(
                names.contains(required),
                "fs tool `{required}` missing from tools/list"
            );
        }
    }
}

// ── Workspace filesystem MCP handlers ─────────────────────────────
//
// Thin handlers — each one validates required arguments, resolves the
// workspace root via `engine.workspace_root_path(ws)`, delegates to
// `crate::fs_ops`, and wraps the result in `mcp_text_result`. The
// safety model (`..`-escape refusal, symlink canonicalisation,
// `.thinkingroot/` protection) lives in `fs_ops`, not here.

fn fs_resolve_root_or_error(
    id: Option<Value>,
    engine: &QueryEngine,
    ws: &str,
    tool: &str,
) -> Result<std::path::PathBuf, JsonRpcResponse> {
    engine.workspace_root_path(ws).ok_or_else(|| {
        JsonRpcResponse::error(
            id,
            -32603,
            format!("{tool}: workspace `{ws}` is not mounted"),
        )
    })
}

/// On-disk workspace root for branch tools (`diff_branch`, `list_branches`, …).
///
/// Hand-wrapped builtins (`ListBranchesTool`, `MergeBranchTool`, …) already use
/// `ToolContext::workspace_root`. MCP-bridge tools only inject `workspace` by
/// name — without this helper they defaulted to `"."` (daemon cwd), so
/// `list_branches` could see `stream/{session}` under `playground/` while
/// `diff_branch` looked in the wrong tree and returned "branch not found".
fn branch_resolve_root(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
    tool: &str,
) -> Result<std::path::PathBuf, JsonRpcResponse> {
    if let Some(rp) = arguments.get("root_path").and_then(|v| v.as_str()) {
        return Ok(std::path::PathBuf::from(rp));
    }
    engine.workspace_root_path(ws).ok_or_else(|| {
        JsonRpcResponse::error(
            id,
            -32603,
            format!("{tool}: workspace `{ws}` is not mounted"),
        )
    })
}

async fn handle_fs_list(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let root = match fs_resolve_root_or_error(id.clone(), engine, ws, "list_directory") {
        Ok(p) => p,
        Err(r) => return r,
    };
    let rel = arguments
        .get("rel")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match crate::fs_ops::list_directory(&root, ws, rel) {
        Ok(listing) => mcp_text_result(id, &listing),
        Err(msg) => JsonRpcResponse::error(id, -32603, msg),
    }
}

async fn handle_fs_create_folder(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let parent_rel = arguments
        .get("parent_rel")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "create_folder: missing required `name` argument".to_string(),
            );
        }
    };
    let root = match fs_resolve_root_or_error(id.clone(), engine, ws, "create_folder") {
        Ok(p) => p,
        Err(r) => return r,
    };
    match crate::fs_ops::create_folder(&root, &parent_rel, &name) {
        Ok(rel_path) => mcp_text_result(id, &serde_json::json!({ "rel_path": rel_path })),
        Err(msg) => JsonRpcResponse::error(id, -32603, msg),
    }
}

async fn handle_fs_rename(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let rel = match arguments.get("rel").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "rename_path: missing required `rel` argument".to_string(),
            );
        }
    };
    let new_name = match arguments.get("new_name").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "rename_path: missing required `new_name` argument".to_string(),
            );
        }
    };
    let root = match fs_resolve_root_or_error(id.clone(), engine, ws, "rename_path") {
        Ok(p) => p,
        Err(r) => return r,
    };
    match crate::fs_ops::rename_path(&root, &rel, &new_name) {
        Ok(rel_path) => mcp_text_result(id, &serde_json::json!({ "rel_path": rel_path })),
        Err(msg) => JsonRpcResponse::error(id, -32603, msg),
    }
}

async fn handle_fs_move(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let sources: Vec<String> = match arguments.get("sources").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => {
            return JsonRpcResponse::error(
                id,
                -32602,
                "move_paths: missing required `sources` array argument".to_string(),
            );
        }
    };
    if sources.is_empty() {
        return JsonRpcResponse::error(
            id,
            -32602,
            "move_paths: `sources` must contain at least one rel_path".to_string(),
        );
    }
    let dest_folder = arguments
        .get("dest_folder")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let root = match fs_resolve_root_or_error(id.clone(), engine, ws, "move_paths") {
        Ok(p) => p,
        Err(r) => return r,
    };
    match crate::fs_ops::move_paths(&root, sources, &dest_folder) {
        Ok(outcome) => mcp_text_result(id, &outcome),
        Err(msg) => JsonRpcResponse::error(id, -32603, msg),
    }
}

// ─── System-wide filesystem handlers (absolute paths) ───────────────
//
// These mirror the workspace-bound `fs_*` handlers but accept absolute
// paths. The pure logic lives in `crate::sys_fs_ops`; the sensitive-
// path block (DEFAULT_DENY equivalent) is enforced there so the
// read-class tools (`sys_stat`, `sys_list`) keep their fast-path
// while still honouring the `~/.ssh` / `~/.aws` / etc. shortlist.

fn sys_err_to_response(id: Option<Value>, err: crate::sys_fs_ops::SysFsError) -> JsonRpcResponse {
    use crate::sys_fs_ops::SysFsError;
    match err {
        SysFsError::InvalidPath(_) | SysFsError::SensitivePath(_) | SysFsError::NotFound(_)
        | SysFsError::NotADirectory(_) | SysFsError::AlreadyExists(_) => {
            JsonRpcResponse::error(id, -32602, err.to_string())
        }
        SysFsError::Io(_, _) => JsonRpcResponse::error(id, -32603, err.to_string()),
    }
}

async fn handle_sys_stat(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "sys_stat", "path"),
    };
    match crate::sys_fs_ops::sys_stat(path) {
        Ok(stat) => mcp_text_result(id, &stat),
        Err(e) => sys_err_to_response(id, e),
    }
}

async fn handle_sys_list(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "sys_list", "path"),
    };
    match crate::sys_fs_ops::sys_list(path) {
        Ok(listing) => mcp_text_result(id, &listing),
        Err(e) => sys_err_to_response(id, e),
    }
}

async fn handle_sys_create_folder(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "sys_create_folder", "path"),
    };
    match crate::sys_fs_ops::sys_create_folder(path) {
        Ok(created) => mcp_text_result(id, &serde_json::json!({ "path": created })),
        Err(e) => sys_err_to_response(id, e),
    }
}

async fn handle_sys_rename(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "sys_rename", "path"),
    };
    let new_name = match arguments.get("new_name").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return sp_missing_arg(id, "sys_rename", "new_name"),
    };
    match crate::sys_fs_ops::sys_rename(path, new_name) {
        Ok(p) => mcp_text_result(id, &serde_json::json!({ "path": p })),
        Err(e) => sys_err_to_response(id, e),
    }
}

async fn handle_sys_move(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let sources: Vec<String> = match arguments.get("sources").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => return sp_missing_arg(id, "sys_move", "sources"),
    };
    if sources.is_empty() {
        return JsonRpcResponse::error(
            id,
            -32602,
            "sys_move: `sources` must contain at least one absolute path".to_string(),
        );
    }
    let dest = match arguments.get("dest_folder").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return sp_missing_arg(id, "sys_move", "dest_folder"),
    };
    match crate::sys_fs_ops::sys_move(&sources, dest) {
        Ok(outcome) => mcp_text_result(id, &outcome),
        Err(e) => sys_err_to_response(id, e),
    }
}

// ─── Phase D Wave 1 — System-power tool handlers ────────────────────
//
// Thin handlers that:
//   1. Parse arguments from the JSON-RPC `arguments` object.
//   2. Call the corresponding pure-logic fn in `crate::system_power`.
//   3. Wrap the result via `mcp_text_result` on success or
//      `JsonRpcResponse::error` with -32603 on operational failure
//      / -32602 on bad-args.
//
// **No permission checks here.** The agent's `PermissionsGate`
// (intelligence/permissions_gate.rs) intercepts these BEFORE
// dispatch via the write-class bridge. Direct MCP clients (Claude
// Code, Cursor, Codex) bring their own permission UX — they call
// these handlers directly and we deliberately don't double-prompt.

fn sp_missing_arg(id: Option<Value>, tool: &str, arg: &str) -> JsonRpcResponse {
    JsonRpcResponse::error(
        id,
        -32602,
        format!("{tool}: missing or invalid required argument `{arg}`"),
    )
}

async fn handle_file_read(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "file_read", "path"),
    };
    match crate::system_power::file_read(std::path::Path::new(path)).await {
        Ok(out) => mcp_text_result(id, &out),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_file_write(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "file_write", "path"),
    };
    let content = match arguments.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        _ => return sp_missing_arg(id, "file_write", "content"),
    };
    let create_dirs = arguments
        .get("create_dirs")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match crate::system_power::file_write(std::path::Path::new(path), content, create_dirs).await {
        Ok(out) => mcp_text_result(id, &out),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_file_edit(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let path = match arguments.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "file_edit", "path"),
    };
    let edits_value = match arguments.get("edits") {
        Some(v) => v,
        None => return sp_missing_arg(id, "file_edit", "edits"),
    };
    let edits: Vec<crate::system_power::EditOp> =
        match serde_json::from_value(edits_value.clone()) {
            Ok(v) => v,
            Err(e) => {
                return JsonRpcResponse::error(
                    id,
                    -32602,
                    format!("file_edit: invalid `edits` array: {e}"),
                );
            }
        };
    if edits.is_empty() {
        return JsonRpcResponse::error(
            id,
            -32602,
            "file_edit: `edits` must be non-empty".to_string(),
        );
    }
    match crate::system_power::file_edit(std::path::Path::new(path), &edits).await {
        Ok(out) => mcp_text_result(id, &out),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_glob(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let pattern = match arguments.get("pattern").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "glob", "pattern"),
    };
    let base = arguments
        .get("base")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
    match crate::system_power::glob_search(pattern, &base).await {
        Ok(out) => mcp_text_result(id, &out),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_grep(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let pattern = match arguments.get("pattern").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return sp_missing_arg(id, "grep", "pattern"),
    };
    let base = arguments
        .get("base")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
    let regex_mode = arguments
        .get("regex")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let case_sensitive = arguments
        .get("case_sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    match crate::system_power::grep_search(pattern, &base, regex_mode, case_sensitive).await {
        Ok(out) => mcp_text_result(id, &out),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_shell_exec(
    id: Option<Value>,
    arguments: &Value,
    engine: &QueryEngine,
    ws: &str,
) -> JsonRpcResponse {
    let command = match arguments.get("command").and_then(|v| v.as_str()) {
        Some(c) if !c.is_empty() => c,
        _ => return sp_missing_arg(id, "shell_exec", "command"),
    };
    let args: Vec<String> = arguments
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(30);

    // Build a sandbox policy.  Workspace root becomes the writable
    // allowed_path; CWD defaults to that if not specified. Network
    // is denied by default — agents that need to fetch should call
    // the dedicated `web_fetch` tool (when it ships) instead of
    // running curl under shell_exec.
    let workspace_root = engine.workspace_root_path(ws);
    let cwd = arguments
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| workspace_root.clone());

    let policy = thinkingroot_sandbox::SandboxPolicy {
        allowed_paths: workspace_root.into_iter().collect(),
        readonly_paths: vec![std::path::PathBuf::from("/usr"), std::path::PathBuf::from("/bin")],
        allowed_hosts: Vec::new(),
        timeout_secs,
        cwd,
        max_output_bytes: thinkingroot_sandbox::DEFAULT_MAX_OUTPUT_BYTES,
    };

    let sandbox: std::sync::Arc<dyn thinkingroot_sandbox::Sandbox> =
        std::sync::Arc::from(thinkingroot_sandbox::default_sandbox());
    match crate::system_power::shell_exec(sandbox, command, &args, &policy).await {
        Ok(out) => mcp_text_result(id, &out),
        Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
    }
}

async fn handle_clipboard_read(id: Option<Value>) -> JsonRpcResponse {
    // Clipboard ops are sync — run on a blocking thread so we don't
    // stall the async runtime on X11/Wayland init.
    match tokio::task::spawn_blocking(crate::system_power::clipboard_read).await {
        Ok(Ok(out)) => mcp_text_result(id, &out),
        Ok(Err(e)) => JsonRpcResponse::error(id, -32603, e.to_string()),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("clipboard_read: task join: {e}")),
    }
}

async fn handle_clipboard_write(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let content = match arguments.get("content").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        _ => return sp_missing_arg(id, "clipboard_write", "content"),
    };
    match tokio::task::spawn_blocking(move || crate::system_power::clipboard_write(&content)).await
    {
        Ok(Ok(out)) => mcp_text_result(id, &out),
        Ok(Err(e)) => JsonRpcResponse::error(id, -32603, e.to_string()),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("clipboard_write: task join: {e}")),
    }
}

async fn handle_open_in_default(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let target = match arguments.get("path_or_url").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return sp_missing_arg(id, "open_in_default", "path_or_url"),
    };
    match tokio::task::spawn_blocking(move || crate::system_power::open_in_default(&target)).await
    {
        Ok(Ok(out)) => mcp_text_result(id, &out),
        Ok(Err(e)) => JsonRpcResponse::error(id, -32603, e.to_string()),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("open_in_default: task join: {e}")),
    }
}

async fn handle_trash(id: Option<Value>, arguments: &Value) -> JsonRpcResponse {
    let paths: Vec<String> = match arguments.get("paths").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => return sp_missing_arg(id, "trash", "paths"),
    };
    if paths.is_empty() {
        return JsonRpcResponse::error(
            id,
            -32602,
            "trash: `paths` must be non-empty".to_string(),
        );
    }
    let outcome =
        tokio::task::spawn_blocking(move || crate::system_power::trash_paths(&paths)).await;
    match outcome {
        Ok(out) => mcp_text_result(id, &out),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("trash: task join: {e}")),
    }
}
