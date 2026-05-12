//! Tauri commands for the self-heal subsystem.
//!
//! Spec: docs/superpowers/specs/2026-05-11-install-runtime-smoothness-design.md §7.

use serde::Serialize;
use thinkingroot_core::recovery_log::{self, RecoveryEvent};
use thinkingroot_core::restart_state::RestartState;

#[derive(Serialize)]
pub struct CircuitBreakerStatus {
    pub active: bool,
    pub until_rfc3339: Option<String>,
    pub recent_failure_count: usize,
    pub recent_crash_signal_count: usize,
}

/// Read current breaker state for the UI.
#[tauri::command]
pub fn get_circuit_breaker_status() -> Result<CircuitBreakerStatus, String> {
    let mut state = RestartState::load().map_err(|e| format!("read restart-state: {e}"))?;
    state.prune();
    Ok(CircuitBreakerStatus {
        active: state.breaker_active(),
        until_rfc3339: state.circuit_breaker_until.map(|t| t.to_rfc3339()),
        recent_failure_count: state.recent_failure_count(),
        recent_crash_signal_count: state.recent_crash_signal_count(),
    })
}

/// Manually reset the circuit breaker.  Called by the UI's
/// "Reset and try again" button.  Clears the breaker timestamp +
/// attempts list, then writes a CircuitBreakerReset recovery event.
#[tauri::command]
pub fn reset_circuit_breaker() -> Result<(), String> {
    let mut state = RestartState::load().map_err(|e| format!("read restart-state: {e}"))?;
    state.reset_circuit_breaker();
    state
        .save()
        .map_err(|e| format!("write restart-state: {e}"))?;
    let _ = recovery_log::append(&RecoveryEvent::circuit_breaker_reset("manual_ui_reset"));
    Ok(())
}
