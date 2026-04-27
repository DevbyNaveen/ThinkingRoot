//! Capability declaration block carried by a TR-1 manifest.
//!
//! Capabilities are **intent declarations** the publisher makes about
//! what the pack's dual-identity `.mcpb` payload requires at mount
//! time. A Claude Desktop host (or other MCP client) uses this block to
//! render a pre-install review modal — the user approves each
//! capability *before* the MCP server starts.
//!
//! The fields here are a direct mapping to the security-model spec's
//! capability declaration. Anything absent implies the default of
//! "not required" and MUST NOT be silently granted at mount time.

use serde::{Deserialize, Serialize};

/// Capability declaration. Every field defaults to `false`/empty, so a
/// pack that does not declare anything is assumed to need nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `true` if the pack's MCP bundle requires outbound network.
    #[serde(default)]
    pub network: bool,

    /// `true` if the bundle must read/write outside the pack sandbox.
    #[serde(default)]
    pub filesystem: bool,

    /// `true` if the bundle executes subprocesses / shell commands.
    #[serde(default)]
    pub exec: bool,

    /// Names of MCP tools the bundle exposes (declarative — the host
    /// shows these in its tool picker).
    #[serde(default)]
    pub mcp_tools: Vec<String>,

    /// URIs of MCP resources the bundle exposes.
    #[serde(default)]
    pub mcp_resources: Vec<String>,
}

impl Capabilities {
    /// `true` if any risky capability is declared. Hosts show a more
    /// prominent review screen when this is `true`.
    pub fn is_privileged(&self) -> bool {
        self.network || self.filesystem || self.exec
    }

    /// Human-readable summary used in logs and CLI `root inspect`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.network {
            parts.push("network");
        }
        if self.filesystem {
            parts.push("filesystem");
        }
        if self.exec {
            parts.push("exec");
        }
        if !self.mcp_tools.is_empty() {
            parts.push("mcp_tools");
        }
        if !self.mcp_resources.is_empty() {
            parts.push("mcp_resources");
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_not_privileged() {
        let c = Capabilities::default();
        assert!(!c.is_privileged());
        assert_eq!(c.summary(), "none");
    }

    #[test]
    fn privileged_summary_is_comma_joined() {
        let c = Capabilities {
            network: true,
            filesystem: false,
            exec: true,
            mcp_tools: vec!["query_claims".into()],
            mcp_resources: vec![],
        };
        assert!(c.is_privileged());
        let s = c.summary();
        assert!(s.contains("network"));
        assert!(s.contains("exec"));
        assert!(s.contains("mcp_tools"));
        assert!(!s.contains("filesystem"));
    }

    #[test]
    fn json_round_trip() {
        let c = Capabilities {
            network: true,
            filesystem: true,
            exec: false,
            mcp_tools: vec!["a".into(), "b".into()],
            mcp_resources: vec!["res://one".into()],
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn unknown_fields_are_rejected_only_if_we_ask_for_strict_parse() {
        // This is informational: we do NOT set #[serde(deny_unknown_fields)],
        // so forward-compatible hosts can receive newer capability keys
        // without crashing. Older manifests parsing newer docs simply
        // ignore unknown keys — validated by json below.
        let json = r#"{"network":true,"exec":true,"new_future_key":"ignored"}"#;
        let parsed: Capabilities = serde_json::from_str(json).unwrap();
        assert!(parsed.network);
        assert!(parsed.exec);
    }
}
