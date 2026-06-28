//! `compile_jobs` — durable compile-run record.
//!
//! Makes a compile a *persistent queued job* rather than an ephemeral HTTP
//! side effect. Three properties fall out of one durable Cozo row:
//!
//!  * **Survives the browser closing** — the streaming compile detaches its
//!    work from the SSE connection (see `rest.rs` `active_compiles`); the row
//!    keeps tracking the live `phase` regardless of any observer.
//!  * **Re-attachable** — a reconnecting console reads the row for a snapshot
//!    and re-subscribes to the in-flight progress broadcast.
//!  * **Crash-honest** — on engine boot, a row still `running` from a *previous*
//!    process (`host_pid` mismatch) is marked `interrupted`, never silently
//!    reported as `done`.
//!
//! Mirrors the `atomic_extract_queue` idiom in `atomic_fact_inserts.rs`: a
//! Cozo relation inside each project's `graph.db`, created idempotently on
//! mount (zero Postgres migration).

use std::collections::BTreeMap;

use cozo::{DataValue, Num};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

fn s(value: impl Into<String>) -> DataValue {
    DataValue::Str(value.into().into())
}
fn i(value: i64) -> DataValue {
    DataValue::Num(Num::Int(value))
}
fn f(value: f64) -> DataValue {
    DataValue::Num(Num::Float(value))
}
fn ds(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}
fn du(v: &DataValue) -> u64 {
    match v {
        DataValue::Num(Num::Int(i)) => *i as u64,
        DataValue::Num(Num::Float(f)) => *f as u64,
        _ => 0,
    }
}
fn df(v: &DataValue) -> f64 {
    match v {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

/// A durable compile-run record (one row of `compile_jobs`).
#[derive(Debug, Clone, PartialEq)]
pub struct CompileJobRow {
    pub job_id: String,
    pub ws: String,
    pub root_path: String,
    pub branch: String,
    /// `running` | `done` | `failed` | `interrupted` | `cancelled`.
    pub status: String,
    /// Coarse pipeline phase token for live progress (`reading`, `extracting`, …).
    pub phase: String,
    pub source_count: i64,
    pub started_at: f64,
    pub updated_at: f64,
    pub error: String,
    /// PID of the process that owns the run (boot-sweep liveness check).
    pub host_pid: i64,
}

const COLS: &str = "job_id, ws, root_path, branch, status, phase, source_count, \
    started_at, updated_at, error, host_pid";

/// Non-PK columns (everything after `job_id`) for the `:put … {job_id => …}` clause.
const NONPK_COLS: &str = "ws, root_path, branch, status, phase, source_count, \
    started_at, updated_at, error, host_pid";

fn row_to_job(row: &[DataValue]) -> Option<CompileJobRow> {
    if row.len() < 11 {
        return None;
    }
    Some(CompileJobRow {
        job_id: ds(&row[0]),
        ws: ds(&row[1]),
        root_path: ds(&row[2]),
        branch: ds(&row[3]),
        status: ds(&row[4]),
        phase: ds(&row[5]),
        source_count: du(&row[6]) as i64,
        started_at: df(&row[7]),
        updated_at: df(&row[8]),
        error: ds(&row[9]),
        host_pid: du(&row[10]) as i64,
    })
}

impl GraphStore {
    /// Insert a fresh `running` compile job. `host_pid` is the owning
    /// process id (`std::process::id()`), used by the boot-sweep to detect
    /// rows orphaned by a crash.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_compile_job(
        &self,
        job_id: &str,
        ws: &str,
        root_path: &str,
        branch: &str,
        source_count: i64,
        now: f64,
        host_pid: i64,
    ) -> Result<()> {
        let row = DataValue::List(vec![
            s(job_id),
            s(ws),
            s(root_path),
            s(branch),
            s("running"),
            s("starting"),
            i(source_count),
            f(now),
            f(now),
            s(""),
            i(host_pid),
        ]);
        let mut params = BTreeMap::new();
        params.insert("rows".into(), DataValue::List(vec![row]));
        let script = format!("?[{COLS}] <- $rows\n:put compile_jobs {{ job_id => {NONPK_COLS} }}");
        self.query(&script, params)
            .map_err(|e| Error::GraphStorage(format!("insert_compile_job: {e}")))?;
        Ok(())
    }

    /// Update the live phase of a running job (cheap, called per phase
    /// transition by the compile forwarder). No-op if the job is gone.
    pub fn update_compile_job_phase(&self, job_id: &str, phase: &str, now: f64) -> Result<()> {
        let Some(job) = self.get_compile_job(job_id)? else {
            return Ok(());
        };
        let row = DataValue::List(vec![
            s(&job.job_id),
            s(&job.ws),
            s(&job.root_path),
            s(&job.branch),
            s(&job.status),
            s(phase),
            i(job.source_count),
            f(job.started_at),
            f(now),
            s(&job.error),
            i(job.host_pid),
        ]);
        let mut params = BTreeMap::new();
        params.insert("rows".into(), DataValue::List(vec![row]));
        let script = format!("?[{COLS}] <- $rows\n:put compile_jobs {{ job_id => {NONPK_COLS} }}");
        self.query(&script, params)
            .map_err(|e| Error::GraphStorage(format!("update_compile_job_phase: {e}")))?;
        Ok(())
    }

    /// Move a job to a terminal status. `done` is the ONLY success value and
    /// must reflect a genuine successful finish (honesty rule). No-op if gone.
    pub fn finish_compile_job(
        &self,
        job_id: &str,
        status: &str,
        error: &str,
        now: f64,
    ) -> Result<()> {
        let Some(job) = self.get_compile_job(job_id)? else {
            return Ok(());
        };
        let row = DataValue::List(vec![
            s(&job.job_id),
            s(&job.ws),
            s(&job.root_path),
            s(&job.branch),
            s(status),
            s(&job.phase),
            i(job.source_count),
            f(job.started_at),
            f(now),
            s(error),
            i(job.host_pid),
        ]);
        let mut params = BTreeMap::new();
        params.insert("rows".into(), DataValue::List(vec![row]));
        let script = format!("?[{COLS}] <- $rows\n:put compile_jobs {{ job_id => {NONPK_COLS} }}");
        self.query(&script, params)
            .map_err(|e| Error::GraphStorage(format!("finish_compile_job: {e}")))?;
        Ok(())
    }

    /// One compile job by id (re-attach snapshot + provisioner `/busy`).
    pub fn get_compile_job(&self, job_id: &str) -> Result<Option<CompileJobRow>> {
        let mut params = BTreeMap::new();
        params.insert("jid".into(), DataValue::Str(job_id.into()));
        let script = format!("?[{COLS}] := *compile_jobs{{{COLS}}}, job_id == $jid");
        let res = self.query(&script, params)?;
        Ok(res.rows.first().and_then(|r| row_to_job(r)))
    }

    /// All jobs currently in `running` status (boot-sweep + `/busy`).
    pub fn list_running_compile_jobs(&self) -> Result<Vec<CompileJobRow>> {
        let script =
            format!("?[{COLS}] := *compile_jobs{{{COLS}}}, status == 'running'");
        let res = self.query_read(&script)?;
        Ok(res.rows.iter().filter_map(|r| row_to_job(r)).collect())
    }

    /// Recent jobs in ANY status, newest-first (by `updated_at`) — the
    /// Import-page queue/history view. Bounded by `limit` so the read stays
    /// cheap regardless of how many compiles a workspace has accumulated.
    pub fn list_recent_compile_jobs(&self, limit: usize) -> Result<Vec<CompileJobRow>> {
        let script = format!(
            "?[{COLS}] := *compile_jobs{{{COLS}}}\n:order -updated_at\n:limit {limit}"
        );
        let res = self.query_read(&script)?;
        Ok(res.rows.iter().filter_map(|r| row_to_job(r)).collect())
    }

    /// Cheap count of `running` jobs — the authoritative busy signal (it
    /// survives an observer disconnecting, unlike an in-memory counter).
    pub fn running_compile_count(&self) -> Result<usize> {
        Ok(self.list_running_compile_jobs()?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::init(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn insert_phase_finish_lifecycle() {
        let (_d, store) = store();
        store
            .insert_compile_job("job1", "main", "/tmp/p", "", 3, 1.0, 4242)
            .unwrap();

        let j = store.get_compile_job("job1").unwrap().unwrap();
        assert_eq!(j.status, "running");
        assert_eq!(j.phase, "starting");
        assert_eq!(j.source_count, 3);
        assert_eq!(j.host_pid, 4242);
        assert_eq!(store.running_compile_count().unwrap(), 1);

        store.update_compile_job_phase("job1", "extracting", 2.0).unwrap();
        let j = store.get_compile_job("job1").unwrap().unwrap();
        assert_eq!(j.phase, "extracting");
        assert_eq!(j.status, "running", "phase update does not change status");

        store.finish_compile_job("job1", "done", "", 3.0).unwrap();
        let j = store.get_compile_job("job1").unwrap().unwrap();
        assert_eq!(j.status, "done");
        assert_eq!(j.updated_at, 3.0);
        assert_eq!(store.running_compile_count().unwrap(), 0);
    }

    #[test]
    fn list_running_excludes_terminal() {
        let (_d, store) = store();
        store.insert_compile_job("a", "main", "/p", "", 1, 1.0, 1).unwrap();
        store.insert_compile_job("b", "main", "/p", "", 1, 1.0, 1).unwrap();
        store.finish_compile_job("b", "failed", "boom", 2.0).unwrap();

        let running = store.list_running_compile_jobs().unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].job_id, "a");
        let b = store.get_compile_job("b").unwrap().unwrap();
        assert_eq!(b.status, "failed");
        assert_eq!(b.error, "boom");
    }

    #[test]
    fn list_recent_is_newest_first_across_statuses() {
        let (_d, store) = store();
        // Insert three, finish them at increasing `updated_at` timestamps.
        store.insert_compile_job("old", "main", "/p", "", 1, 1.0, 1).unwrap();
        store.insert_compile_job("mid", "main", "/p", "", 1, 1.0, 1).unwrap();
        store.insert_compile_job("new", "main", "/p", "", 1, 1.0, 1).unwrap();
        store.finish_compile_job("old", "done", "", 10.0).unwrap();
        store.finish_compile_job("new", "done", "", 30.0).unwrap();
        store.finish_compile_job("mid", "cancelled", "", 20.0).unwrap();

        let recent = store.list_recent_compile_jobs(10).unwrap();
        assert_eq!(recent.len(), 3, "recent includes terminal jobs");
        let ids: Vec<_> = recent.iter().map(|j| j.job_id.as_str()).collect();
        assert_eq!(ids, vec!["new", "mid", "old"], "newest updated_at first");

        let capped = store.list_recent_compile_jobs(2).unwrap();
        assert_eq!(capped.len(), 2, "limit is honored");
        assert_eq!(capped[0].job_id, "new");
    }

    #[test]
    fn phase_and_finish_are_noops_when_missing() {
        let (_d, store) = store();
        // Must not error if the job id is unknown (terminal cleanup races).
        store.update_compile_job_phase("ghost", "reading", 1.0).unwrap();
        store.finish_compile_job("ghost", "done", "", 1.0).unwrap();
        assert!(store.get_compile_job("ghost").unwrap().is_none());
    }
}
