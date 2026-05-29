//! Flow cron scheduler — fires headless flow runs on a schedule.
//!
//! A flow definition may carry a standard 5-field cron `schedule`
//! (`min hour day-of-month month day-of-week`, UTC). The serve daemon spawns
//! one background task that ticks every ~30s, scans the workspace's flow
//! definitions, and triggers a headless run (same path as `POST .../flows/{id}/run`)
//! for each flow whose schedule matches the current minute — at most once per
//! matching minute (tracked per flow). Missed minutes (daemon down) are NOT
//! caught up: a cron flow runs at most once per matching wall-clock minute the
//! daemon is alive for.
//!
//! No external cron crate (keeps the `--locked` cloud build dep-free): the
//! parser below covers the common subset — `*`, `*/step`, `a`, `a-b`,
//! `a-b/step`, and comma lists thereof.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Timelike, Utc};
use tokio::task::JoinHandle;

use crate::rest::AppState;

/// Does `expr` (5 fields) match instant `dt` (to the minute, UTC)?
/// Returns false for malformed expressions (logged by the caller).
pub fn cron_matches(expr: &str, dt: DateTime<Utc>) -> bool {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    let dow = dt.weekday().num_days_from_sunday(); // 0 = Sunday … 6 = Saturday
    field_matches(fields[0], dt.minute(), 0, 59)
        && field_matches(fields[1], dt.hour(), 0, 23)
        && field_matches(fields[2], dt.day(), 1, 31)
        && field_matches(fields[3], dt.month(), 1, 12)
        && field_matches(fields[4], dow, 0, 6)
}

/// Match one cron field (comma list of terms) against `value`.
fn field_matches(field: &str, value: u32, min: u32, max: u32) -> bool {
    field.split(',').any(|term| term_matches(term, value, min, max))
}

fn term_matches(term: &str, value: u32, min: u32, max: u32) -> bool {
    // Split optional `/step`.
    let (base, step) = match term.split_once('/') {
        Some((b, s)) => (b, s.parse::<u32>().ok().filter(|n| *n > 0)),
        None => (term, None),
    };
    // Resolve the base into an inclusive [lo, hi] range.
    let (lo, hi) = if base == "*" {
        (min, max)
    } else if let Some((a, b)) = base.split_once('-') {
        match (a.parse::<u32>(), b.parse::<u32>()) {
            (Ok(a), Ok(b)) if a <= b => (a, b),
            _ => return false,
        }
    } else {
        match base.parse::<u32>() {
            Ok(n) => (n, n),
            Err(_) => return false,
        }
    };
    if value < lo || value > hi {
        return false;
    }
    match step {
        Some(st) => (value - lo) % st == 0,
        None => true,
    }
}

/// Spawn the flow-cron worker. Ticks every 30s; fires due flows once per
/// matching minute. Returns the handle (the daemon drops it — the task runs
/// for the process lifetime).
pub fn spawn_flow_cron(state: Arc<AppState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        // flow_id -> last fired unix-minute, so a flow fires at most once per
        // matching minute even though we tick more often than once a minute.
        let mut last_fired: HashMap<String, i64> = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(e) = tick(&state, &mut last_fired).await {
                tracing::warn!(target: "flow_cron", "tick failed (non-fatal): {e}");
            }
        }
    })
}

async fn tick(state: &Arc<AppState>, last_fired: &mut HashMap<String, i64>) -> Result<(), String> {
    let root = match state.current_workspace_root().await {
        Some(p) => p,
        None => return Ok(()), // no workspace mounted yet
    };
    let now = Utc::now();
    let minute = now.timestamp() / 60;

    let store = thinkingroot_flow::storage::FlowStore::new(root.clone());
    let defs = store
        .list_flow_definitions()
        .map_err(|e| format!("list flow definitions: {e}"))?;

    for rec in defs {
        let Some(sched) = rec.definition.schedule.as_deref() else {
            continue;
        };
        if !cron_matches(sched, now) {
            continue;
        }
        let id = rec.definition.id.clone();
        if last_fired.get(&id) == Some(&minute) {
            continue; // already fired this minute
        }
        last_fired.insert(id.clone(), minute);

        // Fire a headless run (same executor set as the REST trigger).
        let executors = crate::rest::build_headless_executors(state).await;
        let runtime = thinkingroot_flow::runtime::FlowRuntime::new(
            thinkingroot_flow::storage::FlowStore::new(root.clone()),
            executors,
        );
        match runtime
            .start_run_for_session(&id, "main", "main", serde_json::json!({}), None)
            .await
        {
            Ok(h) => tracing::info!(
                target: "flow_cron",
                flow_id = %id, flow_run_id = %h.flow_run_id, schedule = %sched,
                "cron-triggered flow run started"
            ),
            Err(e) => tracing::warn!(
                target: "flow_cron",
                flow_id = %id, "cron flow run failed to start: {e}"
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn every_minute_star() {
        assert!(cron_matches("* * * * *", at(2026, 5, 29, 13, 7)));
    }

    #[test]
    fn step_minutes() {
        let e = "*/5 * * * *";
        assert!(cron_matches(e, at(2026, 5, 29, 13, 0)));
        assert!(cron_matches(e, at(2026, 5, 29, 13, 5)));
        assert!(!cron_matches(e, at(2026, 5, 29, 13, 7)));
    }

    #[test]
    fn exact_hour_and_minute() {
        let e = "30 9 * * *"; // 09:30 daily
        assert!(cron_matches(e, at(2026, 5, 29, 9, 30)));
        assert!(!cron_matches(e, at(2026, 5, 29, 9, 31)));
        assert!(!cron_matches(e, at(2026, 5, 29, 10, 30)));
    }

    #[test]
    fn range_and_list() {
        let e = "0 9-17 * * 1,2,3,4,5"; // top of hour, business hours, Mon-Fri
        // 2026-05-29 is a Friday (dow 5).
        assert!(cron_matches(e, at(2026, 5, 29, 9, 0)));
        assert!(cron_matches(e, at(2026, 5, 29, 17, 0)));
        assert!(!cron_matches(e, at(2026, 5, 29, 18, 0)));
        // 2026-05-30 is a Saturday (dow 6) → excluded.
        assert!(!cron_matches(e, at(2026, 5, 30, 9, 0)));
    }

    #[test]
    fn malformed_is_false() {
        assert!(!cron_matches("* * *", at(2026, 5, 29, 0, 0))); // too few fields
        assert!(!cron_matches("bogus * * * *", at(2026, 5, 29, 0, 0)));
    }
}
