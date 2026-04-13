use super::JsonRpcResponse;
use crate::engine::{AgentClaim, ClaimFilter, QueryEngine};
use crate::intelligence::compressor;
use crate::intelligence::planner::{self, PlanStep};
use crate::intelligence::session::{SessionContext, SessionStore};
use serde_json::Value;

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
                "description": "Merge a knowledge branch into main (runs health CI gate)",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branch":    { "type": "string" },
                        "workspace": { "type": "string" },
                        "force":     { "type": "boolean", "default": false },
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
                                    "claim_type":  { "type": "string", "enum": ["fact","decision","opinion","plan","requirement","metric","definition","dependency","api_signature","architecture"], "default": "fact" },
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
            }
        ]
    });
    JsonRpcResponse::success(id, tools)
}

pub async fn handle_call(
    id: Option<Value>,
    params: &Value,
    engine: &QueryEngine,
    default_ws: Option<&str>,
    session_id: &str,
    sessions: &SessionStore,
) -> JsonRpcResponse {
    let tool_name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return JsonRpcResponse::error(id, -32602, "Missing 'name' parameter".to_string()),
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let ws = arguments
        .get("workspace")
        .and_then(|v| v.as_str())
        .or(default_ws)
        .unwrap_or("default");

    match tool_name {
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
            match engine.search(ws, query, top_k).await {
                Ok(results) => {
                    let content = serde_json::to_string_pretty(&results).unwrap_or_default();
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": content }] }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Classic claim filter ──────────────────────────────────────────
        "query_claims" => {
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
            match engine.list_claims(ws, filter).await {
                Ok(claims) => {
                    let content = serde_json::to_string_pretty(&claims).unwrap_or_default();
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": content }] }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Classic relations ─────────────────────────────────────────────
        "get_relations" => {
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
            match engine.get_relations(ws, entity).await {
                Ok(rels) => {
                    let content = serde_json::to_string_pretty(&rels).unwrap_or_default();
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": content }] }),
                    )
                }
                Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
            }
        }

        // ── Pipeline ──────────────────────────────────────────────────────
        "compile" => match engine.compile(ws).await {
            Ok(result) => {
                let content = serde_json::to_string_pretty(&result).unwrap_or_default();
                JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "content": [{ "type": "text", "text": content }] }),
                )
            }
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },

        "health_check" => match engine.health(ws).await {
            Ok(result) => {
                let content = serde_json::to_string_pretty(&result).unwrap_or_default();
                JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "content": [{ "type": "text", "text": content }] }),
                )
            }
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
            match thinkingroot_branch::create_branch(root, branch_name, "main", description).await {
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
                Ok(diff) => {
                    let content = serde_json::to_string_pretty(&diff).unwrap_or_default();
                    JsonRpcResponse::success(
                        id,
                        serde_json::json!({ "content": [{ "type": "text", "text": content }] }),
                    )
                }
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
            use thinkingroot_branch::diff::compute_diff;
            use thinkingroot_branch::merge::execute_merge;
            use thinkingroot_branch::snapshot::resolve_data_dir;
            use thinkingroot_core::{MergedBy, config::Config};
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
            let mut diff = match compute_diff(
                &main_graph,
                &branch_graph,
                branch_name,
                mc.auto_resolve_threshold,
                mc.max_health_drop,
                mc.block_on_contradictions,
            ) {
                Ok(d) => d,
                Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
            };
            if force {
                diff.merge_allowed = true;
                diff.blocking_reasons.clear();
            }
            match execute_merge(
                root,
                branch_name,
                &diff,
                MergedBy::Human {
                    user: "mcp".to_string(),
                },
                false,
            )
            .await
            {
                Ok(_) => JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "Branch '{}' merged: {} new claims, {} new entities, {} auto-resolved",
                                branch_name,
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
        "brief" => match engine.get_workspace_brief(ws).await {
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
        },

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

            // Read session snapshot for planner.
            let session_snapshot = {
                let store = sessions.lock().await;
                store
                    .get(session_id)
                    .cloned()
                    .unwrap_or_else(|| SessionContext::new(session_id, ws))
            };

            // Plan: choose retrieval strategy (full context / reverse deps / neighborhood).
            let plan = planner::plan_query(&entity_name, &session_snapshot);

            let text = match plan.steps.first() {
                Some(PlanStep::FindReverseDeps(name)) => {
                    match engine.get_entity_context(ws, name).await {
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
                    match engine.get_entity_context(ws, name).await {
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
                    match engine.get_entity_context(ws, &entity_name).await {
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
                .contribute_claims(ws, session_id, active_branch.as_deref(), agent_claims)
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

        other => JsonRpcResponse::error(id, -32601, format!("Unknown tool: {}", other)),
    }
}
