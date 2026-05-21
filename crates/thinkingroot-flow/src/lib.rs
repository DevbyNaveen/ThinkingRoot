//! ThinkingRoot multi-agent flow orchestrator.
//!
//! Declarative YAML/TOML workflows whose state lives on substrate
//! branches and survives daemon restarts via cognition-commit
//! checkpoints. Each flow node executes as one of five executor
//! types — `local_llm`, `mcp_tool`, `client_sampling`,
//! `deterministic`, `human` — mixable in the same DAG.
//!
//! # Architecture pointer
//!
//! See `/Users/naveen/.claude/plans/i-wnat-to-know-lazy-cat.md`
//! sections "Architecture", "The 19 commits", and "Locked-in
//! design decisions" for the production design.
//!
//! # Public surface (C7 scaffold)
//!
//! - [`definition`] — `FlowDefinition`, `NodeSpec`, `EdgeSpec` +
//!   YAML/TOML loaders.
//! - [`error`] — `FlowError` enum (every failure mode in the
//!   orchestrator).
//!
//! Subsequent commits (C8 onwards) add:
//! - `validator` (C8) — cycle detection + tool/function resolution.
//! - `storage` (C9) — per-workspace CozoDB flow_runs relation.
//! - `runtime` (C10) — executor loop + checkpoint + auto-resume.
//! - `executors/` (C11-C14, C16) — the five node types.

pub mod definition;
pub mod error;
pub mod executors;
pub mod runtime;
pub mod storage;
pub mod validator;

pub use definition::{
    BranchStrategy, EdgeSpec, FinalMergeSpec, FlowDefinition, MergeStrategy, NodeSpec,
    NodeType, OutputSpec, ParamSpec, SamplingMessage,
};
pub use error::FlowError;
