use super::JsonRpcResponse;
use crate::engine::{ClaimFilter, QueryEngine};
use serde_json::Value;

pub async fn handle_list(id: Option<Value>) -> JsonRpcResponse {
    let tools = serde_json::json!({
        "tools": [
            { "name": "search", "description": "Semantic search across entities and claims", "inputSchema": { "type": "object", "properties": { "query": { "type": "string" }, "top_k": { "type": "integer", "default": 10 }, "workspace": { "type": "string" } }, "required": ["query", "workspace"] } },
            { "name": "query_claims", "description": "Filter claims by type, entity, or confidence threshold", "inputSchema": { "type": "object", "properties": { "type": { "type": "string" }, "entity": { "type": "string" }, "min_confidence": { "type": "number" }, "workspace": { "type": "string" } }, "required": ["workspace"] } },
            { "name": "get_relations", "description": "Get all relations for a specific entity", "inputSchema": { "type": "object", "properties": { "entity": { "type": "string" }, "workspace": { "type": "string" } }, "required": ["entity", "workspace"] } },
            { "name": "compile", "description": "Trigger full pipeline recompilation (requires LLM credentials)", "inputSchema": { "type": "object", "properties": { "workspace": { "type": "string" } }, "required": ["workspace"] } },
            { "name": "health_check", "description": "Run verification and return knowledge health score", "inputSchema": { "type": "object", "properties": { "workspace": { "type": "string" } }, "required": ["workspace"] } },
            { "name": "create_branch", "description": "Create an isolated knowledge branch for experimentation or agent sandboxing", "inputSchema": { "type": "object", "properties": { "name": { "type": "string", "description": "Branch name (e.g. feature/x)" }, "workspace": { "type": "string" }, "description": { "type": "string" }, "root_path": { "type": "string", "description": "Workspace root path (default: current directory)" } }, "required": ["name", "workspace"] } },
            { "name": "diff_branch", "description": "Compute a semantic Knowledge PR — shows new claims, entities, and contradictions", "inputSchema": { "type": "object", "properties": { "branch": { "type": "string", "description": "Branch to diff against main" }, "workspace": { "type": "string" }, "root_path": { "type": "string", "description": "Workspace root path (default: current directory)" } }, "required": ["branch", "workspace"] } },
            { "name": "merge_branch", "description": "Merge a knowledge branch into main (runs health CI gate)", "inputSchema": { "type": "object", "properties": { "branch": { "type": "string" }, "workspace": { "type": "string" }, "force": { "type": "boolean", "default": false }, "root_path": { "type": "string", "description": "Workspace root path (default: current directory)" } }, "required": ["branch", "workspace"] } }
        ]
    });
    JsonRpcResponse::success(id, tools)
}

pub async fn handle_call(
    id: Option<Value>,
    params: &Value,
    engine: &QueryEngine,
    default_ws: Option<&str>,
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
        "create_branch" => {
            let branch_name = match arguments.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "Missing 'name' argument".to_string(),
                    )
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
            match thinkingroot_branch::create_branch(root, branch_name, "main", description).await
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
                    )
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
                    )
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
            use thinkingroot_core::{config::Config, MergedBy};
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
        other => JsonRpcResponse::error(id, -32601, format!("Unknown tool: {}", other)),
    }
}
