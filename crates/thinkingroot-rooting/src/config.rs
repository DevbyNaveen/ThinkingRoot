//! Configuration knobs for the Rooting gate.

use serde::{Deserialize, Serialize};

/// Runtime configuration for the Rooting gate. Loaded from the workspace
/// config file (`.thinkingroot/config.toml`) under `[rooting]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootingConfig {
    /// Master off-switch. When `true`, Phase 6.5 is skipped entirely and all
    /// claims pass through tagged `Attested`.
    #[serde(default)]
    pub disabled: bool,

    /// Minimum fraction of claim tokens that must appear in the source span
    /// for the provenance probe to pass. Default: `0.70`.
    #[serde(default = "default_provenance_threshold")]
    pub provenance_threshold: f64,

    /// Confidence floor for the contradiction probe. A contradicting claim
    /// below this confidence is ignored. Default: `0.85`.
    #[serde(default = "default_contradiction_floor")]
    pub contradiction_floor: f64,

    /// How the `contribute` MCP tool handles Rejected-tier claims:
    /// - `"advisory"` — log only, persist anyway (default, safe)
    /// - `"enforce"` — drop Rejected claims
    /// - `"off"` — skip Rooting entirely for agent writes
    #[serde(default = "default_contribute_gate")]
    pub contribute_gate: String,
}

impl Default for RootingConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            provenance_threshold: default_provenance_threshold(),
            contradiction_floor: default_contradiction_floor(),
            contribute_gate: default_contribute_gate(),
        }
    }
}

fn default_provenance_threshold() -> f64 {
    0.70
}

fn default_contradiction_floor() -> f64 {
    0.85
}

fn default_contribute_gate() -> String {
    "advisory".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_enabled_with_sane_thresholds() {
        let cfg = RootingConfig::default();
        assert!(!cfg.disabled);
        assert!((cfg.provenance_threshold - 0.70).abs() < f64::EPSILON);
        assert!((cfg.contradiction_floor - 0.85).abs() < f64::EPSILON);
        assert_eq!(cfg.contribute_gate, "advisory");
    }

    #[test]
    fn config_deserializes_defaults_from_empty_json() {
        // An empty JSON object should yield a fully-default config thanks to
        // the `#[serde(default)]` on each field.
        let cfg: RootingConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.contribute_gate, "advisory");
        assert!(!cfg.disabled);
    }
}
