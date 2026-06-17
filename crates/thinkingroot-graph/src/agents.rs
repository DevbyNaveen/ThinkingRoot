//! Agent definitions — the persisted, create-once agent entity (persona +
//! model + memory policy) that the SDK and Console both read/write so the two
//! stay in sync. Stored in the project's shared brain and resolvable from any
//! per-user scope via the engine's primary-workspace fallback (mirrors Root
//! Functions). Un-versioned for v1: `put_agent` upserts the latest config.
//!
//! An agent is "a persona over a brain": the definition lives here; at invoke
//! time it runs against whichever brain scope the request carries (`main`, a
//! sub-topic brain, or a per-user `u_*`), so one definition serves every user.

use std::collections::BTreeMap;

use cozo::{DataValue, Num, ScriptMutability};
use serde::{Deserialize, Serialize};
use thinkingroot_core::{Error, Result};

use crate::graph::GraphStore;

/// One stored agent definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentDef {
    pub name: String,
    /// System prompt / persona.
    pub persona: String,
    /// Provider/model id (`""` = use the workspace's default LLM).
    pub model: String,
    /// JSON policy bag: `{ recall_k, remember, two_tier, tools, ... }`.
    pub config_json: String,
    pub created_at: f64,
    pub updated_at: f64,
}

fn dv_str(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => other.to_string(),
    }
}

fn dv_f64(v: &DataValue) -> f64 {
    match v {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

fn row_to_agent(r: &[DataValue]) -> AgentDef {
    AgentDef {
        name: dv_str(&r[0]),
        persona: dv_str(&r[1]),
        model: dv_str(&r[2]),
        config_json: dv_str(&r[3]),
        created_at: dv_f64(&r[4]),
        updated_at: dv_f64(&r[5]),
    }
}

impl GraphStore {
    /// Create or update an agent definition (upsert by name). Preserves
    /// `created_at` on update; always bumps `updated_at`.
    pub fn put_agent(
        &self,
        name: &str,
        persona: &str,
        model: &str,
        config_json: &str,
    ) -> Result<AgentDef> {
        if name.trim().is_empty() {
            return Err(Error::Template("agent name must be non-empty".into()));
        }
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let created_at = self.get_agent(name)?.map(|a| a.created_at).unwrap_or(now);
        let cfg = if config_json.trim().is_empty() {
            "{}"
        } else {
            config_json
        };

        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        params.insert("persona".into(), DataValue::Str(persona.into()));
        params.insert("model".into(), DataValue::Str(model.into()));
        params.insert("config_json".into(), DataValue::Str(cfg.into()));
        params.insert("created_at".into(), DataValue::Num(Num::Float(created_at)));
        params.insert("updated_at".into(), DataValue::Num(Num::Float(now)));
        self.query(
            r#"?[name, persona, model, config_json, created_at, updated_at] <- [[
                $name, $persona, $model, $config_json, $created_at, $updated_at
            ]]
            :put agents {name => persona, model, config_json, created_at, updated_at}"#,
            params,
        )?;
        Ok(AgentDef {
            name: name.to_string(),
            persona: persona.to_string(),
            model: model.to_string(),
            config_json: cfg.to_string(),
            created_at,
            updated_at: now,
        })
    }

    /// Fetch one agent by name.
    pub fn get_agent(&self, name: &str) -> Result<Option<AgentDef>> {
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        let rows = self
            .raw_db()
            .run_script(
                "?[name, persona, model, config_json, created_at, updated_at] := \
                 *agents{name, persona, model, config_json, created_at, updated_at}, \
                 name = $name",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| Error::GraphStorage(format!("get_agent: {e}")))?;
        Ok(rows.rows.first().map(|r| row_to_agent(r)))
    }

    /// List all agent definitions, sorted by name.
    pub fn list_agents(&self) -> Result<Vec<AgentDef>> {
        let mut out = self
            .query_read(
                "?[name, persona, model, config_json, created_at, updated_at] := \
                 *agents{name, persona, model, config_json, created_at, updated_at}",
            )?
            .rows
            .iter()
            .map(|r| row_to_agent(r))
            .collect::<Vec<_>>();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Delete an agent by name. Returns true if a row was removed.
    pub fn delete_agent(&self, name: &str) -> Result<bool> {
        if self.get_agent(name)?.is_none() {
            return Ok(false);
        }
        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        self.query(
            "?[name] := *agents{name}, name = $name\n:rm agents {name}",
            params,
        )?;
        Ok(true)
    }
}
