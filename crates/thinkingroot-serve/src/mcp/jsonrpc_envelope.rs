//! C5 (2026-05-22) — token budget + structured-error envelope
//! adapters for MCP responses.
//!
//! Two responsibilities:
//!
//! 1. **Token budget on every text result.** External MCP clients
//!    (Claude Code, Cursor, Codex) consume tool results into their
//!    LLM context windows the same way the in-app agent does — an
//!    unbounded 50 MB response from `list_witnesses(limit=100000)`
//!    is just as harmful out there as it is in here. We apply the
//!    same `DEFAULT_TOOL_RESULT_TOKEN_BUDGET = 8_192` cap that the
//!    in-app agent already uses (`intelligence/token_budget.rs:37`).
//!    Over-budget payloads are truncated head+tail with the same
//!    `TRUNCATION_MARKER` the agent loop produces, so the LLM sees
//!    a familiar shape and can re-issue a narrower query.
//!
//! 2. **Structured-error envelope parity with the in-app agent.**
//!    `intelligence/tools.rs:84` defines a `ToolHandlerResult::
//!    structured_error(error_type, hint, retryable)` envelope used
//!    by the in-app agent loop. The MCP layer needs the same wire
//!    shape so external AIs can branch on `error_type` (e.g.
//!    `retryable: true` → back off and retry; `not_found` → give
//!    up; `result_too_large` → narrow the query). Without this
//!    adapter, MCP errors land as plain JSON-RPC `error: { code,
//!    message }` strings that clients can't easily classify.
//!
//! The wire shape for `structured_error` follows the in-app
//! pattern verbatim:
//!
//! ```json
//! {
//!   "ok": false,
//!   "error_type": "<canonical kind>",
//!   "hint": "<actionable next step or null>",
//!   "retryable": true|false
//! }
//! ```
//!
//! Delivered as a normal `result.content[0].text` block (NOT as a
//! JSON-RPC `error` envelope) so the LLM sees it through the
//! tool-result reading path it already uses for happy-path output.
//! `is_error: true` is set on the content block so the client can
//! visually flag it. The legacy `JsonRpcResponse::error` path
//! remains valid for protocol-level errors (invalid params,
//! method-not-found, etc.) — `structured_error` is exclusively for
//! tool-level errors the LLM should reason about.

use serde::Serialize;
use serde_json::Value;

use super::JsonRpcResponse;
use crate::intelligence::token_budget::{
    DEFAULT_TOOL_RESULT_TOKEN_BUDGET, truncate_tool_result_with_stats,
};

/// Canonical `error_type` values the MCP layer emits. Kept in sync
/// with the in-app `ToolHandlerResult::structured_error` doc on
/// `intelligence/tools.rs:84` so the LLM's mental model of error
/// codes is identical across both surfaces.
pub mod error_type {
    /// Required input field absent or null.
    pub const MISSING_FIELD: &str = "missing_field";
    /// Input shape wrong (out-of-range, wrong variant, etc.).
    pub const INVALID_ARGUMENT: &str = "invalid_argument";
    /// Referenced entity / workspace / file / branch absent.
    pub const NOT_FOUND: &str = "not_found";
    /// User declined approval, or RBAC denied.
    pub const PERMISSION_DENIED: &str = "permission_denied";
    /// Engine / external dep errored.
    pub const UPSTREAM_FAILED: &str = "upstream_failed";
    /// Query succeeded but matched nothing.
    pub const EMPTY_RESULT: &str = "empty_result";
    /// Vector index needs rebuild.
    pub const STALE_INDEX: &str = "stale_index";
    /// Network blip / lock contention / quota hit — retryable.
    pub const TRANSIENT: &str = "transient";
    /// Tool response exceeded the per-call token budget; client
    /// should narrow scope (lower `limit`, tighter filter).
    pub const RESULT_TOO_LARGE: &str = "result_too_large";
    /// Tool requires a transport feature the current connection
    /// doesn't carry (e.g., `get_reminder_context` over stdio).
    pub const TRANSPORT_NOT_SUPPORTED: &str = "transport_not_supported";
    /// Tool was cancelled before or during dispatch.
    pub const CANCELLED: &str = "cancelled";
}

/// Build a `result.content[0].text` payload carrying a structured
/// error envelope. See module docs for the wire shape.
///
/// `hint` is `Option<String>` so callers can omit it (the JSON
/// `null` is honest about absence — no fabricated hint).
pub fn structured_error(
    id: Option<Value>,
    error_type: &str,
    hint: Option<String>,
    retryable: bool,
) -> JsonRpcResponse {
    let envelope = serde_json::json!({
        "ok": false,
        "error_type": error_type,
        "hint": hint,
        "retryable": retryable,
    });
    let text = envelope.to_string();
    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": text,
                "isError": true,
            }]
        }),
    )
}

/// Variant of `mcp_text_result` that consults
/// [`DEFAULT_TOOL_RESULT_TOKEN_BUDGET`] before emitting. When the
/// serialized payload exceeds the budget, the response is replaced
/// with a `structured_error` of kind `result_too_large` carrying a
/// hint suggesting the caller narrow scope. The estimated token
/// count + actual byte size are surfaced in the hint so the client
/// has a concrete signal to act on.
///
/// Drop-in compatible with the legacy `mcp_text_result` shape when
/// the payload fits — same `{content: [{type: "text", text}]}`
/// envelope, byte-identical for under-budget responses.
pub fn mcp_text_result_bounded<T: Serialize>(
    id: Option<Value>,
    payload: &T,
) -> JsonRpcResponse {
    let content = match serde_json::to_string_pretty(payload) {
        Ok(s) => s,
        Err(e) => {
            return structured_error(
                id,
                error_type::UPSTREAM_FAILED,
                Some(format!("serialize result: {e}")),
                false,
            );
        }
    };

    let outcome = truncate_tool_result_with_stats(content, DEFAULT_TOOL_RESULT_TOKEN_BUDGET);

    if outcome.truncated {
        // Hard cap reached. We could return the truncated content
        // (the agent loop does), but for MCP we surface a typed
        // error so the external AI knows to narrow scope rather
        // than silently consuming truncated data. Honesty: the
        // truncation marker IS in `outcome.bounded`, but clients
        // tend to act on a typed error envelope sooner than they
        // act on a truncation marker buried mid-string.
        let hint = format!(
            "tool response of {} bytes (~{} tokens) exceeded the {} token cap. \
             Narrow the query (lower `limit`, tighter `branch` / `workspace` / `query`); \
             the truncated payload is bounded at {} bytes if you retry.",
            outcome.original_bytes,
            outcome.original_bytes / 4, // matches the in-app 4-chars-per-token estimator
            DEFAULT_TOOL_RESULT_TOKEN_BUDGET,
            outcome.llm_bytes,
        );
        return structured_error(id, error_type::RESULT_TOO_LARGE, Some(hint), true);
    }

    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "content": [{ "type": "text", "text": outcome.bounded }],
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_error_emits_well_formed_envelope_through_result_path() {
        let resp = structured_error(
            Some(serde_json::json!(1)),
            error_type::NOT_FOUND,
            Some("workspace 'ws-z' is not mounted on this daemon".to_string()),
            false,
        );
        let v = serde_json::to_value(&resp).expect("serialize");

        // Must be a result envelope (not a JSON-RPC error envelope).
        assert!(v["result"].is_object());
        assert!(v.get("error").is_none() || v["error"].is_null());

        // Drill into the content[0] text and re-parse the inner JSON.
        let text = v["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        let envelope: serde_json::Value =
            serde_json::from_str(text).expect("inner envelope JSON");
        assert_eq!(envelope["ok"], serde_json::json!(false));
        assert_eq!(envelope["error_type"], serde_json::json!("not_found"));
        assert_eq!(envelope["retryable"], serde_json::json!(false));
        assert!(envelope["hint"].as_str().unwrap().contains("ws-z"));

        // The content block must carry isError: true.
        assert_eq!(
            v["result"]["content"][0]["isError"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn structured_error_accepts_null_hint() {
        let resp = structured_error(
            Some(serde_json::json!(2)),
            error_type::EMPTY_RESULT,
            None,
            false,
        );
        let v = serde_json::to_value(&resp).expect("serialize");
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        let envelope: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(envelope["hint"], serde_json::Value::Null);
    }

    #[test]
    fn mcp_text_result_bounded_passes_through_small_payloads() {
        let payload = serde_json::json!({ "claims": [], "count": 0 });
        let resp = mcp_text_result_bounded(Some(serde_json::json!(1)), &payload);
        let v = serde_json::to_value(&resp).expect("serialize");
        // Small payload → normal result envelope.
        assert!(v["result"].is_object());
        let text = v["result"]["content"][0]["text"].as_str().expect("text");
        // Must be the pretty-printed JSON, not a truncation marker.
        assert!(text.contains("\"count\""));
        assert!(!text.contains("truncated for token budget"));
    }

    #[test]
    fn mcp_text_result_bounded_returns_too_large_for_oversized_payload() {
        // Build a payload that's guaranteed to exceed the 8K-token
        // budget (~32K bytes at 4 chars/token). 50K bytes of "x"
        // gives a generous overshoot.
        let huge = "x".repeat(50_000);
        let payload = serde_json::json!({ "blob": huge });
        let resp = mcp_text_result_bounded(Some(serde_json::json!(1)), &payload);
        let v = serde_json::to_value(&resp).expect("serialize");
        let text = v["result"]["content"][0]["text"].as_str().expect("text");
        let envelope: serde_json::Value = serde_json::from_str(text).expect("inner JSON");
        assert_eq!(envelope["error_type"], serde_json::json!("result_too_large"));
        assert_eq!(envelope["retryable"], serde_json::json!(true));
        assert!(envelope["hint"].as_str().unwrap().contains("Narrow"));
        // isError flag on the content block.
        assert_eq!(
            v["result"]["content"][0]["isError"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn error_type_constants_are_stable_snake_case() {
        // Pin the canonical kinds — external clients build switch
        // statements on these strings, so changing them is a wire
        // break. Snake_case matches the in-app
        // `ToolHandlerResult::structured_error` doc convention.
        assert_eq!(error_type::MISSING_FIELD, "missing_field");
        assert_eq!(error_type::INVALID_ARGUMENT, "invalid_argument");
        assert_eq!(error_type::NOT_FOUND, "not_found");
        assert_eq!(error_type::PERMISSION_DENIED, "permission_denied");
        assert_eq!(error_type::UPSTREAM_FAILED, "upstream_failed");
        assert_eq!(error_type::EMPTY_RESULT, "empty_result");
        assert_eq!(error_type::STALE_INDEX, "stale_index");
        assert_eq!(error_type::TRANSIENT, "transient");
        assert_eq!(error_type::RESULT_TOO_LARGE, "result_too_large");
        assert_eq!(
            error_type::TRANSPORT_NOT_SUPPORTED,
            "transport_not_supported"
        );
        assert_eq!(error_type::CANCELLED, "cancelled");
    }

    #[test]
    fn serialize_failure_falls_back_to_upstream_failed_envelope() {
        // serde_json can't pretty-print a NaN. Pin the fallback path.
        let payload = serde_json::json!({ "x": f64::NAN });
        // Wait — serde_json silently converts NaN to null at
        // serialize time. So this won't error. Use a custom type
        // that always errors on serialize.
        struct ErroringSer;
        impl serde::Serialize for ErroringSer {
            fn serialize<S: serde::Serializer>(&self, _ser: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("forced serialize failure"))
            }
        }
        let _ = payload;
        let resp = mcp_text_result_bounded(Some(serde_json::json!(1)), &ErroringSer);
        let v = serde_json::to_value(&resp).expect("serialize wrapper");
        let text = v["result"]["content"][0]["text"].as_str().expect("text");
        let envelope: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(envelope["error_type"], serde_json::json!("upstream_failed"));
        assert_eq!(envelope["retryable"], serde_json::json!(false));
        assert!(
            envelope["hint"]
                .as_str()
                .unwrap()
                .contains("forced serialize failure")
        );
    }
}

