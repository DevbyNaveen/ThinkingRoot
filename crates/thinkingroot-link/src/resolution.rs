use std::collections::HashMap;

use strsim::normalized_levenshtein;
use thinkingroot_core::types::{Entity, EntityId};

/// Threshold for fuzzy entity name matching (0.0-1.0).
const SIMILARITY_THRESHOLD: f64 = 0.85;

/// Resolve a new entity against a set of existing entities.
/// Returns Some(existing_id) if the entity should be merged, None if it's new.
///
/// NOTE: this linear scan is O(existing) per call → O(N²) when used to resolve
/// a whole extraction batch. The linker hot path uses [`EntityResolverIndex`]
/// instead (O(1) exact + blocked fuzzy). This function is retained for
/// back-compat and single-shot callers/tests.
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

/// O(1)-exact / blocked-fuzzy entity resolver index — the scale fix for the
/// linker's Phase-1 entity resolution.
///
/// The naïve [`resolve_entity`] scans every already-resolved entity for each
/// incoming one, and the dominant cost is a `normalized_levenshtein` fuzzy
/// comparison against *every* same-type entity → O(N²) edit-distance work.
/// On a large corpus (thousands of entities) that pins a single core for
/// minutes (observed: Phase 7 hang). This index keeps the **exact same match
/// rules** but makes them fast:
///
/// * **Exact** canonical/alias matches (either direction) resolve via a
///   `HashMap` → O(1).
/// * **Fuzzy** matches are *blocked* to candidates of the same `entity_type`
///   AND compatible length (a ≥0.85 normalized-Levenshtein match is
///   impossible once the length gap exceeds 15% of the longer string), so each
///   resolve touches a tiny candidate set instead of the whole corpus.
///
/// The index is mutated in lock-step with the linker's `resolved_entities`
/// (`add_new` on create, `add_merge_aliases` on merge) so later lookups see
/// earlier decisions — identical observable behaviour to the linear scan,
/// minus the quadratic blow-up.
pub struct EntityResolverIndex {
    /// lowercased canonical name + every alias → resolved EntityId.
    exact: HashMap<String, EntityId>,
    /// `{:?}`-tagged entity_type → (lowercased canonical, id) in insertion
    /// order. Canonical-only, matching `resolve_entity`'s fuzzy semantics.
    by_type: HashMap<String, Vec<(String, EntityId)>>,
}

impl EntityResolverIndex {
    /// Build an index over the workspace's already-resolved entities.
    pub fn from_entities(entities: &[Entity]) -> Self {
        let mut idx = Self {
            exact: HashMap::with_capacity(entities.len() * 2),
            by_type: HashMap::new(),
        };
        for e in entities {
            idx.index_entity(e);
        }
        idx
    }

    fn index_entity(&mut self, e: &Entity) {
        let canon = e.canonical_name.to_lowercase();
        self.exact.entry(canon.clone()).or_insert(e.id);
        for a in &e.aliases {
            self.exact.entry(a.to_lowercase()).or_insert(e.id);
        }
        self.by_type
            .entry(format!("{:?}", e.entity_type))
            .or_default()
            .push((canon, e.id));
    }

    /// Register a freshly-created entity so subsequent resolves can match it.
    pub fn add_new(&mut self, e: &Entity) {
        self.index_entity(e);
    }

    /// After merging `merged` into `existing_id`, its names become aliases of
    /// the surviving entity — index them as exact matches pointing at the
    /// surviving id. The fuzzy (canonical-only) bucket is intentionally left
    /// unchanged, matching `resolve_entity`'s rule that fuzzy compares only
    /// canonical names.
    pub fn add_merge_aliases(&mut self, existing_id: EntityId, merged: &Entity) {
        self.exact
            .entry(merged.canonical_name.to_lowercase())
            .or_insert(existing_id);
        for a in &merged.aliases {
            self.exact.entry(a.to_lowercase()).or_insert(existing_id);
        }
    }

    /// Resolve `new_entity` to an existing id, or `None` if it is new.
    /// Mirrors [`resolve_entity`]'s rules (exact canonical, exact alias either
    /// direction, then fuzzy same-type) in ~O(1) + blocked fuzzy.
    pub fn resolve(&self, new_entity: &Entity) -> Option<EntityId> {
        let new_name = new_entity.canonical_name.to_lowercase();

        // Cases 1+2: new canonical == an existing canonical or alias.
        if let Some(&id) = self.exact.get(&new_name) {
            return Some(id);
        }

        // Case 3: a new alias == an existing canonical or alias.
        for alias in &new_entity.aliases {
            if let Some(&id) = self.exact.get(&alias.to_lowercase()) {
                return Some(id);
            }
        }

        // Case 4: fuzzy on canonical, confined to same type + compatible length.
        if let Some(candidates) = self.by_type.get(&format!("{:?}", new_entity.entity_type)) {
            for (cand_name, id) in candidates {
                if !length_compatible(cand_name, &new_name) {
                    continue;
                }
                if normalized_levenshtein(cand_name, &new_name) >= SIMILARITY_THRESHOLD {
                    return Some(*id);
                }
            }
        }

        None
    }
}

/// Cheap necessary-condition prefilter for `normalized_levenshtein >= 0.85`:
/// edit distance is at least the length difference, and
/// `1 - dist/max_len >= 0.85` ⇒ `dist <= 0.15 * max_len`. So a length gap
/// wider than 15% of the longer string can never clear the threshold and the
/// (expensive) edit-distance call can be skipped. Lenient by an epsilon so a
/// true boundary match is never wrongly filtered.
fn length_compatible(a: &str, b: &str) -> bool {
    let la = a.chars().count();
    let lb = b.chars().count();
    let max = la.max(lb);
    if max == 0 {
        return true;
    }
    let diff = la.abs_diff(lb) as f64;
    diff <= (1.0 - SIMILARITY_THRESHOLD) * (max as f64) + 1e-9
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

    // ── EntityResolverIndex parity tests (must mirror resolve_entity) ──

    #[test]
    fn index_exact_canonical() {
        let idx = EntityResolverIndex::from_entities(&[Entity::new(
            "PostgreSQL",
            EntityType::Database,
        )]);
        let new = Entity::new("postgresql", EntityType::Database);
        assert!(idx.resolve(&new).is_some());
    }

    #[test]
    fn index_alias_either_direction() {
        // existing has alias; new canonical matches it.
        let idx = EntityResolverIndex::from_entities(&[
            Entity::new("PostgreSQL", EntityType::Database).with_alias("postgres"),
        ]);
        assert!(idx.resolve(&Entity::new("postgres", EntityType::Database)).is_some());
        // new has alias matching existing canonical.
        let new = Entity::new("PG", EntityType::Database).with_alias("PostgreSQL");
        assert!(idx.resolve(&new).is_some());
    }

    #[test]
    fn index_fuzzy_same_type() {
        let idx = EntityResolverIndex::from_entities(&[Entity::new(
            "PostgreSQL",
            EntityType::Database,
        )]);
        assert!(idx.resolve(&Entity::new("Postgresql", EntityType::Database)).is_some());
    }

    #[test]
    fn index_fuzzy_requires_same_type() {
        let idx =
            EntityResolverIndex::from_entities(&[Entity::new("Configg", EntityType::Config)]);
        // Different type, only fuzzy-similar → must NOT match.
        assert!(idx.resolve(&Entity::new("Configx", EntityType::File)).is_none());
    }

    #[test]
    fn index_no_match() {
        let idx = EntityResolverIndex::from_entities(&[Entity::new(
            "PostgreSQL",
            EntityType::Database,
        )]);
        assert!(idx.resolve(&Entity::new("Redis", EntityType::Database)).is_none());
    }

    #[test]
    fn index_incremental_add_new_then_match() {
        let mut idx = EntityResolverIndex::from_entities(&[]);
        let e = Entity::new("Redis", EntityType::Database);
        assert!(idx.resolve(&e).is_none());
        idx.add_new(&e);
        assert!(idx.resolve(&Entity::new("redis", EntityType::Database)).is_some());
    }

    #[test]
    fn index_merge_alias_resolves_later() {
        let surviving = Entity::new("PostgreSQL", EntityType::Database);
        let surviving_id = surviving.id;
        let mut idx = EntityResolverIndex::from_entities(&[surviving]);
        // Merge "Postgres DB" into the surviving entity.
        idx.add_merge_aliases(surviving_id, &Entity::new("Postgres DB", EntityType::Database));
        // A later entity named "Postgres DB" now resolves to the survivor.
        assert_eq!(
            idx.resolve(&Entity::new("Postgres DB", EntityType::Database)),
            Some(surviving_id)
        );
    }

    #[test]
    fn length_prefilter_keeps_true_matches() {
        // Same-length 1-char swap must pass the prefilter.
        assert!(length_compatible("PostgreSQL".to_lowercase().as_str(), "postgresql"));
        // Wildly different lengths can be skipped.
        assert!(!length_compatible("db", "a-very-long-database-name"));
    }
}
