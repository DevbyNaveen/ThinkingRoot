/// Maximum number of known entities injected into a single LLM prompt.
/// Prevents context overflow when the knowledge graph is large.
pub const MAX_KNOWN_ENTITIES: usize = 200;
/// Maximum number of known relations injected into a single LLM prompt.
pub const MAX_KNOWN_RELATIONS: usize = 100;

/// A single entity known to the knowledge graph.
pub struct KnownEntity {
    pub name: String,
    pub entity_type: String,
}

/// A single relation already in the knowledge graph.
pub struct KnownRelation {
    pub from: String,
    pub to: String,
    pub relation_type: String,
}

/// A snapshot of existing graph state, formatted for injection into LLM
/// extraction prompts. Tells the LLM which entities and relations already
/// exist so it uses canonical names and avoids re-extracting known edges.
pub struct GraphPrimedContext {
    pub entities: Vec<KnownEntity>,
    pub relations: Vec<KnownRelation>,
}

impl GraphPrimedContext {
    /// Create a context from a list of `KnownEntity` values.
    pub fn new(entities: Vec<KnownEntity>) -> Self {
        Self {
            entities,
            relations: Vec::new(),
        }
    }

    /// Create a context from raw (name, entity_type) tuples as returned by
    /// `GraphStore::get_known_entities`.
    pub fn from_tuples(tuples: Vec<(String, String)>) -> Self {
        let entities = tuples
            .into_iter()
            .map(|(name, entity_type)| KnownEntity { name, entity_type })
            .collect();
        Self {
            entities,
            relations: Vec::new(),
        }
    }

    /// Attach known relations (from `GraphStore::get_known_relations`) to this context.
    pub fn with_relations(mut self, relations: Vec<KnownRelation>) -> Self {
        self.relations = relations;
        self
    }

    /// Returns true when no entities are available.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Build the combined `<KNOWN_ENTITIES>` + `<KNOWN_RELATIONS>` XML section
    /// to embed in an LLM extraction prompt.
    ///
    /// Returns an empty string when there are no entities so callers can
    /// skip insertion cleanly.  At most `MAX_KNOWN_ENTITIES` entity entries and
    /// `MAX_KNOWN_RELATIONS` relation entries are emitted to keep prompts within
    /// context limits.
    pub fn prompt_section(&self) -> String {
        if self.entities.is_empty() {
            return String::new();
        }

        let mut lines = Vec::new();

        lines.push("<KNOWN_ENTITIES>".to_string());
        lines.push(
            "The following entities already exist in the knowledge graph. \
Use the EXACT names below when referencing these entities. \
Only create new entities for concepts not already represented."
                .to_string(),
        );
        lines.push(String::new());
        for entity in self.entities.iter().take(MAX_KNOWN_ENTITIES) {
            lines.push(format!("- {} ({})", entity.name, entity.entity_type));
        }
        lines.push("</KNOWN_ENTITIES>".to_string());

        if !self.relations.is_empty() {
            lines.push(String::new());
            lines.push("<KNOWN_RELATIONS>".to_string());
            lines.push(
                "The following relations already exist in the knowledge graph. \
Do NOT re-extract these exact pairs — only extract NEW relations not listed here."
                    .to_string(),
            );
            lines.push(String::new());
            for rel in self.relations.iter().take(MAX_KNOWN_RELATIONS) {
                lines.push(format!(
                    "- {} --[{}]--> {}",
                    rel.from, rel.relation_type, rel.to
                ));
            }
            lines.push("</KNOWN_RELATIONS>".to_string());
        }

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

    #[test]
    fn limits_to_max_relations() {
        let relations: Vec<KnownRelation> = (0..500)
            .map(|i| KnownRelation {
                from: format!("Entity{i}"),
                to: format!("Target{i}"),
                relation_type: "uses".to_string(),
            })
            .collect();
        let ctx = GraphPrimedContext {
            entities: vec![KnownEntity {
                name: "Seed".to_string(),
                entity_type: "concept".to_string(),
            }],
            relations,
        };
        let section = ctx.prompt_section();
        let rel_count = section
            .lines()
            .filter(|l| l.starts_with("- Entity"))
            .count();
        assert_eq!(rel_count, MAX_KNOWN_RELATIONS);
    }

    #[test]
    fn known_relations_appear_in_prompt_section() {
        let ctx = GraphPrimedContext {
            entities: vec![KnownEntity {
                name: "AuthService".to_string(),
                entity_type: "service".to_string(),
            }],
            relations: vec![KnownRelation {
                from: "AuthService".to_string(),
                to: "JWT".to_string(),
                relation_type: "uses".to_string(),
            }],
        };
        let section = ctx.prompt_section();
        assert!(
            section.contains("KNOWN_RELATIONS"),
            "section must include KNOWN_RELATIONS block"
        );
        assert!(
            section.contains("AuthService"),
            "section must include from entity"
        );
        assert!(section.contains("JWT"), "section must include to entity");
        assert!(
            section.contains("uses"),
            "section must include relation type"
        );
    }

    #[test]
    fn empty_relations_still_produces_entities_section() {
        let ctx = GraphPrimedContext {
            entities: vec![KnownEntity {
                name: "MyService".to_string(),
                entity_type: "service".to_string(),
            }],
            relations: vec![],
        };
        let section = ctx.prompt_section();
        assert!(section.contains("KNOWN_ENTITIES"));
        assert!(
            !section.contains("KNOWN_RELATIONS"),
            "no relations block when empty"
        );
    }
}
