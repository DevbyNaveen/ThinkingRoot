//! Clean-room reimplementation. Inspired by openhuman/tools/traits.rs
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.6 (2026-05-17) — incremental tool-trait registry.
//!
//! ## Why incremental, not big-bang
//!
//! The MCP `handle_call` dispatcher in `tools.rs` ships with 64
//! string-keyed `match` arms. Each arm reads its own arguments and
//! calls a free function (`handle_search`, `handle_compile`, …).
//! Refactoring all 64 into trait impls would touch ~4,000 LOC across
//! one PR — high regression risk for zero behavioural change.
//!
//! Instead: existing 64 STAY on the match. New tools added from
//! Phase E onwards (`export_memory_tree`, `import_memory_tree` from
//! E.3; external-MCP-bridged tools from E.5; any future ship's
//! tools) register a `McpToolHandler` impl into a global registry.
//! `handle_list` appends the registry's schemas to its catalogue;
//! `handle_call` adds ONE fall-through arm at the bottom of the
//! match block — if no built-in name matched, look up by name in
//! the registry.
//!
//! ## Write-class propagation
//!
//! `McpToolHandler::is_write()` is the authoritative source for
//! whether a trait-registered tool needs PermissionsGate (Phase D)
//! routing. `bridge_is_write_name` in `mcp/tools.rs` consults both
//! the hardcoded `BRIDGE_WRITE_NAMES` const AND the trait registry
//! so new tools auto-gate at registration time — no manual
//! addition to the const string list.
//!
//! ## Determinism + safety
//!
//! Registration is one-shot at process setup; the registry is read
//! at every `handle_list` / `handle_call` call but typically not
//! written outside startup. We use an `RwLock` (cheap reads,
//! contended writes are non-issue at low write frequency) wrapped
//! in `OnceLock` for the global singleton.

use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use serde_json::Value;

use crate::engine::QueryEngine;
use crate::intelligence::session::SessionStore;
use thinkingroot_core::Error;

/// Context passed to every `McpToolHandler::handle` invocation.
///
/// Mirrors the implicit context the existing `handle_*` free
/// functions in `tools.rs` receive via positional parameters
/// (`engine`, `ws`, `session_id`, …). Bundling them here makes
/// future signature evolution a one-place change.
pub struct McpToolContext<'a> {
    pub engine: &'a QueryEngine,
    pub workspace: &'a str,
    pub session_id: &'a str,
    pub sessions: &'a SessionStore,
    pub engram_manager: &'a Arc<crate::intelligence::engram::EngramManager>,
}

/// Typed error returned from a `McpToolHandler::handle`. Wraps the
/// engine's `Error` for uniform conversion at the dispatcher layer.
#[derive(Debug)]
pub enum McpToolError {
    /// User supplied bad arguments (missing field, wrong type).
    /// Surfaces to the LLM as JSON-RPC -32602 (`Invalid params`).
    InvalidArgs(String),
    /// Backend / pipeline error. JSON-RPC -32603 (`Internal error`).
    Backend(Error),
    /// Tool refused to run (e.g. permissions, missing capability).
    /// JSON-RPC -32603 but the message carries the user-facing
    /// reason verbatim so the model can adapt.
    Refused(String),
}

impl From<Error> for McpToolError {
    fn from(e: Error) -> Self {
        McpToolError::Backend(e)
    }
}

impl std::fmt::Display for McpToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpToolError::InvalidArgs(m) => write!(f, "invalid arguments: {m}"),
            McpToolError::Backend(e) => write!(f, "backend error: {e}"),
            McpToolError::Refused(r) => write!(f, "refused: {r}"),
        }
    }
}

/// The trait every new tool implements.
#[async_trait]
pub trait McpToolHandler: Send + Sync + 'static {
    /// Stable, snake_case tool name. MUST be unique across the union
    /// of `BRIDGE_WRITE_NAMES` and the registry. Names colliding
    /// with a built-in tool will never dispatch through this trait
    /// — the built-in arm runs first.
    fn name(&self) -> &'static str;

    /// One-line description for the MCP `tools/list` catalogue. The
    /// model uses this to decide when to call the tool — write it
    /// like a docstring, not a marketing blurb.
    fn description(&self) -> &'static str;

    /// JSON Schema describing the tool's `input` shape. Standard
    /// MCP `inputSchema` field. The dispatcher does no validation
    /// before calling `handle`; the implementation is responsible
    /// for reading + rejecting bad input via `InvalidArgs`.
    fn input_schema(&self) -> Value;

    /// `true` if this tool changes workspace state (creates rows,
    /// mutates files, sends external requests). PermissionsGate +
    /// `BRIDGE_WRITE_NAMES`-equivalent gating route through this
    /// flag.
    fn is_write(&self) -> bool {
        false
    }

    /// Actually run. The dispatcher serializes the returned value
    /// via `serde_json::to_string_pretty` into the MCP `text`
    /// content block. To return structured data, encode it as JSON
    /// here and the model will see the JSON text.
    async fn handle(
        &self,
        args: Value,
        ctx: &McpToolContext<'_>,
    ) -> Result<Value, McpToolError>;
}

/// Process-wide registry. `OnceLock` lazily initialises the
/// `RwLock<Vec<...>>` on first use; subsequent reads are lock-free
/// modulo the read-side `RwLock` (which is very cheap when not
/// contended).
fn registry() -> &'static RwLock<Vec<Arc<dyn McpToolHandler>>> {
    static REG: OnceLock<RwLock<Vec<Arc<dyn McpToolHandler>>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a tool. Typical call site: workspace mount / process
/// startup. Re-registering a tool with the same name overwrites the
/// previous entry — newer wins. This is intentional: tests can
/// re-register a stub after a fresh `register_*` call without
/// needing a clear API.
pub fn register_tool(handler: Arc<dyn McpToolHandler>) {
    let mut guard = registry().write().expect("registry poisoned");
    if let Some(slot) = guard.iter_mut().find(|h| h.name() == handler.name()) {
        *slot = handler;
    } else {
        guard.push(handler);
    }
}

/// Test helper — drop everything. NOT exposed in production; tests
/// that want isolated registries call this in their setup. Behind
/// `#[cfg(test)]` so production binaries can't accidentally call it.
#[cfg(test)]
pub fn clear_registry() {
    registry().write().expect("registry poisoned").clear();
}

/// True if a tool with this exact name is registered. Used by
/// `bridge_is_write_name` to extend `BRIDGE_WRITE_NAMES` without
/// changing the const.
pub fn is_registered(name: &str) -> bool {
    registry()
        .read()
        .expect("registry poisoned")
        .iter()
        .any(|h| h.name() == name)
}

/// True if a registered tool with this name claims to be a write.
/// Returns `false` if not registered — callers that need both
/// "known" and "is write" should compose with [`is_registered`].
pub fn is_registered_write(name: &str) -> bool {
    registry()
        .read()
        .expect("registry poisoned")
        .iter()
        .find(|h| h.name() == name)
        .map(|h| h.is_write())
        .unwrap_or(false)
}

/// Look up a registered tool by name + clone its `Arc`. Returns
/// `None` when no tool is registered under that name.
pub fn lookup(name: &str) -> Option<Arc<dyn McpToolHandler>> {
    registry()
        .read()
        .expect("registry poisoned")
        .iter()
        .find(|h| h.name() == name)
        .cloned()
}

/// Snapshot of every registered tool's `tools/list` shape. The
/// caller (typically `tools.rs::handle_list`) appends this to the
/// hardcoded built-in catalogue.
pub fn list_schemas() -> Vec<Value> {
    let guard = registry().read().expect("registry poisoned");
    guard
        .iter()
        .map(|h| {
            serde_json::json!({
                "name": h.name(),
                "description": h.description(),
                "inputSchema": h.input_schema(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl McpToolHandler for EchoTool {
        fn name(&self) -> &'static str {
            "test_echo"
        }
        fn description(&self) -> &'static str {
            "test-only echo tool"
        }
        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            })
        }
        fn is_write(&self) -> bool {
            false
        }
        async fn handle(
            &self,
            args: Value,
            _ctx: &McpToolContext<'_>,
        ) -> Result<Value, McpToolError> {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpToolError::InvalidArgs("missing 'text'".into()))?;
            Ok(json!({ "echoed": text }))
        }
    }

    struct WriteyTool;

    #[async_trait]
    impl McpToolHandler for WriteyTool {
        fn name(&self) -> &'static str {
            "test_writey"
        }
        fn description(&self) -> &'static str {
            "test-only write tool"
        }
        fn input_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        fn is_write(&self) -> bool {
            true
        }
        async fn handle(
            &self,
            _args: Value,
            _ctx: &McpToolContext<'_>,
        ) -> Result<Value, McpToolError> {
            Ok(json!("ok"))
        }
    }

    // These tests share the process-global registry, so they must
    // run serially within this module. cargo defaults to parallel
    // test execution, so we gate every test on a shared `Mutex<()>`
    // — adding a `serial_test` dep would be heavier weight for the
    // same observable behaviour.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        // `lock` returns `Result` because a prior panic could poison
        // the mutex. We recover the guard either way so subsequent
        // tests don't all fail with "poisoned"; the panic is what
        // actually matters in test output.
        match LOCK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[test]
    fn register_and_lookup_round_trips() {
        let _g = test_lock();
        clear_registry();
        register_tool(Arc::new(EchoTool));
        assert!(is_registered("test_echo"));
        assert!(!is_registered("not_a_tool"));
        let h = lookup("test_echo").expect("must find");
        assert_eq!(h.name(), "test_echo");
        assert!(!h.is_write());
    }

    #[test]
    fn re_register_overwrites_same_name() {
        let _g = test_lock();
        clear_registry();
        register_tool(Arc::new(EchoTool));
        register_tool(Arc::new(EchoTool)); // 2nd call
        let count = registry().read().unwrap().len();
        assert_eq!(count, 1, "duplicate names must collapse to 1 slot");
    }

    #[test]
    fn list_schemas_returns_each_registered_tool() {
        let _g = test_lock();
        clear_registry();
        register_tool(Arc::new(EchoTool));
        register_tool(Arc::new(WriteyTool));
        let schemas = list_schemas();
        assert_eq!(schemas.len(), 2);
        let names: Vec<&str> = schemas
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"test_echo"));
        assert!(names.contains(&"test_writey"));
    }

    #[test]
    fn is_registered_write_distinguishes_read_from_write() {
        let _g = test_lock();
        clear_registry();
        register_tool(Arc::new(EchoTool));
        register_tool(Arc::new(WriteyTool));
        assert!(!is_registered_write("test_echo"));
        assert!(is_registered_write("test_writey"));
        assert!(!is_registered_write("unknown"));
    }
}
