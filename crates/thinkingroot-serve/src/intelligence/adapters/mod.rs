// crates/thinkingroot-serve/src/intelligence/adapters/mod.rs
//
// Worktree adapters for the Substrate Bus (Week 3, plan 2026-05-09).
//
// An "adapter" is the seam between this server's substrate and an
// external coding agent (Claude Code's `claude -p`, Cursor's
// `cursor-agent -p`). Adapters spawn the agent in a sandbox worktree
// pre-loaded with a `.tr` pack scoped to the relevant substrate
// claims, parse the agent's structured stdout into a stream of
// `AdapterEvent`s, and gracefully terminate the subprocess when the
// caller drops the stream.
//
// Why a trait instead of a free function: each agent's CLI has a
// different output format (Claude Code's stream-json, Cursor's
// json-or-acp, hypothetical Antigravity stream-yaml in v1.1). The
// trait gives the bus one stable consumer surface (`spawn -> Stream`)
// while letting each adapter pick the right parser.
//
// Testability: parsers are pure functions over `&str` lines, unit-
// tested without spawning anything. Process lifecycle uses a
// configurable binary path so integration tests can substitute a
// shell script or a Rust test helper that emits canned events.

use async_trait::async_trait;
use std::path::Path;
use thiserror::Error;
use tokio::sync::mpsc;

pub mod claude_code;
pub mod cursor;

/// One event in the lifecycle of an external agent run. Mirrors the
/// shape of `intelligence::agent::AgentEvent` so the SSE relay layer
/// can map adapter events 1:1 onto the existing wire format. Variants
/// are deliberately a subset — adapters never expose `ToolCallProposed`
/// (the external agent makes its own approval decisions inside the
/// worktree; this server only sees results).
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterEvent {
    /// External agent emitted prose. May fire repeatedly; the relay
    /// concatenates for display.
    Token { content: String },
    /// External agent invoked a tool. `input` is JSON-decoded so the
    /// SSE relay can re-render it without re-parsing.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool result returned to the external agent. `is_error` mirrors
    /// the result tag so the UI can render error pills.
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    /// External agent finished cleanly. `final_text` is the answer
    /// the agent produced for the prompt; `cost_usd` is reported by
    /// agents that surface it (Claude Code does, Cursor does not).
    Done {
        final_text: String,
        cost_usd: Option<f64>,
    },
    /// External agent failed in a non-recoverable way. Distinct from
    /// `Done { final_text: "" }` because the bus should NOT credit
    /// any work to a failed run (no Grounded Diff Certificate, no
    /// belief diff).
    Error { message: String },
}

/// Errors the adapter surface emits. Distinct from `AdapterEvent::Error`
/// (which is a wire event for the consumer); these are spawn-time
/// failures the caller sees as a `Result::Err` instead of a stream
/// item.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The external CLI binary could not be found on PATH or the
    /// configured override path. Caller surfaces a user-facing
    /// "install Claude Code" hint.
    #[error("binary '{binary}' not found: {source}")]
    BinaryNotFound {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    /// The worktree path doesn't exist or isn't a directory. Pre-spawn
    /// validation; cheap.
    #[error("worktree path '{path}' is not a directory")]
    InvalidWorktree { path: String },
    /// I/O failure spawning or attaching to the subprocess. Wraps
    /// `std::io::Error` rather than `tokio::io::Error` because
    /// tokio's IO type aliases std's.
    #[error("spawn failed: {0}")]
    SpawnFailed(#[from] std::io::Error),
}

/// One external coding agent the substrate can delegate to. Stateless
/// — every spawn is independent. Implementations capture configuration
/// (binary path, MCP config path, env vars) at construction time.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Stable name for logging + Grounded Diff Certificates.
    /// Convention: snake_case CLI name (`claude_code`, `cursor`).
    fn name(&self) -> &'static str;

    /// Spawn the external agent in `worktree` and stream its events
    /// through the returned receiver. The caller drops the receiver
    /// to terminate the agent (the adapter listens for the receiver's
    /// drop in a watcher task and kills the subprocess).
    ///
    /// `pack` is an optional path to a `.tr` pack the adapter
    /// pre-mounts so the external agent's MCP queries see the scoped
    /// claim subset, not the full workspace. `None` means "let the
    /// agent see whatever the worktree's existing config exposes" —
    /// useful for tests, dangerous in production (don't use `None` on
    /// real agent invocations).
    async fn spawn(
        &self,
        prompt: &str,
        worktree: &Path,
        pack: Option<&Path>,
    ) -> Result<mpsc::Receiver<AdapterEvent>, AdapterError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_event_variants_round_trip_via_debug() {
        // Sanity check on Debug output — the adapter event log goes
        // through tracing::debug! and we want grep-able output.
        let e = AdapterEvent::Token {
            content: "hi".into(),
        };
        let s = format!("{e:?}");
        assert!(s.contains("Token"));
        assert!(s.contains("hi"));
    }

    #[test]
    fn adapter_error_renders_useful_strings() {
        let e = AdapterError::InvalidWorktree {
            path: "/no/such".into(),
        };
        assert!(format!("{e}").contains("/no/such"));
    }
}
