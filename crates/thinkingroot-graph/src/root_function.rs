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
    /// `"ok"` | `"error"` | `"suspended"`.
    pub status: String,
    pub started_at: f64,
    pub finished_at: f64,
    /// JSON-encoded return value on success, else `""`.
    pub output_json: String,
    /// Error message on failure, else `""`.
    pub error: String,
}

/// A suspended run's outstanding cognition request, awaiting an answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingRequest {
    pub token: String,
    pub run_id: String,
    pub ws: String,
    pub function_name: String,
    /// Journal step key the answer is recorded under (so replay finds it).
    pub step_key: String,
    pub question: String,
    /// JSON-encoded original invocation input, so resume can re-run.
    pub input_json: String,
    /// `"pending"` | `"answered"`.
    pub status: String,
    pub created_at: f64,
}

/// A control-plane-owned test fixture for a Root Function: an input and the
/// expected JSON output. Authored separately from the function body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionFixture {
    pub function_name: String,
    pub fixture_id: String,
    pub input_json: String,
    pub expect_json: String,
}

/// Learned experience for one `(input_class, function)` pair — how well
/// this function has served inputs of this class.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExperienceEntry {
    pub function_name: String,
    /// Raw online accumulator (+1 success / −1 fail, decayed on invalidation).
    /// Kept for transparency; ranking uses [`ExperienceEntry::score`].
    pub weight: f64,
    pub n_success: i64,
    pub n_fail: i64,
}

impl ExperienceEntry {
    /// Ranking score: the Wilson lower bound (95%) of the success rate. This
    /// rewards a *confident* success rate — a function with 3/3 outranks one
    /// with 1/1 (more evidence), and 50/50-at-100% outranks 18/20-at-90% —
    /// rather than ranking by raw volume. Zero when there's no data.
    pub fn score(&self) -> f64 {
        let n = (self.n_success + self.n_fail) as f64;
        if n <= 0.0 {
            return 0.0;
        }
        let p = self.n_success as f64 / n;
        let z = 1.96_f64; // 95% confidence
        let z2 = z * z;
        let centre = p + z2 / (2.0 * n);
        let margin = z * ((p * (1.0 - p) + z2 / (4.0 * n)) / n).sqrt();
        ((centre - margin) / (1.0 + z2 / n)).max(0.0)
    }
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

    /// All journaled durable-execution steps for a run, as
    /// `(step_key, result_json)`. Used to replay a resumed run.
    pub fn list_steps_for_run(&self, run_id: &str) -> Result<Vec<(String, String)>> {
        let mut params = BTreeMap::new();
        params.insert("run_id".into(), DataValue::Str(run_id.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[step_key, result_json] := \
                 *root_function_steps{run_id, step_key, result_json}, run_id = $run_id",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("list_steps_for_run: {e}")))?;
        Ok(rows
            .rows
            .iter()
            .map(|r| (dv_str(&r[0]), dv_str(&r[1])))
            .collect())
    }

    /// Persist newly-recorded steps for a run (idempotent `:put`, keyed on
    /// `(run_id, step_key)` — re-recording the same key is a no-op).
    pub fn record_function_steps(&self, run_id: &str, steps: &[(String, String)]) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        for (key, result_json) in steps {
            let mut params = BTreeMap::new();
            params.insert("run_id".into(), DataValue::Str(run_id.into()));
            params.insert("step_key".into(), DataValue::Str(key.clone().into()));
            params.insert("result_json".into(), DataValue::Str(result_json.clone().into()));
            params.insert("recorded_at".into(), DataValue::Num(Num::Float(now)));
            self.query(
                r#"?[run_id, step_key, result_json, recorded_at] <- [[
                    $run_id, $step_key, $result_json, $recorded_at
                ]]
                :put root_function_steps {run_id, step_key => result_json, recorded_at}"#,
                params,
            )?;
        }
        Ok(())
    }

    /// Register a pending cognition request for a suspended run.
    pub fn put_pending_request(&self, req: &PendingRequest) -> Result<()> {
        let mut params = BTreeMap::new();
        params.insert("token".into(), DataValue::Str(req.token.clone().into()));
        params.insert("run_id".into(), DataValue::Str(req.run_id.clone().into()));
        params.insert("ws".into(), DataValue::Str(req.ws.clone().into()));
        params.insert("function_name".into(), DataValue::Str(req.function_name.clone().into()));
        params.insert("step_key".into(), DataValue::Str(req.step_key.clone().into()));
        params.insert("question".into(), DataValue::Str(req.question.clone().into()));
        params.insert("input_json".into(), DataValue::Str(req.input_json.clone().into()));
        params.insert("status".into(), DataValue::Str(req.status.clone().into()));
        params.insert("created_at".into(), DataValue::Num(Num::Float(req.created_at)));
        self.query(
            r#"?[token, run_id, ws, function_name, step_key, question, input_json, status, created_at] <- [[
                $token, $run_id, $ws, $function_name, $step_key, $question, $input_json, $status, $created_at
            ]]
            :put function_pending_requests {token => run_id, ws, function_name, step_key, question, input_json, status, created_at}"#,
            params,
        )?;
        Ok(())
    }

    /// Look up a pending request by token.
    pub fn get_pending_request(&self, token: &str) -> Result<Option<PendingRequest>> {
        let mut params = BTreeMap::new();
        params.insert("token".into(), DataValue::Str(token.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[token, run_id, ws, function_name, step_key, question, input_json, status, created_at] := \
                 *function_pending_requests{token, run_id, ws, function_name, step_key, question, input_json, status, created_at}, \
                 token = $token",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_pending_request: {e}")))?;
        Ok(rows.rows.first().map(|r| PendingRequest {
            token: dv_str(&r[0]),
            run_id: dv_str(&r[1]),
            ws: dv_str(&r[2]),
            function_name: dv_str(&r[3]),
            step_key: dv_str(&r[4]),
            question: dv_str(&r[5]),
            input_json: dv_str(&r[6]),
            status: dv_str(&r[7]),
            created_at: dv_f64(&r[8]),
        }))
    }

    /// Mark a pending request answered (idempotent; preserves the row for
    /// audit). Re-running `:put` with the same token overwrites status.
    pub fn mark_pending_answered(&self, token: &str) -> Result<()> {
        if let Some(mut req) = self.get_pending_request(token)? {
            req.status = "answered".to_string();
            self.put_pending_request(&req)?;
        }
        Ok(())
    }

    // ─── Run-learning (the moat) ─────────────────────────────────────

    pub fn get_experience(
        &self,
        input_class: &str,
        function_name: &str,
    ) -> Result<Option<ExperienceEntry>> {
        let mut params = BTreeMap::new();
        params.insert("ic".into(), DataValue::Str(input_class.into()));
        params.insert("fnn".into(), DataValue::Str(function_name.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[weight, n_success, n_fail] := \
                 *function_experience{input_class: $ic, function_name: $fnn, weight, n_success, n_fail}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_experience: {e}")))?;
        Ok(rows.rows.first().map(|r| ExperienceEntry {
            function_name: function_name.to_string(),
            weight: dv_f64(&r[0]),
            n_success: dv_i64(&r[1]),
            n_fail: dv_i64(&r[2]),
        }))
    }

    fn put_experience(
        &self,
        input_class: &str,
        function_name: &str,
        weight: f64,
        n_success: i64,
        n_fail: i64,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let mut params = BTreeMap::new();
        params.insert("input_class".into(), DataValue::Str(input_class.into()));
        params.insert("function_name".into(), DataValue::Str(function_name.into()));
        params.insert("weight".into(), DataValue::Num(Num::Float(weight)));
        params.insert("n_success".into(), DataValue::Num(Num::Int(n_success)));
        params.insert("n_fail".into(), DataValue::Num(Num::Int(n_fail)));
        params.insert("updated_at".into(), DataValue::Num(Num::Float(now)));
        self.query(
            r#"?[input_class, function_name, weight, n_success, n_fail, updated_at] <- [[
                $input_class, $function_name, $weight, $n_success, $n_fail, $updated_at
            ]]
            :put function_experience {input_class, function_name => weight, n_success, n_fail, updated_at}"#,
            params,
        )?;
        Ok(())
    }

    /// Online update of learned experience for `(input_class, function)`:
    /// success raises the weight, failure lowers it.
    pub fn bump_function_experience(
        &self,
        input_class: &str,
        function_name: &str,
        success: bool,
    ) -> Result<()> {
        let (mut w, mut ns, mut nf) = self
            .get_experience(input_class, function_name)?
            .map(|e| (e.weight, e.n_success, e.n_fail))
            .unwrap_or((0.0, 0, 0));
        if success {
            ns += 1;
            w += 1.0;
        } else {
            nf += 1;
            w -= 1.0;
        }
        self.put_experience(input_class, function_name, w, ns, nf)
    }

    /// Functions that have served this input class, best (highest weight)
    /// first — the "which function should I run for this input" answer.
    pub fn retrieve_experience(&self, input_class: &str) -> Result<Vec<ExperienceEntry>> {
        let mut params = BTreeMap::new();
        params.insert("ic".into(), DataValue::Str(input_class.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[function_name, weight, n_success, n_fail] := \
                 *function_experience{input_class: $ic, function_name, weight, n_success, n_fail}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("retrieve_experience: {e}")))?;
        let mut out: Vec<ExperienceEntry> = rows
            .rows
            .iter()
            .map(|r| ExperienceEntry {
                function_name: dv_str(&r[0]),
                weight: dv_f64(&r[1]),
                n_success: dv_i64(&r[2]),
                n_fail: dv_i64(&r[3]),
            })
            .collect();
        // Rank by confident success rate (Wilson lower bound), not raw volume.
        out.sort_by(|a, b| b.score().total_cmp(&a.score()));
        Ok(out)
    }

    /// Record that a run touched a graph object (claim/witness/entity),
    /// tagged with the `(input_class, function)` it ran under so a later
    /// change to that object can causally invalidate the experience.
    #[allow(clippy::too_many_arguments)]
    pub fn record_invocation_touch(
        &self,
        run_id: &str,
        object_kind: &str,
        object_id: &str,
        input_class: &str,
        function_name: &str,
        role: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let mut params = BTreeMap::new();
        params.insert("run_id".into(), DataValue::Str(run_id.into()));
        params.insert("object_kind".into(), DataValue::Str(object_kind.into()));
        params.insert("object_id".into(), DataValue::Str(object_id.into()));
        params.insert("input_class".into(), DataValue::Str(input_class.into()));
        params.insert("function_name".into(), DataValue::Str(function_name.into()));
        params.insert("role".into(), DataValue::Str(role.into()));
        params.insert("touched_at".into(), DataValue::Num(Num::Float(now)));
        self.query(
            r#"?[run_id, object_kind, object_id, input_class, function_name, role, touched_at] <- [[
                $run_id, $object_kind, $object_id, $input_class, $function_name, $role, $touched_at
            ]]
            :put invocation_touch_edges {run_id, object_kind, object_id => input_class, function_name, role, touched_at}"#,
            params,
        )?;
        Ok(())
    }

    /// Causal invalidation: when `object_id` changes (a claim superseded,
    /// contradicted, or removed), decay the learned weight of every
    /// `(input_class, function)` whose runs touched it — the backend
    /// "un-knows" advice grounded on a fact that's no longer true. Returns
    /// how many experience edges were decayed.
    pub fn invalidate_experience_for_object(
        &self,
        object_kind: &str,
        object_id: &str,
    ) -> Result<usize> {
        let mut params = BTreeMap::new();
        params.insert("ok".into(), DataValue::Str(object_kind.into()));
        params.insert("oid".into(), DataValue::Str(object_id.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[input_class, function_name] := \
                 *invocation_touch_edges{object_kind: $ok, object_id: $oid, input_class, function_name}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("invalidate lookup: {e}")))?;
        // Distinct (input_class, function_name) pairs.
        let mut seen = std::collections::BTreeSet::new();
        let mut decayed = 0usize;
        for r in &rows.rows {
            let ic = dv_str(&r[0]);
            let fnn = dv_str(&r[1]);
            if !seen.insert((ic.clone(), fnn.clone())) {
                continue;
            }
            if let Some(e) = self.get_experience(&ic, &fnn)? {
                // Decay BOTH the raw weight and the success evidence so the
                // Wilson score (which ranking uses) actually drops when the
                // basis changed — a stale success is less trustworthy.
                self.put_experience(&ic, &fnn, e.weight * 0.5, e.n_success / 2, e.n_fail)?;
                decayed += 1;
            }
        }
        Ok(decayed)
    }

    // ─── Function test fixtures (verify-before-merge) ────────────────

    /// Store (idempotent by `(function_name, fixture_id)`) a test fixture.
    pub fn put_function_fixture(&self, fx: &FunctionFixture) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let mut params = BTreeMap::new();
        params.insert("function_name".into(), DataValue::Str(fx.function_name.clone().into()));
        params.insert("fixture_id".into(), DataValue::Str(fx.fixture_id.clone().into()));
        params.insert("input_json".into(), DataValue::Str(fx.input_json.clone().into()));
        params.insert("expect_json".into(), DataValue::Str(fx.expect_json.clone().into()));
        params.insert("created_at".into(), DataValue::Num(Num::Float(now)));
        self.query(
            r#"?[function_name, fixture_id, input_json, expect_json, created_at] <- [[
                $function_name, $fixture_id, $input_json, $expect_json, $created_at
            ]]
            :put function_test_fixtures {function_name, fixture_id => input_json, expect_json, created_at}"#,
            params,
        )?;
        Ok(())
    }

    /// All fixtures for a function.
    pub fn list_function_fixtures(&self, function_name: &str) -> Result<Vec<FunctionFixture>> {
        let mut params = BTreeMap::new();
        params.insert("fnn".into(), DataValue::Str(function_name.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[fixture_id, input_json, expect_json] := \
                 *function_test_fixtures{function_name: $fnn, fixture_id, input_json, expect_json}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("list_function_fixtures: {e}")))?;
        Ok(rows
            .rows
            .iter()
            .map(|r| FunctionFixture {
                function_name: function_name.to_string(),
                fixture_id: dv_str(&r[0]),
                input_json: dv_str(&r[1]),
                expect_json: dv_str(&r[2]),
            })
            .collect())
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

    #[test]
    fn function_fixtures_round_trip() {
        let s = store();
        s.put_function_fixture(&FunctionFixture {
            function_name: "double".into(),
            fixture_id: "fx1".into(),
            input_json: r#"{"n":2}"#.into(),
            expect_json: "4".into(),
        })
        .unwrap();
        s.put_function_fixture(&FunctionFixture {
            function_name: "double".into(),
            fixture_id: "fx2".into(),
            input_json: r#"{"n":10}"#.into(),
            expect_json: "20".into(),
        })
        .unwrap();
        let fxs = s.list_function_fixtures("double").unwrap();
        assert_eq!(fxs.len(), 2);
        assert!(s.list_function_fixtures("other").unwrap().is_empty());
    }

    #[test]
    fn experience_learns_ranks_and_causally_invalidates() {
        let s = store();
        // Two functions compete on the "refund" input class; classifier wins.
        for _ in 0..3 {
            s.bump_function_experience("refund", "classifyRefund", true).unwrap();
        }
        s.bump_function_experience("refund", "genericReply", true).unwrap();

        // Retrieval ranks the more-proven function first.
        let ranked = s.retrieve_experience("refund").unwrap();
        assert_eq!(ranked[0].function_name, "classifyRefund");
        assert_eq!(ranked[0].n_success, 3);
        let before = ranked[0].score();
        assert!(before > 0.0);

        // A run of classifyRefund touched a policy claim.
        s.record_invocation_touch(
            "run1", "claim", "claim:refund-policy", "refund", "classifyRefund", "read",
        )
        .unwrap();

        // That claim changes ⇒ experience grounded on it is causally decayed.
        let decayed = s
            .invalidate_experience_for_object("claim", "claim:refund-policy")
            .unwrap();
        assert_eq!(decayed, 1);
        let after = s
            .retrieve_experience("refund")
            .unwrap()
            .into_iter()
            .find(|e| e.function_name == "classifyRefund")
            .unwrap()
            .score();
        assert!(after < before, "score must decay after the basis changed: {after} < {before}");

        // A claim nothing touched ⇒ no-op.
        assert_eq!(
            s.invalidate_experience_for_object("claim", "claim:unrelated").unwrap(),
            0
        );
    }

    #[test]
    fn experience_score_rewards_confident_success_rate() {
        let mk = |ns: i64, nf: i64| ExperienceEntry {
            function_name: "f".into(),
            weight: 0.0,
            n_success: ns,
            n_fail: nf,
        };
        // 50/50 (100%, lots of evidence) > 9/10 (90%) > 1/1 (100% but tiny sample).
        assert!(mk(50, 0).score() > mk(9, 1).score());
        assert!(mk(9, 1).score() > mk(1, 0).score());
        // No data ranks at zero.
        assert_eq!(mk(0, 0).score(), 0.0);
        // More failures lowers the score.
        assert!(mk(8, 2).score() < mk(10, 0).score());
    }

    #[test]
    fn contradiction_and_removal_cascades_decay_experience() {
        let s = store();
        s.bump_function_experience("c", "fnX", true).unwrap();
        s.bump_function_experience("c", "fnX", true).unwrap();
        s.record_invocation_touch("r1", "claim", "claim:z", "c", "fnX", "read").unwrap();
        let before = s.retrieve_experience("c").unwrap()[0].score();

        // A contradiction involving the touched claim decays grounded experience.
        s.insert_contradiction("ctr1", "claim:z", "claim:other", "conflict").unwrap();
        let after_contra = s.retrieve_experience("c").unwrap()[0].score();
        assert!(after_contra < before, "contradiction must decay: {after_contra} < {before}");

        // Fully removing the claim decays it again (same mechanism).
        s.remove_claim_fully("claim:z").unwrap();
        let after_removal = s.retrieve_experience("c").unwrap()[0].score();
        assert!(after_removal <= after_contra, "removal must not increase score");
    }
}
