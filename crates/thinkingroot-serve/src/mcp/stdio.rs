use super::JsonRpcRequest;
use crate::engine::QueryEngine;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;

pub async fn run(engine: Arc<RwLock<QueryEngine>>, default_workspace: Option<String>) {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                eprintln!("[mcp-stdio] stdin closed, shutting down");
                break;
            }
            Err(e) => {
                eprintln!("[mcp-stdio] read error: {}", e);
                break;
            }
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err_response =
                    super::JsonRpcResponse::error(None, -32700, format!("Parse error: {}", e));
                let json = serde_json::to_string(&err_response).unwrap_or_default();
                let _ = stdout.write_all(json.as_bytes()).await;
                let _ = stdout.write_all(b"\n").await;
                let _ = stdout.flush().await;
                continue;
            }
        };

        if request.id.is_none() && request.method.starts_with("notifications/") {
            continue;
        }

        let engine_guard = engine.read().await;
        let response = super::dispatch(&request, &engine_guard, default_workspace.as_deref()).await;
        drop(engine_guard);

        let json = serde_json::to_string(&response).unwrap_or_default();
        let _ = stdout.write_all(json.as_bytes()).await;
        let _ = stdout.write_all(b"\n").await;
        let _ = stdout.flush().await;
    }
}
