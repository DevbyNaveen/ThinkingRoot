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
            { "name": "health_check", "description": "Run verification and return knowledge health score", "inputSchema": { "type": "object", "properties": { "workspace": { "type": "string" } }, "required": ["workspace"] } }
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
        other => JsonRpcResponse::error(id, -32601, format!("Unknown tool: {}", other)),
    }
}
