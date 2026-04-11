use serde::{Deserialize, Serialize};

/// The structured output schema that the LLM must return.
/// This is what we parse from the LLM response for each chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    #[serde(default)]
    pub claims: Vec<ExtractedClaim>,
    #[serde(default)]
    pub entities: Vec<ExtractedEntity>,
    #[serde(default)]
    pub relations: Vec<ExtractedRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedClaim {
    pub statement: String,
    pub claim_type: String,
    pub confidence: f64,
    pub entities: Vec<String>,
    #[serde(default)]
    pub source_quote: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    pub entity_type: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRelation {
    pub from_entity: String,
    pub to_entity: String,
    pub relation_type: String,
    pub description: Option<String>,
}

impl ExtractionResult {
    pub fn empty() -> Self {
        Self {
            claims: Vec::new(),
            entities: Vec::new(),
            relations: Vec::new(),
        }
    }

    pub fn merge(&mut self, other: ExtractionResult) {
        self.claims.extend(other.claims);
        self.entities.extend(other.entities);
        self.relations.extend(other.relations);
    }
}
