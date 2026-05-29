//! Root Function storage — versioned code units plus an invocation
//! run-log. Execution lives in `thinkingroot-serve`'s feature-gated
//! `deno_core` isolate executor; this crate only persists definitions
//! and run records (the engine→cloud Console reads them over REST).
//!
//! Versioning mirrors `prompt.rs`: every `put_function` appends a new
//! version row, so a redeploy never clobbers an old body. The flow
//! `root_function` node and the REST `invoke` path both resolve the
//! *latest* version by name.

use std::collections::BTreeMap;

use cozo::{DataValue, Num, ScriptMutability};
use serde::{Deserialize, Serialize};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// One stored function version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RootFunction {
    /// Storage id: `"{name}@{version}"`.
    pub id: String,
    pub name: String,
    pub body: String,
    pub language: String,
    pub version: i64,
    pub created_at: f64,
}

/// One invocation record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RootFunctionRun {
    pub id: String,
    pub function_name: String,
    /// `"ok"` | `"error"`.
    pub status: String,
    pub started_at: f64,
    pub finished_at: f64,
    /// JSON-encoded return value on success, else `""`.
    pub output_json: String,
    /// Error message on failure, else `""`.
    pub error: String,
}

fn dv_str(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        DataValue::Num(Num::Int(i)) => i.to_string(),
        DataValue::Num(Num::Float(f)) => f.to_string(),
        _ => String::new(),
    }
}
fn dv_i64(v: &DataValue) -> i64 {
    match v {
        DataValue::Num(Num::Int(i)) => *i,
        DataValue::Num(Num::Float(f)) => *f as i64,
        _ => 0,
    }
}
fn dv_f64(v: &DataValue) -> f64 {
    match v {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

fn row_to_function(row: &[DataValue]) -> RootFunction {
    RootFunction {
        id: dv_str(&row[0]),
        name: dv_str(&row[1]),
        body: dv_str(&row[2]),
        language: dv_str(&row[3]),
        version: dv_i64(&row[4]),
        created_at: dv_f64(&row[5]),
    }
}

fn row_to_run(row: &[DataValue]) -> RootFunctionRun {
    RootFunctionRun {
        id: dv_str(&row[0]),
        function_name: dv_str(&row[1]),
        status: dv_str(&row[2]),
        started_at: dv_f64(&row[3]),
        finished_at: dv_f64(&row[4]),
        output_json: dv_str(&row[5]),
        error: dv_str(&row[6]),
    }
}

impl GraphStore {
    /// Deploy a new function version (`max(version)+1`). Returns the row.
    pub fn put_function(&self, name: &str, body: &str, language: &str) -> Result<RootFunction> {
        if name.trim().is_empty() {
            return Err(Error::Template("root function name must be non-empty".into()));
        }
        let version = self.function_latest_version(name)?.unwrap_or(0) + 1;
        let id = format!("{name}@{version}");
        let created_at = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let lang = if language.trim().is_empty() { "js" } else { language };

        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.clone().into()));
        params.insert("name".into(), DataValue::Str(name.into()));
        params.insert("body".into(), DataValue::Str(body.into()));
        params.insert("language".into(), DataValue::Str(lang.into()));
        params.insert("version".into(), DataValue::Num(Num::Int(version)));
        params.insert("created_at".into(), DataValue::Num(Num::Float(created_at)));

        self.query(
            r#"?[id, name, body, language, version, created_at] <- [[
                $id, $name, $body, $language, $version, $created_at
            ]]
            :put root_functions {id => name, body, language, version, created_at}"#,
            params,
        )?;

        Ok(RootFunction {
            id,
            name: name.to_string(),
            body: body.to_string(),
            language: lang.to_string(),
            version,
            created_at,
        })
    }

    pub fn function_latest_version(&self, name: &str) -> Result<Option<i64>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[version] := *root_functions{name: $name, version}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("function_latest_version: {e}")))?;
        Ok(rows.rows.iter().map(|r| dv_i64(&r[0])).max())
    }

    /// Latest version of a single function, or `None`.
    pub fn get_function(&self, name: &str) -> Result<Option<RootFunction>> {
        let v = match self.function_latest_version(name)? {
            Some(v) => v,
            None => return Ok(None),
        };
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        params.insert("version".into(), DataValue::Num(Num::Int(v)));
        let rows = self
            .raw_db()
            .run_script(
                "?[id, name, body, language, version, created_at] := \
                 *root_functions{id, name, body, language, version, created_at}, \
                 name = $name, version = $version",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_function: {e}")))?;
        Ok(rows.rows.first().map(|r| row_to_function(r)))
    }

    /// The latest version of every distinct function, sorted by name.
    pub fn list_functions(&self) -> Result<Vec<RootFunction>> {
        let rows = self
            .query_read(
                "?[id, name, body, language, version, created_at] := \
                 *root_functions{id, name, body, language, version, created_at}",
            )?
            .rows
            .iter()
            .map(|r| row_to_function(r))
            .collect::<Vec<_>>();
        let mut latest: BTreeMap<String, RootFunction> = BTreeMap::new();
        for f in rows {
            match latest.get(&f.name) {
                Some(existing) if existing.version >= f.version => {}
                _ => {
                    latest.insert(f.name.clone(), f);
                }
            }
        }
        Ok(latest.into_values().collect())
    }

    /// Record an invocation. `id` is caller-supplied (ULID/uuid) so the
    /// run can be referenced before it finishes.
    #[allow(clippy::too_many_arguments)]
    pub fn record_function_run(&self, run: &RootFunctionRun) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(run.id.clone().into()));
        params.insert("function_name".into(), DataValue::Str(run.function_name.clone().into()));
        params.insert("status".into(), DataValue::Str(run.status.clone().into()));
        params.insert("started_at".into(), DataValue::Num(Num::Float(run.started_at)));
        params.insert("finished_at".into(), DataValue::Num(Num::Float(run.finished_at)));
        params.insert("output_json".into(), DataValue::Str(run.output_json.clone().into()));
        params.insert("error".into(), DataValue::Str(run.error.clone().into()));
        self.query(
            r#"?[id, function_name, status, started_at, finished_at, output_json, error] <- [[
                $id, $function_name, $status, $started_at, $finished_at, $output_json, $error
            ]]
            :put root_function_runs {id => function_name, status, started_at, finished_at, output_json, error}"#,
            params,
        )?;
        Ok(())
    }

    /// Runs for a function, most-recent first.
    pub fn list_function_runs(&self, name: &str) -> Result<Vec<RootFunctionRun>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[id, function_name, status, started_at, finished_at, output_json, error] := \
                 *root_function_runs{id, function_name, status, started_at, finished_at, output_json, error}, \
                 function_name = $name",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("list_function_runs: {e}")))?;
        let mut out: Vec<RootFunctionRun> = rows.rows.iter().map(|r| row_to_run(r)).collect();
        out.sort_by(|a, b| b.started_at.total_cmp(&a.started_at));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> GraphStore {
        let db = cozo::DbInstance::new("mem", "", "").unwrap();
        let s = GraphStore::from_db_for_testing(db);
        s.init_for_testing().unwrap();
        s
    }

    #[test]
    fn deploy_versions_and_lists_latest() {
        let s = store();
        let v1 = s.put_function("hello", "export default () => 1", "js").unwrap();
        assert_eq!(v1.version, 1);
        let v2 = s.put_function("hello", "export default () => 2", "js").unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(s.function_latest_version("hello").unwrap(), Some(2));
        let latest = s.get_function("hello").unwrap().unwrap();
        assert_eq!(latest.version, 2);
        assert!(latest.body.contains("=> 2"));
        let all = s.list_functions().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].version, 2);
        assert!(s.get_function("nope").unwrap().is_none());
    }

    #[test]
    fn run_log_round_trips_newest_first() {
        let s = store();
        s.record_function_run(&RootFunctionRun {
            id: "run1".into(),
            function_name: "hello".into(),
            status: "ok".into(),
            started_at: 100.0,
            finished_at: 100.5,
            output_json: "1".into(),
            error: String::new(),
        })
        .unwrap();
        s.record_function_run(&RootFunctionRun {
            id: "run2".into(),
            function_name: "hello".into(),
            status: "error".into(),
            started_at: 200.0,
            finished_at: 200.2,
            output_json: String::new(),
            error: "boom".into(),
        })
        .unwrap();
        let runs = s.list_function_runs("hello").unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "run2", "newest first");
        assert_eq!(runs[1].id, "run1");
    }
}
