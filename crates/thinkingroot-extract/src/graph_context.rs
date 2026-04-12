/// Maximum number of known entities injected into a single LLM prompt.
/// Prevents context overflow when the knowledge graph is large.
pub const MAX_KNOWN_ENTITIES: usize = 200;

/// A single entity known to the knowledge graph.
pub struct KnownEntity {
    pub name: String,
    pub entity_type: String,
}

/// A snapshot of existing entities from the knowledge graph, formatted for
/// injection into LLM extraction prompts so the model prefers matching known
/// names over inventing new ones.
pub struct GraphPrimedContext {
    pub entities: Vec<KnownEntity>,
}

impl GraphPrimedContext {
    /// Create a context from a list of `KnownEntity` values.
    pub fn new(entities: Vec<KnownEntity>) -> Self {
        Self { entities }
    }

    /// Create a context from raw (name, entity_type) tuples as returned by
    /// `GraphStore::get_known_entities`.
    pub fn from_tuples(tuples: Vec<(String, String)>) -> Self {
        let entities = tuples
            .into_iter()
            .map(|(name, entity_type)| KnownEntity { name, entity_type })
            .collect();
        Self { entities }
    }

    /// Returns true when no entities are available.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Build the `<KNOWN_ENTITIES>` XML section to embed in an LLM prompt.
    ///
    /// Returns an empty string when there are no entities so callers can
    /// skip insertion cleanly.  At most `MAX_KNOWN_ENTITIES` entries are
    /// emitted; entities beyond that cap are silently dropped to keep
    /// prompts within context limits.
    pub fn prompt_section(&self) -> String {
        if self.entities.is_empty() {
            return String::new();
        }

        let mut lines = Vec::new();
        lines.push("<KNOWN_ENTITIES>".to_string());
        lines.push(
            "The following entities already exist in the knowledge graph. \
When you encounter references to these entities, use the EXACT names below \
rather than creating new entities. Only create new entities for concepts not \
already represented."
                .to_string(),
        );
        lines.push(String::new());

        for entity in self.entities.iter().take(MAX_KNOWN_ENTITIES) {
            lines.push(format!("- {} ({})", entity.name, entity.entity_type));
        }

        lines.push("</KNOWN_ENTITIES>".to_string());
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_context_produces_empty_string() {
        let ctx = GraphPrimedContext::new(vec![]);
        assert!(ctx.prompt_section().is_empty());
    }

    #[test]
    fn known_entities_produce_prompt_section() {
        let ctx = GraphPrimedContext::new(vec![
            KnownEntity {
                name: "GraphStore".to_string(),
                entity_type: "system".to_string(),
            },
            KnownEntity {
                name: "Claim".to_string(),
                entity_type: "concept".to_string(),
            },
        ]);
        let section = ctx.prompt_section();
        assert!(section.contains("KNOWN_ENTITIES"));
        assert!(section.contains("GraphStore"));
        assert!(section.contains("Claim"));
    }

    #[test]
    fn from_tuples_converts_correctly() {
        let tuples = vec![
            ("GraphStore".to_string(), "system".to_string()),
            ("Claim".to_string(), "concept".to_string()),
        ];
        let ctx = GraphPrimedContext::from_tuples(tuples);
        assert_eq!(ctx.entities.len(), 2);
        assert_eq!(ctx.entities[0].name, "GraphStore");
        assert_eq!(ctx.entities[0].entity_type, "system");
        assert_eq!(ctx.entities[1].name, "Claim");
        assert_eq!(ctx.entities[1].entity_type, "concept");
    }

    #[test]
    fn limits_to_max_entities() {
        let tuples: Vec<(String, String)> = (0..500)
            .map(|i| (format!("Entity{i}"), "concept".to_string()))
            .collect();
        let ctx = GraphPrimedContext::from_tuples(tuples);
        let section = ctx.prompt_section();

        // Count how many "- Entity" lines appear.
        let entry_count = section
            .lines()
            .filter(|l| l.starts_with("- Entity"))
            .count();
        assert_eq!(entry_count, MAX_KNOWN_ENTITIES);
    }
}
