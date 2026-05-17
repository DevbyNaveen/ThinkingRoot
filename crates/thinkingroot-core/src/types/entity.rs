use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ClaimId, EntityId};

/// A named thing in the knowledge graph — person, system, concept, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub canonical_name: String,
    pub entity_type: EntityType,
    pub aliases: Vec<String>,
    pub attributes: Vec<ClaimId>,
    pub first_seen: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
    pub description: Option<String>,
}

impl Entity {
    pub fn new(canonical_name: impl Into<String>, entity_type: EntityType) -> Self {
        let now = Utc::now();
        Self {
            id: EntityId::new(),
            canonical_name: canonical_name.into(),
            entity_type,
            aliases: Vec::new(),
            attributes: Vec::new(),
            first_seen: now,
            last_updated: now,
            description: None,
        }
    }

    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        let alias = alias.into();
        if !self.aliases.contains(&alias) && alias != self.canonical_name {
            self.aliases.push(alias);
        }
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn add_attribute(&mut self, claim_id: ClaimId) {
        if !self.attributes.contains(&claim_id) {
            self.attributes.push(claim_id);
            self.last_updated = Utc::now();
        }
    }

    pub fn add_alias(&mut self, alias: impl Into<String>) {
        let alias = alias.into();
        if !self.aliases.contains(&alias) && alias != self.canonical_name {
            self.aliases.push(alias);
            self.last_updated = Utc::now();
        }
    }

    /// Check if a name matches this entity (canonical or any alias, case-insensitive).
    pub fn matches_name(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.canonical_name.to_lowercase() == lower
            || self.aliases.iter().any(|a| a.to_lowercase() == lower)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Person,
    System,
    Service,
    Concept,
    Team,
    Api,
    Database,
    Library,
    File,
    Module,
    Function,
    Config,
    Organization,
}

impl EntityType {
    /// Canonical wire form — matches `#[serde(rename_all = "snake_case")]`.
    /// Use this whenever you build a `String` payload from an `EntityType`
    /// (REST, MCP, SDK projections) so the wire matches the typed contract.
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::System => "system",
            Self::Service => "service",
            Self::Concept => "concept",
            Self::Team => "team",
            Self::Api => "api",
            Self::Database => "database",
            Self::Library => "library",
            Self::File => "file",
            Self::Module => "module",
            Self::Function => "function",
            Self::Config => "config",
            Self::Organization => "organization",
        }
    }

    /// Bidirectional parser: accepts both the wire snake_case
    /// (`"function"`) and the legacy Debug-derived TitleCase storage
    /// form (`"Function"`) the graph layer historically wrote via
    /// `format!("{:?}")`. Returns `None` for unknown strings so the
    /// caller can decide between a default and surfacing the raw value.
    pub fn from_any(s: &str) -> Option<Self> {
        match s {
            "person" | "Person" => Some(Self::Person),
            "system" | "System" => Some(Self::System),
            "service" | "Service" => Some(Self::Service),
            "concept" | "Concept" => Some(Self::Concept),
            "team" | "Team" => Some(Self::Team),
            "api" | "Api" => Some(Self::Api),
            "database" | "Database" => Some(Self::Database),
            "library" | "Library" => Some(Self::Library),
            "file" | "File" => Some(Self::File),
            "module" | "Module" => Some(Self::Module),
            "function" | "Function" => Some(Self::Function),
            "config" | "Config" => Some(Self::Config),
            "organization" | "Organization" => Some(Self::Organization),
            _ => None,
        }
    }

    /// Normalizes an arbitrary stored entity-type string to the
    /// canonical wire snake_case. Falls back to a lowercase copy
    /// when the string isn't a recognised variant — keeps the
    /// existing UI tolerance for unknown extractor outputs.
    pub fn normalize_storage(stored: &str) -> String {
        Self::from_any(stored)
            .map(|e| e.wire_str().to_string())
            .unwrap_or_else(|| stored.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_name_matching() {
        let entity = Entity::new("PostgreSQL", EntityType::Database)
            .with_alias("postgres")
            .with_alias("pg");

        assert!(entity.matches_name("PostgreSQL"));
        assert!(entity.matches_name("postgresql"));
        assert!(entity.matches_name("postgres"));
        assert!(entity.matches_name("PG"));
        assert!(!entity.matches_name("MySQL"));
    }

    #[test]
    fn no_duplicate_aliases() {
        let entity = Entity::new("Test", EntityType::Concept)
            .with_alias("test_alias")
            .with_alias("test_alias");

        assert_eq!(entity.aliases.len(), 1);
    }

    #[test]
    fn canonical_name_not_aliased() {
        let entity = Entity::new("Test", EntityType::Concept).with_alias("Test");
        assert!(entity.aliases.is_empty());
    }

    #[test]
    fn wire_str_matches_serde_snake_case() {
        // Single-word variants
        assert_eq!(EntityType::Person.wire_str(), "person");
        assert_eq!(EntityType::Function.wire_str(), "function");
        assert_eq!(EntityType::Api.wire_str(), "api");
        // Round-trip with serde — guards against future variant
        // additions that forget to update the wire_str arm.
        for variant in [
            EntityType::Person,
            EntityType::System,
            EntityType::Service,
            EntityType::Concept,
            EntityType::Team,
            EntityType::Api,
            EntityType::Database,
            EntityType::Library,
            EntityType::File,
            EntityType::Module,
            EntityType::Function,
            EntityType::Config,
            EntityType::Organization,
        ] {
            let serde_form = serde_json::to_value(variant).unwrap();
            assert_eq!(serde_form.as_str().unwrap(), variant.wire_str());
        }
    }

    #[test]
    fn from_any_accepts_both_storage_forms() {
        assert_eq!(EntityType::from_any("Function"), Some(EntityType::Function));
        assert_eq!(EntityType::from_any("function"), Some(EntityType::Function));
        assert_eq!(EntityType::from_any("File"), Some(EntityType::File));
        assert_eq!(EntityType::from_any("file"), Some(EntityType::File));
        assert_eq!(EntityType::from_any("WeirdType"), None);
    }

    #[test]
    fn normalize_storage_returns_wire_form_or_lowercase() {
        assert_eq!(EntityType::normalize_storage("Function"), "function");
        assert_eq!(EntityType::normalize_storage("function"), "function");
        assert_eq!(EntityType::normalize_storage("Organization"), "organization");
        // Unknown values stay lowercase (best-effort tolerance).
        assert_eq!(EntityType::normalize_storage("Something"), "something");
    }
}
