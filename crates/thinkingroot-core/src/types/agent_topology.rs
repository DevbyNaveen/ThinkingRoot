//! Agent State Topology — the three-knob declarative state model
//! (`read_scope` / `write_target` / `merge_policy`) stored inside
//! `AgentDef.config_json`. Defaults reproduce pre-topology behavior exactly.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReadScope {
    Own,
    #[default]
    Inherit,
    InheritUsers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WriteTarget {
    #[default]
    Shared,
    Own,
    PerRun,
    PerUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMergePolicy {
    #[default]
    Auto,
    Verified,
    Manual,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentTopology {
    #[serde(default)]
    pub read_scope: ReadScope,
    #[serde(default)]
    pub write_target: WriteTarget,
    #[serde(default)]
    pub merge_policy: AgentMergePolicy,
}

impl AgentTopology {
    /// Parse from the agent's `config_json` policy bag. Unknown keys ignored;
    /// invalid JSON or absent fields fall back to defaults (legacy behavior).
    pub fn from_config_json(config_json: &str) -> Self {
        serde_json::from_str::<AgentTopology>(config_json).unwrap_or_default()
    }

    /// True when the run must execute in its own forked branch.
    pub fn isolates_run(&self) -> bool {
        matches!(self.write_target, WriteTarget::PerRun)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_reproduce_legacy_behavior() {
        let t = AgentTopology::default();
        assert_eq!(t.read_scope, ReadScope::Inherit);
        assert_eq!(t.write_target, WriteTarget::Shared);
        assert_eq!(t.merge_policy, AgentMergePolicy::Auto);
    }

    #[test]
    fn parses_from_config_json_bag_ignoring_unknown_keys() {
        let json = r#"{"tools":["recall"],"write_target":"per_run","merge_policy":"verified"}"#;
        let t = AgentTopology::from_config_json(json);
        assert_eq!(t.write_target, WriteTarget::PerRun);
        assert_eq!(t.merge_policy, AgentMergePolicy::Verified);
        assert_eq!(t.read_scope, ReadScope::Inherit); // absent → default
    }

    #[test]
    fn bad_json_falls_back_to_default() {
        let t = AgentTopology::from_config_json("not json");
        assert_eq!(t, AgentTopology::default());
    }
}
