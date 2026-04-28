// crates/thinkingroot-serve/src/intelligence/tools.rs
//
// Tool registry — async dispatch from `ToolCall.name` to a handler.
//
// The registry is the data structure the agent loop (`agent.rs`) walks
// when the LLM emits a `ToolUseResponse::ToolCalls`. It owns:
//
//   * The catalogue of [`Tool`] specs (passed verbatim to
//     `LlmClient::chat_with_tools`).
//   * An async [`ToolHandler`] for each tool name.
//   * A `is_write` flag per tool — surfaced to the agent so write
//     tools route through the configured [`ApprovalGate`] before
//     dispatch, while read tools execute freely.
//
// The registry is content-addressed by tool name: registering two
// tools with the same name is a programming error and panics at
// build time. The agent expects every tool the LLM might call to be
// known (the LLM was told about them via `chat_with_tools`); a missing
// handler is surfaced as a `is_error: true` ToolResult so the model
// can recover.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use thinkingroot_extract::llm::Tool;

/// What a tool returns to the agent loop. The `content` is what the
/// LLM sees as the next-turn `ChatMessage::ToolResults` payload, so it
/// should be plain text or stringified JSON — already shaped for the
/// model's eyes.
///
/// `is_error: true` flags a runtime failure (the tool ran but
/// something went wrong, e.g. branch not found, permission denied).
/// The agent feeds the error back to the LLM with the same flag set
/// so the model can decide whether to retry, ask the user, or give up.
#[derive(Debug, Clone)]
pub struct ToolHandlerResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolHandlerResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            is_error: true,
        }
    }
}

/// Async handler invoked by the agent loop when the LLM calls a tool.
/// Implementors do their I/O against the engine, branch, MCP tools,
/// HTTP, etc. and produce a [`ToolHandlerResult`]. Errors that prevent
/// the tool from running at all should still be surfaced as an
/// `is_error: true` result rather than a panic — the agent treats
/// every result as input the model can reason about.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult;
}

/// One tool entry. Owns its spec (so the registry is the single source
/// of truth for what the LLM is told about) and its handler.
struct RegisteredTool {
    spec: Tool,
    handler: Arc<dyn ToolHandler>,
    is_write: bool,
}

/// Tool registry. Cheaply cloneable via internal `Arc`s — pass it to
/// the agent loop, the synthesizer, and any other layer that needs
/// either the spec list or the dispatch table.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    inner: Arc<ToolRegistryInner>,
}

#[derive(Default)]
struct ToolRegistryInner {
    tools: HashMap<String, RegisteredTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a read-only tool. Always dispatched without an
    /// approval check.
    pub fn register_read(self, spec: Tool, handler: Arc<dyn ToolHandler>) -> Self {
        self.register(spec, handler, false)
    }

    /// Register a write tool. Routed through the agent's approval gate
    /// before dispatch.
    pub fn register_write(self, spec: Tool, handler: Arc<dyn ToolHandler>) -> Self {
        self.register(spec, handler, true)
    }

    fn register(self, spec: Tool, handler: Arc<dyn ToolHandler>, is_write: bool) -> Self {
        // The consuming builder pattern means each .register call has
        // exclusive ownership of `self` and therefore the inner Arc has
        // refcount 1 — except if a caller cloned the in-progress
        // registry. That's a programmer bug (the registry is meant to
        // be built up before any sharing), so we surface it loudly
        // rather than silently fork.
        let mut inner = Arc::try_unwrap(self.inner).unwrap_or_else(|_| {
            panic!(
                "ToolRegistry::register called on a registry that has been cloned mid-build; \
                 finish building before sharing"
            )
        });
        let name = spec.name.clone();
        if inner.tools.contains_key(&name) {
            panic!(
                "ToolRegistry: duplicate tool name '{name}' — \
                 each tool must have a unique name"
            );
        }
        inner.tools.insert(
            name,
            RegisteredTool {
                spec,
                handler,
                is_write,
            },
        );
        ToolRegistry {
            inner: Arc::new(inner),
        }
    }

    /// All registered tool specs, in registration order is not
    /// guaranteed (HashMap order). Pass to
    /// `LlmClient::chat_with_tools` as the `tools` slice.
    pub fn specs(&self) -> Vec<Tool> {
        self.inner.tools.values().map(|t| t.spec.clone()).collect()
    }

    /// Whether the tool exists in the registry. The agent uses this to
    /// distinguish "model called an unknown tool" from "model called a
    /// tool whose handler errored".
    pub fn contains(&self, name: &str) -> bool {
        self.inner.tools.contains_key(name)
    }

    /// True iff the registered tool is marked as a write. Unknown
    /// tools answer `false` — the agent will surface a "no such tool"
    /// error rather than gate it as a write.
    pub fn is_write(&self, name: &str) -> bool {
        self.inner
            .tools
            .get(name)
            .map(|t| t.is_write)
            .unwrap_or(false)
    }

    /// Number of registered tools. Useful for tests + observability.
    pub fn len(&self) -> usize {
        self.inner.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.tools.is_empty()
    }

    /// Dispatch a tool call. Returns a "no such tool" error result
    /// when the name isn't registered, so the agent surfaces it back
    /// to the LLM rather than crashing.
    pub async fn dispatch(&self, name: &str, input: serde_json::Value) -> ToolHandlerResult {
        match self.inner.tools.get(name) {
            Some(t) => t.handler.handle(input).await,
            None => ToolHandlerResult::error(format!(
                "no such tool: '{name}' is not registered. Available tools: {}",
                self.tool_names().join(", ")
            )),
        }
    }

    /// Sorted list of tool names — used by the "no such tool" error
    /// message above and by tests / observability.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.tools.keys().cloned().collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoHandler {
        prefix: &'static str,
    }

    #[async_trait]
    impl ToolHandler for EchoHandler {
        async fn handle(&self, input: serde_json::Value) -> ToolHandlerResult {
            ToolHandlerResult::ok(format!("{}:{input}", self.prefix))
        }
    }

    fn fixture_tool(name: &str) -> Tool {
        Tool::new(
            name,
            format!("test tool {name}"),
            json!({"type": "object", "properties": {}}),
        )
    }

    #[tokio::test]
    async fn registry_starts_empty() {
        let r = ToolRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.tool_names().is_empty());
    }

    #[tokio::test]
    async fn register_read_and_dispatch() {
        let r = ToolRegistry::new().register_read(
            fixture_tool("search"),
            Arc::new(EchoHandler { prefix: "read" }),
        );
        assert_eq!(r.len(), 1);
        assert!(r.contains("search"));
        assert!(!r.is_write("search"));
        let result = r.dispatch("search", json!({"q": "x"})).await;
        assert!(!result.is_error);
        assert!(result.content.starts_with("read:"));
        assert!(result.content.contains(r#""q":"x""#));
    }

    #[tokio::test]
    async fn register_write_marks_is_write() {
        let r = ToolRegistry::new().register_write(
            fixture_tool("create_branch"),
            Arc::new(EchoHandler { prefix: "write" }),
        );
        assert!(r.is_write("create_branch"));
        let result = r.dispatch("create_branch", json!({"name": "feat"})).await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error_result() {
        let r = ToolRegistry::new().register_read(
            fixture_tool("search"),
            Arc::new(EchoHandler { prefix: "p" }),
        );
        let result = r.dispatch("nope", json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("no such tool"));
        assert!(result.content.contains("search")); // includes available list
    }

    #[tokio::test]
    async fn specs_lists_every_registered_tool() {
        let r = ToolRegistry::new()
            .register_read(fixture_tool("a"), Arc::new(EchoHandler { prefix: "" }))
            .register_write(fixture_tool("b"), Arc::new(EchoHandler { prefix: "" }))
            .register_read(fixture_tool("c"), Arc::new(EchoHandler { prefix: "" }));
        let specs = r.specs();
        assert_eq!(specs.len(), 3);
        let mut names: Vec<&str> = specs.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    #[should_panic(expected = "duplicate tool name")]
    async fn duplicate_registration_panics() {
        let _ = ToolRegistry::new()
            .register_read(fixture_tool("dup"), Arc::new(EchoHandler { prefix: "" }))
            .register_read(fixture_tool("dup"), Arc::new(EchoHandler { prefix: "" }));
    }

    #[tokio::test]
    async fn is_write_false_for_unknown_tool() {
        let r = ToolRegistry::new();
        assert!(!r.is_write("anything"));
    }

    #[tokio::test]
    async fn tool_handler_result_helpers() {
        let ok = ToolHandlerResult::ok("done");
        assert!(!ok.is_error);
        assert_eq!(ok.content, "done");
        let err = ToolHandlerResult::error("nope");
        assert!(err.is_error);
        assert_eq!(err.content, "nope");
    }

    #[tokio::test]
    async fn registry_is_cloneable_and_shared() {
        let r1 = ToolRegistry::new()
            .register_read(fixture_tool("a"), Arc::new(EchoHandler { prefix: "" }));
        let r2 = r1.clone();
        // Both clones see the same tool.
        assert!(r2.contains("a"));
        let res = r2.dispatch("a", json!({})).await;
        assert!(!res.is_error);
        // Original is independent enough that we can keep using it.
        assert!(r1.contains("a"));
    }
}
