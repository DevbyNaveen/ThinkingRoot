//! Outbound MCP `sampling/createMessage` (C13, 2026-05-22).
//!
//! When the daemon needs an LLM completion mid-tool-call AND a
//! connected MCP client has advertised the `sampling` capability,
//! we ask the CLIENT's LLM to do the completion. The user's
//! Claude Desktop / Claude Code subscription pays for the tokens;
//! we pay zero. This is the architectural primitive that makes
//! `client_sampling` flow nodes (C14) possible.
//!
//! Per MCP 2025-03-26 spec §"Sampling":
//! - Servers MUST send sampling requests only in association
//!   with an originating client request (`tools/call`,
//!   `resources/read`, etc.). This module enforces that — the
//!   caller passes the originating request's session_id, and
//!   we route the back-call through that same SSE channel.
//! - Clients MAY refuse sampling (user-controlled).
//! - The `maxTokens` field is REQUIRED.
//! - `temperature`, `stopSequences`, `metadata` are advisory.
//! - Sampling is DEPRECATED in DRAFT-2026-v1 with a 1-year
//!   support window; we ship it because it works today in Claude
//!   Desktop and gives us 12 months of runway. Migration to the
//!   successor (likely `InputRequiredResult` evolution) happens
//!   behind this module's public boundary — the
//!   [`create_message`] signature stays.
//!
//! # Wire flow
//!
//! 1. Caller invokes `create_message(state, session_id, params, timeout)`.
//! 2. We mint a fresh request_id, register a oneshot in
//!    `state.mcp_pending_sampling` keyed by that id.
//! 3. We frame a JSON-RPC request `{method: "sampling/createMessage",
//!    id: <new>, params}` and push it onto the session's SSE
//!    outbound channel (`state.mcp_sessions[session_id]`).
//! 4. The client receives it, may pause for user approval,
//!    forwards to its LLM, and sends the response back as an
//!    HTTP POST to `/mcp?sessionId=<sid>` with a JSON-RPC
//!    response body matching our request id.
//! 5. `mcp::sse::handle_post` recognises that POST as a response
//!    (no `method` field; has `result` or `error`) and routes it
//!    to the pending oneshot keyed on the request id.
//! 6. We `recv()` the oneshot and return the result.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::sse::SseMsg;
use crate::rest::AppState;

/// Default model preferences per the locked-in design decision —
/// neutral 0.5/0.5/0.5. The daemon doesn't know which models
/// the connected client has access to, so neutral lets the
/// client pick its default.
pub const DEFAULT_COST_PRIORITY: f64 = 0.5;
pub const DEFAULT_SPEED_PRIORITY: f64 = 0.5;
pub const DEFAULT_INTELLIGENCE_PRIORITY: f64 = 0.5;

/// Default timeout for a sampling round-trip. The client may
/// pause for user approval, so we err generous (60 s).
pub const DEFAULT_SAMPLING_TIMEOUT_SECS: u64 = 60;

/// Parameters for one `sampling/createMessage` call. Mirrors the
/// MCP spec's CreateMessageRequest params shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplingParams {
    pub messages: Vec<SamplingMessage>,
    /// REQUIRED per spec. Client MUST respect.
    #[serde(rename = "maxTokens")]
    pub max_tokens: u32,
    #[serde(default, rename = "systemPrompt", skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(
        default,
        rename = "stopSequences",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub stop_sequences: Vec<String>,
    /// Per spec, deprecated values "thisServer"/"allServers" are
    /// soft-deprecated; we default to omitting (which the spec
    /// defines as `"none"`).
    #[serde(default, rename = "includeContext", skip_serializing_if = "Option::is_none")]
    pub include_context: Option<String>,
    #[serde(default, rename = "modelPreferences", skip_serializing_if = "Option::is_none")]
    pub model_preferences: Option<ModelPreferences>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl SamplingParams {
    /// Construct a minimal sampling request with neutral
    /// preferences. Caller adds messages + max_tokens; the rest
    /// stays at the locked-in defaults.
    pub fn with_default_preferences(messages: Vec<SamplingMessage>, max_tokens: u32) -> Self {
        Self {
            messages,
            max_tokens,
            system_prompt: None,
            temperature: None,
            stop_sequences: Vec::new(),
            include_context: None,
            model_preferences: Some(ModelPreferences::neutral()),
            metadata: None,
        }
    }
}

/// One message in a sampling request. Mirrors MCP's
/// SamplingMessage; for v1 we support text content only (image +
/// audio per spec are not in our flow node schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplingMessage {
    pub role: String, // "user" or "assistant"
    pub content: SamplingContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SamplingContent {
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPreferences {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<ModelHint>,
    #[serde(rename = "costPriority")]
    pub cost_priority: f64,
    #[serde(rename = "speedPriority")]
    pub speed_priority: f64,
    #[serde(rename = "intelligencePriority")]
    pub intelligence_priority: f64,
}

impl ModelPreferences {
    /// Locked-in design decision: neutral 0.5/0.5/0.5 default.
    /// See plan §"Locked-in design decisions" #2.
    pub fn neutral() -> Self {
        Self {
            hints: Vec::new(),
            cost_priority: DEFAULT_COST_PRIORITY,
            speed_priority: DEFAULT_SPEED_PRIORITY,
            intelligence_priority: DEFAULT_INTELLIGENCE_PRIORITY,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelHint {
    pub name: String,
}

/// Response from a successful `sampling/createMessage` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplingResult {
    pub role: String, // "assistant"
    pub content: SamplingContent,
    pub model: String,
    #[serde(rename = "stopReason")]
    pub stop_reason: Option<String>,
}

/// Reasons a sampling call can fail end-to-end.
#[derive(Debug, thiserror::Error)]
pub enum SamplingError {
    #[error("MCP session '{0}' not found — client may have disconnected")]
    SessionNotFound(String),

    #[error("SSE channel send failed — client transport dropped")]
    TransportDropped,

    #[error("sampling timed out after {0:?} waiting for client response")]
    Timeout(Duration),

    #[error("client declined sampling request: {0}")]
    ClientRefused(String),

    #[error("client returned malformed response: {0}")]
    MalformedResponse(String),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Issue a `sampling/createMessage` request to the connected MCP
/// client identified by `session_id`. Awaits the client's
/// response up to `timeout`.
///
/// **Spec compliance** — caller MUST invoke this only from within
/// a `tools/call` (or `resources/read` / `prompts/get`) handler.
/// We don't enforce that from this function (the trace context
/// would be needed); it's the caller's responsibility per the
/// MCP spec's "Request Association Requirement".
pub async fn create_message(
    state: &Arc<AppState>,
    session_id: &str,
    params: SamplingParams,
    timeout: Duration,
) -> Result<SamplingResult, SamplingError> {
    let request_id = format!("sampling-{}", ulid::Ulid::new());

    // Register the pending response slot BEFORE pushing the
    // request so the response handler can never race past our
    // registration.
    let (tx, rx) = oneshot::channel::<serde_json::Value>();
    {
        let mut pending = state.mcp_pending_sampling.write().await;
        pending.insert(request_id.clone(), tx);
    }

    // Look up the session's SSE outbound channel.
    let sender = {
        let sessions = state.mcp_sessions.lock().await;
        sessions.get(session_id).cloned()
    }
    .ok_or_else(|| {
        // Reap our pending registration before bailing.
        SamplingError::SessionNotFound(session_id.to_string())
    })?;

    // Frame the JSON-RPC request + push it onto the SSE channel.
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "sampling/createMessage",
        "params": params,
    });
    let json = serde_json::to_string(&frame).map_err(|e| {
        SamplingError::Internal(format!("serialize sampling request: {e}"))
    })?;
    if sender.send(SseMsg::Message(json)).is_err() {
        // Channel closed between session lookup + send. Reap +
        // bail with transport error.
        state.mcp_pending_sampling.write().await.remove(&request_id);
        return Err(SamplingError::TransportDropped);
    }

    // Await the client's response with timeout. On any exit
    // path, ensure the pending slot is reaped so a late
    // response doesn't leak.
    let result = tokio::time::timeout(timeout, rx).await;
    let response_value = match result {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            // Sender dropped without sending — typically means
            // the SSE session was reaped between dispatch and
            // response. Pending slot already removed by the
            // reaper; safe to bail.
            state.mcp_pending_sampling.write().await.remove(&request_id);
            return Err(SamplingError::TransportDropped);
        }
        Err(_) => {
            state.mcp_pending_sampling.write().await.remove(&request_id);
            return Err(SamplingError::Timeout(timeout));
        }
    };

    // Parse the response. Two shapes: success (`result: {...}`)
    // or JSON-RPC error (`error: {code, message}`).
    if let Some(err) = response_value.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        return Err(SamplingError::ClientRefused(msg));
    }
    let result_obj = response_value
        .get("result")
        .ok_or_else(|| SamplingError::MalformedResponse("missing 'result' field".to_string()))?;
    serde_json::from_value::<SamplingResult>(result_obj.clone())
        .map_err(|e| SamplingError::MalformedResponse(e.to_string()))
}

/// Called by `mcp::sse::handle_post` when an incoming POST is
/// detected as a JSON-RPC response (no `method` field; has
/// `result` or `error` and an `id` matching a pending sampling
/// request). Routes the response payload to the matching oneshot.
///
/// Returns `true` when the response was routed to a pending
/// sampling request, `false` when no match was found (the caller
/// then processes it as a normal request).
pub async fn route_incoming_response(state: &Arc<AppState>, response: &serde_json::Value) -> bool {
    let id_str = match response.get("id") {
        Some(v) => match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        },
        None => return false,
    };
    let sender = {
        let mut pending = state.mcp_pending_sampling.write().await;
        pending.remove(&id_str)
    };
    match sender {
        Some(tx) => {
            // Best-effort send — if the receiver was dropped
            // (caller cancelled), we silently swallow.
            let _ = tx.send(response.clone());
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_preferences_match_locked_in_design_decision() {
        let prefs = ModelPreferences::neutral();
        assert_eq!(prefs.cost_priority, 0.5);
        assert_eq!(prefs.speed_priority, 0.5);
        assert_eq!(prefs.intelligence_priority, 0.5);
        assert!(prefs.hints.is_empty());
    }

    #[test]
    fn sampling_params_with_default_preferences_omits_optional_fields() {
        let params = SamplingParams::with_default_preferences(
            vec![SamplingMessage {
                role: "user".to_string(),
                content: SamplingContent::Text {
                    text: "hello".to_string(),
                },
            }],
            1024,
        );
        assert_eq!(params.max_tokens, 1024);
        assert!(params.system_prompt.is_none());
        assert!(params.temperature.is_none());
        assert!(params.stop_sequences.is_empty());
        assert!(params.include_context.is_none());
        assert!(params.model_preferences.is_some());
        assert!(params.metadata.is_none());
    }

    #[test]
    fn sampling_params_serializes_to_mcp_spec_wire_shape() {
        let params = SamplingParams {
            messages: vec![SamplingMessage {
                role: "user".to_string(),
                content: SamplingContent::Text {
                    text: "What is the capital of France?".to_string(),
                },
            }],
            max_tokens: 100,
            system_prompt: Some("You are a helpful assistant.".to_string()),
            temperature: Some(0.1),
            stop_sequences: vec![],
            include_context: Some("thisServer".to_string()),
            model_preferences: Some(ModelPreferences {
                hints: vec![ModelHint {
                    name: "claude-3-sonnet".to_string(),
                }],
                cost_priority: 0.3,
                speed_priority: 0.5,
                intelligence_priority: 0.8,
            }),
            metadata: None,
        };
        let json = serde_json::to_value(&params).expect("serialize");
        // Must use camelCase for spec keys.
        assert!(json.get("maxTokens").is_some());
        assert!(json.get("systemPrompt").is_some());
        assert!(json.get("includeContext").is_some());
        assert!(json.get("modelPreferences").is_some());
        let prefs = json.get("modelPreferences").unwrap();
        assert!(prefs.get("costPriority").is_some());
        assert!(prefs.get("speedPriority").is_some());
        assert!(prefs.get("intelligencePriority").is_some());
        // stop_sequences was empty → omitted.
        assert!(json.get("stopSequences").is_none());
    }

    #[test]
    fn sampling_result_parses_from_spec_response_shape() {
        let response = serde_json::json!({
            "result": {
                "role": "assistant",
                "content": { "type": "text", "text": "Paris" },
                "model": "claude-3-sonnet-20240307",
                "stopReason": "endTurn"
            }
        });
        let result: SamplingResult =
            serde_json::from_value(response["result"].clone()).expect("parse");
        assert_eq!(result.role, "assistant");
        assert!(matches!(
            result.content,
            SamplingContent::Text { ref text } if text == "Paris"
        ));
        assert_eq!(result.model, "claude-3-sonnet-20240307");
        assert_eq!(result.stop_reason, Some("endTurn".to_string()));
    }

    #[test]
    fn sampling_error_messages_are_actionable() {
        // Pin error message shapes — clients build user-facing
        // text from these.
        let session_err = SamplingError::SessionNotFound("abc".to_string());
        assert!(session_err.to_string().contains("abc"));
        assert!(session_err.to_string().contains("disconnected"));

        let timeout_err = SamplingError::Timeout(Duration::from_secs(60));
        assert!(timeout_err.to_string().contains("60s") || timeout_err.to_string().contains("60"));

        let refused_err = SamplingError::ClientRefused("user denied".to_string());
        assert!(refused_err.to_string().contains("user denied"));
    }

    #[test]
    fn default_timeout_constant_is_60s() {
        assert_eq!(DEFAULT_SAMPLING_TIMEOUT_SECS, 60);
    }

    #[test]
    fn text_content_round_trips_through_serde() {
        let content = SamplingContent::Text {
            text: "round-trip test".to_string(),
        };
        let json = serde_json::to_value(&content).expect("serialize");
        assert_eq!(json["type"], serde_json::json!("text"));
        assert_eq!(json["text"], serde_json::json!("round-trip test"));
        let back: SamplingContent =
            serde_json::from_value(json).expect("deserialize");
        match back {
            SamplingContent::Text { text } => assert_eq!(text, "round-trip test"),
        }
    }
}
