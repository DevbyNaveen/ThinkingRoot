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
fn mcp_text_result<T: serde::Serialize>(id: Option<Value>, payload: &T) -> JsonRpcResponse {
    match serde_json::to_string_pretty(payload) {
        Ok(content) => JsonRpcResponse::success(
            id,
            serde_json::json!({ "content": [{ "type": "text", "text": content }] }),
        ),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("serialize result: {e}")),
    }
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
    if let Some(session) = store.get(session_id)
        && let Some(owner) = session.owner.as_ref()
    {
        return crate::engine::BranchActor::User(owner.clone());
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
fn resolve_workspace_arg(
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

#[tracing::instrument(name = "mcp.tools.list", skip_all)]
pub async fn handle_list(id: Option<Value>) -> JsonRpcResponse {
    let tools = serde_json::json!({
        "tools": [
            // ── Classic CRUD tools ────────────────────────────────────────
            {
                "name": "search",
                "description": "Semantic + keyword search across entities and claims",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query":     { "type": "string" },
                        "top_k":    { "type": "integer", "default": 10 },
                        "workspace": { "type": "string" }
                    },
                    "required": ["query", "workspace"]
                }
            },
            {
                "name": "query_claims",
                "description": "Filter claims by type, entity, or confidence threshold",
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
                "name": "get_relations",
                "description": "Get all relations for a specific entity",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "entity":    { "type": "string" },
                        "workspace": { "type": "string" }
                    },
                    "required": ["entity", "workspace"]
                }
            },
            {
                "name": "compile",
                "description": "Trigger full pipeline recompilation (requires LLM credentials)",
                "inputSchema": {
                    "type": "object",
                    "properties": { "workspace": { "type": "string" } },
                    "required": ["workspace"]
                }
            },
            {
                "name": "health_check",
                "description": "Run verification and return knowledge health score",
                "inputSchema": {
                    "type": "object",
                    "properties": { "workspace": { "type": "string" } },
                    "required": ["workspace"]
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
                "description": "Build an Engram (typed sub-graph) for a topic. Returns an EngramSummary plus a pointer (e.g. '0x7F9A') the caller holds for subsequent probes. Default depth_hops=2, event_window_days=90, clearance=['public']. Optional seed_claim_ids skips vector seeding when the caller already has IDs.",
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
                "description": "Probe a materialised Engram with a typed question. Returns a ProbeAnswer with answer rows + per-row provenance (claim_ids, byte spans, BLAKE3, trial_scores, certificate_hash, turn_provenance, derivation_root) + caveats (UnresolvedContradiction, StaleRow, LowConfidence, DerivedFromTest, SupersededByNewerClaim, GapAdjacent, SensitivityRedaction). Optional probe_kind overrides the regex router; turn_provenance lookup is bounded to the most recent 200 turns of the session. Optional score_with_hybrid routes the answer rows through Hybrid Retrieval scoring (vector × Datalog × BLAKE3 × 11-component fusion) for re-ranking before caveat enrichment.",
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
                "description": "List active Engram pointers for the current session.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string" }
                    },
                    "required": ["workspace"]
                }
            },
            {
                "name": "expire_engram",
                "description": "Explicitly evict an Engram from the session. Returns { expired: bool }.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pointer":   { "type": "string" },
                        "workspace": { "type": "string" }
                    },
                    "required": ["pointer", "workspace"]
                }
            },
            {
                "name": "hybrid_retrieve",
                "description": "World-class hybrid retrieval over the 33-table substrate. Combines vector recall + Datalog filters + per-row BLAKE3 verification + 11-component score fusion. Returns ranked hits with full provenance bundles (byte spans, source authority, admission tier, trial scores, certificate hash, derivation lineage), plus caveats (StaleRow, UnresolvedContradiction, SupersededByNewerClaim, DerivedFromTest, GapAdjacent, SensitivityRedaction, LowConfidence, DroppedQuarantined, BytesUnavailable). Use 'typed_predicates' to filter by entity, doc-tag, marker, code-metric range, authorship, heading path, or supersession. Use 'scoring_profile' = 'compliance' (rooted-only, doubled penalties) for legal/audit queries. Spec: docs/2026-05-02-hybrid-retrieval-spec.md.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace":                 { "type": "string" },
                        "query_text":                { "type": "string", "description": "Free-text query for vector recall. Empty when only typed predicates apply." },
                        "typed_predicates":          { "type": "array", "default": [], "items": { "type": "object", "properties": { "kind": { "type": "string", "enum": ["entity_type", "entity_name", "claim_type", "source_trust_at_least", "authored_by", "authored_after", "in_call_graph_of", "has_doc_tag", "has_marker", "quantity_range", "in_heading_path", "supersedes_claim", "referenced_by"] } }, "required": ["kind"] }, "description": "Structured filters. Multiple predicates AND-combined." },
                        "session_id":                { "type": "string" },
                        "clearance":                 { "type": "array", "default": ["public"], "items": { "type": "string", "enum": ["public", "internal", "confidential", "restricted"] } },
                        "top_k":                     { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 },
                        "scoring_profile":           { "type": "string", "enum": ["default", "compliance", "custom"], "default": "default" },
                        "scoring_profile_custom":    { "type": "object", "description": "Required when scoring_profile=custom; same shape as ScoringProfile." },
                        "require_certificate":       { "type": "boolean", "default": false },
                        "include_test_origin":       { "type": "boolean", "default": false },
                        "include_quarantined":       { "type": "boolean", "default": false },
                        "require_provenance_verified": { "type": "boolean", "default": false }
                    },
                    "required": ["workspace", "session_id"]
                }
            }
        ]
    });
    JsonRpcResponse::success(id, tools)
}

#[tracing::instrument(
    name = "mcp.tools.call",
    skip(params, engine, sessions, engram_manager),
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

    match tool_name {
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
            let session_snapshot = {
                let store = sessions.lock().await;
                store.get(session_id).cloned()
            };
            let session_ctx = session_snapshot.unwrap_or_else(|| {
                crate::intelligence::session::SessionContext::new(session_id, ws)
            });
            match engine
                .search_with_routing(ws, query, top_k, &session_ctx)
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
            let active_branch: Option<String> = {
                let store = sessions.lock().await;
                store.get(session_id).and_then(|s| s.active_branch.clone())
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

        // ── Classic relations ─────────────────────────────────────────────
        "get_relations" => {
            let active_branch: Option<String> = {
                let store = sessions.lock().await;
                store.get(session_id).and_then(|s| s.active_branch.clone())
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
            let root_path_str = arguments
                .get("root_path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let root = std::path::Path::new(root_path_str);
            let description = arguments
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from);
            match thinkingroot_branch::create_branch_with_owner(
                root,
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
            let root_path_str = arguments
                .get("root_path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let root = std::path::Path::new(root_path_str);
            use thinkingroot_branch::diff::compute_diff;
            use thinkingroot_branch::snapshot::resolve_data_dir;
            use thinkingroot_core::config::Config;
            use thinkingroot_graph::graph::GraphStore;

            let config = match Config::load_merged(root) {
                Ok(c) => c,
                Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
            };
            let mc = &config.merge;
            let main_data_dir = resolve_data_dir(root, None);
            let branch_data_dir = resolve_data_dir(root, Some(branch_name));
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
            let root_path_str = arguments
                .get("root_path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let root = std::path::Path::new(root_path_str);
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
                    root,
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
            let root_path_str = arguments
                .get("root_path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let root = std::path::Path::new(root_path_str);
            match engine
                .rebase_branch(root, branch_name, session_actor(sessions, session_id).await)
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
            let root_path_str = arguments
                .get("root_path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let root = std::path::Path::new(root_path_str);
            match thinkingroot_branch::list_branches(root) {
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
            let root_path_str = arguments
                .get("root_path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let root = std::path::Path::new(root_path_str);
            match engine
                .delete_branch_as(root, branch_name, session_actor(sessions, session_id).await)
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
            let root_path_str = arguments
                .get("root_path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let root = std::path::Path::new(root_path_str);
            match engine.gc_branches(root).await {
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
            let root_path_str = arguments
                .get("root_path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let root = std::path::Path::new(root_path_str);
            match engine.rollback_merge(root, branch_name).await {
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

        other => JsonRpcResponse::error(id, -32601, format!("Unknown tool: {}", other)),
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
