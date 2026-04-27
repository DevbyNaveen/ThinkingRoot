//! Chat — bridges the UI to the sidecar's streaming `ask/stream`
//! endpoint.
//!
//! The OSS engine's `root serve` binary exposes
//! `POST /api/v1/ws/{ws}/ask/stream` (see
//! `crates/thinkingroot-serve/src/rest.rs::ask_stream_handler`). It
//! emits SSE events shaped as:
//!
//! - `event: meta` carrying `{claims_used, category}`
//! - `event: token` carrying `{text}` — one per delta from the
//!   provider (Anthropic / OpenAI-compatible / Azure SSE)
//! - `event: final` carrying `{claims_used, category, truncated}`
//! - `event: error` carrying `{message}` — only on failure
//!
//! We forward each event into the existing `chat-event` Tauri channel
//! so the UI's `Token | Final | Error` discriminator (which predates
//! real streaming) keeps working unchanged. Tokens are accumulated
//! locally so the `Final` event still carries `full_text` for
//! disk-persistence — the UI's reducer relies on it.

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
        "http://{}:{}/api/v1/ws/{}/ask/stream",
        sidecar.host, sidecar.port, args.workspace
    );

    let app_for_task = app.clone();
    let turn_for_task = turn_id.clone();
    let workspace = args.workspace.clone();
    tokio::spawn(async move {
        consume_ask_stream(app_for_task, turn_for_task, url, args, conv, workspace).await;
    });

    Ok(ChatStreamAck {
        turn_id,
        host: sidecar.host,
        port: sidecar.port,
    })
}

/// Real SSE consumer. Connects to the engine's `/ask/stream`
/// endpoint, forwards each `event: token` to the UI as a Tauri
/// `chat-event` of type `Token`, accumulates the full body locally,
/// and emits a single `Final` carrying `full_text` so the UI's
/// existing reducer can persist the assistant message to disk.
async fn consume_ask_stream(
    app: AppHandle,
    turn_id: String,
    url: String,
    args: ChatStreamArgs,
    conv: Option<String>,
    workspace: String,
) {
    use eventsource_stream::Eventsource;
    use futures::StreamExt;

    tracing::info!(turn_id = %turn_id, url = %url, workspace = %args.workspace, "chat: consume_ask_stream start");

    // The connect itself is fast — the long wait is the LLM body. A
    // 5s connect-only timeout means a wedged sidecar surfaces as an
    // error in seconds, not minutes. Once bytes flow we let the
    // stream run as long as the upstream needs (the engine's own
    // 120s synthesis timeout still bounds the worst case).
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(turn_id = %turn_id, "chat: http client init failed: {e}");
            emit_error(&app, &turn_id, format!("http client init failed: {e}"));
            return;
        }
    };

    let body = serde_json::json!({
        "question": args.question,
        "session_scope": args.session_scope,
        "question_date": chrono::Utc::now().to_rfc3339(),
        "category_hint": "",
    });

    tracing::info!(turn_id = %turn_id, "chat: posting to sidecar");
    let resp = match client
        .post(&url)
        .header("accept", "text/event-stream")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(turn_id = %turn_id, "chat: sidecar unreachable at {url}: {e}");
            emit_error(&app, &turn_id, format!("sidecar unreachable at {url}: {e}"));
            return;
        }
    };

    tracing::info!(turn_id = %turn_id, status = %resp.status(), "chat: sidecar responded");

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(turn_id = %turn_id, "chat: sidecar non-2xx {status}: {body}");
        emit_error(&app, &turn_id, format!("sidecar returned {status}: {body}"));
        return;
    }

    let mut events = resp.bytes_stream().eventsource();
    let mut full_text = String::new();
    let mut claims_used: usize = 0;
    let mut category = String::new();
    let mut emitted_any = false;

    while let Some(item) = events.next().await {
        match item {
            Err(e) => {
                emit_error(&app, &turn_id, format!("sse parse: {e}"));
                return;
            }
            Ok(ev) => match ev.event.as_str() {
                "meta" => {
                    if let Ok(json) =
                        serde_json::from_str::<serde_json::Value>(&ev.data)
                    {
                        claims_used = json
                            .get("claims_used")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        if let Some(c) = json.get("category").and_then(|v| v.as_str()) {
                            category = c.to_string();
                        }
                    }
                }
                "token" => {
                    let json: serde_json::Value =
                        match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            Err(e) => {
                                emit_error(&app, &turn_id, format!("decode token: {e}"));
                                return;
                            }
                        };
                    if let Some(text) = json.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            if !emitted_any {
                                tracing::info!(turn_id = %turn_id, "chat: first token");
                                emitted_any = true;
                            }
                            full_text.push_str(text);
                            let _ = app.emit(
                                "chat-event",
                                ChatEvent::Token {
                                    turn_id: turn_id.clone(),
                                    text: text.to_string(),
                                },
                            );
                        }
                    }
                }
                "final" => {
                    if let Ok(json) =
                        serde_json::from_str::<serde_json::Value>(&ev.data)
                    {
                        if let Some(c) =
                            json.get("claims_used").and_then(|v| v.as_u64())
                        {
                            claims_used = c as usize;
                        }
                        if let Some(c) = json.get("category").and_then(|v| v.as_str()) {
                            category = c.to_string();
                        }
                    }
                    tracing::info!(turn_id = %turn_id, claims_used, "chat: emitting final");
                    let _ = app.emit(
                        "chat-event",
                        ChatEvent::Final {
                            turn_id: turn_id.clone(),
                            full_text: full_text.clone(),
                            claims_used,
                            category: category.clone(),
                            conversation_id: conv.clone(),
                        },
                    );
                    let _ = workspace;
                    return;
                }
                "error" => {
                    let msg = serde_json::from_str::<serde_json::Value>(&ev.data)
                        .ok()
                        .and_then(|v| {
                            v.get("message")
                                .and_then(|m| m.as_str())
                                .map(|s| s.to_string())
                        })
                        .unwrap_or_else(|| "(no message)".to_string());
                    emit_error(&app, &turn_id, msg);
                    return;
                }
                _ => { /* keep-alive comments / unknown events: ignore */ }
            },
        }
    }

    // Stream ended without `final` — surface as error so the UI never
    // gets stuck in "Generating…". Dropping silently is the failure
    // mode the rewrite is meant to eliminate.
    emit_error(
        &app,
        &turn_id,
        "stream closed without final event".to_string(),
    );
}

fn emit_error(app: &AppHandle, turn_id: &str, message: String) {
    let _ = app.emit(
        "chat-event",
        ChatEvent::Error {
            turn_id: turn_id.to_string(),
            message,
        },
    );
}

// ─── LLM health (pre-flight) ─────────────────────────────────

/// Mirror of the engine's `LlmHealthBody` so the UI gets one round-trip
/// and a stable shape when deciding whether to render the
/// "no LLM configured" banner.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LlmHealth {
    pub configured: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub claim_count: usize,
    pub mounted: bool,
}

/// Tauri command — pre-flight check the chat surface calls on workspace
/// switch. The desktop never blocks send on the result; it just renders
/// a banner so users with an unconfigured workspace know *before* they
/// type 200 chars why the answer won't come.
#[tauri::command]
pub async fn llm_health(app: AppHandle, workspace: String) -> Result<LlmHealth, String> {
    let state = app.state::<AppState>();
    let sidecar = state.sidecar.lock().await.clone();
    let Some(sidecar) = sidecar else {
        // No sidecar yet — treat as "not configured" so the UI can show
        // the same banner shape rather than spinning.
        return Ok(LlmHealth {
            configured: false,
            provider: None,
            model: None,
            claim_count: 0,
            mounted: false,
        });
    };

    let url = format!(
        "http://{}:{}/api/v1/ws/{}/llm/health",
        sidecar.host, sidecar.port, workspace
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("http client init failed: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("sidecar unreachable at {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("llm/health returned {}", resp.status()));
    }
    let parsed: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("decode response: {e}"))?;
    let data = parsed
        .get("data")
        .ok_or_else(|| "malformed response (no `data` field)".to_string())?;
    serde_json::from_value::<LlmHealth>(data.clone())
        .map_err(|e| format!("decode llm/health body: {e}"))
}
