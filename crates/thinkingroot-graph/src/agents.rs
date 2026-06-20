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

/// The per-agent guardrails the Console persists under
/// `config_json.guardrails` (see `apps/console/lib/agentPolicy.ts`
/// `AgentGuardrails`). Every field is `#[serde(default)]` so an agent with
/// NO guardrails block (every agent created before this feature) parses to the
/// zero value — which the enforcement sites treat as "off / legacy behavior",
/// preserving zero regression.
///
/// The JSON keys are the EXACT snake_case names the Console writes:
/// `grounded_only`, `abstain_below_confidence`, `block_pii_in_remember`,
/// `blocked_topics`, `tool_allowlist_enabled`, `allowed_tools`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentGuardrails {
    /// Refuse when recall has no supporting claims. (Engine-native; the agent
    /// path leaves grounding to the existing answer pipeline.)
    #[serde(default)]
    pub grounded_only: bool,
    /// Stay silent on low-confidence evidence.
    #[serde(default)]
    pub abstain_below_confidence: bool,
    /// Strip detected PII (emails, SSNs, tokens, …) before persisting a
    /// remembered claim. Reuses the extractor's sensitivity pattern catalog.
    #[serde(default)]
    pub block_pii_in_remember: bool,
    /// Topics the agent must decline. Case-insensitive substring match on the
    /// user input. Empty = no-op.
    #[serde(default)]
    pub blocked_topics: Vec<String>,
    /// When true, restrict the agent's tool catalog to `allowed_tools`
    /// (intersected with the built catalog; READ tools always kept).
    #[serde(default)]
    pub tool_allowlist_enabled: bool,
    /// Tool names permitted when `tool_allowlist_enabled` is true.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

impl AgentGuardrails {
    /// True when the user input matches any blocked topic (case-insensitive
    /// substring). Empty topic strings are ignored. Empty list → never blocks.
    pub fn blocks_question(&self, question: &str) -> Option<String> {
        if self.blocked_topics.is_empty() {
            return None;
        }
        let haystack = question.to_lowercase();
        self.blocked_topics
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .find(|t| haystack.contains(&t.to_lowercase()))
            .map(|t| t.to_string())
    }
}

impl AgentDef {
    /// Parse this agent's `config_json.guardrails` into the typed view.
    /// A missing / malformed `config_json` or absent `guardrails` block
    /// yields `AgentGuardrails::default()` (all-off → legacy behavior).
    pub fn guardrails(&self) -> AgentGuardrails {
        serde_json::from_str::<serde_json::Value>(&self.config_json)
            .ok()
            .and_then(|v| v.get("guardrails").cloned())
            .and_then(|g| serde_json::from_value::<AgentGuardrails>(g).ok())
            .unwrap_or_default()
    }
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

#[cfg(test)]
mod guardrail_tests {
    use super::*;

    fn agent_with_config(config_json: &str) -> AgentDef {
        AgentDef {
            name: "a".into(),
            persona: "p".into(),
            model: "".into(),
            config_json: config_json.into(),
            created_at: 0.0,
            updated_at: 0.0,
        }
    }

    #[test]
    fn absent_guardrails_parse_to_default_all_off() {
        // Every legacy agent (no guardrails block) → all-off, preserving
        // exactly today's behavior. Covers empty, "{}", and bag-without-guardrails.
        for cfg in ["", "{}", r#"{"recall_k":5,"remember":true}"#] {
            let g = agent_with_config(cfg).guardrails();
            assert_eq!(g, AgentGuardrails::default(), "cfg={cfg:?}");
            assert!(!g.grounded_only);
            assert!(!g.tool_allowlist_enabled);
            assert!(g.allowed_tools.is_empty());
            assert!(g.blocked_topics.is_empty());
            assert!(!g.block_pii_in_remember);
            // A default-config agent never blocks any question.
            assert!(g.blocks_question("anything at all").is_none());
        }
    }

    #[test]
    fn malformed_config_falls_back_to_default() {
        let g = agent_with_config("{not json").guardrails();
        assert_eq!(g, AgentGuardrails::default());
    }

    #[test]
    fn parses_exact_console_guardrail_keys() {
        // The EXACT shape `apps/console/lib/agentPolicy.ts` writes.
        let cfg = r#"{
            "recall_k": 5,
            "guardrails": {
                "grounded_only": true,
                "abstain_below_confidence": true,
                "block_pii_in_remember": true,
                "blocked_topics": ["legal advice", "Medical Diagnosis"],
                "tool_allowlist_enabled": true,
                "allowed_tools": ["github::create_issue", "slack::post_message"]
            }
        }"#;
        let g = agent_with_config(cfg).guardrails();
        assert!(g.grounded_only);
        assert!(g.abstain_below_confidence);
        assert!(g.block_pii_in_remember);
        assert_eq!(g.blocked_topics, vec!["legal advice", "Medical Diagnosis"]);
        assert!(g.tool_allowlist_enabled);
        assert_eq!(
            g.allowed_tools,
            vec!["github::create_issue", "slack::post_message"]
        );
    }

    #[test]
    fn partial_guardrails_block_uses_field_defaults() {
        // A guardrails block that omits most fields → omitted = off (serde default).
        let g = agent_with_config(r#"{"guardrails":{"grounded_only":true}}"#).guardrails();
        assert!(g.grounded_only);
        assert!(!g.tool_allowlist_enabled);
        assert!(g.blocked_topics.is_empty());
    }

    #[test]
    fn blocked_topic_query_is_caught_case_insensitively() {
        let g = agent_with_config(
            r#"{"guardrails":{"blocked_topics":["Medical Diagnosis","crypto"]}}"#,
        )
        .guardrails();
        // Case-insensitive substring match.
        assert_eq!(
            g.blocks_question("Can you give me a medical diagnosis?"),
            Some("Medical Diagnosis".to_string())
        );
        assert_eq!(
            g.blocks_question("thoughts on CRYPTO investing"),
            Some("crypto".to_string())
        );
        // A normal question passes through unblocked.
        assert!(g.blocks_question("what is the capital of France?").is_none());
    }

    #[test]
    fn empty_blocked_topics_never_blocks() {
        let g = agent_with_config(r#"{"guardrails":{"blocked_topics":[]}}"#).guardrails();
        assert!(g.blocks_question("medical diagnosis crypto legal").is_none());
        // Whitespace-only topic entries are ignored, never matching everything.
        let g2 = agent_with_config(r#"{"guardrails":{"blocked_topics":["   "]}}"#).guardrails();
        assert!(g2.blocks_question("anything").is_none());
    }
}
