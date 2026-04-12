use thinkingroot_core::types::RelationType;
use thinkingroot_extract::extractor::SourcedRelation;

/// Specificity rank: higher = more specific.
/// Two relations with the same rank in different subtrees are orthogonal (both kept).
pub fn specificity_rank(r: RelationType) -> u8 {
    match r {
        RelationType::RelatedTo    => 0,
        RelationType::Uses         => 1,
        RelationType::Contains     => 1,
        RelationType::CreatedBy    => 1,
        RelationType::OwnedBy      => 2,
        RelationType::DependsOn    => 2,
        RelationType::Calls        => 2,
        RelationType::PartOf       => 2,
        RelationType::Implements   => 2,
        RelationType::TestedBy     => 2,
        RelationType::ConfiguredBy => 2,
        RelationType::Replaces     => 2,
        RelationType::Contradicts  => 2,
    }
}

/// Returns true if `general` subsumes `specific` — meaning both describe the
/// same semantic concept but `specific` is more precise.
/// Only true within the same subtree (Uses→DependsOn, not Uses→PartOf).
pub fn subsumes(general: RelationType, specific: RelationType) -> bool {
    if general == specific {
        return false;
    }
    matches!(
        (general, specific),
        (RelationType::RelatedTo, _)
            | (RelationType::Uses, RelationType::DependsOn)
            | (RelationType::Uses, RelationType::Calls)
            | (RelationType::Contains, RelationType::PartOf)
            | (RelationType::CreatedBy, RelationType::OwnedBy)
    )
}

/// Deduplicate a list of sourced relations:
/// - For any `(from, to)` pair with multiple relation types, keep only the
///   most specific type (per subsumption hierarchy).
/// - If two types are orthogonal (different subtrees), both are kept.
pub fn dedup_relations(relations: &mut Vec<SourcedRelation>) {
    use std::collections::HashMap;

    let mut pair_map: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, sr) in relations.iter().enumerate() {
        let key = (sr.relation.from.to_string(), sr.relation.to.to_string());
        pair_map.entry(key).or_default().push(i);
    }

    let mut to_remove: Vec<usize> = Vec::new();

    for indices in pair_map.values() {
        if indices.len() < 2 {
            continue;
        }
        for i in 0..indices.len() {
            for j in 0..indices.len() {
                if i == j {
                    continue;
                }
                let idx_i = indices[i];
                let idx_j = indices[j];
                if to_remove.contains(&idx_i) || to_remove.contains(&idx_j) {
                    continue;
                }

                let type_i = relations[idx_i].relation.relation_type;
                let type_j = relations[idx_j].relation.relation_type;

                // If i subsumes j (j is more specific), remove i (keep the specific one).
                if subsumes(type_i, type_j) {
                    to_remove.push(idx_i);
                }
            }
        }
    }

    to_remove.sort_unstable();
    to_remove.dedup();
    for idx in to_remove.into_iter().rev() {
        relations.remove(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkingroot_core::types::{Relation, RelationType, EntityId, SourceId};
    use thinkingroot_extract::extractor::SourcedRelation;

    fn make_relation(from: EntityId, to: EntityId, rel: RelationType, strength: f64) -> SourcedRelation {
        SourcedRelation {
            source: SourceId::new(),
            relation: Relation::new(from, to, rel).with_strength(strength),
        }
    }

    #[test]
    fn dedup_removes_uses_when_depends_on_exists_for_same_pair() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::Uses, 0.8),
            make_relation(e1, e2, RelationType::DependsOn, 0.9),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 1, "should keep only DependsOn");
        assert_eq!(relations[0].relation.relation_type, RelationType::DependsOn);
    }

    #[test]
    fn dedup_keeps_orthogonal_types_for_same_pair() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::DependsOn, 0.9),
            make_relation(e1, e2, RelationType::TestedBy, 0.8),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 2, "orthogonal types for same pair must both survive");
    }

    #[test]
    fn dedup_removes_related_to_when_specific_type_exists() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::RelatedTo, 0.5),
            make_relation(e1, e2, RelationType::Calls, 0.9),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].relation.relation_type, RelationType::Calls);
    }

    #[test]
    fn dedup_does_not_touch_different_pairs() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let e3 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::Uses, 0.8),
            make_relation(e1, e3, RelationType::DependsOn, 0.9),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 2, "different pairs must not affect each other");
    }

    #[test]
    fn dedup_does_not_remove_same_type_duplicates() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::RelatedTo, 0.5),
            make_relation(e1, e2, RelationType::RelatedTo, 0.7),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 2, "same-type duplicates should not be removed by subsumption");
    }

    #[test]
    fn dedup_transitive_three_types_keeps_only_most_specific() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut relations = vec![
            make_relation(e1, e2, RelationType::RelatedTo, 0.3),
            make_relation(e1, e2, RelationType::Uses, 0.6),
            make_relation(e1, e2, RelationType::DependsOn, 0.9),
        ];
        dedup_relations(&mut relations);
        assert_eq!(relations.len(), 1, "should collapse to DependsOn only");
        assert_eq!(relations[0].relation.relation_type, RelationType::DependsOn);
    }
}
