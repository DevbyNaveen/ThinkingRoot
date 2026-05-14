//! The Living Paper's section catalogue.
//!
//! v1 ships **deterministic-only** sections — every byte is derived
//! from substrate state, never from an LLM. The AI-narrative sections
//! (`Abstract`, `KeyIdeas`, `HowItFitsTogether`, `RecentChanges`,
//! `HowToUseIt`) are scaffolded here for the v1.1 layer that will add
//! `[[witness:<id>]]`-cited narrative. Each section carries a stable
//! `kebab-case` id used as the markdown H2 slug AND as the
//! `frontmatter.sections[].id` key — same string in both places so a
//! machine consumer can match index entries to body offsets.

use serde::{Deserialize, Serialize};

/// Stable identifier for one section in a `paper.md` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SectionId {
    /// **v1 deterministic.** Workspace + witness + source + branch
    /// counts. Surfaces "what's in here" at a glance, no LLM.
    AtAGlance,
    /// **v1 deterministic.** Mermaid concept map built from the top
    /// witness clusters (by inbound-edge count). Renders as a `graph
    /// LR` block — GitHub + most markdown viewers handle Mermaid
    /// natively.
    Architecture,
    /// **v1 deterministic.** Verbatim invariant list pulled from the
    /// rule catalog: every rule that fired during this compile and
    /// the confidence it was admitted at.
    PromisesItKeeps,
    /// **v1 deterministic.** Test annotation witness counts grouped
    /// by language / framework.
    HowItIsTested,
    /// **v1 deterministic.** Pack identity, rule catalog BLAKE3,
    /// workspace id — everything an AI agent or human verifier needs
    /// to reproduce or audit this paper.
    Provenance,

    /// **v1.1 AI narrative.** ~120-word workspace abstract. Citation-
    /// grounded via `[[witness:<id>]]` markers; post-synthesis
    /// validator rejects any uncited prose.
    Abstract,
    /// **v1.1 AI narrative.** Top 5 most-cited witnesses, ranked by
    /// inbound-edge count, with one-sentence prose for each.
    KeyIdeas,
    /// **v1.1 AI narrative.** Explains the structural skeleton in
    /// human language — references the deterministic Architecture
    /// section by anchor.
    HowItFitsTogether,
    /// **v1.1 AI narrative.** New witnesses + new branches in the
    /// last 7 days. Empty when the workspace is fresh.
    RecentChanges,
    /// **v1.1 AI narrative.** Onboarding instructions — citation-
    /// grounded entry points into the workspace.
    HowToUseIt,
}

impl SectionId {
    /// Stable kebab-case slug used as the H2 markdown heading id AND
    /// the frontmatter index key.
    pub const fn kebab(self) -> &'static str {
        match self {
            SectionId::AtAGlance => "at-a-glance",
            SectionId::Architecture => "architecture",
            SectionId::PromisesItKeeps => "promises-it-keeps",
            SectionId::HowItIsTested => "how-it-is-tested",
            SectionId::Provenance => "provenance",
            SectionId::Abstract => "abstract",
            SectionId::KeyIdeas => "key-ideas",
            SectionId::HowItFitsTogether => "how-it-fits-together",
            SectionId::RecentChanges => "recent-changes",
            SectionId::HowToUseIt => "how-to-use-it",
        }
    }

    /// Human-readable H2 title, capitalised for the rendered body.
    pub const fn title(self) -> &'static str {
        match self {
            SectionId::AtAGlance => "At a glance",
            SectionId::Architecture => "Architecture",
            SectionId::PromisesItKeeps => "Promises it keeps",
            SectionId::HowItIsTested => "How it's tested",
            SectionId::Provenance => "Provenance",
            SectionId::Abstract => "Abstract",
            SectionId::KeyIdeas => "Key ideas",
            SectionId::HowItFitsTogether => "How it fits together",
            SectionId::RecentChanges => "Recent changes",
            SectionId::HowToUseIt => "How to use it",
        }
    }

    /// True when this section is part of the v1 deterministic
    /// skeleton (always present, never requires an LLM). The v1.1
    /// AI sections are skipped until the narrative synthesiser
    /// ships.
    pub const fn is_v1_deterministic(self) -> bool {
        matches!(
            self,
            SectionId::AtAGlance
                | SectionId::Architecture
                | SectionId::PromisesItKeeps
                | SectionId::HowItIsTested
                | SectionId::Provenance
        )
    }
}

/// Ordered list of section ids that v1 actually renders. The
/// frontmatter index reflects this order; the body H2s appear in the
/// same order so a machine consumer can match by position.
pub const V1_RENDER_ORDER: &[SectionId] = &[
    SectionId::AtAGlance,
    SectionId::Architecture,
    SectionId::PromisesItKeeps,
    SectionId::HowItIsTested,
    SectionId::Provenance,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_has_a_unique_kebab_id() {
        let all = [
            SectionId::AtAGlance,
            SectionId::Architecture,
            SectionId::PromisesItKeeps,
            SectionId::HowItIsTested,
            SectionId::Provenance,
            SectionId::Abstract,
            SectionId::KeyIdeas,
            SectionId::HowItFitsTogether,
            SectionId::RecentChanges,
            SectionId::HowToUseIt,
        ];
        let mut seen = std::collections::HashSet::new();
        for s in all {
            assert!(seen.insert(s.kebab()), "duplicate kebab id: {}", s.kebab());
            // Defensively pin kebab-case (lowercase + hyphens only).
            assert!(
                s.kebab()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "kebab id `{}` not pure kebab-case",
                s.kebab()
            );
            assert!(!s.title().is_empty());
        }
    }

    #[test]
    fn v1_render_order_only_lists_deterministic_sections() {
        for s in V1_RENDER_ORDER {
            assert!(
                s.is_v1_deterministic(),
                "{} is in V1_RENDER_ORDER but is_v1_deterministic() = false",
                s.kebab()
            );
        }
    }

    #[test]
    fn ai_sections_are_excluded_from_v1_render_order() {
        for ai in [
            SectionId::Abstract,
            SectionId::KeyIdeas,
            SectionId::HowItFitsTogether,
            SectionId::RecentChanges,
            SectionId::HowToUseIt,
        ] {
            assert!(
                !V1_RENDER_ORDER.contains(&ai),
                "AI section {} leaked into V1_RENDER_ORDER",
                ai.kebab()
            );
        }
    }
}
