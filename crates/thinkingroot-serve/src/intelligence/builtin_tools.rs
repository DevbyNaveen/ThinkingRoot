// crates/thinkingroot-serve/src/intelligence/builtin_tools.rs
//
// Built-in tool handlers — concrete implementations the agent
// (`agent.rs`) dispatches when the LLM picks one. Each tool wraps an
// existing engine / branch API; nothing here is a placeholder. The
// JSON Schema for each tool is the contract the LLM is taught about
// in `chat_with_tools`, so it's deliberately tight: required fields
// are required, optional fields are optional, descriptions guide the
// model on intent.
//
// Tools shipped today (see [`register_builtin_tools`]):
//
//   ── Read tools (no approval gate) ────────────────────────────
//     * `search`           — vector + keyword search across claims
//     * `list_branches`    — enumerate branches in this workspace
//     * `list_claims`      — filter claims by type / entity / confidence
//     * `workspace_info`   — token-efficient workspace summary
//
//   ── Write tools (gated by ApprovalGate) ──────────────────────
//     * `create_branch`    — fork a new knowledge branch from main
//     * `contribute_claim` — append a new claim (to current branch
//                            when set, otherwise main)
//     * `merge_branch`     — merge a branch into main
//     * `abandon_branch`   — soft-delete a branch (data retained)
//
// More tools (resolve_contradiction, supersede_claim, get_relations,
// diff_branch, ask) land as straightforward additions in later
// sprints — none require new architecture.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use thinkingroot_extract::llm::Tool;
use tokio::sync::RwLock;

use crate::engine::{ClaimFilter, QueryEngine};
use crate::intelligence::session::SessionStore;
use crate::intelligence::skills::SkillRegistry;
use crate::intelligence::tools::{ToolHandler, ToolHandlerResult, ToolRegistry};

/// State the tool handlers need to do their work. Cloned cheaply (all
/// fields are `Arc` / `String` / `PathBuf`); shared across every
/// registered handler so they speak to the same engine + session.
#[derive(Clone)]
pub struct ToolContext {
    pub engine: Arc<RwLock<QueryEngine>>,
    /// Workspace name the agent is operating in.
    pub workspace: String,
    /// On-disk root of that workspace. Branch APIs that take a
    /// `&Path` use this.
    pub workspace_root: PathBuf,
    /// MCP session id used for `contribute_claim` provenance
    /// (`mcp://agent/{session_id}`). The chat surface generates one
    /// per conversation; tests can synthesise any stable string.
    pub session_id: String,
    /// Shared session store passed to `engine.contribute_claims`.
    pub sessions: SessionStore,
    /// Stable identifier recorded in `MergedBy::Agent { agent_id }`
    /// when the agent merges. "thinkingroot" for the default chat
    /// surface; tests use synthetic ids.
    pub agent_id: String,
    /// Catalogue of skills (`.thinkingroot/skills/*.md`) the agent
    /// can load via `use_skill`. Empty means no skills are wired —
    /// `use_skill` returns "no such skill" for any name.
    pub skills: Arc<SkillRegistry>,
}

// ─────────────────────────────────────────────────────────────────
// Read tools
// ─────────────────────────────────────────────────────────────────

pub struct SearchTool {
    ctx: ToolContext,
}

impl SearchTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "search",
            "Search the workspace's compiled knowledge graph for claims related to a free-text query. Returns the top matches with their statements, source URIs, and confidence scores. Use this whenever the user asks about something specific in the codebase / documents / memory.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Free-text search terms (e.g. 'how many providers', 'authentication flow')."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of claims to return. Default 10, max 50.",
                        "minimum": 1,
                        "maximum": 50
                    }
                },
                "required": ["query"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for SearchTool {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
        let Some(query) = input.get("query").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("missing required field: query");
        };
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(10)
            .min(50);

        let engine = self.ctx.engine.read().await;
        match engine.search(&self.ctx.workspace, query, limit).await {
            Ok(result) => ToolHandlerResult::ok(format_search_result(&result, limit)),
            Err(e) => ToolHandlerResult::error(format!("search failed: {e}")),
        }
    }
}

fn format_search_result(result: &crate::engine::SearchResult, limit: usize) -> String {
    let mut out = String::new();
    if !result.entities.is_empty() {
        out.push_str(&format!(
            "Top entities ({}):\n",
            result.entities.len().min(limit)
        ));
        for e in result.entities.iter().take(limit) {
            out.push_str(&format!(
                "  - {} ({}, {} claims, relevance {:.2})\n",
                e.name, e.entity_type, e.claim_count, e.relevance
            ));
        }
    }
    if !result.claims.is_empty() {
        out.push_str(&format!(
            "\nTop claims ({}):\n",
            result.claims.len().min(limit)
        ));
        for c in result.claims.iter().take(limit) {
            out.push_str(&format!(
                "  - [{:.2} conf] {} (source: {})\n",
                c.confidence, c.statement, c.source_uri
            ));
        }
    }
    if out.is_empty() {
        return "No matching entities or claims.".to_string();
    }
    out
}

pub struct ListBranchesTool {
    ctx: ToolContext,
}

impl ListBranchesTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "list_branches",
            "List all knowledge branches in this workspace (alternative graph states forked from main). Use to see what experimental directions are in flight before creating a new branch.",
            json!({
                "type": "object",
                "properties": {},
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for ListBranchesTool {
    async fn handle(&self, _input: serde_json::Value) -> ToolHandlerResult {
        match thinkingroot_branch::list_branches(&self.ctx.workspace_root) {
            Ok(branches) => {
                if branches.is_empty() {
                    return ToolHandlerResult::ok("No branches. The workspace has only main.");
                }
                let mut out = format!("{} branches:\n", branches.len());
                for b in &branches {
                    out.push_str(&format!(
                        "  - {} (parent: {}, status: {:?}{})\n",
                        b.name,
                        b.parent,
                        b.status,
                        match &b.description {
                            Some(d) if !d.is_empty() => format!(", {d}"),
                            _ => String::new(),
                        }
                    ));
                }
                ToolHandlerResult::ok(out)
            }
            Err(e) => ToolHandlerResult::error(format!("list_branches failed: {e}")),
        }
    }
}

pub struct ListClaimsTool {
    ctx: ToolContext,
}

impl ListClaimsTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "list_claims",
            "List claims in the workspace, optionally filtered by claim type, related entity, or minimum confidence. Use when the user asks for an enumeration ('show me all decisions', 'what facts are about X').",
            json!({
                "type": "object",
                "properties": {
                    "claim_type": {
                        "type": "string",
                        "description": "Filter by claim type. Common values: 'fact', 'decision', 'opinion', 'plan', 'requirement'."
                    },
                    "entity_name": {
                        "type": "string",
                        "description": "Filter to claims that mention this entity by name."
                    },
                    "min_confidence": {
                        "type": "number",
                        "description": "Minimum confidence score (0..1)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max claims to return. Default 20, max 100.",
                        "minimum": 1,
                        "maximum": 100
                    }
                }
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for ListClaimsTool {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
        let filter = ClaimFilter {
            claim_type: input
                .get("claim_type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            entity_name: input
                .get("entity_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            min_confidence: input.get("min_confidence").and_then(|v| v.as_f64()),
            limit: input
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| (n as usize).min(100)),
            offset: None,
        };

        let engine = self.ctx.engine.read().await;
        match engine.list_claims(&self.ctx.workspace, filter).await {
            Ok(claims) => {
                if claims.is_empty() {
                    return ToolHandlerResult::ok("No claims match the filter.");
                }
                let mut out = format!("{} claims:\n", claims.len());
                for c in &claims {
                    out.push_str(&format!(
                        "  - [{:.2} conf, {}] {}\n",
                        c.confidence, c.claim_type, c.statement
                    ));
                }
                ToolHandlerResult::ok(out)
            }
            Err(e) => ToolHandlerResult::error(format!("list_claims failed: {e}")),
        }
    }
}

pub struct WorkspaceInfoTool {
    ctx: ToolContext,
}

impl WorkspaceInfoTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "workspace_info",
            "Get a token-efficient summary of the current workspace: entity count, claim count, source count, top entities by claim count, recent decisions, and any contradictions. Use when the user asks 'what's in this workspace' or you need to orient yourself.",
            json!({
                "type": "object",
                "properties": {}
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for WorkspaceInfoTool {
    async fn handle(&self, _input: serde_json::Value) -> ToolHandlerResult {
        let engine = self.ctx.engine.read().await;
        match engine.get_workspace_brief(&self.ctx.workspace).await {
            Ok(brief) => {
                let mut out = format!(
                    "Workspace '{}': {} entities, {} claims, {} sources, {} contradictions\n",
                    brief.workspace,
                    brief.entity_count,
                    brief.claim_count,
                    brief.source_count,
                    brief.contradiction_count
                );
                if !brief.top_entities.is_empty() {
                    out.push_str("Top entities:\n");
                    for e in brief.top_entities.iter().take(10) {
                        out.push_str(&format!("  - {} ({} claims)\n", e.name, e.claim_count));
                    }
                }
                if !brief.recent_decisions.is_empty() {
                    out.push_str("Recent decisions:\n");
                    for (statement, conf) in brief.recent_decisions.iter().take(5) {
                        out.push_str(&format!("  - [{conf:.2}] {statement}\n"));
                    }
                }
                ToolHandlerResult::ok(out)
            }
            Err(e) => ToolHandlerResult::error(format!("workspace_info failed: {e}")),
        }
    }
}

pub struct UseSkillTool {
    ctx: ToolContext,
}

impl UseSkillTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "use_skill",
            "Load the full instructions for a registered skill by name. Skills are workspace-specific playbooks (e.g. 'refactor-rust', 'explain-architecture') the user has authored to teach the agent how to do specific tasks well. Use the available skill list shown in the system prompt to pick one.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Exact skill name as listed in the AVAILABLE SKILLS section."
                    }
                },
                "required": ["name"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for UseSkillTool {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
        let Some(name) = input.get("name").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("missing required field: name");
        };
        match self.ctx.skills.get(name) {
            Some(skill) => ToolHandlerResult::ok(format!(
                "## Skill: {} ({})\n\n{}",
                skill.name, skill.description, skill.body
            )),
            None => {
                let available = self.ctx.skills.names().join(", ");
                ToolHandlerResult::error(format!(
                    "no such skill: '{name}'. Available: {}",
                    if available.is_empty() {
                        "(none)"
                    } else {
                        available.as_str()
                    }
                ))
            }
        }
    }
}

/// `read_source` — fetch the exact source bytes a claim cites.
///
/// One of the three v3 MCP tools per the v3 spec §8.5
/// (`docs/2026-04-29-thinkingroot-v3-final-plan.md`). Closes the
/// "verifiable byte range" loop: every claim emitted by extract carries
/// `(file, byte_start, byte_end)`; this tool reads the bytes back so an
/// agent can quote source verbatim instead of paraphrasing.
pub struct ReadSourceTool {
    ctx: ToolContext,
}

impl ReadSourceTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "read_source",
            "Fetch the exact source bytes a claim cites. Pass the claim id (from search or list_claims results) and the tool returns the verbatim text from the source file at the byte range the claim was extracted from. Use this to quote source rather than paraphrase, or to verify a claim's grounding.",
            json!({
                "type": "object",
                "properties": {
                    "claim_id": {
                        "type": "string",
                        "description": "The claim id to read source bytes for."
                    }
                },
                "required": ["claim_id"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for ReadSourceTool {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
        let Some(claim_id) = input.get("claim_id").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("missing required field: claim_id");
        };
        let engine = self.ctx.engine.read().await;
        match engine.read_source(&self.ctx.workspace, claim_id).await {
            Ok(result) => {
                if result.bytes.is_empty() {
                    ToolHandlerResult::ok(format!(
                        "claim {} has no byte-range citation yet (file: {}). Use read_file({:?}) to fetch the whole file.",
                        claim_id, result.file, result.file
                    ))
                } else {
                    ToolHandlerResult::ok(format!(
                        "Source: {} (bytes {}–{})\n\n{}",
                        result.file, result.byte_start, result.byte_end, result.text
                    ))
                }
            }
            Err(e) => ToolHandlerResult::error(format!("read_source failed: {e}")),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Write tools (route through ApprovalGate)
// ─────────────────────────────────────────────────────────────────

pub struct CreateBranchTool {
    ctx: ToolContext,
}

impl CreateBranchTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "create_branch",
            "Fork a new knowledge branch from a parent (default 'main'). Use to explore a what-if without touching the live graph: contribute claims on the branch, then either merge_branch (accept) or abandon_branch (discard). The agent's session is then writing to the new branch by default.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Human-readable branch name. Slashes allowed (e.g. 'experiment/refactor-auth')."
                    },
                    "parent": {
                        "type": "string",
                        "description": "Parent branch to fork from. Default 'main'."
                    },
                    "description": {
                        "type": "string",
                        "description": "One-line note explaining what this branch explores."
                    }
                },
                "required": ["name"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for CreateBranchTool {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
        let Some(name) = input.get("name").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("missing required field: name");
        };
        let parent = input
            .get("parent")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match thinkingroot_branch::create_branch(
            &self.ctx.workspace_root,
            name,
            parent,
            description,
        )
        .await
        {
            Ok(branch) => {
                // Mark the branch active in the agent's session so
                // subsequent contribute_claim calls write to it
                // automatically. The session store is the same one
                // engine.contribute_claims consults at write time.
                let mut store = self.ctx.sessions.lock().await;
                let session = store
                    .entry(self.ctx.session_id.clone())
                    .or_insert_with(|| {
                        crate::intelligence::session::SessionContext::new(
                            self.ctx.session_id.clone(),
                            self.ctx.workspace.clone(),
                        )
                    });
                session.active_branch = Some(branch.name.clone());
                ToolHandlerResult::ok(format!(
                    "Created branch '{}' from '{}'. The session is now writing to this branch.",
                    branch.name, branch.parent
                ))
            }
            Err(e) => ToolHandlerResult::error(format!("create_branch failed: {e}")),
        }
    }
}

pub struct ContributeClaimTool {
    ctx: ToolContext,
}

impl ContributeClaimTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "contribute_claim",
            "Add a new claim to the workspace. Writes to the agent's currently active branch when one is set (e.g. after create_branch); otherwise writes to main. Use when the user states a fact you should remember, or when you've concluded something during investigation that should persist.",
            json!({
                "type": "object",
                "properties": {
                    "statement": {
                        "type": "string",
                        "description": "The claim text — a precise, complete sentence."
                    },
                    "claim_type": {
                        "type": "string",
                        "description": "Claim type. One of: 'fact', 'decision', 'opinion', 'plan', 'requirement'. Default 'fact'.",
                        "enum": ["fact", "decision", "opinion", "plan", "requirement"]
                    },
                    "confidence": {
                        "type": "number",
                        "description": "Subjective confidence (0..1). Default 0.8 for facts, lower for opinions."
                    },
                    "entities": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Entity names this claim is about. Include explicit subjects so the link layer can connect them."
                    }
                },
                "required": ["statement"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for ContributeClaimTool {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
        let Some(statement) = input.get("statement").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("missing required field: statement");
        };
        let claim_type = input
            .get("claim_type")
            .and_then(|v| v.as_str())
            .unwrap_or("fact")
            .to_string();
        let confidence = input.get("confidence").and_then(|v| v.as_f64());
        let entities: Vec<String> = input
            .get("entities")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let agent_claim = crate::engine::AgentClaim {
            statement: statement.to_string(),
            claim_type,
            confidence,
            entities,
        };

        // Resolve active branch from the session store (set by
        // create_branch). Cloned out so we can drop the lock before
        // calling contribute_claims.
        let active_branch = {
            let store = self.ctx.sessions.lock().await;
            store
                .get(&self.ctx.session_id)
                .and_then(|s| s.active_branch.clone())
        };

        let engine = self.ctx.engine.read().await;
        match engine
            .contribute_claims(
                &self.ctx.workspace,
                &self.ctx.session_id,
                active_branch.as_deref(),
                vec![agent_claim],
                &self.ctx.sessions,
            )
            .await
        {
            Ok(result) => {
                let where_to = active_branch
                    .as_ref()
                    .map(|b| format!("branch '{b}'"))
                    .unwrap_or_else(|| "main".to_string());
                let warnings_msg = if result.warnings.is_empty() {
                    String::new()
                } else {
                    format!(" warnings: {}", result.warnings.join("; "))
                };
                ToolHandlerResult::ok(format!(
                    "Contributed {} claim(s) to {}. claim_id={}{warnings_msg}",
                    result.accepted_count,
                    where_to,
                    result.accepted_ids.first().map(|s| s.as_str()).unwrap_or("(none)")
                ))
            }
            Err(e) => ToolHandlerResult::error(format!("contribute_claim failed: {e}")),
        }
    }
}

pub struct MergeBranchTool {
    ctx: ToolContext,
}

impl MergeBranchTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "merge_branch",
            "Merge a branch into main. The merge runs through the contradiction / health-drop checks; specify force=true only when the user has explicitly accepted the risk. Records MergedBy::Agent so the audit trail names the agent that did the merge.",
            json!({
                "type": "object",
                "properties": {
                    "branch": {
                        "type": "string",
                        "description": "Branch name to merge into main."
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Bypass the safety checks (contradiction limits, health drop, auto-resolve threshold). Default false."
                    },
                    "propagate_deletions": {
                        "type": "boolean",
                        "description": "Also delete from main any claims that the branch deleted. Default false (deletions stay branch-local)."
                    }
                },
                "required": ["branch"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for MergeBranchTool {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
        let Some(branch) = input.get("branch").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("missing required field: branch");
        };
        let force = input
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let propagate_deletions = input
            .get("propagate_deletions")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let merged_by = thinkingroot_core::MergedBy::Agent {
            agent_id: self.ctx.agent_id.clone(),
        };

        let engine = self.ctx.engine.read().await;
        match engine
            .merge_branch(
                &self.ctx.workspace_root,
                branch,
                force,
                propagate_deletions,
                merged_by,
            )
            .await
        {
            Ok(diff) => {
                if !diff.merge_allowed {
                    return ToolHandlerResult::error(format!(
                        "merge blocked: {}",
                        diff.blocking_reasons.join("; ")
                    ));
                }
                ToolHandlerResult::ok(format!(
                    "Merged '{branch}' into main. New claims: {}, auto-resolved: {}, needs review: {}.",
                    diff.new_claims.len(),
                    diff.auto_resolved.len(),
                    diff.needs_review.len()
                ))
            }
            Err(e) => ToolHandlerResult::error(format!("merge_branch failed: {e}")),
        }
    }
}

pub struct AbandonBranchTool {
    ctx: ToolContext,
}

impl AbandonBranchTool {
    pub fn new(ctx: ToolContext) -> Self {
        Self { ctx }
    }
    pub fn spec() -> Tool {
        Tool::new(
            "abandon_branch",
            "Soft-delete a branch — marks it Abandoned but keeps the data dir on disk so you can still inspect it. Use when an exploratory branch turned out to be a dead-end. Use 'merge_branch' instead when the branch is the right answer.",
            json!({
                "type": "object",
                "properties": {
                    "branch": {
                        "type": "string",
                        "description": "Branch name to abandon."
                    }
                },
                "required": ["branch"]
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for AbandonBranchTool {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
        let Some(branch) = input.get("branch").and_then(|v| v.as_str()) else {
            return ToolHandlerResult::error("missing required field: branch");
        };

        let engine = self.ctx.engine.read().await;
        match engine
            .delete_branch(&self.ctx.workspace_root, branch)
            .await
        {
            Ok(()) => {
                // If this branch was the agent's active branch, clear
                // it so subsequent contribute_claim calls fall back
                // to main rather than try to write to the abandoned
                // branch.
                let mut store = self.ctx.sessions.lock().await;
                if let Some(session) = store.get_mut(&self.ctx.session_id)
                    && session.active_branch.as_deref() == Some(branch)
                {
                    session.active_branch = None;
                }
                ToolHandlerResult::ok(format!(
                    "Abandoned branch '{branch}'. Data retained on disk; gc_branches reclaims space."
                ))
            }
            Err(e) => ToolHandlerResult::error(format!("abandon_branch failed: {e}")),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Wire-up
// ─────────────────────────────────────────────────────────────────

/// Register all built-in tools on a fresh registry. The set is:
///
///   read:  search, list_branches, list_claims, workspace_info, use_skill
///   write: create_branch, contribute_claim, merge_branch, abandon_branch
///
/// Caller is expected to share this registry with the
/// [`crate::intelligence::agent::Agent`] it constructs. Cheap and
/// idempotent: every call returns a fresh registry with its own
/// dispatch table.
pub fn register_builtin_tools(ctx: ToolContext) -> ToolRegistry {
    ToolRegistry::new()
        .register_read(SearchTool::spec(), Arc::new(SearchTool::new(ctx.clone())))
        .register_read(
            ListBranchesTool::spec(),
            Arc::new(ListBranchesTool::new(ctx.clone())),
        )
        .register_read(
            ListClaimsTool::spec(),
            Arc::new(ListClaimsTool::new(ctx.clone())),
        )
        .register_read(
            WorkspaceInfoTool::spec(),
            Arc::new(WorkspaceInfoTool::new(ctx.clone())),
        )
        .register_read(
            UseSkillTool::spec(),
            Arc::new(UseSkillTool::new(ctx.clone())),
        )
        .register_read(
            ReadSourceTool::spec(),
            Arc::new(ReadSourceTool::new(ctx.clone())),
        )
        .register_write(
            CreateBranchTool::spec(),
            Arc::new(CreateBranchTool::new(ctx.clone())),
        )
        .register_write(
            ContributeClaimTool::spec(),
            Arc::new(ContributeClaimTool::new(ctx.clone())),
        )
        .register_write(
            MergeBranchTool::spec(),
            Arc::new(MergeBranchTool::new(ctx.clone())),
        )
        .register_write(
            AbandonBranchTool::spec(),
            Arc::new(AbandonBranchTool::new(ctx)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_spec_has_required_query_field() {
        let spec = SearchTool::spec();
        assert_eq!(spec.name, "search");
        assert!(!spec.description.is_empty());
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "query"));
    }

    #[test]
    fn create_branch_spec_requires_name() {
        let spec = CreateBranchTool::spec();
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "name"));
        // parent is optional, defaulted in the handler.
        assert!(!required.iter().any(|v| v == "parent"));
    }

    #[test]
    fn contribute_claim_spec_constrains_claim_type_via_enum() {
        let spec = ContributeClaimTool::spec();
        let claim_type = &spec.input_schema["properties"]["claim_type"];
        let allowed = claim_type["enum"].as_array().unwrap();
        let names: Vec<&str> = allowed.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"fact"));
        assert!(names.contains(&"decision"));
        assert!(names.contains(&"opinion"));
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"requirement"));
    }

    #[test]
    fn merge_branch_spec_requires_branch() {
        let spec = MergeBranchTool::spec();
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "branch"));
    }

    #[test]
    fn abandon_branch_spec_requires_branch() {
        let spec = AbandonBranchTool::spec();
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "branch"));
    }

    #[test]
    fn list_claims_spec_has_no_required_fields() {
        let spec = ListClaimsTool::spec();
        // `required` may be absent or empty. If present, must be empty.
        let required = spec.input_schema.get("required");
        if let Some(r) = required {
            assert!(r.as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn workspace_info_spec_has_no_required_fields() {
        let spec = WorkspaceInfoTool::spec();
        let required = spec.input_schema.get("required");
        if let Some(r) = required {
            assert!(r.as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn list_branches_spec_has_no_required_fields() {
        let spec = ListBranchesTool::spec();
        let required = spec.input_schema.get("required");
        if let Some(r) = required {
            assert!(r.as_array().unwrap().is_empty());
        }
    }

    fn fixture_ctx() -> ToolContext {
        // We don't need a real engine for spec tests — only the
        // register_builtin_tools wiring needs the context. Use a
        // bare engine with no workspaces mounted (the dispatch
        // tests for the actual handlers live in integration tests
        // which spin up a real workspace).
        ToolContext {
            engine: Arc::new(RwLock::new(QueryEngine::new())),
            workspace: "test".to_string(),
            workspace_root: PathBuf::from("/tmp/__builtin_tools_test__"),
            session_id: "sess".to_string(),
            sessions: crate::intelligence::session::new_session_store(),
            agent_id: "test-agent".to_string(),
            skills: Arc::new(SkillRegistry::empty()),
        }
    }

    #[test]
    fn register_builtin_tools_produces_ten_tools() {
        let ctx = fixture_ctx();
        let registry = register_builtin_tools(ctx);
        let mut names = registry.tool_names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "abandon_branch",
                "contribute_claim",
                "create_branch",
                "list_branches",
                "list_claims",
                "merge_branch",
                "read_source",
                "search",
                "use_skill",
                "workspace_info",
            ]
        );
    }

    #[test]
    fn register_builtin_tools_classifies_writes() {
        let ctx = fixture_ctx();
        let registry = register_builtin_tools(ctx);
        // Reads.
        for name in [
            "search",
            "list_branches",
            "list_claims",
            "workspace_info",
            "use_skill",
        ] {
            assert!(!registry.is_write(name), "{name} should be a read");
        }
        // Writes.
        for name in [
            "create_branch",
            "contribute_claim",
            "merge_branch",
            "abandon_branch",
        ] {
            assert!(registry.is_write(name), "{name} should be a write");
        }
    }

    #[tokio::test]
    async fn use_skill_returns_skill_body() {
        use crate::intelligence::skills::Skill;
        let skills = SkillRegistry::from_skills(vec![Skill {
            name: "explain-architecture".to_string(),
            description: "How X works".to_string(),
            body: "Step 1...\nStep 2...".to_string(),
            source_path: PathBuf::from("/tmp/x.md"),
        }])
        .unwrap();
        let mut ctx = fixture_ctx();
        ctx.skills = Arc::new(skills);
        let tool = UseSkillTool::new(ctx);
        let res = tool
            .handle(serde_json::json!({"name": "explain-architecture"}))
            .await;
        assert!(!res.is_error);
        assert!(res.content.contains("Step 1"));
        assert!(res.content.contains("explain-architecture"));
    }

    #[tokio::test]
    async fn use_skill_errors_on_unknown_name() {
        let tool = UseSkillTool::new(fixture_ctx());
        let res = tool.handle(serde_json::json!({"name": "nonexistent"})).await;
        assert!(res.is_error);
        assert!(res.content.contains("no such skill"));
    }

    #[tokio::test]
    async fn use_skill_errors_on_missing_name_field() {
        let tool = UseSkillTool::new(fixture_ctx());
        let res = tool.handle(serde_json::json!({})).await;
        assert!(res.is_error);
        assert!(res.content.contains("missing required field"));
    }
}
