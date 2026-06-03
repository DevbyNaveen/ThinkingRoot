// The MCP `handle_list` tool catalogue uses serde_json::json! over a large
// inline JSON literal (~30 tools). Each new tool nests another array level
// in the macro expansion, pushing past the default 128-frame recursion
// limit. Bumped to 256 when RARP added 4 tools (materialize_engram,
// probe_engram, list_engrams, expire_engram).
#![recursion_limit = "256"]

pub mod activity;
pub mod agentmemory;
pub mod backfill;
pub mod branch_cache;
pub mod consolidation;
pub mod egress;
pub mod engine;
pub mod fingerprint;
pub mod flow_cron;
pub mod flow_executors;
pub mod fs_ops;
pub mod graph;
pub mod graph_cache;
pub mod intelligence;
pub mod live_sync;
pub mod maintenance;
pub mod mcp;
pub mod memory_tree;
pub mod operator_tools;
pub mod acquisition_tools;
pub mod root_function_runtime;
pub mod scheduler;
pub mod pipeline;
pub mod rest;
pub mod structural_persist;
pub mod sys_fs_ops;
pub mod system_power;
pub mod tokenjuice;
pub mod workspace_state;
pub mod workspace_watcher;
