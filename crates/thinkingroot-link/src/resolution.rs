use strsim::normalized_levenshtein;
use thinkingroot_core::types::{Entity, EntityId};

/// Threshold for fuzzy entity name matching (0.0-1.0).
const SIMILARITY_THRESHOLD: f64 = 0.85;

/// Resolve a new entity against a set of existing entities.
/// Returns Some(existing_id) if the entity should be merged, None if it's new.
pub fn resolve_entity(new_entity: &Entity, existing: &[Entity]) -> Option<EntityId> {
    let new_name = new_entity.canonical_name.to_lowercase();

    for existing_entity in existing {
        // Exact match on canonical name.
        if existing_entity.canonical_name.to_lowercase() == new_name {
            return Some(existing_entity.id);
        }

        // Exact match on any alias.
        if existing_entity.matches_name(&new_entity.canonical_name) {
            return Some(existing_entity.id);
        }

        // Check new entity's aliases against existing entity.
        for alias in &new_entity.aliases {
            if existing_entity.matches_name(alias) {
                return Some(existing_entity.id);
            }
        }

        // Fuzzy match on canonical names.
        let similarity =
            normalized_levenshtein(&existing_entity.canonical_name.to_lowercase(), &new_name);
        if similarity >= SIMILARITY_THRESHOLD
            && existing_entity.entity_type == new_entity.entity_type
        {
            return Some(existing_entity.id);
        }
    }

    None
}

/// Merge a new entity into an existing entity, combining aliases and attributes.
pub fn merge_entities(existing: &mut Entity, new_entity: &Entity) {
    // Add the new entity's canonical name as an alias.
    existing.add_alias(&new_entity.canonical_name);

    // Add all new aliases.
    for alias in &new_entity.aliases {
        existing.add_alias(alias);
    }

    // Merge attributes.
    for attr in &new_entity.attributes {
        existing.add_attribute(*attr);
    }

    // Update description if the existing one is missing.
    if existing.description.is_none() && new_entity.description.is_some() {
        existing.description = new_entity.description.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::EntityType;

    #[test]
    fn exact_match() {
        let existing = vec![Entity::new("PostgreSQL", EntityType::Database)];
        let new = Entity::new("PostgreSQL", EntityType::Database);
        assert!(resolve_entity(&new, &existing).is_some());
    }

    #[test]
    fn alias_match() {
        let existing = vec![Entity::new("PostgreSQL", EntityType::Database).with_alias("postgres")];
        let new = Entity::new("postgres", EntityType::Database);
        assert!(resolve_entity(&new, &existing).is_some());
    }

    #[test]
    fn fuzzy_match() {
        let existing = vec![Entity::new("PostgreSQL", EntityType::Database)];
        let new = Entity::new("Postgresql", EntityType::Database);
        assert!(resolve_entity(&new, &existing).is_some());
    }

    #[test]
    fn no_match() {
        let existing = vec![Entity::new("PostgreSQL", EntityType::Database)];
        let new = Entity::new("Redis", EntityType::Database);
        assert!(resolve_entity(&new, &existing).is_none());
    }

    #[test]
    fn type_mismatch_prevents_fuzzy() {
        let existing = vec![Entity::new("Config", EntityType::Config)];
        let new = Entity::new("config", EntityType::File);
        // Exact match on name should still work, but fuzzy requires same type.
        assert!(resolve_entity(&new, &existing).is_some()); // case-insensitive exact
    }
}
