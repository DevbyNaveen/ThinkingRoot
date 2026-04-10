use super::JsonRpcResponse;
use crate::engine::QueryEngine;
use serde_json::Value;

pub async fn handle_list(
    id: Option<Value>,
    engine: &QueryEngine,
    _default_ws: Option<&str>,
) -> JsonRpcResponse {
    let workspaces = match engine.list_workspaces().await {
        Ok(ws) => ws,
        Err(e) => return JsonRpcResponse::error(id, -32603, e.to_string()),
    };

    let mut resources = Vec::new();
    for ws in &workspaces {
        let name = &ws.name;
        resources.push(serde_json::json!({ "uri": format!("thinkingroot://{}/entities", name), "name": format!("{} — Entities", name), "mimeType": "application/json" }));
        resources.push(serde_json::json!({ "uri": format!("thinkingroot://{}/health", name), "name": format!("{} — Health", name), "mimeType": "application/json" }));
        resources.push(serde_json::json!({ "uri": format!("thinkingroot://{}/contradictions", name), "name": format!("{} — Contradictions", name), "mimeType": "application/json" }));
        for atype in &[
            "architecture-map",
            "contradiction-report",
            "decision-log",
            "task-pack",
            "agent-brief",
            "runbook",
            "health-report",
        ] {
            resources.push(serde_json::json!({ "uri": format!("thinkingroot://{}/artifacts/{}", name, atype), "name": format!("{} — {}", name, atype), "mimeType": "text/markdown" }));
        }
    }
    JsonRpcResponse::success(id, serde_json::json!({ "resources": resources }))
}

pub async fn handle_read(
    id: Option<Value>,
    params: &Value,
    engine: &QueryEngine,
    default_ws: Option<&str>,
) -> JsonRpcResponse {
    let uri = match params.get("uri").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => return JsonRpcResponse::error(id, -32602, "Missing 'uri' parameter".to_string()),
    };

    let stripped = match uri.strip_prefix("thinkingroot://") {
        Some(s) => s,
        None => return JsonRpcResponse::error(id, -32602, format!("Invalid URI scheme: {}", uri)),
    };

    let parts: Vec<&str> = stripped.splitn(3, '/').collect();
    let ws = if parts.is_empty() || parts[0].is_empty() {
        match default_ws {
            Some(w) => w,
            None => {
                return JsonRpcResponse::error(id, -32602, "No workspace specified".to_string());
            }
        }
    } else {
        parts[0]
    };

    let resource_type = parts.get(1).copied().unwrap_or("");
    let resource_name = parts.get(2).copied().unwrap_or("");

    match resource_type {
        "entities" if resource_name.is_empty() => match engine.list_entities(ws).await {
            Ok(entities) => {
                let content = serde_json::to_string_pretty(&entities).unwrap_or_default();
                JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "contents": [{ "uri": uri, "mimeType": "application/json", "text": content }] }),
                )
            }
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },
        "entities" => match engine.get_entity(ws, resource_name).await {
            Ok(entity) => {
                let content = serde_json::to_string_pretty(&entity).unwrap_or_default();
                JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "contents": [{ "uri": uri, "mimeType": "application/json", "text": content }] }),
                )
            }
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },
        "health" => match engine.health(ws).await {
            Ok(result) => {
                let content = serde_json::to_string_pretty(&result).unwrap_or_default();
                JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "contents": [{ "uri": uri, "mimeType": "application/json", "text": content }] }),
                )
            }
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },
        "contradictions" => match engine.health(ws).await {
            Ok(result) => {
                let content = serde_json::json!({ "contradictions": result.contradictions, "warnings": result.warnings });
                JsonRpcResponse::success(
                    id,
                    serde_json::json!({ "contents": [{ "uri": uri, "mimeType": "application/json", "text": serde_json::to_string_pretty(&content).unwrap_or_default() }] }),
                )
            }
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },
        "artifacts" => match engine.get_artifact(ws, resource_name).await {
            Ok(artifact) => JsonRpcResponse::success(
                id,
                serde_json::json!({ "contents": [{ "uri": uri, "mimeType": "text/markdown", "text": artifact.content }] }),
            ),
            Err(e) => JsonRpcResponse::error(id, -32603, e.to_string()),
        },
        _ => JsonRpcResponse::error(
            id,
            -32602,
            format!("Unknown resource type: {}", resource_type),
        ),
    }
}
