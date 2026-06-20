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
///
/// `created_by` / `parent_agent` are PROVENANCE: who (a human handle, a
/// keyword like `"user"`, or an agent name) created this agent, and — when an
/// agent created it — which agent. They are NOT Cozo columns: they live inside
/// `config_json.provenance.{created_by,parent_agent}` (zero schema change), and
/// are projected onto these fields by [`row_to_agent`] on read and folded back
/// into `config_json` by `put_agent` on write. A legacy agent with no
/// provenance block reads back as `None`/`None`, preserving exactly today's
/// behavior. They `#[serde(skip_serializing_if = "Option::is_none")]` so the
/// wire shape is unchanged when absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentDef {
    pub name: String,
    /// System prompt / persona.
    pub persona: String,
    /// Provider/model id (`""` = use the workspace's default LLM).
    pub model: String,
    /// JSON policy bag: `{ recall_k, remember, two_tier, tools, provenance, ... }`.
    pub config_json: String,
    pub created_at: f64,
    pub updated_at: f64,
    /// Who created this agent (a human handle / `"user"` / an agent name).
    /// Stored in `config_json.provenance.created_by`. `None` = legacy/unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// When an AGENT created this agent, the creating agent's name. Stored in
    /// `config_json.provenance.parent_agent`. `None` for human-created agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent: Option<String>,
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

    /// Who created this agent — read from `config_json.provenance.created_by`.
    /// `None` for a legacy agent (no provenance block) or a malformed config.
    pub fn created_by(&self) -> Option<String> {
        provenance_field(&self.config_json, "created_by")
    }

    /// The creating AGENT's name, when an agent created this one — read from
    /// `config_json.provenance.parent_agent`. `None` for human-created agents.
    pub fn parent_agent(&self) -> Option<String> {
        provenance_field(&self.config_json, "parent_agent")
    }
}

/// Read a single string field from `config_json.provenance.<field>`. A missing
/// config, missing `provenance` block, missing/non-string field, or empty
/// string all yield `None` (honest "unknown", never a fabricated value).
fn provenance_field(config_json: &str, field: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(config_json)
        .ok()
        .and_then(|v| v.get("provenance").and_then(|p| p.get(field)).cloned())
        .and_then(|f| f.as_str().map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
}

/// Merge the supplied provenance into `config_json`, preserving any existing
/// `provenance.<field>` when the corresponding argument is `None` (mirrors how
/// `created_at` is preserved on upsert). A non-object / unparsable `config_json`
/// is treated as an empty object so the write never fails on legacy data.
fn merge_provenance(
    config_json: &str,
    created_by: Option<&str>,
    parent_agent: Option<&str>,
) -> String {
    let mut root = serde_json::from_str::<serde_json::Value>(config_json)
        .ok()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    // Nothing to set and no existing block → leave config untouched.
    let existing = root.get("provenance").cloned();
    if created_by.is_none() && parent_agent.is_none() && existing.is_none() {
        return root.to_string();
    }
    let mut prov = existing
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(cb) = created_by.filter(|s| !s.is_empty()) {
        prov["created_by"] = serde_json::Value::String(cb.to_string());
    }
    if let Some(pa) = parent_agent.filter(|s| !s.is_empty()) {
        prov["parent_agent"] = serde_json::Value::String(pa.to_string());
    }
    root["provenance"] = prov;
    root.to_string()
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
    let config_json = dv_str(&r[3]);
    // Project the logical provenance fields out of config_json so callers
    // (REST/SDK/runtime) see them on the struct without re-parsing.
    let created_by = provenance_field(&config_json, "created_by");
    let parent_agent = provenance_field(&config_json, "parent_agent");
    AgentDef {
        name: dv_str(&r[0]),
        persona: dv_str(&r[1]),
        model: dv_str(&r[2]),
        config_json,
        created_at: dv_f64(&r[4]),
        updated_at: dv_f64(&r[5]),
        created_by,
        parent_agent,
    }
}

impl GraphStore {
    /// Create or update an agent definition (upsert by name). Preserves
    /// `created_at` AND existing `config_json.provenance` on update; always
    /// bumps `updated_at`. (Provenance carried inside the supplied `config_json`
    /// is honored; absent, the stored provenance is preserved — mirroring
    /// `created_at`.)
    pub fn put_agent(
        &self,
        name: &str,
        persona: &str,
        model: &str,
        config_json: &str,
    ) -> Result<AgentDef> {
        self.put_agent_with_provenance(name, persona, model, config_json, None, None)
    }

    /// Upsert an agent, explicitly stamping provenance. `created_by` /
    /// `parent_agent` are merged into `config_json.provenance`; either left
    /// `None` is PRESERVED from the existing record on update (and stays absent
    /// on create), so a plain re-`put` never erases who created the agent.
    pub fn put_agent_with_provenance(
        &self,
        name: &str,
        persona: &str,
        model: &str,
        config_json: &str,
        created_by: Option<&str>,
        parent_agent: Option<&str>,
    ) -> Result<AgentDef> {
        if name.trim().is_empty() {
            return Err(Error::Template("agent name must be non-empty".into()));
        }
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        // Existing record: preserve created_at AND any provenance not being
        // overwritten (carry the old created_by/parent_agent forward).
        let existing = self.get_agent(name)?;
        let created_at = existing.as_ref().map(|a| a.created_at).unwrap_or(now);
        let prior_created_by = existing.as_ref().and_then(|a| a.created_by());
        let prior_parent = existing.as_ref().and_then(|a| a.parent_agent());

        let base = if config_json.trim().is_empty() {
            "{}"
        } else {
            config_json
        };
        // Resolve final provenance: explicit arg → else value carried inside the
        // supplied config_json → else the stored value (preserve on update).
        let cfg_created_by = provenance_field(base, "created_by");
        let cfg_parent = provenance_field(base, "parent_agent");
        let final_created_by = created_by
            .map(|s| s.to_string())
            .or(cfg_created_by)
            .or(prior_created_by);
        let final_parent = parent_agent
            .map(|s| s.to_string())
            .or(cfg_parent)
            .or(prior_parent);
        let cfg = merge_provenance(base, final_created_by.as_deref(), final_parent.as_deref());

        let mut params = BTreeMap::new();
        params.insert("name".into(), DataValue::Str(name.into()));
        params.insert("persona".into(), DataValue::Str(persona.into()));
        params.insert("model".into(), DataValue::Str(model.into()));
        params.insert("config_json".into(), DataValue::Str(cfg.clone().into()));
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
            created_by: provenance_field(&cfg, "created_by"),
            parent_agent: provenance_field(&cfg, "parent_agent"),
            config_json: cfg,
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
            created_by: provenance_field(config_json, "created_by"),
            parent_agent: provenance_field(config_json, "parent_agent"),
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

#[cfg(test)]
mod provenance_tests {
    use super::*;

    fn store() -> GraphStore {
        let db = cozo::DbInstance::new("mem", "", "").unwrap();
        let s = GraphStore::from_db_for_testing(db);
        s.init_for_testing().unwrap();
        s
    }

    #[test]
    fn create_with_provenance_round_trips() {
        let s = store();
        let def = s
            .put_agent_with_provenance(
                "researcher",
                "careful",
                "",
                r#"{"recall_k":5}"#,
                Some("mrguy"),
                Some("mrguy"),
            )
            .unwrap();
        assert_eq!(def.created_by.as_deref(), Some("mrguy"));
        assert_eq!(def.parent_agent.as_deref(), Some("mrguy"));
        // The accessors read the same value back out of config_json.
        assert_eq!(def.created_by(), Some("mrguy".to_string()));
        assert_eq!(def.parent_agent(), Some("mrguy".to_string()));
        // Non-provenance config is preserved alongside the provenance block.
        let v: serde_json::Value = serde_json::from_str(&def.config_json).unwrap();
        assert_eq!(v["recall_k"], 5);
        assert_eq!(v["provenance"]["created_by"], "mrguy");

        // Re-read from storage → provenance projected onto the struct.
        let got = s.get_agent("researcher").unwrap().unwrap();
        assert_eq!(got.created_by.as_deref(), Some("mrguy"));
        assert_eq!(got.parent_agent.as_deref(), Some("mrguy"));
    }

    #[test]
    fn update_without_provenance_preserves_it() {
        let s = store();
        s.put_agent_with_provenance("a", "p1", "", "{}", Some("alice"), None)
            .unwrap();
        // A plain update (the legacy 4-arg path) supplies NO provenance.
        let updated = s.put_agent("a", "p2-changed", "gpt-4", r#"{"recall_k":9}"#).unwrap();
        assert_eq!(updated.persona, "p2-changed");
        assert_eq!(updated.model, "gpt-4");
        // created_by must survive the update untouched (like created_at).
        assert_eq!(updated.created_by.as_deref(), Some("alice"));
        assert_eq!(updated.parent_agent, None);
        // And the new non-provenance config landed.
        let v: serde_json::Value = serde_json::from_str(&updated.config_json).unwrap();
        assert_eq!(v["recall_k"], 9);
    }

    #[test]
    fn legacy_agent_without_provenance_reads_none() {
        let s = store();
        // The 4-arg put with no provenance anywhere = a legacy agent.
        let def = s.put_agent("legacy", "p", "", r#"{"remember":true}"#).unwrap();
        assert_eq!(def.created_by, None);
        assert_eq!(def.parent_agent, None);
        let got = s.get_agent("legacy").unwrap().unwrap();
        assert_eq!(got.created_by(), None);
        assert_eq!(got.parent_agent(), None);
    }

    #[test]
    fn provenance_supplied_inside_config_json_is_honored() {
        let s = store();
        // No explicit args, but the config_json carries a provenance block.
        let def = s
            .put_agent(
                "fromcfg",
                "p",
                "",
                r#"{"provenance":{"created_by":"console-user"}}"#,
            )
            .unwrap();
        assert_eq!(def.created_by.as_deref(), Some("console-user"));
        assert_eq!(def.parent_agent, None);
    }

    #[test]
    fn explicit_provenance_overrides_config_and_updates_preserve() {
        let s = store();
        // Explicit arg wins over a value already in config_json.
        let def = s
            .put_agent_with_provenance(
                "p",
                "x",
                "",
                r#"{"provenance":{"created_by":"old"}}"#,
                Some("new"),
                None,
            )
            .unwrap();
        assert_eq!(def.created_by.as_deref(), Some("new"));
        // A later update that omits provenance keeps "new".
        let again = s.put_agent("p", "x2", "", "{}").unwrap();
        assert_eq!(again.created_by.as_deref(), Some("new"));
    }
}
