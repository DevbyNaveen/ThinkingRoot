//! YAML frontmatter — the machine-readable spine of a Living Paper.
//!
//! Every `paper.md` opens with a fenced `---`-delimited YAML block that
//! AI agents (Cursor, Claude Code, the future ThinkingRoot hub) can
//! parse without invoking an LLM. Human renderers (GitHub, VS Code
//! markdown preview, ReactMarkdown) hide the frontmatter, so a single
//! file serves both audiences.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::sections::SectionId;
use crate::PAPER_VERSION;

/// One frontmatter entry per generated section. Lets a machine
/// consumer skip directly to a section's body or verify its content
/// hash without reparsing the markdown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SectionIndexEntry {
    /// Stable identifier (kebab-case). Matches the section's H2
    /// heading slug in the body, e.g. `at-a-glance`.
    pub id: String,
    /// BLAKE3 hex over the canonical inputs that produced this
    /// section. Lets a future synthesiser short-circuit unchanged
    /// sections (v1.1 section-level caching).
    pub input_blake3: String,
    /// Length of the rendered section body in characters (excludes
    /// the H2 heading line). Makes the index useful for offset
    /// arithmetic without re-parsing the body.
    pub length_chars: u64,
}

/// Full frontmatter shape. Stable across `paper_version = 1`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Frontmatter {
    /// Schema version. Bump when the frontmatter shape changes.
    pub paper_version: u32,
    /// Human-readable workspace name (typically the workspace
    /// directory's basename).
    pub workspace: String,
    /// ISO-8601 UTC timestamp the paper was synthesised at.
    pub compiled_at: DateTime<Utc>,
    /// Total witnesses in the workspace at synthesis time.
    pub witness_count: u64,
    /// Total source files in the workspace at synthesis time.
    pub source_count: u64,
    /// Total active branches at synthesis time (always at least 1
    /// for the canonical `main` branch).
    pub branch_count: u64,
    /// BLAKE3 hex of the rule catalog the witnesses derive from.
    /// Empty when the workspace doesn't yet ship Witness Mesh rows.
    pub rule_catalog_blake3: String,
    /// Section index — one entry per generated section in render
    /// order.
    pub sections: Vec<SectionIndexEntry>,
}

impl Frontmatter {
    /// Render the frontmatter as the fenced YAML block that opens a
    /// `paper.md` file (`---\n...\n---\n\n`).
    pub fn to_markdown_block(&self) -> String {
        // serde_yaml emits stable key order matching struct field
        // order. Append trailing fences + double newline so the body
        // starts cleanly.
        let yaml = serde_yaml::to_string(self).unwrap_or_else(|_| String::new());
        format!("---\n{yaml}---\n\n")
    }
}

/// Helper: build a `SectionIndexEntry` from a section id + its raw
/// input bytes + its rendered body. Centralises the BLAKE3 + length
/// computation so all sections produce identically-shaped index rows.
pub fn section_entry(id: SectionId, input_bytes: &[u8], rendered_body: &str) -> SectionIndexEntry {
    SectionIndexEntry {
        id: id.kebab().to_string(),
        input_blake3: blake3::hash(input_bytes).to_hex().to_string(),
        length_chars: rendered_body.chars().count() as u64,
    }
}

/// Construct a starter frontmatter with the supplied workspace
/// identity. Caller populates `sections` after rendering each
/// section.
pub fn new_frontmatter(
    workspace_name: &str,
    compiled_at: DateTime<Utc>,
    witness_count: u64,
    source_count: u64,
    branch_count: u64,
    rule_catalog_blake3: String,
) -> Frontmatter {
    Frontmatter {
        paper_version: PAPER_VERSION,
        workspace: workspace_name.to_string(),
        compiled_at,
        witness_count,
        source_count,
        branch_count,
        rule_catalog_blake3,
        sections: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_roundtrips_via_yaml() {
        let mut fm = new_frontmatter(
            "demo",
            DateTime::parse_from_rfc3339("2026-05-14T16:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
            42,
            7,
            3,
            "abc123".into(),
        );
        fm.sections
            .push(section_entry(SectionId::AtAGlance, b"raw-input", "rendered body"));
        let yaml = serde_yaml::to_string(&fm).unwrap();
        let parsed: Frontmatter = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, fm);
    }

    #[test]
    fn frontmatter_block_starts_and_ends_with_fences() {
        let fm = new_frontmatter(
            "demo",
            Utc::now(),
            0,
            0,
            1,
            String::new(),
        );
        let block = fm.to_markdown_block();
        assert!(block.starts_with("---\n"));
        assert!(block.contains("\n---\n\n"));
        assert!(block.contains("paper_version: 1"));
        assert!(block.contains("workspace: demo"));
    }

    #[test]
    fn section_entry_hashes_input_not_output() {
        let a = section_entry(SectionId::AtAGlance, b"input", "different body");
        let b = section_entry(SectionId::AtAGlance, b"input", "yet another body");
        assert_eq!(
            a.input_blake3, b.input_blake3,
            "same input must hash to the same blake3 regardless of rendered body"
        );
        let c = section_entry(SectionId::AtAGlance, b"another-input", "different body");
        assert_ne!(
            a.input_blake3, c.input_blake3,
            "different input must hash differently — that's the point of the BLAKE3 spine"
        );
    }
}
