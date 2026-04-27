//! Chat — bridges the UI to the sidecar's existing `ask` endpoint.
//!
//! The OSS engine's `root serve` binary already exposes
//! `POST /api/v1/ws/{ws}/ask` (see
//! `crates/thinkingroot-serve/src/rest.rs:135`). It accepts a question
//! plus a session scope and resolves the configured LLM via the
//! engine's `workspace_llm`. We use that endpoint directly rather than
//! re-implementing provider clients in the desktop.
//!
//! This is **not** token-by-token streaming today: the engine returns
//! a single `{answer, claims_used, category}` payload. The UI fakes a
//! typewriter effect over the returned text. When the engine adds an
//! SSE variant, we'll switch to it without changing the Tauri event
//! shape — `chat-token`/`chat-final` already match what an SSE stream
//! would emit.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ChatStreamArgs {
    pub workspace: String,
    pub question: String,
    /// Optional conversation id — present so we can cross-reference
    /// turns once chat history is fed into retrieval. Today the ask
    /// endpoint is single-turn so this is only used as an event tag.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Optional list of source URIs to scope retrieval to. Empty =
    /// no scoping (engine considers all claims).
    #[serde(default)]
    pub session_scope: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatStreamAck {
    pub turn_id: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Token {
        turn_id: String,
        text: String,
    },
    Final {
        turn_id: String,
        full_text: String,
        claims_used: usize,
        category: String,
        conversation_id: Option<String>,
    },
    Error {
        turn_id: String,
        message: String,
    },
}

#[tauri::command]
pub async fn chat_send_stream(
    app: AppHandle,
    args: ChatStreamArgs,
) -> Result<ChatStreamAck, String> {
    let state = app.state::<AppState>();
    let sidecar = state.sidecar.lock().await.clone();
    let Some(sidecar) = sidecar else {
        return Err(
            "agent runtime sidecar is not running — try restarting the app".to_string(),
        );
    };

    let turn_id = Uuid::new_v4().to_string();
    let conv = args.conversation_id.clone();
    let url = format!(
        "http://{}:{}/api/v1/ws/{}/ask",
        sidecar.host, sidecar.port, args.workspace
    );

    let app_for_task = app.clone();
    let turn_for_task = turn_id.clone();
    let workspace = args.workspace.clone();
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = app_for_task.emit(
                    "chat-event",
                    ChatEvent::Error {
                        turn_id: turn_for_task.clone(),
                        message: format!("http client init failed: {e}"),
                    },
                );
                return;
            }
        };
        let body = serde_json::json!({
            "question": args.question,
            "session_scope": args.session_scope,
            "question_date": chrono::Utc::now().to_rfc3339(),
            "category_hint": "",
        });
        let resp = match client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                let _ = app_for_task.emit(
                    "chat-event",
                    ChatEvent::Error {
                        turn_id: turn_for_task.clone(),
                        message: format!("sidecar unreachable at {url}: {e}"),
                    },
                );
                return;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let _ = app_for_task.emit(
                "chat-event",
                ChatEvent::Error {
                    turn_id: turn_for_task.clone(),
                    message: format!("sidecar returned {status}: {body}"),
                },
            );
            return;
        }
        let parsed: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                let _ = app_for_task.emit(
                    "chat-event",
                    ChatEvent::Error {
                        turn_id: turn_for_task.clone(),
                        message: format!("decode response: {e}"),
                    },
                );
                return;
            }
        };
        let data = match parsed.get("data") {
            Some(d) => d,
            None => {
                let err = parsed
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("malformed response (no `data` field)");
                let _ = app_for_task.emit(
                    "chat-event",
                    ChatEvent::Error {
                        turn_id: turn_for_task.clone(),
                        message: format!("ask failed: {err}"),
                    },
                );
                return;
            }
        };
        let answer = data
            .get("answer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let claims_used = data
            .get("claims_used")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let category = data
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Token-by-token simulation. Splitting on whitespace gives the
        // typewriter feel without lying about the upstream — we still
        // emit a single Final at the end with the full text. When the
        // engine ships SSE this loop is replaced with a real stream
        // reader and the same Final still fires.
        for word in answer.split_inclusive(char::is_whitespace) {
            let _ = app_for_task.emit(
                "chat-event",
                ChatEvent::Token {
                    turn_id: turn_for_task.clone(),
                    text: word.to_string(),
                },
            );
            tokio::time::sleep(Duration::from_millis(12)).await;
        }
        let _ = app_for_task.emit(
            "chat-event",
            ChatEvent::Final {
                turn_id: turn_for_task.clone(),
                full_text: answer,
                claims_used,
                category,
                conversation_id: conv,
            },
        );
        // workspace is part of the closure for trace context only.
        let _ = workspace;
    });

    Ok(ChatStreamAck {
        turn_id,
        host: sidecar.host,
        port: sidecar.port,
    })
}
