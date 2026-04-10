pub mod resources;
pub mod sse;
pub mod stdio;
pub mod tools;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Value>, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: None,
            }),
        }
    }
}

pub fn server_info() -> Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "serverInfo": { "name": "thinkingroot", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "resources": { "listChanged": false }, "tools": {} }
    })
}

pub async fn dispatch(
    request: &JsonRpcRequest,
    engine: &crate::engine::QueryEngine,
    default_workspace: Option<&str>,
) -> JsonRpcResponse {
    let id = request.id.clone();
    match request.method.as_str() {
        "initialize" => JsonRpcResponse::success(id, server_info()),
        "notifications/initialized" => JsonRpcResponse::success(id, Value::Null),
        "resources/list" => resources::handle_list(id, engine, default_workspace).await,
        "resources/read" => {
            resources::handle_read(id, &request.params, engine, default_workspace).await
        }
        "tools/list" => tools::handle_list(id).await,
        "tools/call" => tools::handle_call(id, &request.params, engine, default_workspace).await,
        "ping" => JsonRpcResponse::success(id, serde_json::json!({})),
        other => JsonRpcResponse::error(id, -32601, format!("Method not found: {}", other)),
    }
}
