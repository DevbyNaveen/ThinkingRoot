//! Compiled Prompt substrate — versioned prompt templates stored in
//! CozoDB plus a deterministic `{{var}}` assembler.
//!
//! The selling point for buyers is cache-stable prompt bytes: a given
//! `(name, version, vars)` always assembles to the *same* string, so
//! upstream prompt-caching (Anthropic/OpenAI) stays warm. We guarantee
//! that two ways:
//!   1. `variables_json` is derived from the template body at write
//!      time, so the declared variable set can never drift from the
//!      `{{...}}` references actually present.
//!   2. [`substitute`] errors on an unknown `{{var}}` rather than
//!      emitting an empty string — a missing variable is a caller bug,
//!      not a silently-degraded prompt.
//!
//! Versioning is append-only: every [`GraphStore::prompt_put_template`]
//! writes a NEW row with `version = max(version)+1`, so older versions
//! remain readable for diff/rollback (the Console "Prompts" tab shows
//! version history).

use std::collections::{BTreeMap, BTreeSet};

use cozo::{DataValue, Num, ScriptMutability};
use serde::{Deserialize, Serialize};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// One stored template version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptTemplate {
    /// Storage id: `"{name}@{version}"`.
    pub id: String,
    pub name: String,
    pub template_text: String,
    /// The `{{var}}` names the body references, sorted + deduped.
    pub variables: Vec<String>,
    pub version: i64,
    pub created_at: f64,
}

/// Extract the distinct `{{var}}` names a template references, sorted
/// for determinism. Whitespace inside the braces is tolerated
/// (`{{ name }}` == `{{name}}`).
pub fn extract_vars(template: &str) -> Vec<String> {
    let mut set = BTreeSet::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(close_rel) = template[i + 2..].find("}}") {
                let name = template[i + 2..i + 2 + close_rel].trim();
                if !name.is_empty() {
                    set.insert(name.to_string());
                }
                i += 2 + close_rel + 2;
                continue;
            }
        }
        i += 1;
    }
    set.into_iter().collect()
}

/// Substitute every `{{var}}` in `template` with the value from `vars`.
/// Returns `Error::Template` if the body references a variable not
/// present in `vars`, or contains an unterminated `{{`.
pub fn substitute(template: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let Some(close_rel) = template[i + 2..].find("}}") else {
                return Err(Error::Template(format!(
                    "unterminated {{{{ at byte {i} in prompt template"
                )));
            };
            let name = template[i + 2..i + 2 + close_rel].trim();
            let value = vars.get(name).ok_or_else(|| {
                Error::Template(format!(
                    "prompt references undefined variable `{{{{{name}}}}}` — \
                     supply it in the assemble call (provided: {:?})",
                    vars.keys().collect::<Vec<_>>()
                ))
            })?;
            out.push_str(value);
            i += 2 + close_rel + 2;
        } else {
            // Step one UTF-8 char so multi-byte chars copy intact.
            let ch = template[i..].chars().next().expect("non-empty rest");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

// ── row parsing helpers (column order: id, name, template_text,
//    variables_json, version, created_at) ──────────────────────────────────

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

fn row_to_template(row: &[DataValue]) -> PromptTemplate {
    let variables_json = dv_str(&row[3]);
    let variables = serde_json::from_str::<Vec<String>>(&variables_json).unwrap_or_default();
    PromptTemplate {
        id: dv_str(&row[0]),
        name: dv_str(&row[1]),
        template_text: dv_str(&row[2]),
        variables,
        version: dv_i64(&row[4]),
        created_at: dv_f64(&row[5]),
    }
}

const SELECT_COLS: &str =
    "?[id, name, template_text, variables_json, version, created_at] := \
     *prompt_templates{id, name, template_text, variables_json, version, created_at}";

impl GraphStore {
    /// Write a new template version. The new `version` is
    /// `max(existing)+1` (or 1 for a first write), so history is
    /// preserved. `variables` are derived from the body. Returns the
    /// stored row.
    pub fn prompt_put_template(&self, name: &str, template_text: &str) -> Result<PromptTemplate> {
        if name.trim().is_empty() {
            return Err(Error::Template("prompt template name must be non-empty".into()));
        }
        let version = self.prompt_latest_version(name)?.unwrap_or(0) + 1;
        let id = format!("{name}@{version}");
        let variables = extract_vars(template_text);
        let variables_json = serde_json::to_string(&variables)
            .map_err(|e| Error::Serialization(format!("variables_json: {e}")))?;
        let created_at = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;

        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.clone().into()));
        params.insert("name".into(), DataValue::Str(name.into()));
        params.insert("template_text".into(), DataValue::Str(template_text.into()));
        params.insert("variables_json".into(), DataValue::Str(variables_json.into()));
        params.insert("version".into(), DataValue::Num(Num::Int(version)));
        params.insert("created_at".into(), DataValue::Num(Num::Float(created_at)));

        self.query(
            r#"?[id, name, template_text, variables_json, version, created_at] <- [[
                $id, $name, $template_text, $variables_json, $version, $created_at
            ]]
            :put prompt_templates {id => name, template_text, variables_json, version, created_at}"#,
            params,
        )?;

        Ok(PromptTemplate {
            id,
            name: name.to_string(),
            template_text: template_text.to_string(),
            variables,
            version,
            created_at,
        })
    }

    /// Highest stored version for `name`, or `None` if the template
    /// doesn't exist yet.
    pub fn prompt_latest_version(&self, name: &str) -> Result<Option<i64>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[version] := *prompt_templates{name: $name, version}",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("prompt_latest_version: {e}")))?;
        Ok(rows.rows.iter().map(|r| dv_i64(&r[0])).max())
    }

    /// Latest version of a single template, or `None`.
    pub fn prompt_get_latest(&self, name: &str) -> Result<Option<PromptTemplate>> {
        match self.prompt_latest_version(name)? {
            Some(v) => self.prompt_get_version(name, v),
            None => Ok(None),
        }
    }

    /// A specific `(name, version)`, or `None`.
    pub fn prompt_get_version(&self, name: &str, version: i64) -> Result<Option<PromptTemplate>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        params.insert("version".into(), DataValue::Num(Num::Int(version)));
        let rows = self
            .raw_db()
            .run_script(
                "?[id, name, template_text, variables_json, version, created_at] := \
                 *prompt_templates{id, name, template_text, variables_json, version, created_at}, \
                 name = $name, version = $version",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("prompt_get_version: {e}")))?;
        Ok(rows.rows.first().map(|r| row_to_template(r)))
    }

    /// The latest version of every distinct template, sorted by name.
    pub fn prompt_list_latest(&self) -> Result<Vec<PromptTemplate>> {
        let rows = self
            .query_read(SELECT_COLS)?
            .rows
            .iter()
            .map(|r| row_to_template(r))
            .collect::<Vec<_>>();
        // Group by name, keep the highest version of each.
        let mut latest: BTreeMap<String, PromptTemplate> = BTreeMap::new();
        for t in rows {
            match latest.get(&t.name) {
                Some(existing) if existing.version >= t.version => {}
                _ => {
                    latest.insert(t.name.clone(), t);
                }
            }
        }
        Ok(latest.into_values().collect())
    }

    /// Every stored version of `name`, ascending by version.
    pub fn prompt_list_versions(&self, name: &str) -> Result<Vec<PromptTemplate>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[id, name, template_text, variables_json, version, created_at] := \
                 *prompt_templates{id, name, template_text, variables_json, version, created_at}, \
                 name = $name",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("prompt_list_versions: {e}")))?;
        let mut out: Vec<PromptTemplate> = rows.rows.iter().map(|r| row_to_template(r)).collect();
        out.sort_by_key(|t| t.version);
        Ok(out)
    }

    /// Assemble the latest version of `name` with `vars`. Errors if the
    /// template is absent or references an undefined variable.
    pub fn assemble_prompt(&self, name: &str, vars: &BTreeMap<String, String>) -> Result<String> {
        let template = self
            .prompt_get_latest(name)?
            .ok_or_else(|| Error::Template(format!("prompt template `{name}` not found")))?;
        substitute(&template.template_text, vars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn extract_vars_dedupes_and_sorts() {
        let v = extract_vars("Hello {{name}}, your {{ role }} on {{name}}.");
        assert_eq!(v, vec!["name".to_string(), "role".to_string()]);
    }

    #[test]
    fn substitute_replaces_and_is_deterministic() {
        let t = "Hi {{name}} — tier {{ tier }}.";
        let out = substitute(t, &vars(&[("name", "Ada"), ("tier", "pro")])).unwrap();
        assert_eq!(out, "Hi Ada — tier pro.");
        // Same inputs → identical bytes (cache-stability contract).
        let out2 = substitute(t, &vars(&[("tier", "pro"), ("name", "Ada")])).unwrap();
        assert_eq!(out, out2);
    }

    #[test]
    fn substitute_errors_on_unknown_var() {
        let err = substitute("Hi {{missing}}", &vars(&[("name", "x")])).unwrap_err();
        assert!(matches!(err, Error::Template(_)));
    }

    #[test]
    fn substitute_errors_on_unterminated() {
        let err = substitute("Hi {{oops", &vars(&[])).unwrap_err();
        assert!(matches!(err, Error::Template(_)));
    }

    #[test]
    fn versioning_round_trips_through_cozo() {
        let db = cozo::DbInstance::new("mem", "", "").unwrap();
        let store = GraphStore::from_db_for_testing(db);
        store.init_for_testing().unwrap();
        // First write → v1.
        let t1 = store.prompt_put_template("greeting", "Hello {{name}}").unwrap();
        assert_eq!(t1.version, 1);
        assert_eq!(t1.variables, vec!["name".to_string()]);
        // Second write → v2, history preserved.
        let t2 = store.prompt_put_template("greeting", "Hi {{name}} ({{tier}})").unwrap();
        assert_eq!(t2.version, 2);

        assert_eq!(store.prompt_latest_version("greeting").unwrap(), Some(2));
        let latest = store.prompt_get_latest("greeting").unwrap().unwrap();
        assert_eq!(latest.version, 2);
        assert_eq!(store.prompt_list_versions("greeting").unwrap().len(), 2);

        // assemble uses the latest version.
        let out = store
            .assemble_prompt("greeting", &vars(&[("name", "Ada"), ("tier", "pro")]))
            .unwrap();
        assert_eq!(out, "Hi Ada (pro)");

        // Unknown template → error.
        assert!(store.assemble_prompt("nope", &vars(&[])).is_err());
        // list_latest returns one row (latest) for the single name.
        let all = store.prompt_list_latest().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].version, 2);
    }
}
