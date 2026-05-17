// crates/thinkingroot-serve/src/intelligence/mcp_bridge.rs
//
// McpBridge — single ToolHandler that surfaces every safe MCP tool
// (per `mcp::tools::handle_list`) inside the desktop chat agent's
// ToolRegistry. One adapter, N tool registrations: avoids the bug
// surface of N hand-wrapped tool structs and keeps the in-app agent
// byte-for-byte equivalent to external MCP clients on every tool
// call.
//
// Architecture:
//   - At registry build time (`register_mcp_bridge_tools`), the
//     bridge awaits `mcp::tools::handle_list(None)`, walks the
//     returned tool array, and registers each tool through the
//     existing `ToolRegistry::register_{read,write}` paths.
//   - At dispatch time, `McpBridgeTool::handle` injects the
//     workspace name into the arguments JSON (the chat surface
//     pins one workspace per agent run; the agent shouldn't have
//     to repeat it in every tool call), builds the MCP wire shape
//     `{"name":..., "arguments":...}`, and calls
//     `mcp::tools::handle_call` with the shared `ToolContext` —
//     SAME engine read-guard pattern as every other builtin tool.
//   - The MCP response (a `JsonRpcResponse` struct with
//     mutually-exclusive `result` / `error` fields) is flattened
//     into a `ToolHandlerResult`: success → text content extracted
//     and joined; error → `is_error: true` with the JSON-RPC
//     message + code.
//
// Skip + classification policy:
//
//   BRIDGE_SKIP_NAMES — tools refused from the bridge:
//     * Names already registered by the hand-wrapped builtins (those
//       have different argument schemas / response shapes the in-app
//       UI depends on — `search`, `list_branches`, `commit_cognition`,
//       etc.). Bridging them would duplicate-register and panic via
//       `ToolRegistry::register`'s contains-check.
//     * `contribute_bulk` — `mcp::tools` rejects non-`Principal::Connector`
//       callers at the API boundary; a chat agent is `Principal::Agent`,
//       so the bridge call would always reject. Dead code if exposed.
//     * `compile` — the legacy `mcp::tools` "compile" arm calls
//       `engine.compile()` but skips the post-compile remount +
//       engram-invalidation owned by the desktop's `ChatView.tsx`
//       `compileToolWorkspace` bridge. Skip; that bridge is the
//       canonical AI-driven compile path.
//
//   BRIDGE_WRITE_NAMES — tools that flip workspace state and must
//     route through the existing `ApprovalGate` before dispatch.
//     Same gating discipline as the hand-wrapped write tools.
//
// Result extraction:
//
//   `mcp::tools` wraps every success in
//     {"content": [{"type": "text", "text": "..."}], ...}
//   via the local `mcp_text_result` / `mcp_text_results` helpers.
//   The bridge concatenates every `text` content block with a
//   blank-line separator so multi-content responses still render as
//   one cohesive payload for the LLM. The MCP convention that a
//   tool-level failure surfaces as `isError: true` inside the result
//   envelope (not at the JSON-RPC level) is honoured.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use thinkingroot_llm::llm::Tool;

use crate::intelligence::builtin_tools::ToolContext;
use crate::intelligence::tools::{ToolHandler, ToolHandlerResult, ToolRegistry};
use crate::mcp::JsonRpcResponse;
use crate::mcp::tools as mcp_tools;

/// Tool names refused from the bridge. Two reasons a name lives here:
/// (1) the hand-wrapped builtin already owns the name and the two
/// schemas diverge enough that the desktop UI depends on the builtin
/// shape; (2) the MCP arm rejects the principal a chat agent uses
/// (`contribute_bulk` is Connector-only) or has out-of-band
/// orchestration the bridge doesn't replicate (`compile` skips the
/// post-compile remount + engram-invalidation owned by the
/// `ChatView.tsx` compile bridge).
pub const BRIDGE_SKIP_NAMES: &[&str] = &[
    // Already registered by the hand-wrapped builtins — same names,
    // different schemas / response shapes the desktop UI is tuned to.
    "commit_cognition",
    "create_branch",
    "ingest_path",
    "list_branches",
    "list_commits",
    "list_directory",
    "merge_branch",
    "merge_cognition",
    "organize_files",
    "regenerate_paper",
    "save_note",
    "search",
    "trash_files",
    // Connector-only at the API boundary; bridge call would always reject.
    "contribute_bulk",
    // Skips post-compile remount + engram invalidation — desktop's
    // ChatView.tsx `compileToolWorkspace` bridge owns the canonical
    // AI-driven compile path.
    "compile",
];

/// Tools that mutate workspace state when called. Registered through
/// `register_write` so the agent's `ApprovalGate` is consulted before
/// dispatch — same discipline as the existing hand-wrapped write
/// tools (`create_branch`, `contribute_claim`, …).
///
/// Anything NOT in this list (and not in [`BRIDGE_SKIP_NAMES`]) is
/// registered as read-only.
pub const BRIDGE_WRITE_NAMES: &[&str] = &[
    // Branch lifecycle
    "checkout_branch",
    "delete_branch",
    "rebase_branch",
    "rollback_merge",
    "gc_branches",
    // Proposal lifecycle
    "open_proposal",
    "close_proposal",
    "review_proposal",
    "dismiss_gap",
    // Substrate writes
    "contribute",
    "observe_turn",
    "flush_observations",
    "synthesize_merge",
    // Filesystem mutation
    "fs_create_folder",
    "fs_rename",
    "fs_move",
    // Engram lifecycle (mutates per-session engram store)
    "expire_engram",
    // Phase D Wave 1 — system-power tools. `file_read`, `glob`, and
    // `grep` are intentionally listed as write-class even though
    // they only read from disk: they exfiltrate file contents into
    // the LLM context window, which flows to Anthropic/Azure/OpenAI.
    // Treating them as write-class routes them through the
    // PermissionsGate just like writes — DEFAULT_DENY paths
    // (~/.ssh, ~/.aws, browser profiles) are refused without
    // prompting.
    "file_read",
    "file_write",
    "file_edit",
    "glob",
    "grep",
    "shell_exec",
    "clipboard_read",
    "clipboard_write",
    "open_in_default",
    "trash",
];

/// One bridge handler instance. Cloned (via Arc) into the registry
/// per registered tool name; each instance knows its own tool name
/// and shares the same `ToolContext` (engine, workspace, session,
/// engram manager) with every other builtin handler.
pub struct McpBridgeTool {
    /// Wire-level MCP tool name forwarded into
    /// `mcp::tools::handle_call`'s `name` parameter.
    name: String,
    ctx: ToolContext,
}

impl McpBridgeTool {
    pub fn new(name: impl Into<String>, ctx: ToolContext) -> Self {
        Self {
            name: name.into(),
            ctx,
        }
    }

    /// Build a `Tool` spec from the raw catalog entry returned by
    /// `mcp::tools::handle_list`. Returns `None` when the entry is
    /// malformed (missing `name` / `description` / `inputSchema`),
    /// so the bridge never registers a half-built tool.
    fn spec_from_catalog(entry: &Value) -> Option<Tool> {
        let name = entry.get("name").and_then(|v| v.as_str())?;
        let description = entry.get("description").and_then(|v| v.as_str())?;
        let schema = entry.get("inputSchema")?.clone();
        Some(Tool::new(name, description, schema))
    }
}

#[async_trait]
impl ToolHandler for McpBridgeTool {
    async fn handle(&self, input: Value) -> ToolHandlerResult {
        // The chat surface pins one workspace per agent run; the
        // LLM shouldn't have to repeat it in every tool call. Inject
        // when missing so the bridge stays a drop-in shim for tools
        // whose schemas mark `workspace` required.
        let mut args = match input {
            Value::Object(map) => Value::Object(map),
            Value::Null => Value::Object(Default::default()),
            other => {
                return ToolHandlerResult::error(format!(
                    "MCP bridge tool '{}' expected an object argument, got: {}",
                    self.name, other
                ));
            }
        };
        if let Value::Object(ref mut map) = args {
            map.entry("workspace")
                .or_insert_with(|| Value::String(self.ctx.workspace.clone()));
        }

        let params = json!({
            "name": &self.name,
            "arguments": args,
        });

        let engine = self.ctx.engine.read().await;
        let response = mcp_tools::handle_call(
            None,
            &params,
            &*engine,
            Some(&self.ctx.workspace),
            &self.ctx.session_id,
            &self.ctx.sessions,
            &self.ctx.engram_manager,
        )
        .await;
        drop(engine);

        flatten_jsonrpc_response(&self.name, response)
    }
}

/// Walk the catalog returned by `mcp::tools::handle_list`, build a
/// `McpBridgeTool` for every entry not in [`BRIDGE_SKIP_NAMES`], and
/// chain it onto `registry`. Read vs write is decided by
/// [`BRIDGE_WRITE_NAMES`]. Returns the extended registry.
///
/// The catalog is fetched at registry-build time (once per agent
/// run), so the in-app agent always sees the same tools the external
/// MCP clients do — no second hardcoded list to drift from
/// `mcp/tools.rs`.
///
/// Tools the catalog advertises but the registry already contains
/// (the 13 hand-wrapped builtins that share names with MCP entries —
/// `search`, `list_branches`, etc.) are skipped via
/// [`BRIDGE_SKIP_NAMES`]; the bridge never duplicate-registers.
pub async fn register_mcp_bridge_tools(
    mut registry: ToolRegistry,
    ctx: &ToolContext,
) -> ToolRegistry {
    let entries = fetch_catalog_entries().await;
    for entry in entries.iter() {
        let Some(spec) = McpBridgeTool::spec_from_catalog(entry) else {
            tracing::warn!(
                target: "mcp_bridge",
                entry = ?entry,
                "skipping malformed catalog entry — missing name/description/inputSchema",
            );
            continue;
        };
        if BRIDGE_SKIP_NAMES.contains(&spec.name.as_str()) {
            continue;
        }
        let handler = Arc::new(McpBridgeTool::new(spec.name.clone(), ctx.clone()));
        // E.6 (2026-05-17): the const BRIDGE_WRITE_NAMES covers the 64
        // hardcoded built-ins; the trait-registry path covers any tool
        // registered post-startup via `tool_trait::register_tool`. A
        // tool that declares `is_write() = true` auto-routes through
        // the write-class gate without manual addition to the const.
        let is_write = BRIDGE_WRITE_NAMES.contains(&spec.name.as_str())
            || crate::mcp::tool_trait::is_registered_write(&spec.name);
        if is_write {
            registry = registry.register_write(spec, handler);
        } else {
            registry = registry.register_read(spec, handler);
        }
    }
    registry
}

/// Fetch the inner `tools` array from `mcp::tools::handle_list`.
/// Returns an empty Vec on protocol error so the agent still boots
/// with the hand-wrapped builtins — losing the bridge is recoverable;
/// crashing the agent isn't.
async fn fetch_catalog_entries() -> Vec<Value> {
    let response = mcp_tools::handle_list(None).await;
    if let Some(err) = response.error.as_ref() {
        tracing::error!(
            target: "mcp_bridge",
            code = err.code,
            message = %err.message,
            "mcp::tools::handle_list returned an error — bridge will register zero tools",
        );
        return Vec::new();
    }
    response
        .result
        .as_ref()
        .and_then(|v| v.get("tools"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Collapse a JSON-RPC response into a tool result the agent LLM can
/// read. Success payloads coming from `mcp::tools` always have the
/// shape `{"content": [{"type":"text", "text": "..."}], ...}` — we
/// concatenate every text block with a blank line so multi-content
/// responses still render as one cohesive answer. Errors are surfaced
/// with `is_error: true` so the model can retry or surface to the
/// user.
fn flatten_jsonrpc_response(tool_name: &str, response: JsonRpcResponse) -> ToolHandlerResult {
    if let Some(err) = response.error {
        return ToolHandlerResult::error(format!(
            "{tool_name} failed (code {}): {}",
            err.code, err.message
        ));
    }
    let Some(result) = response.result else {
        // Both fields None is malformed per JSON-RPC; surface
        // honestly rather than silently OK-ing an empty payload.
        return ToolHandlerResult::error(format!(
            "{tool_name} returned a JSON-RPC response with neither `result` nor `error`",
        ));
    };
    // Honour an explicit `isError: true` on the result — the MCP
    // convention is that a tool-level failure surfaces inside the
    // result envelope, not at the JSON-RPC level.
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let text = extract_text_blocks(&result).unwrap_or_else(|| {
        // Fall back to the raw JSON so the LLM still has something
        // to reason about. Honest about the shape.
        result.to_string()
    });
    if is_error {
        ToolHandlerResult::error(text)
    } else {
        ToolHandlerResult::ok(text)
    }
}

/// Extract every `text` content block from an MCP result payload and
/// join with blank lines. Returns `None` when the payload doesn't
/// match the MCP content-block shape; callers fall back to the raw
/// JSON in that case.
fn extract_text_blocks(result: &Value) -> Option<String> {
    let blocks = result.get("content")?.as_array()?;
    let mut out = String::new();
    let mut wrote = false;
    for block in blocks {
        let Some(kind) = block.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if kind != "text" {
            continue;
        }
        let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        if wrote {
            out.push_str("\n\n");
        }
        out.push_str(text);
        wrote = true;
    }
    if wrote { Some(out) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn skip_list_covers_hand_wrapped_names() {
        // Every name the hand-wrapped `register_builtin_tools` owns
        // and that also appears in the MCP catalog must be in the
        // skip list — otherwise registering both would panic on a
        // duplicate-name collision when `register_mcp_bridge_tools`
        // runs after `register_builtin_tools`.
        for shared in [
            "commit_cognition",
            "create_branch",
            "ingest_path",
            "list_branches",
            "list_commits",
            "list_directory",
            "merge_branch",
            "merge_cognition",
            "organize_files",
            "regenerate_paper",
            "save_note",
            "search",
            "trash_files",
        ] {
            assert!(
                BRIDGE_SKIP_NAMES.contains(&shared),
                "shared name '{shared}' missing from BRIDGE_SKIP_NAMES — \
                 ToolRegistry will panic on duplicate registration",
            );
        }
    }

    #[test]
    fn skip_list_covers_connector_only_and_compile() {
        assert!(BRIDGE_SKIP_NAMES.contains(&"contribute_bulk"));
        assert!(BRIDGE_SKIP_NAMES.contains(&"compile"));
    }

    #[test]
    fn write_list_does_not_overlap_skip_list() {
        for w in BRIDGE_WRITE_NAMES {
            assert!(
                !BRIDGE_SKIP_NAMES.contains(w),
                "tool '{w}' is in both BRIDGE_WRITE_NAMES and BRIDGE_SKIP_NAMES — \
                 skip would mean it's never registered, write classification is dead",
            );
        }
    }

    #[test]
    fn spec_from_catalog_accepts_well_formed_entry() {
        let entry = json!({
            "name": "hybrid_retrieve",
            "description": "Top-tier retrieval over the substrate.",
            "inputSchema": {"type": "object", "properties": {}},
        });
        let spec = McpBridgeTool::spec_from_catalog(&entry).expect("well-formed");
        assert_eq!(spec.name, "hybrid_retrieve");
        assert_eq!(spec.description, "Top-tier retrieval over the substrate.");
        assert_eq!(spec.input_schema["type"], "object");
    }

    #[test]
    fn spec_from_catalog_rejects_missing_fields() {
        let no_name = json!({
            "description": "x",
            "inputSchema": {},
        });
        assert!(McpBridgeTool::spec_from_catalog(&no_name).is_none());
        let no_desc = json!({
            "name": "x",
            "inputSchema": {},
        });
        assert!(McpBridgeTool::spec_from_catalog(&no_desc).is_none());
        let no_schema = json!({
            "name": "x",
            "description": "y",
        });
        assert!(McpBridgeTool::spec_from_catalog(&no_schema).is_none());
    }

    #[test]
    fn extract_text_blocks_joins_with_blank_lines() {
        let result = json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ],
        });
        let joined = extract_text_blocks(&result).expect("joined");
        assert_eq!(joined, "first\n\nsecond");
    }

    #[test]
    fn extract_text_blocks_skips_non_text_blocks() {
        let result = json!({
            "content": [
                {"type": "image", "data": "..."},
                {"type": "text", "text": "kept"},
            ],
        });
        let joined = extract_text_blocks(&result).expect("kept");
        assert_eq!(joined, "kept");
    }

    #[test]
    fn extract_text_blocks_returns_none_when_no_content() {
        let result = json!({"status": "ok"});
        assert!(extract_text_blocks(&result).is_none());
    }

    #[test]
    fn flatten_success_returns_ok_text_result() {
        let response = JsonRpcResponse::success(
            None,
            json!({
                "content": [{"type": "text", "text": "done"}],
            }),
        );
        let result = flatten_jsonrpc_response("hybrid_retrieve", response);
        assert!(!result.is_error);
        assert_eq!(result.content, "done");
    }

    #[test]
    fn flatten_success_honors_is_error_envelope() {
        let response = JsonRpcResponse::success(
            None,
            json!({
                "content": [{"type": "text", "text": "boom"}],
                "isError": true,
            }),
        );
        let result = flatten_jsonrpc_response("probe_engram", response);
        assert!(result.is_error);
        assert_eq!(result.content, "boom");
    }

    #[test]
    fn flatten_jsonrpc_error_surfaces_code_and_message() {
        let response = JsonRpcResponse::error(None, -32602, "missing workspace".to_string());
        let result = flatten_jsonrpc_response("ask", response);
        assert!(result.is_error);
        assert!(result.content.contains("-32602"));
        assert!(result.content.contains("missing workspace"));
        assert!(result.content.contains("ask"));
    }

    #[test]
    fn flatten_falls_back_to_raw_json_when_no_text_content() {
        let response = JsonRpcResponse::success(None, json!({"foo": "bar"}));
        let result = flatten_jsonrpc_response("walk_mesh", response);
        assert!(!result.is_error);
        assert!(result.content.contains("foo"));
        assert!(result.content.contains("bar"));
    }

    #[test]
    fn flatten_handles_response_with_neither_result_nor_error() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: None,
            result: None,
            error: None,
        };
        let result = flatten_jsonrpc_response("ask", response);
        assert!(result.is_error);
        assert!(result.content.contains("neither `result` nor `error`"));
    }

    #[tokio::test]
    async fn fetch_catalog_returns_nonempty_array() {
        // Wires the live `mcp::tools::handle_list` — the contract is
        // that handle_list always returns the full MCP catalog. Empty
        // result means the catalog wiring broke.
        let entries = fetch_catalog_entries().await;
        assert!(
            !entries.is_empty(),
            "mcp::tools::handle_list returned no tools — bridge would register zero handlers",
        );
        // Every entry must carry the three fields the spec helper
        // depends on.
        for entry in entries.iter() {
            assert!(
                entry.get("name").is_some(),
                "catalog entry missing `name`: {entry}",
            );
            assert!(
                entry.get("description").is_some(),
                "catalog entry missing `description`: {entry}",
            );
            assert!(
                entry.get("inputSchema").is_some(),
                "catalog entry missing `inputSchema`: {entry}",
            );
        }
    }

    #[tokio::test]
    async fn fetch_catalog_includes_marquee_tools() {
        // Smoke test the catalog wiring with three tools that MUST
        // remain MCP-exposed: hybrid_retrieve (read), list_witnesses
        // (Witness Mesh read path), and materialize_engram (AEP).
        let entries = fetch_catalog_entries().await;
        let names: Vec<String> = entries
            .iter()
            .filter_map(|e| e.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        for must_exist in ["hybrid_retrieve", "list_witnesses", "materialize_engram"] {
            assert!(
                names.iter().any(|n| n == must_exist),
                "MCP catalog missing must-have tool '{must_exist}' — names: {names:?}",
            );
        }
    }
}
