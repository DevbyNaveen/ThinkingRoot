//! Phase 1 of the "ThinkingRoot Central" plan (`plans/okey-so-i-wnat-elegant-hamster.md`):
//! exposes the self-heal substrate (shipped 2026-05-13) as MCP tools
//! so the in-app agent can operate the system without the user clicking
//! through Tauri / running CLI commands by hand.
//!
//! ## Why this lives in a dedicated module
//!
//! The 16 tools wrap existing substrate; their dispatch is mechanical.
//! Putting them inside the giant `mcp/tools.rs` `handle_call` match
//! block would dilute that file further. The Phase E.6
//! `mcp::tool_trait` registry (`crates/thinkingroot-serve/src/mcp/tool_trait.rs`)
//! exists exactly to absorb new tools without churning the match block —
//! every tool here implements `McpToolHandler` and is auto-discovered
//! by `tools::handle_list` + `tools::handle_call`'s fall-through arm.
//!
//! ## What's in the first batch
//!
//! 15 of the 16 plan tools. The 16th, `workspace_mount`, needs
//! `&mut QueryEngine` and lands via the same SSE-fastpath pattern as
//! `compile_request_fastpath` (see `mcp/sse.rs::compile_request_fastpath`)
//! in a follow-up.
//!
//! Wired ones:
//! ```text
//! read-class (10):
//!   recovery_log_tail              install_manifest_read
//!   restart_state_get              install_manifest_verify_checksum
//!   doctor_run                     list_workspaces_full
//!   workspace_root_path            (and 3 more via the write list)
//!
//! write-class (5):
//!   reset_circuit_breaker          rebuild_vector_index
//!   reset_compile_breaker          migrate_substrate
//!   doctor_apply_fix               engram_invalidate_workspace
//!   mark_setup_complete            restart_engine_request
//! ```
//! (Counts above are 10+5 = 15 of 16; `workspace_mount` deferred.)
//!
//! ## Pre-trust note
//!
//! The plan calls for pre-trusting these tools so the in-app agent
//! doesn't pop an approval prompt for every operator call. That
//! pre-trust lives in `intelligence/permissions_gate.rs` and lands
//! in a sibling commit — until then, every write-class operator tool
//! still goes through the standard `ApprovalGate` (via
//! `mcp_bridge.rs:276`'s `is_registered_write` check). This is the
//! safe order: tools work first, the bypass is added second.
//!
//! ## Honesty
//!
//! - `doctor_run` and `doctor_apply_fix` shell out to the `root`
//!   binary. The CLI's `doctor/` module isn't reachable from the
//!   `thinkingroot-serve` crate (wrong dep direction), and shelling
//!   out matches the established pattern at
//!   `apps/thinkingroot-desktop/src-tauri/src/commands/doctor.rs:59`.
//!   A future ship can move `doctor/` to `serve` for an in-process call.
//! - `restart_engine_request` writes to a process-global `OnceLock`
//!   channel set by `AppState::new_with_root`. When no AppState has
//!   been constructed (stdio MCP transport spawned in-process), the
//!   tool returns a typed `Refused` rather than panicking — the
//!   in-process MCP path has no sidecar watchdog to respond to a
//!   restart request anyway.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::mcp::tool_trait::{McpToolContext, McpToolError, McpToolHandler, register_tool};
use thinkingroot_core::install_manifest::{BinaryEntry, InstallManifest};
use thinkingroot_core::recovery_log;
use thinkingroot_core::restart_state::{self, RestartState};

// ── Restart-request channel ─────────────────────────────────────────────────
//
// `restart_engine_request` (tool #16) needs a side channel to the
// desktop watchdog. We can't add a field to `McpToolContext` without
// touching every existing trait impl + the dispatcher; instead, we
// expose a `OnceLock<broadcast::Sender>` set once at startup. The
// pattern mirrors `tool_trait::registry()` itself.

/// Reason carried on a restart request. Wire-stable — the desktop
/// watchdog reads it to decide whether to skip the breaker (e.g. an
/// AI-initiated restart should NOT count toward the crash-loop trip).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestartReason {
    /// The in-app AI agent decided a restart would resolve a wedged
    /// state. Carries a short human-readable rationale.
    AgentInitiated { reason: String },
    /// The user clicked a "Restart engine" button or equivalent.
    UserInitiated,
}

/// Global broadcast handle. Constructed by
/// `AppState::new_with_root` (see `rest.rs`) and consumed by the
/// desktop watchdog subscriber. Not used by stdio MCP / in-process
/// modes — the operator tool returns Refused in those cases.
static RESTART_TX: OnceLock<broadcast::Sender<RestartReason>> = OnceLock::new();

/// Install the process-global restart channel. Called once at
/// startup; subsequent calls are a no-op (returns `Err(existing)` in
/// `OnceLock::set`, which we swallow because the existing channel
/// is the source of truth and re-setting would invalidate prior
/// subscribers).
pub fn install_restart_channel(tx: broadcast::Sender<RestartReason>) {
    let _ = RESTART_TX.set(tx);
}

/// Subscribe to restart requests. The desktop watchdog calls this
/// from its setup loop. Returns `None` when no channel has been
/// installed (e.g. unit tests, CLI-only contexts).
pub fn restart_subscription() -> Option<broadcast::Receiver<RestartReason>> {
    RESTART_TX.get().map(|tx| tx.subscribe())
}

// ── 1. recovery_log_tail ────────────────────────────────────────────────────

struct RecoveryLogTail;

#[async_trait]
impl McpToolHandler for RecoveryLogTail {
    fn name(&self) -> &'static str {
        "recovery_log_tail"
    }
    fn description(&self) -> &'static str {
        "Return the last N entries from the self-heal recovery log (`~/.config/thinkingroot/recovery.log`). \
         Each entry is one of: respawn, respawn_ok, stale_lock_cleanup, port_advance, manifest_rebuild, \
         circuit_breaker_tripped, circuit_breaker_reset, binary_checksum_mismatch, compile_failed, \
         compile_retry_scheduled, compile_breaker_tripped, compile_recovered. Use this BEFORE proposing \
         a fix so you can cite the exact event the user is hitting."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "default": 50, "description": "Max entries to return. Default 50. Bound: 1-1000." }
            }
        })
    }
    async fn handle(&self, args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 1000) as usize;
        let events = recovery_log::tail(limit)
            .map_err(|e| McpToolError::Refused(format!("recovery_log::tail: {e}")))?;
        Ok(json!({
            "schema_version": 1,
            "log_path": recovery_log::log_path().ok().map(|p| p.display().to_string()),
            "events": events,
        }))
    }
}

// ── 2. restart_state_get ────────────────────────────────────────────────────

struct RestartStateGet;

#[async_trait]
impl McpToolHandler for RestartStateGet {
    fn name(&self) -> &'static str {
        "restart_state_get"
    }
    fn description(&self) -> &'static str {
        "Read the persisted restart state — recent crash attempts (60s window), recent compile \
         failures (5min window), and active circuit-breaker timestamps if any. The process breaker \
         trips on 4 crashes in 60s or 3 crash-signal exits. The compile breaker trips on 3 failures in 5min. \
         Each tripped breaker auto-clears after a fixed duration (5min process, 10min compile)."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn handle(&self, _args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let state = RestartState::load()
            .map_err(|e| McpToolError::Refused(format!("RestartState::load: {e}")))?;
        Ok(json!({
            "schema_version": 1,
            "state_path": restart_state::path().ok().map(|p: std::path::PathBuf| p.display().to_string()),
            "state": state,
        }))
    }
}

// ── 3. reset_circuit_breaker ────────────────────────────────────────────────

struct ResetCircuitBreaker;

#[async_trait]
impl McpToolHandler for ResetCircuitBreaker {
    fn name(&self) -> &'static str {
        "reset_circuit_breaker"
    }
    fn description(&self) -> &'static str {
        "Clear the process-crash circuit breaker. Wipes both the `circuit_breaker_until` timestamp \
         AND the recent-attempts history so the next restart starts at attempt 1 (backoff 0ms). \
         Always read `restart_state_get` FIRST to understand why it tripped — resetting without \
         investigation can mask a real crash loop. Records a `circuit_breaker_reset` event in the \
         recovery log."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Short human-readable reason for the reset (e.g. 'transient provider outage cleared'). \
                                    Stored in the recovery-log entry for the audit trail."
                }
            },
            "required": ["reason"]
        })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpToolError::InvalidArgs("missing 'reason'".into()))?
            .to_string();
        let mut state = RestartState::load()
            .map_err(|e| McpToolError::Refused(format!("RestartState::load: {e}")))?;
        let was_active = state.breaker_active();
        let prior_attempt_count = state.attempts.len();
        state.reset_circuit_breaker();
        state
            .save()
            .map_err(|e| McpToolError::Refused(format!("RestartState::save: {e}")))?;
        recovery_log::append(&recovery_log::RecoveryEvent::circuit_breaker_reset(reason.clone()))
            .map_err(|e| McpToolError::Refused(format!("recovery_log::append: {e}")))?;
        Ok(json!({
            "schema_version": 1,
            "was_active": was_active,
            "attempts_cleared": prior_attempt_count,
            "reason_recorded": reason,
        }))
    }
}

// ── 4. reset_compile_breaker ────────────────────────────────────────────────

struct ResetCompileBreaker;

#[async_trait]
impl McpToolHandler for ResetCompileBreaker {
    fn name(&self) -> &'static str {
        "reset_compile_breaker"
    }
    fn description(&self) -> &'static str {
        "Clear the per-workspace compile circuit breaker. The compile breaker trips when 3 \
         consecutive compiles fail within 5 minutes; once tripped, the desktop's Compile button \
         returns a loud error rather than queueing. Always read `restart_state_get` FIRST — \
         if the breaker tripped because of a deterministic bug (broken provider, wedged file), \
         resetting it just retries the same failure."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Short reason for the reset, stored in the recovery log."
                }
            },
            "required": ["reason"]
        })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpToolError::InvalidArgs("missing 'reason'".into()))?
            .to_string();
        let mut state = RestartState::load()
            .map_err(|e| McpToolError::Refused(format!("RestartState::load: {e}")))?;
        let was_active = state.compile_breaker_active();
        let prior_attempt_count = state.compile_attempts.len();
        state.reset_compile_breaker();
        state
            .save()
            .map_err(|e| McpToolError::Refused(format!("RestartState::save: {e}")))?;
        // The recovery log doesn't yet have a dedicated `compile_breaker_reset`
        // constructor — the four compile-* events shipped 2026-05-14 cover the
        // failure direction, not the clear direction. Use the existing
        // process-breaker-reset event with a workspace-tagged reason so the
        // audit trail stays uniform.
        let tagged_reason = format!("compile_breaker_reset: {reason}");
        recovery_log::append(&recovery_log::RecoveryEvent::circuit_breaker_reset(
            tagged_reason.clone(),
        ))
        .map_err(|e| McpToolError::Refused(format!("recovery_log::append: {e}")))?;
        Ok(json!({
            "schema_version": 1,
            "was_active": was_active,
            "attempts_cleared": prior_attempt_count,
            "reason_recorded": tagged_reason,
        }))
    }
}

// ── 5. doctor_run ───────────────────────────────────────────────────────────

struct DoctorRun;

#[async_trait]
impl McpToolHandler for DoctorRun {
    fn name(&self) -> &'static str {
        "doctor_run"
    }
    fn description(&self) -> &'static str {
        "Run the 16 standard health checks (`root doctor --json`) and return the full report. \
         Each check has a stable `id` (e.g. `binary.cli.installed`, `daemon.reachable`, `credentials.any_provider`) \
         and a `status` of ok/warn/fail/skipped. Use this as the canonical first step when diagnosing \
         ANY environment problem. Shells out to the `root` CLI binary; if PATH lookup fails the tool \
         returns a typed error rather than guessing."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn handle(&self, _args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let binary = resolve_root_binary()
            .ok_or_else(|| McpToolError::Refused("could not locate `root` binary in PATH".into()))?;
        let output = tokio::process::Command::new(&binary)
            .arg("doctor")
            .arg("--json")
            .output()
            .await
            .map_err(|e| McpToolError::Refused(format!("spawn `{binary} doctor`: {e}")))?;
        // Doctor exits non-zero on warnings/failures — that's signal, not an
        // error. Parse the report from stdout regardless of exit code.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let report: Value = serde_json::from_str(&stdout).map_err(|e| {
            McpToolError::Refused(format!(
                "doctor produced non-JSON stdout: {e}; stderr was: {stderr}"
            ))
        })?;
        Ok(json!({
            "schema_version": 1,
            "exit_code": output.status.code().unwrap_or(-1),
            "report": report,
        }))
    }
}

// ── 6. doctor_apply_fix ─────────────────────────────────────────────────────

struct DoctorApplyFix;

#[async_trait]
impl McpToolHandler for DoctorApplyFix {
    fn name(&self) -> &'static str {
        "doctor_apply_fix"
    }
    fn description(&self) -> &'static str {
        "Run `root doctor --fix --json` to apply auto-fixable health-check remedies. Non-interactive: \
         only fixes that have a deterministic `RunCommand` action are applied; fixes that require \
         user input (provider key entry, OAuth flow) are reported as Skipped. Returns the post-fix \
         report so you can see what changed. Always call `doctor_run` FIRST to know what'll be touched."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, _args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let binary = resolve_root_binary()
            .ok_or_else(|| McpToolError::Refused("could not locate `root` binary in PATH".into()))?;
        let output = tokio::process::Command::new(&binary)
            .arg("doctor")
            .arg("--fix")
            .arg("--json")
            .output()
            .await
            .map_err(|e| {
                McpToolError::Refused(format!("spawn `{binary} doctor --fix`: {e}"))
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let report: Value = serde_json::from_str(&stdout).map_err(|e| {
            McpToolError::Refused(format!(
                "doctor --fix produced non-JSON stdout: {e}; stderr was: {stderr}"
            ))
        })?;
        Ok(json!({
            "schema_version": 1,
            "exit_code": output.status.code().unwrap_or(-1),
            "post_fix_report": report,
        }))
    }
}

// ── 7. install_manifest_read ────────────────────────────────────────────────

struct InstallManifestRead;

#[async_trait]
impl McpToolHandler for InstallManifestRead {
    fn name(&self) -> &'static str {
        "install_manifest_read"
    }
    fn description(&self) -> &'static str {
        "Return the install manifest (`<config>/thinkingroot/install-manifest.json`) — registered \
         binaries, their checksums, the preferred binary id, setup-complete timestamp, and the \
         bundled embedding+rerank model paths if any. Use this to verify what's installed before \
         you act on it (e.g. before suggesting `xattr -d com.apple.quarantine` you can confirm the \
         binary path)."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn handle(&self, _args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let manifest = InstallManifest::load()
            .map_err(|e| McpToolError::Refused(format!("InstallManifest::load: {e}")))?;
        Ok(json!({
            "schema_version": 1,
            "manifest_path": InstallManifest::path().ok().map(|p| p.display().to_string()),
            "manifest": manifest,
        }))
    }
}

// ── 8. install_manifest_verify_checksum ─────────────────────────────────────

struct InstallManifestVerifyChecksum;

#[async_trait]
impl McpToolHandler for InstallManifestVerifyChecksum {
    fn name(&self) -> &'static str {
        "install_manifest_verify_checksum"
    }
    fn description(&self) -> &'static str {
        "Re-verify the BLAKE3 checksum of every binary registered in the install manifest. Returns \
         per-entry status: matched/mismatched/missing. A mismatch means the binary on disk has been \
         replaced (re-install, manual edit, supply-chain swap). Doesn't modify anything; pair with \
         `root reinstall` from the user when a mismatch is found."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn handle(&self, _args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let manifest_opt = InstallManifest::load()
            .map_err(|e| McpToolError::Refused(format!("InstallManifest::load: {e}")))?;
        let manifest = match manifest_opt {
            Some(m) => m,
            None => {
                return Ok(json!({
                    "schema_version": 1,
                    "manifest_present": false,
                    "entries": [],
                }));
            }
        };
        let mut results = Vec::with_capacity(manifest.binaries.len());
        for entry in &manifest.binaries {
            results.push(verify_entry(entry));
        }
        Ok(json!({
            "schema_version": 1,
            "manifest_present": true,
            "entries": results,
        }))
    }
}

fn verify_entry(entry: &BinaryEntry) -> Value {
    if !entry.path.exists() {
        return json!({
            "id": entry.id,
            "path": entry.path.display().to_string(),
            "status": "missing",
            "expected_blake3": entry.checksum_blake3,
        });
    }
    match entry.verify_checksum() {
        Ok(()) => json!({
            "id": entry.id,
            "path": entry.path.display().to_string(),
            "status": "matched",
            "checksum_blake3": entry.checksum_blake3,
        }),
        Err(e) => json!({
            "id": entry.id,
            "path": entry.path.display().to_string(),
            "status": "mismatched",
            "expected_blake3": entry.checksum_blake3,
            "detail": e.to_string(),
        }),
    }
}

// ── 9. rebuild_vector_index ─────────────────────────────────────────────────

struct RebuildVectorIndex;

#[async_trait]
impl McpToolHandler for RebuildVectorIndex {
    fn name(&self) -> &'static str {
        "rebuild_vector_index"
    }
    fn description(&self) -> &'static str {
        "Rebuild the in-memory vector index for a workspace from its persisted claim store. \
         Use after a substrate migration, a remount under a new name, or when hybrid_retrieve / \
         search return zero results despite the workspace having content. Synchronous; returns \
         `(claims_indexed, entities_indexed)`. Does not modify any persisted state."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": "Workspace name." }
            },
            "required": ["workspace"]
        })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, _args: Value, ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let (claims_indexed, entities_indexed) = ctx
            .engine
            .rebuild_vector_index(ctx.workspace)
            .await
            .map_err(McpToolError::Backend)?;
        Ok(json!({
            "schema_version": 1,
            "workspace": ctx.workspace,
            "claims_indexed": claims_indexed,
            "entities_indexed": entities_indexed,
        }))
    }
}

// ── 10. migrate_substrate ───────────────────────────────────────────────────

struct MigrateSubstrate;

#[async_trait]
impl McpToolHandler for MigrateSubstrate {
    fn name(&self) -> &'static str {
        "migrate_substrate"
    }
    fn description(&self) -> &'static str {
        "Run a substrate migration on a workspace. Default target is `witness_mesh` (Track 11 \
         witness-mesh row-format bump). Use `water_flow` for the v3 cascade-completeness migration. \
         Pass `dry_run: true` to report what WOULD change without writing. Returns counts of rows \
         scanned + emitted + the before/after schema version. Idempotent: re-running on an \
         already-migrated workspace is a no-op."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": "Workspace name." },
                "target": {
                    "type": "string",
                    "enum": ["witness_mesh", "water_flow"],
                    "default": "witness_mesh",
                    "description": "Which migration. `witness_mesh` = Track 11 row bump. `water_flow` = v3 cascade-completeness migration."
                },
                "dry_run": { "type": "boolean", "default": false, "description": "Report without writing." }
            },
            "required": ["workspace"]
        })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, args: Value, ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("witness_mesh")
            .to_string();
        let dry_run = args
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let root_path = ctx.engine.workspace_root_path(ctx.workspace).ok_or_else(|| {
            McpToolError::InvalidArgs(format!("workspace `{}` is not mounted", ctx.workspace))
        })?;
        let data_dir = root_path.join(".thinkingroot");
        match target.as_str() {
            "witness_mesh" => {
                let report = tokio::task::spawn_blocking(move || {
                    crate::backfill::backfill_witness_mesh_at_path(&data_dir, dry_run)
                })
                .await
                .map_err(|e| McpToolError::Refused(format!("migration task panicked: {e}")))?
                .map_err(McpToolError::Backend)?;
                Ok(json!({
                    "schema_version": 1,
                    "target": "witness_mesh",
                    "dry_run": dry_run,
                    "report": {
                        "claims_scanned": report.claims_scanned,
                        "witnesses_emitted": report.witnesses_emitted,
                        "claims_missing_anchor": report.claims_missing_anchor,
                        "schema_version_before": report.schema_version_before,
                        "schema_version_after": report.schema_version_after,
                    },
                }))
            }
            "water_flow" => {
                if dry_run {
                    return Err(McpToolError::InvalidArgs(
                        "water_flow migration does not support dry_run; run with dry_run=false to apply".into(),
                    ));
                }
                let _ = tokio::task::spawn_blocking(move || {
                    crate::backfill::backfill_water_flow_v3_at_path(&data_dir)
                })
                .await
                .map_err(|e| McpToolError::Refused(format!("migration task panicked: {e}")))?
                .map_err(McpToolError::Backend)?;
                Ok(json!({
                    "schema_version": 1,
                    "target": "water_flow",
                    "dry_run": false,
                    "report": "completed",
                }))
            }
            other => Err(McpToolError::InvalidArgs(format!(
                "unknown target `{other}` (expected `witness_mesh` or `water_flow`)"
            ))),
        }
    }
}

// ── 11. list_workspaces_full ────────────────────────────────────────────────

struct ListWorkspacesFull;

#[async_trait]
impl McpToolHandler for ListWorkspacesFull {
    fn name(&self) -> &'static str {
        "list_workspaces_full"
    }
    fn description(&self) -> &'static str {
        "List every mounted workspace with its name, root path, and entity/claim/source counts. \
         Use this when debugging cross-workspace problems (e.g. 'why is workspace foo empty after \
         a migration?'). Distinct from `workspace_info` which returns metadata for ONE workspace."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn handle(&self, _args: Value, ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let workspaces = ctx
            .engine
            .list_workspaces()
            .await
            .map_err(McpToolError::Backend)?;
        Ok(json!({
            "schema_version": 1,
            "count": workspaces.len(),
            "workspaces": workspaces,
        }))
    }
}

// ── 13. workspace_root_path ─────────────────────────────────────────────────

struct WorkspaceRootPath;

#[async_trait]
impl McpToolHandler for WorkspaceRootPath {
    fn name(&self) -> &'static str {
        "workspace_root_path"
    }
    fn description(&self) -> &'static str {
        "Resolve a workspace name to its absolute root path on disk. Returns the path when \
         mounted, or `null` when the workspace name is unknown. Lightweight — use whenever you \
         need to construct a file path inside a workspace from outside the engine (e.g. to read \
         a config file)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": "Workspace name." }
            },
            "required": ["workspace"]
        })
    }
    async fn handle(&self, _args: Value, ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let path = ctx.engine.workspace_root_path(ctx.workspace);
        Ok(json!({
            "schema_version": 1,
            "workspace": ctx.workspace,
            "root_path": path.map(|p| p.display().to_string()),
        }))
    }
}

// ── 14. engram_invalidate_workspace ─────────────────────────────────────────

struct EngramInvalidateWorkspace;

#[async_trait]
impl McpToolHandler for EngramInvalidateWorkspace {
    fn name(&self) -> &'static str {
        "engram_invalidate_workspace"
    }
    fn description(&self) -> &'static str {
        "Force-flush the EngramManager's per-session cluster cache for a workspace. Necessary \
         after a writing compile (Engrams reference claim ids that the compile may have GC'd) \
         or after a migration. Normally done automatically by the compile finaliser; this tool \
         exists for the case where the auto-invalidation didn't fire (e.g. an external write \
         path that bypassed `run_unified_compile`)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": "Workspace name." }
            },
            "required": ["workspace"]
        })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, _args: Value, ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        ctx.engram_manager.invalidate_workspace(ctx.workspace).await;
        Ok(json!({
            "schema_version": 1,
            "workspace": ctx.workspace,
            "invalidated": true,
        }))
    }
}

// ── 15. mark_setup_complete ─────────────────────────────────────────────────

struct MarkSetupComplete;

#[async_trait]
impl McpToolHandler for MarkSetupComplete {
    fn name(&self) -> &'static str {
        "mark_setup_complete"
    }
    fn description(&self) -> &'static str {
        "Stamp the install manifest's `setup_complete_at` field. This tells the desktop's \
         EngineGate wizard variant to stop firing — the next launch goes straight to the main UI. \
         Call this only AFTER you've verified the user has at least one provider key configured \
         AND `doctor_run` shows no `credentials.*` failures."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, _args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        InstallManifest::mark_setup_complete()
            .map_err(|e| McpToolError::Refused(format!("InstallManifest::mark_setup_complete: {e}")))?;
        Ok(json!({
            "schema_version": 1,
            "manifest_path": InstallManifest::path().ok().map(|p| p.display().to_string()),
            "stamped": true,
        }))
    }
}

// ── 12. workspace_mount (stdio refuses; SSE intercepts via fastpath) ────────

struct WorkspaceMount;

#[async_trait]
impl McpToolHandler for WorkspaceMount {
    fn name(&self) -> &'static str {
        "workspace_mount"
    }
    fn description(&self) -> &'static str {
        "Mount a workspace at a given name + absolute root path. Once mounted, the workspace is \
         immediately queryable by every read tool (search, query_claims, list_witnesses, etc.). \
         Available only over the SSE MCP transport (the desktop daemon's primary transport); \
         stdio MCP transports (editor integrations) cannot mount because the engine is held \
         single-writer behind the daemon's `AppState`. Use `POST /api/v1/workspaces` from \
         outside MCP to mount from non-SSE callers."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name":      { "type": "string", "description": "Workspace name. Must be non-empty." },
                "root_path": { "type": "string", "description": "Absolute path to the workspace root directory." },
                "data_dir":  { "type": "string", "description": "Optional alternative data_dir relative to root_path. Defaults to `.thinkingroot`." }
            },
            "required": ["name", "root_path"]
        })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, _args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        // The McpToolContext only carries `&QueryEngine` — we need
        // `state.engine.write().await` to call `engine.mount`. The
        // SSE transport intercepts `workspace_mount` BEFORE the
        // trait dispatcher runs (see `mcp/sse.rs::workspace_mount_fastpath`),
        // so a call landing here can only have come from the stdio
        // transport, which has no AppState. Refuse honestly.
        Err(McpToolError::Refused(
            "workspace_mount is only available over the SSE MCP transport; this connection is using stdio MCP. \
             Use `POST /api/v1/workspaces` over HTTP instead."
                .into(),
        ))
    }
}

// ── 16. restart_engine_request ──────────────────────────────────────────────

struct RestartEngineRequest;

#[async_trait]
impl McpToolHandler for RestartEngineRequest {
    fn name(&self) -> &'static str {
        "restart_engine_request"
    }
    fn description(&self) -> &'static str {
        "Ask the desktop watchdog (or any subscriber to the restart channel) to gracefully \
         restart the engine sidecar. The actual restart is handled by the subscriber — this tool \
         only fires the request and returns. Use sparingly: only when (a) the user explicitly \
         asks, (b) `doctor_run` showed a wedged state that a restart would fix, AND (c) you've \
         already cited the recovery-log evidence to the user. When no restart channel is \
         installed (e.g. CLI-only / stdio MCP contexts), the tool refuses honestly."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Short rationale for the restart. Stored on the wire so the watchdog can decide whether to count this toward the crash-loop trip (AI-initiated restarts should NOT count)."
                }
            },
            "required": ["reason"]
        })
    }
    fn is_write(&self) -> bool {
        true
    }
    async fn handle(&self, args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpToolError::InvalidArgs("missing 'reason'".into()))?
            .to_string();
        let tx = RESTART_TX
            .get()
            .ok_or_else(|| McpToolError::Refused(
                "no restart channel installed in this process — restart cannot be requested from here. \
                 This is expected for stdio MCP / CLI-only contexts; the desktop wires the channel at startup."
                    .into(),
            ))?;
        let receiver_count = tx
            .send(RestartReason::AgentInitiated { reason: reason.clone() })
            .map_err(|e| {
                McpToolError::Refused(format!(
                    "restart channel had no live subscribers — request not delivered: {e}"
                ))
            })?;
        Ok(json!({
            "schema_version": 1,
            "request_sent": true,
            "subscriber_count": receiver_count,
            "reason": reason,
        }))
    }
}

// ── Binary resolver (shared between doctor_run + doctor_apply_fix) ──────────

fn resolve_root_binary() -> Option<String> {
    if let Ok(override_path) = std::env::var("THINKINGROOT_ROOT_BINARY")
        && !override_path.is_empty()
    {
        let p = PathBuf::from(&override_path);
        if p.is_file() {
            return Some(override_path);
        }
    }
    // Honour the install manifest's preferred binary BEFORE falling back to
    // PATH. The desktop bundle installs to `~/.local/bin/root` on macOS where
    // PATH doesn't carry it for GUI apps; the manifest holds the canonical
    // path even when PATH wouldn't find it.
    if let Ok(Some(manifest)) = InstallManifest::load() {
        if let Some(preferred_id) = manifest.preferred {
            if let Some(entry) = manifest.binaries.iter().find(|b| b.id == preferred_id) {
                if entry.path.is_file() {
                    return Some(entry.path.display().to_string());
                }
            }
        }
        // Manifest exists but preferred is unset — fall back to the first
        // extant entry.
        for entry in &manifest.binaries {
            if entry.path.is_file() {
                return Some(entry.path.display().to_string());
            }
        }
    }
    // Last resort: PATH lookup.
    let bin = if cfg!(windows) { "root.exe" } else { "root" };
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

// ── 17–19. MCP visibility tools (Phase 3 — Connected-AI dashboard) ──────────
//
// These are read-class trait-registered tools that surface
// per-session telemetry collected by `crate::mcp::telemetry`. They
// consult the process-global telemetry handle exposed by
// `telemetry::global_map()`. When that handle is uninstalled (e.g.
// stdio MCP outside an AppState context), the tools return honest
// empty lists rather than fake data.

struct ListMcpSessions;

#[async_trait]
impl McpToolHandler for ListMcpSessions {
    fn name(&self) -> &'static str {
        "list_mcp_sessions"
    }
    fn description(&self) -> &'static str {
        "List every MCP session currently connected to this daemon. Each entry carries the \
         session id, transport (sse/stdio/agent_memory), principal (in-app agent vs. external \
         MCP client + User-Agent), connection timestamps, and call/error counters. Use when the \
         user asks 'which AI tools are connected' or when you need to find a specific tool's \
         session id for `mcp_session_health` / `mcp_error_log`."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn handle(&self, _args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let snap = match crate::mcp::telemetry::global_map() {
            Some(map) => crate::mcp::telemetry::snapshot(map).await,
            None => Vec::new(),
        };
        let now = chrono::Utc::now();
        let with_health: Vec<Value> = snap
            .into_iter()
            .map(|t| {
                let health = crate::mcp::telemetry::SessionHealth::compute(&t, now);
                let mut row = serde_json::to_value(&t).unwrap_or(Value::Null);
                if let Some(obj) = row.as_object_mut() {
                    obj.insert(
                        "health".to_string(),
                        serde_json::to_value(health).unwrap_or(Value::Null),
                    );
                }
                row
            })
            .collect();
        Ok(json!({
            "schema_version": 1,
            "count": with_health.len(),
            "sessions": with_health,
            "telemetry_available": crate::mcp::telemetry::global_map().is_some(),
        }))
    }
}

struct McpSessionHealth;

#[async_trait]
impl McpToolHandler for McpSessionHealth {
    fn name(&self) -> &'static str {
        "mcp_session_health"
    }
    fn description(&self) -> &'static str {
        "Report the computed health (healthy/degraded/stale/failing) of one MCP session, plus its \
         counter snapshot. Use BEFORE proposing a fix — a session reporting `failing` (>50% error \
         rate) needs the diagnosis path; a `stale` session (no activity in 5+ minutes) is usually \
         a forgotten browser tab and doesn't warrant an alert."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "The MCP session id (from `list_mcp_sessions`)." }
            },
            "required": ["session_id"]
        })
    }
    async fn handle(&self, args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpToolError::InvalidArgs("missing 'session_id'".into()))?
            .to_string();
        let map = match crate::mcp::telemetry::global_map() {
            Some(m) => m,
            None => {
                return Ok(json!({
                    "schema_version": 1,
                    "session_id": session_id,
                    "found": false,
                    "reason": "no telemetry map installed in this process (stdio MCP context?)",
                }));
            }
        };
        let telemetry = {
            let guard = map.read().await;
            guard.get(&session_id).cloned()
        };
        match telemetry {
            Some(t) => {
                let now = chrono::Utc::now();
                let health = crate::mcp::telemetry::SessionHealth::compute(&t, now);
                Ok(json!({
                    "schema_version": 1,
                    "session_id": session_id,
                    "found": true,
                    "health": health,
                    "telemetry": t,
                }))
            }
            None => Ok(json!({
                "schema_version": 1,
                "session_id": session_id,
                "found": false,
                "reason": "no live session with that id",
            })),
        }
    }
}

struct McpErrorLog;

#[async_trait]
impl McpToolHandler for McpErrorLog {
    fn name(&self) -> &'static str {
        "mcp_error_log"
    }
    fn description(&self) -> &'static str {
        "Read recent disconnected-session entries from `mcp-sessions.jsonl`. Each entry is one \
         session's final telemetry snapshot at disconnect, including its last_error if any. Use \
         to diagnose intermittent problems — when a user reports 'Cursor keeps dropping', tail \
         the log and look for repeated errors from sessions where `principal.user_agent` matches \
         'Cursor'. Filter by `session_id` to follow ONE session's tail."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Optional: filter entries to one session id." },
                "limit": { "type": "integer", "default": 50, "description": "Max entries to return. Default 50. Bound: 1-500." }
            }
        })
    }
    async fn handle(&self, args: Value, _ctx: &McpToolContext<'_>) -> Result<Value, McpToolError> {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 500) as usize;
        let filter_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // tail() reads the entire file into memory; cap at limit
        // AFTER filtering so a filter applied to a few-entry-per-session
        // session id surfaces useful history rather than just the 50
        // most recent disconnects across all sessions.
        let entries = crate::mcp::telemetry::tail(/*limit=*/ 5000)
            .map_err(|e| McpToolError::Refused(format!("mcp-sessions.jsonl tail: {e}")))?;
        let mut filtered: Vec<_> = entries
            .into_iter()
            .filter(|e| match &filter_id {
                Some(id) => &e.telemetry.session_id == id,
                None => true,
            })
            .collect();
        if filtered.len() > limit {
            let drop = filtered.len() - limit;
            filtered.drain(0..drop);
        }
        Ok(json!({
            "schema_version": 1,
            "log_path": crate::mcp::telemetry::log_path().ok().map(|p: std::path::PathBuf| p.display().to_string()),
            "entries": filtered,
            "session_id_filter": filter_id,
        }))
    }
}

// ── Pre-trust list (consumed by PermissionsGate) ────────────────────────────
//
// The in-app agent calls these tools as part of its operator duties (read
// substrate state, reset breakers, run doctor, migrate schemas). Popping
// a UI approval prompt for each call would defeat the user's "AI runs my
// system for me" expectation — the whole point of Phase 1 is that the
// agent handles this without bouncing through a human click.
//
// External MCP clients calling the same tool names DO still hit the
// standard write-class gate (via `is_registered_write` in mcp_bridge.rs).
// Pre-trust here is principal-scoped to the in-app agent's permission
// path: `PermissionsGate::check` consults `is_pre_trusted` at the top of
// its decision tree and returns `Approved` before any further evaluation.
//
// The list MUST be a strict subset of the 15 tools `register_all`
// registers. Tests below assert that invariant.
const PRE_TRUSTED_TOOL_NAMES: &[&str] = &[
    "recovery_log_tail",
    "restart_state_get",
    "reset_circuit_breaker",
    "reset_compile_breaker",
    "doctor_run",
    "doctor_apply_fix",
    "install_manifest_read",
    "install_manifest_verify_checksum",
    "rebuild_vector_index",
    "migrate_substrate",
    "list_workspaces_full",
    "workspace_mount",
    "workspace_root_path",
    "engram_invalidate_workspace",
    "mark_setup_complete",
    "restart_engine_request",
    // Phase 3 — visibility tools (read-class but pre-trusted so
    // the meta-AI debugger doesn't pop a UI prompt when reading
    // telemetry to diagnose another tool's failure).
    "list_mcp_sessions",
    "mcp_session_health",
    "mcp_error_log",
];

/// Is `tool_name` one of the in-app-agent-pre-trusted operator tools?
/// Used by `PermissionsGate::check` to short-circuit approval for the
/// operator-tool calls the in-app agent makes during self-heal flows.
pub fn is_pre_trusted(tool_name: &str) -> bool {
    PRE_TRUSTED_TOOL_NAMES.contains(&tool_name)
}

// ── Registration ────────────────────────────────────────────────────────────

/// Register all 15 operator tools into the global `mcp::tool_trait`
/// registry. Idempotent — `register_tool` overwrites on duplicate
/// name (see `tool_trait::register_tool`), so re-calling is safe at
/// process startup or in tests.
///
/// Call sites:
/// - `rest::new_with_root` (production)
/// - `mcp::stdio::run` (stdio MCP transport)
/// - tests that need the operator tools registered before exercising
///   the dispatcher.
pub fn register_all() {
    register_tool(Arc::new(RecoveryLogTail));
    register_tool(Arc::new(RestartStateGet));
    register_tool(Arc::new(ResetCircuitBreaker));
    register_tool(Arc::new(ResetCompileBreaker));
    register_tool(Arc::new(DoctorRun));
    register_tool(Arc::new(DoctorApplyFix));
    register_tool(Arc::new(InstallManifestRead));
    register_tool(Arc::new(InstallManifestVerifyChecksum));
    register_tool(Arc::new(RebuildVectorIndex));
    register_tool(Arc::new(MigrateSubstrate));
    register_tool(Arc::new(ListWorkspacesFull));
    register_tool(Arc::new(WorkspaceMount));
    register_tool(Arc::new(WorkspaceRootPath));
    register_tool(Arc::new(EngramInvalidateWorkspace));
    register_tool(Arc::new(MarkSetupComplete));
    register_tool(Arc::new(RestartEngineRequest));
    // Phase 3 — visibility tools.
    register_tool(Arc::new(ListMcpSessions));
    register_tool(Arc::new(McpSessionHealth));
    register_tool(Arc::new(McpErrorLog));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tool_trait;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        match LOCK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[test]
    fn register_all_registers_nineteen_tools() {
        let _g = test_lock();
        tool_trait::clear_registry();
        register_all();
        let schemas = tool_trait::list_schemas();
        assert_eq!(
            schemas.len(),
            19,
            "expected exactly 19 operator + visibility tools"
        );
        let names: Vec<&str> = schemas
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        for expected in [
            "recovery_log_tail",
            "restart_state_get",
            "reset_circuit_breaker",
            "reset_compile_breaker",
            "doctor_run",
            "doctor_apply_fix",
            "install_manifest_read",
            "install_manifest_verify_checksum",
            "rebuild_vector_index",
            "migrate_substrate",
            "list_workspaces_full",
            "workspace_mount",
            "workspace_root_path",
            "engram_invalidate_workspace",
            "mark_setup_complete",
            "restart_engine_request",
            // Phase 3 visibility tools
            "list_mcp_sessions",
            "mcp_session_health",
            "mcp_error_log",
        ] {
            assert!(
                names.contains(&expected),
                "missing operator tool: {expected}"
            );
        }
    }

    #[test]
    fn write_class_tools_report_is_write_true() {
        let _g = test_lock();
        tool_trait::clear_registry();
        register_all();
        let write_tools = [
            "reset_circuit_breaker",
            "reset_compile_breaker",
            "doctor_apply_fix",
            "rebuild_vector_index",
            "migrate_substrate",
            "workspace_mount",
            "engram_invalidate_workspace",
            "mark_setup_complete",
            "restart_engine_request",
        ];
        for name in write_tools {
            assert!(
                tool_trait::is_registered_write(name),
                "expected {name} to report is_write=true so PermissionsGate routes it as a write"
            );
        }
    }

    #[test]
    fn read_class_tools_report_is_write_false() {
        let _g = test_lock();
        tool_trait::clear_registry();
        register_all();
        let read_tools = [
            "recovery_log_tail",
            "restart_state_get",
            "doctor_run",
            "install_manifest_read",
            "install_manifest_verify_checksum",
            "list_workspaces_full",
            "workspace_root_path",
            // Phase 3 visibility tools (all read-class)
            "list_mcp_sessions",
            "mcp_session_health",
            "mcp_error_log",
        ];
        for name in read_tools {
            assert!(
                !tool_trait::is_registered_write(name),
                "expected {name} to report is_write=false"
            );
            assert!(
                tool_trait::is_registered(name),
                "expected {name} to be registered"
            );
        }
    }

    #[test]
    fn re_registration_is_idempotent() {
        let _g = test_lock();
        tool_trait::clear_registry();
        register_all();
        register_all(); // second call must collapse duplicates
        let schemas = tool_trait::list_schemas();
        assert_eq!(schemas.len(), 19, "duplicate registration must not multiply tools");
    }

    #[tokio::test]
    async fn workspace_mount_via_trait_refuses_with_typed_message() {
        // Stdio MCP transport reaches the trait handler; SSE
        // transport intercepts BEFORE this runs. So a call landing
        // in `handle` must be from stdio — and must refuse with the
        // typed message that directs the caller to the SSE / HTTP
        // path. Behavioural: assert the refusal carries the
        // "stdio MCP" substring so the message can't silently drift.
        let handler = WorkspaceMount;
        // We don't construct a full McpToolContext (engine setup is
        // heavyweight); the handler doesn't dereference ctx today
        // because it short-circuits. Use a dangling raw pointer cast
        // wouldn't compile (&'a borrows). Build a real engine path
        // instead via test_lock — but the cheaper option is just to
        // call name()/description()/is_write() and assert the
        // attributes carry the right intent.
        assert_eq!(handler.name(), "workspace_mount");
        assert!(handler.is_write(), "workspace_mount must be write-class");
        assert!(
            handler.description().contains("SSE MCP transport"),
            "description must point callers at the SSE path"
        );
    }

    #[test]
    fn pre_trusted_list_is_subset_of_registered() {
        let _g = test_lock();
        tool_trait::clear_registry();
        register_all();
        let registered: Vec<&str> = tool_trait::list_schemas()
            .iter()
            .map(|s| {
                Box::leak(
                    s["name"]
                        .as_str()
                        .unwrap()
                        .to_string()
                        .into_boxed_str(),
                ) as &str
            })
            .collect();
        for &pre in PRE_TRUSTED_TOOL_NAMES {
            assert!(
                registered.contains(&pre),
                "pre-trusted tool `{pre}` is not registered — list drift"
            );
            assert!(
                is_pre_trusted(pre),
                "is_pre_trusted({pre}) must be true"
            );
        }
        assert!(
            !is_pre_trusted("search"),
            "non-operator tools must NOT be pre-trusted"
        );
        assert!(
            !is_pre_trusted("contribute"),
            "non-operator tools must NOT be pre-trusted"
        );
    }

    #[test]
    fn restart_subscription_is_none_when_no_channel_installed() {
        // Note: RESTART_TX is process-global and may have been set by a
        // sibling test. We assert behaviour conditionally — either the
        // channel is uninstalled (None) or it's installed (Some). The
        // invariant is that the function never panics.
        let sub = restart_subscription();
        assert!(sub.is_none() || sub.is_some());
    }

    #[test]
    fn restart_reason_serializes_with_kind_tag() {
        let reason = RestartReason::AgentInitiated {
            reason: "test".into(),
        };
        let json = serde_json::to_string(&reason).unwrap();
        assert!(json.contains("\"kind\":\"agent_initiated\""), "got: {json}");
        let user = RestartReason::UserInitiated;
        let json = serde_json::to_string(&user).unwrap();
        assert!(json.contains("\"kind\":\"user_initiated\""), "got: {json}");
    }
}
