//! Clean-room reimplementation. Inspired by openhuman/tree_summarizer/
//! (GPL-3.0 reference, NOT lifted). Design notes in
//! plans/okey-so-i-wnat-elegant-hamster.md.
//!
//! Phase E.3 (2026-05-17) — YAML frontmatter parse + emit for
//! exported markdown nodes.
//!
//! Every .md emitted by the export carries a YAML frontmatter
//! block delimited by `---` lines. The schema is fixed (see
//! `FrontmatterNode` below). The same parser is used by the import
//! verification path so emit + parse round-trip.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Discriminator. Drives the parser's choice of which optional
/// fields are load-bearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    /// Top-level `index.md` for the entire workspace.
    Index,
    /// `sources/<slug>/index.md`.
    Source,
    /// One row of the claims table.
    Claim,
    /// One Witness from the witness mesh.
    Witness,
    /// `topics/<branch>/index.md`.
    Topic,
}

/// Parsed frontmatter — every field optional except `node_type`
/// and `workspace`. Optional-by-default keeps the schema tolerant
/// of future field additions; consumers downcast based on
/// `node_type`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FrontmatterNode {
    pub node_type: NodeType,
    pub id: Option<String>,
    pub workspace: String,
    /// RFC3339 UTC timestamp. Optional because the workspace
    /// `index.md` doesn't need it.
    pub created_at: Option<String>,
    /// BLAKE3 hex of the source byte range, for Witness / Claim nodes.
    pub content_blake3: Option<String>,
    /// Witness rule (`tree-sitter::function-decl@v1`).
    pub rule: Option<String>,
    /// Witness DAG parents (other Witness ids).
    #[serde(default)]
    pub parents: Vec<String>,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
    pub source_id: Option<String>,
    /// Claim statement type (when node_type == Claim).
    pub claim_type: Option<String>,
    /// Free-form extension fields for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

impl Default for NodeType {
    fn default() -> Self {
        Self::Index
    }
}

/// Render a frontmatter block as a string ready to prepend to the
/// markdown body. Includes both `---` fences and a trailing newline.
pub fn emit(node: &FrontmatterNode) -> String {
    let body = serde_yaml::to_string(node).unwrap_or_default();
    format!("---\n{body}---\n")
}

/// Parse a markdown document with a leading frontmatter block.
/// Returns `(parsed_frontmatter, body_text)`. On absent or malformed
/// frontmatter, returns an `Err` rather than fabricating defaults —
/// import verification fails loudly.
#[derive(Debug)]
pub enum FrontmatterParseError {
    MissingFrontmatter,
    UnterminatedFrontmatter,
    YamlParseFailure(String),
}

impl std::fmt::Display for FrontmatterParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingFrontmatter => write!(f, "missing `---` frontmatter opener"),
            Self::UnterminatedFrontmatter => write!(f, "unterminated frontmatter — no closing `---`"),
            Self::YamlParseFailure(m) => write!(f, "YAML parse failure: {m}"),
        }
    }
}

impl std::error::Error for FrontmatterParseError {}

pub fn parse(input: &str) -> Result<(FrontmatterNode, String), FrontmatterParseError> {
    // Tolerate optional leading BOM + a single newline before the
    // opening fence — markdown editors sometimes save the latter.
    let stripped = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let after_lead = stripped.trim_start_matches('\n');

    let after_open = after_lead
        .strip_prefix("---\n")
        .ok_or(FrontmatterParseError::MissingFrontmatter)?;

    // Find the closing `\n---\n` OR `\n---<EOF>`.
    let (yaml_body, rest) = match after_open.find("\n---\n") {
        Some(idx) => (&after_open[..idx], &after_open[idx + "\n---\n".len()..]),
        None => match after_open.strip_suffix("\n---") {
            Some(prefix) => (prefix, ""),
            None => return Err(FrontmatterParseError::UnterminatedFrontmatter),
        },
    };

    let node: FrontmatterNode = serde_yaml::from_str(yaml_body)
        .map_err(|e| FrontmatterParseError::YamlParseFailure(e.to_string()))?;

    Ok((node, rest.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_then_parse_round_trips_basic_node() {
        let node = FrontmatterNode {
            node_type: NodeType::Witness,
            id: Some("abc123".into()),
            workspace: "ws1".into(),
            created_at: Some("2026-05-17T12:00:00Z".into()),
            content_blake3: Some("deadbeef".into()),
            rule: Some("tree-sitter::function-decl@v1".into()),
            parents: vec!["p1".into(), "p2".into()],
            byte_start: Some(100),
            byte_end: Some(200),
            source_id: Some("src1".into()),
            claim_type: None,
            extra: BTreeMap::new(),
        };
        let serialized = emit(&node);
        let doc = format!("{serialized}# Witness body\n\nSome content here.\n");
        let (parsed, body) = parse(&doc).expect("must parse");
        assert_eq!(parsed.id.as_deref(), Some("abc123"));
        assert_eq!(parsed.workspace, "ws1");
        assert_eq!(parsed.parents, vec!["p1", "p2"]);
        assert_eq!(parsed.byte_start, Some(100));
        assert_eq!(parsed.node_type, NodeType::Witness);
        assert_eq!(body, "# Witness body\n\nSome content here.\n");
    }

    #[test]
    fn parse_rejects_missing_opener() {
        let doc = "no frontmatter here\n";
        let err = parse(doc).unwrap_err();
        assert!(matches!(err, FrontmatterParseError::MissingFrontmatter));
    }

    #[test]
    fn parse_rejects_unterminated() {
        let doc = "---\nnode_type: witness\nworkspace: ws1\n";
        let err = parse(doc).unwrap_err();
        assert!(matches!(err, FrontmatterParseError::UnterminatedFrontmatter));
    }

    #[test]
    fn parse_rejects_malformed_yaml() {
        let doc = "---\nnode_type: : witness :\n---\nbody\n";
        let err = parse(doc).unwrap_err();
        assert!(matches!(err, FrontmatterParseError::YamlParseFailure(_)));
    }

    #[test]
    fn parse_tolerates_leading_newlines_and_bom() {
        let doc = "\u{FEFF}\n---\nnode_type: index\nworkspace: ws1\n---\nbody\n";
        let (node, body) = parse(doc).expect("must parse");
        assert_eq!(node.workspace, "ws1");
        assert_eq!(body, "body\n");
    }

    #[test]
    fn extra_fields_survive_round_trip() {
        let mut extra = BTreeMap::new();
        extra.insert(
            "custom_metric".to_string(),
            serde_yaml::Value::Number(42i64.into()),
        );
        let node = FrontmatterNode {
            node_type: NodeType::Index,
            workspace: "ws1".into(),
            extra,
            ..Default::default()
        };
        let serialized = emit(&node);
        let doc = format!("{serialized}body\n");
        let (parsed, _) = parse(&doc).unwrap();
        assert!(parsed.extra.contains_key("custom_metric"));
    }
}
