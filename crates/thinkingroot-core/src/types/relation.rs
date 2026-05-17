use chrono::{DateTime, Utc};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

use super::{ClaimId, EntityId, RelationId};

/// A typed, directed edge between two entities in the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    pub id: RelationId,
    pub from: EntityId,
    pub to: EntityId,
    pub relation_type: RelationType,
    pub evidence: Vec<ClaimId>,
    pub strength: Strength,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub description: Option<String>,
}

impl Relation {
    pub fn new(from: EntityId, to: EntityId, relation_type: RelationType) -> Self {
        Self {
            id: RelationId::new(),
            from,
            to,
            relation_type,
            evidence: Vec::new(),
            strength: Strength::new(1.0),
            valid_from: Utc::now(),
            valid_until: None,
            description: None,
        }
    }

    pub fn with_evidence(mut self, claim: ClaimId) -> Self {
        if !self.evidence.contains(&claim) {
            self.evidence.push(claim);
        }
        self
    }

    pub fn with_strength(mut self, strength: f64) -> Self {
        self.strength = Strength::new(strength);
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn add_evidence(&mut self, claim: ClaimId) {
        if !self.evidence.contains(&claim) {
            self.evidence.push(claim);
            // More evidence = stronger relation, cap at 1.0.
            let new_strength = (self.strength.value() + 0.1).min(1.0);
            self.strength = Strength::new(new_strength);
        }
    }

    pub fn is_active(&self) -> bool {
        self.valid_until.is_none_or(|until| until > Utc::now())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    DependsOn,
    OwnedBy,
    Replaces,
    Contradicts,
    Implements,
    Uses,
    Contains,
    CreatedBy,
    PartOf,
    RelatedTo,
    Calls,
    ConfiguredBy,
    TestedBy,
}

impl RelationType {
    /// Canonical wire form — matches `#[serde(rename_all = "snake_case")]`.
    /// Use this whenever you build a `String` payload from a
    /// `RelationType` so the wire matches the typed contract instead
    /// of the legacy Debug-derived TitleCase the graph layer wrote.
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::DependsOn => "depends_on",
            Self::OwnedBy => "owned_by",
            Self::Replaces => "replaces",
            Self::Contradicts => "contradicts",
            Self::Implements => "implements",
            Self::Uses => "uses",
            Self::Contains => "contains",
            Self::CreatedBy => "created_by",
            Self::PartOf => "part_of",
            Self::RelatedTo => "related_to",
            Self::Calls => "calls",
            Self::ConfiguredBy => "configured_by",
            Self::TestedBy => "tested_by",
        }
    }

    /// Bidirectional parser: accepts wire snake_case (`"depends_on"`)
    /// and the legacy Debug-derived storage form (`"DependsOn"`).
    pub fn from_any(s: &str) -> Option<Self> {
        match s {
            "depends_on" | "DependsOn" => Some(Self::DependsOn),
            "owned_by" | "OwnedBy" => Some(Self::OwnedBy),
            "replaces" | "Replaces" => Some(Self::Replaces),
            "contradicts" | "Contradicts" => Some(Self::Contradicts),
            "implements" | "Implements" => Some(Self::Implements),
            "uses" | "Uses" => Some(Self::Uses),
            "contains" | "Contains" => Some(Self::Contains),
            "created_by" | "CreatedBy" => Some(Self::CreatedBy),
            "part_of" | "PartOf" => Some(Self::PartOf),
            "related_to" | "RelatedTo" => Some(Self::RelatedTo),
            "calls" | "Calls" => Some(Self::Calls),
            "configured_by" | "ConfiguredBy" => Some(Self::ConfiguredBy),
            "tested_by" | "TestedBy" => Some(Self::TestedBy),
            _ => None,
        }
    }

    /// Normalizes an arbitrary stored relation-type string to the
    /// canonical wire snake_case. Unknown values (e.g. structural
    /// extractor strings outside the enum) pass through unchanged.
    pub fn normalize_storage(stored: &str) -> String {
        Self::from_any(stored)
            .map(|r| r.wire_str().to_string())
            .unwrap_or_else(|| stored.to_string())
    }
}

/// Relation strength clamped to [0.0, 1.0].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Strength(OrderedFloat<f64>);

impl Strength {
    pub fn new(value: f64) -> Self {
        Self(OrderedFloat(value.clamp(0.0, 1.0)))
    }

    pub fn value(&self) -> f64 {
        self.0.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_evidence_strengthens() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut rel = Relation::new(e1, e2, RelationType::DependsOn).with_strength(0.5);

        let c1 = ClaimId::new();
        let c2 = ClaimId::new();
        rel.add_evidence(c1);
        rel.add_evidence(c2);

        assert_eq!(rel.evidence.len(), 2);
        assert!(rel.strength.value() > 0.5);
    }

    #[test]
    fn no_duplicate_evidence() {
        let e1 = EntityId::new();
        let e2 = EntityId::new();
        let mut rel = Relation::new(e1, e2, RelationType::Uses);
        let c = ClaimId::new();
        rel.add_evidence(c);
        rel.add_evidence(c);
        assert_eq!(rel.evidence.len(), 1);
    }

    #[test]
    fn relation_type_wire_str_matches_serde() {
        // Compound variants are the load-bearing case: Debug emits
        // `"DependsOn"` but serde emits `"depends_on"`.
        assert_eq!(RelationType::DependsOn.wire_str(), "depends_on");
        assert_eq!(RelationType::OwnedBy.wire_str(), "owned_by");
        assert_eq!(RelationType::TestedBy.wire_str(), "tested_by");
        for variant in [
            RelationType::DependsOn,
            RelationType::OwnedBy,
            RelationType::Replaces,
            RelationType::Contradicts,
            RelationType::Implements,
            RelationType::Uses,
            RelationType::Contains,
            RelationType::CreatedBy,
            RelationType::PartOf,
            RelationType::RelatedTo,
            RelationType::Calls,
            RelationType::ConfiguredBy,
            RelationType::TestedBy,
        ] {
            let serde_form = serde_json::to_value(variant).unwrap();
            assert_eq!(serde_form.as_str().unwrap(), variant.wire_str());
        }
    }

    #[test]
    fn relation_type_normalize_storage() {
        assert_eq!(RelationType::normalize_storage("DependsOn"), "depends_on");
        assert_eq!(RelationType::normalize_storage("depends_on"), "depends_on");
        assert_eq!(RelationType::normalize_storage("Calls"), "calls");
        // Unknown extractor strings pass through unchanged.
        assert_eq!(
            RelationType::normalize_storage("custom_relation"),
            "custom_relation"
        );
    }
}
